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
            .args(["--fetch-every", "1"])
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
fn ls_through_the_daemon() {
    let env = Env::new();
    let url = env.artifacts();
    let d = env.daemon();

    let r = env.stew(&["ls"], &d.socket);
    assert_eq!((r.code, r.stdout.as_str()), (0, ""), "{}", r.stderr);

    // another hub claims; this daemon's mirror sees it at its next fetch (`ls` never fetches)
    let other = Mirror::new(env.base.join("other/artifacts.git"), url.clone());
    let a = other.claim(&meta("Port forwarding"), None).unwrap();
    let b = other.claim(&meta("Something else"), None).unwrap();
    let ls = |args: &[&str]| {
        let r = env.stew(args, &d.socket);
        assert_eq!(r.code, 0, "{}", r.stderr);
        r.stdout
    };
    let json = |args: &[&str]| -> Vec<Value> {
        let v: Value = serde_json::from_str(&ls(args)).unwrap();
        v["stews"].as_array().unwrap().clone()
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let v = loop {
        let v = json(&["ls", "--json"]);
        if v.len() == 2 {
            break v;
        }
        assert!(Instant::now() < deadline, "not fetched: {v:?}");
        std::thread::sleep(Duration::from_millis(100));
    };
    let ids: Vec<_> = v.iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&a.as_str()) && ids.contains(&b.as_str()));
    // tracked, on no site
    assert_eq!(v[0]["state"], "removed");
    assert!(v[0].get("site").is_none());
    let pf = v.iter().find(|s| s["id"] == a.as_str()).unwrap();
    assert_eq!(pf["readme"], "# Port forwarding\n\n## Now\n");

    // an agent in `a` writes its README; `b` gets closed
    for (id, dir) in [(&a, "wa"), (&b, "wb")] {
        env.git(
            &env.base,
            &["clone", "-q", "-b", &format!("stew/{id}"), &url, dir],
        );
    }
    let wa = env.base.join("wa");
    let readme =
        "# Port forwarding\n\nPorts per box.\n\n## Now\n- waiting: Georg applies the units\n";
    fs::write(wa.join("README.md"), readme).unwrap();
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
    let deadline = Instant::now() + Duration::from_secs(10);
    while json(&["ls", "--json"]).len() != 1 {
        assert!(Instant::now() < deadline, "not fetched");
        std::thread::sleep(Duration::from_millis(100));
    }

    // one line each: title, short id, site, state; -l adds the README as it is
    let short = |id: &str| id[id.len() - 4..].to_string();
    let out = ls(&["ls"]);
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines.len(), 1, "{out}");
    assert!(
        lines[0].starts_with(&format!("Port forwarding  {}  -  removed", short(&a))),
        "{out}"
    );
    let out = ls(&["ls", "-l"]);
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(
        &lines[1..],
        [
            "    # Port forwarding",
            "",
            "    Ports per box.",
            "",
            "    ## Now",
            "    - waiting: Georg applies the units",
            ""
        ],
        "{out}"
    );
    assert_eq!(json(&["ls", "--json"])[0]["readme"], readme);

    // --closed: the closed ones too, after the open ones
    let out = ls(&["ls", "--closed"]);
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    assert!(lines[0].starts_with("Port forwarding"), "{out}");
    assert!(
        lines[1].starts_with("Something else") && lines[1].contains("closed"),
        "{out}"
    );
    let closed = json(&["ls", "--closed", "--json"]);
    assert_eq!(closed[1]["state"], "closed");
    assert_eq!(closed[1]["closed"], "2026-11-15");

    // a search covers closed stews too, and the notes
    assert!(ls(&["ls", "something"]).starts_with("Something else"));
    assert!(ls(&["ls", "forwarding"]).starts_with("Port forwarding"));
    assert!(ls(&["ls", "applies"]).starts_with("Port forwarding"));
    assert!(
        env.home
            .join(".local/share/stew/artifacts.git/HEAD")
            .exists()
    );

    // the remote goes away: the last fetched state
    fs::rename(
        env.base.join("remotes/notes.git"),
        env.base.join("remotes/moved.git"),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(ls(&["ls"]).lines().count(), 1);
}

#[test]
fn errors_reach_the_cli() {
    let env = Env::new();
    let d = env.daemon();
    // stews on a site still list without the mirror, with a warning
    let r = env.stew(&["ls"], &d.socket);
    assert_eq!((r.code, r.stdout.as_str()), (0, ""));
    assert!(r.stderr.contains("no artifacts repo"), "{}", r.stderr);

    let r = env.stew(&["ls"], &env.base.join("nobody.sock"));
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
