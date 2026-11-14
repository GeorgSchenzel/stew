//! Stews end to end on the host driver: the daemon in the foreground, the CLI as its client,
//! real repos in tempdirs. Above all the pushed check: `remove` must refuse whatever it
//! would lose.

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::Env;
use serde_json::Value;

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

struct T {
    env: Env,
    d: Daemon,
    stews: PathBuf,
    notes: PathBuf,
}

fn daemon(env: &Env, extra: &[&str]) -> Daemon {
    let socket = env.base.join("run/stew.sock");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stew"));
    cmd.args(["daemon", "--socket", socket.to_str().unwrap()])
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    env.envs(&mut cmd);
    let child = cmd.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "the daemon didn't come up");
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon { child, socket }
}

/// Site `t` (the default) on `driver`: host in `stews`, or a bento that can't be reached;
/// site `t2` host in `<stews>2`.
fn config(notes: &Path, stews: &Path, driver: &str) -> String {
    let t = if driver == "bento" {
        "{driver: bento, via: /nonexistent/bento-via}".to_string()
    } else {
        format!("{{driver: host, root: {}}}", stews.display())
    };
    format!(
        "artifacts: {}\nscan: false\nruntime: none\nsite: t\nsites:\n  t: {t}\n  t2: {{driver: host, root: {}2}}\n",
        notes.display(),
        stews.display()
    )
}

fn setup() -> T {
    let env = Env::new();
    let notes = env.base.join("remotes/notes.git");
    env.git(
        &env.base,
        &["init", "-q", "--bare", notes.to_str().unwrap()],
    );
    let a = env.remote("a", &["main", "feat"], "main");
    let b = env.remote("b", &["master"], "master");
    env.write(
        ".config/stew/repos.yaml",
        &format!("a: {}\ngroup:\n  b: {}\n", a.display(), b.display()),
    );
    let stews = env.base.join("stews");
    env.write(".config/stew/config.yaml", &config(&notes, &stews, "host"));
    let d = daemon(&env, &[]);
    T {
        env,
        d,
        stews,
        notes,
    }
}

impl T {
    fn stew(&self, args: &[&str]) -> Run {
        self.stew_in(args, &self.env.base, None)
    }

