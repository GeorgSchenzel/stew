//! stew's code that runs where a stew lives, through the driver's `exec`: a look at its root
//! (`scripts/inspect.sh`: the clones, `notes/stew.yaml`, README, and the pushed check), the
//! clones (`scripts/clone.sh`) and stew's own commit in `notes/` (`scripts/commit.sh`).
//!
//! "Everything is pushed" (plan.md) means, in every clone and `notes/`: no uncommitted
//! changes, no untracked files, no stash, and every local branch's tip contained in some
//! remote ref. The check is fail-closed: what it can't verify counts as not pushed. It
//! also counts, beyond the plan's list: anything in the root that is not a repo, tags on
//! no remote, a detached HEAD's commits, linked worktrees, initialized submodules, an
//! operation in progress (rebase, merge, ...), files hidden from `status`
//! (assume-unchanged, skip-worktree outside a sparse checkout), and a remote that is
//! removed with the stew (a local path inside it; any local path in a box). Remote refs
//! are what `ls-remote` lists after a fetch, never the tracking refs.

use std::time::Duration;

use stew_proto::RepoRecord;

use crate::catalog::Catalog;
use crate::driver::{Driver, Status, Target};
use crate::error::{Result, fail};
use crate::url::{normalize, same_repo};

pub const INSPECT: &str = include_str!("scripts/inspect.sh");
pub const CLONE: &str = include_str!("scripts/clone.sh");
pub const COMMIT: &str = include_str!("scripts/commit.sh");

/// A clone as found in the root.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub dir: String,
    /// Its remote's URL: origin, else its only remote (empty: none).
    pub url: String,
    /// The checked-out branch (empty: detached).
    pub branch: String,
    /// `origin/HEAD`'s branch (empty: unknown).
    pub default: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct Look {
    pub repos: Vec<Found>,
    /// `notes/stew.yaml`, when there.
    pub meta: Option<String>,
    pub readme: Option<String>,
    /// What a remove would lose (only from a check).
    pub findings: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Ls,
    Check,
}

/// `sh -c <script> stew <args…>`.
pub fn sh(script: &str, args: &[&str]) -> Vec<String> {
    let mut argv = vec!["sh".to_string(), "-c".into(), script.into(), "stew".into()];
    argv.extend(args.iter().map(|a| a.to_string()));
    argv
}

/// Parses the script's output. It must end with `DONE` and hold nothing else.
pub fn parse(out: &str) -> Result<Look> {
    let mut look = Look::default();
    let mut meta = Vec::new();
    let mut readme = Vec::new();
    let mut done = false;
    for line in out.lines() {
        if done {
            return Err(fail!("unexpected output after the end: {line}"));
        }
        let (tag, rest) = line.split_at(line.len().min(2));
        match tag {
            "R " => {
                let f: Vec<&str> = rest.split('\t').collect();
                let [dir, url, branch, default] = f[..] else {
                    return Err(fail!("bad clone line: {line}"));
                };
                look.repos.push(Found {
                    dir: dir.into(),
                    url: url.into(),
                    branch: branch.into(),
                    default: default.into(),
                });
            }
            "Y " => meta.push(rest),
            "N " => readme.push(rest),
            "F " => look.findings.push(rest.to_string()),
            "W " => look.warnings.push(rest.to_string()),
            _ if line == "DONE" => done = true,
            _ => return Err(fail!("unexpected output: {line}")),
        }
    }
    if !done {
        return Err(fail!("it ended early"));
    }
    let join = |lines: Vec<&str>| (!lines.is_empty()).then(|| lines.join("\n") + "\n");
    look.meta = join(meta);
    look.readme = join(readme);
    Ok(look)
}

pub enum Looked {
    Look(Look),
    NotRunning,
}

/// Looks at the stew's root. An error when the look couldn't run or finish: for a check,
/// that means "not verified".
pub fn look(
    driver: &dyn Driver,
    t: &Target,
    mode: Mode,
    no_start: bool,
    timeout: Option<Duration>,
) -> Result<Looked> {
    let mode_word = match mode {
        Mode::Ls => "ls",
        Mode::Check => "check",
    };
    let argv = sh(INSPECT, &[mode_word, driver.local_remotes().word()]);
    let out = driver.exec(t, &argv, None, None, no_start, timeout)?;
    match out.status {
        Status::NotRunning => return Ok(Looked::NotRunning),
        Status::NoBox => return Err(fail!("stew {} is not on its site", t.id)),
        Status::Exited(0) => {}
        Status::Exited(_) => {
            return Err(fail!("looking at stew {} failed: {}", t.id, out.reason()));
        }
    }
    parse(&out.stdout())
        .map(Looked::Look)
        .map_err(|e| fail!("looking at stew {}: {e}", t.id))
}

