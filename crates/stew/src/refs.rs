//! Ref resolution at the hub, before anything exists: one `git ls-remote --symref` per
//! repo, so a bad name or base fails before any branch, dir or box.
//!
//! Base rules (`stew new`): `--base b` applies to every repo; a repo without `b` uses its
//! default branch. Without `--base`, the repo the client runs in (`here`) starts on its
//! current branch, the others on their defaults. A base that was given or implied for the
//! repo the client runs in must exist there; under `--base`, at least one repo must have
//! it. Default branch: the remote's `HEAD`, else `main`, else `master`.

use std::collections::BTreeSet;
use std::path::Path;

use stew_proto::{Here, RepoRecord, Resolved};

use crate::catalog::Entry;
use crate::error::{Result, fail};
use crate::git::git;
use crate::url::same_repo;

#[derive(Debug, Default, PartialEq)]
pub struct RemoteRefs {
    /// The branch `HEAD` points to, if the remote says.
    pub head: Option<String>,
    pub branches: BTreeSet<String>,
}

impl RemoteRefs {
    pub fn parse(out: &str) -> Self {
        let mut refs = RemoteRefs::default();
        for line in out.lines() {
            if let Some(rest) = line.strip_prefix("ref: ") {
                if let Some((target, "HEAD")) = rest.split_once('\t') {
                    refs.head = target.strip_prefix("refs/heads/").map(String::from);
                }
            } else if let Some((_, name)) = line.split_once('\t')
                && let Some(b) = name.strip_prefix("refs/heads/")
            {
                refs.branches.insert(b.to_string());
            }
        }
        refs
    }

    pub fn has(&self, branch: &str) -> bool {
        self.branches.contains(branch)
    }

    /// `HEAD`'s branch, else `main`, else `master`.
    pub fn default_branch(&self) -> Option<&str> {
        self.head
            .as_deref()
            .filter(|h| self.has(h))
            .or_else(|| ["main", "master"].into_iter().find(|b| self.has(b)))
    }
}

pub fn ls_remote(remote: &str) -> Result<RemoteRefs> {
    let out = git(
        Path::new("/"),
        [
            "ls-remote",
            "--symref",
            "--",
            remote,
            "HEAD",
            "refs/heads/*",
        ],
    )
    .map_err(|e| fail!("cannot reach {remote}: {e}"))?;
    Ok(RemoteRefs::parse(&out))
}

/// `ls_remote` for every repo, in parallel.
fn fetch_all(remotes: &[&str]) -> Result<Vec<RemoteRefs>> {
    std::thread::scope(|s| {
        let jobs: Vec<_> = remotes
            .iter()
            .map(|r| s.spawn(move || ls_remote(r)))
            .collect();
        jobs.into_iter()
            .map(|j| {
                j.join()
                    .unwrap_or_else(|_| Err(fail!("ls-remote panicked")))
            })
            .collect()
    })
}

/// Repos with distinct clone dirs, none of them `notes`.
fn check_dirs(repos: &[Entry]) -> Result<()> {
    if repos.is_empty() {
        return Ok(());
    }
    let mut seen = BTreeSet::new();
    for e in repos {
        if e.dir() == "notes" {
            return Err(fail!(
                "{}: a repo can't be named 'notes' (the stew's notes dir)",
                e.name
            ));
        }
        if !seen.insert(e.dir()) {
            return Err(fail!("two repos would both be cloned to '{}'", e.dir()));
        }
    }
    Ok(())
}

fn default_of<'a>(e: &Entry, refs: &'a RemoteRefs) -> Result<&'a str> {
    refs.default_branch().ok_or_else(|| {
        fail!(
            "{} has no default branch (no HEAD, main or master on {})",
            e.name,
            e.remote
        )
    })
}

fn resolved(e: &Entry, branch: &str) -> Resolved {
    Resolved {
        name: e.name.clone(),
        dir: e.dir().to_string(),
        remote: e.remote.clone(),
        branch: branch.to_string(),
    }
}

/// The branches a new stew's clones start on, with warnings for repos that fell back.
pub fn resolve_new(
    repos: &[Entry],
    base: Option<&str>,
    here: Option<&Here>,
) -> Result<(Vec<Resolved>, Vec<String>)> {
    check_dirs(repos)?;
    let remotes: Vec<&str> = repos.iter().map(|e| e.remote.as_str()).collect();
    let all = fetch_all(&remotes)?;
    pick(repos, &all, base, here)
}

