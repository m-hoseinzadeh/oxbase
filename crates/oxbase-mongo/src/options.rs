//! Keys of [`ConnectParams::options`](oxbase_driver::ConnectParams::options) the MongoDB driver
//! reads (profile `options` in the connection dialog), and their parsed form
//! [`MongoOptions`]. Unknown keys are refused with `InvalidInput` naming the key. Contract:
//! `docs/m7-contracts.md` §7.1.
//!
//! The profile's other fields map as usual: `host` / `port` (the first host, or the SRV name),
//! `user` / `password` (`None` user: no authentication; the auth database is
//! [`AUTH_SOURCE`]), `database` (the shell's starting `db`, default `test`), `tls`, `ssh`,
//! `connect_timeout`, `application_name` (the driver's `appName`).

use oxbase_driver::ConnectParams;
use oxbase_types::{Endpoint, Error, Result};

/// More hosts of a replica set or sharded cluster, `host:port` comma-separated, besides
/// `host:port`.
pub const HOSTS: &str = "hosts";
/// `true`: `host` is an SRV name (`mongodb+srv://`): hosts and options come from DNS SRV and
/// TXT records. Not with SSH, `hosts` or a port.
pub const SRV: &str = "srv";
/// Replica set name (`replicaSet`).
pub const REPLICA_SET: &str = "replica_set";
/// The authentication database (`authSource`). Default `admin` (the fixture and most
/// deployments keep users there); `$external` for X.509 / PLAIN.
pub const AUTH_SOURCE: &str = "auth_source";
/// `SCRAM-SHA-256`, `SCRAM-SHA-1`, `MONGODB-X509` or `PLAIN`. Default: negotiated (SCRAM).
pub const AUTH_MECHANISM: &str = "auth_mechanism";
/// `true` / `false`: talk only to the given host (`directConnection`). Default: `true` for one
/// host without a replica set name, else `false`. Always `true` through SSH.
pub const DIRECT_CONNECTION: &str = "direct_connection";
/// `primary` (default), `primaryPreferred`, `secondary`, `secondaryPreferred` or `nearest`.
pub const READ_PREFERENCE: &str = "read_preference";
/// Documents sampled to infer a result's columns (default
/// `oxbase_dialect::document::SCHEMA_SAMPLE_DOCUMENTS`, 1 .. 100 000).
pub const SAMPLE_SIZE: &str = "sample_size";
/// Shell: memory limit of the JavaScript runtime in MiB (default [`DEFAULT_SHELL_MEMORY_MB`],
/// 8 .. 1024).
pub const SHELL_MEMORY_MB: &str = "shell_memory_mb";
/// Shell: CPU time one statement's JavaScript may run, in milliseconds, not counting the time
/// it waits for the server (default [`DEFAULT_SHELL_TIME_LIMIT_MS`]; 0 = no limit).
pub const SHELL_TIME_LIMIT_MS: &str = "shell_time_limit_ms";

/// Every key above, for validation.
pub const ALL: [&str; 10] = [
    HOSTS,
    SRV,
    REPLICA_SET,
    AUTH_SOURCE,
    AUTH_MECHANISM,
    DIRECT_CONNECTION,
    READ_PREFERENCE,
    SAMPLE_SIZE,
    SHELL_MEMORY_MB,
    SHELL_TIME_LIMIT_MS,
];

/// The MongoDB port used when a host has none.
pub const DEFAULT_PORT: u16 = 27017;
/// Default [`AUTH_SOURCE`].
pub const DEFAULT_AUTH_SOURCE: &str = "admin";
/// Default [`SHELL_MEMORY_MB`].
pub const DEFAULT_SHELL_MEMORY_MB: u32 = 64;
/// Default [`SHELL_TIME_LIMIT_MS`].
pub const DEFAULT_SHELL_TIME_LIMIT_MS: u64 = 10_000;
/// Default [`SAMPLE_SIZE`].
pub const DEFAULT_SAMPLE_SIZE: usize = 1000;

