//! `notes/README.md` and `notes/LOG.md`: a stew's state and history, plain markdown kept by
//! its agents. stew writes the first version of both in the claim commit and reads
//! README's description and `## Now` for `ls`.

use stew_proto::StewMeta;

/// README.md at the claim: the title as heading and an empty Now.
pub fn initial_readme(meta: &StewMeta) -> String {
    format!("# {}\n\n## Now\n", meta.title)
}

/// LOG.md at the claim: `- <date> created[ from <from>]`.
pub fn initial_log(meta: &StewMeta) -> String {
    let from = meta
        .from
        .as_deref()
        .map(|f| format!(" from {f}"))
        .unwrap_or_default();
    format!("# Log\n- {} created{from}\n", meta.created)
}

/// What `ls` shows of a README: the paragraph under the heading, and the Now items.
#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    pub description: String,
    pub now: Vec<String>,
}

/// The description is the first paragraph after the `#` heading (before any `##`), its
/// lines joined. Now is everything under `## Now` up to the next heading: one item per
/// list entry (`-`, `*`, `+`; indented lines continue it) or per plain line.
pub fn summarize(text: &str) -> Summary {
    let mut s = Summary::default();
    let mut section = Section::Top;
    let mut para: Vec<&str> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        if let Some(h) = heading(line) {
            section = match (h.level, h.text.eq_ignore_ascii_case("now")) {
                (1, _) if section == Section::Top => Section::About,
                (2, true) => Section::Now,
                _ => Section::Other,
            };
            continue;
        }
        match section {
            Section::Top | Section::About => {
                if line.trim().is_empty() {
                    if !para.is_empty() && s.description.is_empty() {
                        s.description = para.join(" ");
                    }
                    para.clear();
                } else if s.description.is_empty() {
                    para.push(line.trim());
                }
            }
            Section::Now => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let item = ["- ", "* ", "+ "]
                    .iter()
                    .find_map(|m| trimmed.strip_prefix(m));
                let indented = raw.starts_with(' ') || raw.starts_with('\t');
                match (item, s.now.last_mut()) {
                    (Some(text), _) => s.now.push(text.trim().to_string()),
                    (None, Some(last)) if indented => {
                        last.push(' ');
                        last.push_str(trimmed);
                    }
                    (None, _) => s.now.push(trimmed.to_string()),
                }
            }
            Section::Other => {}
        }
    }
    if s.description.is_empty() && !para.is_empty() {
        s.description = para.join(" ");
    }
    s
}

#[derive(Clone, Copy, PartialEq)]
enum Section {
    /// Before the first heading.
    Top,
    /// Under the `#` heading.
    About,
    Now,
    Other,
}

struct Heading<'a> {
    level: usize,
    text: &'a str,
}

fn heading(line: &str) -> Option<Heading<'_>> {
    let level = line.bytes().take_while(|&b| b == b'#').count();
    let rest = &line[level..];
    (level > 0 && (rest.is_empty() || rest.starts_with(' '))).then(|| Heading {
        level,
        text: rest.trim(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(from: Option<&str>) -> StewMeta {
        StewMeta {
            id: "x-abcd".into(),
            title: "Port forwarding".into(),
            from: from.map(String::from),
            created: "2026-11-14".into(),
            state: None,
            closed: None,
            repos: vec![],
        }
    }

    #[test]
    fn initial_files() {
        assert_eq!(initial_readme(&meta(None)), "# Port forwarding\n\n## Now\n");
        assert_eq!(initial_log(&meta(None)), "# Log\n- 2026-11-14 created\n");
        assert_eq!(
            initial_log(&meta(Some("a-0000"))),
            "# Log\n- 2026-11-14 created from a-0000\n"
        );
        assert_eq!(summarize(&initial_readme(&meta(None))), Summary::default());
    }

    #[test]
    fn reads_a_readme() {
        let text = "# agents in forests

Coding agents that each get
their own workspace.

A second paragraph, not shown.

## Now
- bento: the interface is agreed;
  next, the box itself.
* waiting: Georg reviews A1
plain line
blocked: tokens

## Later
- not now
";
        let s = summarize(text);
        assert_eq!(
            s.description,
            "Coding agents that each get their own workspace."
        );
        assert_eq!(
            s.now,
            [
                "bento: the interface is agreed; next, the box itself.",
                "waiting: Georg reviews A1",
                "plain line",
                "blocked: tokens"
            ]
        );
    }

    #[test]
    fn odd_readmes() {
        assert_eq!(summarize(""), Summary::default());
        let s = summarize("just text\nno headings");
        assert_eq!(s.description, "just text no headings");
        let s = summarize("# T\n## now\n- a\n#hashtag is not a heading\n");
        assert_eq!(s.now, ["a", "#hashtag is not a heading"]);
        // a description after a ## section doesn't count
        let s = summarize("# T\n## Notes\nnot it\n## Now\n- a\n");
        assert_eq!((s.description.as_str(), s.now.len()), ("", 1));
    }
}