/// The snapshot's repos from what is in the root. A clone keeps its recorded name when
/// it is the same repo in the same dir, else takes a catalog name that fits, else its dir.
/// Detached: its remote's HEAD branch. No remote (none, or several and no origin): left
/// out, with a warning.
pub fn snapshot(
    found: &[Found],
    previous: &[RepoRecord],
    catalog: Option<&Catalog>,
) -> (Vec<RepoRecord>, Vec<String>) {
    let last = |name: &str| name.rsplit('/').next().unwrap_or(name).to_string();
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    for f in found {
        if f.url.is_empty() {
            warnings.push(format!(
                "{}: no origin (and not just one remote), left out of the snapshot",
                f.dir
            ));
            continue;
        }
        let remote = match normalize(&f.url) {
            Ok(r) => r,
            Err(e) => {
                warnings.push(format!("{}: {e}; left out of the snapshot", f.dir));
                continue;
            }
        };
        let prev = previous
            .iter()
            .find(|p| same_repo(&p.remote, &remote) && last(&p.name) == f.dir);
        let name = prev
            .map(|p| p.name.clone())
            .or_else(|| {
                catalog.and_then(|c| {
                    c.entries
                        .iter()
                        .find(|e| same_repo(&e.remote, &remote) && e.dir() == f.dir)
                        .map(|e| e.name.clone())
                })
            })
            .unwrap_or_else(|| f.dir.clone());
        let branch = if !f.branch.is_empty() {
            f.branch.clone()
        } else if !f.default.is_empty() {
            warnings.push(format!(
                "{}: detached HEAD, recorded as {}",
                f.dir, f.default
            ));
            f.default.clone()
        } else if let Some(p) = prev {
            warnings.push(format!(
                "{}: detached HEAD, keeping the recorded {}",
                f.dir, p.branch
            ));
            p.branch.clone()
        } else {
            warnings.push(format!(
                "{}: detached HEAD and no default branch, left out of the snapshot",
                f.dir
            ));
            continue;
        };
        out.push(RepoRecord {
            name,
            remote,
            branch,
        });
    }
    (out, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Entry;

    #[test]
    fn parses_output() {
        let out = "R a\t/r/a.git\tmain\tmain\nR b\t\t\t\nF a: x\nF     y\nW w\nY id: x\nY title: T\nN # T\nDONE\n";
        let l = parse(out).unwrap();
        assert_eq!(l.repos.len(), 2);
        assert_eq!(l.repos[1].url, "");
        assert_eq!(l.findings, ["a: x", "    y"]);
        assert_eq!(l.warnings, ["w"]);
        assert_eq!(l.meta.as_deref(), Some("id: x\ntitle: T\n"));
        assert_eq!(l.readme.as_deref(), Some("# T\n"));

        assert!(parse("R a\t/r\tmain\tmain\n").is_err(), "no DONE");
        assert!(parse("DONE\nmore\n").is_err());
        assert!(
            parse("sh: grep: not found\nDONE\n").is_err(),
            "stray output"
        );
        assert!(parse("R a\tb\nDONE\n").is_err());
        assert_eq!(parse("DONE\n").unwrap(), Look::default());
    }

    #[test]
    fn snapshots() {
        let found = |dir: &str, url: &str, branch: &str, default: &str| Found {
            dir: dir.into(),
            url: url.into(),
            branch: branch.into(),
            default: default.into(),
        };
        let prev = [RepoRecord {
            name: "work/svc".into(),
            remote: "https://h.x/a/svc".into(),
            branch: "old".into(),
        }];
        let cat = Catalog {
            entries: vec![Entry {
                name: "g/tool".into(),
                remote: "https://h.x/a/tool".into(),
            }],
            warnings: vec![],
        };
        let (r, w) = snapshot(
            &[
                found("svc", "git@h.x:a/svc.git", "feat", "main"),
                found("tool", "https://h.x/a/tool.git", "", "main"),
                found("mine", "https://h.x/a/mine", "x", ""),
                found("local", "", "main", ""),
                found("lost", "https://h.x/a/lost", "", ""),
            ],
            &prev,
            Some(&cat),
        );
        let got: Vec<(&str, &str, &str)> = r
            .iter()
            .map(|r| (r.name.as_str(), r.remote.as_str(), r.branch.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("work/svc", "https://h.x/a/svc", "feat"),
                ("g/tool", "https://h.x/a/tool", "main"),
                ("mine", "https://h.x/a/mine", "x"),
            ]
        );
        assert_eq!(w.len(), 3, "{w:?}");
    }
}
