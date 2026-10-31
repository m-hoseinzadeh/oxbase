//! Server version numbers.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// A server version such as `16.2`, `8.0.36`, `10.11.6` or `16.0.1000.6`.
///
/// Equality, ordering and hashing use only the numeric parts
/// (`major.minor.patch.build`); the original banner text is kept in [`ServerVersion::raw`] for
/// display and for engine-specific sniffing (for example the `-MariaDB` suffix).
///
/// `raw` is set at construction and is not updated if the public numeric fields are changed
/// afterwards; use `Display` for the canonical numeric form. A version deserialized without
/// `raw` gets the canonical form.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "VersionRepr")]
pub struct ServerVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    /// Fourth component (SQL Server build numbers); 0 when absent.
    #[serde(default)]
    pub build: u32,
    /// The text this version was parsed from, or the canonical form for constructed versions.
    raw: String,
}

/// Deserialization shape of [`ServerVersion`]: fills `raw` when it is missing or empty.
#[derive(Deserialize)]
struct VersionRepr {
    major: u32,
    minor: u32,
    patch: u32,
    #[serde(default)]
    build: u32,
    #[serde(default)]
    raw: String,
}

impl From<VersionRepr> for ServerVersion {
    fn from(r: VersionRepr) -> Self {
        let mut v = Self::with_build(r.major, r.minor, r.patch, r.build);
        if !r.raw.is_empty() {
            v.raw = r.raw;
        }
        v
    }
}

impl ServerVersion {
    /// A version with no build component.
    pub fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self::with_build(major, minor, patch, 0)
    }

    pub fn with_build(major: u32, minor: u32, patch: u32, build: u32) -> Self {
        let mut v = Self {
            major,
            minor,
            patch,
            build,
            raw: String::new(),
        };
        v.raw = v.to_string();
        v
    }

    /// Parses a version out of a server banner.
    ///
    /// The first numeric run that either contains a dot (`8.0.36`) or is directly followed by a
    /// pre-release tag (`16beta1`) is taken, so product years such as "SQL Server 2022" are
    /// skipped. The `5.5.5-` prefix MariaDB sends over the MySQL protocol is stripped.
    ///
    /// ```
    /// use oxbase_types::ServerVersion;
    /// let v = ServerVersion::parse("PostgreSQL 16.2 (Debian 16.2-1.pgdg120+2) on x86_64").unwrap();
    /// assert_eq!((v.major, v.minor, v.patch), (16, 2, 0));
    /// let v = ServerVersion::parse("Microsoft SQL Server 2022 (RTM) - 16.0.1000.6 (X64)").unwrap();
    /// assert_eq!(v, ServerVersion::with_build(16, 0, 1000, 6));
    /// ```
    pub fn parse(text: &str) -> Result<Self> {
        let trimmed = text.trim();
        let body = match trimmed.strip_prefix("5.5.5-") {
            Some(rest) if rest.contains("MariaDB") => rest,
            _ => trimmed,
        };
        let parts = find_version(body)
            .ok_or_else(|| Error::invalid_input(format!("no version number in '{text}'")))?;
        let mut v = Self::with_build(parts[0], parts[1], parts[2], parts[3]);
        trimmed.clone_into(&mut v.raw);
        Ok(v)
    }

    /// The text this version was parsed from.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// `true` when this version is `>= major.minor`.
    pub fn at_least(&self, major: u32, minor: u32) -> bool {
        (self.major, self.minor) >= (major, minor)
    }

    /// `true` when this version is `>= major.minor.patch`.
    pub fn at_least_patch(&self, major: u32, minor: u32, patch: u32) -> bool {
        (self.major, self.minor, self.patch) >= (major, minor, patch)
    }

    /// `true` when this version is `>= other` (all four components).
    pub fn at_least_version(&self, other: &ServerVersion) -> bool {
        self >= other
    }

    /// `true` when this version is `< major.minor`.
    pub fn before(&self, major: u32, minor: u32) -> bool {
        !self.at_least(major, minor)
    }

    fn key(&self) -> (u32, u32, u32, u32) {
        (self.major, self.minor, self.patch, self.build)
    }
}

/// Finds the first version-looking numeric run and returns up to four components.
fn find_version(s: &str) -> Option<[u32; 4]> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip digits glued to a word (`x86_64`, `ubu2204`), but allow a `v` prefix (`v7.0.2`).
        let glued = i > 0
            && bytes[i - 1].is_ascii_alphanumeric()
            && !(matches!(bytes[i - 1], b'v' | b'V')
                && (i < 2 || !bytes[i - 2].is_ascii_alphanumeric()));
        if !bytes[i].is_ascii_digit() || glued {
            i += 1;
            continue;
        }
        // Scan `digits(.digits)*` starting at i.
        let start = i;
        let mut end = i;
        let mut dots = 0;
        while end < bytes.len() {
            if bytes[end].is_ascii_digit() {
                end += 1;
            } else if bytes[end] == b'.'
                && end + 1 < bytes.len()
                && bytes[end + 1].is_ascii_digit()
                && dots < 3
            {
                dots += 1;
                end += 1;
            } else {
                break;
            }
        }
        let followed_by_tag = bytes.get(end).is_some_and(u8::is_ascii_alphabetic);
        if dots > 0 || followed_by_tag {
            let mut parts = [0u32; 4];
            let mut ok = true;
            for (slot, piece) in parts.iter_mut().zip(s[start..end].split('.')) {
                if let Ok(n) = piece.parse() {
                    *slot = n;
                } else {
                    // A component overflowed u32: not a version, try the next candidate.
                    ok = false;
                    break;
                }
            }
            if ok {
                return Some(parts);
            }
        }
        i = end;
    }
    None
}

