//! Where the hub keeps things. Everything follows `HOME` and the XDG variables, so tests
//! (and a second instance) move it all by setting them.

use std::env;
use std::path::{Path, PathBuf};

use crate::error::{Result, fail};

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    /// `~/.config/stew`: config.yaml, repos.yaml.
    pub config: PathBuf,
    /// `~/.local/state/stew`: the daemon's state file.
    pub state: PathBuf,
    /// `~/.local/share/stew`: the artifacts mirror, the transcripts, the skills dir.
    pub data: PathBuf,
    /// `$XDG_RUNTIME_DIR/stew`: the socket.
    pub runtime: PathBuf,
}

fn xdg(var: &str, home: &Path, fallback: &str) -> PathBuf {
    match env::var_os(var) {
        Some(v) if Path::new(&v).is_absolute() => PathBuf::from(v),
        _ => home.join(fallback),
    }
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_absolute())
            .ok_or_else(|| fail!("HOME is not set"))?;
        let state = xdg("XDG_STATE_HOME", &home, ".local/state").join("stew");
        let runtime = match env::var_os("XDG_RUNTIME_DIR") {
            Some(v) if Path::new(&v).is_absolute() => PathBuf::from(v).join("stew"),
            _ => state.clone(),
        };
        Ok(Paths {
            config: xdg("XDG_CONFIG_HOME", &home, ".config").join("stew"),
            data: xdg("XDG_DATA_HOME", &home, ".local/share").join("stew"),
            state,
            runtime,
            home,
        })
    }

    /// The default layout under `home`, ignoring the environment.
    pub fn under(home: &Path) -> Self {
        Paths {
            home: home.to_path_buf(),
            config: home.join(".config/stew"),
            state: home.join(".local/state/stew"),
            data: home.join(".local/share/stew"),
            runtime: home.join(".local/state/stew"),
        }
    }

    /// The daemon's socket: `STEW_SOCKET`, else `$XDG_RUNTIME_DIR/stew/stew.sock`.
    pub fn socket(&self) -> PathBuf {
        match env::var_os("STEW_SOCKET") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => self.runtime.join("stew.sock"),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.yaml")
    }

    pub fn repos_file(&self) -> PathBuf {
        self.config.join("repos.yaml")
    }

    pub fn state_file(&self) -> PathBuf {
        self.state.join("state.json")
    }

    pub fn mirror(&self) -> PathBuf {
        self.data.join("artifacts.git")
    }

    /// The hub's checkout of the transcripts repo (never cloned into a stew).
    pub fn transcripts(&self) -> PathBuf {
        self.data.join("transcripts")
    }

    /// The skills dir boxes see (bento's global config mounts it read-only).
    pub fn skills(&self) -> PathBuf {
        self.data.join("skills")
    }

    /// The hub's checkout of the skills repo.
    pub fn skills_repo(&self) -> PathBuf {
        self.data.join("skills-repo")
    }

    /// `~/x` → `$HOME/x`; anything else as given.
    pub fn expand(&self, p: &str) -> PathBuf {
        match p.strip_prefix("~/") {
            Some(rest) => self.home.join(rest),
            None if p == "~" => self.home.clone(),
            None => PathBuf::from(p),
        }
    }
}
