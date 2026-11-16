use std::path::Path;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use stew::daemon::{self, Hub};
use stew::error::{Error, Result};
use stew::paths::Paths;
use stew_proto::show::{printable, table};
use stew_proto::{Call, Here, Reply, Request};

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
        /// A file (`-`: stdin) that becomes its notes/BRIEF.md
        #[arg(long)]
        brief: Option<String>,
        /// Start an agent on the brief
        #[arg(long, requires = "brief")]
        start: bool,
        /// The agent for --start (claude, opencode; default from config.yaml)
        #[arg(long, requires = "start")]
        agent: Option<String>,
        /// Its model, passed through as --model
        #[arg(long, requires = "start")]
        model: Option<String>,
        /// claude's account (to spread usage; default from config.yaml, else the box's)
        #[arg(long, requires = "start")]
        account: Option<String>,
    },
    /// Start an agent in a stew on a task; prints its address
    Agent {
        id: String,
        /// claude, opencode (default from config.yaml)
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// claude's account (to spread usage; default from config.yaml, else the box's)
        #[arg(long)]
        account: Option<String>,
        task: String,
    },
    /// An agent's live terminal (`<id>/<name>`, or a name only one stew has); ctrl+\ leaves it
    Attach { agent: String },
    /// Assemble the skills dir boxes see (~/.local/share/stew/skills)
    Skills,
    /// Print the stew skill
    Skill,
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
    /// Export running stews' agent sessions to the transcripts repo (commits daily)
    Export {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        id: Option<String>,
        /// Every running stew (the hourly timer)
        #[arg(long)]
        all: bool,
        /// Commit and push now
        #[arg(long)]
        push: bool,
    },
    /// Snapshot a stew, export its sessions, warn about unpushed work, stop it
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
    /// List the open stews, wherever they are (running, stopped, removed), most recent first
    Ls {
        /// With each stew's README
        #[arg(short = 'l', long)]
        long: bool,
        /// The closed ones too
        #[arg(long)]
        closed: bool,
        /// Open and closed stews whose id, title or notes contain this
        search: Option<String>,
        /// Tracked stews started from this one
        #[arg(long, conflicts_with_all = ["closed", "search"])]
        from: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// The hub's daemon (systemd starts it on demand; in tests, in the foreground)
    Daemon {
        /// Listen here instead of $XDG_RUNTIME_DIR/stew/stew.sock (or systemd's socket); the
        /// stews' socket is `box.sock` next to it
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        /// Exit after this many seconds without a request (none in progress)
        #[arg(long)]
        idle_exit: Option<u64>,
        /// Fetch the artifacts mirror (what `ls` reads) every this many seconds
        #[arg(long, default_value_t = 60)]
        fetch_every: u64,
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
        Cmd::Daemon {
            socket,
            idle_exit,
            fetch_every,
        } => {
            let idle = idle_exit.map(Duration::from_secs);
            let (listener, stews, socket) = match (socket, daemon::activated()) {
                (None, Some((l, stews))) => (l, stews, paths.socket()),
                (socket, _) => {
                    let socket = socket.unwrap_or_else(|| paths.socket());
                    (daemon::bind(&socket)?, None, socket)
                }
            };
            // the stews' socket sits next to the CLI's
            let box_socket = socket.with_file_name("box.sock");
            let stews = match stews {
                Some(l) => l,
                None => daemon::bind(&box_socket)?,
            };
            let hub = Arc::new(Hub::open(paths, socket, box_socket)?);
            daemon::serve(
                hub,
                listener,
                stews,
                idle,
                Duration::from_secs(fetch_every.max(1)),
            )?;
            Ok(0)
        }
        Cmd::New {
            repos,
            title,
            site,
            branch,
            base,
            brief,
            start,
            agent,
            model,
            account,
        } => {
            let here = if base.is_none() { here() } else { None };
            let brief = brief.map(|f| read_brief(&f)).transpose()?;
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
                    brief,
                    start,
                    agent,
                    model,
                    account,
                },
            )?;
            created(reply)
        }
        Cmd::Agent {
            id,
            agent,
            model,
            account,
            task,
        } => match call(
            &paths,
            Call::Agent {
                id,
                agent,
                model,
                account,
                task,
            },
        )? {
            Reply::Started { address, warnings } => {
                warn(&warnings);
                println!("{address}");
                Ok(0)
            }
            _ => Err(Error::Fail("unexpected reply from the daemon".into())),
        },
        Cmd::Attach { agent } => launch(&paths, Call::Attach { agent }),
        Cmd::Skills => done(call(&paths, Call::Skills)?),
        Cmd::Skill => {
            print!("{}", stew_proto::SKILL);
            Ok(0)
        }
        Cmd::Open { id, site } => created(call(&paths, Call::Open { id, site })?),
        Cmd::Remove { id, force } => done(call(&paths, Call::Remove { id, force })?),
        Cmd::Close { id, force } => done(call(&paths, Call::Close { id, force })?),
        Cmd::Start { id, .. } => done(call(&paths, Call::Start { id })?),
        Cmd::Export { id, push, .. } => match call(&paths, Call::Export { id, push })? {
            Reply::Exported { lines, warnings } => {
                warn(&warnings);
                for l in lines {
                    println!("{l}");
                }
                Ok(0)
            }
            _ => Err(Error::Fail("unexpected reply from the daemon".into())),
        },
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
            from,
            long,
            closed,
            search,
            json,
        } => {
            let from_one = from.is_some();
            let req = match from {
                Some(id) => Call::LsFrom { id },
                None => Call::Ls { search, closed },
            };
            let Reply::Listed {
                stews,
                creations,
                warnings,
            } = call(&paths, req)?
            else {
                return Err(Error::Fail("unexpected reply from the daemon".into()));
            };
            warn(&warnings);
            if json && from_one {
                // as in a stew's `ls --from .`: the stews alone
                let v = serde_json::json!(stews);
                println!("{}", serde_json::to_string_pretty(&v).expect("serializes"));
            } else if json {
                let v = serde_json::json!({"stews": stews, "creations": creations});
                println!("{}", serde_json::to_string_pretty(&v).expect("serializes"));
            } else {
                print!("{}", table(&stews, &creations, long));
            }
            Ok(0)
        }
    }
}

