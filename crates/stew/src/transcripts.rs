//! The transcripts repo: raw session exports, `<id>/<agent>-<session>.json[l]` plus a
//! `.meta.json` (agent, version, stew, dir, dates). Only the hub has a checkout
//! (`~/.local/share/stew/transcripts`); it is never cloned into a stew.
//!
//! Export is stew's code run where the stew lives, through the driver's `exec`
//! (`scripts/export.sh`), in two passes: `list` names the sessions under the root (claude's
//! jsonl files with their sha256, opencode's `session list`), the hub picks the ones that
//! changed since the last export (claude: the sha256; opencode: its `updated` time), and
//! `send` returns only those. Files are rewritten only when their bytes change. Commits come
//! daily (the hourly timer) or at once (`stop`, `remove`, `export --push`), so a growing
//! session is one new version a day.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::driver::{Driver, Status, Target};
use crate::error::{Result, fail};
use crate::git::{git, git_ok, git_with};
use crate::inspect::sh;

pub const EXPORT: &str = include_str!("scripts/export.sh");

/// How long listing a stew's sessions may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// How long sending them may take.
const SEND_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// A timer run commits when the last commit is at least this old.
const DAILY: i64 = 24 * 60 * 60;
/// The branch the hub writes.
const BRANCH: &str = "main";
const IDENTITY: (&str, &str) = ("stew", "stew@localhost");

/// A session's metadata, next to it as `<agent>-<session>.meta.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub stew: String,
    pub session: String,
    /// Where it ran, relative to the stew's root (`.`: the root).
    pub dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    /// claude: the jsonl's sha256 as listed (what tells a changed session).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// What one stew's export did.
#[derive(Debug, Default)]
pub struct Report {
    /// Sessions under its root.
    pub sessions: usize,
    /// Content or metadata files written (changed or new).
    pub written: usize,
    /// What could not be listed or sent: the export is incomplete.
    pub problems: Vec<String>,
}

pub enum Exported {
    /// `no_start` and the stew isn't running.
    NotRunning,
    Done(Report),
}

/// A record of the script's output: a line's tab-separated fields, with the bytes that
/// follow it for the blob tags (`O`, `F`).
#[derive(Debug, PartialEq)]
enum Rec {
    Line(Vec<String>),
    Blob(Vec<String>, Vec<u8>),
}

/// Parses the script's output. It must end with `DONE` and hold nothing after it.
fn records(out: &[u8]) -> Result<Vec<Rec>> {
    let mut recs = Vec::new();
    let mut at = 0;
    loop {
        let Some(len) = out[at..].iter().position(|&b| b == b'\n') else {
            return Err(fail!("the export's output ends early (no DONE)"));
        };
        let line = String::from_utf8_lossy(&out[at..at + len]).into_owned();
        at += len + 1;
        if line == "DONE" {
            if at != out.len() {
                return Err(fail!("unexpected output after the export's end"));
            }
            return Ok(recs);
        }
        let mut fields: Vec<String> = line.split('\t').map(String::from).collect();
        match fields[0].as_str() {
            "O" | "F" => {
                let n: usize = fields
                    .pop()
                    .and_then(|n| n.parse().ok())
                    .ok_or_else(|| fail!("bad export record: {line}"))?;
                if out.len() < at + n + 1 || out[at + n] != b'\n' {
                    return Err(fail!("an export record is cut short: {line}"));
                }
                recs.push(Rec::Blob(fields, out[at..at + n].to_vec()));
                at += n + 1;
            }
            "R" | "C" | "V" | "E" => recs.push(Rec::Line(fields)),
            _ => return Err(fail!("unexpected export output: {line}")),
        }
    }
}

/// A session id as it may name a file.
fn session_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `dir` relative to `root` when it is the root or below it.
fn under(root: &str, dir: &str) -> Option<String> {
    if dir == root {
        return Some(".".into());
    }
    let rest = dir.strip_prefix(root)?.strip_prefix('/')?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() {
        Some(".".into())
    } else {
        Some(rest.to_string())
    }
}

/// A unix time in milliseconds as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let day = secs.rem_euclid(86_400);
    format!(
        "{}T{:02}:{:02}:{:02}.{:03}Z",
        crate::id::date(secs),
        day / 3600,
        day % 3600 / 60,
        day % 60,
        ms.rem_euclid(1000)
    )
}

