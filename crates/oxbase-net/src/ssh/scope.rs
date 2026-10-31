//! Sharing scopes: when an existing SSH session or tunnel may serve a new request.
//!
//! A session (and every tunnel on it) was host-key checked under one [`KnownHostsPolicy`] and
//! authenticated with one credential. Reusing it for a request with a different policy would
//! skip that request's host-key check (a `Strict` profile riding on a session accepted under
//! `AcceptNew`, or under another pin); reusing it for a request with a different credential would
//! skip authentication (a "test connection" with a wrong password succeeding because a session
//! with the right one is open). So sessions and pooled tunnels are shared only within one
//! [`Scope`]: same policy, same credential.
//!
//! Credentials are compared through HMAC-SHA256 tags under a random key that lives only in this
//! process (one key per cache). The tags never leave the cache, are never formatted (their
//! `Debug` is redacted), and cannot be checked against a guess without that key. Key files are
//! compared by path: whoever can replace the file can also read the session it opens.

use std::fmt;
use std::path::PathBuf;

use oxbase_types::{KeySource, KnownHostsPolicy, Secret, SshAuth, SshOptions};
use ring::hmac;

/// An HMAC-SHA256 tag of a secret under the owning [`Scoper`]'s key.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Tag(Vec<u8>);

impl fmt::Debug for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Tag(..)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum KeyRef {
    File(PathBuf),
    Inline(Tag),
}

/// Which credential a session authenticated with, without the credential itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum AuthId {
    Password(Tag),
    PrivateKey {
        key: KeyRef,
        passphrase: Option<Tag>,
    },
    /// The agent the provider is configured with: one per provider, so nothing to distinguish.
    Agent,
}

/// What, beyond endpoint and user, two requests must share to share a session or tunnel.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Scope {
    known_hosts: KnownHostsPolicy,
    auth: AuthId,
}

/// Computes [`Scope`]s under its own random HMAC key.
pub(crate) struct Scoper {
    key: hmac::Key,
}

impl Scoper {
    pub(crate) fn new() -> Self {
        let key = hmac::Key::generate(hmac::HMAC_SHA256, &ring::rand::SystemRandom::new())
            .unwrap_or_else(|_| {
                // No OS randomness (practically never). The tags then remain unguessable only
                // to the extent this fallback is; they still never leave the process.
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos());
                let seed = format!("{nanos}:{}", std::process::id());
                hmac::Key::new(hmac::HMAC_SHA256, seed.as_bytes())
            });
        Self { key }
    }

    pub(crate) fn scope(&self, ssh: &SshOptions) -> Scope {
        let auth = match &ssh.auth {
            SshAuth::Password(password) => AuthId::Password(self.tag(password)),
            SshAuth::PrivateKey { key, passphrase } => AuthId::PrivateKey {
                key: match key {
                    KeySource::File(path) => KeyRef::File(path.clone()),
                    KeySource::Pem(pem) => KeyRef::Inline(self.tag(pem)),
                },
                passphrase: passphrase.as_ref().map(|p| self.tag(p)),
            },
            SshAuth::Agent => AuthId::Agent,
        };
        Scope {
            known_hosts: ssh.known_hosts.clone(),
            auth,
        }
    }

    fn tag(&self, secret: &Secret) -> Tag {
        Tag(hmac::sign(&self.key, secret.expose().as_bytes())
            .as_ref()
            .to_vec())
    }
}

impl Default for Scoper {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Scoper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Scoper(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh(auth: SshAuth) -> SshOptions {
        SshOptions::new("bastion", "ops", auth)
    }

    fn password(p: &str) -> SshAuth {
        SshAuth::Password(Secret::new(p))
    }

    #[test]
    fn scopes_separate_policies_and_credentials() {
        let scoper = Scoper::new();
        let a = scoper.scope(&ssh(password("pw-scope-1")));
        assert_eq!(a, scoper.scope(&ssh(password("pw-scope-1"))));
        assert_ne!(a, scoper.scope(&ssh(password("pw-scope-2"))));
        assert_ne!(a, scoper.scope(&ssh(SshAuth::Agent)));
        let pinned = ssh(password("pw-scope-1")).with_known_hosts(KnownHostsPolicy::Fingerprint {
            fingerprint: "SHA256:x".into(),
        });
        assert_ne!(a, scoper.scope(&pinned));

        let key = |pem: &str, pass: Option<&str>| {
            scoper.scope(&ssh(SshAuth::PrivateKey {
                key: KeySource::Pem(Secret::new(pem)),
                passphrase: pass.map(Secret::new),
            }))
        };
        assert_eq!(key("k1", Some("p")), key("k1", Some("p")));
        assert_ne!(key("k1", Some("p")), key("k1", Some("q")));
        assert_ne!(key("k1", None), key("k2", None));

        // Another scoper (another cache) uses another key.
        assert_ne!(a, Scoper::new().scope(&ssh(password("pw-scope-1"))));
        // Nothing secret in Debug output.
        let text = format!("{a:?} {:?} {scoper:?}", key("k1-inline", Some("p-xyz")));
        assert!(
            !text.contains("pw-scope-1") && !text.contains("k1-inline"),
            "{text}"
        );
        assert!(!text.contains("p-xyz"), "{text}");
    }
}
