//! The bento driver, written against bento's `contracts.md` (v2): a box per stew, named by
//! the stew's id, `/workspace` as its root. Reached with `bento …`, or `<via> bento …`
//! (e.g. `ssh mac`), whose arguments are then shell-quoted (ssh joins them into one line).
//! Unit tests cover its command lines and parsing; verified against the real bento in B5
//! (work-stew.md).

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use super::{
    BoxState, Driver, ExecOpts, Launch, LocalRemotes, Output, Port, Status, Target, run,
    shell_quote,
};
use crate::config::DriverKind;
use crate::error::{Result, fail};
use crate::id;

/// The box's root.
pub const ROOT: &str = "/workspace";

// bento's exit codes (contracts.md, Exit codes)
const NO_SUCH_BOX: i32 = 121;
const NOT_RUNNING: i32 = 122;
const EXISTS: i32 = 123;

pub struct Bento {
    via: Vec<String>,
}

#[derive(Deserialize)]
struct LsJson {
    boxes: Vec<LsBox>,
}

#[derive(Deserialize)]
struct LsBox {
    name: String,
    state: String,
}

#[derive(Deserialize)]
struct PortsJson {
    ports: Vec<PortJson>,
}

#[derive(Deserialize)]
struct PortJson {
    guest: u16,
    address: Option<String>,
    #[serde(default)]
    state: String,
}

impl Bento {
    pub fn new(via: Option<&str>) -> Self {
        Bento {
            via: via
                .map(|v| v.split_whitespace().map(String::from).collect())
                .unwrap_or_default(),
        }
    }

    /// `bento <args>`, behind `via` when set.
    pub fn argv<S: AsRef<str>>(&self, args: &[S]) -> Vec<String> {
        self.argv_tty(args, false)
    }

    /// As `argv`; with `tty`, an ssh `via` asks for a remote pty (`ssh -t`), or the bento
    /// there has no TTY for `enter` and `exec -t`.
    fn argv_tty<S: AsRef<str>>(&self, args: &[S], tty: bool) -> Vec<String> {
        let mut argv = self.via.clone();
        if tty
            && argv
                .first()
                .is_some_and(|p| p.rsplit('/').next() == Some("ssh"))
        {
            argv.insert(1, "-t".into());
        }
        let quote = !self.via.is_empty();
        argv.push("bento".into());
        argv.extend(args.iter().map(|a| {
            if quote {
                shell_quote(a.as_ref())
            } else {
                a.as_ref().to_string()
            }
        }));
        argv
    }

    fn bento<S: AsRef<str>>(
        &self,
        args: &[S],
        timeout: Option<Duration>,
    ) -> Result<(i32, String, String)> {
        let launch = Launch {
            argv: self.argv(args),
            cwd: None,
            env: vec![],
            path: vec![],
        };
        let (code, out, err) = run(&launch, None, timeout)?;
        Ok((
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        ))
    }

    /// `bento <args>`, which must succeed; returns stdout and its warnings.
    fn ok<S: AsRef<str>>(&self, args: &[S], what: &str, id: &str) -> Result<(String, Vec<String>)> {
        let (code, out, err) = self.bento(args, None)?;
        match code {
            0 => Ok((out, warnings(&err))),
            NO_SUCH_BOX => Err(fail!("{what} {id}: no such box")),
            _ => Err(fail!("{what} {id}: {} (bento exit {code})", message(&err))),
        }
    }
}

/// bento's `bento: warning: …` lines.
fn warnings(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter_map(|l| l.strip_prefix("bento: warning: "))
        .map(|w| format!("bento: {w}"))
        .collect()
}

fn message(stderr: &str) -> String {
    let m = stderr.trim();
    if m.is_empty() {
        "failed".into()
    } else {
        m.replace('\n', "; ")
    }
}

