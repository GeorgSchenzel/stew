//! `~/.config/stew/config.yaml`.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::{Result, fail};
use crate::paths::Paths;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The artifacts repo (every stew's `stew/<id>` branch).
    pub artifacts: Option<String>,
    /// The transcripts repo.
    pub transcripts: Option<String>,
    /// The site new stews go to.
    pub site: Option<String>,
    /// Clones whose `origin` joins the catalog: a dir, or `false`. Default
    /// `~/projects/repos` when it exists.
    pub scan: Option<Scan>,
    pub agent: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Scan {
    On(bool),
    Dir(String),
}

impl Config {
    /// The config file, or the defaults when there is none.
    pub fn load(paths: &Paths) -> Result<Self> {
        let file = paths.config_file();
        match fs::read_to_string(&file) {
            Ok(text) if text.trim().is_empty() => Ok(Config::default()),
            Ok(text) => serde_yaml::from_str(&text).map_err(|e| fail!("{}: {e}", file.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(fail!("cannot read {}: {e}", file.display())),
        }
    }

    pub fn artifacts(&self, paths: &Paths) -> Result<&str> {
        self.artifacts.as_deref().ok_or_else(|| {
            fail!(
                "no artifacts repo: set `artifacts:` in {}",
                paths.config_file().display()
            )
        })
    }

    /// The dir to scan for clones, if any.
    pub fn scan_dir(&self, paths: &Paths) -> Option<PathBuf> {
        let default = paths.home.join("projects/repos");
        match &self.scan {
            None | Some(Scan::On(true)) => default.is_dir().then_some(default),
            Some(Scan::On(false)) => None,
            Some(Scan::Dir(d)) => Some(paths.expand(d)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        let c: Config = serde_yaml::from_str(
            "artifacts: git@gitlab.com:a/notes.git\nscan: false\nsite: desktop\nextra: 1\n",
        )
        .unwrap();
        assert_eq!(c.artifacts.as_deref(), Some("git@gitlab.com:a/notes.git"));
        assert!(matches!(c.scan, Some(Scan::On(false))));
        let c: Config = serde_yaml::from_str("scan: ~/src\n").unwrap();
        let p = Paths::under(std::path::Path::new("/h"));
        assert_eq!(c.scan_dir(&p), Some(PathBuf::from("/h/src")));
    }
}
