use std::path::Path;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use stew::daemon::{self, Hub};
use stew::error::{Error, Result};
use stew::paths::Paths;
use stew_proto::{Call, CreationInfo, Here, Live, Reply, Request, Tracked};

/// Stews: workspaces of repo clones in a box.
#[derive(Parser)]
#[command(name = "stew", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// A new stew: clones of the repos on their base, `notes/` on `stew/<id>`; prints its id
    New {
        /// Catalog names or remote URLs
        repos: Vec<String>,
        #[arg(long)]
        title: String,
        /// Where it lives (default: the config's `site`, else `desktop`)
        #[arg(long)]
        site: Option<String>,
        /// Create this branch from the base in every repo (bare: the title's slug)
        #[arg(long, num_args = 0..=1, default_missing_value = "", require_equals = true)]
        branch: Option<String>,
        /// The base branch for every repo that has it
        #[arg(long)]
        base: Option<String>,
    },
    /// Recreate a tracked stew on a site, on its recorded branches; reopens a closed one
    Open {
        id: String,
        #[arg(long)]
        site: Option<String>,
    },
    /// Take a stew off its site, once everything in it is pushed
    Remove {
        id: String,
        /// Remove even what is not pushed (it is lost)
        #[arg(long)]
        force: bool,
    },
    /// `remove`, and mark it closed
    Close {
        id: String,
        #[arg(long)]
        force: bool,
    },
    /// Start a stew, or every stew on a site
    Start {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        id: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Snapshot a stew, warn about unpushed work, stop it
    Stop { id: String },
    /// A shell where the stew lives
    Enter { id: String },
    /// Run a command where the stew lives, in its root, with stdin
    Exec {
        id: String,
        /// Fail on a stopped stew instead of starting it
        #[arg(long)]
        no_start: bool,
        /// A pty (needs a TTY)
        #[arg(short = 't', long)]
        tty: bool,
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Change a stew's title
    Title { id: String, title: String },
    /// List stews, most recent first: those on a site, or with --all every tracked one
    Ls {
        /// Every open tracked stew, from the artifacts repo
        #[arg(long)]
        all: bool,
        /// With --all: the closed ones instead
        #[arg(long)]
        closed: bool,
        /// Every Now line of an open stew that needs a human (`waiting:`, `blocked:`)
        #[arg(long, conflicts_with_all = ["all", "closed", "search"])]
        waiting: bool,
        /// With --all: open and closed stews whose id, title or notes contain this
        search: Option<String>,
        /// Tracked stews started from this one
        #[arg(long, conflicts_with_all = ["all", "closed", "waiting", "search"])]
        from: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// The hub's daemon (systemd starts it on demand; in tests, in the foreground)
    Daemon {
        /// Listen here instead of $XDG_RUNTIME_DIR/stew/stew.sock (or systemd's socket)
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        /// Exit after this many seconds without a request (none in progress)
        #[arg(long)]
        idle_exit: Option<u64>,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("stew: {e}");
            ExitCode::from(e.exit_code() as u8)
        }
    }
}

fn run(cli: Cli) -> Result<u8> {
    let paths = Paths::from_env()?;
    match cli.cmd {
        Cmd::Daemon { socket, idle_exit } => {
            let idle = idle_exit.map(Duration::from_secs);
            let (listener, socket) = match (socket, daemon::activated()) {
                (None, Some(l)) => (l, paths.socket()),
                (socket, _) => {
                    let socket = socket.unwrap_or_else(|| paths.socket());
                    (daemon::bind(&socket)?, socket)
                }
            };
            let hub = Arc::new(Hub::open(paths, socket)?);
            daemon::serve(hub, listener, idle)?;
            Ok(0)
        }
        Cmd::New {
            repos,
            title,
            site,
            branch,
            base,
        } => {
            let here = if base.is_none() { here() } else { None };
            let reply = call(
                &paths,
                Call::New {
                    repos,
                    title,
                    site,
                    branch,
                    base,
                    here,
                    from: None,
                },
            )?;
            created(reply)
        }
        Cmd::Open { id, site } => created(call(&paths, Call::Open { id, site })?),
        Cmd::Remove { id, force } => done(call(&paths, Call::Remove { id, force })?),
        Cmd::Close { id, force } => done(call(&paths, Call::Close { id, force })?),
        Cmd::Start { id, .. } => done(call(&paths, Call::Start { id })?),
        Cmd::Stop { id } => done(call(&paths, Call::Stop { id })?),
        Cmd::Title { id, title } => done(call(&paths, Call::Title { id, title })?),
        Cmd::Enter { id } => launch(
            &paths,
            Call::Where {
                id,
                argv: vec![],
                no_start: false,
                tty: true,
            },
        ),
        Cmd::Exec {
            id,
            no_start,
            tty,
            cmd,
        } => launch(
            &paths,
            Call::Where {
                id,
                argv: cmd,
                no_start,
                tty,
            },
        ),
        Cmd::Ls {
            from: Some(from),
            json,
            ..
        } => {
            let mut stews = Vec::new();
            for closed in [false, true] {
                let Reply::Tracked { stews: s, warnings } = call(
                    &paths,
                    Call::LsAll {
                        search: None,
                        closed,
                    },
                )?
                else {
                    return Err(Error::Fail("unexpected reply from the daemon".into()));
                };
                if !closed {
                    warn(&warnings);
                }
                stews.extend(
                    s.into_iter()
                        .filter(|t| t.meta.from.as_deref() == Some(from.as_str())),
                );
            }
            stews.sort_by_key(|t| std::cmp::Reverse(t.updated));
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&stews).expect("serializes")
                );
            } else {
                print!("{}", table(&stews));
            }
            Ok(0)
        }
        Cmd::Ls {
            all,
            closed,
            waiting,
            search,
            json,
            ..
        } => {
            if !(all || closed || waiting) {
                if search.is_some() {
                    return Err(Error::Fail("a search needs --all".into()));
                }
                let Reply::Live {
                    stews,
                    creations,
                    warnings,
                } = call(&paths, Call::Ls)?
                else {
                    return Err(Error::Fail("unexpected reply from the daemon".into()));
                };
                warn(&warnings);
                if json {
                    let v = serde_json::json!({"stews": stews, "creations": creations});
                    println!("{}", serde_json::to_string_pretty(&v).expect("serializes"));
                } else {
                    print!("{}", live_table(&stews, &creations));
                }
                return Ok(0);
            }
            let Reply::Tracked { stews, warnings } = call(&paths, Call::LsAll { search, closed })?
            else {
                return Err(Error::Fail("unexpected reply from the daemon".into()));
            };
            for w in &warnings {
                eprintln!("stew: warning: {w}");
            }
            if waiting {
                let lines: Vec<(&str, &str)> = stews
                    .iter()
                    .flat_map(|t| t.waiting().map(|l| (t.meta.id.as_str(), l)))
                    .collect();
                if json {
                    let v: Vec<_> = lines
                        .iter()
                        .map(|(id, line)| serde_json::json!({"id": id, "line": line}))
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&v).expect("serializes"));
                } else {
                    let w = lines.iter().map(|(id, _)| id.len()).max().unwrap_or(0);
                    for (id, line) in lines {
                        println!("{id:<w$}  {line}");
                    }
                }
            } else if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&stews).expect("serializes")
                );
            } else {
                print!("{}", table(&stews));
            }
            Ok(0)
        }
    }
}