pub fn parse_state(ls_json: &str, name: &str) -> Result<BoxState> {
    let ls: LsJson = serde_json::from_str(ls_json).map_err(|e| fail!("bento ls --json: {e}"))?;
    Ok(match ls.boxes.into_iter().find(|b| b.name == name) {
        None => BoxState::Missing,
        Some(b) => match b.state.as_str() {
            "running" => BoxState::Running,
            "stopped" => BoxState::Stopped,
            other => BoxState::Other(other.to_string()),
        },
    })
}

pub fn parse_ports(json: &str) -> Result<Vec<Port>> {
    let p: PortsJson = serde_json::from_str(json).map_err(|e| fail!("bento ports --json: {e}"))?;
    Ok(p.ports
        .into_iter()
        .map(|p| Port {
            guest: p.guest,
            address: p.address,
            pending: p.state == "pending",
        })
        .collect())
}

/// What an exec's exit code means. bento's 121 and 122 can't be told apart from the
/// command's own, so they count only when `ls --json` confirms them (`state`).
pub fn classify(code: i32, no_start: bool, state: impl FnOnce() -> Result<BoxState>) -> Status {
    match code {
        NO_SUCH_BOX | NOT_RUNNING => match state() {
            Ok(BoxState::Missing) => Status::NoBox,
            Ok(BoxState::Running) => Status::Exited(code),
            Ok(_) if code == NOT_RUNNING && no_start => Status::NotRunning,
            _ => Status::Exited(code),
        },
        c => Status::Exited(c),
    }
}

impl Driver for Bento {
    fn kind(&self) -> DriverKind {
        DriverKind::Bento
    }

    fn create(&self, t: &Target) -> Result<String> {
        id::check(&t.id)?;
        let mut args = vec!["create".to_string(), t.id.clone()];
        for (k, v) in &t.env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        args.push("--json".into());
        let (code, _, err) = self.bento(&args, None)?;
        match code {
            0 => Ok(ROOT.into()),
            EXISTS => Err(fail!("a box named {} already exists", t.id)),
            _ => Err(fail!(
                "bento create {}: {} (exit {code})",
                t.id,
                message(&err)
            )),
        }
    }

    fn start(&self, t: &Target) -> Result<Vec<String>> {
        Ok(self
            .ok(&["start", &t.id, "--json"], "bento start", &t.id)?
            .1)
    }

    fn restart(&self, t: &Target) -> Result<Vec<String>> {
        Ok(self
            .ok(&["restart", &t.id, "--json"], "bento restart", &t.id)?
            .1)
    }

    fn stop(&self, t: &Target) -> Result<Vec<String>> {
        Ok(self.ok(&["stop", &t.id], "bento stop", &t.id)?.1)
    }

    fn remove(&self, t: &Target, force: bool) -> Result<()> {
        let mut args = vec!["rm", &t.id];
        if force {
            args.push("--force");
        }
        let (code, _, err) = self.bento(&args, None)?;
        match code {
            0 | NO_SUCH_BOX => Ok(()),
            _ => Err(fail!("bento rm {}: {} (exit {code})", t.id, message(&err))),
        }
    }

    fn state(&self, t: &Target) -> Result<BoxState> {
        let (code, out, err) = self.bento(&["ls", "--json"], Some(Duration::from_secs(20)))?;
        if code != 0 {
            return Err(fail!("bento ls: {} (exit {code})", message(&err)));
        }
        parse_state(&out, &t.id)
    }

    fn launch(&self, t: &Target, argv: &[String], opts: &ExecOpts) -> Result<Launch> {
        id::check(&t.id)?;
        if argv.is_empty() {
            return Err(fail!("no command"));
        }
        let mut args = vec!["exec".to_string()];
        if opts.no_start {
            args.push("--no-start".into());
        }
        if opts.stdin {
            args.push("-i".into());
        }
        if opts.tty {
            args.push("-t".into());
        }
        if let Some(cwd) = &opts.cwd {
            super::check_cwd(cwd)?;
            args.push("-w".into());
            args.push(if cwd.starts_with('/') {
                cwd.clone()
            } else {
                format!("{ROOT}/{cwd}")
            });
        }
        args.push(t.id.clone());
        args.push("--".into());
        args.extend(argv.iter().cloned());
        Ok(Launch {
            argv: self.argv_tty(&args, opts.tty),
            cwd: None,
            env: vec![],
            path: vec![],
        })
    }

