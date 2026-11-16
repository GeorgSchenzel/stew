//! The skills dir the hub assembles for boxes (`~/.local/share/stew/skills/`, copied into
//! both agents' skills dirs at each box's boot by bento's global config): the `stew` skill,
//! hc's stub, and the skills of Georg's skills repo (`skills:` in config.yaml; Georg,
//! 2026-11-16), each a top-level dir of it with a `SKILL.md`, copied whole. The hub keeps a
//! shallow checkout of that repo and updates it on `stew skills` (hourly). The dir is built
//! beside the old one and swapped in, so a box booting meanwhile copies one or the other.

use std::fs;
use std::path::Path;

use crate::error::{Result, fail};
use crate::git::git;

/// hc's stub: it points at the installed hc's own manual (the release ships no skill file).
pub const HC_STUB: &str = include_str!("../../../skills/herdr-core/SKILL.md");

/// stew's own skills: the repo can't replace them.
const OWN: &[&str] = &["stew", "herdr-core"];

/// Makes `checkout` a shallow clone of `remote` at its default branch's tip.
pub fn update_repo(checkout: &Path, remote: &str) -> Result<()> {
    if checkout.join(".git").is_dir() {
        if git(checkout, ["remote", "get-url", "origin"])? != remote {
            git(checkout, ["remote", "set-url", "origin", remote])?;
        }
        git(
            checkout,
            ["fetch", "--quiet", "--depth", "1", "origin", "HEAD"],
        )
        .map_err(|e| fail!("cannot fetch the skills repo: {e}"))?;
        git(checkout, ["reset", "--quiet", "--hard", "FETCH_HEAD"])?;
        return Ok(());
    }
    let parent = checkout
        .parent()
        .ok_or_else(|| fail!("bad skills checkout path"))?;
    fs::create_dir_all(parent)?;
    let _ = fs::remove_dir_all(checkout);
    let name = checkout.as_os_str();
    git(
        parent,
        [
            "clone".as_ref(),
            "--quiet".as_ref(),
            "--depth".as_ref(),
            "1".as_ref(),
            "--".as_ref(),
            remote.as_ref(),
            name,
        ],
    )
    .map_err(|e| fail!("cannot clone the skills repo: {e}"))?;
    Ok(())
}

/// Builds the skills into `dir`: stew's own, then `repo`'s (a checkout, when there is one).
/// Returns warnings (a repo skill named like one of stew's is left out).
pub fn assemble(dir: &Path, repo: Option<&Path>) -> Result<Vec<String>> {
    let parent = dir.parent().ok_or_else(|| fail!("bad skills dir"))?;
    fs::create_dir_all(parent)?;
    let name = dir
        .file_name()
        .ok_or_else(|| fail!("bad skills dir"))?
        .to_string_lossy();
    let new = parent.join(format!(".{name}.new"));
    let old = parent.join(format!(".{name}.old"));
    for p in [&new, &old] {
        if fs::symlink_metadata(p).is_ok() {
            remove(p)?;
        }
    }
    fs::create_dir_all(&new)?;
    for (skill, body) in [("stew", stew_proto::SKILL), ("herdr-core", HC_STUB)] {
        fs::create_dir_all(new.join(skill))?;
        fs::write(new.join(skill).join("SKILL.md"), body)?;
    }
    let mut warnings = Vec::new();
    if let Some(repo) = repo {
        let mut entries: Vec<_> = fs::read_dir(repo)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let skill = e.file_name().to_string_lossy().into_owned();
            // dirs only, never through a link, never git's own
            if skill.starts_with('.') || !e.file_type()?.is_dir() {
                continue;
            }
            if !e.path().join("SKILL.md").is_file() {
                continue;
            }
            if OWN.contains(&skill.as_str()) {
                warnings.push(format!(
                    "the skills repo's {skill} is left out (stew's own skill has that name)"
                ));
                continue;
            }
            copy_tree(&e.path(), &new.join(&skill))?;
        }
    }
    // the swap: the old dir (or whatever is there, never followed) goes aside, then away
    if fs::symlink_metadata(dir).is_ok() {
        fs::rename(dir, &old)?;
    }
    fs::rename(&new, dir)?;
    if fs::symlink_metadata(&old).is_ok() {
        remove(&old)?;
    }
    Ok(warnings)
}

