//! The MySQL / MariaDB [`Driver`]: connection options, TLS and server flavour detection.

use std::time::Duration;

use async_trait::async_trait;
use mysql_async::prelude::Queryable;
use mysql_async::{ClientIdentity, Conn, Opts, OptsBuilder, SslOpts};
use oxbase_driver::{ConnectParams, Connection, Driver, DriverRegistry};
use oxbase_net::tls;
use oxbase_types::{Engine, Error, Result, ServerVersion, TlsMode};

use crate::conn::MySqlConnection;
use crate::error::{is_no_tls_offered, map_connect_error, map_error};
use crate::types::TypeOptions;

/// Connect deadline when [`ConnectParams::connect_timeout`] is not set.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// `ConnectParams::options` key: `true`/`false`, see [`TypeOptions::tinyint1_as_bool`].
pub const OPT_TINYINT1_AS_BOOL: &str = "tinyint1_as_bool";
/// `ConnectParams::options` key: maximum rows per Arrow batch.
pub const OPT_BATCH_ROWS: &str = "batch_rows";
/// `ConnectParams::options` key: approximate maximum bytes per Arrow batch.
pub const OPT_BATCH_BYTES: &str = "batch_bytes";
/// `ConnectParams::options` key: `true`/`false`, see [`MySqlOptions::utc_session`].
pub const OPT_UTC_SESSION: &str = "utc_session";

/// Which server product a connection talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flavor {
    MySql,
    MariaDb,
}

impl Flavor {
    /// Detects the flavour from a `VERSION()` string (`11.8.9-MariaDB-ubu2404`, `8.4.13`).
    pub fn detect(version: &str) -> Self {
        if version.to_ascii_lowercase().contains("mariadb") {
            Self::MariaDb
        } else {
            Self::MySql
        }
    }

    /// The matching [`Engine`].
    pub fn engine(self) -> Engine {
        match self {
            Self::MySql => Engine::MySql,
            Self::MariaDb => Engine::MariaDb,
        }
    }
}

/// Driver-level defaults, overridable per connection through `ConnectParams::options`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlOptions {
    pub types: TypeOptions,
    /// Maximum rows per Arrow batch (default 4096).
    pub batch_rows: usize,
    /// Approximate maximum bytes per Arrow batch (default 8 MiB).
    pub batch_bytes: usize,
    /// Run `SET time_zone = '+00:00'` after connecting (default `true`), so `TIMESTAMP` values
    /// are read and written as UTC instants (`Timestamp(µs, "UTC")`, `Value::TimestampTz`
    /// parameters). Session functions such as `NOW()` then also report UTC. With `false`, or
    /// when the statement fails, the server's default zone stays and `TIMESTAMP` columns are
    /// zone-less wall-clock values like `DATETIME`.
    pub utc_session: bool,
}

impl Default for MySqlOptions {
    fn default() -> Self {
        Self {
            types: TypeOptions::default(),
            batch_rows: 4096,
            batch_bytes: 8 * 1024 * 1024,
            utc_session: true,
        }
    }
}

fn parse_bool(key: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(Error::invalid_input(format!(
            "option {key} must be true or false, not '{value}'"
        ))),
    }
}

fn parse_positive(key: &str, value: &str) -> Result<usize> {
    match value.trim().parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(Error::invalid_input(format!(
            "option {key} must be a positive integer, not '{value}'"
        ))),
    }
}

impl MySqlOptions {
    /// These defaults with the overrides in `params.options` applied. Unknown keys are ignored.
    pub fn with_overrides(mut self, params: &ConnectParams) -> Result<Self> {
        for (key, value) in &params.options {
            match key.as_str() {
                OPT_TINYINT1_AS_BOOL => self.types.tinyint1_as_bool = parse_bool(key, value)?,
                OPT_BATCH_ROWS => self.batch_rows = parse_positive(key, value)?,
                OPT_BATCH_BYTES => self.batch_bytes = parse_positive(key, value)?,
                OPT_UTC_SESSION => self.utc_session = parse_bool(key, value)?,
                _ => {}
            }
        }
        Ok(self)
    }
}

/// The MySQL / MariaDB driver. One instance serves both engines (see [`register`]).
#[derive(Debug, Clone, Default)]
pub struct MySqlDriver {
    options: MySqlOptions,
}

/// Registers one [`MySqlDriver`] for [`Engine::MySql`] and [`Engine::MariaDb`].
pub fn register(registry: &mut DriverRegistry) {
    let driver = std::sync::Arc::new(MySqlDriver::new());
    registry.register_as(Engine::MySql, driver.clone());
    registry.register_as(Engine::MariaDb, driver);
}

