//! Connection parameters.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use oxbase_types::{Engine, Error, Result};

/// A secret string (password, passphrase, token).
///
/// It never appears in `Debug` output, has no `Display` and no `Serialize`, so it cannot leak
/// into logs or saved documents by accident. Read it with [`Secret::expose`] only at the point of
/// use. On drop the buffer is overwritten (best effort, without `unsafe`).
#[derive(Clone, Default)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret text. Call only where the value is handed to a library or the network.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.fill(0);
        // Keep the zeroing store from being optimised away.
        std::hint::black_box(&bytes);
    }
}

/// How TLS is negotiated.
///
/// The default, [`TlsMode::Prefer`], matches libpq: it encrypts when the server offers TLS but
/// verifies nothing and silently falls back to plaintext. It is kept for parity and so local
/// servers without TLS work out of the box. The UI must show the effective mode on every
/// connection profile, and should steer remote connections to [`TlsMode::VerifyFull`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum TlsMode {
    /// Never use TLS.
    Disable,
    /// Use TLS if the server offers it, without verifying the certificate.
    #[default]
    Prefer,
    /// Require TLS, without verifying the certificate.
    Require,
    /// Require TLS and verify the certificate chain against the CA.
    VerifyCa,
    /// Require TLS, verify the chain and check the host name.
    VerifyFull,
}

impl TlsMode {
    /// `true` when the certificate chain must be verified.
    pub fn verifies_certificate(self) -> bool {
        matches!(self, Self::VerifyCa | Self::VerifyFull)
    }
}

/// TLS settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsOptions {
    pub mode: TlsMode,
    /// PEM file with CA certificates; `None` uses the platform trust store.
    pub ca_cert: Option<PathBuf>,
    /// PEM client certificate for mutual TLS.
    pub client_cert: Option<PathBuf>,
    /// PEM private key for `client_cert`.
    pub client_key: Option<PathBuf>,
    /// Name to verify instead of the host (SNI override).
    pub server_name: Option<String>,
}

/// How to authenticate to the SSH server.
#[derive(Debug, Clone)]
pub enum SshAuth {
    Password(Secret),
    PrivateKey {
        path: PathBuf,
        passphrase: Option<Secret>,
    },
    /// Use the running SSH agent.
    Agent,
}

/// An SSH tunnel to reach the database host.
#[derive(Debug, Clone)]
pub struct SshOptions {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    /// known_hosts file; `None` uses the user's default.
    pub known_hosts: Option<PathBuf>,
    /// Refuse unknown or changed host keys (the default).
    pub strict_host_key: bool,
}

impl SshOptions {
    pub fn new(host: impl Into<String>, user: impl Into<String>, auth: SshAuth) -> Self {
        Self {
            host: host.into(),
            port: 22,
            user: user.into(),
            auth,
            known_hosts: None,
            strict_host_key: true,
        }
    }
}

/// Everything a [`crate::Driver`] needs to open a connection.
///
/// Runtime-only: it holds secrets, so it is not serializable. Saved connection profiles live in
/// `oxbase-store` and refer to secrets by keychain id.
///
/// `Debug` never prints secrets: the password is a [`Secret`], and the values of
/// [`ConnectParams::options`] whose key looks sensitive (see [`is_sensitive_option`]) are shown as
/// `***`.
#[derive(Clone)]
pub struct ConnectParams {
    pub engine: Engine,
    pub host: Option<String>,
    /// `None` uses [`Engine::default_port`].
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<Secret>,
    /// Initial database (Redis: logical DB index as text).
    pub database: Option<String>,
    /// Database file for SQLite.
    pub file: Option<PathBuf>,
    pub tls: TlsOptions,
    pub ssh: Option<SshOptions>,
    pub connect_timeout: Option<Duration>,
    /// Reported to the server where supported (PostgreSQL `application_name`, ...).
    pub application_name: Option<String>,
    /// Engine-specific options passed through to the driver (`sslnegotiation`, `authSource`, ...).
    ///
    /// Do not put secrets here when a dedicated field exists. Values of keys that look sensitive
    /// are redacted from `Debug` output.
    pub options: BTreeMap<String, String>,
}

/// `true` if an option key may carry a secret (password, token, key, credential or a URI that can
/// embed credentials), compared case-insensitively. Such values are redacted from `Debug`.
pub fn is_sensitive_option(key: &str) -> bool {
    const MARKERS: [&str; 9] = [
        "pass",
        "pwd",
        "secret",
        "token",
        "key",
        "credential",
        "auth",
        "uri",
        "url",
    ];
    let key = key.to_ascii_lowercase();
    MARKERS.iter().any(|m| key.contains(m))
}

/// `Debug` view of [`ConnectParams::options`] with sensitive values redacted.
struct RedactedOptions<'a>(&'a BTreeMap<String, String>);

impl fmt::Debug for RedactedOptions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(k, v)| {
                let shown = if is_sensitive_option(k) {
                    "***"
                } else {
                    v.as_str()
                };
                (k, shown)
            }))
            .finish()
    }
}

impl fmt::Debug for ConnectParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectParams")
            .field("engine", &self.engine)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &self.password)
            .field("database", &self.database)
            .field("file", &self.file)
            .field("tls", &self.tls)
            .field("ssh", &self.ssh)
            .field("connect_timeout", &self.connect_timeout)
            .field("application_name", &self.application_name)
            .field("options", &RedactedOptions(&self.options))
            .finish()
    }
}

