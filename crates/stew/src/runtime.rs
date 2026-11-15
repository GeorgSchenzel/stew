//! The agent runtime: what stew itself does with agents at the hub (connect a stew, start an
//! agent, list them with their status, attach). hc implements it now: the hub's core is hc's
//! default session (`stew-hc.service`, or whoever started it first: herdr-app, a plain `hc`),
//! which adds each stew as a machine reached through `stew exec <id> --no-start -- sh -c`,
//! so a stopped stew stays stopped (hc's reconnect loop never starts one). Agents in a stew
//! use hc themselves; stew only starts the first.
//!
//! herdr-app (plan.md, herdr-app): each stew is one space on its machine. At connect stew
//! starts a shell at the root labelled `herdr.space`/`herdr.tab` unless the space has a
//! terminal; every agent stew starts is a tab of that space.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use serde_json::Value;
use stew_proto::AgentInfo;

use crate::config::{Config, RuntimeKind};
use crate::driver::{Launch, run_command};
use crate::error::{Result, fail};

/// The hub core's session: hc's default one. `HC_SESSION` is removed from every call, never
/// inherited; this names its socket.
const HUB_SESSION: &str = "default";

/// The label values of a stew's one space and its shell's tab (ids local to the stew's
/// machine, so every stew uses the same ones; herdr makes a space or tab of an id it doesn't
/// know). Each agent's tab id is its name.
pub const SPACE: &str = "stew";
const SHELL_TAB: &str = "shell";
/// The name of stew's own shell (left out of the agents).
pub const SHELL: &str = "stew-shell";

/// Variables that would point hc at another core or terminal than the hub's.
const HC_VARS: [&str; 4] = [
    "HC_SESSION",
    "HC_SOCKET",
    "HC_TERMINAL_ID",
    "HC_TERMINAL_SOCKET",
];

/// How hc reaches a stew: hc appends its remote line as one argument.
pub fn via(id: &str) -> String {
    format!("stew exec {id} --no-start -- sh -c")
}

pub trait Runtime: Send + Sync {
    /// Adds the stew as a machine, reached by `via`; one that is already there the same way
    /// is kept, one reached another way is replaced. Then its space: a shell in `root`
    /// unless the space has a terminal.
    fn connect(&self, id: &str, via: &str, root: &str) -> Result<()>;
    /// Forgets the machine (nothing there is stopped by this).
    fn disconnect(&self, id: &str) -> Result<()>;
    /// Starts `argv` in a new terminal on the stew's machine, in `cwd`, named `<prefix>-<n>`.
    /// Returns its address, `<id>/<name>`.
    fn run(&self, id: &str, prefix: &str, argv: &[String], cwd: &str) -> Result<String>;
    /// Live terminals on stews' machines, by machine (stew's own shell left out).
    fn agents(&self) -> Result<BTreeMap<String, Vec<AgentInfo>>>;
    /// The command line that attaches a client's terminal to `address`.
    fn attach(&self, address: &str) -> Result<Launch>;
    /// The command that stops the stew's own core where it lives, its terminals with it
    /// (a host stew's core outlives its dir otherwise). None: nothing to stop.
    fn stop_there(&self) -> Option<Vec<String>>;
}

pub fn for_config(config: &Config) -> Box<dyn Runtime> {
    match config.runtime.unwrap_or(RuntimeKind::Hc) {
        RuntimeKind::Hc => Box::new(Hc::new("hc")),
        RuntimeKind::None => Box::new(NoRuntime),
    }
}

/// Whether the config's runtime can start agents (checked before anything is claimed or
/// started for one).
pub fn can_run(config: &Config) -> Result<()> {
    match config.runtime {
        Some(RuntimeKind::None) => Err(fail!("{NONE}")),
        _ => Ok(()),
    }
}

/// `runtime: none`: stews without agents.
pub struct NoRuntime;

const NONE: &str = "no agent runtime (`runtime: none` in stew's config.yaml)";

