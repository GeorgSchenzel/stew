//! The hub's bare mirror of the artifacts repo: claiming `stew/<id>` and `ls --all`.
//!
//! Claiming makes a stew's first commit (`stew.yaml`, and `BRIEF.md` with a brief) with
//! `commit-tree`, without a checkout, and pushes it only if the branch doesn't exist yet
//! (`--force-with-lease=<ref>:`). A rejected push means the id is taken: retry with new
//! random chars. A creation that fails later leaves the branch as its record.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use stew_proto::{StewMeta, Tracked};

use crate::error::{Result, fail};
use crate::git::{git, git_ok, git_with};
use crate::id;

/// How many ids to try before giving up.
const CLAIM_ATTEMPTS: usize = 8;

/// The claim commit's author when git has no identity configured.
const IDENTITY: (&str, &str) = ("stew", "stew@localhost");

pub struct Mirror {
    pub dir: PathBuf,
    pub remote: String,
}

pub fn branch(id: &str) -> String {
    format!("refs/heads/stew/{id}")
}

pub fn meta_yaml(meta: &StewMeta) -> String {
    serde_yaml::to_string(meta).expect("StewMeta serializes")
}

pub fn parse_meta(text: &str) -> Result<StewMeta> {
    serde_yaml::from_str(text).map_err(|e| fail!("stew.yaml: {e}"))
}

impl Mirror {
    pub fn new(dir: PathBuf, remote: String) -> Self {
        Mirror { dir, remote }
    }

    /// Creates the mirror on first use and points it at the configured remote.
    pub fn ensure(&self) -> Result<()> {
        if !self.dir.join("HEAD").exists() {
            let parent = self.dir.parent().ok_or_else(|| fail!("bad mirror path"))?;
            fs::create_dir_all(parent)?;
            let dir = self.dir.to_string_lossy();
            git(parent, ["init", "--quiet", "--bare", "--", &dir])?;
            git(&self.dir, ["remote", "add", "origin", &self.remote])?;
            git(
                &self.dir,
                [
                    "config",
                    "remote.origin.fetch",
                    "+refs/heads/stew/*:refs/heads/stew/*",
                ],
            )?;
        } else if git(&self.dir, ["remote", "get-url", "origin"])? != self.remote {
            git(&self.dir, ["remote", "set-url", "origin", &self.remote])?;
        }
        Ok(())
    }

    /// Brings every `stew/*` branch up to date with the remote.
    pub fn fetch(&self) -> Result<()> {
        git(&self.dir, ["fetch", "--quiet", "--prune", "origin"])
            .map(|_| ())
            .map_err(|e| fail!("cannot fetch the artifacts repo: {e}"))
    }

    /// Claims a fresh id for `meta` (its `id` is replaced) and returns it.
    pub fn claim(&self, meta: &StewMeta, brief: Option<&str>) -> Result<String> {
        self.claim_with(meta, brief, || id::new_id(&meta.title))
    }

    /// `claim`, with the ids to try coming from `next_id`.
    pub fn claim_with(
        &self,
        meta: &StewMeta,
        brief: Option<&str>,
        mut next_id: impl FnMut() -> Result<String>,
    ) -> Result<String> {
        self.ensure()?;
        for _ in 0..CLAIM_ATTEMPTS {
            let id = next_id()?;
            id::check(&id)?;
            let refname = branch(&id);
            if git_ok(&self.dir, ["rev-parse", "--verify", "--quiet", &refname]) {
                continue;
            }
            let meta = StewMeta {
                id: id.clone(),
                ..meta.clone()
            };
            let commit = self.first_commit(&meta, brief)?;
            let pushed = git(
                &self.dir,
                [
                    "push",
                    "--porcelain",
                    &format!("--force-with-lease={refname}:"),
                    "origin",
                    &format!("{commit}:{refname}"),
                ],
            );
            match pushed {
                // `*`: the ref is new. Anything else (`=`: an identical commit is already
                // there, same title in the same second) means someone else has the id.
                Ok(out) if out.lines().any(|l| l.starts_with("*\t")) => {
                    git(&self.dir, ["update-ref", &refname, &commit])?;
                    return Ok(id);
                }
                Ok(_) => {}
                Err(e) => {
                    // taken (or raced): the remote has it now; anything else is a real failure
                    let there = git(&self.dir, ["ls-remote", "origin", &refname])?;
                    if there.is_empty() {
                        return Err(fail!("cannot claim stew/{id}: {e}"));
                    }
                }
            }
        }
        Err(fail!("no free id after {CLAIM_ATTEMPTS} tries"))
    }

