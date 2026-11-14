//! Agent command lines: stew builds them itself, a few lines per agent, no launcher. The
//! model is passed through as given (both agents call the flag `--model`), never validated
//! or translated. Permission prompts are the agents' settings' business (bento's presets),
//! never a flag added here.

use crate::config::Config;
use crate::error::{Result, fail};

/// The agents stew knows how to start.
pub const AGENTS: &[&str] = &["claude", "opencode"];

/// The agent when neither the call nor the config names one.
pub const DEFAULT_AGENT: &str = "claude";

/// What `--start` gives the agent.
pub const BRIEF_TASK: &str = "Your brief is in notes/BRIEF.md.";

/// The agent and model a call ends up with: the call's, else the config's. The config's
/// model goes only with the config's agent (another agent's model names differ).
pub fn choose(
    config: &Config,
    agent: Option<&str>,
    model: Option<&str>,
) -> (String, Option<String>) {
    let configured = config.agent.as_deref().unwrap_or(DEFAULT_AGENT);
    let agent = agent.unwrap_or(configured).to_string();
    let model = match model {
        Some(m) => Some(m.to_string()),
        None if agent == configured => config.model.clone(),
        None => None,
    };
    (agent, model)
}

/// The agent's argv for a task.
pub fn command(agent: &str, model: Option<&str>, task: &str) -> Result<Vec<String>> {
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
    let model = model
        .into_iter()
        .flat_map(|m| ["--model".to_string(), m.to_string()]);
    Ok(match agent {
        "claude" => ["claude".to_string()]
            .into_iter()
            .chain(model)
            .chain([task.to_string()])
            .collect(),
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
    fn choosing() {
        let c: Config = serde_yaml::from_str("agent: opencode\nmodel: m1\n").unwrap();
        assert_eq!(
            choose(&c, None, None),
            ("opencode".into(), Some("m1".into()))
        );
        assert_eq!(choose(&c, Some("claude"), None), ("claude".into(), None));
        assert_eq!(
            choose(&c, Some("opencode"), Some("m2")),
            ("opencode".into(), Some("m2".into()))
        );
        let none = Config::default();
        assert_eq!(choose(&none, None, None), ("claude".into(), None));
    }
}
