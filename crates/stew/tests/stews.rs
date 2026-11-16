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
    let d = daemon(&env, &["--fetch-every", "1"]);
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
    // `runtime: none` can start no agent: refused before anything is claimed or made
    let r = t.stew_in(
        &["new", "a", "--title", "No agent", "--brief", "-", "--start"],
        &t.env.base,
        Some(b"b"),
    );
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("no agent runtime"), "{}", r.stderr);
    assert!(
        t.git(&t.notes, &["for-each-ref", "refs/heads/stew/"])
            .is_empty(),
        "nothing claimed"
    );
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

    // ls: its site and state, and README as pushed (once the daemon has fetched it)
    let readme = "# Test it\n\nAbout.\n\n## Now\n- waiting: Georg looks\n";
    fs::write(root.join("notes/README.md"), readme).unwrap();
    t.sh(&root.join("notes"), "git commit -qam now && git push -q");
    let deadline = Instant::now() + Duration::from_secs(10);
    while t.ls()["stews"][0]["readme"] != readme {
        assert!(Instant::now() < deadline, "not fetched");
        std::thread::sleep(Duration::from_millis(100));
    }
    let v = t.ls();
    let s = &v["stews"][0];
    assert_eq!(
        (s["id"].as_str(), s["site"].as_str()),
        (Some(id.as_str()), Some("t"))
    );
    assert_eq!(s["state"], "running");
    let human = t.ok(&["ls"]).stdout;
    assert!(human.starts_with("Test it  "), "{human}");
    assert!(human.contains("  t  running"), "{human}");

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
        format!(
            "in\n{}\nunix:{}\n",
            root.display(),
            t.d.socket.with_file_name("box.sock").display()
        )
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
    // still listed, on no site
    let v = t.ls();
    assert_eq!(v["stews"][0]["state"], "removed");
    assert!(v["stews"][0].get("site").is_none());

    // close on no site: only its branch, closed with a LOG line; out of ls
    t.ok(&["close", &id]);
    let snap = yaml(&t.tracked_file(&id, "stew.yaml"));
    assert_eq!(snap["state"], "closed");
    assert_eq!(snap["repos"][0]["branch"], "work", "the rest as it was");
    let log = t.tracked_file(&id, "LOG.md");
    assert!(log.trim_end().ends_with(" closed"), "{log}");
    assert!(
        t.tracked_file(&id, "README.md")
            .contains("waiting: Georg looks")
    );
    assert!(!t.ok(&["ls"]).stdout.contains("Test it"));
    let r = t.stew(&["close", &id]);
    assert!(
        r.code != 0 && r.stderr.contains("closed already"),
        "{}",
        r.stderr
    );
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

    // close: closed in stew.yaml, a LOG line, out of ls
    t.ok(&["close", &id]);
    assert!(!root2.exists());
    let snap = yaml(&t.tracked_file(&id, "stew.yaml"));
    assert_eq!(snap["state"], "closed");
    assert!(snap["closed"].as_str().unwrap().starts_with("20"));
    let log = t.tracked_file(&id, "LOG.md");
    assert!(log.trim_end().ends_with(" closed"), "{log}");
    let all = t.ok(&["ls", "--json"]).stdout;
    assert!(!all.contains(&id), "{all}");
    let v: Value = serde_json::from_str(&t.ok(&["ls", "--closed", "--json"]).stdout).unwrap();
    assert_eq!(v["stews"][0]["state"], "closed");

    // open reopens it
    t.ok(&["open", &id]);
    let meta = yaml(&fs::read_to_string(root.join("notes/stew.yaml")).unwrap());
    assert_eq!(meta["state"], "open");
    assert!(meta.get("closed").is_none());
    let log = t.tracked_file(&id, "LOG.md");
    assert!(log.trim_end().ends_with(" reopened"), "{log}");
    // created, closed (on no site), reopened, closed, reopened
    assert_eq!(
        log.lines().filter(|l| l.starts_with("- ")).count(),
        5,
        "{log}"
    );
    assert_eq!(t.ls()["stews"][0]["state"], "running");
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
    // its short form names it, with the steps on stderr
    let short = id.rsplit_once('-').unwrap().1;
    let r = t.ok(&["start", short]);
    assert!(
        r.stderr.contains(&format!("stew: {id}: starting on")),
        "{}",
        r.stderr
    );
    t.ok(&["exec", short, "--", "true"]);
    let r = t.stew(&["start", "zzzz"]);
    assert!(
        r.code != 0 && r.stderr.contains("no stew zzzz"),
        "{}",
        r.stderr
    );
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
        // the check's own fetch must not rewrite or prune what it checks
        ("prune tags by config", "cd a && git config fetch.prune true && git config fetch.pruneTags true && echo c > c && git add c && git commit -qm c && git tag v2 && git reset -q --hard HEAD~1".into(), "tag v2 is on no remote"),
        ("prune by the remote's config", "cd a && git config remote.origin.prune true && git config remote.origin.pruneTags true && git tag v2".into(), "tag v2 is on no remote"),
        ("a forced refspec into a branch", "cd a && git push -q origin main:other && git switch -qc other && echo c > c && git add c && git commit -qm wip && git switch -q main && git config --add remote.origin.fetch '+refs/heads/other:refs/heads/other'".into(), "branch other has 1 commit(s) on no remote"),
        ("a mirror refspec", "cd a && git switch -qc side && echo c > c && git add c && git commit -qm c && git switch -q main && git config remote.origin.fetch '+refs/*:refs/*'".into(), "branch side has 1"),
        ("a forced tag refspec", "cd a && git push -q origin main:refs/tags/v3 && echo c > c && git add c && git commit -qm c && git tag v3 && git reset -q --hard HEAD~1 && git config --add remote.origin.fetch '+refs/tags/*:refs/tags/*' && git config remote.origin.tagOpt --tags".into(), "tag v3 is on no remote"),
        ("a deinit'd submodule", format!("cd a && git -c protocol.file.allow=always submodule add -q {} sub3 && git commit -qm sub && git push -q && cd sub3 && echo s > s && git add s && git commit -qm s && cd .. && git add sub3 && git commit -qm bump && git push -q && git submodule -q deinit -f sub3", remote_a.display()), "a (module sub3): branch main has 1 commit(s) on no remote"),
        ("a worktree whose dir is gone", format!("git -C a worktree add -q --detach {o}/wt-g && cd {o}/wt-g && echo c > c && git add c && git commit -qm c && cd / && rm -rf {o}/wt-g", o = outside.display()), "(its dir is gone): a detached HEAD has 1"),
        ("an ignored nested repo", "printf 'third/\\n' >> a/.git/info/exclude && mkdir -p a/third && git init -q a/third/x && cd a/third/x && echo c > c && git add c && git commit -qm c".into(), "a/third/x: branch main has 1 commit(s) and no remote"),
        ("a rebase in a worktree elsewhere", format!("git -C a worktree add -q {o}/wt-r -b wr && cd {o}/wt-r && echo 1 > f && git add f && git commit -qm f && git push -q origin wr && GIT_SEQUENCE_EDITOR='sed -i s/^pick/edit/' git rebase -q -i HEAD~1", o = outside.display()), "in progress (rebase-merge)"),
        ("a linked worktree's own ref", format!("git -C a worktree add -q --detach {o}/wt-k && cd {o}/wt-k && echo c > c && git add c && git commit -qm c && git update-ref refs/worktree/keep HEAD && git switch -q --detach origin/main", o = outside.display()), "wt-k): refs/worktree/keep has 1 commit(s) on no remote"),
        ("a submodule initialised in a linked worktree", format!("cd a && git -c protocol.file.allow=always submodule add -q {r} sub4 && git commit -qm sub && git push -q && git worktree add -q --detach {o}/wt-s && cd {o}/wt-s && git -c protocol.file.allow=always submodule -q update --init sub4 && cd sub4 && git switch -qc keep && echo s > s && git add s && git commit -qm s && git switch -q --detach HEAD~1", r = remote_a.display(), o = outside.display()), "a (module worktrees/wt-s/modules/sub4): branch keep has 1 commit(s) on no remote"),
        ("hooks are not run", format!("cd a && printf '#!/bin/sh\\ntouch {o}/hook-ran\\n' > .git/hooks/reference-transaction && chmod +x .git/hooks/reference-transaction && git config core.fsmonitor 'touch {o}/fsmonitor-ran; false' && echo c > c", o = outside.display()), "?? c"),
    ];
    // every ref of every repo (and submodule git dir) the check looks at, and whether a
    // FETCH_HEAD appeared
    let refs = |root: &Path| -> String {
        let mut dirs = Vec::new();
        for repo in ["a", "notes", "a/third/x"] {
            let git = root.join(repo).join(".git");
            if !git.is_dir() {
                continue;
            }
            let mut modules = vec![git.join("modules")];
            for w in fs::read_dir(git.join("worktrees")).into_iter().flatten() {
                modules.push(w.unwrap().path().join("modules"));
            }
            dirs.push(git);
            for m in modules {
                for d in fs::read_dir(m).into_iter().flatten() {
                    dirs.push(d.unwrap().path());
                }
            }
        }
        let mut all = String::new();
        for dir in dirs {
            all.push_str(&format!("{}\n", dir.display()));
            all.push_str(&t.git(&dir, &["for-each-ref"]));
            all.push_str(&format!(
                "\nFETCH_HEAD {}\n",
                dir.join("FETCH_HEAD").exists()
            ));
        }
        all
    };
    for (name, script, expect) in &cases {
        let id = t.create(&["a", "--title", name]);
        let root = t.root(&id);
        t.sh(&root, script);
        let before = refs(&root);
        let r = t.stew(&["remove", &id]);
        assert_eq!(r.code, 2, "{name}: remove must refuse\n{}", r.stderr);
        assert_eq!(refs(&root), before, "{name}: the check changed refs");
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
        ("only an upstream remote", "git -C a remote rename origin upstream".into()),
        ("an ignored nested clone of a pushed repo", format!("printf 'deps/\\n' >> a/.git/info/exclude && mkdir a/deps && git clone -q {} a/deps/b", t.env.base.join("remotes/b.git").display())),
        ("a deinit'd submodule with all pushed", format!("cd a && git -c protocol.file.allow=always submodule add -q {} sub && git commit -qm sub && git push -q && git submodule -q deinit -f sub", t.env.base.join("remotes/a.git").display())),
        ("pruning config", "cd a && git config fetch.prune true && git config fetch.pruneTags true && git config --add remote.origin.fetch '+refs/*:refs/*'".into()),
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
        let meta = yaml(&t.tracked_file(&id, "stew.yaml"));
        assert_eq!(
            meta["repos"][0]["name"], "a",
            "{name}: the snapshot keeps the repo"
        );
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

/// In a box nothing local survives, and neither does a git server in the box itself.
#[test]
fn remotes_on_the_box_itself_die_with_it() {
    let env = Env::new();
    let root = env.base.join("root");
    fs::create_dir_all(root.join("r")).unwrap();
    env.git(&root.join("r"), &["init", "-q"]);
    fs::write(root.join("r/f"), "x").unwrap();
    env.git(&root.join("r"), &["add", "f"]);
    env.git(&root.join("r"), &["commit", "-qm", "f"]);
    for (name, url) in [
        ("lo", "http://127.0.0.1:9/x.git"),
        ("name", "ssh://git@localhost/x.git"),
        ("v6", "http://[::1]:9/x.git"),
        ("scp", "git@localhost:x.git"),
        ("far", "https://example.invalid/x.git"),
    ] {
        env.git(&root.join("r"), &["remote", "add", name, url]);
    }
    let mut cmd = Command::new("sh");
    cmd.args(["-c", stew::inspect::INSPECT, "stew", "check", "none"])
        .current_dir(&root);
    env.envs(&mut cmd);
    let out = String::from_utf8(cmd.output().unwrap().stdout).unwrap();
    for name in ["lo", "name", "v6", "scp"] {
        assert!(
            out.contains(&format!("W r: remote {name} ("))
                && out.contains("is removed with the stew"),
            "{name}: {out}"
        );
    }
    assert!(!out.contains("remote far ("), "{out}");
    assert!(out.contains("W r: cannot fetch far"), "{out}");
    assert!(
        out.contains("branch main has 1 commit(s) and no remote"),
        "{out}"
    );
}