fn pick(
    repos: &[Entry],
    all: &[RemoteRefs],
    base: Option<&str>,
    here: Option<&Here>,
) -> Result<(Vec<Resolved>, Vec<String>)> {
    let here_idx = here.and_then(|h| repos.iter().position(|e| same_repo(&e.remote, &h.remote)));
    if let Some(b) = base
        && !repos.is_empty()
        && !all.iter().any(|r| r.has(b))
    {
        return Err(fail!("no repo has a branch '{b}'"));
    }
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    for (i, (e, refs)) in repos.iter().zip(all).enumerate() {
        let wanted = base.or(if Some(i) == here_idx {
            here.map(|h| h.branch.as_str())
        } else {
            None
        });
        let branch = match wanted {
            Some(b) if refs.has(b) => b,
            Some(b) if Some(i) == here_idx => {
                return Err(fail!("{} has no branch '{b}' on {}", e.name, e.remote));
            }
            Some(b) => {
                let d = default_of(e, refs)?;
                warnings.push(format!("{} has no branch '{b}', using {d}", e.name));
                d
            }
            None => default_of(e, refs)?,
        };
        out.push(resolved(e, branch));
    }
    Ok((out, warnings))
}

/// The branches for reopening a stew from its snapshot: each recorded branch that still
/// exists, else (with a warning) the default.
pub fn resolve_open(records: &[RepoRecord]) -> Result<(Vec<Resolved>, Vec<String>)> {
    let repos: Vec<Entry> = records
        .iter()
        .map(|r| Entry {
            name: r.name.clone(),
            remote: r.remote.clone(),
        })
        .collect();
    check_dirs(&repos)?;
    let remotes: Vec<&str> = repos.iter().map(|e| e.remote.as_str()).collect();
    let all = fetch_all(&remotes)?;
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    for ((e, refs), r) in repos.iter().zip(&all).zip(records) {
        if refs.has(&r.branch) {
            out.push(resolved(e, &r.branch));
        } else {
            let d = default_of(e, refs)?;
            warnings.push(format!(
                "{}: branch '{}' is not on the remote, using {d}",
                e.name, r.branch
            ));
            out.push(resolved(e, d));
        }
    }
    Ok((out, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(head: Option<&str>, branches: &[&str]) -> RemoteRefs {
        RemoteRefs {
            head: head.map(String::from),
            branches: branches.iter().map(|b| b.to_string()).collect(),
        }
    }

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.into(),
            remote: format!("https://h/x/{name}"),
        }
    }

    fn branches(r: &[Resolved]) -> Vec<&str> {
        r.iter().map(|r| r.branch.as_str()).collect()
    }

    #[test]
    fn parse() {
        let out = "ref: refs/heads/develop\tHEAD\nabc\tHEAD\nabc\trefs/heads/develop\ndef\trefs/heads/feat/x\n";
        assert_eq!(
            RemoteRefs::parse(out),
            refs(Some("develop"), &["develop", "feat/x"])
        );
    }

    #[test]
    fn defaults() {
        assert_eq!(
            refs(Some("dev"), &["dev", "main"]).default_branch(),
            Some("dev")
        );
        assert_eq!(
            refs(Some("gone"), &["master", "main"]).default_branch(),
            Some("main")
        );
        assert_eq!(
            refs(None, &["master", "x"]).default_branch(),
            Some("master")
        );
        assert_eq!(refs(None, &["x"]).default_branch(), None);
    }

    #[test]
    fn base_rules() {
        let repos = [entry("a"), entry("b")];
        let all = [
            refs(Some("main"), &["main", "feat"]),
            refs(Some("dev"), &["dev"]),
        ];

        let (r, w) = pick(&repos, &all, None, None).unwrap();
        assert_eq!((branches(&r), w.len()), (vec!["main", "dev"], 0));

        let (r, w) = pick(&repos, &all, Some("feat"), None).unwrap();
        assert_eq!(branches(&r), ["feat", "dev"]);
        assert!(w[0].contains("b has no branch 'feat'"), "{w:?}");

        assert!(pick(&repos, &all, Some("nope"), None).is_err());

        // implied: the repo the client runs in, on its branch
        let here = Here {
            remote: "git@h:x/a.git".into(),
            branch: "feat".into(),
        };
        let (r, _) = pick(&repos, &all, None, Some(&here)).unwrap();
        assert_eq!(branches(&r), ["feat", "dev"]);
        let gone = Here {
            branch: "local-only".into(),
            ..here.clone()
        };
        assert!(pick(&repos, &all, None, Some(&gone)).is_err());

        // given, and the client runs in a repo that lacks it
        let in_b = Here {
            remote: "https://h/x/b".into(),
            branch: "dev".into(),
        };
        assert!(pick(&repos, &all, Some("feat"), Some(&in_b)).is_err());

        // not among the repos: no effect
        let elsewhere = Here {
            remote: "https://h/x/c".into(),
            branch: "zzz".into(),
        };
        let (r, _) = pick(&repos, &all, None, Some(&elsewhere)).unwrap();
        assert_eq!(branches(&r), ["main", "dev"]);

        assert!(pick(&[entry("a")], &[refs(None, &["x"])], None, None).is_err());
    }

    #[test]
    fn dirs() {
        let dup = [
            entry("a"),
            Entry {
                name: "g/a".into(),
                remote: "https://h/y/a".into(),
            },
        ];
        assert!(check_dirs(&dup).is_err());
        assert!(check_dirs(&[entry("notes")]).is_err());
        assert!(check_dirs(&[entry("a"), entry("b")]).is_ok());
    }
}
