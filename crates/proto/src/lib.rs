//! The protocol between stew clients (the host CLI, the in-box client) and the hub daemon.
//!
//! JSON lines over a unix socket: the client sends one `Request` line and reads `Reply`
//! lines until it has its answer (with `progress`, `Reply::Progress` lines come first),
//! then closes. Nobody half-closes (a guest `SHUT_WR` can
//! truncate buffered data on a channel), so a message ends at its newline, never at EOF.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub mod show;

/// The `stew` skill (what exists, for agents in a stew); ships with stew.
pub const SKILL: &str = include_str!("../../../skills/stew/SKILL.md");

/// Bumped when a request or reply changes incompatibly.
pub const VERSION: u32 = 4;

/// The longest request line the daemon reads (a brief travels inside one).
pub const MAX_REQUEST: u64 = 4 << 20;

/// The longest brief `new` takes.
pub const MAX_BRIEF: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// The caller's stew, sent by the in-box client (found from `notes/stew.yaml`). A request
    /// that names one is taken as coming from that stew only with its token, and only for
    /// the calls a stew's client makes (`Call::from_stew`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stew: Option<String>,
    /// The stew's token (`STEW_TOKEN`), issued by the daemon at `new`/`open`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The client shows what the daemon is doing: `Reply::Progress` lines before the answer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub progress: bool,
    #[serde(flatten)]
    pub call: Call,
}

