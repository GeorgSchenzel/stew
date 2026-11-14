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
        /// Every tracked stew, from the artifacts repo
        #[arg(long)]
        all: bool,
        /// With --all: only stews whose id, title or notes contain this
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
        Cmd::Ls { all, search, json } => {
            if !all {
                return Err(Error::Fail(
                    "`stew ls` without --all is not built yet (B2); use --all".into(),
                ));
            }
            match call(&paths, Call::LsAll { search })? {
                Reply::Tracked { stews, warnings } => {
                    for w in &warnings {
                        eprintln!("stew: warning: {w}");
                    }
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&stews).expect("serializes")
                        );
                    } else {
                        print!("{}", table(&stews));
                    }
                    Ok(())
                }
                other => Err(unexpected(other)),
            }
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

fn unexpected(r: Reply) -> Error {
    Error::Fail(format!("unexpected reply from the daemon: {r:?}"))
}

fn table(stews: &[Tracked]) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let rows: Vec<[String; 4]> = stews
        .iter()
        .map(|t| {
            [
                t.meta.id.clone(),
                t.meta.title.clone(),
                age(now - t.updated),
                t.status.clone(),
            ]
        })
        .collect();
    let mut widths = [0; 3];
    for r in &rows {
        for (w, cell) in widths.iter_mut().zip(r) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for [id, title, age, status] in &rows {
        out.push_str(&format!(
            "{id:<w0$}  {title:<w1$}  {age:>w2$}  {status}\n",
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2]
        ));
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
