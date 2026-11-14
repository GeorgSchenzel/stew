//! Git on the hub, hook-free: no hooks, no fsmonitor, never a prompt (a credential problem
//! fails instead of hanging the daemon). The hub's own repos only (the artifacts mirror,
//! later transcripts) and `ls-remote`; git in clones runs in the box.

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{Result, fail};

fn command<I, S>(dir: &Path, args: I) -> (Command, String)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    let mut shown = String::from("git");
    for a in args {
        shown.push(' ');
        shown.push_str(&a.as_ref().to_string_lossy());
        cmd.arg(a);
    }
    (cmd, shown)
}

/// Runs git in `dir` with extra `env` and optional stdin; returns raw stdout, or an error
/// carrying git's stderr.
pub fn git_with<I, S>(
    dir: &Path,
    args: I,
    env: &[(&str, &str)],
    input: Option<&[u8]>,
) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let (mut cmd, shown) = command(dir, args);
    cmd.envs(env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().map_err(|e| fail!("cannot run git: {e}"))?;
    let out = std::thread::scope(|s| {
        if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
            // a writer thread, so a large input can't deadlock against a full stdout pipe
            s.spawn(move || {
                let _ = stdin.write_all(data);
            });
        }
        child.wait_with_output()
    })
    .map_err(|e| fail!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(fail!("{shown} failed: {}", stderr.trim()))
    }
}

/// Runs git in `dir` and returns its trimmed stdout.
pub fn git<I, S>(dir: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = git_with(dir, args, &[], None)?;
    Ok(String::from_utf8_lossy(&out).trim_end().to_string())
}

/// Runs git silently and reports whether it succeeded.
pub fn git_ok<I, S>(dir: &Path, args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let (mut cmd, _) = command(dir, args);
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
