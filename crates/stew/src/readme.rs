//! `notes/README.md` and `notes/LOG.md`: a stew's state and history, plain markdown kept by
//! its agents. stew writes the first version of both in the claim commit; `ls` shows
//! README as it is, unparsed.

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
    }
}