impl MySqlDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// A driver with other defaults.
    pub fn with_options(options: MySqlOptions) -> Self {
        Self { options }
    }

    /// Like [`Driver::connect`], returning the concrete connection type.
    ///
    /// The returned connection reports the flavour the server announced, which may differ from
    /// `params.engine` (a MariaDB server reached through a MySQL profile reports
    /// [`Engine::MariaDb`]).
    pub async fn connect_mysql(&self, params: &ConnectParams) -> Result<MySqlConnection> {
        if !params.engine.is_mysql_family() {
            return Err(Error::invalid_input(format!(
                "the MySQL driver cannot open {} connections",
                params.engine.display_name()
            )));
        }
        params.validate()?;
        let mut options = self.options.with_overrides(params)?;
        let server = params
            .server_endpoint()
            .ok_or_else(|| Error::invalid_input("MySQL needs a host"))?;
        let dial = params
            .dial_endpoint()
            .ok_or_else(|| Error::invalid_input("MySQL needs a host"))?;
        let shown = if params.is_tunnelled() {
            format!("{server} (via {dial})")
        } else {
            server.to_string()
        };
        let timeout = params.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT);

        let mut base = OptsBuilder::default()
            .ip_or_hostname(dial.host.trim_start_matches('[').trim_end_matches(']'))
            .tcp_port(dial.port)
            .user(params.user.clone())
            .pass(params.password.as_ref().map(|p| p.expose().to_owned()))
            .db_name(params.database.clone())
            // Never switch to a local Unix socket: it would bypass the tunnel and TLS.
            .prefer_socket(false)
            // An UPDATE reports matched rows, not changed ones, so writing the value a row
            // already has still counts 1 (the grid editor's "exactly one row" check, M3).
            .client_found_rows(true)
            .tcp_nodelay(true);
        if let Some(name) = &params.application_name {
            base = base.connect_attribute("program_name", name.clone());
        }

        let (conn, opts, tls_in_use) = match ssl_opts(params)? {
            None => {
                let opts = Opts::from(base);
                (open(&opts, timeout, &shown).await?, opts, false)
            }
            Some(ssl) => {
                let secure = Opts::from(base.clone().ssl_opts(ssl));
                match open_raw(&secure, timeout).await {
                    Ok(conn) => (conn, secure, true),
                    Err(Some(e)) if params.tls.mode == TlsMode::Prefer && is_no_tls_offered(&e) => {
                        tracing::debug!(server = %shown, "server offers no TLS; Prefer falls back");
                        let plain = Opts::from(base);
                        (open(&plain, timeout, &shown).await?, plain, false)
                    }
                    Err(Some(e)) => return Err(map_connect_error(e, &shown)),
                    Err(None) => return Err(connect_timeout(&shown, timeout)),
                }
            }
        };

        let mut conn = conn;
        let raw_version = match conn.query_first::<String, _>("SELECT VERSION()").await {
            Ok(Some(v)) => v,
            Ok(None) => String::new(),
            Err(e) => return Err(map_error(e)),
        };
        let flavor = Flavor::detect(&raw_version);
        let version = ServerVersion::parse(&raw_version).unwrap_or_else(|_| {
            let (major, minor, patch) = conn.server_version();
            ServerVersion::new(major.into(), minor.into(), patch.into())
        });
        if options.utc_session {
            // TIMESTAMP columns are stored as UTC and converted through the session zone: a UTC
            // session makes reads and `TimestampTz` parameters exact instants.
            match conn.query_drop("SET time_zone = '+00:00'").await {
                Ok(()) => options.types.timestamp_utc = true,
                Err(e) => tracing::warn!(
                    server = %shown,
                    error = %e,
                    "cannot set the session time zone to UTC; TIMESTAMP values stay in the \
                     server's zone"
                ),
            }
        }
        if flavor.engine() != params.engine {
            tracing::debug!(
                requested = %params.engine,
                detected = %flavor.engine(),
                "server flavour differs from the profile's engine"
            );
        }
        // Cancel side connections reuse the session's options; the database may be dropped
        // meanwhile and is not needed for KILL.
        let side_opts = Opts::from(OptsBuilder::from_opts(opts).db_name(None::<String>));
        Ok(
            MySqlConnection::new(conn, side_opts, options, flavor, version, tls_in_use, shown)
                .with_database(params.database.clone()),
        )
    }
}

#[async_trait]
impl Driver for MySqlDriver {
    fn engine(&self) -> Engine {
        Engine::MySql
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        Ok(Box::new(self.connect_mysql(params).await?))
    }
}

fn connect_timeout(shown: &str, timeout: Duration) -> Error {
    Error::timeout(format!(
        "connecting to {shown} took longer than {}s",
        timeout.as_secs_f64()
    ))
}