    fn enter(&self, t: &Target) -> Result<Launch> {
        id::check(&t.id)?;
        Ok(Launch {
            argv: self.argv_tty(&["enter", &t.id], true),
            cwd: None,
            env: vec![],
            path: vec![],
        })
    }

    fn exec(
        &self,
        t: &Target,
        argv: &[String],
        stdin: Option<&[u8]>,
        cwd: Option<&str>,
        no_start: bool,
        timeout: Option<Duration>,
    ) -> Result<Output> {
        let opts = ExecOpts {
            no_start,
            tty: false,
            stdin: stdin.is_some(),
            cwd: cwd.map(String::from),
        };
        let launch = self.launch(t, argv, &opts)?;
        let (code, stdout, stderr) = run(&launch, stdin, timeout)?;
        Ok(Output {
            status: classify(code, no_start, || self.state(t)),
            stdout,
            stderr,
        })
    }

    fn cp_in(&self, t: &Target, host: &Path, guest: &str) -> Result<()> {
        if !self.via.is_empty() {
            return Err(fail!("cp to a bento behind `via` is not supported"));
        }
        guest_path(guest)?;
        let host = host.to_string_lossy();
        self.ok(
            &["cp", &host, &format!("{}:{ROOT}/{guest}", t.id)],
            "bento cp",
            &t.id,
        )
        .map(|_| ())
    }

    fn cp_out(&self, t: &Target, guest: &str, host: &Path) -> Result<()> {
        if !self.via.is_empty() {
            return Err(fail!("cp from a bento behind `via` is not supported"));
        }
        guest_path(guest)?;
        let host = host.to_string_lossy();
        self.ok(
            &["cp", &format!("{}:{ROOT}/{guest}", t.id), &host],
            "bento cp",
            &t.id,
        )
        .map(|_| ())
    }

    fn ports(&self, t: &Target) -> Result<Vec<Port>> {
        let (out, _) = self.ok(&["ports", &t.id, "--json"], "bento ports", &t.id)?;
        parse_ports(&out)
    }

    fn root(&self, t: &Target) -> Result<String> {
        id::check(&t.id)?;
        Ok("/workspace".into())
    }

    fn local_remotes(&self) -> LocalRemotes {
        LocalRemotes::None
    }
}