    fn stew_in(&self, args: &[&str], cwd: &Path, stdin: Option<&[u8]>) -> Run {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stew"));
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        self.env.envs(&mut cmd);
        cmd.env("STEW_SOCKET", &self.d.socket);
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

    fn ok(&self, args: &[&str]) -> Run {
        let r = self.stew(args);
        assert_eq!(r.code, 0, "stew {args:?}: {}", r.stderr);
        r
    }

    fn create(&self, args: &[&str]) -> String {
        let mut all = vec!["new"];
        all.extend(args);
        self.ok(&all).stdout.trim().to_string()
    }

    fn root(&self, id: &str) -> PathBuf {
        self.stews.join(id)
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        self.env.git(dir, args)
    }

    /// A shell command in `dir`, as an agent in the stew would run it.
    fn sh(&self, dir: &Path, script: &str) {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]).current_dir(dir);
        self.env.envs(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A file on the stew's notes branch, on the remote.
    fn tracked_file(&self, id: &str, file: &str) -> String {
        self.git(
            &self.notes,
            &["show", &format!("refs/heads/stew/{id}:{file}")],
        )
    }

    fn ls(&self) -> Value {
        let r = self.ok(&["ls", "--json"]);
        serde_json::from_str(&r.stdout).unwrap()
    }
}

fn yaml(text: &str) -> serde_yaml::Value {
    serde_yaml::from_str(text).unwrap()
}

#[test]
fn new_remove_open_close() {
    let t = setup();
    let id = t.create(&["a", "b", "--title", "Test it"]);
    assert!(id.starts_with("test-it-"), "{id}");
    let root = t.root(&id);
    assert_eq!(
        t.git(&root.join("a"), &["branch", "--show-current"]),
        "main"
    );
    assert_eq!(
        t.git(&root.join("b"), &["branch", "--show-current"]),
        "master"
    );
    assert_eq!(
        t.git(&root.join("notes"), &["branch", "--show-current"]),
        format!("stew/{id}")
    );
    assert_eq!(
        t.git(&root.join("notes"), &["rev-parse", "--abbrev-ref", "@{u}"]),
        format!("origin/stew/{id}")
    );
    let meta = yaml(&fs::read_to_string(root.join("notes/stew.yaml")).unwrap());
    assert_eq!(meta["title"], "Test it");
    assert_eq!(meta["repos"][1]["name"], "group/b");
    assert_eq!(meta["repos"][1]["branch"], "master");

    // ls: live from the root, with Now lines
    fs::write(
        root.join("notes/README.md"),
        "# Test it\n\nAbout.\n\n## Now\n- waiting: Georg looks\n",
    )
    .unwrap();
    let v = t.ls();
    let s = &v["stews"][0];
    assert_eq!(
        (s["id"].as_str(), s["site"].as_str()),
        (Some(id.as_str()), Some("t"))
    );
    assert_eq!(s["state"], "running");
    assert_eq!(s["now"][0], "waiting: Georg looks");
    assert_eq!(s["repos"][0]["dir"], "a");
    let human = t.ok(&["ls"]).stdout;
    assert!(human.contains("a@main  b@master"), "{human}");
    assert!(human.contains("- waiting: Georg looks"), "{human}");
    t.sh(&root.join("notes"), "git commit -qam now && git push -q");

    // exec: stdin, exit codes, the stew's env, in its root
    let r = t.stew_in(
        &[
            "exec",
            &id,
            "--",
            "sh",
            "-c",
            "cat; pwd; test -n \"$STEW_TOKEN\"; echo $STEW_HUB; exit 7",
        ],
        &t.env.base,
        Some(b"in\n"),
    );
    assert_eq!(r.code, 7, "{}", r.stderr);
    assert_eq!(
        r.stdout,
        format!("in\n{}\nunix:{}\n", root.display(), t.d.socket.display())
    );
    assert_ne!(t.stew(&["exec", "nope-0000", "--", "true"]).code, 0);

    // an unpushed commit: refused, nothing removed
    t.sh(
        &root.join("a"),
        "git switch -qc work && echo x > x && git add x && git commit -qm x",
    );
    let r = t.stew(&["remove", &id]);
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(
        r.stderr
            .contains("branch work has 1 commit(s) on no remote"),
        "{}",
        r.stderr
    );
    assert!(root.join("a/x").exists());

    // pushed: removed, with a snapshot of what was there
    t.sh(&root.join("a"), "git push -q -u origin work");
    let r = t.ok(&["remove", &id]);
    assert!(!root.exists(), "{}", r.stderr);
    let snap = yaml(&t.tracked_file(&id, "stew.yaml"));
    assert_eq!(snap["repos"][0]["name"], "a");
    assert_eq!(snap["repos"][0]["branch"], "work");
    assert_eq!(snap["repos"][1]["name"], "group/b");
    assert!(snap.get("state").is_none());
    assert_eq!(t.ls()["stews"], serde_json::json!([]));
    let r = t.stew(&["remove", &id]);
    assert!(
        r.code != 0 && r.stderr.contains("on no site"),
        "{}",
        r.stderr
    );

    // open: back on the recorded branches, on another site
    assert_eq!(t.ok(&["open", &id, "--site", "t2"]).stdout.trim(), id);
    let root2 = t.env.base.join("stews2").join(&id);
    assert_eq!(
        t.git(&root2.join("a"), &["branch", "--show-current"]),
        "work"
    );
    assert!(root2.join("a/x").exists());
    assert_eq!(
        t.git(&root2.join("b"), &["branch", "--show-current"]),
        "master"
    );
    let r = t.stew(&["open", &id]);
    assert!(
        r.code != 0 && r.stderr.contains("already on site t2"),
        "{}",
        r.stderr
    );

    // close: closed in stew.yaml, a LOG line, out of ls --all
    t.ok(&["close", &id]);
    assert!(!root2.exists());
    let snap = yaml(&t.tracked_file(&id, "stew.yaml"));
    assert_eq!(snap["state"], "closed");
    assert!(snap["closed"].as_str().unwrap().starts_with("20"));
    let log = t.tracked_file(&id, "LOG.md");
    assert!(log.trim_end().ends_with(" closed"), "{log}");
    let all = t.ok(&["ls", "--all", "--json"]).stdout;
    assert!(!all.contains(&id), "{all}");
    assert!(t.ok(&["ls", "--all", "--closed"]).stdout.contains(&id));
    assert!(t.ok(&["ls", "--waiting"]).stdout.is_empty());

    // open reopens it
    t.ok(&["open", &id]);
    let meta = yaml(&fs::read_to_string(root.join("notes/stew.yaml")).unwrap());
    assert_eq!(meta["state"], "open");
    assert!(meta.get("closed").is_none());
    let log = t.tracked_file(&id, "LOG.md");
    assert!(log.trim_end().ends_with(" reopened"), "{log}");
    assert_eq!(
        log.lines().filter(|l| l.starts_with("- ")).count(),
        3,
        "{log}"
    );
    assert!(t.ok(&["ls", "--all"]).stdout.contains(&id));
    let w = t.ok(&["ls", "--waiting"]).stdout;
    assert!(w.contains(&format!("{id}  waiting: Georg looks")), "{w}");
    t.ok(&["remove", &id]);
}

#[test]
fn branch_base_title_stop_start() {
    let t = setup();
    let id = t.create(&[
        "a",
        "b",
        "--title",
        "Feature work",
        "--branch",
        "--base",
        "feat",
    ]);
    let root = t.root(&id);
    assert_eq!(
        t.git(&root.join("a"), &["branch", "--show-current"]),
        "feature-work"
    );
    assert_eq!(
        t.git(&root.join("a"), &["rev-parse", "HEAD"]),
        t.git(&root.join("a"), &["rev-parse", "origin/feat"])
    );
    // b has no feat: its default, with a warning
    assert_eq!(
        t.git(&root.join("b"), &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feature-work"
    );
    let id2 = t.create(&["a", "--title", "x", "--branch=my/topic"]);
    assert_eq!(
        t.git(&t.root(&id2).join("a"), &["branch", "--show-current"]),
        "my/topic"
    );
    let r = t.stew(&["new", "a", "--title", "x", "--branch=bad..name"]);
    assert!(
        r.code != 0 && r.stderr.contains("not a valid branch"),
        "{}",
        r.stderr
    );
    let r = t.stew(&["new", "nope", "--title", "x"]);
    assert!(
        r.code != 0 && r.stderr.contains("no repo 'nope'"),
        "{}",
        r.stderr
    );

    // the base implied by the repo the CLI runs in
    let here = t.env.base.join("here");
    t.git(
        &t.env.base,
        &[
            "clone",
            "-q",
            "-b",
            "feat",
            t.env.base.join("remotes/a.git").to_str().unwrap(),
            "here",
        ],
    );
    let r = t.stew_in(&["new", "a", "--title", "here"], &here, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let id3 = r.stdout.trim();
    assert_eq!(
        t.git(&t.root(id3).join("a"), &["branch", "--show-current"]),
        "feat"
    );

    // title: written where the stew lives, pushed
    t.ok(&["title", &id, "A better title"]);
    let meta = yaml(&t.tracked_file(&id, "stew.yaml"));
    assert_eq!(meta["title"], "A better title");
    assert_eq!(
        t.ls()["stews"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id.as_str())
            .unwrap()["title"],
        "A better title"
    );

    // stop: a snapshot of the branches, a warning about unpushed work; host stops nothing
    t.sh(&root.join("b"), "echo y > y");
    let r = t.ok(&["stop", &id]);
    assert!(
        r.stderr.contains("not everything in") && r.stderr.contains("?? y"),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("stops nothing"), "{}", r.stderr);
    let meta = yaml(&t.tracked_file(&id, "stew.yaml"));
    assert_eq!(meta["repos"][0]["branch"], "feature-work");
    assert_eq!(meta["title"], "A better title");
    t.ok(&["start", &id]);
    t.ok(&["start", "--all"]);
    for i in [&id, &id2, &id3.to_string()] {
        t.ok(&["remove", "--force", i]);
    }
}

/// Every way of having work a remove would lose: each must be refused, the root kept.
#[test]
fn the_pushed_check_refuses_what_it_would_lose() {
    let t = setup();
    let outside = t.env.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let remote_a = t.env.base.join("remotes/a.git");
    let cases: Vec<(&str, String, &str)> = vec![
        ("modified", "echo changed >> a/README".into(), "a: uncommitted changes"),
        ("staged", "echo n > a/n && git -C a add n".into(), "a: uncommitted changes"),
        ("untracked", "echo n > a/n".into(), "?? n"),
        ("untracked hidden by config", "git -C a config status.showUntrackedFiles no && echo n > a/n".into(), "?? n"),
        ("untracked in notes", "echo n > notes/n".into(), "notes: uncommitted"),
        ("stash", "echo changed >> a/README && git -C a stash -q".into(), "a: has a stash"),
        ("commit", "cd a && echo c > c && git add c && git commit -qm c".into(), "branch main has 1 commit(s) on no remote"),
        ("new branch", "cd a && git switch -qc side && echo c > c && git add c && git commit -qm c && git switch -q main".into(), "branch side has 1"),
        ("notes commit", "cd notes && echo c > c && git add c && git commit -qm c".into(), "notes: branch stew/"),
        ("detached", "cd a && git switch -q --detach && echo c > c && git add c && git commit -qm c".into(), "a detached HEAD has 1"),
        ("tag", "git -C a tag v9".into(), "tag v9 is on no remote"),
        ("annotated tag", "git -C a tag -a v9 -m v9".into(), "tag v9 is on no remote"),
        ("stray file", "echo x > notes.txt".into(), "notes.txt: not in a repo"),
        ("hidden dir", "mkdir .cache && echo x > .cache/f".into(), ".cache: not in a repo"),
        ("repo without remote", "git init -q c && cd c && echo c > c && git add c && git commit -qm c".into(), "c: branch main has 1 commit(s) and no remote"),
        ("remote inside the stew", "git init -q --bare keep.git && cd a && git remote add keep ../keep.git && echo c > c && git add c && git commit -qm c && git push -q keep main".into(), "branch main has 1 commit(s) on no remote"),
        ("faked tracking ref", "cd a && echo c > c && git add c && git commit -qm c && git update-ref refs/remotes/origin/main HEAD".into(), "branch main has 1 commit(s) on no remote"),
        ("deleted on the remote", "cd a && git switch -qc gone && echo c > c && git add c && git commit -qm c && git push -q -u origin gone && git push -q origin :gone".into(), "branch gone has 1"),
        ("unreachable remote", format!("cd a && git remote set-url origin {}/nowhere.git && git switch -qc w && echo c > c && git add c && git commit -qm c && git switch -q main", t.env.base.display()), "branch w has 2 commit(s) and no remote"),
        ("assume-unchanged", "cd a && git update-index --assume-unchanged README && echo changed >> README".into(), "assume-unchanged"),
        ("skip-worktree", "cd a && git update-index --skip-worktree README && echo changed >> README".into(), "skip-worktree"),
        ("merge in progress", "cd a && git switch -qc m && echo 1 > README && git commit -qam m && git switch -q main && echo 2 > README && git commit -qam n && git push -q origin main m && (git merge -q m >/dev/null 2>&1 || true)".into(), "in progress (MERGE_HEAD)"),
        ("worktree in the root", "git -C a worktree add -q ../wt -b wtb && echo x > wt/new".into(), "wt: uncommitted"),
        ("worktree elsewhere", format!("git -C a worktree add -q {}/wt-out -b wtb && echo x > {}/wt-out/new", outside.display(), outside.display()), "(worktree "),
        ("worktree detached commit", format!("git -C a worktree add -q --detach {}/wt-d && cd {}/wt-d && echo c > c && git add c && git commit -qm c", outside.display(), outside.display()), "a detached HEAD has 1"),
        ("submodule commit", format!("cd a && git -c protocol.file.allow=always submodule add -q {} sub && git commit -qm sub && git push -q && cd sub && echo s > s && git add s && git commit -qm s && cd .. && git add sub && git commit -qm bump && git push -q", remote_a.display()), "a/sub: branch main has 1 commit(s) on no remote"),
        ("submodule dirty", format!("cd a && git -c protocol.file.allow=always submodule add -q {} sub2 && git commit -qm sub && git push -q && echo s > sub2/new", remote_a.display()), "a/sub2: uncommitted"),
        ("notes gone", "rm -rf notes".into(), "notes: missing"),
        ("hooks are not run", format!("cd a && printf '#!/bin/sh\\ntouch {o}/hook-ran\\n' > .git/hooks/reference-transaction && chmod +x .git/hooks/reference-transaction && git config core.fsmonitor 'touch {o}/fsmonitor-ran; false' && echo c > c", o = outside.display()), "?? c"),
    ];
    for (name, script, expect) in &cases {
        let id = t.create(&["a", "--title", name]);
        let root = t.root(&id);
        t.sh(&root, script);
        let r = t.stew(&["remove", &id]);
        assert_eq!(r.code, 2, "{name}: remove must refuse\n{}", r.stderr);
        assert!(
            r.stderr.contains(expect),
            "{name}: wanted {expect:?} in\n{}",
            r.stderr
        );
        assert!(
            root.join("a").exists() || *name == "notes gone",
            "{name}: the root was touched"
        );
        assert!(!outside.join("hook-ran").exists(), "{name}: ran a hook");
        assert!(
            !outside.join("fsmonitor-ran").exists(),
            "{name}: ran the repo's fsmonitor"
        );
        let r = t.stew(&["close", &id]);
        assert_eq!(r.code, 2, "{name}: close must refuse too\n{}", r.stderr);
        // --force removes it anyway, and says what was lost
        let r = t.stew(&["remove", "--force", &id]);
        assert_eq!(r.code, 0, "{name}: {}", r.stderr);
        assert!(
            r.stderr.contains("lost (--force)") || *name == "notes gone",
            "{name}: {}",
            r.stderr
        );
        assert!(!root.exists(), "{name}");
    }
}

/// What the check must let through: work that is on a remote that survives.
#[test]
fn the_pushed_check_passes_what_is_safe() {
    let t = setup();
    let outside = t.env.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let backup = t.env.base.join("backup.git");
    t.git(
        &t.env.base,
        &["init", "-q", "--bare", backup.to_str().unwrap()],
    );
    let cases: Vec<(&str, String)> = vec![
        ("clean", String::new()),
        ("ignored files", "printf 'target/\\n' > a/.git/info/exclude && mkdir a/target && echo x > a/target/out".into()),
        ("pushed to another remote outside", format!("cd a && git remote add backup {} && git switch -qc b2 && echo c > c && git add c && git commit -qm c && git push -q backup b2", backup.display())),
        ("a branch behind its remote", "cd a && git branch old HEAD && git switch -q old".into()),
        ("merged and deleted on the remote", "cd a && git switch -qc done && echo c > c && git add c && git commit -qm c && git push -q origin done:main && git push -q origin done && git push -q origin :done".into()),
        ("symlink to outside", format!("ln -s {} link", outside.display())),
        ("an empty dir", "mkdir scratch".into()),
        ("a reference repo with nothing of its own", format!("git clone -q {} ref", t.env.base.join("remotes/b.git").display())),
        ("remote tags", "cd a && git tag t1 && git push -q origin t1".into()),
        ("a pruned worktree", format!("git -C a worktree add -q {}/wt-gone -b wg && git -C a push -q origin wg && rm -rf {}/wt-gone", outside.display(), outside.display())),
    ];
    fs::write(outside.join("keep"), "x").unwrap();
    for (name, script) in &cases {
        let id = t.create(&["a", "--title", name]);
        let root = t.root(&id);
        if !script.is_empty() {
            t.sh(&root, script);
        }
        let r = t.stew(&["remove", &id]);
        assert_eq!(r.code, 0, "{name}: remove must pass\n{}", r.stderr);
        assert!(!root.exists(), "{name}");
        assert!(
            outside.join("keep").exists(),
            "{name}: removed outside the root"
        );
    }
}

#[test]
fn unreachable_sites_and_failed_creations() {
    let t = setup();
    let id = t.create(&["a", "--title", "Far away"]);
    // the site's driver can't be reached: ls still lists it, from the notes branch
    let stews = t.stews.clone();
    t.env.write(
        ".config/stew/config.yaml",
        &config(&t.notes, &stews, "bento"),
    );
    let v = t.ls();
    let s = &v["stews"][0];
    assert_eq!(
        (s["id"].as_str(), s["state"].as_str()),
        (Some(id.as_str()), Some("unreachable"))
    );
    assert_eq!(s["title"], "Far away");
    assert_eq!(s["repos"][0]["branch"], "main");
    let r = t.stew(&["remove", &id]);
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("cannot verify"), "{}", r.stderr);
    assert!(t.root(&id).exists());
    t.env.write(
        ".config/stew/config.yaml",
        &config(&t.notes, &stews, "host"),
    );

    // a clone that fails: the stew is removed again, the creation shows as failed
    // (b's refs are still listed, its objects are gone)
    let objects = t.env.base.join("remotes/b.git/objects");
    let away = t.env.base.join("b-objects");
    fs::rename(&objects, &away).unwrap();
    fs::create_dir_all(objects.join("pack")).unwrap();
    let r = t.stew(&["new", "a", "b", "--title", "Broken"]);
    fs::remove_dir_all(&objects).unwrap();
    fs::rename(&away, &objects).unwrap();
    assert_ne!(r.code, 0);
    assert!(r.stderr.contains("cloning failed"), "{}", r.stderr);
    let v = t.ls();
    let c = &v["creations"][0];
    assert_eq!(c["state"], "failed");
    let failed = c["id"].as_str().unwrap();
    assert!(failed.starts_with("broken-"));
    assert!(!t.root(failed).exists(), "a failed creation leaves no root");
    // the branch stays as its record
    assert!(t.tracked_file(failed, "stew.yaml").contains("Broken"));
    assert!(t.ok(&["ls"]).stdout.contains("creation failed"));

    // a site that refuses to create: nothing of anyone else's is removed
    fs::write(t.env.base.join("stews2"), "a file, not a dir").unwrap();
    let r = t.stew(&["new", "a", "--title", "x", "--site", "t2"]);
    assert_ne!(r.code, 0);
    assert!(t.root(&id).exists());
    t.ok(&["remove", &id]);
}

#[test]
fn the_daemon_exits_when_idle() {
    let env = Env::new();
    let mut d = daemon(&env, &["--idle-exit", "1"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(st) = d.child.try_wait().unwrap() {
            assert!(st.success());
            break;
        }
        assert!(Instant::now() < deadline, "still running");
        std::thread::sleep(Duration::from_millis(50));
    }
}