/// claude's jsonl: its version (the last one named) and first and last timestamps.
fn claude_facts(content: &[u8]) -> (Option<String>, Option<String>, Option<String>) {
    let (mut version, mut first, mut last) = (None, None, None);
    for line in content.split(|&b| b == b'\n') {
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if let Some(s) = v.get("version").and_then(Value::as_str) {
            version = Some(s.to_string());
        }
        if let Some(s) = v.get("timestamp").and_then(Value::as_str) {
            if first.is_none() {
                first = Some(s.to_string());
            }
            last = Some(s.to_string());
        }
    }
    (version, first, last)
}

/// A time from opencode's session list: milliseconds (a number), or a string as it is.
fn oc_time(v: &Value, key: &str) -> Option<String> {
    let t = v
        .get(key)
        .or_else(|| v.get("time").and_then(|t| t.get(key)))?;
    match t {
        Value::Number(n) => n.as_i64().map(iso),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// An opencode session found in a list.
struct OcSession {
    /// The dir its list ran in (what `send` runs export in).
    ran: String,
    meta: Meta,
}

/// A claude session found by `list`.
struct ClaudeSession {
    file: String,
    meta: Meta,
}

pub struct Transcripts {
    pub dir: PathBuf,
    pub remote: String,
}

impl Transcripts {
    pub fn new(dir: PathBuf, remote: String) -> Self {
        Transcripts { dir, remote }
    }

    /// The checkout, made on first use (fetching what the remote has, if it can be
    /// reached); pointed at the configured remote.
    pub fn ensure(&self, warnings: &mut Vec<String>) -> Result<()> {
        if !self.dir.join(".git").is_dir() {
            fs::create_dir_all(&self.dir)?;
            git(&self.dir, ["init", "--quiet"])?;
            git(
                &self.dir,
                ["symbolic-ref", "HEAD", &format!("refs/heads/{BRANCH}")],
            )?;
            git(&self.dir, ["remote", "add", "origin", &self.remote])?;
            match git(&self.dir, ["fetch", "--quiet", "origin"]) {
                Ok(_) => {
                    let theirs = format!("refs/remotes/origin/{BRANCH}");
                    if git_ok(&self.dir, ["rev-parse", "--verify", "--quiet", &theirs]) {
                        git(&self.dir, ["reset", "--quiet", "--hard", &theirs])?;
                    }
                }
                Err(e) => warnings.push(format!(
                    "cannot fetch the transcripts repo: {e} (pushed later)"
                )),
            }
        } else if git(&self.dir, ["remote", "get-url", "origin"])? != self.remote {
            git(&self.dir, ["remote", "set-url", "origin", &self.remote])?;
        }
        Ok(())
    }

    fn path(&self, id: &str, name: &str) -> PathBuf {
        self.dir.join(id).join(name)
    }

    fn meta(&self, id: &str, key: &str) -> Option<Meta> {
        let text = fs::read(self.path(id, &format!("{key}.meta.json"))).ok()?;
        serde_json::from_slice(&text).ok()
    }

    /// Writes `content` to `<id>/<name>` unless it is there already; true when written.
    fn put(&self, id: &str, name: &str, content: &[u8]) -> Result<bool> {
        let path = self.path(id, name);
        if fs::read(&path).is_ok_and(|old| old == content) {
            return Ok(false);
        }
        let dir = self.dir.join(id);
        fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!(".{name}.tmp"));
        fs::write(&tmp, content)?;
        fs::rename(&tmp, &path)?;
        Ok(true)
    }

    fn put_meta(&self, id: &str, key: &str, meta: &Meta) -> Result<bool> {
        let mut text = serde_json::to_vec_pretty(meta).expect("Meta serializes");
        text.push(b'\n');
        self.put(id, &format!("{key}.meta.json"), &text)
    }

    /// Commits what changed: with `now` always, else only when the last commit is a day old
    /// (or there is none). True when it committed.
    pub fn commit(&self, now: bool, today: &str) -> Result<bool> {
        if git(
            &self.dir,
            ["status", "--porcelain", "--untracked-files=all"],
        )?
        .is_empty()
        {
            return Ok(false);
        }
        if !now {
            let last = git(&self.dir, ["log", "-1", "--format=%ct"])
                .ok()
                .and_then(|t| t.parse::<i64>().ok());
            let clock = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            if last.is_some_and(|l| clock - l < DAILY) {
                return Ok(false);
            }
        }
        git(&self.dir, ["add", "--all"])?;
        let env = self.identity();
        git_with(
            &self.dir,
            ["commit", "--quiet", "-m", &format!("sessions {today}")],
            &env,
            None,
        )?;
        Ok(true)
    }

    fn identity(&self) -> Vec<(&'static str, &'static str)> {
        let mut env = Vec::new();
        if !git_ok(&self.dir, ["config", "user.name"]) {
            env.extend([
                ("GIT_AUTHOR_NAME", IDENTITY.0),
                ("GIT_COMMITTER_NAME", IDENTITY.0),
            ]);
        }
        if !git_ok(&self.dir, ["config", "user.email"]) {
            env.extend([
                ("GIT_AUTHOR_EMAIL", IDENTITY.1),
                ("GIT_COMMITTER_EMAIL", IDENTITY.1),
            ]);
        }
        env
    }

    /// Pushes local commits the remote lacks (after a rebase on what it has). Nothing to
    /// push: no network. True when it pushed.
    pub fn push(&self) -> Result<bool> {
        if !git_ok(&self.dir, ["rev-parse", "--verify", "--quiet", "HEAD"]) {
            return Ok(false);
        }
        let theirs = format!("refs/remotes/origin/{BRANCH}");
        let known = git_ok(&self.dir, ["rev-parse", "--verify", "--quiet", &theirs]);
        if known
            && git(
                &self.dir,
                ["rev-list", "--count", &format!("{theirs}..HEAD")],
            )? == "0"
        {
            return Ok(false);
        }
        git(&self.dir, ["fetch", "--quiet", "origin"])
            .map_err(|e| fail!("cannot fetch the transcripts repo: {e}"))?;
        if git_ok(&self.dir, ["rev-parse", "--verify", "--quiet", &theirs])
            && !git_ok(&self.dir, ["merge-base", "--is-ancestor", &theirs, "HEAD"])
        {
            let env = self.identity();
            if let Err(e) = git_with(&self.dir, ["rebase", "--quiet", &theirs], &env, None) {
                let _ = git(&self.dir, ["rebase", "--abort"]);
                return Err(fail!("the transcripts repo has diverged: {e}"));
            }
        }
        git(
            &self.dir,
            [
                "push",
                "--quiet",
                "origin",
                &format!("HEAD:refs/heads/{BRANCH}"),
            ],
        )
        .map_err(|e| fail!("cannot push the transcripts repo: {e}"))?;
        Ok(true)
    }
}

/// Exports one stew's sessions into the checkout. The script runs where the stew lives
/// (`no_start`: not if it is stopped); `lock` guards the checkout while files are written.
pub fn export(
    tr: &Transcripts,
    lock: &std::sync::Mutex<()>,
    driver: &dyn Driver,
    t: &Target,
    no_start: bool,
) -> Result<Exported> {
    let id = &t.id;
    let out = driver.exec(
        t,
        &sh(EXPORT, &["list"]),
        None,
        None,
        no_start,
        Some(LIST_TIMEOUT),
    )?;
    match out.status {
        Status::NotRunning => return Ok(Exported::NotRunning),
        Status::NoBox => return Err(fail!("stew {id} is not on its site")),
        Status::Exited(0) => {}
        Status::Exited(_) => return Err(fail!("listing its sessions failed: {}", out.reason())),
    }
    let mut report = Report::default();
    let mut root = None;
    let mut oc_version = None;
    let mut claude: Vec<ClaudeSession> = Vec::new();
    let mut lists: Vec<(String, Vec<u8>)> = Vec::new();
    for rec in records(&out.stdout)? {
        match rec {
            Rec::Line(f) => match f[0].as_str() {
                "R" if f.len() == 2 => root = Some(f[1].clone()),
                "V" if f.len() == 3 && f[1] == "opencode" => {
                    oc_version = Some(f[2].trim().to_string()).filter(|v| !v.is_empty())
                }
                "C" if f.len() == 4 => {
                    let root = root.as_deref().unwrap_or("");
                    let file = &f[1];
                    let stem = Path::new(file)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.strip_suffix(".jsonl"))
                        .unwrap_or("");
                    let Some(dir) = under(root, &f[3]) else {
                        continue;
                    };
                    if !session_ok(stem) {
                        report
                            .problems
                            .push(format!("claude session {file}: a bad name"));
                        continue;
                    }
                    claude.push(ClaudeSession {
                        file: file.clone(),
                        meta: Meta {
                            agent: "claude".into(),
                            version: None,
                            stew: id.clone(),
                            session: stem.to_string(),
                            dir,
                            created: None,
                            updated: None,
                            sha256: Some(f[2].clone()),
                        },
                    });
                }
                "E" => report.problems.push(f[1..].join(" ")),
                _ => return Err(fail!("bad export line: {}", f.join("\t"))),
            },
            Rec::Blob(f, bytes) if f[0] == "O" && f.len() == 2 => lists.push((f[1].clone(), bytes)),
            Rec::Blob(f, _) => return Err(fail!("bad export record: {}", f.join("\t"))),
        }
    }
    let root = root.ok_or_else(|| fail!("the export named no root"))?;

    // opencode's sessions: those whose directory is the root or below it
    let mut oc: BTreeMap<String, OcSession> = BTreeMap::new();
    for (ran, bytes) in lists {
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let list: Vec<Value> = match serde_json::from_slice(&bytes) {
            Ok(Value::Array(a)) => a,
            _ => {
                report.problems.push(format!(
                    "opencode session list in {ran}: not a JSON list: {}",
                    String::from_utf8_lossy(&bytes[..bytes.len().min(120)]).trim()
                ));
                continue;
            }
        };
        for s in list {
            let Some(sid) = s.get("id").and_then(Value::as_str) else {
                report.problems.push(format!(
                    "opencode session list in {ran}: a session without id"
                ));
                continue;
            };
            if !session_ok(sid) {
                report
                    .problems
                    .push(format!("opencode session {sid}: a bad id"));
                continue;
            }
            // on the host every stew shares opencode's state: only what ran here counts
            let Some(dir) = s
                .get("directory")
                .and_then(Value::as_str)
                .and_then(|d| under(&root, d))
            else {
                continue;
            };
            oc.entry(sid.to_string()).or_insert(OcSession {
                ran: ran.clone(),
                meta: Meta {
                    agent: "opencode".into(),
                    version: oc_version.clone(),
                    stew: id.clone(),
                    session: sid.to_string(),
                    dir,
                    created: oc_time(&s, "created"),
                    updated: oc_time(&s, "updated"),
                    sha256: None,
                },
            });
        }
    }
    report.sessions = claude.len() + oc.len();

    // what changed since the last export
    let mut wanted = String::new();
    let mut by_file: BTreeMap<String, Meta> = BTreeMap::new();
    for c in claude {
        let key = format!("claude-{}", c.meta.session);
        let had = tr.meta(id, &key);
        let content = tr.path(id, &format!("{key}.jsonl"));
        if had.as_ref().and_then(|m| m.sha256.as_ref()) == c.meta.sha256.as_ref()
            && content.is_file()
        {
            continue;
        }
        wanted.push_str(&format!("C\t{}\n", c.file));
        by_file.insert(c.file, c.meta);
    }
    let mut by_id: BTreeMap<String, Meta> = BTreeMap::new();
    for (sid, s) in oc {
        let key = format!("opencode-{sid}");
        let had = tr.meta(id, &key);
        let content = tr.path(id, &format!("{key}.json"));
        if had.as_ref() == Some(&s.meta) && content.is_file() {
            continue;
        }
        wanted.push_str(&format!("O\t{}\t{sid}\n", s.ran));
        by_id.insert(sid, s.meta);
    }
    if wanted.is_empty() {
        return Ok(Exported::Done(report));
    }

    let out = driver.exec(
        t,
        &sh(EXPORT, &["send"]),
        Some(wanted.as_bytes()),
        None,
        no_start,
        Some(SEND_TIMEOUT),
    )?;
    if !out.ok() {
        return Err(fail!("sending its sessions failed: {}", out.reason()));
    }
    let recs = records(&out.stdout)?;
    let _lock = lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut sent = BTreeSet::new();
    for rec in recs {
        match rec {
            Rec::Line(f) if f[0] == "E" => report.problems.push(f[1..].join(" ")),
            Rec::Blob(f, bytes) if f.len() == 3 && f[0] == "F" && f[1] == "C" => {
                let Some(mut meta) = by_file.get(&f[2]).cloned() else {
                    return Err(fail!("the export sent {}, which was not asked for", f[2]));
                };
                let (version, created, updated) = claude_facts(&bytes);
                meta.version = version;
                meta.created = created;
                meta.updated = updated;
                let key = format!("claude-{}", meta.session);
                report.written += usize::from(tr.put(id, &format!("{key}.jsonl"), &bytes)?);
                report.written += usize::from(tr.put_meta(id, &key, &meta)?);
                sent.insert(f[2].clone());
            }
            Rec::Blob(f, bytes) if f.len() == 3 && f[0] == "F" && f[1] == "O" => {
                let Some(meta) = by_id.get(&f[2]) else {
                    return Err(fail!(
                        "the export sent opencode session {}, which was not asked for",
                        f[2]
                    ));
                };
                if serde_json::from_slice::<Value>(&bytes).is_err() {
                    report
                        .problems
                        .push(format!("opencode session export {}: not JSON", f[2]));
                    continue;
                }
                let key = format!("opencode-{}", meta.session);
                report.written += usize::from(tr.put(id, &format!("{key}.json"), &bytes)?);
                report.written += usize::from(tr.put_meta(id, &key, meta)?);
                sent.insert(f[2].clone());
            }
            _ => return Err(fail!("bad export record from send")),
        }
    }
    for missing in by_file.keys().chain(by_id.keys()) {
        if !sent.contains(missing) && !report.problems.iter().any(|p| p.contains(missing)) {
            report.problems.push(format!("{missing} was not sent"));
        }
    }
    Ok(Exported::Done(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_records() {
        let out = b"R\t/w\nO\t.\t3\n[1]\nC\t/h/a.jsonl\tab\t/w\nF\tC\t/h/a.jsonl\t2\n{}\nDONE\n";
        let r = records(out).unwrap();
        assert_eq!(r.len(), 4);
        assert_eq!(
            r[1],
            Rec::Blob(vec!["O".into(), ".".into()], b"[1]".to_vec())
        );
        assert_eq!(
            r[3],
            Rec::Blob(
                vec!["F".into(), "C".into(), "/h/a.jsonl".into()],
                b"{}".to_vec()
            )
        );
        // a blob may hold newlines and DONE lines
        let r = records(b"O\t.\t6\nDONE\n\n\nDONE\n").unwrap();
        assert_eq!(
            r[0],
            Rec::Blob(vec!["O".into(), ".".into()], b"DONE\n\n".to_vec())
        );
        for bad in [
            &b""[..],
            b"R\t/w\n",
            b"DONE\nx\n",
            b"O\t.\t9\n[1]\nDONE\n",
            b"O\t.\t1\n[1]\nDONE\n",
            b"O\t.\tx\n[\nDONE\n",
            b"what\nDONE\n",
        ] {
            assert!(records(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn names_dirs_and_times() {
        assert!(session_ok("ses_3a8f"));
        assert!(session_ok("0b8d5a1c-1f2e-4c1e-9c1f-2a3b4c5d6e7f"));
        for bad in ["", ".x", "a/b", "..", "a b", &"x".repeat(129)] {
            assert!(!session_ok(bad), "{bad}");
        }
        assert_eq!(under("/w", "/w").as_deref(), Some("."));
        assert_eq!(under("/w", "/w/a/b").as_deref(), Some("a/b"));
        assert_eq!(under("/w", "/w/").as_deref(), Some("."));
        assert_eq!(under("/w", "/wx"), None);
        assert_eq!(under("/w", "/"), None);
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(1_763_164_800_123), "2025-11-15T00:00:00.123Z");
        assert_eq!(iso(1_763_208_245_007), "2025-11-15T12:04:05.007Z");
        let v: Value = serde_json::from_str(r#"{"time":{"created":0,"updated":"x"}}"#).unwrap();
        assert_eq!(
            oc_time(&v, "created").as_deref(),
            Some("1970-01-01T00:00:00.000Z")
        );
        assert_eq!(oc_time(&v, "updated").as_deref(), Some("x"));
        let (v, a, b) = claude_facts(
            b"{\"type\":\"summary\"}\nnot json\n{\"version\":\"1\",\"timestamp\":\"t1\"}\n{\"version\":\"2\",\"timestamp\":\"t2\"}\n",
        );
        assert_eq!(
            (v.as_deref(), a.as_deref(), b.as_deref()),
            (Some("2"), Some("t1"), Some("t2"))
        );
    }
}
