//! The daemon in the foreground (`stew daemon --socket`) and the CLI as its client.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::Env;
use serde_json::Value;
use stew::mirror::Mirror;
use stew_proto::{Call, Here, Reply, Request, StewMeta};

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// A daemon child; killed by its PID when dropped.
struct Daemon {
    child: Child,
    socket: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn call(&self, call: Call) -> Reply {
        stew_proto::call(&self.socket, &Request::new(call)).unwrap()
    }
}

trait Stew {
    fn stew(&self, args: &[&str], socket: &Path) -> Run;
    fn daemon(&self) -> Daemon;
    fn artifacts(&self) -> String;
}

impl Stew for Env {
    fn stew(&self, args: &[&str], socket: &Path) -> Run {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stew"));
        cmd.args(args).current_dir(&self.base);
        self.envs(&mut cmd);
        cmd.env("STEW_SOCKET", socket);
        let out = cmd.output().unwrap();
        Run {
            code: out.status.code().unwrap(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn daemon(&self) -> Daemon {
        let socket = self.base.join("run/stew.sock");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stew"));
        cmd.args(["daemon", "--socket", socket.to_str().unwrap()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        self.envs(&mut cmd);
        let child = cmd.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "the daemon didn't come up");
            std::thread::sleep(Duration::from_millis(20));
        }
        Daemon { child, socket }
    }

    /// An empty artifacts repo, set in the config.
    fn artifacts(&self) -> String {
        let remote = self.base.join("remotes/notes.git");
        self.git(
            &self.base,
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        let url = remote.to_str().unwrap().to_string();
        self.write(
            ".config/stew/config.yaml",
            &format!("artifacts: {url}\nscan: false\nruntime: none\n"),
        );
        url
    }
}

fn meta(title: &str) -> StewMeta {
    StewMeta {
        id: String::new(),
        title: title.into(),
        from: None,
        created: "2026-11-14".into(),
        state: None,
        closed: None,
        repos: vec![],
    }
}

#[test]
fn ls_all_through_the_daemon() {
    let env = Env::new();
    let url = env.artifacts();
    let d = env.daemon();

    let r = env.stew(&["ls", "--all"], &d.socket);
    assert_eq!((r.code, r.stdout.as_str()), (0, ""), "{}", r.stderr);

    // another hub claims; this daemon's mirror sees it at the next fetch
    let other = Mirror::new(env.base.join("other/artifacts.git"), url.clone());
    let a = other.claim(&meta("Port forwarding"), None).unwrap();
    let b = other.claim(&meta("Something else"), None).unwrap();

    let r = env.stew(&["ls", "--all", "--json"], &d.socket);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v: Value = serde_json::from_str(&r.stdout).unwrap();
    let ids: Vec<_> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&a.as_str()) && ids.contains(&b.as_str()));
    assert_eq!(v[0]["now"], serde_json::json!([]));

    // an agent in `a` writes its README; `b` gets closed
    for (id, dir) in [(&a, "wa"), (&b, "wb")] {
        env.git(
            &env.base,
            &["clone", "-q", "-b", &format!("stew/{id}"), &url, dir],
        );
    }
    let wa = env.base.join("wa");
    fs::write(
        wa.join("README.md"),
        "# Port forwarding\n\nPorts per box.\n\n## Now\n- loopback IPs: done\n- waiting: Georg applies the units\n",
    )
    .unwrap();
    env.git(&wa, &["commit", "-q", "-am", "now"]);
    env.git(&wa, &["push", "-q"]);
    let wb = env.base.join("wb");
    let yaml = fs::read_to_string(wb.join("stew.yaml")).unwrap();
    fs::write(
        wb.join("stew.yaml"),
        yaml.replace("repos:", "state: closed\nclosed: 2026-11-15\nrepos:"),
    )
    .unwrap();
    env.git(&wb, &["commit", "-q", "-am", "closed"]);
    env.git(&wb, &["push", "-q"]);

    let r = env.stew(&["ls", "--all"], &d.socket);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let lines: Vec<_> = r.stdout.lines().collect();
    assert_eq!(lines.len(), 4, "{}", r.stdout);
    assert!(lines[0].starts_with(&a) && lines[0].contains("Port forwarding"));
    assert_eq!(
        &lines[1..],
        [
            "    Ports per box.",
            "    - loopback IPs: done",
            "    - waiting: Georg applies the units"
        ]
    );

    let r = env.stew(&["ls", "--all", "--closed"], &d.socket);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout.starts_with(&b) && r.stdout.trim_end().ends_with("closed"),
        "{}",
        r.stdout
    );
    assert_eq!(r.stdout.lines().count(), 1);

    let r = env.stew(&["ls", "--waiting"], &d.socket);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout, format!("{a}  waiting: Georg applies the units\n"));