/// `authMechanism`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthMechanism {
    ScramSha256,
    ScramSha1,
    /// The TLS client certificate is the credential (`$external`).
    X509,
    /// LDAP proxy auth (`$external`); the password goes to the server in the clear inside TLS,
    /// so it requires `TlsMode::VerifyFull`, or `VerifyCa` with a CA file.
    Plain,
}

/// `readPreference` modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadPreference {
    Primary,
    PrimaryPreferred,
    Secondary,
    SecondaryPreferred,
    Nearest,
}

/// [`ConnectParams`] options of a MongoDB profile, parsed and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MongoOptions {
    /// `host:port` first, then [`HOSTS`]; for SRV, just the SRV name (port unused).
    pub hosts: Vec<Endpoint>,
    pub srv: bool,
    pub replica_set: Option<String>,
    pub auth_source: String,
    /// `None`: negotiated.
    pub auth_mechanism: Option<AuthMechanism>,
    pub direct_connection: bool,
    pub read_preference: ReadPreference,
    pub sample_size: usize,
    pub shell_memory_mb: u32,
    /// 0 = no limit.
    pub shell_time_limit_ms: u64,
}

impl MongoOptions {
    /// Parses and checks `params.options` (and `host`, `port`, `ssh`): unknown keys and bad
    /// values are `InvalidInput`; so are SRV with SSH, a host list or a port, more than one
    /// host through SSH (a tunnel reaches one server: M7 connects directly to it), and `PLAIN`
    /// without a verifying TLS mode.
    pub fn from_params(params: &ConnectParams) -> Result<Self> {
        if let Some(key) = params.options.keys().find(|k| !ALL.contains(&k.as_str())) {
            return Err(Error::invalid_input(format!(
                "unknown MongoDB option '{key}'"
            )));
        }
        let get = |key: &str| {
            params
                .options
                .get(key)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
        };
        let flag = |key: &str| -> Result<Option<bool>> {
            match get(key) {
                None => Ok(None),
                Some("true") => Ok(Some(true)),
                Some("false") => Ok(Some(false)),
                Some(other) => Err(Error::invalid_input(format!(
                    "{key} must be true or false, not '{other}'"
                ))),
            }
        };
        let srv = flag(SRV)?.unwrap_or(false);
        let host = params
            .host
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .ok_or_else(|| Error::invalid_input("the MongoDB profile has no host"))?;
        let mut hosts = vec![Endpoint::new(host, params.port.unwrap_or(DEFAULT_PORT))];
        if let Some(more) = get(HOSTS) {
            for endpoint in Endpoint::parse_list(more, DEFAULT_PORT)? {
                if !hosts.contains(&endpoint) {
                    hosts.push(endpoint);
                }
            }
        }
        if srv && (params.ssh.is_some() || hosts.len() > 1 || params.port.is_some()) {
            return Err(Error::invalid_input(
                "an SRV name stands alone: no SSH tunnel, no other hosts and no port",
            ));
        }
        if params.ssh.is_some() && hosts.len() > 1 {
            return Err(Error::invalid_input(
                "an SSH tunnel reaches one server: give one host (M7 connects directly to it)",
            ));
        }
        let replica_set = get(REPLICA_SET).map(str::to_owned);
        let direct_connection = match flag(DIRECT_CONNECTION)? {
            _ if params.ssh.is_some() => true,
            Some(direct) => direct,
            None => hosts.len() == 1 && replica_set.is_none() && !srv,
        };
        if direct_connection && hosts.len() > 1 {
            return Err(Error::invalid_input(
                "a direct connection talks to one host only",
            ));
        }
        let auth_mechanism = match get(AUTH_MECHANISM) {
            None => None,
            Some("SCRAM-SHA-256") => Some(AuthMechanism::ScramSha256),
            Some("SCRAM-SHA-1") => Some(AuthMechanism::ScramSha1),
            Some("MONGODB-X509") => Some(AuthMechanism::X509),
            Some("PLAIN") => Some(AuthMechanism::Plain),
            Some(other) => {
                return Err(Error::invalid_input(format!(
                    "auth_mechanism must be SCRAM-SHA-256, SCRAM-SHA-1, MONGODB-X509 or PLAIN, \
                     not '{other}'"
                )));
            }
        };
        // The password may only go to an authenticated server: the full check, or the chain
        // against the profile's own CA file (never VerifyCa against the platform store alone).
        let authenticated = match params.tls.mode {
            oxbase_types::TlsMode::VerifyFull => true,
            oxbase_types::TlsMode::VerifyCa => params.tls.ca_cert.is_some(),
            _ => false,
        };
        if auth_mechanism == Some(AuthMechanism::Plain) && !authenticated {
            return Err(Error::invalid_input(
                "PLAIN sends the password itself: use TLS mode verify_full, or verify_ca with a \
                 CA file",
            ));
        }
        let auth_source = get(AUTH_SOURCE).map_or_else(
            || match auth_mechanism {
                Some(AuthMechanism::X509 | AuthMechanism::Plain) => "$external".to_owned(),
                _ => DEFAULT_AUTH_SOURCE.to_owned(),
            },
            str::to_owned,
        );
        let read_preference = match get(READ_PREFERENCE) {
            None | Some("primary") => ReadPreference::Primary,
            Some("primaryPreferred") => ReadPreference::PrimaryPreferred,
            Some("secondary") => ReadPreference::Secondary,
            Some("secondaryPreferred") => ReadPreference::SecondaryPreferred,
            Some("nearest") => ReadPreference::Nearest,
            Some(other) => {
                return Err(Error::invalid_input(format!(
                    "read_preference '{other}' is not a MongoDB read preference"
                )));
            }
        };
        let number = |key: &str, default: u64, range: std::ops::RangeInclusive<u64>| match get(key)
        {
            None => Ok(default),
            Some(text) => text
                .parse::<u64>()
                .ok()
                .filter(|n| range.contains(n))
                .ok_or_else(|| {
                    Error::invalid_input(format!(
                        "{key} must be a number from {} to {}, not '{text}'",
                        range.start(),
                        range.end()
                    ))
                }),
        };
        let sample_size = number(SAMPLE_SIZE, DEFAULT_SAMPLE_SIZE as u64, 1..=100_000)?;
        let shell_memory_mb = number(
            SHELL_MEMORY_MB,
            u64::from(DEFAULT_SHELL_MEMORY_MB),
            8..=1024,
        )?;
        let shell_time_limit_ms = number(
            SHELL_TIME_LIMIT_MS,
            DEFAULT_SHELL_TIME_LIMIT_MS,
            0..=86_400_000,
        )?;
        Ok(Self {
            hosts,
            srv,
            replica_set,
            auth_source,
            auth_mechanism,
            direct_connection,
            read_preference,
            sample_size: usize::try_from(sample_size).unwrap_or(DEFAULT_SAMPLE_SIZE),
            shell_memory_mb: u32::try_from(shell_memory_mb).unwrap_or(DEFAULT_SHELL_MEMORY_MB),
            shell_time_limit_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{Engine, SshAuth, SshOptions, TlsMode};

    fn params() -> ConnectParams {
        let mut p = ConnectParams::network(Engine::MongoDb, "127.0.0.1");
        p.port = Some(15417);
        p
    }

    #[test]
    fn single_host_defaults_to_a_direct_connection() {
        let o = MongoOptions::from_params(&params()).unwrap();
        assert_eq!(o.hosts, [Endpoint::new("127.0.0.1", 15417)]);
        assert!(o.direct_connection);
        assert!(!o.srv);
        assert_eq!(o.auth_source, "admin");
        assert_eq!(o.auth_mechanism, None);
        assert_eq!(o.read_preference, ReadPreference::Primary);
        assert_eq!(o.sample_size, DEFAULT_SAMPLE_SIZE);
        assert_eq!(o.shell_memory_mb, DEFAULT_SHELL_MEMORY_MB);
        assert_eq!(o.shell_time_limit_ms, DEFAULT_SHELL_TIME_LIMIT_MS);
    }

    #[test]
    fn replica_sets_and_srv() {
        let mut p = params();
        p.options.insert(HOSTS.into(), "m2:27018,m3".into());
        p.options.insert(REPLICA_SET.into(), "rs0".into());
        p.options
            .insert(READ_PREFERENCE.into(), "secondaryPreferred".into());
        let o = MongoOptions::from_params(&p).unwrap();
        assert_eq!(o.hosts.len(), 3);
        assert_eq!(o.hosts[2], Endpoint::new("m3", DEFAULT_PORT));
        assert!(!o.direct_connection);
        assert_eq!(o.replica_set.as_deref(), Some("rs0"));
        p.options.insert(DIRECT_CONNECTION.into(), "true".into());
        assert!(MongoOptions::from_params(&p).is_err());

        let mut srv = ConnectParams::network(Engine::MongoDb, "cluster0.example.net");
        srv.options.insert(SRV.into(), "true".into());
        let o = MongoOptions::from_params(&srv).unwrap();
        assert!(o.srv);
        assert!(!o.direct_connection);
        srv.port = Some(27017);
        assert!(MongoOptions::from_params(&srv).is_err());
    }

    #[test]
    fn ssh_means_one_direct_host() {
        let mut p = params();
        p.ssh = Some(SshOptions::new("127.0.0.1", "oxbase", SshAuth::Agent));
        p.options.insert(DIRECT_CONNECTION.into(), "false".into());
        assert!(MongoOptions::from_params(&p).unwrap().direct_connection);
        p.options.insert(HOSTS.into(), "m2".into());
        assert!(MongoOptions::from_params(&p).is_err());
    }

    #[test]
    fn auth_options() {
        let mut p = params();
        p.options
            .insert(AUTH_MECHANISM.into(), "MONGODB-X509".into());
        let o = MongoOptions::from_params(&p).unwrap();
        assert_eq!(o.auth_mechanism, Some(AuthMechanism::X509));
        assert_eq!(o.auth_source, "$external");
        p.options.insert(AUTH_MECHANISM.into(), "PLAIN".into());
        assert!(MongoOptions::from_params(&p).is_err());
        // VerifyCa authenticates the server only against the profile's own CA file.
        p.tls.mode = TlsMode::VerifyCa;
        let err = MongoOptions::from_params(&p).unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);
        assert!(err.to_string().contains("CA file"), "{err}");
        p.tls.ca_cert = Some(oxbase_types::PemSource::Pem("ca".into()));
        assert!(MongoOptions::from_params(&p).is_ok());
        p.tls = oxbase_types::TlsOptions::new(TlsMode::VerifyFull);
        assert!(MongoOptions::from_params(&p).is_ok());
        p.options.insert(AUTH_SOURCE.into(), "users".into());
        assert_eq!(MongoOptions::from_params(&p).unwrap().auth_source, "users");
    }

    #[test]
    fn bad_values_are_refused() {
        for (key, value) in [
            ("authSource", "admin"),
            (SRV, "yes"),
            (AUTH_MECHANISM, "GSSAPI"),
            (READ_PREFERENCE, "fastest"),
            (SAMPLE_SIZE, "0"),
            (SHELL_MEMORY_MB, "4"),
            (SHELL_TIME_LIMIT_MS, "-1"),
            (HOSTS, "a:b"),
        ] {
            let mut p = params();
            p.options.insert(key.into(), value.into());
            assert!(MongoOptions::from_params(&p).is_err(), "{key}={value}");
        }
    }
}
