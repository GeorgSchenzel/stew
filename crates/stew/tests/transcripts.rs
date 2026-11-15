//! Sessions export on the host driver: the daemon in the foreground, fixture claude sessions
//! under the test's HOME, a stand-in `opencode` (never a real agent or real sessions), a
//! bare transcripts remote in a tempdir.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::Env;
use serde_json::Value;

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

struct T {
    env: Env,
    d: Daemon,
    stews: PathBuf,
    remote: PathBuf,
    /// The stand-in opencode's fixtures and call log.
    fix: PathBuf,
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// `opencode` as the export uses it: `--version`; `session list --format json`, listing a
/// session of another dir always and those in `$OC_FIX/sessions` (`<dir>\t<id>\t<updated>`)
/// whose dir is the cwd; `session export <id>` printing `$OC_FIX/<id>.json`. Every call is
/// logged; without `--standalone` it fails; with `$OC_FIX/fail` present it fails.
const OPENCODE: &str = r#"#!/bin/sh
fix=$OC_FIX
d=$(pwd -P)
printf '%s\t%s\n' "$d" "$*" >>"$fix/calls"
if [ "$1" = --version ]; then echo "opencode v9.9.9"; exit 0; fi
case " $* " in *" --standalone "*) ;; *) echo "not standalone" >&2; exit 9 ;; esac
if [ -e "$fix/fail" ]; then echo "it broke" >&2; exit 3; fi
case "$1 $2" in
"session list")
	printf '[{"id":"ses_foreign","title":"x","updated":1,"created":1,"projectId":"g","directory":"/elsewhere"}'
	tab=$(printf '\t')
	while IFS=$tab read -r dir id up; do
		[ "$dir" = "$d" ] || continue
		printf ',{"id":"%s","title":"t","updated":%s,"created":1000,"projectId":"p","directory":"%s"}' "$id" "$up" "$dir"
	done <"$fix/sessions"
	printf ']\n'
	;;
"session export") cat "$fix/$3.json" ;;
*) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#;

fn setup() -> T {
    let env = Env::new();
    let notes = env.base.join("remotes/notes.git");
    let remote = env.base.join("remotes/transcripts.git");
    for r in [&notes, &remote] {
        env.git(&env.base, &["init", "-q", "--bare", r.to_str().unwrap()]);
    }
    let a = env.remote("a", &["main"], "main");
    env.write(".config/stew/repos.yaml", &format!("a: {}\n", a.display()));
    let stews = env.base.join("stews");
    env.write(
        ".config/stew/config.yaml",
        &format!(
            "artifacts: {}\ntranscripts: {}\nscan: false\nruntime: none\nsite: t\nsites:\n  t: {{driver: host, root: {}}}\n",
            notes.display(),
            remote.display(),
            stews.display()
        ),
    );
    let fix = env.base.join("oc");
    let bin = env.base.join("bin");
    fs::create_dir_all(&fix).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(fix.join("sessions"), "").unwrap();
    fs::write(bin.join("opencode"), OPENCODE).unwrap();
    fs::set_permissions(bin.join("opencode"), fs::Permissions::from_mode(0o755)).unwrap();

    let socket = env.base.join("run/stew.sock");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stew"));
    cmd.args(["daemon", "--socket", socket.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    env.envs(&mut cmd);
    let path = std::env::var("PATH").unwrap_or_default();
    cmd.env("PATH", format!("{}:{path}", bin.display()))
        .env("OC_FIX", &fix)
        .env_remove("CLAUDE_CONFIG_DIR");
    let child = cmd.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "the daemon didn't come up");
        std::thread::sleep(Duration::from_millis(20));
    }
    T {
        env,
        d: Daemon { child, socket },
        stews,
        remote,
        fix,
    }
}