    // a search covers closed stews too
    let r = env.stew(&["ls", "--all", "something"], &d.socket);
    assert!(r.stdout.starts_with(&b), "{}", r.stdout);
    let r = env.stew(&["ls", "--all", "forwarding"], &d.socket);
    assert!(r.stdout.starts_with(&a), "{}", r.stdout);
    assert!(
        env.home
            .join(".local/share/stew/artifacts.git/HEAD")
            .exists()
    );

    // the remote goes away: the last fetched state, with a warning
    fs::rename(
        env.base.join("remotes/notes.git"),
        env.base.join("remotes/moved.git"),
    )
    .unwrap();
    let r = env.stew(&["ls", "--all"], &d.socket);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout.lines().count(), 4);
    assert!(r.stderr.contains("warning: cannot fetch"), "{}", r.stderr);
}

#[test]
fn errors_reach_the_cli() {
    let env = Env::new();
    let d = env.daemon();
    let r = env.stew(&["ls", "--all"], &d.socket);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("no artifacts repo"), "{}", r.stderr);

    // stews on a site still list without the mirror, with a warning
    let r = env.stew(&["ls"], &d.socket);
    assert_eq!((r.code, r.stdout.as_str()), (0, ""));
    assert!(r.stderr.contains("no artifacts repo"), "{}", r.stderr);

    let r = env.stew(&["ls", "--all"], &env.base.join("nobody.sock"));
    assert_eq!(r.code, 1);
    assert!(
        r.stderr.contains("cannot reach the stew daemon"),
        "{}",
        r.stderr
    );

    // a malformed request gets an error reply, and the daemon carries on
    let conn = UnixStream::connect(&d.socket).unwrap();
    (&conn).write_all(b"{\"call\":\"no_such\"}\n").unwrap();
    let mut line = String::new();
    BufReader::new(&conn).read_line(&mut line).unwrap();
    assert!(
        line.contains("\"reply\":\"error\"") && line.contains("bad request"),
        "{line}"
    );
    assert_eq!(
        d.call(Call::Hello),
        Reply::Hello {
            version: stew_proto::VERSION
        }
    );
}

#[test]
fn catalog_and_refs_over_the_socket() {
    let env = Env::new();
    let a = env.remote("a", &["main", "feat"], "main");
    let b = env.remote("b", &["dev"], "dev");
    env.write(
        ".config/stew/repos.yaml",
        &format!(
            "a: {}\nwork:\n  b: {}\n  gh: git@github.com:x/dup.git\n",
            a.display(),
            b.display()
        ),
    );
    // scanned clones: one new, one the catalog already has
    for (dir, origin) in [
        ("tool", "git@github.com:x/tool.git"),
        ("dup", "https://github.com/x/dup"),
    ] {
        let p = env.home.join("projects/repos").join(dir);
        fs::create_dir_all(&p).unwrap();
        env.git(&p, &["init", "-q"]);
        env.git(&p, &["remote", "add", "origin", origin]);
    }
    let d = env.daemon();

    let Reply::Repos { repos } = d.call(Call::Repos) else {
        panic!()
    };
    let names: Vec<_> = repos
        .iter()
        .map(|r| (r.name.as_str(), r.remote.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("a", a.to_str().unwrap()),
            ("work/b", b.to_str().unwrap()),
            ("work/gh", "https://github.com/x/dup"),
            ("tool", "https://github.com/x/tool"),
        ]
    );

    let resolve = |base: Option<&str>, here: Option<Here>| {
        d.call(Call::Resolve {
            repos: vec!["a".into(), "b".into()],
            base: base.map(String::from),
            here,
        })
    };
    let Reply::Resolved { repos, warnings } = resolve(None, None) else {
        panic!()
    };
    assert_eq!(
        repos.iter().map(|r| r.branch.as_str()).collect::<Vec<_>>(),
        ["main", "dev"]
    );
    assert_eq!(repos[1].name, "work/b");
    assert!(warnings.is_empty());

    let here = Here {
        remote: a.to_str().unwrap().into(),
        branch: "feat".into(),
    };
    let Reply::Resolved { repos, .. } = resolve(None, Some(here)) else {
        panic!()
    };
    assert_eq!(repos[0].branch, "feat");

    let Reply::Error { message, .. } = resolve(Some("zzz"), None) else {
        panic!()
    };
    assert!(message.contains("zzz"), "{message}");
    let Reply::Error { message, .. } = d.call(Call::Resolve {
        repos: vec!["nope".into()],
        base: None,
        here: None,
    }) else {
        panic!()
    };
    assert!(message.contains("no repo 'nope'"), "{message}");
}