impl ConnectParams {
    /// Empty parameters for `engine`.
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            host: None,
            port: None,
            user: None,
            password: None,
            database: None,
            file: None,
            tls: TlsOptions::default(),
            ssh: None,
            connect_timeout: None,
            application_name: None,
            options: BTreeMap::new(),
        }
    }

    /// Parameters for a network engine at `host`.
    pub fn network(engine: Engine, host: impl Into<String>) -> Self {
        let mut p = Self::new(engine);
        p.host = Some(host.into());
        p
    }

    /// Parameters for a SQLite file.
    pub fn sqlite(file: impl Into<PathBuf>) -> Self {
        let mut p = Self::new(Engine::Sqlite);
        p.file = Some(file.into());
        p
    }

    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    #[must_use]
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    #[must_use]
    pub fn with_password(mut self, password: impl Into<Secret>) -> Self {
        self.password = Some(password.into());
        self
    }

    #[must_use]
    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.database = Some(database.into());
        self
    }

    #[must_use]
    pub fn with_tls(mut self, tls: TlsOptions) -> Self {
        self.tls = tls;
        self
    }

    #[must_use]
    pub fn with_ssh(mut self, ssh: SshOptions) -> Self {
        self.ssh = Some(ssh);
        self
    }

    /// The port to use: the explicit one or the engine default.
    pub fn effective_port(&self) -> Option<u16> {
        self.port.or(self.engine.default_port())
    }

    /// Checks the parameters make sense for the engine (a host for network engines, a file for
    /// SQLite, a client key with a client certificate).
    pub fn validate(&self) -> Result<()> {
        if self.engine.is_networked() {
            if self
                .host
                .as_deref()
                .map(str::trim)
                .is_none_or(str::is_empty)
            {
                return Err(Error::invalid_input(format!(
                    "{} needs a host",
                    self.engine.display_name()
                )));
            }
        } else if self
            .file
            .as_deref()
            .is_none_or(|p| p.as_os_str().is_empty())
        {
            return Err(Error::invalid_input("SQLite needs a database file"));
        }
        if self.tls.client_cert.is_some() != self.tls.client_key.is_some() {
            return Err(Error::invalid_input(
                "TLS client certificate and key must be given together",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_never_debug_prints() {
        let p = ConnectParams::network(Engine::Postgres, "db.local")
            .with_user("app")
            .with_password("hunter2-very-secret")
            .with_ssh(SshOptions::new(
                "bastion",
                "ops",
                SshAuth::PrivateKey {
                    path: "/home/ops/.ssh/id_ed25519".into(),
                    passphrase: Some(Secret::new("key-passphrase-xyz")),
                },
            ));
        let dbg = format!("{p:?} {p:#?}");
        assert!(!dbg.contains("hunter2"), "{dbg}");
        assert!(!dbg.contains("key-passphrase"), "{dbg}");
        assert!(dbg.contains("Secret(***)"));
        assert_eq!(
            p.password.as_ref().map(Secret::expose),
            Some("hunter2-very-secret")
        );
        let pw = SshAuth::Password(Secret::new("ssh-pw-123"));
        assert!(!format!("{pw:?}").contains("ssh-pw-123"));
    }

    #[test]
    fn sensitive_options_are_redacted() {
        let mut p = ConnectParams::network(Engine::MongoDb, "h");
        for (k, v) in [
            ("sslpassword", "pw-aaa"),
            ("URI", "mongodb://u:pw-bbb@h"),
            ("authToken", "pw-ccc"),
            ("sslkey", "pw-ddd"),
            ("Client_Secret", "pw-eee"),
            ("sslnegotiation", "direct"),
        ] {
            p.options.insert(k.into(), v.into());
        }
        let dbg = format!("{p:?} {p:#?}");
        assert!(!dbg.contains("pw-"), "{dbg}");
        assert!(dbg.contains("sslpassword"), "keys are still shown: {dbg}");
        assert!(dbg.contains("direct"), "harmless values are shown: {dbg}");
        assert!(is_sensitive_option("PGPASSWORD"));
        assert!(!is_sensitive_option("application_name"));
    }

    #[test]
    fn ports_and_validation() {
        let p = ConnectParams::network(Engine::MySql, "h");
        assert_eq!(p.effective_port(), Some(3306));
        assert_eq!(p.clone().with_port(3307).effective_port(), Some(3307));
        assert!(p.validate().is_ok());
        assert!(ConnectParams::new(Engine::Redis).validate().is_err());
        assert!(ConnectParams::new(Engine::Sqlite).validate().is_err());
        assert!(ConnectParams::sqlite("/tmp/x.db").validate().is_ok());
        assert!(ConnectParams::sqlite("").validate().is_err());
        assert!(
            ConnectParams::network(Engine::Postgres, "  ")
                .validate()
                .is_err()
        );
        assert!(
            ConnectParams::network(Engine::Postgres, "")
                .validate()
                .is_err()
        );
        let mut bad = ConnectParams::network(Engine::Postgres, "h");
        bad.tls.client_cert = Some("c.pem".into());
        assert!(bad.validate().is_err());
    }
}
