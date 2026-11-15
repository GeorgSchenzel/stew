//! The host driver: no isolation. A stew's root is `<dir>/<id>` (default
//! `~/projects/stews/<id>`) on the hub's host, `exec` a local process in it, no ports of its
//! own. For what bento can't do (bento itself: no KVM in a box) and for stew's own tests.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use super::{BoxState, Driver, ExecOpts, Launch, LocalRemotes, Output, Port, Status, Target, run};
use crate::config::DriverKind;
use crate::error::{Result, fail};
use crate::id;

pub struct Host {
    dir: PathBuf,
    /// `STEW_HUB` for processes in its stews.
    hub: String,
    /// The dir holding the in-box client (as `stew`), first on its stews' `PATH`.
    client: Option<PathBuf>,
}

/// The hc session of a host stew's own core. A stew owns a whole machine elsewhere (a box);
/// on the host it gets a session of its own, so the hub's core (hc's default session) can add
/// it as a machine like a box, and its agents' `hc` reaches its own core.
pub fn hc_session(id: &str) -> String {
    format!("stew-{id}")
}

impl Host {
    pub fn new(dir: PathBuf, hub: String, client: Option<PathBuf>) -> Self {
        Host { dir, hub, client }
    }

    fn root_dir(&self, t: &Target) -> Result<PathBuf> {
        id::check(&t.id)?;
        Ok(self.dir.join(&t.id))
    }

    /// The root, which must be a real dir (not a symlink).
    fn existing_root(&self, t: &Target) -> Result<Option<PathBuf>> {
        let root = self.root_dir(t)?;
        match fs::symlink_metadata(&root) {
            Ok(m) if m.is_dir() => Ok(Some(root)),
            Ok(_) => Err(fail!("{} is not a dir", root.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(fail!("{}: {e}", root.display())),
        }
    }

    fn env(&self, t: &Target) -> Vec<(String, String)> {
        let mut env = t.env.clone();
        env.push(("STEW_HUB".into(), format!("unix:{}", self.hub)));
        env.push(("HC_SESSION".into(), hc_session(&t.id)));
        env
    }

    fn path(&self) -> Vec<String> {
        self.client
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect()
    }

    fn guest(&self, t: &Target, guest: &str) -> Result<PathBuf> {
        let root = self
            .existing_root(t)?
            .ok_or_else(|| fail!("stew {} has no dir on this site", t.id))?;
        let rel = Path::new(guest);
        if rel
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(fail!(
                "'{guest}': a path relative to the stew root, without '..'"
            ));
        }
        Ok(root.join(rel))
    }
}

fn cp(from: &Path, to: &Path) -> Result<()> {
    let launch = Launch {
        argv: vec![
            "cp".into(),
            "-R".into(),
            "--".into(),
            from.to_string_lossy().into(),
            to.to_string_lossy().into(),
        ],
        cwd: None,
        env: vec![],
        path: vec![],
    };
    let (code, _, err) = run(&launch, None, None)?;
    if code != 0 {
        return Err(fail!("cp failed: {}", String::from_utf8_lossy(&err).trim()));
    }
    Ok(())
}

impl Driver for Host {
    fn kind(&self) -> DriverKind {
        DriverKind::Host
    }

    fn create(&self, t: &Target) -> Result<String> {
        let root = self.root_dir(t)?;
        fs::create_dir_all(&self.dir).map_err(|e| fail!("{}: {e}", self.dir.display()))?;
        fs::create_dir(&root).map_err(|e| fail!("cannot create {}: {e}", root.display()))?;
        Ok(root.to_string_lossy().into_owned())
    }

    fn start(&self, t: &Target) -> Result<Vec<String>> {
        match self.existing_root(t)? {
            Some(_) => Ok(vec![]),
            None => Err(fail!("stew {} has no dir on this site", t.id)),
        }
    }

    /// Nothing waits for a boot here.
    fn restart(&self, t: &Target) -> Result<Vec<String>> {
        self.start(t)
    }

    fn stop(&self, t: &Target) -> Result<Vec<String>> {
        let root = self.root_dir(t)?;
        Ok(vec![format!(
            "the host driver stops nothing: processes in {} keep running",
            root.display()
        )])
    }

    fn remove(&self, t: &Target, _force: bool) -> Result<()> {
        if let Some(root) = self.existing_root(t)? {
            fs::remove_dir_all(&root)
                .map_err(|e| fail!("cannot remove {}: {e}", root.display()))?;
        }
        Ok(())
    }

    fn state(&self, t: &Target) -> Result<BoxState> {
        Ok(match self.existing_root(t)? {
            Some(_) => BoxState::Running,
            None => BoxState::Missing,
        })
    }

    fn launch(&self, t: &Target, argv: &[String], opts: &ExecOpts) -> Result<Launch> {
        let root = self
            .existing_root(t)?
            .ok_or_else(|| fail!("stew {} has no dir on this site", t.id))?;
        if argv.is_empty() {
            return Err(fail!("no command"));
        }
        let cwd = match &opts.cwd {
            None => root,
            Some(c) => {
                super::check_cwd(c)?;
                let dir = root.join(c);
                if !dir.is_dir() {
                    return Err(fail!("no dir {c} in stew {}", t.id));
                }
                dir
            }
        };
        Ok(Launch {
            argv: argv.to_vec(),
            cwd: Some(cwd),
            env: self.env(t),
            path: self.path(),
        })
    }

