use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use stew::daemon::{self, Hub};
use stew::error::{Error, Result};
use stew::paths::Paths;
use stew_proto::{Call, Reply, Request, Tracked};

/// Stews: workspaces of repo clones in a box.
#[derive(Parser)]
#[command(name = "stew", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List stews, most recent first
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
        #[arg(long)]
        json: bool,
    },
    /// The hub's daemon (systemd starts it on demand; in tests, in the foreground)
    Daemon {
        /// Listen here instead of $XDG_RUNTIME_DIR/stew/stew.sock
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("stew: {e}");
            ExitCode::from(e.exit_code() as u8)
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let paths = Paths::from_env()?;
    match cli.cmd {
        Cmd::Daemon { socket } => {
            let socket = socket.unwrap_or_else(|| paths.socket());
            let hub = Arc::new(Hub::open(paths)?);
            let listener = daemon::bind(&socket)?;
            daemon::serve(hub, listener, socket)
        }
        Cmd::Ls {
            all,
            closed,
            waiting,
            search,
            json,
        } => {
            if !(all || closed || waiting) {
                return Err(Error::Fail(
                    "`stew ls` without --all is not built yet (B2); use --all".into(),
                ));
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
            Ok(())
        }
    }
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
