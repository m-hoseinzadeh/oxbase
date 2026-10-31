//! SSH host key verification: OpenSSH `known_hosts` files and pinned `SHA256:` fingerprints.
//!
//! The file format follows `sshd(8)`, "SSH_KNOWN_HOSTS FILE FORMAT": one key per line,
//! `[marker] hostpatterns keytype base64-key [comment]`. Host patterns are a comma-separated list
//! with `*` / `?` wildcards and `!` negation, or a single hashed name (`|1|salt|hash`, an
//! HMAC-SHA1 of the name keyed by the salt). Non-standard ports are written `[host]:port`.
//! `@revoked` lines reject a key outright. `@cert-authority` lines are skipped: host
//! certificates are not negotiated, so they never apply.
//!
//! Every function here does blocking file I/O; callers run them on `spawn_blocking`.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use oxbase_types::{Endpoint, Error, KnownHostsPolicy, Result};
use russh::keys::ssh_key::known_hosts::HostPatterns;
use russh::keys::{Algorithm, HashAlg, PublicKey};

/// Serialises read-check-append cycles on `known_hosts` files within this process, so two
/// concurrent first connections to the same new host write a single line.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// The OpenSSH `SHA256:<base64>` fingerprint of `key` (the form `ssh-keygen -l` prints).
pub(crate) fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// Unpadded base64 length of a SHA-256 digest.
const SHA256_B64_LEN: usize = 43;