impl Request {
    pub fn new(call: Call) -> Self {
        Request {
            stew: None,
            token: None,
            progress: false,
            call,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub enum Call {
    /// The daemon's protocol version.
    Hello,
    /// The catalog.
    Repos,
    /// Resolves repo names and the base rules against the remotes, creating nothing.
    Resolve {
        repos: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        here: Option<Here>,
    },
    /// A new stew: resolve, claim, create on the site, clone, start.
    New {
        repos: Vec<String>,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<String>,
        /// `--branch`: `Some("")` for the bare flag (the title's slug).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        here: Option<Here>,
        /// Ignored from a stew's client: the daemon sets it to the caller.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        /// `notes/BRIEF.md`, in the claim commit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        brief: Option<String>,
        /// An agent starts on the brief once the stew exists.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        start: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// claude's account (`CLAUDE_ACCOUNT_<NAME>_TOKEN` where the stew lives).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    /// A tracked stew recreated on a site; a closed one is reopened.
    Open {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<String>,
    },
    /// Off its site, after the pushed check (`force` skips its refusal).
    Remove {
        id: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        force: bool,
    },
    /// `Remove` plus `state: closed` and a LOG.md line.
    Close {
        id: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        force: bool,
    },
    /// The driver's start: one stew, or every stew on a site with `id: None`.
    Start {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// The snapshot, a warning about unpushed work, the driver's stop.
    Stop { id: String },
    /// A new title, written to `stew.yaml` where the stew lives.
    Title { id: String, title: String },
    /// Every open tracked stew, wherever it is, never looking into one: the daemon's
    /// state, the drivers' state, the runtime's agents and the artifacts mirror as last
    /// fetched. With `closed` the closed ones too; `search` keeps those whose id, title or
    /// notes contain it.
    Ls {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        search: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        closed: bool,
    },
    /// Where a command runs for `exec` and `enter` (the client runs it itself).
    Where {
        id: String,
        /// Empty: an interactive shell (`enter`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        no_start: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        tty: bool,
    },
    /// The caller's own stew (a stew's client only).
    Info,
    /// Starts an agent in a stew on `task`. From a stew's client: only in a stew it started.
    Agent {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// claude's account (`CLAUDE_ACCOUNT_<NAME>_TOKEN` where the stew lives).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        task: String,
    },
    /// Where an agent's live terminal is attached from (the client runs it).
    Attach { agent: String },
    /// The stews started from `id` (`.`: the caller's own), live where they exist.
    LsFrom { id: String },
    /// Assembles the skills dir boxes see (`~/.local/share/stew/skills/`).
    Skills,
    /// Exports the sessions of a running stew (`id: None`: every running stew) to the
    /// transcripts repo; commits daily, or at once with `push`.
    Export {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        push: bool,
    },
}

impl Call {
    /// The calls a stew's client makes. Anything else that names a stew is refused, so an
    /// agent's mistake can't `remove` or `exec` into another stew through its client. (Not
    /// a security boundary: a request that names no stew is the host CLI's.)
    pub fn from_stew(&self) -> bool {
        matches!(
            self,
            Call::Hello
                | Call::Repos
                | Call::Info
                | Call::New { .. }
                | Call::Agent { .. }
                | Call::LsFrom { .. }
        )
    }
}

/// The repo a client runs in, which implies its current branch as that repo's base.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Here {
    pub remote: String,
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Hello {
        version: u32,
    },
    Repos {
        repos: Vec<Repo>,
    },
    Resolved {
        repos: Vec<Resolved>,
        warnings: Vec<String>,
    },
    /// `new` and `open`: the stew exists (with `--start`, its agent runs at `agent`).
    Created {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        warnings: Vec<String>,
    },
    /// `agent`: it runs, at this address (`<id>/<name>`).
    Started {
        address: String,
        warnings: Vec<String>,
    },
    /// The caller's stew.
    Info {
        stew: Listed,
        /// Its clones and their branches, live.
        repos: Vec<LiveRepo>,
        /// The site's driver (`bento`, `host`).
        driver: String,
        /// Where its root is on its site.
        root: String,
    },
    /// `export`: a line per stew (sessions, files written, or why it was skipped).
    Exported {
        lines: Vec<String>,
        warnings: Vec<String>,
    },
    /// A command that has nothing else to say.
    Done {
        warnings: Vec<String>,
    },
    Listed {
        stews: Vec<Listed>,
        /// Creations in progress or failed.
        creations: Vec<CreationInfo>,
        warnings: Vec<String>,
    },
    /// Run this (for `exec`/`enter`/`attach`): argv, in `cwd` when set, with `env` added
    /// and `path` put before the client's own `PATH`.
    Launch {
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env: Vec<(String, String)>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        path: Vec<String>,
    },
    Error {
        kind: ErrorKind,
        message: String,
    },
    /// A step the daemon is on, before the answer (only when the request asked for them).
    Progress {
        line: String,
    },
}

/// A stew as `stew ls` shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Listed {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Its site, while it is on one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// On a site: `running`, `stopped`, `missing`, `unreachable`, or the driver's own
    /// word. On none: `removed`, or `closed`.
    pub state: String,
    /// With `state: closed`: when, `YYYY-MM-DD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<String>,
    /// Unix time of the last commit on its notes branch (0: unknown).
    pub updated: i64,
    /// Its `notes/README.md` as last fetched, unparsed.
    #[serde(default)]
    pub readme: String,
    /// Its agents in the runtime, while it runs.
    #[serde(default)]
    pub agents: Vec<AgentInfo>,
}

/// An agent (a live terminal of the runtime) in a stew.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    /// `<id>/<name>`, or `<id>/t<n>` for a terminal without a name.
    pub address: String,
    /// The program, as the runtime sees it (`claude`, `opencode`, `sh`).
    pub program: String,
    /// Its own word for what it does (OSC 7501: `working`, `idle`, `blocked`, …), else
    /// `running`.
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveRepo {
    pub dir: String,
    /// Empty when detached or unknown.
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreationInfo {
    pub id: String,
    /// `running` or `failed`.
    pub state: String,
    pub started: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Something failed (exit 1).
    Fail,
    /// A safety check said no (exit 2).
    Refused,
}

/// A catalog entry: its name (the path through the groups) and its https remote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Repo {
    pub name: String,
    pub remote: String,
}

/// A repo of a stew-to-be, with the branch its clone starts on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolved {
    /// The catalog name, as recorded in `stew.yaml`.
    pub name: String,
    /// The clone's dir in the stew.
    pub dir: String,
    pub remote: String,
    pub branch: String,
}

/// `notes/stew.yaml`: a stew's tracking record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StewMeta {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// `YYYY-MM-DD`.
    pub created: String,
    /// Absent means open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Openness>,
    /// With `state: closed`: when, `YYYY-MM-DD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<String>,
    #[serde(default)]
    pub repos: Vec<RepoRecord>,
}

impl StewMeta {
    pub fn is_closed(&self) -> bool {
        self.state == Some(Openness::Closed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Openness {
    Open,
    Closed,
}

/// A repo in the snapshot: catalog name, normalized remote, branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoRecord {
    pub name: String,
    pub remote: String,
    pub branch: String,
}

/// A tracked stew in the artifacts mirror: `stew.yaml` and its `README.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tracked {
    #[serde(flatten)]
    pub meta: StewMeta,
    /// Unix time of the last commit on its notes branch (for recency).
    pub updated: i64,
    pub readme: String,
}

/// Writes one message as a line.
pub fn send<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

/// Reads one message; `None` at EOF before any byte of it.
pub fn recv<T: DeserializeOwned>(r: &mut impl BufRead) -> io::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed mid-message",
        ));
    }
    Ok(Some(serde_json::from_str(&line)?))
}