/// A path for `cp`, relative to the root: plain components only (as the host driver's).
fn guest_path(guest: &str) -> Result<()> {
    let ok = !guest.is_empty()
        && Path::new(guest).components().all(|c| {
            matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        });
    if ok {
        Ok(())
    } else {
        Err(fail!(
            "'{guest}': a path relative to the stew root, without '..'"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            id: "port-forwarding-k3fq".into(),
            env: vec![("STEW_TOKEN".into(), "t0k".into())],
        }
    }

    #[test]
    fn command_lines() {
        let b = Bento::new(None);
        let t = target();
        let argv = ["sh".to_string(), "-c".into(), "echo 'a b'".into()];
        let l = b
            .launch(
                &t,
                &argv,
                &ExecOpts {
                    no_start: true,
                    tty: false,
                    stdin: true,
                    cwd: None,
                },
            )
            .unwrap();
        assert_eq!(
            l.argv,
            [
                "bento",
                "exec",
                "--no-start",
                "-i",
                "port-forwarding-k3fq",
                "--",
                "sh",
                "-c",
                "echo 'a b'"
            ]
        );
        assert_eq!(
            b.enter(&t).unwrap().argv,
            ["bento", "enter", "port-forwarding-k3fq"]
        );

        // a dir: relative to /workspace, or absolute; never '..'
        let opts = |cwd: &str| ExecOpts {
            cwd: Some(cwd.into()),
            ..ExecOpts::default()
        };
        let w = |cwd: &str| {
            b.launch(&t, &argv, &opts(cwd))
                .map(|l| l.argv[2..4].to_vec())
        };
        assert_eq!(w("a/b").unwrap(), ["-w", "/workspace/a/b"]);
        assert_eq!(w("/root/.claude").unwrap(), ["-w", "/root/.claude"]);
        assert!(w("../etc").is_err() && w("").is_err());
        for bad in ["../x", "/abs", "a/../../x", ""] {
            assert!(b.cp_in(&t, Path::new("/tmp/f"), bad).is_err(), "{bad}");
            assert!(b.cp_out(&t, bad, Path::new("/tmp/f")).is_err(), "{bad}");
        }

        // behind ssh, everything after `via` is one shell line's words
        let b = Bento::new(Some("ssh  mac"));
        let l = b.launch(&t, &argv, &ExecOpts::default()).unwrap();
        assert_eq!(
            l.argv,
            [
                "ssh",
                "mac",
                "bento",
                "exec",
                "port-forwarding-k3fq",
                "--",
                "sh",
                "-c",
                "'echo '\\''a b'\\'''"
            ]
        );
        // and the TTY commands get a remote pty
        assert_eq!(b.enter(&t).unwrap().argv[..3], ["ssh", "-t", "mac"]);
        let tty = ExecOpts {
            tty: true,
            ..ExecOpts::default()
        };
        assert_eq!(
            b.launch(&t, &argv, &tty).unwrap().argv[..3],
            ["ssh", "-t", "mac"]
        );
        assert!(
            b.launch(
                &Target {
                    id: "../x".into(),
                    env: vec![]
                },
                &argv,
                &ExecOpts::default()
            )
            .is_err()
        );
    }

    #[test]
    fn parses_ls_and_ports() {
        let ls = r#"{"boxes": [
            {"name": "a-k3fq", "state": "running", "config_changed": false, "new": 1},
            {"name": "b-k3fq", "state": "stopped", "config_changed": true},
            {"name": "c-k3fq", "state": "creating", "config_changed": false}]}"#;
        assert_eq!(parse_state(ls, "a-k3fq").unwrap(), BoxState::Running);
        assert_eq!(parse_state(ls, "b-k3fq").unwrap(), BoxState::Stopped);
        assert_eq!(
            parse_state(ls, "c-k3fq").unwrap(),
            BoxState::Other("creating".into())
        );
        assert_eq!(parse_state(ls, "d-k3fq").unwrap(), BoxState::Missing);
        assert!(parse_state("bento: oops", "a").is_err());

        let ports = r#"{"box": "x", "running": true, "ports": [
          {"guest": 5173, "address": "127.77.0.5:5173", "state": "published"},
          {"guest": 8080, "address": null, "state": "busy"},
          {"guest": 3000, "address": null, "state": "pending"}]}"#;
        assert_eq!(
            parse_ports(ports).unwrap(),
            [
                Port {
                    guest: 5173,
                    address: Some("127.77.0.5:5173".into()),
                    pending: false
                },
                Port {
                    guest: 8080,
                    address: None,
                    pending: false
                },
                Port {
                    guest: 3000,
                    address: None,
                    pending: true
                }
            ]
        );
    }

    #[test]
    fn exit_codes_confirmed_by_ls() {
        let st = |s: BoxState| move || Ok(s);
        assert_eq!(classify(0, false, st(BoxState::Running)), Status::Exited(0));
        assert_eq!(
            classify(122, true, st(BoxState::Stopped)),
            Status::NotRunning
        );
        // the command itself exited 122 on a running box
        assert_eq!(
            classify(122, true, st(BoxState::Running)),
            Status::Exited(122)
        );
        assert_eq!(classify(121, false, st(BoxState::Missing)), Status::NoBox);
        assert_eq!(
            classify(121, false, st(BoxState::Running)),
            Status::Exited(121)
        );
        assert_eq!(
            classify(122, true, || Err(fail!("unreachable"))),
            Status::Exited(122)
        );
    }

    #[test]
    fn warnings_from_stderr() {
        assert_eq!(
            warnings("bento: warning: port 8080 busy\nbento: other\n"),
            ["bento: port 8080 busy"]
        );
    }
}