impl T {
    fn stew(&self, args: &[&str]) -> Run {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stew"));
        cmd.args(args)
            .current_dir(&self.env.base)
            .stdin(Stdio::null());
        self.env.envs(&mut cmd);
        cmd.env("STEW_SOCKET", &self.d.socket);
        let out = cmd.output().unwrap();
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn ok(&self, args: &[&str]) -> Run {
        let r = self.stew(args);
        assert_eq!(r.code, 0, "stew {args:?}: {}", r.stderr);
        r
    }

    fn checkout(&self) -> PathBuf {
        self.env.home.join(".local/share/stew/transcripts")
    }

    /// A file on the remote's main, if there.
    fn pushed(&self, file: &str) -> Option<String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.remote)
            .args(["show", &format!("main:{file}")]);
        self.env.envs(&mut cmd);
        let out = cmd.output().unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.fix.join("calls")).unwrap_or_default()
    }

    fn exports(&self) -> usize {
        self.calls().matches("session export").count()
    }

    /// A claude session jsonl under `~/.claude/projects/<dir encoded>/`, with `cwd`.
    fn claude(&self, dir: &str, session: &str, cwd: &Path, lines: &[&str]) -> PathBuf {
        let enc: String = dir
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let d = self.env.home.join(".claude/projects").join(enc);
        fs::create_dir_all(&d).unwrap();
        let f = d.join(format!("{session}.jsonl"));
        let mut text = String::from("{\"type\":\"summary\",\"summary\":\"s\"}\n");
        for (i, l) in lines.iter().enumerate() {
            text.push_str(&format!(
                "{{\"cwd\":\"{}\",\"sessionId\":\"{session}\",\"version\":\"2.1.{i}\",\"timestamp\":\"2026-11-15T10:00:0{i}.000Z\",\"message\":\"{l}\"}}\n",
                cwd.display()
            ));
        }
        fs::write(&f, text).unwrap();
        f
    }

    fn oc_session(&self, dir: &Path, id: &str, updated: u64, body: &str) {
        let mut list = fs::read_to_string(self.fix.join("sessions")).unwrap();
        let lines: Vec<String> = list
            .lines()
            .filter(|l| !l.contains(&format!("\t{id}\t")))
            .map(String::from)
            .collect();
        list = lines.join("\n");
        if !list.is_empty() {
            list.push('\n');
        }
        list.push_str(&format!("{}\t{id}\t{updated}\n", dir.display()));
        fs::write(self.fix.join("sessions"), list).unwrap();
        fs::write(
            self.fix.join(format!("{id}.json")),
            format!("{{\"info\":{{\"id\":\"{id}\"}},\"messages\":[\"{body}\"]}}\n"),
        )
        .unwrap();
    }
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

#[test]
fn sessions_go_to_the_transcripts_repo() {
    let t = setup();
    let id = t
        .ok(&["new", "a", "--title", "Talk"])
        .stdout
        .trim()
        .to_string();
    let root = t.stews.join(&id);
    let r = root.to_str().unwrap();

    // ours: a session in the root, one in a clone; not ours: a dir whose encoding looks
    // like a subdir but whose cwd is another stew's root, and another stew entirely
    let s1 = t.claude(r, "s1", &root, &["hi", "there"]);
    t.claude(&format!("{r}/a"), "s2", &root.join("a"), &["in a"]);
    t.claude(
        &format!("{r}-x"),
        "s3",
        Path::new(&format!("{r}-x")),
        &["other"],
    );
    t.claude("/elsewhere", "s4", Path::new("/elsewhere"), &["other"]);
    t.oc_session(&root, "ses_root", 5000, "root one");
    t.oc_session(&root.join("a"), "ses_a", 6000, "a one");

    let out = t.ok(&["export", &id]);
    assert!(
        out.stdout
            .contains(&format!("{id}: 4 session(s), 8 file(s) written")),
        "{}{}",
        out.stdout,
        out.stderr
    );
    // the first export commits (nothing was committed before) and pushes
    let s1_text = fs::read_to_string(&s1).unwrap();
    assert_eq!(
        t.pushed(&format!("{id}/claude-s1.jsonl")).as_deref(),
        Some(s1_text.as_str())
    );
    let meta = json(&t.pushed(&format!("{id}/claude-s1.meta.json")).unwrap());
    assert_eq!(meta["agent"], "claude");
    assert_eq!(meta["version"], "2.1.1");
    assert_eq!(meta["stew"], id.as_str());
    assert_eq!(meta["session"], "s1");
    assert_eq!(meta["dir"], ".");
    assert_eq!(meta["created"], "2026-11-15T10:00:00.000Z");
    assert_eq!(meta["updated"], "2026-11-15T10:00:01.000Z");
    let meta = json(&t.pushed(&format!("{id}/claude-s2.meta.json")).unwrap());
    assert_eq!(meta["dir"], "a");
    let meta = json(&t.pushed(&format!("{id}/opencode-ses_a.meta.json")).unwrap());
    assert_eq!(meta["agent"], "opencode");
    assert_eq!(meta["version"], "opencode v9.9.9");
    assert_eq!(meta["dir"], "a");
    assert_eq!(meta["created"], "1970-01-01T00:00:01.000Z");
    assert_eq!(meta["updated"], "1970-01-01T00:00:06.000Z");
    assert!(
        t.pushed(&format!("{id}/opencode-ses_root.json"))
            .unwrap()
            .contains("root one")
    );
    for gone in [
        "claude-s3.jsonl",
        "claude-s4.jsonl",
        "opencode-ses_foreign.json",
    ] {
        assert!(t.pushed(&format!("{id}/{gone}")).is_none(), "{gone}");
    }
    // nothing absolute of the host in the metadata
    assert!(
        !t.pushed(&format!("{id}/claude-s1.meta.json"))
            .unwrap()
            .contains(r)
    );
    // list ran in the root and the clones, export in the session's dir, all standalone
    let calls = t.calls();
    assert!(
        calls.contains(&format!("{}/a\tsession export ses_a --standalone", r)),
        "{calls}"
    );
    assert!(
        calls.contains(&format!("{r}\tsession list --format json")),
        "{calls}"
    );
    assert!(
        calls.contains(&format!("{r}/notes\tsession list")),
        "{calls}"
    );

    // unchanged: nothing sent again, nothing written
    let before = t.exports();
    let out = t.ok(&["export", &id]);
    assert!(
        out.stdout.contains("4 session(s), 0 file(s) written"),
        "{}",
        out.stdout
    );
    assert_eq!(t.exports(), before);

    // changed: written, but a timer run commits only daily
    t.claude(r, "s1", &root, &["hi", "there", "more"]);
    t.oc_session(&root.join("a"), "ses_a", 7000, "a two");
    let out = t.ok(&["export", "--all"]);
    assert!(
        out.stdout.contains("4 session(s), 4 file(s) written"),
        "{}",
        out.stdout
    );
    assert_eq!(t.exports(), before + 1);
    assert!(
        !t.pushed(&format!("{id}/claude-s1.jsonl"))
            .unwrap()
            .contains("more")
    );
    // a day later it does
    let co = t.checkout();
    t.env.git_env(
        &co,
        &[
            "commit",
            "-q",
            "--amend",
            "--no-edit",
            "--date=2000-01-01T00:00:00Z",
        ],
        &[("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")],
    );
    t.ok(&["export", "--all"]);
    assert!(
        t.pushed(&format!("{id}/claude-s1.jsonl"))
            .unwrap()
            .contains("more")
    );
    assert!(
        t.pushed(&format!("{id}/opencode-ses_a.json"))
            .unwrap()
            .contains("a two")
    );

    // --push commits at once
    t.claude(r, "s1", &root, &["four"]);
    t.ok(&["export", &id, "--push"]);
    assert!(
        t.pushed(&format!("{id}/claude-s1.jsonl"))
            .unwrap()
            .contains("four")
    );

    // stop exports and pushes before it stops
    t.claude(r, "s1", &root, &["five"]);
    t.ok(&["stop", &id]);
    assert!(
        t.pushed(&format!("{id}/claude-s1.jsonl"))
            .unwrap()
            .contains("five")
    );

    // remove: a failing export refuses (nothing removed); --force removes anyway
    t.claude(r, "s1", &root, &["six"]);
    t.oc_session(&root, "ses_root", 9000, "root two");
    fs::write(t.fix.join("fail"), "").unwrap();
    let res = t.stew(&["remove", &id]);
    assert_eq!(res.code, 2, "{}", res.stderr);
    assert!(
        res.stderr.contains("sessions failed, nothing removed"),
        "{}",
        res.stderr
    );
    assert!(root.is_dir());
    fs::remove_file(t.fix.join("fail")).unwrap();
    // fixed: remove exports (the claude session too) and pushes, then removes
    let res = t.ok(&["remove", &id]);
    assert!(!root.exists(), "{}", res.stderr);
    assert!(
        t.pushed(&format!("{id}/claude-s1.jsonl"))
            .unwrap()
            .contains("six")
    );
    assert!(
        t.pushed(&format!("{id}/opencode-ses_root.json"))
            .unwrap()
            .contains("root two")
    );
}

