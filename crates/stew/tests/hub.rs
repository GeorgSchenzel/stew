//! The hub's parts against real git remotes in tempdirs.

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use common::Env;
use stew::catalog::Catalog;
use stew::mirror::{Mirror, parse_meta};
use stew::refs::{resolve_new, resolve_open};
use stew_proto::{Here, RepoRecord, StewMeta};

fn branches(r: &[stew_proto::Resolved]) -> Vec<&str> {
    r.iter().map(|r| r.branch.as_str()).collect()
}

fn meta(title: &str) -> StewMeta {
    StewMeta {
        id: String::new(),
        title: title.into(),
        from: Some("parent-abcd".into()),
        created: "2026-11-14".into(),
        repos: vec![RepoRecord {
            name: "a".into(),
            remote: "https://h/x/a".into(),
            branch: "main".into(),
        }],
    }
}

#[test]
fn resolves_against_remotes() {
    let env = Env::new();
    let a = env.remote("a", &["main", "feat"], "main");
    let b = env.remote("b", &["dev"], "dev");
    let c = env.remote("c", &["master", "x"], "trunk");
    let cat = Catalog::parse(&format!(
        "a: {}\ngroup:\n  b: {}\n  c: {}\n",
        a.display(),
        b.display(),
        c.display()
    ))
    .unwrap();
    let repos: Vec<_> = ["a", "b", "group/c"]
        .iter()
        .map(|n| cat.find(n).unwrap())
        .collect();

    let (r, w) = resolve_new(&repos, None, None).unwrap();
    assert_eq!(branches(&r), ["main", "dev", "master"]);
    assert!(w.is_empty());
    assert_eq!(r[2].name, "group/c");
    assert_eq!(r[2].dir, "c");
    assert_eq!(r[0].remote, a.to_str().unwrap());

    let (r, w) = resolve_new(&repos, Some("feat"), None).unwrap();
    assert_eq!(branches(&r), ["feat", "dev", "master"]);
    assert_eq!(w.len(), 2, "{w:?}");

    let e = resolve_new(&repos, Some("nope"), None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("no repo has a branch 'nope'"), "{e}");

    let here = Here {
        remote: format!("{}/", b.display()),
        branch: "dev".into(),
    };
    let (r, _) = resolve_new(&repos, Some("feat"), None).unwrap();
    assert_eq!(r[1].branch, "dev");
    let e = resolve_new(&repos, Some("feat"), Some(&here))
        .unwrap_err()
        .to_string();
    assert!(e.contains("b has no branch 'feat'"), "{e}");

    let missing = Catalog::parse(&format!("gone: {}/nothing.git\n", env.base.display())).unwrap();
    let e = resolve_new(&[missing.find("gone").unwrap()], None, None)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("cannot reach") && e.contains("nothing.git"),
        "{e}"
    );

    let records = [
        RepoRecord {
            name: "a".into(),
            remote: r[0].remote.clone(),
            branch: "feat".into(),
        },
        RepoRecord {
            name: "b".into(),
            remote: r[1].remote.clone(),
            branch: "gone".into(),
        },
    ];
    let (r, w) = resolve_open(&records).unwrap();
    assert_eq!(branches(&r), ["feat", "dev"]);
    assert!(
        w[0].contains("'gone' is not on the remote, using dev"),
        "{w:?}"
    );
}

