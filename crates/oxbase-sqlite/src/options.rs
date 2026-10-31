//! SQLite-specific connection options, read from [`ConnectParams`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use oxbase_driver::ConnectParams;
use oxbase_types::{Engine, Error, Result};

/// `ConnectParams::options` key: open the database read-only (`true` / `false`, default `false`).
pub const OPT_READ_ONLY: &str = "read_only";
/// `ConnectParams::options` key: create the database file when it does not exist (`true` /
/// `false`, default `true`; ignored when read-only).
pub const OPT_CREATE_IF_MISSING: &str = "create_if_missing";
/// `ConnectParams::options` key: how long a statement waits for a lock held by another
/// connection before failing with `SQLITE_BUSY`, in milliseconds (default 5000).
pub const OPT_BUSY_TIMEOUT_MS: &str = "busy_timeout_ms";

const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How to open one SQLite database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteOptions {
    /// Database file, `:memory:` or a `file:` URI.
    pub path: PathBuf,
    pub read_only: bool,
    pub create_if_missing: bool,
    pub busy_timeout: Duration,
}

impl SqliteOptions {
    /// Options for `path` with the defaults (read-write, create if missing, 5 s busy timeout).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            read_only: false,
            create_if_missing: true,
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
        }
    }

    /// Reads the options from `params`.
    ///
    /// SSH tunnels and TLS do not apply to an embedded database file: an `ssh` section, a
    /// `dial_addr`, a TLS mode that requires TLS or any TLS material is rejected with
    /// `Unsupported`. `TlsMode::Prefer` (the default) and `Disable` are accepted and ignored.
    pub fn from_params(params: &ConnectParams) -> Result<Self> {
        if params.engine != Engine::Sqlite {
            return Err(Error::invalid_input(format!(
                "the SQLite driver cannot open a {} connection",
                params.engine.display_name()
            )));
        }
        if params.ssh.is_some() {
            return Err(Error::unsupported(
                "SSH tunnels do not apply to SQLite: it opens a local database file",
            ));
        }
        if params.dial_addr.is_some() {
            return Err(Error::unsupported(
                "a network address (dial_addr) does not apply to SQLite: it opens a local \
                 database file",
            ));
        }
        let tls = &params.tls;
        if tls.mode.requires_tls()
            || tls.ca_cert.is_some()
            || tls.client_cert.is_some()
            || tls.client_key.is_some()
            || tls.server_name.is_some()
        {
            return Err(Error::unsupported(
                "TLS does not apply to SQLite: it opens a local database file (set the TLS mode \
                 to disable or prefer and remove certificates)",
            ));
        }
        let path = params
            .file
            .clone()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| Error::invalid_input("SQLite needs a database file or :memory:"))?;

        let mut out = Self::new(path);
        for (key, value) in &params.options {
            match key.as_str() {
                OPT_READ_ONLY => out.read_only = parse_bool(key, value)?,
                OPT_CREATE_IF_MISSING => out.create_if_missing = parse_bool(key, value)?,
                OPT_BUSY_TIMEOUT_MS => {
                    let ms: u64 = value.trim().parse().map_err(|_| {
                        Error::invalid_input(format!(
                            "option {key} must be a number of milliseconds, got '{value}'"
                        ))
                    })?;
                    out.busy_timeout = Duration::from_millis(ms);
                }
                // Options of other layers (and future ones) pass through untouched.
                _ => {}
            }
        }
        Ok(out)
    }

    /// `true` for an in-memory database (`:memory:` or a `file:` URI with `mode=memory`).
    pub fn is_memory(&self) -> bool {
        let p = self.path.to_string_lossy();
        p == ":memory:" || (p.starts_with("file:") && p.contains("mode=memory"))
    }

    /// The path for messages: a `file:` URI loses its query string (it may carry options that
    /// are nobody's business in an error message).
    pub(crate) fn display_path(&self) -> String {
        display_path(&self.path)
    }
}

pub(crate) fn display_path(path: &Path) -> String {
    let p = path.to_string_lossy();
    if p.starts_with("file:")
        && let Some((base, _)) = p.split_once('?')
    {
        return base.to_owned();
    }
    p.into_owned()
}

fn parse_bool(key: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(Error::invalid_input(format!(
            "option {key} must be true or false, got '{value}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{ErrorCategory, PemSource, SshAuth, SshOptions, TlsMode, TlsOptions};

    #[test]
    fn defaults_and_options() {
        let o = SqliteOptions::from_params(&ConnectParams::sqlite("/tmp/a.db")).unwrap();
        assert_eq!(o, SqliteOptions::new("/tmp/a.db"));
        assert!(!o.read_only && o.create_if_missing);

        let mut p = ConnectParams::sqlite(":memory:");
        p.options.insert(OPT_READ_ONLY.into(), "YES".into());
        p.options.insert(OPT_CREATE_IF_MISSING.into(), "0".into());
        p.options.insert(OPT_BUSY_TIMEOUT_MS.into(), "250".into());
        p.options.insert("something_else".into(), "kept".into());
        let o = SqliteOptions::from_params(&p).unwrap();
        assert!(o.read_only && !o.create_if_missing && o.is_memory());
        assert_eq!(o.busy_timeout, Duration::from_millis(250));

        p.options.insert(OPT_READ_ONLY.into(), "maybe".into());
        assert_eq!(
            SqliteOptions::from_params(&p).unwrap_err().category(),
            ErrorCategory::InvalidInput
        );
        assert!(SqliteOptions::from_params(&ConnectParams::sqlite("")).is_err());
        assert!(SqliteOptions::from_params(&ConnectParams::new(Engine::Postgres)).is_err());
    }

    #[test]
    fn ssh_and_tls_are_unsupported() {
        let ssh = ConnectParams::sqlite("a.db").with_ssh(SshOptions::new("b", "u", SshAuth::Agent));
        let err = SqliteOptions::from_params(&ssh).unwrap_err();
        assert_eq!(err.category(), ErrorCategory::Unsupported);
        assert!(err.to_string().contains("SSH"), "{err}");

        for tls in [
            TlsOptions::new(TlsMode::Require),
            TlsOptions::new(TlsMode::VerifyFull),
            TlsOptions::new(TlsMode::Prefer).with_ca_cert(PemSource::File("ca.pem".into())),
            TlsOptions::new(TlsMode::Disable).with_server_name("db"),
        ] {
            let p = ConnectParams::sqlite("a.db").with_tls(tls);
            let err = SqliteOptions::from_params(&p).unwrap_err();
            assert_eq!(err.category(), ErrorCategory::Unsupported);
            assert!(err.to_string().contains("TLS"), "{err}");
        }
        for mode in [TlsMode::Disable, TlsMode::Prefer] {
            let p = ConnectParams::sqlite("a.db").with_tls(TlsOptions::new(mode));
            SqliteOptions::from_params(&p).unwrap();
        }
        let tunnel = ConnectParams::sqlite("a.db").with_dial_addr("127.0.0.1:1".parse().unwrap());
        assert_eq!(
            SqliteOptions::from_params(&tunnel).unwrap_err().category(),
            ErrorCategory::Unsupported
        );
    }

    #[test]
    fn display_path_hides_uri_query() {
        assert_eq!(
            display_path(Path::new("file:/data/x.db?mode=ro&cache=shared")),
            "file:/data/x.db"
        );
        assert_eq!(display_path(Path::new("/data/x?.db")), "/data/x?.db");
    }
}