#[test]
fn remove_force_and_no_transcripts_repo() {
    let t = setup();
    let id = t
        .ok(&["new", "a", "--title", "Force"])
        .stdout
        .trim()
        .to_string();
    let root = t.stews.join(&id);
    t.oc_session(&root, "ses_x", 1, "x");
    fs::write(t.fix.join("fail"), "").unwrap();
    let res = t.ok(&["remove", &id, "--force"]);
    assert!(
        res.stderr.contains("sessions not exported (--force)"),
        "{}",
        res.stderr
    );
    assert!(!root.exists());

    // a bad listing is incomplete, never silently empty
    fs::remove_file(t.fix.join("fail")).unwrap();
    let id = t
        .ok(&["new", "a", "--title", "Bad"])
        .stdout
        .trim()
        .to_string();
    let root = t.stews.join(&id);
    fs::write(
        t.fix.join("../bin/opencode"),
        "#!/bin/sh\n[ \"$1\" = --version ] && echo v1 && exit 0\necho 'No sessions, but not JSON'\n",
    )
    .unwrap();
    let out = t.ok(&["export", &id]);
    assert!(out.stdout.contains("(incomplete)"), "{}", out.stdout);
    assert!(out.stderr.contains("not a JSON list"), "{}", out.stderr);
    let res = t.stew(&["remove", &id]);
    assert_eq!(res.code, 2, "{}", res.stderr);
    assert!(root.is_dir());

    // without `transcripts:`: export refuses, remove warns and goes on
    let config = t.env.home.join(".config/stew/config.yaml");
    let text = fs::read_to_string(&config).unwrap();
    let text: String = text
        .lines()
        .filter(|l| !l.starts_with("transcripts:"))
        .map(|l| format!("{l}\n"))
        .collect();
    fs::write(&config, text).unwrap();
    let res = t.stew(&["export", "--all"]);
    assert_eq!(res.code, 1);
    assert!(res.stderr.contains("no transcripts repo"), "{}", res.stderr);
    let res = t.ok(&["remove", &id]);
    assert!(
        res.stderr.contains("sessions are not exported"),
        "{}",
        res.stderr
    );
    assert!(!root.exists());
}