#[test]
fn claims_a_branch_and_retries_a_clash() {
    let env = Env::new();
    let remote = env.base.join("remotes/notes.git");
    env.git(
        &env.base,
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    let url = remote.to_str().unwrap().to_string();

    let mirror = Mirror::new(env.base.join("hub1/artifacts.git"), url.clone());
    let id = mirror
        .claim(&meta("Port forwarding"), Some("Do the thing.\n"))
        .unwrap();
    assert!(id.starts_with("port-forwarding-") && id.len() == 20, "{id}");

    let refname = format!("refs/heads/stew/{id}");
    assert_eq!(env.git(&remote, &["rev-list", "--count", &refname]), "1");
    assert_eq!(
        env.git(&remote, &["log", "-1", "--format=%s", &refname]),
        "created: Port forwarding"
    );
    assert_eq!(
        env.git(&remote, &["ls-tree", "--name-only", &refname]),
        "BRIEF.md\nstew.yaml"
    );
    assert_eq!(
        env.git(&remote, &["show", &format!("{refname}:BRIEF.md")]),
        "Do the thing."
    );
    let m = parse_meta(&env.git(&remote, &["show", &format!("{refname}:stew.yaml")])).unwrap();
    assert_eq!(
        m,
        StewMeta {
            id: id.clone(),
            ..meta("Port forwarding")
        }
    );

    // a second hub that hasn't fetched: the push is rejected, the next id is used
    mirror
        .claim_with(&meta("x"), None, || Ok("x-aaaa".into()))
        .unwrap();
    let other = Mirror::new(env.base.join("hub2/artifacts.git"), url.clone());
    let mut ids = ["x-aaaa", "x-bbbb"].into_iter();
    let got = other
        .claim_with(&meta("x"), None, || Ok(ids.next().unwrap().into()))
        .unwrap();
    assert_eq!(got, "x-bbbb");
    assert_eq!(
        env.git(
            &remote,
            &["ls-tree", "--name-only", "refs/heads/stew/x-bbbb"]
        ),
        "stew.yaml"
    );
    // the first one's commit is untouched
    assert_eq!(
        env.git(&remote, &["rev-list", "--count", "refs/heads/stew/x-aaaa"]),
        "1"
    );

    // the hub that made it skips it without pushing
    let mut ids = ["x-aaaa", "x-cccc"].into_iter();
    assert_eq!(
        mirror
            .claim_with(&meta("x"), None, || Ok(ids.next().unwrap().into()))
            .unwrap(),
        "x-cccc"
    );

    // always taken: gives up
    let e = other
        .claim_with(&meta("x"), None, || Ok("x-aaaa".into()))
        .unwrap_err()
        .to_string();
    assert!(e.contains("no free id"), "{e}");

    // ids are checked
    assert!(
        other
            .claim_with(&meta("x"), None, || Ok("t42".into()))
            .is_err()
    );

    // a remote that doesn't exist is an error, not a retry
    let gone = Mirror::new(
        env.base.join("hub3/artifacts.git"),
        format!("{}/missing.git", env.base.display()),
    );
    let e = gone.claim(&meta("y"), None).unwrap_err().to_string();
    assert!(!e.contains("no free id"), "{e}");
}

#[test]
fn lists_tracked_stews() {
    let env = Env::new();
    let remote = env.base.join("remotes/notes.git");
    env.git(
        &env.base,
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    let url = remote.to_str().unwrap().to_string();
    let maker = Mirror::new(env.base.join("maker/artifacts.git"), url.clone());
    let old = maker.claim(&meta("Old work"), None).unwrap();
    let new = maker.claim(&meta("Zoo keeper"), None).unwrap();

    // an agent writes notes in `old` later
    let clone = env.base.join("clone");
    env.git(
        &env.base,
        &[
            "clone",
            "-q",
            "--single-branch",
            "-b",
            &format!("stew/{old}"),
            &url,
            "clone",
        ],
    );
    std::fs::write(clone.join("findings.md"), "The ZEBRA is striped.\n").unwrap();
    env.git(&clone, &["add", "findings.md"]);
    let later = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 100;
    let date = format!("@{later} +0000");
    env.git_env(
        &clone,
        &["commit", "-q", "-m", "B1 wip: findings"],
        &[("GIT_COMMITTER_DATE", &date), ("GIT_AUTHOR_DATE", &date)],
    );
    env.git(&clone, &["push", "-q"]);
    // and a branch without a readable stew.yaml
    env.git(&clone, &["checkout", "-q", "--orphan", "broken"]);
    std::fs::write(clone.join("stew.yaml"), "id: [\n").unwrap();
    env.git(&clone, &["add", "stew.yaml"]);
    env.git(&clone, &["commit", "-q", "-m", "broken"]);
    env.git(
        &clone,
        &["push", "-q", "origin", "HEAD:refs/heads/stew/broken-0000"],
    );

    let hub = Mirror::new(env.base.join("hub/artifacts.git"), url);
    hub.ensure().unwrap();
    hub.fetch().unwrap();
    let (all, warnings) = hub.tracked(None).unwrap();
    let ids: Vec<_> = all.iter().map(|t| t.meta.id.as_str()).collect();
    assert_eq!(ids[0], old, "most recent first: {ids:?}");
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&new.as_str()));
    assert_eq!(all[0].status, "B1 wip: findings");
    assert_eq!(all[0].updated, later as i64);
    assert_eq!(all[0].meta.title, "Old work");
    assert!(
        warnings.len() == 1 && warnings[0].contains("broken-0000"),
        "{warnings:?}"
    );

    let search = |term| {
        let (hits, _) = hub.tracked(Some(term)).unwrap();
        hits.into_iter().map(|t| t.meta.id).collect::<Vec<_>>()
    };
    assert_eq!(search("zebra"), [old.as_str()]);
    assert_eq!(search("zoo"), [new.as_str()]);
    assert_eq!(search(&new[new.len() - 4..]), [new.as_str()]);
    assert!(search("nothing like it").is_empty());
    assert_eq!(search("  ").len(), 2);

    // a deleted branch disappears at the next fetch
    env.git(
        &remote,
        &["update-ref", "-d", &format!("refs/heads/stew/{new}")],
    );
    hub.fetch().unwrap();
    assert_eq!(hub.tracked(None).unwrap().0.len(), 1);
}
