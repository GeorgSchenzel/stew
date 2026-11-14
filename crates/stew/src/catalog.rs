//! The catalog: repo names instead of URLs. `~/.config/stew/repos.yaml` is the source,
//! nested in groups (a mapping is a group, a string a repo's remote):
//!
//! ```yaml
//! bento: git@github.com:GeorgSchenzel/bento.git
//! work:
//!   gitlab-a:
//!     service-x: git@gitlab.a.example:team/service-x.git
//! ```
//!
//! plus, where it exists, every clone in the scan dir (`~/projects/repos/`), named after
//! its dir, with its `origin`. A name is the path through the groups
//! (`work/gitlab-a/service-x`); any trailing part of it (`service-x`) works when unique.

use std::fs;
use std::path::Path;

use serde_yaml::Value;
use stew_proto::Repo;

use crate::config::Config;
use crate::error::{Result, fail};
use crate::git::git;
use crate::paths::Paths;
use crate::url::{looks_like_url, normalize, repo_name, same_repo};

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The path through the groups.
    pub name: String,
    /// The https form.
    pub remote: String,
}

impl Entry {
    /// The clone's dir in a stew: the name's last part.
    pub fn dir(&self) -> &str {
        self.name.rsplit('/').next().unwrap_or(&self.name)
    }
}

#[derive(Debug, Default)]
pub struct Catalog {
    pub entries: Vec<Entry>,
    /// Scanned clones that were left out, and why.
    pub warnings: Vec<String>,
}

