//! How both clients print stews, so `stew ls` on the host and `stew ls --from .` in a stew
//! read the same.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::{CreationInfo, Listed};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `text` without control characters (C0, C1, DEL) except newlines: titles, READMEs and
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

/// The short form of an id: its 4 random chars.
pub fn short(id: &str) -> &str {
    id.rsplit_once('-').map_or(id, |(_, s)| s)
}

/// One line per stew: title, short id, site, state, agents, age; with `long`, its README
/// indented under it. Then a line per creation in progress or failed.
pub fn table(stews: &[Listed], creations: &[CreationInfo], long: bool) -> String {
    let now = now();
    let rows: Vec<[String; 5]> = stews
        .iter()
        .map(|t| {
            let agents: Vec<String> = t
                .agents
                .iter()
                .map(|a| {
                    let name = a.address.rsplit_once('/').map_or(&*a.address, |(_, n)| n);
                    format!("{name} {}", a.status)
                })
                .collect();
            let age = if t.updated > 0 {
                age(now - t.updated)
            } else {
                "?".into()
            };
            [
                t.title.clone(),
                short(&t.id).to_string(),
                t.site.clone().unwrap_or_else(|| "-".into()),
                t.state.clone(),
                format!("{age:>4}  {}", agents.join(", ")),
            ]
        })
        .collect();
    let width = |i: usize| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0);
    let (w0, w1, w2, w3) = (width(0), width(1), width(2), width(3));
    let mut out = String::new();
    for (t, r) in stews.iter().zip(&rows) {
        let line = format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {}",
            r[0], r[1], r[2], r[3], r[4]
        );
        out.push_str(line.trim_end());
        out.push('\n');
        if long {
            for l in t.readme.lines() {
                out.push_str(format!("    {l}").trim_end());
                out.push('\n');
            }
            if !t.readme.is_empty() {
                out.push('\n');
            }
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
