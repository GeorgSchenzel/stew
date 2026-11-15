//! How both clients print stews, so `stew ls` on the host and `stew ls --from .` in a stew
//! read the same.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::{CreationInfo, Live};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `text` without control characters (C0, C1, DEL) except newlines: titles, Now lines and
/// agents' status are written in stews, and an escape sequence must not reach the terminal
/// of whoever lists them.
pub fn printable(text: &str) -> String {
    text.chars()
        .filter(|&c| c == '\n' || !c.is_control())
        .collect()
}

/// `now`, `5m`, `3h`, `2d`.
pub fn age(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// Per stew: id, title, site, state, age; then from, repos, agents and Now lines indented.
pub fn live_table(stews: &[Live], creations: &[CreationInfo]) -> String {
    let now = now();
    let w0 = stews.iter().map(|t| t.id.len()).max().unwrap_or(0);
    let w1 = stews
        .iter()
        .map(|t| t.title.chars().count())
        .max()
        .unwrap_or(0);
    let w2 = stews.iter().map(|t| t.site.len()).max().unwrap_or(0);
    let mut out = String::new();
    for t in stews {
        let age = if t.updated > 0 {
            age(now - t.updated)
        } else {
            "?".into()
        };
        let head = format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<11}  {:>4}",
            t.id, t.title, t.site, t.state, age
        );
        out.push_str(head.trim_end());
        out.push('\n');
        if let Some(f) = &t.from {
            out.push_str(&format!("    from {f}\n"));
        }
        if !t.repos.is_empty() {
            let repos: Vec<String> = t
                .repos
                .iter()
                .map(|r| {
                    if r.branch.is_empty() {
                        format!("{} (detached)", r.dir)
                    } else {
                        format!("{}@{}", r.dir, r.branch)
                    }
                })
                .collect();
            out.push_str(&format!("    {}\n", repos.join("  ")));
        }
        for a in &t.agents {
            out.push_str(&format!(
                "    agent {}  {}  {}\n",
                a.address, a.program, a.status
            ));
        }
        for line in &t.now {
            out.push_str(&format!("    - {line}\n"));
        }
    }
    for c in creations {
        let what = match &c.error {
            Some(e) => format!("creation {}: {e}", c.state),
            None => format!("creation {}", c.state),
        };
        out.push_str(&format!("{}  {what}\n", c.id));
    }
    printable(&out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn printable() {
        assert_eq!(
            super::printable("a\x1b]52;c;eA==\x07b\u{9b}2Jc\x7f\td\ne"),
            "a]52;c;eA==b2Jcd\ne"
        );
    }
}