impl Catalog {
    pub fn load(paths: &Paths, config: &Config) -> Result<Self> {
        let file = paths.repos_file();
        let mut cat = match fs::read_to_string(&file) {
            Ok(text) => Catalog::parse(&text).map_err(|e| fail!("{}: {e}", file.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Catalog::default(),
            Err(e) => return Err(fail!("cannot read {}: {e}", file.display())),
        };
        if let Some(dir) = config.scan_dir(paths) {
            cat.scan(&dir);
        }
        Ok(cat)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let value: Value = serde_yaml::from_str(text).map_err(|e| fail!("{e}"))?;
        let mut cat = Catalog::default();
        match value {
            Value::Null => {}
            Value::Mapping(_) => walk("", &value, &mut cat.entries)?,
            _ => return Err(fail!("expected a mapping of names to remotes and groups")),
        }
        Ok(cat)
    }

    /// Adds each clone in `dir` by its dir name and `origin`, unless the catalog already
    /// has that remote or that name.
    pub fn scan(&mut self, dir: &Path) {
        let Ok(read) = fs::read_dir(dir) else {
            self.warnings.push(format!("cannot read {}", dir.display()));
            return;
        };
        let mut found: Vec<_> = read.flatten().map(|e| e.path()).collect();
        found.sort();
        for path in found {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.') || !path.join(".git").exists() {
                continue;
            }
            // `remote get-url` only reads config: no hooks, no fsmonitor
            let Ok(origin) = git(&path, ["remote", "get-url", "origin"]) else {
                continue;
            };
            let Ok(remote) = normalize(&origin) else {
                self.warnings.push(format!(
                    "{}: origin '{origin}' is not a URL",
                    path.display()
                ));
                continue;
            };
            if self.entries.iter().any(|e| same_repo(&e.remote, &remote)) {
                continue;
            }
            if self.entries.iter().any(|e| e.name == name) {
                self.warnings.push(format!(
                    "{}: the name '{name}' is taken in repos.yaml, left out",
                    path.display()
                ));
                continue;
            }
            self.entries.push(Entry {
                name: name.to_string(),
                remote,
            });
        }
    }

    /// A repo by name (the full path wins, else a unique trailing part) or by URL.
    pub fn find(&self, arg: &str) -> Result<Entry> {
        if looks_like_url(arg) {
            let remote = normalize(arg)?;
            if let Some(e) = self.entries.iter().find(|e| same_repo(&e.remote, &remote)) {
                return Ok(e.clone());
            }
            return Ok(Entry {
                name: repo_name(&remote).to_string(),
                remote,
            });
        }
        let arg = arg.trim_matches('/');
        if let Some(e) = self.entries.iter().find(|e| e.name == arg) {
            return Ok(e.clone());
        }
        let suffix = format!("/{arg}");
        let hits: Vec<_> = self
            .entries
            .iter()
            .filter(|e| e.name.ends_with(&suffix))
            .collect();
        match hits.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(fail!(
                "no repo '{arg}' in the catalog (`stew repos` lists it)"
            )),
            many => Err(fail!(
                "'{arg}' is ambiguous: {}",
                many.iter()
                    .map(|e| e.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    pub fn repos(&self) -> Vec<Repo> {
        self.entries
            .iter()
            .map(|e| Repo {
                name: e.name.clone(),
                remote: e.remote.clone(),
            })
            .collect()
    }
}

fn walk(prefix: &str, value: &Value, out: &mut Vec<Entry>) -> Result<()> {
    let Value::Mapping(map) = value else {
        unreachable!()
    };
    for (k, v) in map {
        let key = match k {
            Value::String(s) => s.clone(),
            other => serde_yaml::to_string(other)
                .unwrap_or_default()
                .trim()
                .to_string(),
        };
        if key.is_empty() || key.starts_with('.') || key.contains(['/', ':', ' ']) {
            return Err(fail!(
                "'{prefix}{key}': names are plain words (no '/', ':' or spaces)"
            ));
        }
        let name = format!("{prefix}{key}");
        match v {
            Value::String(url) => {
                let remote = normalize(url).map_err(|e| fail!("{name}: {e}"))?;
                out.push(Entry { name, remote });
            }
            Value::Mapping(_) => walk(&format!("{name}/"), v, out)?,
            _ => return Err(fail!("{name}: expected a remote URL or a group")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const YAML: &str = "
bento: git@github.com:GeorgSchenzel/bento.git
forest: https://github.com/GeorgSchenzel/forest
work:
  gitlab-a:
    service-x: git@gitlab.a.example:team/service-x.git
  gitlab-b:
    service-x: git@gitlab.b.example:team/service-x.git
    bento: https://gitlab.b.example/fork/bento
";

    #[test]
    fn names_and_groups() {
        let cat = Catalog::parse(YAML).unwrap();
        let names: Vec<_> = cat.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "bento",
                "forest",
                "work/gitlab-a/service-x",
                "work/gitlab-b/service-x",
                "work/gitlab-b/bento"
            ]
        );
        assert_eq!(
            cat.entries[0].remote,
            "https://github.com/GeorgSchenzel/bento"
        );
        assert_eq!(cat.entries[2].dir(), "service-x");
    }

    #[test]
    fn find() {
        let cat = Catalog::parse(YAML).unwrap();
        assert_eq!(
            cat.find("bento").unwrap().remote,
            "https://github.com/GeorgSchenzel/bento"
        );
        assert_eq!(
            cat.find("gitlab-b/bento").unwrap().name,
            "work/gitlab-b/bento"
        );
        assert_eq!(
            cat.find("gitlab-a/service-x").unwrap().name,
            "work/gitlab-a/service-x"
        );
        let e = cat.find("service-x").unwrap_err().to_string();
        assert!(
            e.contains("ambiguous") && e.contains("work/gitlab-b/service-x"),
            "{e}"
        );
        assert!(cat.find("nope").is_err());
        assert_eq!(
            cat.find("git@github.com:georgschenzel/forest.git")
                .unwrap()
                .name,
            "forest"
        );
        let u = cat.find("git@host:x/other.git").unwrap();
        assert_eq!(
            (u.name.as_str(), u.remote.as_str()),
            ("other", "https://host/x/other")
        );
    }

    #[test]
    fn bad_files() {
        assert!(Catalog::parse("- a\n- b\n").is_err());
        assert!(Catalog::parse("a: 3\n").is_err());
        assert!(Catalog::parse("a/b: git@h:x/y\n").is_err());
        assert!(Catalog::parse("a: notaurl\n").is_err());
        assert!(Catalog::parse("").unwrap().entries.is_empty());
    }
}
