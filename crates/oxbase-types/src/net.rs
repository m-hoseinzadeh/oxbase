//! Network addressing, TLS and SSH options shared by drivers (`oxbase-driver`), the network layer
//! (`oxbase-net`), storage (`oxbase-store`) and services.
//!
//! These are runtime types: the ones that can hold secrets ([`TlsOptions`], [`SshOptions`],
//! [`KeySource`], [`SshAuth`]) are not serializable. Saved profiles in `oxbase-store` use their
//! own serializable forms that refer to secrets by keychain key.
//!
//! # Addressing rule (see `docs/m1-contracts.md`)
//!
//! A connection has two addresses that must not be confused:
//!
//! - the **server endpoint**: the host and port the user configured. It is what an SSH tunnel
//!   forwards to, what TLS verifies the certificate against (unless
//!   [`TlsOptions::server_name`] overrides it) and what the UI shows;
//! - the **dial endpoint**: where the driver actually opens TCP sockets. Without SSH it is the
//!   server endpoint; through SSH it is the local end of the tunnel (`127.0.0.1:<ephemeral>`).
//!
//! Every socket of a session dials the dial endpoint, including out-of-band ones such as the
//! PostgreSQL cancel request or the MySQL `KILL QUERY` side connection.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{Error, Result, Secret};

/// A host (DNS name or IP literal) and a TCP port.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    /// The host parsed as an IP address, if it is an IP literal (`[::1]` brackets allowed).
    pub fn ip(&self) -> Option<IpAddr> {
        self.host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse()
            .ok()
    }

    /// The endpoint as a socket address, if the host is an IP literal (no DNS lookup).
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        self.ip().map(|ip| SocketAddr::new(ip, self.port))
    }
}

impl From<SocketAddr> for Endpoint {
    fn from(addr: SocketAddr) -> Self {
        Self::new(addr.ip().to_string(), addr.port())
    }
}

impl fmt::Display for Endpoint {
    /// `host:port`, with IPv6 literals in brackets.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ip() {
            Some(IpAddr::V6(ip)) => write!(f, "[{ip}]:{}", self.port),
            _ => write!(f, "{}:{}", self.host, self.port),
        }
    }
}

/// How TLS is negotiated.
///
/// The default, [`TlsMode::Prefer`], matches libpq: it encrypts when the server offers TLS but
/// verifies nothing and silently falls back to plaintext. It is kept for parity and so local
/// servers without TLS work out of the box. The UI must show the effective mode on every
/// connection profile, and should steer remote connections to [`TlsMode::VerifyFull`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// Never use TLS.
    Disable,
    /// Use TLS if the server offers it, without verifying the certificate. Falls back to
    /// plaintext when the server has no TLS. **Insecure** against an active attacker.
    #[default]
    Prefer,
    /// Require TLS, without verifying the certificate. Encrypts, but does not authenticate the
    /// server. **Insecure** against an active attacker.
    Require,
    /// Require TLS and verify the certificate chain against the CA, but not the host name.
    VerifyCa,
    /// Require TLS, verify the chain and check the server name (the default for remote hosts
    /// in the UI).
    VerifyFull,
}

impl TlsMode {
    /// `true` unless the mode is [`TlsMode::Disable`].
    pub fn uses_tls(self) -> bool {
        !matches!(self, Self::Disable)
    }

    /// `true` when the connection must fail rather than fall back to plaintext.
    pub fn requires_tls(self) -> bool {
        matches!(self, Self::Require | Self::VerifyCa | Self::VerifyFull)
    }

    /// `true` when the certificate chain must be verified.
    pub fn verifies_certificate(self) -> bool {
        matches!(self, Self::VerifyCa | Self::VerifyFull)
    }

    /// `true` when the certificate must also match the server name.
    pub fn verifies_hostname(self) -> bool {
        matches!(self, Self::VerifyFull)
    }

    /// Stable lowercase identifier (also the serde form).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify_ca",
            Self::VerifyFull => "verify_full",
        }
    }
}

impl fmt::Display for TlsMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Public PEM material (CA bundle, client certificate chain): a file or inline text.
#[derive(Clone, PartialEq, Eq)]
pub enum PemSource {
    /// A PEM file on disk, read when the TLS configuration is built.
    File(PathBuf),
    /// PEM text held in memory.
    Pem(String),
}

impl fmt::Debug for PemSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(p) => f.debug_tuple("File").field(p).finish(),
            Self::Pem(s) => write!(f, "Pem(<{} bytes>)", s.len()),
        }
    }
}

