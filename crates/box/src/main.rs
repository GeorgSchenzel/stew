//! The stew client inside a box. It finds its stew by walking up to `notes/stew.yaml` and
//! talks to the hub over bento's `stew` channel (`/run/bento/stew.sock`), sending its id
//! and the box's token (`/run/bento/secrets/stew-token`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use stew_proto::{Call, Reply, Request, StewMeta};

const SOCKET: &str = "/run/bento/stew.sock";
const TOKEN: &str = "/run/bento/secrets/stew-token";

/// Stews: workspaces of repo clones in a box (the in-box client).
#[derive(Parser)]
#[command(name = "stew", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// The catalog: names and remote URLs
    Repos,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, msg)) => {
            eprintln!("stew: {msg}");
            ExitCode::from(code)
        }
    }
}

type Failure = (u8, String);

fn run(cli: Cli) -> Result<(), Failure> {
    match cli.cmd {
        Cmd::Repos => match call(Call::Repos)? {
            Reply::Repos { repos } => {
                let width = repos.iter().map(|r| r.name.len()).max().unwrap_or(0);
                for r in repos {
                    println!("{:<width$}  {}", r.name, r.remote);
                }
                Ok(())
            }
            other => Err((1, format!("unexpected reply: {other:?}"))),
        },
    }
}

fn call(call: Call) -> Result<Reply, Failure> {
    let socket = env::var_os("STEW_SOCKET").map_or_else(|| PathBuf::from(SOCKET), PathBuf::from);
    let cwd = env::current_dir().map_err(|e| (1, e.to_string()))?;
    let req = Request {
        stew: find_stew(&cwd).map(|m| m.id),
        token: fs::read_to_string(TOKEN).ok().map(|t| t.trim().to_string()),
        call,
    };
    let reply = stew_proto::call(&socket, &req).map_err(|e| {
        (
            1,
            format!("cannot reach the stew hub at {}: {e}", socket.display()),
        )
    })?;
    match reply {
        Reply::Error { kind, message } => Err((
            if kind == stew_proto::ErrorKind::Refused {
                2
            } else {
                1
            },
            message,
        )),
        r => Ok(r),
    }
}

/// The stew `dir` is in: the nearest `notes/stew.yaml` walking up.
fn find_stew(dir: &Path) -> Option<StewMeta> {
    dir.ancestors().find_map(|d| {
        let text = fs::read_to_string(d.join("notes/stew.yaml")).ok()?;
        serde_yaml::from_str(&text).ok()
    })
}