/// Opens a session; `Err(None)` is a timeout.
async fn open_raw(
    opts: &Opts,
    timeout: Duration,
) -> std::result::Result<Conn, Option<mysql_async::Error>> {
    match tokio::time::timeout(timeout, Conn::new(opts.clone())).await {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => Err(Some(e)),
        Err(_) => Err(None),
    }
}

async fn open(opts: &Opts, timeout: Duration, shown: &str) -> Result<Conn> {
    match open_raw(opts, timeout).await {
        Ok(conn) => Ok(conn),
        Err(Some(e)) => Err(map_connect_error(e, shown)),
        Err(None) => Err(connect_timeout(shown, timeout)),
    }
}

/// Why `TlsMode::VerifyCa` is refused (see [`ssl_opts`]).
const VERIFY_CA_UNSUPPORTED: &str = "TLS mode \"verify CA\" (certificate chain without host name \
     check) is not available for MySQL / MariaDB yet; use \"verify full\" (with tls.server_name \
     set to a name in the server certificate if needed) or \"require\"";

/// Maps [`oxbase_types::TlsOptions`] onto `mysql_async`'s rustls settings (`None`: plaintext).
///
/// - The TLS server name (SNI and certificate check) is always
///   [`ConnectParams::tls_server_name`], never the dial address, so `VerifyFull` works through an
///   SSH tunnel.
/// - `Prefer` / `Require` accept any certificate; `VerifyFull` checks the chain and the name.
///   `VerifyCa` is refused with `Error::Unsupported` (see the crate docs).
/// - With `ca_cert`, only that CA is trusted. Without it, `mysql_async` trusts its bundled
///   Mozilla roots (`webpki-roots`) rather than the platform store.
/// - PEM material is validated with `oxbase_net::tls` first, so errors are typed and never quote
///   key material.
pub(crate) fn ssl_opts(params: &ConnectParams) -> Result<Option<SslOpts>> {
    let opts = &params.tls;
    if !opts.mode.uses_tls() {
        return Ok(None);
    }
    // mysql_async calls `ClientConfig::builder()`, which needs a process-default provider.
    tls::install_default_crypto_provider();
    let mut ssl = SslOpts::default();
    if let Some(ca) = &opts.ca_cert {
        tls::load_certificates(ca)?;
        ssl = ssl
            .with_root_certs(vec![tls::load_pem(ca)?.into()])
            .with_disable_built_in_roots(true);
    }
    if tls::load_client_identity(opts)?.is_some()
        && let (Some(cert), Some(key)) = (&opts.client_cert, &opts.client_key)
    {
        let cert = tls::load_pem(cert)?;
        let key = tls::load_key_pem(key)?;
        ssl = ssl.with_client_identity(Some(ClientIdentity::new(
            cert.into(),
            key.expose().as_bytes().to_vec().into(),
        )));
    }
    ssl = match opts.mode {
        TlsMode::Disable => return Ok(None),
        TlsMode::Prefer | TlsMode::Require => ssl
            .with_danger_accept_invalid_certs(true)
            .with_danger_skip_domain_validation(true),
        // mysql_async 0.37.1 recognises a name mismatch by searching the rustls error text for
        // `NotValidForName`, which rustls 0.23.46 no longer prints, so "chain only" would silently
        // behave like VerifyFull. Refuse it until the library matches on the error variant.
        TlsMode::VerifyCa => return Err(Error::unsupported(VERIFY_CA_UNSUPPORTED)),
        TlsMode::VerifyFull => ssl,
    };
    let name = params
        .tls_server_name()
        .map(|n| n.trim_start_matches('[').trim_end_matches(']').to_owned())
        .ok_or_else(|| Error::invalid_input("TLS needs a server name"))?;
    if opts.mode.verifies_hostname() {
        // Fails early, with a clear message, on names rustls cannot verify.
        tls::server_name(opts, &name)?;
    }
    Ok(Some(ssl.with_danger_tls_hostname_override(Some(name))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{KeySource, PemSource, Secret, TlsOptions};

    #[test]
    fn flavour_detection() {
        assert_eq!(Flavor::detect("11.8.9-MariaDB-ubu2404"), Flavor::MariaDb);
        assert_eq!(Flavor::detect("5.5.5-10.11.6-MariaDB-log"), Flavor::MariaDb);
        assert_eq!(Flavor::detect("8.4.13"), Flavor::MySql);
        assert_eq!(Flavor::MariaDb.engine(), Engine::MariaDb);
        assert_eq!(Flavor::MySql.engine(), Engine::MySql);
    }

    #[test]
    fn options_from_params() {
        let mut p = ConnectParams::network(Engine::MySql, "db");
        let o = MySqlOptions::default().with_overrides(&p).unwrap();
        assert!(o.types.tinyint1_as_bool);
        p.options
            .insert(OPT_TINYINT1_AS_BOOL.into(), "false".into());
        p.options.insert(OPT_BATCH_ROWS.into(), "100".into());
        p.options.insert(OPT_UTC_SESSION.into(), "off".into());
        p.options.insert("unrelated".into(), "x".into());
        let o = MySqlOptions::default().with_overrides(&p).unwrap();
        assert!(!o.types.tinyint1_as_bool);
        assert_eq!(o.batch_rows, 100);
        assert!(!o.utc_session);
        assert!(MySqlOptions::default().utc_session);
        p.options.insert(OPT_BATCH_BYTES.into(), "0".into());
        assert!(MySqlOptions::default().with_overrides(&p).is_err());
    }

    #[test]
    fn registers_both_engines() {
        let mut registry = DriverRegistry::new();
        register(&mut registry);
        assert_eq!(registry.engines(), [Engine::MySql, Engine::MariaDb]);
    }

    #[test]
    fn tls_mapping_uses_server_name_not_dial_address() {
        let p = ConnectParams::network(Engine::MySql, "mysql")
            .with_dial_addr("127.0.0.1:41234".parse().unwrap())
            .with_tls(TlsOptions::new(TlsMode::VerifyFull));
        let ssl = ssl_opts(&p).unwrap().unwrap();
        assert_eq!(ssl.tls_hostname_override(), Some("mysql"));
        assert!(!ssl.accept_invalid_certs());
        assert!(!ssl.skip_domain_validation());
        assert!(!ssl.disable_built_in_roots());

        let p = p.with_tls(TlsOptions::new(TlsMode::VerifyFull).with_server_name("db.example"));
        assert_eq!(
            ssl_opts(&p).unwrap().unwrap().tls_hostname_override(),
            Some("db.example")
        );

        let p = ConnectParams::network(Engine::MariaDb, "10.0.0.5")
            .with_tls(TlsOptions::new(TlsMode::Require));
        let ssl = ssl_opts(&p).unwrap().unwrap();
        assert!(ssl.accept_invalid_certs());
        assert_eq!(ssl.tls_hostname_override(), Some("10.0.0.5"));

        // VerifyCa is refused rather than silently checking the name too.
        let p = p.with_tls(TlsOptions::new(TlsMode::VerifyCa));
        assert!(matches!(ssl_opts(&p), Err(Error::Unsupported { .. })));

        let p = p.with_tls(TlsOptions::new(TlsMode::Disable));
        assert!(ssl_opts(&p).unwrap().is_none());
    }

    #[test]
    fn tls_material_errors_are_typed_and_redacted() {
        let p = ConnectParams::network(Engine::MySql, "db").with_tls(
            TlsOptions::new(TlsMode::VerifyFull)
                .with_ca_cert(PemSource::Pem("not a certificate".into())),
        );
        assert!(matches!(ssl_opts(&p), Err(Error::InvalidInput { .. })));

        let marker = "MARKER-key-material-1234";
        let p = ConnectParams::network(Engine::MySql, "db").with_tls(
            TlsOptions::new(TlsMode::Require).with_client_identity(
                PemSource::Pem(
                    "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n".into(),
                ),
                KeySource::Pem(Secret::new(marker)),
            ),
        );
        let err = ssl_opts(&p).unwrap_err().to_string();
        assert!(!err.contains(marker), "{err}");
    }

    #[tokio::test]
    async fn rejects_other_engines_and_unopened_tunnels() {
        let driver = MySqlDriver::new();
        let pg = ConnectParams::network(Engine::Postgres, "db");
        assert!(matches!(
            driver.connect_mysql(&pg).await,
            Err(Error::InvalidInput { .. })
        ));
        let ssh = oxbase_types::SshOptions::new("bastion", "ops", oxbase_types::SshAuth::Agent);
        let tunnelled = ConnectParams::network(Engine::MySql, "db").with_ssh(ssh);
        assert!(matches!(
            driver.connect_mysql(&tunnelled).await,
            Err(Error::InvalidInput { .. })
        ));
    }

    #[tokio::test]
    async fn connect_errors_name_the_server_and_hide_the_password() {
        // Port 1 on loopback refuses connections.
        let marker = "MARKER-password-5678";
        let p = ConnectParams::network(Engine::MySql, "127.0.0.1")
            .with_port(1)
            .with_user("u")
            .with_password(marker)
            .with_tls(TlsOptions::new(TlsMode::Disable));
        let err = MySqlDriver::new().connect_mysql(&p).await.unwrap_err();
        let text = format!("{err} {err:?}");
        assert!(matches!(err, Error::Connection { .. }), "{text}");
        assert!(text.contains("127.0.0.1:1"), "{text}");
        assert!(!text.contains(marker), "{text}");
    }
}