impl PartialEq for ServerVersion {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for ServerVersion {}

impl PartialOrd for ServerVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ServerVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}

impl Hash for ServerVersion {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

impl fmt::Display for ServerVersion {
    /// Canonical numeric form: `major.minor.patch`, plus `.build` when non-zero.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if self.build != 0 {
            write!(f, ".{}", self.build)?;
        }
        Ok(())
    }
}

impl FromStr for ServerVersion {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn p(s: &str) -> ServerVersion {
        ServerVersion::parse(s).unwrap()
    }

    #[test]
    fn parses_banners() {
        assert_eq!(p("8.0.36"), ServerVersion::new(8, 0, 36));
        assert_eq!(p("8.4.0-debug"), ServerVersion::new(8, 4, 0));
        assert_eq!(
            p("10.11.6-MariaDB-1:10.11.6+maria~ubu2204"),
            ServerVersion::new(10, 11, 6)
        );
        assert_eq!(
            p("5.5.5-10.11.6-MariaDB-log"),
            ServerVersion::new(10, 11, 6)
        );
        assert_eq!(p("3.45.1"), ServerVersion::new(3, 45, 1));
        assert_eq!(p("v7.0.2"), ServerVersion::new(7, 0, 2));
        assert_eq!(p("redis_version:7.2.4"), ServerVersion::new(7, 2, 4));
        assert_eq!(p("7.0.15"), ServerVersion::new(7, 0, 15));
        assert_eq!(
            p(
                "PostgreSQL 16.2 (Debian 16.2-1.pgdg120+2) on x86_64-pc-linux-gnu, compiled by gcc 12.2.0"
            ),
            ServerVersion::new(16, 2, 0)
        );
        assert_eq!(
            p(
                "PostgreSQL 17beta1 on x86_64-pc-linux-gnu, compiled by gcc (Debian 12.2.0-14) 12.2.0"
            ),
            ServerVersion::new(17, 0, 0)
        );
        assert_eq!(
            p("Microsoft SQL Server 2022 (RTM-CU12) (KB5033663) - 16.0.4115.5 (X64)"),
            ServerVersion::with_build(16, 0, 4115, 5)
        );
        assert_eq!(p("16.0.1000.6").to_string(), "16.0.1000.6");
        assert_eq!(p("  8.0.36  ").raw(), "8.0.36");
    }

    #[test]
    fn rejects_garbage() {
        assert!(ServerVersion::parse("").is_err());
        assert!(ServerVersion::parse("no digits here").is_err());
        assert!(ServerVersion::parse("SQL Server 2022").is_err());
        assert!(ServerVersion::parse("99999999999.1.2").is_err());
    }

    #[test]
    fn skips_overflowing_candidates() {
        assert_eq!(p("99999999999.1.2 then 8.0.1"), ServerVersion::new(8, 0, 1));
    }

    #[test]
    fn ordering_and_helpers() {
        let v = ServerVersion::new(8, 0, 36);
        assert!(v > ServerVersion::new(5, 7, 44));
        assert!(v < ServerVersion::new(8, 4, 0));
        assert!(v.at_least(8, 0));
        assert!(!v.at_least(8, 1));
        assert!(v.at_least_patch(8, 0, 36));
        assert!(!v.at_least_patch(8, 0, 37));
        assert!(v.before(8, 4));
        assert!(v.at_least_version(&ServerVersion::new(8, 0, 13)));
        // Raw text does not take part in equality.
        assert_eq!(p("8.0.36-log"), ServerVersion::new(8, 0, 36));
        let mut vs = [p("10.2"), p("9.6.24"), p("10.11.6"), p("10.2.1")];
        vs.sort();
        let shown: Vec<String> = vs.iter().map(ToString::to_string).collect();
        assert_eq!(shown, ["9.6.24", "10.2.0", "10.2.1", "10.11.6"]);
    }

    #[test]
    fn serde_round_trip() {
        let v = p("PostgreSQL 16.2");
        let json = serde_json::to_string(&v).unwrap();
        let back: ServerVersion = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
        assert_eq!(back.raw(), "PostgreSQL 16.2");
        // Missing or empty `raw` falls back to the canonical form.
        let back: ServerVersion =
            serde_json::from_str(r#"{"major":8,"minor":0,"patch":36}"#).unwrap();
        assert_eq!(back.raw(), "8.0.36");
        let back: ServerVersion =
            serde_json::from_str(r#"{"major":16,"minor":0,"patch":1000,"build":6,"raw":""}"#)
                .unwrap();
        assert_eq!(back.raw(), "16.0.1000.6");
    }
}