/// Parses a pinned fingerprint into the canonical `SHA256:<unpadded base64>` form.
///
/// Tolerates surrounding whitespace, base64 padding, the prefix in any case or no prefix at all.
/// Anything that is not a SHA-256 fingerprint (an MD5 `aa:bb:..` one, a truncated paste) is
/// `Error::InvalidInput`, so it is not mistaken for a host-key mismatch.
///
/// Note: [`oxbase_types::SshOptions::validate`], which [`super::TunnelProvider::open`] runs
/// first, currently requires the value to start with `SHA256:` exactly, so through `open` only
/// trailing whitespace and padding are tolerated.
fn parse_fingerprint(fp: &str) -> Result<String> {
    let invalid = || {
        Error::invalid_input(
            "SSH host key fingerprint must be a SHA256 fingerprint (SHA256:<43 base64 \
             characters>, as printed by `ssh-keygen -l -E sha256 -f <host key>`)",
        )
    };
    let fp = fp.trim();
    let body = match fp.split_once(':') {
        Some((alg, rest)) if alg.trim().eq_ignore_ascii_case("SHA256") => rest,
        Some(_) => return Err(invalid()),
        None => fp,
    };
    let body = body.trim().trim_end_matches('=');
    let base64 = |b: u8| b.is_ascii_alphanumeric() || b == b'+' || b == b'/';
    if body.len() != SHA256_B64_LEN || !body.bytes().all(base64) {
        return Err(invalid());
    }
    Ok(format!("SHA256:{body}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Strict(PathBuf),
    AcceptNew(PathBuf),
    Fingerprint(String),
}

/// Checks one SSH server's host key against a [`KnownHostsPolicy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostKeyCheck {
    mode: Mode,
    /// The name looked up in and written to `known_hosts`: `host` or `[host]:port`.
    name: String,
    /// The SSH server as the user configured it, for messages.
    server: Endpoint,
}

impl HostKeyCheck {
    /// Resolves the policy for `host:port` (the default file is `~/.ssh/known_hosts`).
    pub(crate) fn new(policy: &KnownHostsPolicy, host: &str, port: u16) -> Result<Self> {
        let mode = match policy {
            KnownHostsPolicy::Strict { path } => Mode::Strict(resolve_path(path.as_deref())?),
            KnownHostsPolicy::AcceptNew { path } => Mode::AcceptNew(resolve_path(path.as_deref())?),
            KnownHostsPolicy::Fingerprint { fingerprint } => {
                Mode::Fingerprint(parse_fingerprint(fingerprint)?)
            }
        };
        Ok(Self {
            mode,
            name: known_hosts_name(host, port),
            server: Endpoint::new(host, port),
        })
    }

    /// Host key algorithms ordered so the ones already recorded for this host come first.
    ///
    /// OpenSSH does the same: otherwise a server that has both an ECDSA and an Ed25519 key, with
    /// only the ECDSA one in `known_hosts`, would present Ed25519 and look like a changed key.
    /// `None` keeps the library default (no file, nothing recorded, or a fingerprint pin).
    pub(crate) fn preferred_algorithms(&self, defaults: &[Algorithm]) -> Option<Vec<Algorithm>> {
        let path = match &self.mode {
            Mode::Strict(p) | Mode::AcceptNew(p) => p,
            Mode::Fingerprint(_) => return None,
        };
        let contents = read_optional(path).ok()?;
        let known: Vec<Algorithm> = entries_for(&contents, &self.name)
            .filter(|e| e.marker.is_none())
            .map(|e| e.key.algorithm())
            .collect();
        if known.is_empty() {
            return None;
        }
        let (mut first, rest): (Vec<_>, Vec<_>) = defaults
            .iter()
            .cloned()
            .partition(|alg| known.iter().any(|k| same_family(k, alg)));
        if first.is_empty() {
            return None;
        }
        first.extend(rest);
        Some(first)
    }

    /// Checks `key`; for `AcceptNew`, records an unknown host. Blocking (file I/O).
    ///
    /// Failures are `Error::Connection` whose message starts with `SSH host key` and names the
    /// presented fingerprint.
    pub(crate) fn verify(&self, key: &PublicKey) -> Result<()> {
        let presented = fingerprint(key);
        match &self.mode {
            Mode::Fingerprint(expected) => {
                if presented == *expected {
                    Ok(())
                } else {
                    Err(self.failure(
                        key,
                        &format!("does not match the pinned fingerprint {expected}"),
                    ))
                }
            }
            Mode::Strict(path) => {
                let contents = self.read(path, key)?;
                match lookup(&contents, &self.name, key) {
                    Lookup::Match => Ok(()),
                    Lookup::Unknown => Err(self.failure(
                        key,
                        &format!("is not in known_hosts file {}", path.display()),
                    )),
                    other => Err(self.mismatch(key, path, &other)),
                }
            }
            Mode::AcceptNew(path) => {
                // A poisoned lock only means another append panicked; the file is still usable.
                let _guard = WRITE_LOCK
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let contents = self.read(path, key)?;
                match lookup(&contents, &self.name, key) {
                    Lookup::Match => Ok(()),
                    Lookup::Unknown => {
                        match append(path, &self.name, key) {
                            Ok(()) => tracing::info!(
                                ssh = %self.server,
                                fingerprint = %presented,
                                file = %path.display(),
                                "trusting new SSH host key on first use"
                            ),
                            // Like OpenSSH: accept, but warn that the key was not remembered.
                            Err(e) => tracing::warn!(
                                ssh = %self.server,
                                fingerprint = %presented,
                                file = %path.display(),
                                error = %e,
                                "could not record the new SSH host key"
                            ),
                        }
                        Ok(())
                    }
                    other => Err(self.mismatch(key, path, &other)),
                }
            }
        }
    }

    /// The SSH server this check is for.
    pub(crate) fn server(&self) -> &Endpoint {
        &self.server
    }

    fn read(&self, path: &Path, key: &PublicKey) -> Result<String> {
        read_optional(path).map_err(|e| {
            self.failure(
                key,
                &format!(
                    "could not be checked: cannot read known_hosts file {}: {e}",
                    path.display()
                ),
            )
        })
    }

    fn mismatch(&self, key: &PublicKey, path: &Path, lookup: &Lookup) -> Error {
        let detail = match lookup {
            Lookup::Revoked { line } => {
                format!("is marked @revoked in {} line {line}", path.display())
            }
            Lookup::Mismatch { line } => format!(
                "does not match the key recorded in {} line {line}; the host key changed or \
                 someone may be intercepting the connection",
                path.display()
            ),
            Lookup::Match | Lookup::Unknown => String::from("was rejected"),
        };
        self.failure(key, &detail)
    }

    fn failure(&self, key: &PublicKey, detail: &str) -> Error {
        Error::connection(format!(
            "SSH host key for {} {detail} (presented {} key {})",
            self.server,
            key.algorithm().as_str(),
            fingerprint(key),
        ))
    }
}

/// `host` for port 22, `[host]:port` otherwise; host lowercased, IPv6 brackets removed.
pub(crate) fn known_hosts_name(host: &str, port: u16) -> String {
    let host = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if port == 22 {
        host
    } else {
        format!("[{host}]:{port}")
    }
}

fn resolve_path(path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = path {
        return Ok(p.to_owned());
    }
    std::env::home_dir()
        .filter(|h| !h.as_os_str().is_empty())
        .map(|h| h.join(".ssh").join("known_hosts"))
        .ok_or_else(|| {
            Error::invalid_input(
                "no home directory for the default known_hosts file; set a known_hosts path",
            )
        })
}

/// Reads the file; a missing file reads as empty.
fn read_optional(path: &Path) -> io::Result<String> {
    match fs::read(path) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

fn append(path: &Path, name: &str, key: &PublicKey) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let existing = read_optional(path)?;
    let key_text = key
        .to_openssh()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut line = String::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        line.push('\n');
    }
    line.push_str(name);
    line.push(' ');
    line.push_str(key_text.trim());
    line.push('\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    file.flush()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    Revoked,
}

#[derive(Debug)]
struct Entry {
    /// 1-based line number, for messages.
    line: usize,
    marker: Option<Marker>,
    key: PublicKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Lookup {
    Match,
    Unknown,
    /// The host is known with a different key.
    Mismatch {
        line: usize,
    },
    Revoked {
        line: usize,
    },
}

fn lookup(contents: &str, name: &str, key: &PublicKey) -> Lookup {
    let mut matched = false;
    let mut mismatch = None;
    for entry in entries_for(contents, name) {
        let same = entry.key.key_data() == key.key_data();
        match entry.marker {
            Some(Marker::Revoked) if same => return Lookup::Revoked { line: entry.line },
            Some(Marker::Revoked) => {}
            None if same => matched = true,
            None => {
                mismatch.get_or_insert(entry.line);
            }
        }
    }
    match (matched, mismatch) {
        (true, _) => Lookup::Match,
        (false, Some(line)) => Lookup::Mismatch { line },
        (false, None) => Lookup::Unknown,
    }
}

/// The parseable entries whose host patterns match `name`. Lines with unknown key types,
/// unknown markers or bad syntax are skipped, like OpenSSH does.
fn entries_for<'a>(contents: &'a str, name: &'a str) -> impl Iterator<Item = Entry> + 'a {
    contents.lines().enumerate().filter_map(move |(i, raw)| {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let mut fields = line.split_whitespace();
        let mut first = fields.next()?;
        let marker = if first.starts_with('@') {
            let marker = match first {
                "@revoked" => Marker::Revoked,
                // `@cert-authority` (host certificates are not negotiated) and unknown markers.
                _ => return None,
            };
            first = fields.next()?;
            Some(marker)
        } else {
            None
        };
        if !host_matches(first, name) {
            return None;
        }
        let key_type = fields.next()?;
        let key_b64 = fields.next()?;
        let key = PublicKey::from_openssh(&format!("{key_type} {key_b64}")).ok()?;
        Some(Entry {
            line: i + 1,
            marker,
            key,
        })
    })
}

/// Whether a known_hosts host-pattern field matches `name` (`host` or `[host]:port`).
fn host_matches(patterns: &str, name: &str) -> bool {
    if patterns.starts_with("|1|") {
        return match patterns.parse::<HostPatterns>() {
            Ok(HostPatterns::HashedName { salt, hash }) => {
                let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &salt);
                ring::hmac::verify(&key, name.as_bytes(), &hash).is_ok()
            }
            _ => false,
        };
    }
    let name = name.to_ascii_lowercase();
    let mut matched = false;
    for pattern in patterns.split(',').filter(|p| !p.is_empty()) {
        let (negated, pattern) = match pattern.strip_prefix('!') {
            Some(p) => (true, p),
            None => (false, pattern),
        };
        if glob_match(pattern.to_ascii_lowercase().as_bytes(), name.as_bytes()) {
            if negated {
                return false;
            }
            matched = true;
        }
    }
    matched
}

/// `*` (any run) and `?` (any one byte) wildcard match.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, t));
                p += 1;
            }
            Some(&c) if c == b'?' || c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    p = sp + 1;
                    t = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