impl Runtime for NoRuntime {
    fn connect(&self, _: &str, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn disconnect(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn run(&self, _: &str, _: &str, _: &[String], _: &str) -> Result<String> {
        Err(fail!("{NONE}"))
    }
    fn agents(&self) -> Result<BTreeMap<String, Vec<AgentInfo>>> {
        Ok(BTreeMap::new())
    }
    fn attach(&self, _: &str) -> Result<Launch> {
        Err(fail!("{NONE}"))
    }
    fn stop_there(&self) -> Option<Vec<String>> {
        None
    }
}

pub struct Hc {
    /// `hc` by name, from PATH (never a store path).
    bin: String,
}

/// An error hc reported: its kind (`name_taken`, `machine_unknown`, …) and message.
struct HcError {
    kind: String,
    message: String,
}

impl Hc {
    pub fn new(bin: &str) -> Self {
        Hc { bin: bin.into() }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        // the hub's core (the default session), whatever the daemon was started from
        for var in HC_VARS {
            cmd.env_remove(var);
        }
        cmd
    }

    /// One hc call with `--json`; its JSON, or the error it reported. Only while the hub's
    /// core runs: hc would otherwise start a core of the session itself, outside
    /// `stew-hc.service` (the daemon is only that core's client).
    fn call(&self, args: &[&str], timeout: Duration) -> std::result::Result<Value, HcError> {
        let socket = core_socket();
        if std::os::unix::net::UnixStream::connect(&socket).is_err() {
            return Err(HcError {
                kind: "unavailable".into(),
                message: format!(
                    "the hub's hc core is not running (stew-hc.service; no core at {})",
                    socket.display()
                ),
            });
        }
        let mut cmd = self.command();
        cmd.args(args);
        let (code, out, err) = run_command(cmd, None, Some(timeout)).map_err(|e| HcError {
            kind: "run".into(),
            message: e.to_string(),
        })?;
        let json: Option<Value> = serde_json::from_slice(&out).ok();
        if code == 0
            && let Some(v) = json
        {
            return Ok(v);
        }
        let reported = json.as_ref().and_then(|v| v.get("error"));
        Err(HcError {
            kind: reported
                .and_then(|e| e["kind"].as_str())
                .unwrap_or("failed")
                .into(),
            message: reported
                .and_then(|e| e["message"].as_str())
                .map(String::from)
                .unwrap_or_else(|| {
                    let err = String::from_utf8_lossy(&err);
                    let err = err.trim();
                    if err.is_empty() {
                        format!("exit {code}")
                    } else {
                        err.lines().last().unwrap_or(err).to_string()
                    }
                }),
        })
    }

    fn ok(&self, args: &[&str], timeout: Duration) -> Result<Value> {
        self.call(args, timeout)
            .map_err(|e| fail!("hc {}: {}", args[0], e.message))
    }

    /// The machines added to the hub's core: name → target.
    fn machines(&self) -> Result<BTreeMap<String, String>> {
        let v = self.ok(&["machine", "ls", "--json"], SHORT)?;
        Ok(v["machines"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                Some((
                    m["name"].as_str()?.to_string(),
                    m["target"].as_str().unwrap_or("").to_string(),
                ))
            })
            .collect())
    }

    fn terminals(&self) -> Result<Vec<Value>> {
        let v = self.ok(&["ls", "--json"], SHORT)?;
        Ok(v["terminals"].as_array().cloned().unwrap_or_default())
    }
}

/// Where the hub's core (the default session) listens, by hc's rule (docs/security.md, "State and endpoints"):
/// `$XDG_RUNTIME_DIR/herdr-core/<session>.sock`, else `/run/user/<uid>/herdr-core/` when that
/// dir exists, else the session's state dir. (A `socket` in hc's config.toml moves it; the
/// hub's hc config must not set one.)
pub fn core_socket() -> PathBuf {
    let name = format!("{HUB_SESSION}.sock");
    let var = |v: &str| {
        std::env::var_os(v)
            .filter(|x| !x.is_empty())
            .map(PathBuf::from)
    };
    if let Some(run) = var("XDG_RUNTIME_DIR") {
        return run.join("herdr-core").join(name);
    }
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata("/proc/self").map_or(0, |m| m.uid());
    let run = PathBuf::from(format!("/run/user/{uid}"));
    if run.is_dir() {
        return run.join("herdr-core").join(name);
    }
    let state = var("XDG_STATE_HOME").unwrap_or_else(|| {
        var("HOME")
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".local/state")
    });
    state.join("herdr-core").join(HUB_SESSION).join(name)
}

const SHORT: Duration = Duration::from_secs(30);
/// `machine add` dials (hc gives a dial 20 s) and `run` waits for the machine's connection.
const LONG: Duration = Duration::from_secs(60);

/// `t3` → the terminal's machine part (`box` in `box/t3`); None for the hub's own.
fn machine_of(address: &str) -> Option<&str> {
    address.split_once('/').map(|(m, _)| m)
}