    fn first_commit(&self, meta: &StewMeta, brief: Option<&str>) -> Result<String> {
        let mut tree = String::new();
        let mut files = vec![("stew.yaml", meta_yaml(meta))];
        if let Some(b) = brief {
            files.push(("BRIEF.md", b.to_string()));
        }
        files.sort_by_key(|(name, _)| *name);
        for (name, content) in &files {
            let sha = self.write(["hash-object", "-w", "--stdin"], content.as_bytes())?;
            tree.push_str(&format!("100644 blob {sha}\t{name}\n"));
        }
        let tree = self.write(["mktree"], tree.as_bytes())?;
        let subject = format!("created: {}", meta.title);
        let mut env = Vec::new();
        if !git_ok(&self.dir, ["config", "user.name"])
            || !git_ok(&self.dir, ["config", "user.email"])
        {
            env = vec![
                ("GIT_AUTHOR_NAME", IDENTITY.0),
                ("GIT_AUTHOR_EMAIL", IDENTITY.1),
                ("GIT_COMMITTER_NAME", IDENTITY.0),
                ("GIT_COMMITTER_EMAIL", IDENTITY.1),
            ];
        }
        let out = git_with(
            &self.dir,
            ["commit-tree", &tree, "-m", &subject],
            &env,
            None,
        )?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    fn write<const N: usize>(&self, args: [&str; N], input: &[u8]) -> Result<String> {
        let out = git_with(&self.dir, args, &[], Some(input))?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// Every tracked stew, most recent first. With `search`, those whose id, title or
    /// any notes file contains it (case-insensitive). Unreadable `stew.yaml` files are
    /// skipped with a warning.
    pub fn tracked(&self, search: Option<&str>) -> Result<(Vec<Tracked>, Vec<String>)> {
        let listing = git(
            &self.dir,
            [
                "for-each-ref",
                "--sort=-committerdate",
                "--format=%(refname:lstrip=3)%00%(committerdate:unix)%00%(contents:subject)",
                "refs/heads/stew/",
            ],
        )?;
        let mut rows = Vec::new();
        for line in listing.lines() {
            let mut parts = line.splitn(3, '\0');
            let (Some(id), Some(time), Some(subject)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            rows.push((
                id.to_string(),
                time.parse::<i64>().unwrap_or(0),
                subject.to_string(),
            ));
        }
        let ids: Vec<&str> = rows.iter().map(|(id, _, _)| id.as_str()).collect();
        let metas = self.read_metas(&ids)?;
        let mut warnings = Vec::new();
        let mut out = Vec::new();
        for ((id, updated, status), meta) in rows.iter().zip(metas) {
            match meta {
                Ok(meta) => out.push(Tracked {
                    meta,
                    updated: *updated,
                    status: status.clone(),
                }),
                Err(e) => warnings.push(format!("stew/{id}: {e}")),
            }
        }
        if let Some(term) = search.map(str::trim).filter(|t| !t.is_empty()) {
            let lower = term.to_lowercase();
            let hit = |t: &Tracked| {
                t.meta.id.contains(&lower) || t.meta.title.to_lowercase().contains(&lower)
            };
            let rest: Vec<&str> = out
                .iter()
                .filter(|t| !hit(t))
                .map(|t| t.meta.id.as_str())
                .collect();
            let in_notes = self.grep(term, &rest)?;
            out.retain(|t| hit(t) || in_notes.contains(&t.meta.id));
        }
        Ok((out, warnings))
    }

    /// Each id's `stew.yaml`, in one `cat-file --batch`.
    fn read_metas(&self, ids: &[&str]) -> Result<Vec<Result<StewMeta>>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let input: String = ids
            .iter()
            .map(|id| format!("{}:stew.yaml\n", branch(id)))
            .collect();
        let out = git_with(
            &self.dir,
            ["cat-file", "--batch"],
            &[],
            Some(input.as_bytes()),
        )?;
        let mut rest = &out[..];
        let mut metas = Vec::new();
        for _ in ids {
            let nl = rest
                .iter()
                .position(|&b| b == b'\n')
                .ok_or_else(|| fail!("short cat-file output"))?;
            let header = String::from_utf8_lossy(&rest[..nl]).to_string();
            rest = &rest[nl + 1..];
            let mut words = header.split(' ');
            match (words.next(), words.next(), words.next()) {
                (Some(_), Some("blob"), Some(size)) => {
                    let size: usize = size.parse().map_err(|_| fail!("bad cat-file header"))?;
                    let body = rest
                        .get(..size)
                        .ok_or_else(|| fail!("short cat-file output"))?;
                    metas.push(parse_meta(&String::from_utf8_lossy(body)));
                    rest = rest.get(size + 1..).unwrap_or_default();
                }
                _ => metas.push(Err(fail!("no stew.yaml"))),
            }
        }
        Ok(metas)
    }

    /// The ids (of those given) whose notes contain `term`.
    fn grep(&self, term: &str, ids: &[&str]) -> Result<BTreeSet<String>> {
        let mut found = BTreeSet::new();
        for chunk in ids.chunks(200) {
            let mut args: Vec<String> = ["grep", "-l", "-I", "-i", "-F", "-e", term]
                .map(String::from)
                .to_vec();
            args.extend(chunk.iter().map(|id| branch(id)));
            args.push("--".into());
            // exit 1 means no match
            let Ok(out) = git(&self.dir, &args) else {
                continue;
            };
            for line in out.lines() {
                if let Some(id) = line
                    .strip_prefix("refs/heads/stew/")
                    .and_then(|l| l.split(':').next())
                {
                    found.insert(id.to_string());
                }
            }
        }
        Ok(found)
    }
}