/// Copies files and dirs; links are left out (one could point anywhere on the host).
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let ty = e.file_type()?;
        let to = dst.join(e.file_name());
        if ty.is_dir() {
            copy_tree(&e.path(), &to)?;
        } else if ty.is_file() {
            fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

/// A file, link or dir, never following a link.
fn remove(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(dir: &Path) -> Vec<String> {
        let mut n: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        n.sort();
        n
    }

    #[test]
    fn own_skills_and_the_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skills");
        // what an older stew left: a workflow skill and a stray file
        fs::create_dir_all(dir.join("notes")).unwrap();
        fs::write(dir.join("notes/SKILL.md"), "n").unwrap();
        fs::write(dir.join("stray"), "x").unwrap();

        assert!(assemble(&dir, None).unwrap().is_empty());
        assert_eq!(names(&dir), ["herdr-core", "stew"]);
        assert_eq!(
            fs::read_to_string(dir.join("stew/SKILL.md")).unwrap(),
            stew_proto::SKILL
        );
        assert_eq!(
            fs::read_to_string(dir.join("herdr-core/SKILL.md")).unwrap(),
            HC_STUB
        );
        assert_eq!(names(tmp.path()), ["skills"], "nothing left beside it");

        // a repo: skills copied whole; no SKILL.md, files, dot dirs and links left out;
        // stew's names stay stew's
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("review/scripts")).unwrap();
        fs::write(repo.join("review/SKILL.md"), "r").unwrap();
        fs::write(repo.join("review/scripts/run.sh"), "echo").unwrap();
        fs::create_dir_all(repo.join("drafts")).unwrap();
        fs::write(repo.join("drafts/notes.md"), "d").unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(".git/SKILL.md"), "g").unwrap();
        fs::write(repo.join("README.md"), "readme").unwrap();
        fs::create_dir_all(repo.join("stew")).unwrap();
        fs::write(repo.join("stew/SKILL.md"), "not stew's").unwrap();
        let outside = tmp.path().join("outside");
        fs::write(&outside, "secret").unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("review/leak")).unwrap();

        let w = assemble(&dir, Some(&repo)).unwrap();
        assert_eq!(names(&dir), ["herdr-core", "review", "stew"]);
        assert_eq!(
            fs::read_to_string(dir.join("review/scripts/run.sh")).unwrap(),
            "echo"
        );
        assert!(!dir.join("review/leak").exists());
        assert_eq!(
            fs::read_to_string(dir.join("stew/SKILL.md")).unwrap(),
            stew_proto::SKILL
        );
        assert!(w.len() == 1 && w[0].contains("stew"), "{w:?}");

        // the dir itself a link: replaced, its target untouched
        fs::remove_dir_all(&dir).unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("keep"), "k").unwrap();
        std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();
        assemble(&dir, None).unwrap();
        assert!(fs::symlink_metadata(&dir).unwrap().is_dir());
        assert_eq!(fs::read_to_string(elsewhere.join("keep")).unwrap(), "k");
    }

    #[test]
    fn the_repo_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote");
        let g = |dir: &Path, args: &[&str]| git(dir, args).unwrap();
        fs::create_dir_all(&remote).unwrap();
        g(&remote, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(remote.join("a")).unwrap();
        fs::write(remote.join("a/SKILL.md"), "one").unwrap();
        g(&remote, &["add", "."]);
        g(
            &remote,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "1",
            ],
        );
        let url = remote.to_string_lossy().into_owned();
        let co = tmp.path().join("hub/skills-repo");
        update_repo(&co, &url).unwrap();
        assert_eq!(fs::read_to_string(co.join("a/SKILL.md")).unwrap(), "one");
        fs::write(remote.join("a/SKILL.md"), "two").unwrap();
        g(
            &remote,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qam",
                "2",
            ],
        );
        update_repo(&co, &url).unwrap();
        assert_eq!(fs::read_to_string(co.join("a/SKILL.md")).unwrap(), "two");
        let gone = tmp.path().join("missing");
        let e = update_repo(&co, &gone.to_string_lossy()).unwrap_err();
        assert!(e.to_string().contains("cannot fetch"), "{e}");
    }
}
