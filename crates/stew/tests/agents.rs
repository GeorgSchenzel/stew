//! Phase 5 on the host driver: the hub's hc core (a real hc, isolated in the test's own
//! runtime and state dirs), stews as its machines, the in-box client inside a stew, the
//! token, agents starting stews. The agents are stand-ins (`claude`/`opencode` scripts that
//! log what they got and report `working` over OSC 7501); real agents come in B5.
//!
//! Everything started here is stopped by PID or through hc's own `server stop` per
//! session, and checked gone.

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::Env;
use serde_json::Value;
use stew_proto::{Call, ErrorKind, Reply, Request};

const STANDIN: &str = r#"#!/bin/sh
log="$STANDIN_LOG/$(basename "$PWD")-$(basename "$0")-$$"
{
  printf 'argv'; for a in "$@"; do printf '\t%s' "$a"; done; printf '\n'
  echo "pid $$"
  echo "pwd $PWD"
  echo "token $STEW_TOKEN"
  echo "hub $STEW_HUB"
  echo "session $HC_SESSION"
  echo "stew $(command -v stew)"
} > "$log.tmp" && mv "$log.tmp" "$log"
printf '\033]7501;state=working:app=standin\033\\'
if [ -e "$STANDIN_LOG/commit-on-hup" ]; then
  # an agent that still commits as it is stopped
  here=$PWD
  trap 'cd "$here/a" && echo late > late && git add late && git commit -qm late; exit 0' HUP TERM
  while :; do sleep 600 & wait $!; done
fi
exec sleep 600
"#;

/// The hub core's session: hc's default one.
const HUB: &str = "default";

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// A child killed by its PID when dropped.
struct Kid(Child);

impl Drop for Kid {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct T {
    env: Env,
    socket: PathBuf,
    /// The stews' socket (`STEW_HUB`), next to the CLI's.
    box_socket: PathBuf,
    stews: PathBuf,
    notes: PathBuf,
    log: PathBuf,
    hc: PathBuf,
    daemon: Option<Kid>,
    core: Option<Kid>,
}

impl Drop for T {
    fn drop(&mut self) {
        // the daemon first: nothing redials a stew's core through it after this. An hc call
        // it had in flight can still start a core (hc starts one on demand), so every
        // session is stopped until two rounds a while apart find none running.
        self.daemon.take();
        self.core.take();
        let state = self.env.home.join(".local/state/herdr-core");
        let mut quiet = 0;
        for _ in 0..10 {
            let mut stopped = false;
            if let Ok(dirs) = fs::read_dir(&state) {
                for d in dirs.flatten() {
                    let s = d.file_name().to_string_lossy().into_owned();
                    stopped |= self.hc_in(&s, &["server", "stop", "--json"])["stopped"] == true;
                }
            }
            quiet = if stopped { 0 } else { quiet + 1 };
            if quiet == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1200));
        }
    }
}

fn which(prog: &str) -> PathBuf {
    let out = Command::new("sh")
        .args(["-c", &format!("command -v {prog}")])
        .output()
        .unwrap();
    assert!(out.status.success(), "{prog} is not on PATH (nix develop)");
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
}

