//! The skills dir the hub assembles for boxes (`~/.local/share/stew/skills/`, mounted
//! read-only as the agents' skills dir by bento's global config): the `stew` skill and hc's
//! stub from the installed tools, plus `skills/` of the artifacts repo's `main` (the
//! workflow skills). Boxes see a change at once, so files are replaced in place (each by
//! rename) and the dir itself is never swapped.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path};

use crate::error::{Result, fail};
use crate::git::{git, git_with};
use crate::mirror::Mirror;

/// hc's stub: it points at the installed hc's own manual (the release ships no skill file).
pub const HC_STUB: &str = "---
name: herdr-core
description: Use herdr-core (its CLI is hc) for sub-agents, to talk to or coordinate other
  agents, run background tasks or servers, long-running commands, or read and control
  terminals. Prefer it over your own sub-agent tool, shell '&', nohup or tmux.
---
This skill is for herdr-core, whose command is `hc`. Before using hc, run `hc --skill` and
follow it.
";

/// The skills stew writes itself; the artifacts repo can't replace them.
const OWN: &[&str] = &["stew", "herdr-core"];

/// Lists what the dir holds from the last assembly, so a skill dropped upstream goes too.
const MANIFEST: &str = ".stew-skills";

const MAIN: &str = "refs/skills/main";

/// Writes the skills into `dir`; returns warnings (an unreachable artifacts repo is one:
/// the last fetched `main` is used).
pub fn assemble(dir: &Path, mirror: Option<&Mirror>) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let mut files: Vec<(String, Vec<u8>)> = vec![
        (
            "stew/SKILL.md".into(),
            stew_proto::SKILL.as_bytes().to_vec(),
        ),
        ("herdr-core/SKILL.md".into(), HC_STUB.as_bytes().to_vec()),
    ];
    if let Some(m) = mirror {
        files.extend(workflow(m, &mut warnings)?);
    }
    write(dir, &files)?;
    Ok(warnings)
}

/// `skills/<name>/…` of the artifacts repo's `main`: regular files only.
fn workflow(m: &Mirror, warnings: &mut Vec<String>) -> Result<Vec<(String, Vec<u8>)>> {
    m.ensure()?;
    if let Err(e) = git(
        &m.dir,
        [
            "fetch",
            "--quiet",
            "--no-tags",
            "origin",
            &format!("+refs/heads/main:{MAIN}"),
        ],
    ) {
        warnings.push(format!(
            "the artifacts repo's main: {e} (using the last fetched)"
        ));
    }
    if git(&m.dir, ["rev-parse", "--verify", "--quiet", MAIN]).is_err() {
        return Ok(vec![]);
    }
    let listing = git_with(
        &m.dir,
        ["ls-tree", "-r", "-z", "--full-tree", MAIN, "--", "skills/"],
        &[],
        None,
    )?;
    let mut out = Vec::new();
    for entry in listing.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        let entry = String::from_utf8_lossy(entry);
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let mut words = meta.split(' ');
        let (mode, sha) = (words.next().unwrap_or(""), words.nth(1).unwrap_or(""));
        let Some(rel) = path.strip_prefix("skills/") else {
            continue;
        };
        if !safe(rel) {
            warnings.push(format!("skills/{rel}: skipped (not a plain path)"));
            continue;
        }
        let top = rel.split('/').next().unwrap_or("");
        if OWN.contains(&top) || !rel.contains('/') {
            warnings.push(format!(
                "skills/{rel}: skipped (stew's own, or not in a skill dir)"
            ));
            continue;
        }
        if mode != "100644" && mode != "100755" {
            warnings.push(format!("skills/{rel}: skipped (not a regular file)"));
            continue;
        }
        let body = git_with(&m.dir, ["cat-file", "blob", sha], &[], None)?;
        out.push((rel.to_string(), body));
    }
    Ok(out)
}

/// Plain relative components only.
fn safe(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('.')
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(n) if !n.to_string_lossy().starts_with('.')))
}

