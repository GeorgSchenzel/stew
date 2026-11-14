//! Remote URLs. `git@host:a/b.git`, `ssh://git@host/a/b` and `https://host/a/b.git` are
//! the same repo; clones always get the https form, so the box's token helpers apply.
//! Local remotes (an absolute path, `file://`) stay as they are, `.git` included.

use crate::error::{Result, fail};

/// Whether a repo argument is a URL rather than a catalog name (names have no `:` and
/// never start with `/` or `.`).
pub fn looks_like_url(s: &str) -> bool {
    s.contains(':') || s.starts_with('/') || s.starts_with('.')
}

/// The https form of a remote.
pub fn normalize(url: &str) -> Result<String> {
    let url = url.trim();
    let bad = || fail!("not a git remote URL: '{url}'");
    if url.starts_with('/') || url.starts_with("file://") {
        return Ok(url.trim_end_matches('/').to_string());
    }
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        let keep_port = match scheme.to_ascii_lowercase().as_str() {
            "https" | "http" => true,
            "ssh" | "git" | "git+ssh" | "ssh+git" => false,
            _ => return Err(bad()),
        };
        let (authority, path) = rest.split_once('/').ok_or_else(bad)?;
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        // an ssh port says nothing about the https one
        let host = match host.rsplit_once(':') {
            Some((h, _)) if !keep_port => h,
            _ => host,
        };
        (host, path)
    } else {
        // scp syntax: [user@]host:path, with no '/' before the ':'
        let (authority, path) = url.split_once(':').ok_or_else(bad)?;
        if authority.contains('/') {
            return Err(bad());
        }
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        (host, path)
    };
    let path = trim_path(path.trim_start_matches('/'));
    if host.is_empty() || path.is_empty() || host.contains(char::is_whitespace) {
        return Err(bad());
    }
    Ok(format!("https://{}/{path}", host.to_ascii_lowercase()))
}

fn trim_path(p: &str) -> &str {
    let p = p.trim_end_matches('/');
    p.strip_suffix(".git").unwrap_or(p).trim_end_matches('/')
}

/// Whether two remotes are the same repo (git hosts compare paths case-insensitively).
pub fn same_repo(a: &str, b: &str) -> bool {
    match (normalize(a), normalize(b)) {
        (Ok(a), Ok(b)) => a.eq_ignore_ascii_case(&b),
        _ => false,
    }
}

/// The last path component of a remote, without `.git`: the default clone dir.
pub fn repo_name(url: &str) -> &str {
    let p = trim_path(url);
    p.rsplit(['/', ':']).next().unwrap_or(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_agree() {
        let want = "https://github.com/GeorgSchenzel/bento";
        for u in [
            "git@github.com:GeorgSchenzel/bento.git",
            "git@GitHub.com:GeorgSchenzel/bento",
            "ssh://git@github.com/GeorgSchenzel/bento.git",
            "ssh://git@github.com:22/GeorgSchenzel/bento.git",
            "https://github.com/GeorgSchenzel/bento",
            "https://github.com/GeorgSchenzel/bento.git/",
            "https://user@github.com/GeorgSchenzel/bento",
            "http://github.com/GeorgSchenzel/bento.git",
            "  git+ssh://github.com/GeorgSchenzel/bento  ",
        ] {
            assert_eq!(normalize(u).unwrap(), want, "{u}");
        }
        assert_eq!(
            normalize("git@gitlab.com:SchenzelGeorg/sub/group/x.git").unwrap(),
            "https://gitlab.com/SchenzelGeorg/sub/group/x"
        );
        assert_eq!(
            normalize("https://git.example.com:8443/a/b").unwrap(),
            "https://git.example.com:8443/a/b"
        );
    }

    #[test]
    fn local_stays() {
        assert_eq!(normalize("/srv/git/a.git/").unwrap(), "/srv/git/a.git");
        assert_eq!(
            normalize("file:///srv/git/a.git").unwrap(),
            "file:///srv/git/a.git"
        );
    }

    #[test]
    fn rejects() {
        for u in [
            "",
            "bento",
            "ftp://x/y",
            "https://host",
            "a/b:c",
            "git@host:",
        ] {
            assert!(normalize(u).is_err(), "{u}");
        }
    }

    #[test]
    fn same() {
        assert!(same_repo(
            "git@github.com:A/Bento.git",
            "https://github.com/a/bento"
        ));
        assert!(!same_repo(
            "git@github.com:a/bento",
            "git@github.com:a/forest"
        ));
    }

    #[test]
    fn names() {
        assert_eq!(repo_name("https://github.com/a/bento"), "bento");
        assert_eq!(repo_name("git@host:a/b.git"), "b");
        assert_eq!(repo_name("git@host:b.git"), "b");
        assert_eq!(repo_name("/srv/git/x.git/"), "x");
        assert!(
            looks_like_url("git@host:a/b") && looks_like_url("/x") && !looks_like_url("work/x")
        );
    }
}