fn script(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn link(dir: &Path, name: &str, to: &Path) {
    fs::create_dir_all(dir).unwrap();
    std::os::unix::fs::symlink(to, dir.join(name)).unwrap();
}

fn wait_for(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn path_with(dirs: &[&Path]) -> std::ffi::OsString {
    let own = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(
        dirs.iter()
            .map(|d| d.to_path_buf())
            .chain(std::env::split_paths(&own)),
    )
    .unwrap()
}

/// `slow_hc`: the daemon's `hc` waits a second per call (to kill a client mid-creation).
fn setup(slow_hc: bool) -> T {
    let env = Env::new();
    let notes = env.base.join("remotes/notes.git");
    env.git(
        &env.base,
        &["init", "-q", "--bare", notes.to_str().unwrap()],
    );
    let a = env.remote("a", &["main"], "main");
    env.write(".config/stew/repos.yaml", &format!("a: {}\n", a.display()));
    let stews = env.base.join("stews");
    env.write(
        ".config/stew/config.yaml",
        &format!(
            "artifacts: {}\nscan: false\nsite: t\nsites:\n  t: {{driver: host, root: {}}}\n",
            notes.display(),
            stews.display()
        ),
    );
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_stew"));
    let client = exe.with_file_name("stew-box");
    assert!(
        client.exists(),
        "{} is missing: build the workspace first (`just test` does)",
        client.display()
    );
    let bin = env.base.join("bin");
    link(&bin.join("host"), "stew", &exe);
    link(&bin.join("client"), "stew", &client);
    script(&bin.join("agents/claude"), STANDIN);
    script(&bin.join("agents/opencode"), STANDIN);
    let hc = which("hc");
    script(
        &bin.join("slow/hc"),
        &format!("#!/bin/sh\nsleep 1\nexec {} \"$@\"\n", hc.display()),
    );
    let log = env.base.join("log");
    fs::create_dir_all(&log).unwrap();
    let socket = env.base.join("s.sock");

    // the hub's core, as stew-hc.service runs it: hc's default session (in the test's own
    // runtime and state dirs), `stew` and the agents by name on PATH
    let mut cmd = Command::new(&hc);
    cmd.arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    env.envs(&mut cmd);
    cmd.env("STEW_SOCKET", &socket)
        .env("STANDIN_LOG", &log)
        .env("PATH", path_with(&[&bin.join("host"), &bin.join("agents")]));
    let core = Kid(cmd.spawn().unwrap());
    let core_socket = env.runtime().join("herdr-core/default.sock");
    wait_for("the hub's hc core", 15, || {
        UnixStream::connect(&core_socket).is_ok()
    });

    let mut cmd = Command::new(&exe);
    cmd.args(["daemon", "--socket", socket.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    env.envs(&mut cmd);
    cmd.env("STEW_CLIENT_DIR", bin.join("client"));
    if slow_hc {
        cmd.env("PATH", path_with(&[&bin.join("slow")]));
    }
    let daemon = Kid(cmd.spawn().unwrap());
    wait_for("the daemon", 10, || UnixStream::connect(&socket).is_ok());
    T {
        env,
        box_socket: socket.with_file_name("box.sock"),
        socket,
        stews,
        notes,
        log,
        hc,
        daemon: Some(daemon),
        core: Some(core),
    }
}

impl T {
    fn stew_in(&self, args: &[&str], stdin: Option<&[u8]>) -> Run {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stew"));
        cmd.args(args)
            .current_dir(&self.env.base)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        self.env.envs(&mut cmd);
        cmd.env("STEW_SOCKET", &self.socket);
        let mut child = cmd.spawn().unwrap();
        {
            let mut i = child.stdin.take().unwrap();
            if let Some(data) = stdin {
                i.write_all(data).unwrap();
            }
        }
        let out = child.wait_with_output().unwrap();
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn stew(&self, args: &[&str]) -> Run {
        self.stew_in(args, None)
    }

    fn ok(&self, args: &[&str]) -> Run {
        let r = self.stew(args);
        assert_eq!(r.code, 0, "stew {args:?}: {}", r.stderr);
        r
    }

    /// The in-box client, run inside stew `id` (through `stew exec`, as an agent there runs
    /// it).
    fn inside(&self, id: &str, args: &[&str], stdin: Option<&[u8]>) -> Run {
        let mut all = vec!["exec", id, "--", "stew"];
        all.extend(args);
        self.stew_in(&all, stdin)
    }

    fn hc_in(&self, session: &str, args: &[&str]) -> Value {
        let mut cmd = Command::new(&self.hc);
        cmd.args(args).stdin(Stdio::null());
        self.env.envs(&mut cmd);
        cmd.env("HC_SESSION", session);
        let out = cmd.output().unwrap();
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }

    fn machines(&self) -> Vec<String> {
        self.hc_in(HUB, &["machine", "ls", "--json"])["machines"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| m["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// The names of the terminals in stew `id`'s herdr space (`herdr.space=stew` on its
    /// machine), sorted.
    fn space(&self, id: &str) -> Vec<String> {
        let v = self.hc_in(HUB, &["ls", "--label", "herdr.space=stew", "--json"]);
        let mut names: Vec<String> = v["terminals"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| {
                t["address"]
                    .as_str()
                    .is_some_and(|a| a.starts_with(&format!("{id}/")))
            })
            .map(|t| t["name"].as_str().unwrap_or("").to_string())
            .collect();
        names.sort();
        names
    }

    fn ls(&self) -> Value {
        serde_json::from_str(&self.ok(&["ls", "--json"]).stdout).unwrap()
    }

    fn live<'a>(v: &'a Value, id: &str) -> &'a Value {
        v["stews"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap_or_else(|| panic!("{id} not in {v}"))
    }

    /// What a stand-in logged in stew `id` (waits for it).
    fn standin(&self, id: &str, agent: &str) -> Vec<(String, String)> {
        let prefix = format!("{id}-{agent}-");
        let mut found = None;
        wait_for(&format!("{agent} in {id}"), 20, || {
            found = fs::read_dir(&self.log).unwrap().flatten().find(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with(&prefix) && !n.ends_with(".tmp")
            });
            found.is_some()
        });
        fs::read_to_string(found.unwrap().path())
            .unwrap()
            .lines()
            .map(|l| {
                let (k, v) = l.split_once(['\t', ' ']).unwrap_or((l, ""));
                (k.to_string(), v.to_string())
            })
            .collect()
    }

    fn token(&self, id: &str) -> String {
        let state: Value = serde_json::from_str(
            &fs::read_to_string(self.env.home.join(".local/state/stew/state.json")).unwrap(),
        )
        .unwrap();
        state["stews"][id]["token"].as_str().unwrap().to_string()
    }

    /// A request on the stews' socket, as a stew's client sends it.
    fn raw(&self, req: &Request) -> Reply {
        stew_proto::call(&self.box_socket, req).unwrap()
    }

    /// A request on the host CLI's socket.
    fn cli(&self, req: &Request) -> Reply {
        stew_proto::call(&self.socket, req).unwrap()
    }
}

fn root_of(t: &T, id: &str) -> String {
    t.stews.join(id).to_string_lossy().into_owned()
}

fn get<'a>(log: &'a [(String, String)], key: &str) -> &'a str {
    log.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("no {key} in {log:?}"))
}

fn alive(pid: &str) -> bool {
    Path::new("/proc").join(pid).exists()
}

#[test]
fn agents_start_stews_from_inside_one() {
    let t = setup(false);
    let parent = t
        .ok(&["new", "a", "--title", "Parent"])
        .stdout
        .trim()
        .to_string();
    assert_eq!(
        t.machines(),
        std::slice::from_ref(&parent),
        "connected at new"
    );

    // inside the stew, `stew` is the in-box client, and it is this stew
    let r = t.inside(&parent, &["info"], None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout.starts_with(&format!("{parent}  Parent\n")),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("site    t (host), running"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("repo    a@main"), "{}", r.stdout);

    // from the stew: a started stew (brief on stdin) and a prepared one
    let brief = b"# Brief\n\nDo the thing.\n";
    let r = t.inside(
        &parent,
        &["new", "a", "--title", "Child", "--brief", "-", "--start"],
        Some(brief),
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let child = r.stdout.trim().to_string();
    assert!(
        r.stderr.contains(&format!("agent {child}/claude-1")),
        "{}",
        r.stderr
    );
    let r = t.inside(
        &parent,
        &["new", "a", "--title", "Prepared", "--brief", "-"],
        Some(b"later\n"),
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let prepared = r.stdout.trim().to_string();

    // from a client: the new stew with from = the caller and its agent working
    let mut v = t.ls();
    wait_for("the child's agent to report working", 20, || {
        v = t.ls();
        T::live(&v, &child)["agents"][0]["status"] == "working"
    });
    let c = T::live(&v, &child);
    assert_eq!(c["from"], parent.as_str());
    assert_eq!(c["agents"][0]["address"], format!("{child}/claude-1"));
    assert_eq!(c["agents"][0]["program"], "standin");
    let p = T::live(&v, &prepared);
    assert_eq!(p["from"], parent.as_str());
    assert_eq!(
        p["agents"],
        Value::Array(vec![]),
        "without --start, no agent"
    );
    let human = t.ok(&["ls"]).stdout;
    assert!(
        human.contains(&format!("agent {child}/claude-1  standin  working")),
        "{human}"
    );
    assert_eq!(
        c["agents"].as_array().unwrap().len(),
        1,
        "stew's shell left out"
    );

    // herdr-app's view: each stew one space (stew's shell, labelled), its agent a tab of it
    assert_eq!(t.space(&child), ["claude-1", "stew-shell"]);
    assert_eq!(t.space(&prepared), ["stew-shell"]);
    assert_eq!(t.space(&parent), ["stew-shell"]);
    let tab = |a: &str| {
        let g = t.hc_in(HUB, &["get", a, "--json"]);
        (
            g["labels"]["herdr.space"].clone(),
            g["labels"]["herdr.tab"].clone(),
            g["keep"].clone(),
        )
    };
    assert_eq!(
        tab(&format!("{child}/claude-1")),
        ("stew".into(), "claude-1".into(), false.into())
    );
    assert_eq!(
        tab(&format!("{child}/stew-shell")),
        ("stew".into(), "shell".into(), true.into())
    );
    assert_eq!(
        t.hc_in(HUB, &["get", &format!("{child}/stew-shell"), "--json"])["cwd"],
        root_of(&t, &child)
    );

    // the brief is in the claim commit and the clone; the agent got stew's command line,
    // in the stew's root, with its token and hub, its own hc session, the in-box client
    let root = t.stews.join(&child);
    assert_eq!(fs::read(root.join("notes/BRIEF.md")).unwrap(), brief);
    assert_eq!(
        t.env.git(
            &t.notes,
            &["show", &format!("refs/heads/stew/{child}:BRIEF.md")]
        ),
        "# Brief\n\nDo the thing."
    );
    let log = t.standin(&child, "claude");
    assert_eq!(get(&log, "argv"), "Your brief is in notes/BRIEF.md.");
    assert_eq!(get(&log, "pwd"), root.to_str().unwrap());
    assert_eq!(get(&log, "token"), t.token(&child));
    assert_ne!(t.token(&child), t.token(&parent));
    assert_eq!(get(&log, "hub"), format!("unix:{}", t.box_socket.display()));
    assert_eq!(get(&log, "session"), format!("stew-{child}"));
    assert_eq!(
        get(&log, "stew"),
        t.env.base.join("bin/client/stew").to_str().unwrap()
    );
    assert!(
        !fs::read_to_string(t.env.home.join(".local/state/stew/state.json"))
            .unwrap()
            .is_empty()
    );
    let mode = fs::metadata(t.env.home.join(".local/state/stew/state.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "the tokens are the owner's only");

    // `stew agent` into the prepared stew, with another agent and a model
    let r = t.inside(
        &parent,
        &[
            "agent", &prepared, "--agent", "opencode", "--model", "m/x", "go on",
        ],
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout.trim(), format!("{prepared}/opencode-1"));
    let log = t.standin(&prepared, "opencode");
    assert_eq!(
        get(&log, "argv"),
        "--standalone\t--model\tm/x\t--prompt\tgo on"
    );
    // a second one gets the next name
    let r = t.ok(&["agent", &prepared, "--agent", "opencode", "more"]);
    assert_eq!(r.stdout.trim(), format!("{prepared}/opencode-2"));

    // ls --from: from inside (`.`) and from the host
    let r = t.inside(&parent, &["ls", "--from", "."], None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout.contains(&child) && r.stdout.contains(&prepared),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains(&format!("agent {child}/claude-1")),
        "{}",
        r.stdout
    );
    let r = t.ok(&["ls", "--from", &parent, "--json"]);
    let v: Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2, "{v}");

    // attach: the hub's hc, at the agent's address (by address or by a unique name)
    for name in [format!("{child}/claude-1"), "claude-1".into()] {
        let Reply::Launch { argv, env, .. } = t.cli(&Request::new(Call::Attach { agent: name }))
        else {
            panic!("no launch")
        };
        assert_eq!(
            argv,
            [
                "env",
                "-u",
                "HC_SESSION",
                "-u",
                "HC_SOCKET",
                "-u",
                "HC_TERMINAL_ID",
                "-u",
                "HC_TERMINAL_SOCKET",
                "hc",
                "attach",
                &format!("{child}/claude-1")
            ]
        );
        assert!(env.is_empty());
    }
    let r = t.stew(&["attach", "nobody-1"]);
    assert_eq!(r.code, 1, "{}", r.stderr);

    // an agent can't start agents in stews it didn't start
    let other = t
        .ok(&["new", "a", "--title", "Other"])
        .stdout
        .trim()
        .to_string();
    let r = t.inside(&parent, &["agent", &other, "go"], None);
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("was not started from"), "{}", r.stderr);
    let r = t.inside(&child, &["agent", &prepared, "go"], None);
    assert_eq!(r.code, 2, "a sibling isn't yours either: {}", r.stderr);

    // remove: the machine goes, then the stew's own core and its agents
    let pid = get(&t.standin(&child, "claude"), "pid").to_string();
    assert!(alive(&pid));
    let r = t.ok(&["remove", &child]);
    assert!(!t.machines().contains(&child), "{}", r.stderr);
    wait_for("the child's agent to end", 10, || !alive(&pid));
    assert_eq!(
        t.hc_in(&format!("stew-{child}"), &["server", "stop", "--json"])["stopped"],
        false,
        "its core is gone"
    );
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        t.hc_in(&format!("stew-{child}"), &["server", "stop", "--json"])["stopped"],
        false,
        "and nothing started it again"
    );
    let v = t.ls();
    assert!(
        v["stews"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["id"] != child.as_str())
    );

    // start reconnects a machine that went missing
    t.hc_in(HUB, &["machine", "rm", &parent, "--json"]);
    t.ok(&["start", &parent]);
    assert!(t.machines().contains(&parent));
    // one reached another way is replaced
    t.hc_in(HUB, &["machine", "rm", &parent, "--json"]);
    t.hc_in(
        HUB,
        &[
            "machine",
            "add",
            "--name",
            &parent,
            "--via",
            &format!("stew exec {other} --no-start -- sh -c"),
            "--json",
        ],
    );
    t.ok(&["start", &parent]);
    // its space is there already (the stew's own core kept the shell): no second one
    assert_eq!(t.space(&parent), ["stew-shell"]);
    let m = t.hc_in(HUB, &["machine", "ls", "--json"]);
    let target = m["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == parent.as_str())
        .unwrap()["target"]
        .clone();
    assert_eq!(target, format!("stew exec {parent} --no-start -- sh -c"));

    for id in [&parent, &prepared, &other] {
        t.ok(&["remove", id]);
    }
    assert!(t.machines().is_empty());
}

#[test]
fn the_token_and_what_a_stew_may_ask() {
    let t = setup(false);
    let a = t
        .ok(&["new", "a", "--title", "A"])
        .stdout
        .trim()
        .to_string();
    let b = t
        .ok(&["new", "a", "--title", "B"])
        .stdout
        .trim()
        .to_string();
    let (ta, tb) = (t.token(&a), t.token(&b));
    let req = |stew: Option<&str>, token: Option<&str>, call: Call| Request {
        stew: stew.map(String::from),
        token: token.map(String::from),
        progress: false,
        call,
    };
    let refused = |r: Reply, what: &str| match r {
        Reply::Error {
            kind: ErrorKind::Refused,
            message,
        } => assert!(message.contains(what), "{message}"),
        other => panic!("not refused ({what}): {other:?}"),
    };
    // the right token is this stew
    assert!(matches!(
        t.raw(&req(Some(&a), Some(&ta), Call::Info)),
        Reply::Info { ref stew, .. } if stew.id == a
    ));
    // another stew's token, a cut or longer token, none, a stew on no site
    refused(t.raw(&req(Some(&a), Some(&tb), Call::Info)), "not stew");
    refused(
        t.raw(&req(Some(&a), Some(&ta[..31]), Call::Info)),
        "not stew",
    );
    refused(
        t.raw(&req(Some(&a), Some(&format!("{ta}0")), Call::Info)),
        "not stew",
    );
    refused(t.raw(&req(Some(&a), Some(""), Call::Info)), "not stew");
    refused(t.raw(&req(Some(&a), None, Call::Info)), "not stew");
    refused(
        t.raw(&req(Some("gone-0000"), Some(&ta), Call::Info)),
        "on no site",
    );
    refused(
        t.raw(&req(Some("../x"), Some(&ta), Call::Info)),
        "not a stew id",
    );
    refused(
        t.cli(&req(None, Some(&ta), Call::Repos)),
        "token without a stew",
    );
    // on the stews' socket, a request that names no stew is refused, whatever it asks: the
    // host CLI's calls are not reachable from a stew
    for call in [
        Call::Repos,
        Call::Remove {
            id: b.clone(),
            force: true,
        },
        Call::Start { id: None },
        Call::Where {
            id: b.clone(),
            argv: vec!["true".into()],
            no_start: false,
            tty: false,
        },
        Call::New {
            repos: vec!["a".into()],
            title: "Host".into(),
            site: Some("t".into()),
            branch: None,
            base: None,
            here: None,
            from: None,
            brief: None,
            start: false,
            agent: None,
            model: None,
            account: None,
        },
    ] {
        refused(t.raw(&req(None, None, call.clone())), "names its stew");
        refused(t.raw(&req(None, Some(&ta), call)), "names its stew");
    }
    // a stew's `new` names no site (it goes to the default one)
    refused(
        t.raw(&req(
            Some(&a),
            Some(&ta),
            Call::New {
                repos: vec!["a".into()],
                title: "Site".into(),
                site: Some("t".into()),
                branch: None,
                base: None,
                here: None,
                from: None,
                brief: None,
                start: false,
                agent: None,
                model: None,
                account: None,
            },
        )),
        "default site",
    );
    // with its token, a stew still makes only a stew client's calls
    for call in [
        Call::Remove {
            id: b.clone(),
            force: true,
        },
        Call::Close {
            id: b.clone(),
            force: true,
        },
        Call::Stop { id: b.clone() },
        Call::Start { id: None },
        Call::Where {
            id: b.clone(),
            argv: vec!["true".into()],
            no_start: false,
            tty: false,
        },
        Call::Attach { agent: "x".into() },
        Call::Title {
            id: b.clone(),
            title: "t".into(),
        },
        Call::Ls,
        Call::LsAll {
            search: None,
            closed: false,
        },
        Call::Open {
            id: b.clone(),
            site: None,
        },
        Call::Skills,
    ] {
        refused(t.raw(&req(Some(&a), Some(&ta), call)), "not a call");
    }
    assert!(t.stews.join(&b).exists(), "nothing happened to b");
    // `from` is the caller, whatever the request says
    let Reply::Created { id: c, .. } = t.raw(&req(
        Some(&a),
        Some(&ta),
        Call::New {
            repos: vec!["a".into()],
            title: "C".into(),
            site: None,
            branch: None,
            base: None,
            here: None,
            from: Some(b.clone()),
            brief: None,
            start: false,
            agent: None,
            model: None,
            account: None,
        },
    )) else {
        panic!()
    };
    let meta = fs::read_to_string(t.stews.join(&c).join("notes/stew.yaml")).unwrap();
    assert!(meta.contains(&format!("from: {a}")), "{meta}");

    // the in-box client: a changed token, no token, no hub, outside a stew
    let sh = |id: &str, script: &str| t.stew(&["exec", id, "--", "sh", "-c", script]);
    let r = sh(&a, &format!("STEW_TOKEN={tb} stew info"));
    assert_eq!(r.code, 2, "{}", r.stderr);
    let r = sh(&a, "unset STEW_TOKEN; stew info");
    assert_eq!(r.code, 2, "{}", r.stderr);
    let r = sh(&a, "unset STEW_HUB; stew info");
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("STEW_HUB is not set"), "{}", r.stderr);
    let r = sh(&a, "STEW_HUB=tcp://x stew info");
    assert_eq!(r.code, 1, "{}", r.stderr);
    let r = sh(&a, "cd / && stew info");
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("not in a stew"), "{}", r.stderr);
    // another stew's notes checked out inside this one: its id, but this token
    let r = sh(
        &a,
        &format!("mkdir -p x/notes && cp ../{b}/notes/stew.yaml x/notes/ && cd x && stew info"),
    );
    assert_eq!(r.code, 2, "{}", r.stderr);
    fs::remove_dir_all(t.stews.join(&a).join("x")).unwrap();

    // a request that is too long is cut off, not read into memory
    let mut s = UnixStream::connect(&t.socket).unwrap();
    let big = vec![b'x'; (stew_proto::MAX_REQUEST + 10) as usize];
    let _ = s.write_all(&big);
    let reply: Option<Reply> = stew_proto::recv(&mut std::io::BufReader::new(&s)).unwrap_or(None);
    assert!(matches!(reply, Some(Reply::Error { .. })), "{reply:?}");

    // bad agent arguments fail before anything exists
    let n = fs::read_dir(&t.stews).unwrap().count();
    for args in [
        vec!["new", "a", "--title", "X", "--brief", "/nonexistent"],
        vec!["new", "a", "--title", "X", "--start"],
        vec!["new", "a", "--title", "X", "--agent", "claude"],
    ] {
        let r = t.stew(&args);
        assert_ne!(r.code, 0, "{args:?}");
    }
    let r = t.stew_in(
        &[
            "new", "a", "--title", "X", "--brief", "-", "--start", "--agent", "sh",
        ],
        Some(b"b"),
    );
    assert!(r.stderr.contains("unknown agent 'sh'"), "{}", r.stderr);
    let r = t.stew_in(
        &[
            "new",
            "a",
            "--title",
            "X",
            "--brief",
            "-",
            "--start",
            "--model=-x",
        ],
        Some(b"b"),
    );
    assert_eq!(r.code, 1, "{}", r.stderr);
    let r = t.stew_in(
        &["new", "a", "--title", "X", "--brief", "-"],
        Some(&vec![b'b'; stew_proto::MAX_BRIEF + 1]),
    );
    assert!(r.stderr.contains("longer than"), "{}", r.stderr);
    // under MAX_BRIEF, but over MAX_REQUEST as JSON (control characters are escaped): the
    // client says so, instead of the daemon's cut-off line
    let r = t.inside(
        &a,
        &["new", "a", "--title", "X", "--brief", "-"],
        Some(&vec![1u8; 700_000]),
    );
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("(as JSON)"), "{}", r.stderr);
    assert_eq!(fs::read_dir(&t.stews).unwrap().count(), n);
    let r = t.stew(&["agent", &a, "--dangerously-skip-permissions"]);
    assert_ne!(r.code, 0, "{}", r.stderr);

    for id in [&a, &b, &c] {
        t.ok(&["remove", id]);
    }
}

#[test]
fn a_killed_client_still_gets_a_complete_stew() {
    // every hc call of the daemon waits a second: the client is killed while it waits
    let t = setup(true);
    fs::write(t.env.base.join("brief.md"), "brief\n").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stew"));
    cmd.args([
        "new", "a", "--title", "Killed", "--brief", "brief.md", "--start",
    ])
    .current_dir(&t.env.base)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    t.env.envs(&mut cmd);
    cmd.env("STEW_SOCKET", &t.socket);
    let mut client = Kid(cmd.spawn().unwrap());
    // the stew's dir exists: the creation runs; kill the client by its PID
    wait_for("the creation to start", 20, || {
        fs::read_dir(&t.stews).is_ok_and(|mut d| d.next().is_some())
    });
    client.0.kill().unwrap();
    let status = client.0.wait().unwrap();
    assert!(!status.success(), "killed before it finished");
    let id = fs::read_dir(&t.stews)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .into_owned();
    // the daemon finishes: clones, connected, the agent working
    let log = t.standin(&id, "claude");
    assert_eq!(get(&log, "argv"), "Your brief is in notes/BRIEF.md.");
    let v = t.ls();
    let s = T::live(&v, &id);
    assert_eq!(s["state"], "running");
    assert_eq!(s["repos"][0]["dir"], "a");
    assert_eq!(s["agents"][0]["address"], format!("{id}/claude-1"));
    assert!(v["creations"].as_array().unwrap().is_empty(), "{v}");
    t.ok(&["remove", &id]);
}

#[test]
fn without_the_hub_core_nothing_starts_one() {
    let t = setup(false);
    assert_eq!(t.hc_in(HUB, &["server", "stop", "--json"])["stopped"], true);
    let socket = t.env.runtime().join("herdr-core/default.sock");
    let r = t.ok(&["ls"]);
    assert!(
        r.stderr.contains("hub's hc core is not running"),
        "{}",
        r.stderr
    );
    let r = t.ok(&["new", "a", "--title", "Unconnected"]);
    let id = r.stdout.trim().to_string();
    assert!(
        r.stderr.contains("not connected to the agent runtime"),
        "{}",
        r.stderr
    );
    let r = t.stew(&["agent", &id, "go"]);
    assert_eq!(r.code, 1, "{}", r.stderr);
    let r = t.stew_in(
        &["new", "a", "--title", "S", "--brief", "-", "--start"],
        Some(b"b"),
    );
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(
        r.stderr.contains("exists, but its agent did not start"),
        "{}",
        r.stderr
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(UnixStream::connect(&socket).is_err(), "no core was started");
    t.ok(&["remove", &id]);
}

#[test]
fn work_done_while_removing_is_not_lost() {
    let t = setup(false);
    fs::write(t.log.join("commit-on-hup"), "").unwrap();
    let r = t.stew_in(
        &["new", "a", "--title", "Late", "--brief", "-", "--start"],
        Some(b"b"),
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let id = r.stdout.trim().to_string();
    t.standin(&id, "claude");
    // the first check passes; stopping the agent makes it commit; the second check sees it.
    // `close` (remove's code with the closed marker): refused, so not marked closed, neither
    // in the stew nor on the notes branch
    let r = t.stew(&["close", &id]);
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("since the first check"), "{}", r.stderr);
    assert!(
        r.stderr
            .contains("branch main has 1 commit(s) on no remote"),
        "{}",
        r.stderr
    );
    assert!(t.stews.join(&id).join("a/late").exists(), "nothing removed");
    assert!(t.machines().contains(&id), "connected again");
    let tracked = |path: &str| {
        t.env
            .git(&t.notes, &["show", &format!("refs/heads/stew/{id}:{path}")])
    };
    let root = t.stews.join(&id).join("notes");
    for yaml in [
        tracked("stew.yaml"),
        fs::read_to_string(root.join("stew.yaml")).unwrap(),
    ] {
        assert!(!yaml.contains("closed"), "{yaml}");
    }
    assert!(!tracked("LOG.md").contains("closed"));
    let r = t.ok(&["close", "--force", &id]);
    assert!(r.stderr.contains("lost (--force)"), "{}", r.stderr);
    assert!(!t.stews.join(&id).exists());
    assert!(tracked("stew.yaml").contains("state: closed"));
    assert_eq!(tracked("LOG.md").matches("closed").count(), 1);
}

/// R3: what any stew can push to the artifacts repo never makes it another stew's parent.
/// A branch's `stew.yaml` speaks only for its own id, and `agent` asks the daemon's record.
#[test]
fn a_pushed_from_takes_over_no_stew() {
    let t = setup(false);
    let a = t
        .ok(&["new", "a", "--title", "A"])
        .stdout
        .trim()
        .to_string();
    let b = t
        .ok(&["new", "a", "--title", "B"])
        .stdout
        .trim()
        .to_string();
    let r = t.inside(&a, &["agent", &b, "go"], None);
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("was not started from"), "{}", r.stderr);

    // b's stew.yaml with `from: a`, pushed as a new, newer branch, then onto b's own
    let work = t.env.base.join("spoof");
    let notes = t.notes.to_str().unwrap();
    let branch = format!("stew/{b}");
    t.env.git(
        &t.env.base,
        &[
            "clone",
            "-q",
            "--branch",
            &branch,
            notes,
            work.to_str().unwrap(),
        ],
    );
    let yaml = fs::read_to_string(work.join("stew.yaml")).unwrap();
    let yaml: String = yaml
        .lines()
        .filter(|l| !l.starts_with("from:"))
        .map(|l| format!("{l}\n"))
        .collect();
    fs::write(work.join("stew.yaml"), format!("{yaml}from: {a}\n")).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    t.env.git(&work, &["commit", "-qam", "spoof"]);
    t.env.git(
        &work,
        &["push", "-q", "origin", "HEAD:refs/heads/stew/zzzz-0000"],
    );
    let r = t.inside(&a, &["agent", &b, "go"], None);
    assert_eq!(r.code, 2, "a spoof branch: {}", r.stderr);
    assert!(r.stderr.contains("was not started from"), "{}", r.stderr);
    // the other readers skip it too: b is not a's child, b's info is b's own
    let r = t.inside(&a, &["ls", "--from", ".", "--json"], None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!r.stdout.contains(&b), "{}", r.stdout);
    assert!(
        r.stderr.contains("stew/zzzz-0000"),
        "a warning: {}",
        r.stderr
    );
    let r = t.inside(&b, &["info"], None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!r.stdout.contains(&format!("from    {a}")), "{}", r.stdout);

    // on b's own branch (every stew can push there too): still not a's to start agents in
    t.env
        .git(&work, &["push", "-q", "origin", &format!("HEAD:{branch}")]);
    let r = t.inside(&a, &["agent", &b, "go"], None);
    assert_eq!(r.code, 2, "b's own branch: {}", r.stderr);
    // what the daemon recorded does: a child of a
    let r = t.inside(
        &a,
        &["new", "a", "--title", "Child", "--brief", "-"],
        Some(b"b"),
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let c = r.stdout.trim().to_string();
    let r = t.inside(&a, &["agent", &c, "go"], None);
    assert_eq!(r.code, 0, "{}", r.stderr);

    // b closed, `from: a` pushed onto its branch, then `stew open b`: still not a's, since
    // open takes `from` from the claim commit
    t.ok(&["close", "--force", &b]);
    t.env.git(&work, &["fetch", "-q", "origin", &branch]);
    t.env.git(&work, &["reset", "-q", "--hard", "FETCH_HEAD"]);
    let yaml = fs::read_to_string(work.join("stew.yaml")).unwrap();
    assert!(yaml.contains("state: closed"), "{yaml}");
    let yaml: String = yaml
        .lines()
        .filter(|l| !l.starts_with("from:"))
        .map(|l| format!("{l}\n"))
        .collect();
    fs::write(work.join("stew.yaml"), format!("{yaml}from: {a}\n")).unwrap();
    t.env.git(
        &work,
        &["commit", "-qa", "--allow-empty", "-m", "spoof while closed"],
    );
    t.env
        .git(&work, &["push", "-q", "origin", &format!("HEAD:{branch}")]);
    let r = t.ok(&["open", &b]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let r = t.inside(&a, &["agent", &b, "go"], None);
    assert_eq!(r.code, 2, "a reopened b: {}", r.stderr);
    assert!(r.stderr.contains("was not started from"), "{}", r.stderr);
    let reopened = t.env.git(
        &t.notes,
        &["show", &format!("refs/heads/{branch}:stew.yaml")],
    );
    assert!(!reopened.contains(&format!("from: {a}")), "{reopened}");
    for id in [&a, &b, &c] {
        t.ok(&["remove", "--force", id]);
    }
}