fn write(dir: &Path, files: &[(String, Vec<u8>)]) -> Result<()> {
    fs::create_dir_all(dir).map_err(|e| fail!("{}: {e}", dir.display()))?;
    let before: BTreeSet<String> = fs::read_to_string(dir.join(MANIFEST))
        .unwrap_or_default()
        .lines()
        .filter(|l| safe(l))
        .map(String::from)
        .collect();
    for (rel, body) in files {
        let path = dir.join(rel);
        let parent = path.parent().expect("a file in a skill dir");
        // never write through a link someone put there
        for c in Path::new(rel).ancestors().skip(1) {
            if c.as_os_str().is_empty() {
                break;
            }
            if fs::symlink_metadata(dir.join(c)).is_ok_and(|m| !m.is_dir()) {
                return Err(fail!("{}: not a dir", dir.join(c).display()));
            }
        }
        fs::create_dir_all(parent)?;
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            fs::remove_file(&path)?;
        }
        if fs::read(&path).ok().as_deref() == Some(&body[..]) {
            continue;
        }
        let tmp = parent.join(format!(
            ".{}.tmp",
            path.file_name().unwrap().to_string_lossy()
        ));
        fs::write(&tmp, body)?;
        fs::rename(&tmp, &path)?;
    }
    let now: BTreeSet<String> = files.iter().map(|(r, _)| r.clone()).collect();
    for gone in before.difference(&now) {
        let path = dir.join(gone);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            fs::remove_file(&path)?;
        }
        // drop dirs left empty, up to the skills dir
        let mut d = path.parent();
        while let Some(p) = d {
            if p == dir || fs::remove_dir(p).is_err() {
                break;
            }
            d = p.parent();
        }
    }
    let mut list: Vec<&str> = now.iter().map(String::as_str).collect();
    list.push("");
    let tmp = dir.join(format!("{MANIFEST}.tmp"));
    fs::write(&tmp, list.join("\n"))?;
    fs::rename(&tmp, dir.join(MANIFEST))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_in_place_and_drops_stale_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skills");
        let f = |r: &str, b: &str| (r.to_string(), b.as_bytes().to_vec());
        write(
            &dir,
            &[
                f("stew/SKILL.md", "a"),
                f("notes/SKILL.md", "n"),
                f("x/y/z.md", "z"),
            ],
        )
        .unwrap();
        let inode = |p: &str| {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(dir.join(p)).unwrap().ino()
        };
        let dir_inode = fs::metadata(&dir).unwrap();
        let before = inode("stew/SKILL.md");
        write(&dir, &[f("stew/SKILL.md", "a"), f("x/y/z.md", "z2")]).unwrap();
        assert_eq!(
            inode("stew/SKILL.md"),
            before,
            "unchanged files stay as they are"
        );
        assert_eq!(fs::read_to_string(dir.join("x/y/z.md")).unwrap(), "z2");
        assert!(!dir.join("notes").exists(), "a dropped skill goes");
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(&dir).unwrap().ino(),
            dir_inode.ino(),
            "the dir is kept"
        );
        // a link planted where a skill goes is replaced, never written through
        let outside = tmp.path().join("outside");
        fs::write(&outside, "keep").unwrap();
        fs::remove_file(dir.join("stew/SKILL.md")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("stew/SKILL.md")).unwrap();
        write(&dir, &[f("stew/SKILL.md", "a")]).unwrap();
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        assert_eq!(fs::read_to_string(dir.join("stew/SKILL.md")).unwrap(), "a");
    }

    #[test]
    fn plain_paths_only() {
        for ok in ["notes/SKILL.md", "a/b/c.md"] {
            assert!(safe(ok), "{ok}");
        }
        for bad in ["", "../x", "a/../b", "/abs", ".hidden/x", "a/.git/x", "./a"] {
            assert!(!safe(bad), "{bad}");
        }
    }
}
