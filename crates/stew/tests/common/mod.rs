#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use stew::paths::Paths;
use tempfile::TempDir;

pub struct Env {
    _tmp: TempDir,
    pub base: PathBuf,
    pub home: PathBuf,
}

impl Env {
    pub fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let home = base.join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            base.join("gitconfig"),
            "[user]\nname = T\nemail = t@example.com\n[init]\ndefaultBranch = main\n",
        )
        .unwrap();
        Env {
            _tmp: tmp,
            base,
            home,
        }
    }

    /// A runtime dir of the test's own (short: socket paths).
    pub fn runtime(&self) -> PathBuf {
        let d = self.base.join("xrun");
        fs::create_dir_all(&d).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
        d
    }

    pub fn paths(&self) -> Paths {
        Paths::under(&self.home)
    }

    /// An isolated environment: this home, this gitconfig, no XDG or stew variables.
    pub fn envs(&self, cmd: &mut Command) {
        for var in [
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_DATA_HOME",
            "STEW_SOCKET",
            "STEW_HUB",
            "STEW_TOKEN",
            "STEW_CLIENT_DIR",
            "HC_SESSION",
            "HC_SOCKET",
            "HC_TERMINAL_ID",
            "HC_TERMINAL_SOCKET",
        ] {
            cmd.env_remove(var);
        }
        // never the user's runtime dir: hc there would start cores as systemd user units
        cmd.env("XDG_RUNTIME_DIR", self.runtime());
        cmd.env("GIT_CONFIG_GLOBAL", self.base.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", &self.home);
    }

    pub fn git(&self, dir: &Path, args: &[&str]) -> String {
        self.git_env(dir, args, &[])
    }

    pub fn git_env(&self, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(dir).args(args);
        self.envs(&mut cmd);
        cmd.envs(env.iter().copied());
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }

    /// A bare remote `<base>/remotes/<name>.git` with a commit on each branch and `HEAD`
    /// pointing at `head` (which need not exist).
    pub fn remote(&self, name: &str, branches: &[&str], head: &str) -> PathBuf {
        let bare = self.base.join(format!("remotes/{name}.git"));
        let work = self.base.join(format!("work/{name}"));
        fs::create_dir_all(&work).unwrap();
        self.git(
            &self.base,
            &["init", "-q", "--bare", bare.to_str().unwrap()],
        );
        self.git(&work, &["init", "-q"]);
        fs::write(work.join("README"), name).unwrap();
        self.git(&work, &["add", "README"]);
        self.git(&work, &["commit", "-q", "-m", "init"]);
        for b in branches {
            self.git(
                &work,
                &[
                    "push",
                    "-q",
                    bare.to_str().unwrap(),
                    &format!("HEAD:refs/heads/{b}"),
                ],
            );
        }
        self.git(
            &bare,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{head}")],
        );
        bare
    }

    pub fn write(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.home.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
        p
    }
}
