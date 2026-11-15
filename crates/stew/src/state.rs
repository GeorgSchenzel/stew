//! The daemon's state: each stew's site and box token, creations in progress and failed
//! ones. One JSON file, replaced by rename, so a crash leaves the old or the new file,
//! never half of one.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, fail};

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// Stews that exist on a site, by id.
    #[serde(default)]
    pub stews: BTreeMap<String, Placed>,
    /// Creations (`new`, `open`) that are running or failed, by id.
    #[serde(default)]
    pub creations: BTreeMap<String, Creation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placed {
    pub site: String,
    pub token: String,
    /// The stew it was started from, as recorded here at `new` (the caller) or `open` (its
    /// own branch's `stew.yaml`): what lets that stew start agents in it, never what a
    /// box-writable branch says later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Creation {
    pub state: Progress,
    /// Unix time it began.
    pub started: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Progress {
    Running,
    Failed,
}

pub struct StateFile {
    path: PathBuf,
    pub state: State,
}

impl StateFile {
    /// Reads the file (none yet: an empty state).
    pub fn load(path: &Path) -> Result<Self> {
        let state = match fs::read_to_string(path) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|e| fail!("{}: {e}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(fail!("cannot read {}: {e}", path.display())),
        };
        Ok(StateFile {
            path: path.to_path_buf(),
            state,
        })
    }

    /// Marks creations that were running when the last daemon stopped as failed.
    /// Returns their ids.
    pub fn recover(&mut self) -> Vec<String> {
        let mut ids = Vec::new();
        for (id, c) in &mut self.state.creations {
            if c.state == Progress::Running {
                c.state = Progress::Failed;
                c.error = Some("the daemon restarted during the creation".into());
                ids.push(id.clone());
            }
        }
        ids
    }

    pub fn in_progress(&self) -> bool {
        self.state
            .creations
            .values()
            .any(|c| c.state == Progress::Running)
    }

    /// Writes the state to a temp file next to it, then renames it over the old one.
    pub fn save(&self) -> Result<()> {
        let dir = self.path.parent().ok_or_else(|| fail!("bad state path"))?;
        fs::create_dir_all(dir)?;
        // it holds the stews' tokens: owner only
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        let tmp = self.path.with_extension("json.tmp");
        let mut text = serde_json::to_vec_pretty(&self.state).expect("State serializes");
        text.push(b'\n');
        let write = || -> std::io::Result<()> {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
            f.write_all(&text)?;
            f.sync_all()?;
            fs::rename(&tmp, &self.path)?;
            File::open(dir)?.sync_all()
        };
        write().map_err(|e| fail!("cannot write {}: {e}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_recover() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/state.json");
        let mut f = StateFile::load(&path).unwrap();
        assert_eq!(f.state, State::default());
        f.state.stews.insert(
            "a-k3fq".into(),
            Placed {
                site: "desktop".into(),
                token: "t".into(),
                from: None,
            },
        );
        for (id, state) in [("b-0000", Progress::Running), ("c-0000", Progress::Failed)] {
            f.state.creations.insert(
                id.into(),
                Creation {
                    state,
                    started: 1,
                    error: None,
                },
            );
        }
        assert!(f.in_progress());
        f.save().unwrap();
        assert!(!path.with_extension("json.tmp").exists());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let mut g = StateFile::load(&path).unwrap();
        assert_eq!(g.state, f.state);
        assert_eq!(g.recover(), ["b-0000"]);
        assert!(!g.in_progress());
        assert!(
            g.state.creations["b-0000"]
                .error
                .as_deref()
                .unwrap()
                .contains("restarted")
        );
        assert_eq!(g.state.creations["c-0000"].error, None);
    }

    #[test]
    fn corrupt_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        fs::write(&path, "{").unwrap();
        assert!(StateFile::load(&path).is_err());
    }
}
