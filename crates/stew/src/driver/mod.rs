//! Drivers put a stew somewhere and isolate it; a site is a named driver with its settings
//! (`~/.config/stew/config.yaml`, `sites:`). The trait is stew-internal and small on purpose:
//! everything else (clones, git, the pushed check, the snapshot) is stew's code run through
//! `exec`, the same for every driver.

pub mod bento;
pub mod host;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{DriverKind, Site};
use crate::error::{Result, fail};
use crate::paths::Paths;

/// A stew as a driver sees it: its id and the env every process in it gets.
#[derive(Debug, Clone)]
pub struct Target {
    pub id: String,
    /// Given at `create` (bento keeps it in its state; host adds it to every process).
    pub env: Vec<(String, String)>,
}

/// A command line to run where the stew lives, from wherever the hub runs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Launch {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// Dirs put before the runner's own `PATH` (host: the in-box client's dir).
    pub path: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ExecOpts {
    /// Fail at once on a stopped stew instead of starting it.
    pub no_start: bool,
    /// A pty (the caller has a TTY).
    pub tty: bool,
    /// Stdin is forwarded.
    pub stdin: bool,
    /// Where it runs: relative to the root (no `..`), or absolute; None: the root.
    pub cwd: Option<String>,
}

/// A command's dir as `exec` takes it: relative to the root (plain components only), or
/// absolute (e.g. under `$HOME`, for the agents' sessions).
pub fn check_cwd(cwd: &str) -> Result<()> {
    let p = Path::new(cwd);
    if cwd.is_empty()
        || p.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(fail!(
            "'{cwd}': a dir relative to the root without '..', or absolute"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The command's own exit code (128+n for a signal).
    Exited(i32),
    /// `no_start` and the stew isn't running.
    NotRunning,
    /// The stew doesn't exist on its site.
    NoBox,
}

#[derive(Debug)]
pub struct Output {
    pub status: Status,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == Status::Exited(0)
    }

    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// A one-line reason for a failure: stderr's last lines, or the status.
    pub fn reason(&self) -> String {
        let err = String::from_utf8_lossy(&self.stderr);
        let err = err.trim();
        let status = match self.status {
            Status::Exited(c) => format!("exit {c}"),
            Status::NotRunning => "not running".into(),
            Status::NoBox => "no such box".into(),
        };
        if err.is_empty() {
            status
        } else {
            let tail: Vec<&str> = err.lines().rev().take(3).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            format!("{} ({status})", tail.join("; "))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoxState {
    Running,
    Stopped,
    /// Not on the site.
    Missing,
    /// The driver's own word for anything else (bento: `creating`, `unknown`).
    Other(String),
}

impl BoxState {
    pub fn word(&self) -> &str {
        match self {
            BoxState::Running => "running",
            BoxState::Stopped => "stopped",
            BoxState::Missing => "missing",
            BoxState::Other(w) => w,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Port {
    pub guest: u16,
    /// Where the host reaches it; opaque (show or dial it, never derive it).
    pub address: Option<String>,
    /// Declared since the last boot: published at the next one (bento: `restart`).
    pub pending: bool,
}

/// Which remotes that are local paths survive a `remove` (the pushed check counts only
/// those): none in a box, those outside the root on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRemotes {
    None,
    OutsideRoot,
}

impl LocalRemotes {
    pub fn word(self) -> &'static str {
        match self {
            LocalRemotes::None => "none",
            LocalRemotes::OutsideRoot => "outside",
        }
    }
}

pub trait Driver: Send + Sync {
    fn kind(&self) -> DriverKind;
    /// Makes the stew's place and starts it; returns its root.
    fn create(&self, t: &Target) -> Result<String>;
    /// Starts it; a running one stays as it is. Warnings.
    fn start(&self, t: &Target) -> Result<Vec<String>>;
    /// Stops and starts it, so what applies only at a boot applies (bento: pending ports);
    /// what runs in it ends. A stopped one is just started. Warnings.
    fn restart(&self, t: &Target) -> Result<Vec<String>>;
    /// Stops it. Warnings (a driver that can't stop says so).
    fn stop(&self, t: &Target) -> Result<Vec<String>>;
    /// Takes it off the site, everything in it; `force` also when its place is broken
    /// (bento: a box a killed `create` left `unknown`).
    fn remove(&self, t: &Target, force: bool) -> Result<()>;
    /// Its state; an error means the site is unreachable.
    fn state(&self, t: &Target) -> Result<BoxState>;
    /// The command line that runs `argv` in the root (for `exec`, and the client's own).
    fn launch(&self, t: &Target, argv: &[String], opts: &ExecOpts) -> Result<Launch>;
    /// The command line of an interactive shell in the root.
    fn enter(&self, t: &Target) -> Result<Launch>;
    /// Runs `argv` with `stdin` in `cwd` (relative to the root, or absolute; None: the root)
    /// and waits (at most `timeout`).
    fn exec(
        &self,
        t: &Target,
        argv: &[String],
        stdin: Option<&[u8]>,
        cwd: Option<&str>,
        no_start: bool,
        timeout: Option<Duration>,
    ) -> Result<Output>;
    /// Its root where it lives (bento: `/workspace`; host: the dir).
    fn root(&self, t: &Target) -> Result<String>;
    /// Copies a host file or dir into the stew (`guest` relative to the root) or out of it.
    fn cp_in(&self, t: &Target, host: &Path, guest: &str) -> Result<()>;
    fn cp_out(&self, t: &Target, guest: &str, host: &Path) -> Result<()>;
    fn ports(&self, t: &Target) -> Result<Vec<Port>>;
    fn local_remotes(&self) -> LocalRemotes;
}

/// The driver for a site. `hub` is the hub's socket, which the host driver gives its stews
/// in `STEW_HUB`; `client` the dir of the in-box client, put first on their `PATH`.
pub fn for_site(site: &Site, paths: &Paths, hub: &str, client: Option<&Path>) -> Box<dyn Driver> {
    match site.driver {
        DriverKind::Bento => Box::new(bento::Bento::new(site.via.as_deref())),
        DriverKind::Host => {
            let dir = site
                .root
                .as_deref()
                .map_or_else(|| paths.home.join("projects/stews"), |r| paths.expand(r));
            Box::new(host::Host::new(
                dir,
                hub.to_string(),
                client.map(Path::to_path_buf),
            ))
        }
    }
}

/// Runs a launch with `stdin` (none: empty), collecting both streams; killed (by its PID)
/// after `timeout`. The exit code is 128+n for a signal, as a shell reports it.
pub fn run(
    launch: &Launch,
    stdin: Option<&[u8]>,
    timeout: Option<Duration>,
) -> Result<(i32, Vec<u8>, Vec<u8>)> {
    let (prog, args) = launch
        .argv
        .split_first()
        .ok_or_else(|| fail!("empty command"))?;
    let mut cmd = Command::new(prog);
    cmd.args(args).envs(launch.env.iter().map(|(k, v)| (k, v)));
    if let Some(path) = launch.full_path() {
        cmd.env("PATH", path);
    }
    if let Some(cwd) = &launch.cwd {
        cmd.current_dir(cwd);
    }
    run_command(cmd, stdin, timeout)
}

impl Launch {
    /// `path` before this process's own `PATH`, when there is any.
    pub fn full_path(&self) -> Option<std::ffi::OsString> {
        if self.path.is_empty() {
            return None;
        }
        let own = std::env::var_os("PATH").unwrap_or_default();
        let dirs = self
            .path
            .iter()
            .map(PathBuf::from)
            .chain(std::env::split_paths(&own));
        std::env::join_paths(dirs).ok()
    }
}

/// Runs `cmd` as `run` does. It runs in a process group of its own: at `timeout` the whole
/// group is killed (a grandchild, git under `sh`, would otherwise keep the pipes open and the
/// wait going), and the output is waited for at most a little longer.
pub fn run_command(
    mut cmd: Command,
    stdin: Option<&[u8]>,
    timeout: Option<Duration>,
) -> Result<(i32, Vec<u8>, Vec<u8>)> {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::sync::mpsc;

    let prog = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .process_group(0);
    let mut child = cmd.spawn().map_err(|e| fail!("cannot run {prog}: {e}"))?;
    let group = child.id() as libc::pid_t;
    let kill_group = || {
        // SAFETY: a plain syscall on the group this child leads
        unsafe { libc::kill(-group, libc::SIGKILL) };
    };
    let reader = |mut pipe: Box<dyn Read + Send>| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
        rx
    };
    let out = reader(Box::new(child.stdout.take().expect("piped")));
    let err = reader(Box::new(child.stderr.take().expect("piped")));
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = pipe.write_all(&data);
        });
    }
    let deadline = timeout.map(|t| Instant::now() + t);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Ok(st),
            Ok(None) => {}
            Err(e) => break Err(fail!("waiting for {prog}: {e}")),
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            kill_group();
            let _ = child.kill();
            let _ = child.wait();
            break Err(fail!(
                "{prog} took longer than {}s",
                timeout.unwrap_or_default().as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // the output: until EOF, or with a timeout until the deadline (then the group is
    // killed) and a moment more
    let collect = |rx: mpsc::Receiver<Vec<u8>>| -> Vec<u8> {
        match deadline {
            None => rx.recv().unwrap_or_default(),
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                rx.recv_timeout(left).unwrap_or_else(|_| {
                    kill_group();
                    rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default()
                })
            }
        }
    };
    let out = collect(out);
    let err = collect(err);
    let st = status?;
    let code = st
        .code()
        .or_else(|| st.signal().map(|n| 128 + n))
        .unwrap_or(1);
    Ok((code, out, err))
}

/// Quotes a word for a POSIX shell (for `via` commands like ssh, which join their
/// arguments into one shell line).
pub fn shell_quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b))
    {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("abc-1/x"), "abc-1/x");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("$(x)"), "'$(x)'");
    }

    #[test]
    fn runs_with_stdin_and_timeout() {
        let l = Launch {
            argv: vec!["sh".into(), "-c".into(), "cat; echo err >&2; exit 3".into()],
            cwd: None,
            env: vec![],
            path: vec![],
        };
        let (code, out, err) = run(&l, Some(b"hi"), None).unwrap();
        assert_eq!((code, &out[..], &err[..]), (3, &b"hi"[..], &b"err\n"[..]));
        let slow = Launch {
            argv: vec!["sleep".into(), "5".into()],
            cwd: None,
            env: vec![],
            path: vec![],
        };
        let t = Instant::now();
        assert!(run(&slow, None, Some(Duration::from_millis(200))).is_err());
        assert!(t.elapsed() < Duration::from_secs(3));
        // a grandchild holding the pipes doesn't hold the wait past the timeout
        let held = Launch {
            argv: vec!["sh".into(), "-c".into(), "sleep 30 & sleep 30".into()],
            cwd: None,
            env: vec![],
            path: vec![],
        };
        let t = Instant::now();
        assert!(run(&held, None, Some(Duration::from_millis(300))).is_err());
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        let bg = Launch {
            argv: vec!["sh".into(), "-c".into(), "sleep 30 & echo done".into()],
            cwd: None,
            env: vec![],
            path: vec![],
        };
        let t = Instant::now();
        let (code, out, _) = run(&bg, None, Some(Duration::from_millis(500))).unwrap();
        assert_eq!((code, &out[..]), (0, &b"done\n"[..]));
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        let kill = Launch {
            argv: vec!["sh".into(), "-c".into(), "kill -9 $$".into()],
            cwd: None,
            env: vec![],
            path: vec![],
        };
        assert_eq!(run(&kill, None, None).unwrap().0, 137);
    }
}
