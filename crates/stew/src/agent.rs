//! Agent command lines: stew builds them itself, a few lines per agent, no launcher. The
//! model is passed through as given (both agents call the flag `--model`), never validated
//! or translated. Permission prompts are the agents' settings' business (bento's presets),
//! never a flag added here. A claude account (to spread usage over subscriptions) is the
//! token claude gets: `CLAUDE_CODE_OAUTH_TOKEN` from `CLAUDE_ACCOUNT_<NAME>_TOKEN`, which
//! bento's claude preset sets per account where the stew lives.

use crate::config::Config;
use crate::error::{Result, fail};

/// The agents stew knows how to start.
pub const AGENTS: &[&str] = &["claude", "opencode"];

/// The agent when neither the call nor the config names one.
pub const DEFAULT_AGENT: &str = "claude";

/// What `--start` gives the agent.
pub const BRIEF_TASK: &str = "Your brief is in notes/BRIEF.md.";

/// What an agent starts as: which agent, its model and (claude) its account.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub agent: String,
    pub model: Option<String>,
    pub account: Option<String>,
}

impl Choice {
    /// Its terminal's name prefix: the agent, and the account when one is chosen
    /// (`claude-second`), so `ls` and herdr's tabs show it.
    pub fn prefix(&self) -> String {
        match &self.account {
            Some(a) => format!("{}-{a}", self.agent),
            None => self.agent.clone(),
        }
    }
}

/// What a call ends up with: the call's, else the config's. The config's model goes only
/// with the config's agent (another agent's model names differ); its account only with
/// claude.
pub fn choose(
    config: &Config,
    agent: Option<&str>,
    model: Option<&str>,
    account: Option<&str>,
) -> Choice {
    let configured = config.agent.as_deref().unwrap_or(DEFAULT_AGENT);
    let agent = agent.unwrap_or(configured).to_string();
    let model = match model {
        Some(m) => Some(m.to_string()),
        None if agent == configured => config.model.clone(),
        None => None,
    };
    let account = match account {
        Some(a) => Some(a.to_string()),
        None if agent == "claude" => config.account.clone(),
        None => None,
    };
    Choice {
        agent,
        model,
        account,
    }
}

/// The variable holding an account's token where the stew lives (bento's claude preset):
/// `second` → `CLAUDE_ACCOUNT_SECOND_TOKEN`.
fn token_var(account: &str) -> String {
    format!(
        "CLAUDE_ACCOUNT_{}_TOKEN",
        account.to_uppercase().replace('-', "_")
    )
}

