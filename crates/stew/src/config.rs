//! `~/.config/stew/config.yaml`.

use std::collections::BTreeMap;
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
    /// Georg's skills repo: its top-level dirs with a `SKILL.md` go into boxes' skills.
    pub skills: Option<String>,
    /// The site new stews go to.
    pub site: Option<String>,
    /// Clones whose `origin` joins the catalog: a dir, or `false`. Default
    /// `~/projects/repos` when it exists.
    pub scan: Option<Scan>,
    /// The agent `--start` and `agent` use when none is given (default `claude`).
    pub agent: Option<String>,
    /// The model for that agent (passed through as `--model`).
    pub model: Option<String>,
    /// claude's account when a call names none (bento's preset: `CLAUDE_ACCOUNT_<NAME>_TOKEN`
    /// where the stew lives); none: the box's default account.
    pub account: Option<String>,
    /// The agent runtime: `hc` (default) or `none` (no machines, no agents).
    pub runtime: Option<RuntimeKind>,
    /// Where stews live, by name. None: `desktop` (bento) and `desktop-host` (host).
    pub sites: Option<BTreeMap<String, Site>>,
}

/// A site: a driver and its settings.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Site {
    pub driver: DriverKind,
    /// bento: the command words that reach it (`ssh mac`), put before `bento`.
    #[serde(default)]
    pub via: Option<String>,
    /// host: the dir that holds the stews' roots (default `~/projects/stews`).
    #[serde(default)]
    pub root: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverKind {
    Bento,
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    Hc,
    None,
}

/// The site `new` uses when none is given and the config names none.
pub const DEFAULT_SITE: &str = "desktop";

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

    /// Every site: the configured ones, else the defaults.
    pub fn sites(&self) -> BTreeMap<String, Site> {
        self.sites.clone().unwrap_or_else(|| {
            let site = |driver| Site {
                driver,
                via: None,
                root: None,
            };
            BTreeMap::from([
                ("desktop".to_string(), site(DriverKind::Bento)),
                ("desktop-host".to_string(), site(DriverKind::Host)),
            ])
        })
    }

    /// The named site, or the default one.
    pub fn site(&self, name: Option<&str>) -> Result<(String, Site)> {
        let name = name
            .or(self.site.as_deref())
            .unwrap_or(DEFAULT_SITE)
            .to_string();
        match self.sites().remove(&name) {
            Some(s) => Ok((name, s)),
            None => Err(fail!(
                "no site '{name}' (sites: {})",
                self.sites().into_keys().collect::<Vec<_>>().join(", ")
            )),
        }
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
        assert_eq!(c.site(None).unwrap().0, "desktop");
        assert_eq!(c.site(None).unwrap().1.driver, DriverKind::Bento);
        assert!(c.site(Some("nope")).is_err());
        let c: Config = serde_yaml::from_str(
            "sites:\n  mac: {driver: bento, via: ssh mac}\n  h: {driver: host, root: /x}\nsite: h\n",
        )
        .unwrap();
        assert_eq!(c.site(None).unwrap().1.root.as_deref(), Some("/x"));
        assert_eq!(
            c.site(Some("mac")).unwrap().1.via.as_deref(),
            Some("ssh mac")
        );
        assert!(c.site(Some("desktop")).is_err());
        assert!(serde_yaml::from_str::<Config>("sites: {a: {driver: nono}}").is_err());
        let c: Config = serde_yaml::from_str("runtime: none\n").unwrap();
        assert_eq!(c.runtime, Some(RuntimeKind::None));
        assert!(serde_yaml::from_str::<Config>("runtime: rex\n").is_err());
        assert!(serde_yaml::from_str::<Config>("sites: {a: {driver: host, rot: /x}}").is_err());
        let c: Config = serde_yaml::from_str("scan: ~/src\n").unwrap();
        let p = Paths::under(std::path::Path::new("/h"));
        assert_eq!(c.scan_dir(&p), Some(PathBuf::from("/h/src")));
    }
}