    fn enter(&self, t: &Target) -> Result<Launch> {
        // run by the client, so its own SHELL applies
        let sh = ["sh", "-c", "exec \"${SHELL:-sh}\""].map(String::from);
        self.launch(t, &sh, &ExecOpts::default())
    }

    fn exec(
        &self,
        t: &Target,
        argv: &[String],
        stdin: Option<&[u8]>,
        cwd: Option<&str>,
        _no_start: bool,
        timeout: Option<Duration>,
    ) -> Result<Output> {
        if self.existing_root(t)?.is_none() {
            return Ok(Output {
                status: Status::NoBox,
                stdout: vec![],
                stderr: vec![],
            });
        }
        let opts = ExecOpts {
            cwd: cwd.map(String::from),
            ..ExecOpts::default()
        };
        let launch = self.launch(t, argv, &opts)?;
        let (code, stdout, stderr) = run(&launch, stdin, timeout)?;
        Ok(Output {
            status: Status::Exited(code),
            stdout,
            stderr,
        })
    }

    fn cp_in(&self, t: &Target, host: &Path, guest: &str) -> Result<()> {
        cp(host, &self.guest(t, guest)?)
    }

    fn cp_out(&self, t: &Target, guest: &str, host: &Path) -> Result<()> {
        cp(&self.guest(t, guest)?, host)
    }

    fn ports(&self, _t: &Target) -> Result<Vec<Port>> {
        Ok(vec![])
    }

    fn root(&self, t: &Target) -> Result<String> {
        Ok(self.root_dir(t)?.to_string_lossy().into_owned())
    }

    fn local_remotes(&self) -> LocalRemotes {
        LocalRemotes::OutsideRoot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: &str) -> Target {
        Target {
            id: id.into(),
            env: vec![("STEW_TOKEN".into(), "tok".into())],
        }
    }

    #[test]
    fn lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let h = Host::new(
            tmp.path().join("stews"),
            "/run/hub.sock".into(),
            Some(tmp.path().join("client")),
        );
        let t = target("a-k3fq");
        assert_eq!(h.state(&t).unwrap(), BoxState::Missing);
        assert_eq!(
            h.exec(&t, &["true".into()], None, None, false, None)
                .unwrap()
                .status,
            Status::NoBox
        );
        let root = h.create(&t).unwrap();
        assert!(root.ends_with("stews/a-k3fq"));
        assert!(h.create(&t).is_err(), "create never reuses a dir");
        assert_eq!(h.state(&t).unwrap(), BoxState::Running);

        let sh = |s: &str| vec!["sh".to_string(), "-c".into(), s.into()];
        let out = h
            .exec(
                &t,
                &sh("pwd; echo $STEW_TOKEN $STEW_HUB $HC_SESSION; echo \"${PATH%%:*}\"; cat"),
                Some(b"in"),
                None,
                true,
                None,
            )
            .unwrap();
        assert!(out.ok());
        assert_eq!(
            out.stdout(),
            format!(
                "{root}\ntok unix:/run/hub.sock stew-a-k3fq\n{}\nin",
                tmp.path().join("client").display()
            )
        );

        // a dir relative to the root, or absolute; never '..'
        fs::create_dir(tmp.path().join("stews/a-k3fq/sub")).unwrap();
        let pwd = |cwd| h.exec(&t, &["pwd".into()], None, Some(cwd), false, None);
        assert_eq!(pwd("sub").unwrap().stdout(), format!("{root}/sub\n"));
        let abs = tmp.path().to_string_lossy().into_owned();
        assert_eq!(pwd(&abs).unwrap().stdout(), format!("{abs}\n"));
        assert!(pwd("../x").is_err() && pwd("sub/../..").is_err() && pwd("").is_err());

        fs::write(tmp.path().join("f"), "x").unwrap();
        h.cp_in(&t, &tmp.path().join("f"), "g").unwrap();
        h.cp_out(&t, "g", &tmp.path().join("back")).unwrap();
        assert_eq!(fs::read_to_string(tmp.path().join("back")).unwrap(), "x");
        assert!(h.cp_in(&t, &tmp.path().join("f"), "../escape").is_err());
        assert!(h.cp_in(&t, &tmp.path().join("f"), "/abs").is_err());
        assert!(h.ports(&t).unwrap().is_empty());

        assert!(!h.stop(&t).unwrap().is_empty(), "says it stops nothing");
        h.remove(&t, false).unwrap();
        assert_eq!(h.state(&t).unwrap(), BoxState::Missing);
        h.remove(&t, true).unwrap();
    }

    #[test]
    fn refuses_bad_ids_and_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let h = Host::new(tmp.path().to_path_buf(), "/s".into(), None);
        assert!(h.create(&target("../x")).is_err());
        assert!(h.create(&target("a/b")).is_err());
        // a root that is a symlink is never followed by remove
        let outside = tmp.path().join("outside-keep");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("precious"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, tmp.path().join("l-0000")).unwrap();
        assert!(h.remove(&target("l-0000"), true).is_err());
        assert!(outside.join("precious").exists());
    }
}
