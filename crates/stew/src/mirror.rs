//! The hub's bare mirror of the artifacts repo: claiming `stew/<id>` and `ls --all`.
//!
//! Claiming makes a stew's first commit (`stew.yaml`, `README.md`, `LOG.md`, and `BRIEF.md`
//! with a brief) with
//! `commit-tree`, without a checkout, and pushes it only if the branch doesn't exist yet
//! (`--force-with-lease=<ref>:`). A rejected push means the id is taken: retry with new
//! random chars, as for an id whose short form (its random part) another stew has. A creation that fails later leaves the branch as its record.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use stew_proto::{StewMeta, Tracked};

use crate::error::{Result, fail};
use crate::git::{git, git_ok, git_with};
use crate::{id, readme};

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
        // the short forms in use, as fresh as can be (a failed fetch fails the push below
        // too, or the push decides)
        let _ = self.fetch();
        let taken: BTreeSet<String> = self
            .ids()?
            .iter()
            .map(|i| id::suffix(i).to_string())
            .collect();
        for _ in 0..CLAIM_ATTEMPTS {
            let id = next_id()?;
            id::check(&id)?;
            let refname = branch(&id);
            if taken.contains(id::suffix(&id))
                || git_ok(&self.dir, ["rev-parse", "--verify", "--quiet", &refname])
            {
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
        let mut files = vec![
            ("stew.yaml", meta_yaml(meta)),
            ("README.md", readme::initial_readme(meta)),
            ("LOG.md", readme::initial_log(meta)),
        ];
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

    /// The `from` in the claim commit (the branch's only root) of `stew/<id>`: fixed when
    /// the daemon claimed the id, so a later push onto the branch doesn't change it. `None`
    /// when the branch has no single root whose `stew.yaml` names this id.
    pub fn claimed_from(&self, id: &str) -> Result<Option<String>> {
        let roots = git(
            &self.dir,
            ["rev-list", "--max-parents=0", &branch(id), "--"],
        )?;
        let mut roots = roots.lines();
        let (Some(root), None) = (roots.next(), roots.next()) else {
            return Ok(None);
        };
        let Ok(text) = git(&self.dir, ["show", &format!("{root}:stew.yaml")]) else {
            return Ok(None);
        };
        Ok(parse_meta(&text)
            .ok()
            .filter(|m| m.id == id)
            .and_then(|m| m.from))
    }

    /// Tracked stews, most recent first: the open ones, or with `closed` the closed
    /// ones. With `search`, those of either kind whose id, title or any notes file
    /// contains it (case-insensitive). Unreadable `stew.yaml` files are skipped with a
    /// warning.
    pub fn tracked(
        &self,
        search: Option<&str>,
        closed: bool,
    ) -> Result<(Vec<Tracked>, Vec<String>)> {
        let search = search.map(str::trim).filter(|t| !t.is_empty());
        let (mut out, warnings) =
            self.load(|meta| search.is_some() || meta.is_closed() == closed)?;
        if let Some(term) = search {
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

    /// The ids of every `stew/*` branch the mirror has (no fetch).
    pub fn ids(&self) -> Result<Vec<String>> {
        let out = git(
            &self.dir,
            [
                "for-each-ref",
                "--format=%(refname:lstrip=3)",
                "refs/heads/stew/",
            ],
        )?;
        Ok(out.lines().map(String::from).collect())
    }

    /// Every tracked stew, open or closed, most recent first.
    pub fn every(&self) -> Result<(Vec<Tracked>, Vec<String>)> {
        self.load(|_| true)
    }

    /// The tracked stews `keep` accepts, most recent first, with warnings for unreadable
    /// `stew.yaml` files.
    fn load(&self, keep: impl Fn(&StewMeta) -> bool) -> Result<(Vec<Tracked>, Vec<String>)> {
        let listing = git(
            &self.dir,
            [
                "for-each-ref",
                "--sort=-committerdate",
                "--format=%(refname:lstrip=3) %(committerdate:unix)",
                "refs/heads/stew/",
            ],
        )?;
        let rows: Vec<(&str, i64)> = listing
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(id, time)| (id, time.parse().unwrap_or(0)))
            .collect();
        let ids: Vec<&str> = rows.iter().map(|(id, _)| *id).collect();
        let files = self.read_files(&ids, &["stew.yaml", "README.md"])?;
        let mut warnings = Vec::new();
        let mut out = Vec::new();
        for ((id, updated), mut found) in rows.iter().zip(files) {
            let readme = found.pop().flatten();
            let meta = match found.pop().flatten() {
                Some(text) => parse_meta(&text),
                None => Err(fail!("no stew.yaml")),
            };
            // a branch speaks only for its own id: a `stew.yaml` copied (or pushed) into
            // another branch never stands in for the stew it names
            let meta = match meta {
                Ok(m) if m.id != *id => {
                    warnings.push(format!("stew/{id}: its stew.yaml names {} (skipped)", m.id));
                    continue;
                }
                Ok(m) => m,
                Err(e) => {
                    warnings.push(format!("stew/{id}: {e}"));
                    continue;
                }
            };
            if !keep(&meta) {
                continue;
            }
            let summary = readme::summarize(readme.as_deref().unwrap_or_default());
            out.push(Tracked {
                meta,
                updated: *updated,
                description: summary.description,
                now: summary.now,
            });
        }
        Ok((out, warnings))
    }

    /// For each id, each of `names` on its branch (`None` when missing), in one
    /// `cat-file --batch`.
    fn read_files(&self, ids: &[&str], names: &[&str]) -> Result<Vec<Vec<Option<String>>>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut input = String::new();
        for id in ids {
            for name in names {
                input.push_str(&format!("{}:{name}\n", branch(id)));
            }
        }
        let out = git_with(
            &self.dir,
            ["cat-file", "--batch"],
            &[],
            Some(input.as_bytes()),
        )?;
        let mut rest = &out[..];
        let mut all = Vec::new();
        for _ in ids {
            let mut files = Vec::new();
            for _ in names {
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
                        files.push(Some(String::from_utf8_lossy(body).into_owned()));
                        rest = rest.get(size + 1..).unwrap_or_default();
                    }
                    // `<name> missing`, or not a blob
                    (Some(_), Some(_), Some(size)) => {
                        let size: usize = size.parse().unwrap_or(0);
                        rest = rest.get(size + 1..).unwrap_or_default();
                        files.push(None);
                    }
                    _ => files.push(None),
                }
            }
            all.push(files);
        }
        Ok(all)
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