fn warn(warnings: &[String]) {
    for w in warnings {
        eprintln!("stew: warning: {}", printable(w));
    }
}

fn created(reply: Reply) -> Result<u8> {
    match reply {
        Reply::Created {
            id,
            agent,
            warnings,
        } => {
            warn(&warnings);
            if let Some(a) = agent {
                eprintln!("stew: agent {a}");
            }
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
    let Reply::Launch {
        argv,
        cwd,
        env,
        path,
    } = call(paths, call_)?
    else {
        return Err(Error::Fail("unexpected reply from the daemon".into()));
    };
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| Error::Fail("the daemon gave an empty command".into()))?;
    let mut cmd = Command::new(prog);
    cmd.args(args).envs(env);
    if !path.is_empty() {
        let own = std::env::var_os("PATH").unwrap_or_default();
        let dirs = path
            .iter()
            .map(std::path::PathBuf::from)
            .chain(std::env::split_paths(&own));
        if let Ok(p) = std::env::join_paths(dirs) {
            cmd.env("PATH", p);
        }
    }
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

/// A brief from a file, or stdin for `-`.
fn read_brief(file: &str) -> Result<String> {
    use std::io::Read;
    let mut text = String::new();
    let res = if file == "-" {
        std::io::stdin()
            .take(stew_proto::MAX_BRIEF as u64 + 1)
            .read_to_string(&mut text)
    } else {
        std::fs::File::open(file).and_then(|f| {
            f.take(stew_proto::MAX_BRIEF as u64 + 1)
                .read_to_string(&mut text)
        })
    };
    res.map_err(|e| Error::Fail(format!("cannot read the brief {file}: {e}")))?;
    if text.len() > stew_proto::MAX_BRIEF {
        return Err(Error::Fail(format!(
            "the brief is longer than {} bytes",
            stew_proto::MAX_BRIEF
        )));
    }
    Ok(text)
}

/// One request to the daemon; its error replies become errors.
fn call(paths: &Paths, call: Call) -> Result<Reply> {
    let socket = paths.socket();
    let req = Request {
        progress: true,
        ..Request::new(call)
    };
    let reply = stew_proto::call_with(&socket, &req, |l| eprintln!("stew: {l}")).map_err(|e| {
        match e.kind() {
            std::io::ErrorKind::InvalidInput => Error::Fail(e.to_string()),
            _ => Error::Fail(format!(
                "cannot reach the stew daemon at {}: {e}",
                socket.display()
            )),
        }
    })?;
    match reply {
        Reply::Error { kind, message } => Err(Error::from_reply(kind, message)),
        r => Ok(r),
    }
}
