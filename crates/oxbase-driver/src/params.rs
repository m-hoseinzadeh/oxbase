//! Connection parameters.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use oxbase_types::{Endpoint, Engine, Error, Result, Secret, SshOptions, TlsOptions};

/// Everything a [`crate::Driver`] needs to open a connection.
///
/// Runtime-only: it holds secrets, so it is not serializable. Saved connection profiles live in
/// `oxbase-store` and refer to secrets by keychain id.
///
/// `Debug` never prints secrets: the password is a [`Secret`], and the values of
/// [`ConnectParams::options`] whose key looks sensitive (see [`is_sensitive_option`]) are shown as
/// `***`.
///
/// # Addressing (server endpoint vs dial endpoint)
///
/// `host` / `port` are the **server endpoint**: the logical server the user configured. Drivers
/// use it for TLS server-name verification and SNI (via [`ConnectParams::tls_server_name`]),
/// for display and in error messages. `dial_addr`, when set, is where sockets are actually
/// opened (the local end of an SSH tunnel). Drivers must:
///
/// - open **every** socket of the session at [`ConnectParams::dial_endpoint`], including
///   out-of-band ones (PostgreSQL cancel requests, the MySQL `KILL QUERY` side connection);
/// - never open SSH tunnels themselves and never dial `ssh` (the connection manager opens the
///   tunnel and sets `dial_addr`; [`ConnectParams::validate`] rejects `ssh` without it);
/// - verify TLS against [`ConnectParams::tls_server_name`], never against the dial address, so
///   `VerifyFull` keeps working through a tunnel.
#[derive(Clone)]
pub struct ConnectParams {
    pub engine: Engine,
    /// Server host (DNS name or IP literal) as configured by the user.
    pub host: Option<String>,
    /// Server port. `None` uses [`Engine::default_port`].
    pub port: Option<u16>,
    /// Where to open sockets instead of `host:port`. Set by the connection manager to the local
    /// address of the SSH tunnel; `None` dials the server endpoint directly.
    pub dial_addr: Option<SocketAddr>,
    pub user: Option<String>,
    pub password: Option<Secret>,
    /// Initial database (Redis: logical DB index as text).
    pub database: Option<String>,
    /// Database file for SQLite.
    pub file: Option<PathBuf>,
    pub tls: TlsOptions,
    /// The SSH tunnel this connection goes through. Informational for drivers: the tunnel is
    /// already open and reachable at `dial_addr`.
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
            .field("dial_addr", &self.dial_addr)
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
            dial_addr: None,
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

    /// Dials `addr` (the local end of a tunnel) instead of the server endpoint.
    #[must_use]
    pub fn with_dial_addr(mut self, addr: SocketAddr) -> Self {
        self.dial_addr = Some(addr);
        self
    }

    /// The server port: the explicit one or the engine default.
    pub fn effective_port(&self) -> Option<u16> {
        self.port.or(self.engine.default_port())
    }

    /// The logical server endpoint (`host` + effective port). `None` for SQLite or when the host
    /// or port is missing. This is what an SSH tunnel forwards to.
    pub fn server_endpoint(&self) -> Option<Endpoint> {
        let host = self
            .host
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())?;
        Some(Endpoint::new(host, self.effective_port()?))
    }

    /// Where the driver opens sockets: `dial_addr` if set, otherwise the server endpoint.
    pub fn dial_endpoint(&self) -> Option<Endpoint> {
        match self.dial_addr {
            Some(addr) => Some(Endpoint::from(addr)),
            None => self.server_endpoint(),
        }
    }

    /// `true` when sockets go to a tunnel instead of the server endpoint.
    pub fn is_tunnelled(&self) -> bool {
        self.dial_addr.is_some()
    }

    /// The name TLS must verify and send as SNI: `tls.server_name` if set, otherwise the server
    /// `host`. Never the dial (tunnel) address.
    pub fn tls_server_name(&self) -> Option<&str> {
        let host = self.host.as_deref().map(str::trim).unwrap_or_default();
        let name = self.tls.effective_server_name(host);
        (!name.is_empty()).then_some(name)
    }

    /// Checks the parameters make sense for the engine: a host for network engines, a file for
    /// SQLite, a client key with a client certificate, and an already-opened tunnel
    /// (`dial_addr`) whenever `ssh` is set.
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
        self.tls.validate()?;
        if let Some(ssh) = &self.ssh {
            if !self.engine.is_networked() {
                return Err(Error::invalid_input(format!(
                    "{} cannot be reached through an SSH tunnel",
                    self.engine.display_name()
                )));
            }
            ssh.validate()?;
            if self.dial_addr.is_none() {
                return Err(Error::invalid_input(
                    "SSH tunnel is not open: connect through the connection manager, which \
                     opens the tunnel and sets dial_addr",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{KeySource, PemSource, SshAuth, TlsMode};

    #[test]
    fn secret_never_debug_prints() {
        let p = ConnectParams::network(Engine::Postgres, "db.local")
            .with_user("app")
            .with_password("hunter2-very-secret")
            .with_ssh(SshOptions::new(
                "bastion",
                "ops",
                SshAuth::PrivateKey {
                    key: KeySource::File("/home/ops/.ssh/id_ed25519".into()),
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
        bad.tls.client_cert = Some(PemSource::File("c.pem".into()));
        assert!(bad.validate().is_err());
    }

    #[test]
    fn dial_and_server_endpoints_are_separate() {
        let tunnel: SocketAddr = "127.0.0.1:41234".parse().unwrap();
        let ssh = SshOptions::new("bastion", "ops", SshAuth::Agent);
        let p = ConnectParams::network(Engine::Postgres, "db.internal")
            .with_tls(TlsOptions::new(TlsMode::VerifyFull))
            .with_ssh(ssh);
        assert!(p.validate().is_err(), "ssh without an opened tunnel");
        let p = p.with_dial_addr(tunnel);
        p.validate().unwrap();
        assert!(p.is_tunnelled());
        assert_eq!(
            p.server_endpoint(),
            Some(Endpoint::new("db.internal", 5432))
        );
        assert_eq!(p.dial_endpoint(), Some(Endpoint::new("127.0.0.1", 41234)));
        assert_eq!(p.tls_server_name(), Some("db.internal"));
        let p = p.with_tls(TlsOptions::new(TlsMode::VerifyFull).with_server_name("pg.example"));
        assert_eq!(p.tls_server_name(), Some("pg.example"));

        let direct = ConnectParams::network(Engine::MySql, "db").with_port(3307);
        assert_eq!(direct.dial_endpoint(), direct.server_endpoint());
        assert_eq!(direct.dial_endpoint(), Some(Endpoint::new("db", 3307)));
        assert_eq!(ConnectParams::sqlite("x.db").server_endpoint(), None);
        assert_eq!(ConnectParams::sqlite("x.db").tls_server_name(), None);

        let mut sqlite_ssh = ConnectParams::sqlite("x.db").with_dial_addr(tunnel);
        sqlite_ssh.ssh = Some(SshOptions::new("b", "u", SshAuth::Agent));
        assert!(sqlite_ssh.validate().is_err());
    }
}
