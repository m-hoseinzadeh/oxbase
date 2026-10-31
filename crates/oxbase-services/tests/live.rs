//! Live end-to-end tests of the connection manager against the Docker fixtures: for PostgreSQL,
//! MySQL and MariaDB a plain connect, a TLS connect (`VerifyFull` with the fixture CA, as the
//! TLS-only user) and connects through the SSH bastion (plain and `VerifyFull`); for SQLite the
//! fixture file. Everything comes from `tests/fixtures.toml`.
//!
//! ```sh
//! scripts/fixtures-up.sh
//! cargo test -p oxbase-services --test live -- --ignored --test-threads=2
//! scripts/fixtures-down.sh
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_services::{
    CancellationToken, ConnectionInfo, ConnectionManager, ConnectionProfile, ProvidedSecrets,
    SecretRef, SecretStore, default_drivers,
};
use oxbase_store::{
    Extra, KnownHostsPolicy, MemorySecretStore, PemRef, ProfileAddress, Secret, SecretSlot,
    SshAuthSettings, SshSettings, TlsMode, TlsSettings,
};
use oxbase_types::Engine;

/// Repository root (`crates/oxbase-services/../..`).
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn fixtures() -> toml::Table {
    let path = std::env::var_os("OXBASE_FIXTURES")
        .map(PathBuf::from)
        .map_or_else(|| root().join("tests/fixtures.toml"), |p| root().join(p));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.parse().expect("fixtures.toml parses")
}

fn table<'a>(t: &'a toml::Table, key: &str) -> &'a toml::Table {
    t.get(key)
        .and_then(toml::Value::as_table)
        .unwrap_or_else(|| panic!("fixtures.toml: missing table {key}"))
}

fn string(t: &toml::Table, key: &str) -> String {
    t.get(key)
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("fixtures.toml: missing string {key}"))
        .to_owned()
}

fn port(t: &toml::Table, key: &str) -> u16 {
    t.get(key)
        .and_then(toml::Value::as_integer)
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or_else(|| panic!("fixtures.toml: missing port {key}"))
}

struct Live {
    fx: toml::Table,
    mgr: ConnectionManager,
    secrets: Arc<MemorySecretStore>,
}

impl Live {
    fn new() -> Self {
        let secrets = Arc::new(MemorySecretStore::new());
        let mgr = ConnectionManager::new(
            default_drivers(),
            secrets.clone(),
            Arc::new(RusshTunnelProvider::new()),
        );
        Self {
            fx: fixtures(),
            mgr,
            secrets,
        }
    }

    fn engine(&self, key: &str) -> &toml::Table {
        table(table(&self.fx, "engines"), key)
    }

    /// A direct profile for engine `key`, as the plain user or the TLS-only user.
    async fn profile(&self, key: &str, engine: Engine, tls_user: bool) -> ConnectionProfile {
        let e = self.engine(key);
        let creds = if tls_user { table(e, "tls_user") } else { e };
        let mut p = ConnectionProfile::new(
            format!("live {key}"),
            engine,
            ProfileAddress::Network {
                host: string(e, "host"),
                port: Some(port(e, "port")),
            },
        );
        p.user = Some(string(creds, "user"));
        p.database = Some(string(creds, "database"));
        let key = p.secret_key(SecretSlot::Password);
        self.secrets
            .set(&key, &Secret::new(string(creds, "password")))
            .await
            .unwrap();
        p.password = Some(SecretRef::Keychain { key });
        p.tls = TlsSettings {
            mode: TlsMode::Disable,
            ..TlsSettings::default()
        };
        p.connect_timeout_secs = Some(15);
        p
    }

    fn verify_full(&self) -> TlsSettings {
        let tls = table(&self.fx, "tls");
        let ca = root().join(string(tls, "dir")).join(string(tls, "ca_cert"));
        assert!(ca.is_file(), "fixture CA missing: {}", ca.display());
        TlsSettings {
            mode: TlsMode::VerifyFull,
            ca_cert: Some(PemRef::File { path: ca }),
            ..TlsSettings::default()
        }
    }