/// The agent's argv for a task.
pub fn command(c: &Choice, task: &str) -> Result<Vec<String>> {
    let (agent, model) = (c.agent.as_str(), c.model.as_deref());
    if task.trim().is_empty() {
        return Err(fail!("an agent needs a task"));
    }
    // a word an agent's CLI would read as a flag never goes where a value is expected
    if task.starts_with('-') {
        return Err(fail!("a task must not start with '-'"));
    }
    if let Some(m) = model
        && (m.is_empty() || m.starts_with('-') || m.chars().any(char::is_whitespace))
    {
        return Err(fail!("'{m}' is not a model name"));
    }
    if let Some(a) = &c.account {
        let ok = !a.is_empty()
            && a.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !a.starts_with('-');
        if !ok {
            return Err(fail!(
                "'{a}' is not an account name (lowercase letters, digits, dashes)"
            ));
        }
        if agent != "claude" {
            return Err(fail!(
                "accounts are claude's (--account with --agent {agent})"
            ));
        }
    }
    let model = model
        .into_iter()
        .flat_map(|m| ["--model".to_string(), m.to_string()]);
    Ok(match agent {
        // the token by the variable's name only: no secret ever goes into an argv
        "claude" => match &c.account {
            Some(a) => {
                let var = token_var(a);
                let script = format!(
                    "[ -n \"${{{var}:-}}\" ] || {{ echo \"stew: no claude account {a} here (${var} is not set)\" >&2; exit 64; }}; CLAUDE_CODE_OAUTH_TOKEN=\"${var}\" exec claude \"$@\""
                );
                ["sh".to_string(), "-c".into(), script, "claude".into()]
                    .into_iter()
                    .chain(model)
                    .chain([task.to_string()])
                    .collect()
            }
            None => ["claude".to_string()]
                .into_iter()
                .chain(model)
                .chain([task.to_string()])
                .collect(),
        },
        // --standalone: two in one box would collide on the background service's port
        "opencode" => ["opencode".to_string(), "--standalone".into()]
            .into_iter()
            .chain(model)
            .chain(["--prompt".to_string(), task.to_string()])
            .collect(),
        other => {
            return Err(fail!(
                "unknown agent '{other}' (agents: {})",
                AGENTS.join(", ")
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn c(agent: &str, model: Option<&str>) -> Choice {
        Choice {
            agent: agent.into(),
            model: model.map(String::from),
            account: None,
        }
    }

    fn command(agent: &str, model: Option<&str>, task: &str) -> Result<Vec<String>> {
        super::command(&c(agent, model), task)
    }

    #[test]
    fn command_lines() {
        assert_eq!(
            command("claude", None, "do x").unwrap(),
            v(&["claude", "do x"])
        );
        assert_eq!(
            command("claude", Some("opus"), "do x").unwrap(),
            v(&["claude", "--model", "opus", "do x"])
        );
        assert_eq!(
            command("opencode", None, "t").unwrap(),
            v(&["opencode", "--standalone", "--prompt", "t"])
        );
        assert_eq!(
            command("opencode", Some("anthropic/claude-sonnet-4-5"), "t").unwrap(),
            v(&[
                "opencode",
                "--standalone",
                "--model",
                "anthropic/claude-sonnet-4-5",
                "--prompt",
                "t"
            ])
        );
        // a prompt with quotes, newlines and shell words stays one argument
        let task = "it's \"x\"\n$(rm -rf /) `y`";
        assert_eq!(command("claude", None, task).unwrap()[1], task);
        for bad in ["", "  ", "--dangerously-skip-permissions", "-p"] {
            assert!(command("claude", None, bad).is_err(), "{bad:?}");
        }
        for bad in ["", "-x", "a b"] {
            assert!(command("claude", Some(bad), "t").is_err(), "{bad:?}");
        }
        assert!(command("sh", None, "t").is_err());
    }

    #[test]
    fn accounts() {
        let with = |agent: &str, account: &str| Choice {
            account: Some(account.into()),
            ..c(agent, Some("opus"))
        };
        let argv = super::command(&with("claude", "second"), "do x").unwrap();
        assert_eq!(argv[..2], v(&["sh", "-c"]));
        assert!(argv[2].contains("\"$CLAUDE_ACCOUNT_SECOND_TOKEN\" exec claude"));
        assert_eq!(argv[3..], v(&["claude", "--model", "opus", "do x"]));
        assert_eq!(with("claude", "second").prefix(), "claude-second");
        assert_eq!(c("claude", None).prefix(), "claude");
        assert_eq!(token_var("work-2"), "CLAUDE_ACCOUNT_WORK_2_TOKEN");
        for bad in ["", "Main", "a b", "-x", "a;b"] {
            assert!(
                super::command(&with("claude", bad), "t").is_err(),
                "{bad:?}"
            );
        }
        assert!(super::command(&with("opencode", "second"), "t").is_err());
    }

    /// The script runs: the account's token becomes claude's, a missing one is exit 64.
    #[test]
    fn account_script() {
        let argv = super::command(&with_account(), "the task").unwrap();
        let run = |env: &[(&str, &str)]| {
            let dir = tempfile::tempdir().unwrap();
            let claude = dir.path().join("claude");
            std::fs::write(
                &claude,
                "#!/bin/sh\nprintf '%s|' \"$CLAUDE_CODE_OAUTH_TOKEN\" \"$@\"\n",
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut cmd = std::process::Command::new(&argv[0]);
            cmd.args(&argv[1..])
                .env_clear()
                .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()));
            for (k, val) in env {
                cmd.env(k, val);
            }
            let out = cmd.output().unwrap();
            (
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        };
        let (code, out, _) = run(&[
            ("CLAUDE_ACCOUNT_SECOND_TOKEN", "tok2"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "tok1"),
        ]);
        assert_eq!((code, out.as_str()), (Some(0), "tok2|the task|"));
        let (code, _, err) = run(&[("CLAUDE_CODE_OAUTH_TOKEN", "tok1")]);
        assert_eq!(code, Some(64));
        assert!(err.contains("no claude account second"), "{err}");
    }

    fn with_account() -> Choice {
        Choice {
            account: Some("second".into()),
            ..c("claude", None)
        }
    }

    #[test]
    fn choosing() {
        let am = |c: Choice| (c.agent, c.model, c.account);
        let s = |x: &str| Some(x.to_string());
        let cfg: Config =
            serde_yaml::from_str("agent: opencode\nmodel: m1\naccount: second\n").unwrap();
        assert_eq!(
            am(choose(&cfg, None, None, None)),
            ("opencode".into(), s("m1"), None)
        );
        assert_eq!(
            am(choose(&cfg, Some("claude"), None, None)),
            ("claude".into(), None, s("second"))
        );
        assert_eq!(
            am(choose(&cfg, Some("claude"), None, Some("main"))),
            ("claude".into(), None, s("main"))
        );
        assert_eq!(
            am(choose(&cfg, Some("opencode"), Some("m2"), None)),
            ("opencode".into(), s("m2"), None)
        );
        let none = Config::default();
        assert_eq!(
            am(choose(&none, None, None, None)),
            ("claude".into(), None, None)
        );
    }
}
