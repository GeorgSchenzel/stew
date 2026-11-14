//! The protocol between stew clients (the host CLI, the in-box client) and the hub daemon.
//!
//! JSON lines over a unix socket: the client sends one `Request` line and reads `Reply`
//! lines until it has its answer, then closes. Nobody half-closes (a guest `SHUT_WR` can
//! truncate buffered data on a channel), so a message ends at its newline, never at EOF.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Bumped when a request or reply changes incompatibly.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// The caller's stew, sent by the in-box client (found from `notes/stew.yaml`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stew: Option<String>,
    /// The box's token (the secret `stew-token`). Not checked yet (hardening).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(flatten)]
    pub call: Call,
}

impl Request {
    pub fn new(call: Call) -> Self {
        Request {
            stew: None,
            token: None,
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
    /// Every tracked stew, from the artifacts mirror.
    LsAll {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        search: Option<String>,
    },
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
    Tracked {
        stews: Vec<Tracked>,
        warnings: Vec<String>,
    },
    Error {
        kind: ErrorKind,
        message: String,
    },
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
    #[serde(default)]
    pub repos: Vec<RepoRecord>,
}

/// A repo in the snapshot: catalog name, normalized remote, branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoRecord {
    pub name: String,
    pub remote: String,
    pub branch: String,
}

/// A tracked stew as `ls --all` shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tracked {
    #[serde(flatten)]
    pub meta: StewMeta,
    /// Unix time of the last commit on its notes branch.
    pub updated: i64,
    /// The subject of that commit: the stew's status line.
    pub status: String,
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

/// Sends `req` to the daemon at `socket` and returns its reply.
pub fn call(socket: &Path, req: &Request) -> io::Result<Reply> {
    let stream = UnixStream::connect(socket)?;
    send(&mut &stream, req)?;
    recv(&mut BufReader::new(&stream))?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the daemon closed the connection without a reply",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_lines() {
        let req = Request::new(Call::LsAll {
            search: Some("x".into()),
        });
        let mut buf = Vec::new();
        send(&mut buf, &req).unwrap();
        assert_eq!(buf, b"{\"call\":\"ls_all\",\"search\":\"x\"}\n");
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
                repos: vec![],
            },
            updated: 1,
            status: "s".into(),
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["id"], "a-k3fq");
        assert_eq!(v["status"], "s");
        assert!(v.get("from").is_none());
    }
}