/// Private key material (TLS client key, SSH private key): a file or an inline secret.
///
/// Inline keys are [`Secret`]s, so they never appear in `Debug` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// A key file on disk, read at the point of use.
    File(PathBuf),
    /// PEM / OpenSSH key text held in memory (for example loaded from the keychain).
    Pem(Secret),
}

/// TLS settings for one connection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsOptions {
    pub mode: TlsMode,
    /// CA certificates to trust. `None` uses the platform trust store. When set, **only** these
    /// CAs are trusted for [`TlsMode::VerifyCa`] / [`TlsMode::VerifyFull`].
    pub ca_cert: Option<PemSource>,
    /// Client certificate chain (PEM) for mutual TLS. Must be given together with `client_key`.
    pub client_cert: Option<PemSource>,
    /// Unencrypted private key (PEM: PKCS#8, PKCS#1 or SEC1) for `client_cert`.
    pub client_key: Option<KeySource>,
    /// Name to send as SNI and verify the certificate against, instead of the server host.
    ///
    /// Use it when the certificate names differ from the host the user typed (for example the
    /// server is reached by IP). It is never the tunnel address.
    pub server_name: Option<String>,
}

impl TlsOptions {
    /// Options with the given mode and nothing else.
    pub fn new(mode: TlsMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_ca_cert(mut self, ca: PemSource) -> Self {
        self.ca_cert = Some(ca);
        self
    }

    #[must_use]
    pub fn with_client_identity(mut self, cert: PemSource, key: KeySource) -> Self {
        self.client_cert = Some(cert);
        self.client_key = Some(key);
        self
    }

    #[must_use]
    pub fn with_server_name(mut self, name: impl Into<String>) -> Self {
        self.server_name = Some(name.into());
        self
    }

    /// The name TLS verifies and sends as SNI: [`TlsOptions::server_name`] if set, otherwise
    /// `server_host` (the *server* endpoint host, never the tunnel/dial host).
    pub fn effective_server_name<'a>(&'a self, server_host: &'a str) -> &'a str {
        self.server_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(server_host)
    }

    /// Checks the options are consistent (client certificate and key given together).
    pub fn validate(&self) -> Result<()> {
        if self.client_cert.is_some() != self.client_key.is_some() {
            return Err(Error::invalid_input(
                "TLS client certificate and key must be given together",
            ));
        }
        Ok(())
    }
}

/// How to authenticate to the SSH server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshAuth {
    Password(Secret),
    PrivateKey {
        /// OpenSSH or PEM private key.
        key: KeySource,
        /// Passphrase for an encrypted key.
        passphrase: Option<Secret>,
    },
    /// Use the running SSH agent (`SSH_AUTH_SOCK` on Unix, the OpenSSH agent pipe on Windows).
    Agent,
}

/// How the SSH server's host key is checked.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum KnownHostsPolicy {
    /// Accept only keys already listed in the known_hosts file (`None`: the user's default,
    /// `~/.ssh/known_hosts`). An unknown or changed key fails the connection.
    Strict {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
    /// Trust on first use: an unknown host is accepted and appended to the file; a *changed*
    /// key still fails the connection.
    AcceptNew {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
    /// Accept exactly this key, given as an OpenSSH fingerprint (`SHA256:<base64>`); no file.
    Fingerprint { fingerprint: String },
}

impl Default for KnownHostsPolicy {
    fn default() -> Self {
        Self::Strict { path: None }
    }
}

/// An SSH tunnel to reach the database server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshOptions {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    pub known_hosts: KnownHostsPolicy,
    /// Deadline for TCP connect + SSH handshake + authentication. `None`: provider default.
    pub connect_timeout: Option<Duration>,
    /// SSH keepalive interval. `None`: provider default.
    pub keepalive_interval: Option<Duration>,
}

impl SshOptions {
    /// Options for `user@host:22` with strict host-key checking against the default file.
    pub fn new(host: impl Into<String>, user: impl Into<String>, auth: SshAuth) -> Self {
        Self {
            host: host.into(),
            port: 22,
            user: user.into(),
            auth,
            known_hosts: KnownHostsPolicy::default(),
            connect_timeout: None,
            keepalive_interval: None,
        }
    }

    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    #[must_use]
    pub fn with_known_hosts(mut self, policy: KnownHostsPolicy) -> Self {
        self.known_hosts = policy;
        self
    }

    /// The SSH server endpoint.
    pub fn endpoint(&self) -> Endpoint {
        Endpoint::new(self.host.clone(), self.port)
    }

