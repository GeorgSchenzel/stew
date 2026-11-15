//! The skills dir the hub assembles for boxes (`~/.local/share/stew/skills/`, mounted
//! read-only as the agents' skills dir by bento's global config): the `stew` skill and hc's
//! stub, nothing else (Georg, 2026-11-15: no skills in notes). Boxes see a change at once,
//! so files are replaced in place (each by rename) and the dir itself is never swapped;
//! anything else in it (e.g. workflow skills an older stew took from the artifacts repo)
//! is removed.

use std::fs;
use std::path::Path;

use crate::error::{Result, fail};

/// hc's stub: it points at the installed hc's own manual (the release ships no skill file).
pub const HC_STUB: &str = include_str!("../../../skills/herdr-core/SKILL.md");

/// Writes the skills into `dir`.
pub fn assemble(dir: &Path) -> Result<()> {
    write(
        dir,
        &[
            ("stew", stew_proto::SKILL.as_bytes()),
            ("herdr-core", HC_STUB.as_bytes()),
        ],
    )
}

/// Each skill as `<name>/SKILL.md`; every other entry in `dir` and in the skills' dirs goes.
fn write(dir: &Path, skills: &[(&str, &[u8])]) -> Result<()> {
    fs::create_dir_all(dir).map_err(|e| fail!("{}: {e}", dir.display()))?;
    for (name, body) in skills {
        let sdir = dir.join(name);
        // never write through a link someone put there
        if fs::symlink_metadata(&sdir).is_ok_and(|m| !m.is_dir()) {
            fs::remove_file(&sdir)?;
        }
        fs::create_dir_all(&sdir)?;
        let path = sdir.join("SKILL.md");
        if fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file()) {
            remove(&path)?;
        }
        if fs::read(&path).ok().as_deref() != Some(body) {
            let tmp = sdir.join(".SKILL.md.tmp");
            fs::write(&tmp, body)?;
            fs::rename(&tmp, &path)?;
        }
        prune(&sdir, &["SKILL.md"])?;
    }
    let names: Vec<&str> = skills.iter().map(|(n, _)| *n).collect();
    prune(dir, &names)
}

/// Removes every entry of `dir` not named in `keep`.
fn prune(dir: &Path, keep: &[&str]) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !keep.iter().any(|k| entry.file_name() == *k) {
            remove(&entry.path())?;
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
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn writes_in_place_and_drops_everything_else() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skills");
        // what an older stew left: a workflow skill, its manifest, a stray file
        fs::create_dir_all(dir.join("notes")).unwrap();
        fs::write(dir.join("notes/SKILL.md"), "n").unwrap();
        fs::write(dir.join(".stew-skills"), "notes/SKILL.md\n").unwrap();
        fs::create_dir_all(dir.join("stew")).unwrap();
        fs::write(dir.join("stew/extra.md"), "x").unwrap();
        let dir_inode = fs::metadata(&dir).unwrap().ino();

        assemble(&dir).unwrap();
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["herdr-core", "stew"]);
        assert!(!dir.join("stew/extra.md").exists());
        assert_eq!(
            fs::read_to_string(dir.join("stew/SKILL.md")).unwrap(),
            stew_proto::SKILL
        );
        assert_eq!(
            fs::read_to_string(dir.join("herdr-core/SKILL.md")).unwrap(),
            HC_STUB
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().ino(),
            dir_inode,
            "the dir is kept"
        );

        let inode = |p: &str| fs::metadata(dir.join(p)).unwrap().ino();
        let before = inode("stew/SKILL.md");
        assemble(&dir).unwrap();
        assert_eq!(inode("stew/SKILL.md"), before, "unchanged files stay");
        fs::write(dir.join("stew/SKILL.md"), "old").unwrap();
        assemble(&dir).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("stew/SKILL.md")).unwrap(),
            stew_proto::SKILL
        );

        // a link planted where a skill goes is replaced, never written through
        let outside = tmp.path().join("outside");
        fs::write(&outside, "keep").unwrap();
        fs::remove_file(dir.join("stew/SKILL.md")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("stew/SKILL.md")).unwrap();
        let out_dir = tmp.path().join("outdir");
        fs::create_dir_all(&out_dir).unwrap();
        fs::write(out_dir.join("SKILL.md"), "keep").unwrap();
        fs::remove_dir_all(dir.join("herdr-core")).unwrap();
        std::os::unix::fs::symlink(&out_dir, dir.join("herdr-core")).unwrap();
        assemble(&dir).unwrap();
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        assert_eq!(
            fs::read_to_string(out_dir.join("SKILL.md")).unwrap(),
            "keep"
        );
        assert_eq!(
            fs::read_to_string(dir.join("stew/SKILL.md")).unwrap(),
            stew_proto::SKILL
        );
        assert!(
            fs::symlink_metadata(dir.join("herdr-core"))
                .unwrap()
                .is_dir()
        );
    }
}