#[test]
fn socket_and_state_on_start() {
    let env = Env::new();
    let socket = env.base.join("run/stew.sock");

    // a stale socket file from a dead daemon is replaced
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    drop(UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());

    // a creation left running by the last daemon is marked failed
    let state = env.write(
        ".local/state/stew/state.json",
        r#"{"stews":{"a-0000":{"site":"desktop","token":"t"}},
            "creations":{"b-0000":{"state":"running","started":5},"c-0000":{"state":"failed","started":1,"error":"x"}}}"#,
    );
    let d = env.daemon();
    // the socket accepts before the state is recovered; a reply comes after it
    let _ = d.call(Call::Resolve {
        repos: vec!["nope".into()],
        base: None,
        here: None,
    });
    let v: Value = serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(v["creations"]["b-0000"]["state"], "failed");
    assert!(
        v["creations"]["b-0000"]["error"]
            .as_str()
            .unwrap()
            .contains("restarted")
    );
    assert_eq!(v["creations"]["c-0000"]["error"], "x");
    assert_eq!(v["stews"]["a-0000"]["site"], "desktop");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&d.socket).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // a live one is not taken over
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stew"));
    cmd.args(["daemon", "--socket", socket.to_str().unwrap()]);
    env.envs(&mut cmd);
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("already listens"));
    assert_eq!(
        d.call(Call::Hello),
        Reply::Hello {
            version: stew_proto::VERSION
        }
    );

    // a corrupt state file stops the daemon instead of being overwritten
    drop(d);
    fs::write(&state, "{").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stew"));
    cmd.args(["daemon", "--socket", socket.to_str().unwrap()]);
    env.envs(&mut cmd);
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(fs::read_to_string(&state).unwrap(), "{");
}

/// Socket activation: fd 3 is served, made close-on-exec and `LISTEN_*` unset (children
/// never inherit the hub's socket); idle, it exits 0, and every accepted connection is
/// answered until then.
#[test]
fn socket_activation_and_idle_exit() {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let env = Env::new();
    let socket = env.base.join("act.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let fd = listener.as_raw_fd();
    // the daemon's children: a `git` that records what it inherited
    let bin = env.base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let seen = env.base.join("seen");
    fs::write(
        bin.join("git"),
        format!(
            "#!/bin/sh\n{{ env; [ -e /proc/$$/fd/3 ] && echo FD3; }} > {}\nexit 1\n",
            seen.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    env.write(".config/stew/repos.yaml", "x: /nonexistent/x.git\n");
    let mut cmd = Command::new("sh");
    cmd.args([
        "-c",
        "LISTEN_PID=$$ LISTEN_FDS=1 LISTEN_FDNAMES=stew.socket exec \"$0\" daemon --idle-exit 2",
        env!("CARGO_BIN_EXE_stew"),
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
    env.envs(&mut cmd);
    let path = std::env::var("PATH").unwrap();
    cmd.env("PATH", format!("{}:{path}", bin.display()));
    // SAFETY: only async-signal-safe calls between fork and exec
    unsafe {
        cmd.pre_exec(move || {
            // dup2 onto itself keeps close-on-exec: clear it instead
            let ok = if fd == 3 {
                libc::fcntl(3, libc::F_SETFD, 0) >= 0
            } else {
                libc::dup2(fd, 3) >= 0
            };
            if !ok {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    drop(listener);
    let pid = child.id();
    let hello = || stew_proto::call(&socket, &Request::new(Call::Hello)).unwrap();
    assert_eq!(
        hello(),
        Reply::Hello {
            version: stew_proto::VERSION
        }
    );
    let _ = stew_proto::call(
        &socket,
        &Request::new(Call::Resolve {
            repos: vec!["x".into()],
            base: None,
            here: None,
        }),
    )
    .unwrap();
    let inherited = fs::read_to_string(&seen).unwrap();
    assert!(inherited.contains("PATH="), "{inherited}");
    assert!(!inherited.contains("LISTEN_"), "LISTEN_* left for children");
    assert!(!inherited.contains("FD3"), "a child got the hub's socket");
    let info = fs::read_to_string(format!("/proc/{pid}/fdinfo/3")).unwrap();
    let flags = info
        .lines()
        .find_map(|l| l.strip_prefix("flags:"))
        .map(|f| u32::from_str_radix(f.trim(), 8).unwrap())
        .unwrap();
    assert_ne!(flags & 0o2000000, 0, "fd 3 is not close-on-exec: {info}");
    // kept busy, it stays; every connection gets its answer
    for _ in 0..6 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert_eq!(
            hello(),
            Reply::Hello {
                version: stew_proto::VERSION
            }
        );
    }
    // then idle: it exits 0 by itself
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        assert!(std::time::Instant::now() < deadline, "no idle exit");
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    assert!(status.success());
}