/// Whether two algorithms use the same kind of host key (all RSA signature hashes are one key).
fn same_family(a: &Algorithm, b: &Algorithm) -> bool {
    match (a, b) {
        (Algorithm::Rsa { .. }, Algorithm::Rsa { .. }) => true,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_A: &str = include_str!("testdata/host_ed25519_a.pub");
    const HOST_B: &str = include_str!("testdata/host_ed25519_b.pub");
    const HOST_ECDSA: &str = include_str!("testdata/host_ecdsa.pub");

    fn key(text: &str) -> PublicKey {
        PublicKey::from_openssh(text.trim()).unwrap()
    }

    fn key_fields(text: &str) -> String {
        let mut f = text.split_whitespace();
        format!("{} {}", f.next().unwrap(), f.next().unwrap())
    }

    #[test]
    fn names_and_globs() {
        assert_eq!(known_hosts_name("Bastion.Example", 22), "bastion.example");
        assert_eq!(known_hosts_name("127.0.0.1", 15422), "[127.0.0.1]:15422");
        assert_eq!(known_hosts_name("[::1]", 2222), "[::1]:2222");
        assert!(glob_match(b"*.example.com", b"db.example.com"));
        assert!(glob_match(b"db?.lan", b"db1.lan"));
        assert!(!glob_match(b"db?.lan", b"db12.lan"));
        assert!(glob_match(b"*", b""));
        assert!(glob_match(b"a*b*c", b"axxbyyc"));
        assert!(!glob_match(b"a*b*c", b"axxbyy"));
        assert!(host_matches("foo,*.example.com", "db.example.com"));
        assert!(!host_matches(
            "*.example.com,!db.example.com",
            "db.example.com"
        ));
        assert!(host_matches("[127.0.0.1]:15422", "[127.0.0.1]:15422"));
        assert!(!host_matches("127.0.0.1", "[127.0.0.1]:15422"));
        assert!(host_matches("BASTION", "bastion"));
    }

    #[test]
    fn hashed_host_names() {
        // HMAC-SHA1 of "example.com" with this salt, as written by `ssh-keygen -H`.
        let salt = [7u8; 20];
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &salt);
        let tag = ring::hmac::sign(&key, b"example.com");
        let b64 = |b: &[u8]| {
            use russh::keys::ssh_key::known_hosts::HostPatterns as H;
            let mut hash = [0u8; 20];
            hash.copy_from_slice(b);
            H::HashedName {
                salt: salt.to_vec(),
                hash,
            }
            .to_string()
        };
        let pattern = b64(tag.as_ref());
        assert!(pattern.starts_with("|1|"));
        assert!(host_matches(&pattern, "example.com"));
        assert!(!host_matches(&pattern, "example.org"));
    }

    #[test]
    fn lookup_rules() {
        let a = key(HOST_A);
        let b = key(HOST_B);
        let file = format!(
            "# comment\n\n[127.0.0.1]:2222 {}\nother.host {}\n@cert-authority * {}\nbad line\n",
            key_fields(HOST_A),
            key_fields(HOST_B),
            key_fields(HOST_B),
        );
        assert_eq!(lookup(&file, "[127.0.0.1]:2222", &a), Lookup::Match);
        assert_eq!(
            lookup(&file, "[127.0.0.1]:2222", &b),
            Lookup::Mismatch { line: 3 }
        );
        assert_eq!(lookup(&file, "new.host", &a), Lookup::Unknown);
        let revoked = format!("{file}@revoked * {}\n", key_fields(HOST_B));
        assert_eq!(
            lookup(&revoked, "other.host", &b),
            Lookup::Revoked { line: 7 }
        );
        // Tabs and comments after the key are fine.
        let tabbed = format!("h1,h2\t{}\tsome comment\n", key_fields(HOST_A));
        assert_eq!(lookup(&tabbed, "h2", &a), Lookup::Match);
    }

    #[test]
    fn preferred_algorithms_put_known_types_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        fs::write(&path, format!("[h]:2222 {}\n", key_fields(HOST_ECDSA))).unwrap();
        let check = HostKeyCheck::new(
            &KnownHostsPolicy::Strict {
                path: Some(path.clone()),
            },
            "h",
            2222,
        )
        .unwrap();
        let defaults = russh::Preferred::DEFAULT.key.to_vec();
        let ordered = check.preferred_algorithms(&defaults).unwrap();
        assert_eq!(ordered.len(), defaults.len());
        assert_eq!(ordered[0], key(HOST_ECDSA).algorithm());
        let unknown = HostKeyCheck::new(&KnownHostsPolicy::Strict { path: Some(path) }, "x", 22);
        assert!(unknown.unwrap().preferred_algorithms(&defaults).is_none());
    }

    #[test]
    fn verify_policies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("known_hosts");
        let a = key(HOST_A);
        let b = key(HOST_B);
        let strict = |p: &Path| {
            HostKeyCheck::new(
                &KnownHostsPolicy::Strict {
                    path: Some(p.to_owned()),
                },
                "bastion",
                2222,
            )
            .unwrap()
        };
        // Strict with a missing file: unknown host.
        let err = strict(&path).verify(&a).unwrap_err();
        let msg = err.to_string();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::Connection);
        assert!(
            msg.contains("SSH host key for bastion:2222 is not in"),
            "{msg}"
        );
        assert!(msg.contains(&fingerprint(&a)), "{msg}");

        // AcceptNew learns the key (creating the directory), then a changed key fails.
        let tofu = HostKeyCheck::new(
            &KnownHostsPolicy::AcceptNew {
                path: Some(path.clone()),
            },
            "bastion",
            2222,
        )
        .unwrap();
        tofu.verify(&a).unwrap();
        tofu.verify(&a).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        assert_eq!(written.lines().count(), 1, "{written}");
        assert!(
            written.starts_with("[bastion]:2222 ssh-ed25519 "),
            "{written}"
        );
        strict(&path).verify(&a).unwrap();
        let msg = tofu.verify(&b).unwrap_err().to_string();
        assert!(msg.contains("does not match the key recorded in"), "{msg}");
        assert!(msg.contains(&fingerprint(&b)), "{msg}");
        let msg = strict(&path).verify(&b).unwrap_err().to_string();
        assert!(msg.contains("line 1"), "{msg}");

        // Fingerprint pins, with or without base64 padding, prefix case or prefix.
        let pin = |fp: &str| {
            HostKeyCheck::new(
                &KnownHostsPolicy::Fingerprint {
                    fingerprint: fp.to_owned(),
                },
                "bastion",
                22,
            )
            .unwrap()
        };
        pin(&fingerprint(&a)).verify(&a).unwrap();
        pin(&format!(" {}= ", fingerprint(&a))).verify(&a).unwrap();
        let body = fingerprint(&a)["SHA256:".len()..].to_owned();
        pin(&format!("sha256:{body}")).verify(&a).unwrap();
        pin(&body).verify(&a).unwrap();
        let msg = pin(&fingerprint(&a)).verify(&b).unwrap_err().to_string();
        assert!(
            msg.starts_with("connection error: SSH host key for bastion:22 does not match"),
            "{msg}"
        );
    }

    #[test]
    fn fingerprint_forms() {
        let fp = fingerprint(&key(HOST_A));
        let body = &fp["SHA256:".len()..];
        assert_eq!(body.len(), SHA256_B64_LEN);
        for ok in [
            fp.clone(),
            format!("{fp}="),
            format!("  {fp} \n"),
            format!("sha256:{body}"),
            format!("Sha256: {body}"),
            body.to_owned(),
        ] {
            assert_eq!(parse_fingerprint(&ok).unwrap(), fp, "{ok:?}");
        }
        for bad in [
            String::new(),
            "SHA256:".to_owned(),
            format!("SHA256:{}", &body[..20]),
            format!("SHA256:{body}A"),
            format!("SHA256:{}!", &body[..42]),
            "MD5:16:27:ac:a5:76:28:2d:36:63:1b:56:4d:eb:df:a6:48".to_owned(),
            "16:27:ac:a5:76:28:2d:36:63:1b:56:4d:eb:df:a6:48".to_owned(),
        ] {
            let err = HostKeyCheck::new(
                &KnownHostsPolicy::Fingerprint {
                    fingerprint: bad.clone(),
                },
                "h",
                22,
            )
            .unwrap_err();
            assert_eq!(
                err.category(),
                oxbase_types::ErrorCategory::InvalidInput,
                "{bad:?}"
            );
            assert!(err.to_string().contains("SHA256"), "{err}");
        }
    }

    #[test]
    fn append_keeps_existing_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        fs::write(&path, "other.host ssh-ed25519 AAAA-not-a-key").unwrap();
        let a = key(HOST_A);
        append(&path, "new.host", &a).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "other.host ssh-ed25519 AAAA-not-a-key");
        assert_eq!(lookup(&text, "new.host", &a), Lookup::Match);
    }
}