    /// Checks the options make sense (host and user present, port non-zero).
    pub fn validate(&self) -> Result<()> {
        if self.host.trim().is_empty() {
            return Err(Error::invalid_input("SSH tunnel needs a host"));
        }
        if self.user.trim().is_empty() {
            return Err(Error::invalid_input("SSH tunnel needs a user"));
        }
        if self.port == 0 {
            return Err(Error::invalid_input("SSH port must not be 0"));
        }
        if let KnownHostsPolicy::Fingerprint { fingerprint } = &self.known_hosts
            && !fingerprint.starts_with("SHA256:")
        {
            return Err(Error::invalid_input(
                "SSH host key fingerprint must look like SHA256:<base64>",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_display_and_parse() {
        assert_eq!(Endpoint::new("db.local", 5432).to_string(), "db.local:5432");
        assert_eq!(Endpoint::new("::1", 5432).to_string(), "[::1]:5432");
        assert_eq!(Endpoint::new("[::1]", 5432).to_string(), "[::1]:5432");
        let sa: SocketAddr = "127.0.0.1:41234".parse().unwrap();
        let e = Endpoint::from(sa);
        assert_eq!(e.to_string(), "127.0.0.1:41234");
        assert_eq!(e.socket_addr(), Some(sa));
        assert_eq!(Endpoint::new("db.local", 1).socket_addr(), None);
    }

    #[test]
    fn tls_mode_semantics() {
        assert!(!TlsMode::Disable.uses_tls());
        assert!(TlsMode::Prefer.uses_tls() && !TlsMode::Prefer.requires_tls());
        assert!(TlsMode::Require.requires_tls() && !TlsMode::Require.verifies_certificate());
        assert!(TlsMode::VerifyCa.verifies_certificate() && !TlsMode::VerifyCa.verifies_hostname());
        assert!(TlsMode::VerifyFull.verifies_hostname());
        for m in [
            TlsMode::Disable,
            TlsMode::Prefer,
            TlsMode::Require,
            TlsMode::VerifyCa,
            TlsMode::VerifyFull,
        ] {
            let json = serde_json::to_string(&m).unwrap();
            assert_eq!(json, format!("\"{m}\""));
            assert_eq!(serde_json::from_str::<TlsMode>(&json).unwrap(), m);
        }
    }

    #[test]
    fn server_name_rule() {
        let t = TlsOptions::new(TlsMode::VerifyFull);
        assert_eq!(t.effective_server_name("db.example.com"), "db.example.com");
        let t = t.with_server_name("pg.internal");
        assert_eq!(t.effective_server_name("10.0.0.5"), "pg.internal");
        let blank = TlsOptions::new(TlsMode::VerifyFull).with_server_name("  ");
        assert_eq!(blank.effective_server_name("h"), "h");
    }

    #[test]
    fn secrets_never_debug_print() {
        let t = TlsOptions::new(TlsMode::VerifyFull).with_client_identity(
            PemSource::Pem("-----BEGIN CERTIFICATE-----cert-body".into()),
            KeySource::Pem(Secret::new("-----BEGIN PRIVATE KEY-----key-body")),
        );
        let ssh = SshOptions::new(
            "bastion",
            "ops",
            SshAuth::PrivateKey {
                key: KeySource::Pem(Secret::new("ssh-key-body")),
                passphrase: Some(Secret::new("pass-phrase")),
            },
        );
        let dbg = format!("{t:?} {t:#?} {ssh:?} {ssh:#?}");
        for leaked in ["key-body", "cert-body", "pass-phrase"] {
            assert!(!dbg.contains(leaked), "{leaked} leaked: {dbg}");
        }
        assert!(t.validate().is_ok());
        let mut half = TlsOptions::new(TlsMode::Require);
        half.client_cert = Some(PemSource::File("c.pem".into()));
        assert!(half.validate().is_err());
    }

    #[test]
    fn ssh_validation_and_policy_serde() {
        let ok = SshOptions::new("b", "u", SshAuth::Agent);
        assert_eq!(ok.port, 22);
        assert!(ok.validate().is_ok());
        assert!(
            SshOptions::new(" ", "u", SshAuth::Agent)
                .validate()
                .is_err()
        );
        assert!(SshOptions::new("b", "", SshAuth::Agent).validate().is_err());
        let bad_fp = ok.clone().with_known_hosts(KnownHostsPolicy::Fingerprint {
            fingerprint: "md5:aa".into(),
        });
        assert!(bad_fp.validate().is_err());
        let p = KnownHostsPolicy::AcceptNew {
            path: Some("/x/known_hosts".into()),
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, r#"{"policy":"accept_new","path":"/x/known_hosts"}"#);
        assert_eq!(serde_json::from_str::<KnownHostsPolicy>(&json).unwrap(), p);
        assert_eq!(
            serde_json::from_str::<KnownHostsPolicy>(r#"{"policy":"strict"}"#).unwrap(),
            KnownHostsPolicy::default()
        );
    }
}