impl Runtime for Hc {
    fn connect(&self, id: &str, via: &str, root: &str) -> Result<()> {
        let there = self.machines()?.get(id).map(|t| t == via);
        if there != Some(true) {
            if there.is_some() {
                self.ok(&["machine", "rm", id, "--json"], SHORT)?;
            }
            let added = self.ok(
                &["machine", "add", "--name", id, "--via", via, "--json"],
                LONG,
            )?;
            let name = added["name"].as_str().unwrap_or("");
            if name != id {
                // hc renames a taken name (`id-2`); a machine under another name is no use
                let _ = self.call(&["machine", "rm", name, "--json"], SHORT);
                return Err(fail!("hc added {id} as '{name}'"));
            }
        }
        // the stew's space: a shell, unless the space already has a terminal
        let space = format!("herdr.space={SPACE}");
        let v = self.ok(&["ls", "--label", &space, "--json"], SHORT)?;
        let has = v["terminals"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|t| t["address"].as_str().and_then(machine_of) == Some(id));
        if !has {
            let tab = format!("herdr.tab={SHELL_TAB}");
            let args = [
                "run",
                "--bg",
                "--on",
                id,
                "--cwd",
                root,
                "--name",
                SHELL,
                "--keep",
                "--label",
                &space,
                "--label",
                &tab,
                "--no-parent",
                "--json",
            ];
            match self.call(&args, LONG) {
                // there since the ls (another connect): fine
                Ok(_) => {}
                Err(e) if e.kind == "name_taken" => {}
                Err(e) => return Err(fail!("hc run on {id} (its shell): {}", e.message)),
            }
        }
        Ok(())
    }

    fn disconnect(&self, id: &str) -> Result<()> {
        if self.machines()?.contains_key(id) {
            self.ok(&["machine", "rm", id, "--json"], SHORT)?;
        }
        Ok(())
    }

    fn run(&self, id: &str, prefix: &str, argv: &[String], cwd: &str) -> Result<String> {
        let taken: Vec<String> = self
            .terminals()?
            .iter()
            .filter(|t| t["address"].as_str().and_then(machine_of) == Some(id))
            .filter_map(|t| t["name"].as_str().map(String::from))
            .collect();
        let mut n = 1;
        for _ in 0..8 {
            while taken.contains(&format!("{prefix}-{n}")) {
                n += 1;
            }
            let name = format!("{prefix}-{n}");
            // a tab of the stew's space, its id the agent's name
            let space = format!("herdr.space={SPACE}");
            let tab = format!("herdr.tab={name}");
            let mut args = vec![
                "run",
                "--bg",
                "--on",
                id,
                "--cwd",
                cwd,
                "--name",
                &name,
                "--label",
                &space,
                "--label",
                &tab,
                "--no-parent",
                "--json",
                "--",
            ];
            args.extend(argv.iter().map(String::as_str));
            match self.call(&args, LONG) {
                Ok(_) => return Ok(format!("{id}/{name}")),
                // started at the same moment by someone else: the next number
                Err(e) if e.kind == "name_taken" => n += 1,
                Err(e) => return Err(fail!("hc run on {id}: {}", e.message)),
            }
        }
        Err(fail!("no free terminal name for {prefix} on {id}"))
    }

    fn agents(&self) -> Result<BTreeMap<String, Vec<AgentInfo>>> {
        let mut by: BTreeMap<String, Vec<AgentInfo>> = BTreeMap::new();
        for t in self.terminals()? {
            let Some(address) = t["address"].as_str() else {
                continue;
            };
            let Some(machine) = machine_of(address) else {
                continue;
            };
            if t["name"].as_str() == Some(SHELL) {
                continue;
            }
            let address = match t["name"].as_str() {
                Some(name) => format!("{machine}/{name}"),
                None => address.to_string(),
            };
            by.entry(machine.to_string()).or_default().push(AgentInfo {
                address,
                program: t["agent"].as_str().unwrap_or("").to_string(),
                status: t["status"].as_str().unwrap_or("running").to_string(),
            });
        }
        Ok(by)
    }

    /// `env -u …`: the client runs it with its own environment, whose hc variables must
    /// not pick another core.
    fn attach(&self, address: &str) -> Result<Launch> {
        let mut argv = vec!["env".to_string()];
        for var in HC_VARS {
            argv.extend(["-u".into(), var.into()]);
        }
        argv.extend([self.bin.clone(), "attach".into(), address.into()]);
        Ok(Launch {
            argv,
            cwd: None,
            env: vec![],
            path: vec![],
        })
    }

    fn stop_there(&self) -> Option<Vec<String>> {
        Some(
            ["hc", "server", "stop", "--json"]
                .map(String::from)
                .to_vec(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(machine_of("a-k3fq/t3"), Some("a-k3fq"));
        assert_eq!(machine_of("t3"), None);
        assert_eq!(via("a-k3fq"), "stew exec a-k3fq --no-start -- sh -c");
    }
}