/// `STEW_HUB` as a socket path: `unix:<path>` (the only form so far).
pub fn hub_socket(hub: &str) -> Result<std::path::PathBuf, String> {
    match hub.strip_prefix("unix:") {
        Some(p) if p.starts_with('/') => Ok(p.into()),
        _ => Err(format!(
            "STEW_HUB '{hub}' is not unix:<absolute socket path>"
        )),
    }
}

/// Sends `req` to the daemon at `socket` and returns its reply.
pub fn call(socket: &Path, req: &Request) -> io::Result<Reply> {
    call_with(socket, req, |_| {})
}

/// `call`, handing each `Reply::Progress` line to `progress` as it comes.
pub fn call_with(
    socket: &Path,
    req: &Request,
    mut progress: impl FnMut(&str),
) -> io::Result<Reply> {
    // the daemon reads at most MAX_REQUEST: say so here instead of a cut-off line there
    // (JSON escapes control characters, so a brief under MAX_BRIEF can still be too long)
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    if line.len() as u64 > MAX_REQUEST {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the request is longer than {MAX_REQUEST} bytes (as JSON)"),
        ));
    }
    let stream = UnixStream::connect(socket)?;
    (&stream).write_all(&line)?;
    let mut r = BufReader::new(&stream);
    loop {
        match recv(&mut r)? {
            Some(Reply::Progress { line }) => progress(&line),
            Some(reply) => return Ok(reply),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the daemon closed the connection without a reply",
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_lines() {
        let req = Request::new(Call::Ls {
            search: Some("x".into()),
            closed: false,
        });
        let mut buf = Vec::new();
        send(&mut buf, &req).unwrap();
        assert_eq!(buf, b"{\"call\":\"ls\",\"search\":\"x\"}\n");
        let back: Request = recv(&mut &buf[..]).unwrap().unwrap();
        assert_eq!(back, req);

        let from_box: Request =
            serde_json::from_str(r#"{"stew":"a-k3fq","token":"t","call":"hello"}"#).unwrap();
        assert_eq!(from_box.stew.as_deref(), Some("a-k3fq"));
        assert_eq!(from_box.call, Call::Hello);
    }

    #[test]
    fn partial_line_is_an_error() {
        let r: io::Result<Option<Request>> = recv(&mut &b"{\"call\":\"hello\"}"[..]);
        assert!(r.is_err());
        let r: io::Result<Option<Request>> = recv(&mut &b""[..]);
        assert!(r.unwrap().is_none());
    }

    #[test]
    fn tracked_flattens_meta() {
        let t = Tracked {
            meta: StewMeta {
                id: "a-k3fq".into(),
                title: "A".into(),
                from: None,
                created: "2026-11-14".into(),
                state: None,
                closed: None,
                repos: vec![],
            },
            updated: 1,
            readme: "# A\n".into(),
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["id"], "a-k3fq");
        assert_eq!(v["readme"], "# A\n");
        assert!(v.get("from").is_none() && v.get("state").is_none());
        assert!(!t.meta.is_closed());
    }
}
