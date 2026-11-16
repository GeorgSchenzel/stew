//! The stew client inside a stew (installed there as `stew`). It talks to the hub named in
//! `STEW_HUB` (bento: `unix:/run/bento/stew.sock`, the channel; host: the hub's own
//! socket), finds its stew by walking up to `notes/stew.yaml`, and sends that id with the
//! stew's token from `STEW_TOKEN`.

use std::env;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

use clap::{Parser, Subcommand};
use stew_proto::show::{printable, table};
use stew_proto::{Call, ErrorKind, Here, Reply, Request, StewMeta};

/// Stews: workspaces of repo clones in a box (the client inside a stew).
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
    /// This stew: id, title, from, site and driver, repos and branches, agents
    Info,
    /// A new stew started from this one; prints its id
    New {
        /// Catalog names or remote URLs
        repos: Vec<String>,
        #[arg(long)]
        title: String,
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
        /// The agent for --start (claude, opencode)
        #[arg(long, requires = "start")]
        agent: Option<String>,
        /// Its model, passed through as --model
        #[arg(long, requires = "start")]
        model: Option<String>,
        /// claude's account (to spread usage; default from config.yaml, else the box's)
        #[arg(long, requires = "start")]
        account: Option<String>,
    },
    /// Start an agent in a stew you started, on a task; prints its address
    Agent {
        id: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// claude's account (to spread usage; default: the hub's config, else the box's)
        #[arg(long)]
        account: Option<String>,
        task: String,
    },
    /// Stews started from a stew (`.`: this one), with agents and Now lines
    Ls {
        #[arg(long, required = true)]
        from: String,
        #[arg(long)]
        json: bool,
    },
    /// Print the stew skill
    Skill,
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

fn unexpected(r: Reply) -> Failure {
    (1, format!("unexpected reply: {r:?}"))
}

fn warn(warnings: &[String]) {
    for w in warnings {
        eprintln!("stew: warning: {}", printable(w));
    }
}

fn run(cli: Cli) -> Result<(), Failure> {
    match cli.cmd {
        Cmd::Skill => {
            print!("{}", stew_proto::SKILL);
            Ok(())
        }
        Cmd::Repos => match call(Call::Repos)? {
            Reply::Repos { repos } => {
                let width = repos.iter().map(|r| r.name.len()).max().unwrap_or(0);
                for r in repos {
                    println!("{:<width$}  {}", r.name, r.remote);
                }
                Ok(())
            }
            other => Err(unexpected(other)),
        },
        Cmd::Info => match call(Call::Info)? {
            Reply::Info {
                stew,
                repos,
                driver,
                root,
            } => {
                println!("{}  {}", stew.id, stew.title);
                if let Some(f) = &stew.from {
                    println!("from    {f}");
                }
                let site = stew.site.as_deref().unwrap_or("-");
                println!("site    {site} ({driver}), {}", stew.state);
                println!("root    {root}");
                for r in &repos {
                    let b = if r.branch.is_empty() {
                        "(detached)"
                    } else {
                        &r.branch
                    };
                    println!("repo    {}@{b}", r.dir);
                }
                for a in &stew.agents {
                    println!("agent   {}  {}  {}", a.address, a.program, a.status);
                }
                Ok(())
            }
            other => Err(unexpected(other)),
        },
        Cmd::New {
            repos,
            title,
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
            match call(Call::New {
                repos,
                title,
                site: None,
                branch,
                base,
                here,
                from: None,
                brief,
                start,
                agent,
                model,
                account,
            })? {
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
                    Ok(())
                }
                other => Err(unexpected(other)),
            }
        }
        Cmd::Agent {
            id,
            agent,
            model,
            account,
            task,
        } => match call(Call::Agent {
            id,
            agent,
            model,
            account,
            task,
        })? {
            Reply::Started { address, warnings } => {
                warn(&warnings);
                println!("{address}");
                Ok(())
            }
            other => Err(unexpected(other)),
        },
        Cmd::Ls { from, json } => match call(Call::LsFrom { id: from })? {
            Reply::Listed {
                stews,
                creations,
                warnings,
            } => {
                warn(&warnings);
                if json {
                    let text =
                        serde_json::to_string_pretty(&stews).map_err(|e| (1, e.to_string()))?;
                    println!("{text}");
                } else {
                    print!("{}", table(&stews, &creations, false));
                }
                Ok(())
            }
            other => Err(unexpected(other)),
        },
    }
}

fn call(call: Call) -> Result<Reply, Failure> {
    let hub = env::var("STEW_HUB")
        .map_err(|_| (1, "STEW_HUB is not set: this is not a stew".to_string()))?;
    let socket = stew_proto::hub_socket(&hub).map_err(|e| (1, e))?;
    let cwd = env::current_dir().map_err(|e| (1, e.to_string()))?;
    let stew = find_stew(&cwd).ok_or_else(|| {
        (
            1,
            format!(
                "not in a stew: no notes/stew.yaml in {} or above",
                cwd.display()
            ),
        )
    })?;
    let req = Request {
        stew: Some(stew.id),
        token: env::var("STEW_TOKEN").ok().filter(|t| !t.is_empty()),
        progress: true,
        call,
    };
    let reply = stew_proto::call_with(&socket, &req, |l| eprintln!("stew: {l}")).map_err(|e| {
        match e.kind() {
            std::io::ErrorKind::InvalidInput => (1, e.to_string()),
            _ => (
                1,
                format!("cannot reach the stew hub at {}: {e}", socket.display()),
            ),
        }
    })?;
    match reply {
        Reply::Error { kind, message } => {
            Err((if kind == ErrorKind::Refused { 2 } else { 1 }, message))
        }
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

fn read_brief(file: &str) -> Result<String, Failure> {
    let limit = stew_proto::MAX_BRIEF as u64 + 1;
    let mut text = String::new();
    let res = if file == "-" {
        std::io::stdin().take(limit).read_to_string(&mut text)
    } else {
        fs::File::open(file).and_then(|f| f.take(limit).read_to_string(&mut text))
    };
    res.map_err(|e| (1, format!("cannot read the brief {file}: {e}")))?;
    if text.len() > stew_proto::MAX_BRIEF {
        return Err((
            1,
            format!("the brief is longer than {} bytes", stew_proto::MAX_BRIEF),
        ));
    }
    Ok(text)
}

/// The repo the client runs in, which implies its branch as that repo's base.
fn here() -> Option<Here> {
    let cwd = env::current_dir().ok()?;
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&cwd)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stderr(Stdio::null())
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
