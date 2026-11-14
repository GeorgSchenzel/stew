//! Stew ids: `<slug>-<rand4>`, e.g. `port-forwarding-k3fq`. Lowercase, ≤32 chars, never
//! `t<digits>` (hc's terminal ids), so an id is also the hc machine name and the dir name.

use std::fs::File;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Result, fail};

/// The longest slug part of an id.
const SLUG_MAX: usize = 24;

/// The title's slug: lowercase ASCII letters and digits, everything else one dash, cut
/// to ~24 chars (at a dash when one is near the end). `stew` when nothing is left.
pub fn slug(title: &str) -> String {
    let mut s = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.is_empty() && !s.ends_with('-') {
            s.push('-');
        }
    }
    if s.len() > SLUG_MAX {
        s.truncate(SLUG_MAX);
        if let Some(i) = s.rfind('-').filter(|&i| i >= SLUG_MAX * 2 / 3) {
            s.truncate(i);
        }
    }
    let s = s.trim_end_matches('-');
    if s.is_empty() {
        "stew".into()
    } else {
        s.into()
    }
}

/// A fresh id for a title.
pub fn new_id(title: &str) -> Result<String> {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let suffix: String = random_bytes(4)?
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect();
    Ok(format!("{}-{suffix}", slug(title)))
}

/// Checks an id given by a caller (it becomes a branch, a dir and an hc machine name).
pub fn check(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-')
        && !id.contains("--")
        && !(id.starts_with('t') && id.len() > 1 && id[1..].bytes().all(|b| b.is_ascii_digit()));
    if ok {
        Ok(())
    } else {
        Err(fail!("not a stew id: '{id}'"))
    }
}

pub fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0; n];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| fail!("cannot read /dev/urandom: {e}"))?;
    Ok(buf)
}

/// A random hex token (box tokens).
pub fn token() -> Result<String> {
    Ok(random_bytes(16)?
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Today in UTC, `YYYY-MM-DD`.
pub fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    date(secs)
}

/// The UTC date of a unix time (Howard Hinnant's civil-from-days).
pub fn date(secs: i64) -> String {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("Port forwarding"), "port-forwarding");
        assert_eq!(slug("  fix: the `ls` --all bug!  "), "fix-the-ls-all-bug");
        assert_eq!(slug("Ünïcode wins"), "n-code-wins");
        assert_eq!(slug("!!!"), "stew");
        assert_eq!(slug("t12"), "t12");
        assert_eq!(
            slug("a very long title that keeps going on"),
            "a-very-long-title-that"
        );
        assert_eq!(
            slug("abcdefghijklmnopqrstuvwxyz0123"),
            "abcdefghijklmnopqrstuvwx"
        );
        assert!(slug("x".repeat(100).as_str()).len() <= SLUG_MAX);
    }

    #[test]
    fn ids() {
        for title in [
            "test",
            "a very long title that keeps going on and on",
            "t1",
            "...",
        ] {
            let id = new_id(title).unwrap();
            check(&id).unwrap();
            assert!(id.len() <= 32, "{id}");
            assert_eq!(id.len(), slug(title).len() + 5);
        }
        assert_ne!(new_id("x").unwrap(), new_id("x").unwrap());
        for bad in [
            "",
            "T-abcd",
            "t123",
            "a_b",
            "-a",
            "a-",
            "a--b",
            &"a".repeat(33),
        ] {
            assert!(check(bad).is_err(), "{bad}");
        }
        check("t1-abcd").unwrap();
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_763_078_400), "2025-11-14");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_798_675_200), "2026-12-31");
    }
}