fn warn(warnings: &[String]) {
    for w in warnings {
        eprintln!("stew: warning: {w}");
    }
}

fn created(reply: Reply) -> Result<u8> {
    match reply {
        Reply::Created { id, warnings } => {
            warn(&warnings);
            println!("{id}");
            Ok(0)
        }
        _ => Err(Error::Fail("unexpected reply from the daemon".into())),
    }
}

fn done(reply: Reply) -> Result<u8> {
    match reply {
        Reply::Done { warnings } => {
            warn(&warnings);
            Ok(0)
        }
        _ => Err(Error::Fail("unexpected reply from the daemon".into())),
    }
}

/// Asks the daemon where the command runs, then runs it as this process (its exit code
/// and signals are the command's).
fn launch(paths: &Paths, call_: Call) -> Result<u8> {
    let Reply::Launch { argv, cwd, env } = call(paths, call_)? else {
        return Err(Error::Fail("unexpected reply from the daemon".into()));
    };
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| Error::Fail("the daemon gave an empty command".into()))?;
    let mut cmd = Command::new(prog);
    cmd.args(args).envs(env);
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    use std::os::unix::process::CommandExt;
    let e = cmd.exec();
    Err(Error::Fail(format!("cannot run {prog}: {e}")))
}

/// The repo the CLI runs in, which implies its branch as that repo's base.
fn here() -> Option<Here> {
    let cwd = std::env::current_dir().ok()?;
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(Path::new(&cwd))
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    Some(Here {
        remote: git(&["remote", "get-url", "origin"])?,
        branch: git(&["symbolic-ref", "-q", "--short", "HEAD"])?,
    })
}