    /// Routes `p` through the bastion to the in-network address of engine `key` (password auth,
    /// strict host-key check against the fixture known_hosts).
    async fn through_ssh(&self, p: &mut ConnectionProfile, key: &str) {
        let ssh = table(&self.fx, "ssh");
        let target = table(table(ssh, "targets"), key);
        p.address = ProfileAddress::Network {
            host: string(target, "host"),
            port: Some(port(target, "port")),
        };
        let known_hosts = root()
            .join(string(ssh, "key_dir"))
            .join(string(ssh, "known_hosts"));
        let pw_key = p.secret_key(SecretSlot::SshPassword);
        self.secrets
            .set(&pw_key, &Secret::new(string(ssh, "password")))
            .await
            .unwrap();
        p.ssh = Some(SshSettings {
            host: string(ssh, "host"),
            port: port(ssh, "port"),
            user: string(ssh, "user"),
            auth: SshAuthSettings::Password {
                password: SecretRef::Keychain { key: pw_key },
            },
            known_hosts: KnownHostsPolicy::Strict {
                path: Some(known_hosts),
            },
            connect_timeout_secs: Some(15),
            keepalive_secs: Some(30),
            extra: Extra::new(),
        });
    }

    /// `test_connection`, then a session that runs `SELECT 1`, then close. Returns the info.
    async fn exercise(&self, p: &ConnectionProfile) -> ConnectionInfo {
        let info = self
            .mgr
            .test_connection(p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{}: test_connection failed: {e}", p.name));
        assert!(
            !info.server_version.to_string().is_empty(),
            "{}: {info:?}",
            p.name
        );

        let session = self
            .mgr
            .open(p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{}: open failed: {e}", p.name));
        assert_eq!(session.info().via_ssh, info.via_ssh);
        assert_eq!(session.info().tls_in_use, info.tls_in_use);
        let stream = session
            .connection()
            .execute("SELECT 1", &[], CancellationToken::new())
            .await
            .unwrap();
        let out = oxbase_driver::collect(stream).await.unwrap();
        assert_eq!(out.result_sets.len(), 1, "{}", p.name);
        assert_eq!(self.mgr.list_open().len(), 1);
        self.mgr.close(session.id()).await.unwrap();
        drop(session);
        assert_eq!(self.mgr.list_open(), Vec::new());
        assert_eq!(self.mgr.open_tunnel_count(), 0, "tunnel released");
        info
    }
}

async fn plain(key: &str, engine: Engine) {
    let live = Live::new();
    let p = live.profile(key, engine, false).await;
    let info = live.exercise(&p).await;
    assert!(!info.tls_in_use && !info.via_ssh, "{info:?}");
}

async fn tls(key: &str, engine: Engine) {
    let live = Live::new();
    let mut p = live.profile(key, engine, true).await;
    p.tls = live.verify_full();
    let info = live.exercise(&p).await;
    assert!(info.tls_in_use && !info.via_ssh, "{info:?}");

    // The TLS-only user is refused in plaintext.
    p.tls = TlsSettings::default();
    p.tls.mode = TlsMode::Disable;
    assert!(
        live.mgr
            .test_connection(&p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .is_err()
    );
}

async fn ssh(key: &str, engine: Engine) {
    let live = Live::new();
    let mut p = live.profile(key, engine, false).await;
    live.through_ssh(&mut p, key).await;
    let info = live.exercise(&p).await;
    assert!(info.via_ssh && !info.tls_in_use, "{info:?}");

    // Through the tunnel with VerifyFull: the certificate must match the service name, not
    // the 127.0.0.1 tunnel address.
    let mut p = live.profile(key, engine, true).await;
    live.through_ssh(&mut p, key).await;
    p.tls = live.verify_full();
    let info = live.exercise(&p).await;
    assert!(info.via_ssh && info.tls_in_use, "{info:?}");
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn postgres_plain() {
    plain("postgres", Engine::Postgres).await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn postgres_tls() {
    tls("postgres", Engine::Postgres).await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn postgres_ssh() {
    ssh("postgres", Engine::Postgres).await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn mysql_plain() {
    plain("mysql", Engine::MySql).await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn mysql_tls() {
    tls("mysql", Engine::MySql).await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn mysql_ssh() {
    ssh("mysql", Engine::MySql).await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn mariadb_plain() {
    plain("mariadb", Engine::MariaDb).await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn mariadb_tls() {
    tls("mariadb", Engine::MariaDb).await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn mariadb_ssh() {
    ssh("mariadb", Engine::MariaDb).await;
}

/// Cancelling `open` while it connects through the SSH bastion returns `Cancelled` and leaves no
/// tunnel or session behind; the manager still connects afterwards.
async fn cancel_open_via_ssh(key: &str, engine: Engine) {
    let live = Live::new();
    let mut p = live.profile(key, engine, false).await;
    live.through_ssh(&mut p, key).await;
    for delay_ms in [0, 1, 3] {
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            trigger.cancel();
        });
        let started = Instant::now();
        let err = live
            .mgr
            .open(&p, &ProvidedSecrets::none(), cancel)
            .await
            .expect_err("open finished before the cancel");
        assert!(err.is_cancelled(), "{key} after {delay_ms} ms: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{key}: cancel was slow"
        );
        assert_eq!(live.mgr.list_open(), Vec::new());
        // Give a dropped russh session a moment to finish closing.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(live.mgr.open_tunnel_count(), 0, "{key}: tunnel left open");
    }
    live.exercise(&p).await;
}

/// Cancelling a running query on a session that goes through the bastion stops it promptly.
async fn cancel_query_via_ssh(key: &str, engine: Engine, sleep_sql: &str) {
    let live = Live::new();
    let mut p = live.profile(key, engine, false).await;
    live.through_ssh(&mut p, key).await;
    let session = live
        .mgr
        .open(&p, &ProvidedSecrets::none(), CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("{key}: open failed: {e}"));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let result = async {
        let stream = session.connection().execute(sleep_sql, &[], cancel).await?;
        oxbase_driver::collect(stream).await
    }
    .await;
    let err = result.expect_err("the 30 s query was not cancelled");
    assert!(err.is_cancelled(), "{key}: {err}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{key}: cancel took {:?}",
        started.elapsed()
    );
    // The session is still usable after the cancel.
    let out = oxbase_driver::collect(
        session
            .connection()
            .execute("SELECT 1", &[], CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(out.result_sets.len(), 1);
    live.mgr.close(session.id()).await.unwrap();
    drop(session);
    assert_eq!(live.mgr.open_tunnel_count(), 0);
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn postgres_cancel_via_ssh() {
    cancel_open_via_ssh("postgres", Engine::Postgres).await;
    cancel_query_via_ssh("postgres", Engine::Postgres, "SELECT pg_sleep(30)").await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn mysql_cancel_via_ssh() {
    cancel_open_via_ssh("mysql", Engine::MySql).await;
    cancel_query_via_ssh("mysql", Engine::MySql, "SELECT SLEEP(30)").await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn mariadb_cancel_via_ssh() {
    cancel_open_via_ssh("mariadb", Engine::MariaDb).await;
    cancel_query_via_ssh("mariadb", Engine::MariaDb, "SELECT SLEEP(30)").await;
}

#[tokio::test]
#[ignore = "needs the fixture SQLite file location (tests/fixtures.toml)"]
async fn sqlite_file() {
    let live = Live::new();
    let path = root().join(string(live.engine("sqlite"), "path"));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    let p = ConnectionProfile::new("live sqlite", Engine::Sqlite, ProfileAddress::File { path });
    let info = live.exercise(&p).await;
    assert!(!info.tls_in_use && !info.via_ssh, "{info:?}");
}