/// Per stew: id, title, site, state, age; then from, repos and Now lines indented.
fn live_table(stews: &[Live], creations: &[CreationInfo]) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let w0 = stews.iter().map(|t| t.id.len()).max().unwrap_or(0);
    let w1 = stews
        .iter()
        .map(|t| t.title.chars().count())
        .max()
        .unwrap_or(0);
    let w2 = stews.iter().map(|t| t.site.len()).max().unwrap_or(0);
    let mut out = String::new();
    for t in stews {
        let age = if t.updated > 0 {
            age(now - t.updated)
        } else {
            "?".into()
        };
        let head = format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<11}  {:>4}",
            t.id, t.title, t.site, t.state, age
        );
        out.push_str(head.trim_end());
        out.push('\n');
        if let Some(f) = &t.from {
            out.push_str(&format!("    from {f}\n"));
        }
        if !t.repos.is_empty() {
            let repos: Vec<String> = t
                .repos
                .iter()
                .map(|r| {
                    if r.branch.is_empty() {
                        format!("{} (detached)", r.dir)
                    } else {
                        format!("{}@{}", r.dir, r.branch)
                    }
                })
                .collect();
            out.push_str(&format!("    {}\n", repos.join("  ")));
        }
        for line in &t.now {
            out.push_str(&format!("    - {line}\n"));
        }
    }
    for c in creations {
        let what = match &c.error {
            Some(e) => format!("creation {}: {e}", c.state),
            None => format!("creation {}", c.state),
        };
        out.push_str(&format!("{}  {what}\n", c.id));
    }
    out
}

/// One request to the daemon; its error replies become errors.
fn call(paths: &Paths, call: Call) -> Result<Reply> {
    let socket = paths.socket();
    let reply = stew_proto::call(&socket, &Request::new(call)).map_err(|e| {
        Error::Fail(format!(
            "cannot reach the stew daemon at {}: {e}",
            socket.display()
        ))
    })?;
    match reply {
        Reply::Error { kind, message } => Err(Error::from_reply(kind, message)),
        r => Ok(r),
    }
}

/// Per stew: id, title, age (and `closed`), then its description and Now lines indented.
fn table(stews: &[Tracked]) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let w0 = stews.iter().map(|t| t.meta.id.len()).max().unwrap_or(0);
    let w1 = stews
        .iter()
        .map(|t| t.meta.title.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for t in stews {
        let mut head = format!(
            "{:<w0$}  {:<w1$}  {:>4}",
            t.meta.id,
            t.meta.title,
            age(now - t.updated)
        );
        if t.meta.is_closed() {
            head.push_str("  closed");
        }
        out.push_str(head.trim_end());
        out.push('\n');
        if !t.description.is_empty() {
            out.push_str(&format!("    {}\n", t.description));
        }
        for line in &t.now {
            out.push_str(&format!("    - {line}\n"));
        }
    }
    out
}

fn age(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}
