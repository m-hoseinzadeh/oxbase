//! Connection manager tests with the mock driver, an in-memory secret store and a fake tunnel
//! provider (no Docker, no network).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use oxbase_driver::mock::MockDriver;
use oxbase_driver::{Driver, Endpoint, KeySource, PemSource, ResultStream, SshAuth, Tx};
use oxbase_net::ssh::TunnelKey;
use oxbase_store::{
    Extra, KnownHostsPolicy, MemorySecretStore, PemRef, ProfileAddress, SecretSlot,
    SshAuthSettings, SshSettings, TlsMode, TlsSettings,
};
use oxbase_types::{ErrorCategory, Value};

use super::*;

const DB_PW: &str = "MARKER-db-pw-1d2c";
const SSH_PW: &str = "MARKER-ssh-pw-88aa";
const PASSPHRASE: &str = "MARKER-passphrase-31f0";
const SSH_KEY: &str = "MARKER-ssh-key-body-77e1";
const TLS_KEY: &str = "MARKER-tls-key-body-a5b6";
const PROVIDED: &str = "MARKER-provided-0c0c";

type Log = Arc<Mutex<Vec<String>>>;

fn push(log: &Log, entry: impl Into<String>) {
    log.lock().unwrap().push(entry.into());
}

// ---------------------------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------------------------

/// Wraps [`MockDriver`]: records the parameters it was given, can be slow, reports TLS for
/// `Require`+ modes, and logs connection closes.
struct RecordingDriver {
    mock: MockDriver,
    delay: Duration,
    last: Mutex<Option<ConnectParams>>,
    open_connections: Arc<AtomicUsize>,
    log: Log,
}

impl RecordingDriver {
    fn new(engine: Engine, log: Log) -> Self {
        Self {
            mock: MockDriver::new(engine).with_version(ServerVersion::new(16, 4, 0)),
            delay: Duration::ZERO,
            last: Mutex::new(None),
            open_connections: Arc::new(AtomicUsize::new(0)),
            log,
        }
    }

    fn last(&self) -> ConnectParams {
        self.last
            .lock()
            .unwrap()
            .clone()
            .expect("driver was called")
    }

    fn open(&self) -> usize {
        self.open_connections.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Driver for RecordingDriver {
    fn engine(&self) -> Engine {
        self.mock.engine()
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        *self.last.lock().unwrap() = Some(params.clone());
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let inner = self.mock.connect(params).await?;
        self.open_connections.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(RecordingConnection {
            inner,
            tls: params.tls.mode.requires_tls(),
            open_connections: self.open_connections.clone(),
            closed: std::sync::atomic::AtomicBool::new(false),
            log: self.log.clone(),
        }))
    }
}

struct RecordingConnection {
    inner: Box<dyn Connection>,
    tls: bool,
    open_connections: Arc<AtomicUsize>,
    closed: std::sync::atomic::AtomicBool,
    log: Log,
}

#[async_trait]
impl Connection for RecordingConnection {
    fn engine(&self) -> Engine {
        self.inner.engine()
    }

    fn server_version(&self) -> &ServerVersion {
        self.inner.server_version()
    }

    fn tls_in_use(&self) -> bool {
        self.tls
    }

    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        self.inner.execute(sql, params, cancel).await
    }

    async fn cancel_running(&self) -> Result<()> {
        self.inner.cancel_running().await
    }

    async fn begin(&self) -> Result<Box<dyn Tx>> {
        self.inner.begin().await
    }

    async fn close(&self) -> Result<()> {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.open_connections.fetch_sub(1, Ordering::SeqCst);
            push(&self.log, "connection closed");
        }
        self.inner.close().await
    }
}

/// Logs when a fake tunnel's resources are dropped (the tunnel closes).
struct TunnelGuard {
    log: Log,
}

impl Drop for TunnelGuard {
    fn drop(&mut self) {
        push(&self.log, "tunnel closed");
    }
}

/// Hands out fake loopback addresses; can be slow (honouring `cancel`).
struct FakeTunnels {
    opened: AtomicU16,
    delay: Duration,
    tokens: Mutex<Vec<CancellationToken>>,
    log: Log,
}

impl FakeTunnels {
    fn new(log: Log) -> Self {
        Self {
            opened: AtomicU16::new(0),
            delay: Duration::ZERO,
            tokens: Mutex::new(Vec::new()),
            log,
        }
    }

    fn opened(&self) -> u16 {
        self.opened.load(Ordering::SeqCst)
    }

    fn all_closed(&self) -> bool {
        self.tokens
            .lock()
            .unwrap()
            .iter()
            .all(CancellationToken::is_cancelled)
    }
}

#[async_trait]
impl TunnelProvider for FakeTunnels {
    async fn open(
        &self,
        ssh: &SshOptions,
        target: &Endpoint,
        cancel: CancellationToken,
    ) -> Result<TunnelHandle> {
        ssh.validate()?;
        if !self.delay.is_zero() {
            tokio::select! {
                () = cancel.cancelled() => return Err(Error::Cancelled),
                () = tokio::time::sleep(self.delay) => {}
            }
        }
        let n = self.opened.fetch_add(1, Ordering::SeqCst) + 1;
        let closed = CancellationToken::new();
        self.tokens.lock().unwrap().push(closed.clone());
        push(&self.log, format!("tunnel opened to {target}"));
        Ok(TunnelHandle::new(
            SocketAddr::from(([127, 0, 0, 1], 40000 + n)),
            TunnelKey::new(ssh, target),
            closed,
            TunnelGuard {
                log: self.log.clone(),
            },
        ))
    }
}

struct Harness {
    mgr: ConnectionManager,
    driver: Arc<RecordingDriver>,
    tunnels: Arc<FakeTunnels>,
    secrets: Arc<MemorySecretStore>,
    log: Log,
}

fn harness_with(configure: impl FnOnce(&mut RecordingDriver, &mut FakeTunnels)) -> Harness {
    let log = Log::default();
    let mut driver = RecordingDriver::new(Engine::Postgres, log.clone());
    let mut tunnels = FakeTunnels::new(log.clone());
    configure(&mut driver, &mut tunnels);
    let driver = Arc::new(driver);
    let tunnels = Arc::new(tunnels);
    let secrets = Arc::new(MemorySecretStore::new());
    let mut drivers = DriverRegistry::new();
    drivers.register(driver.clone());
    let mgr = ConnectionManager::new(drivers, secrets.clone(), tunnels.clone());
    Harness {
        mgr,
        driver,
        tunnels,
        secrets,
        log,
    }
}

fn harness() -> Harness {
    harness_with(|_, _| {})
}

fn log_entries(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

// ---------------------------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------------------------

/// PostgreSQL at db.internal:6543, password in the keychain, VerifyFull, no SSH.
fn direct_profile() -> ConnectionProfile {
    let mut p = ConnectionProfile::new(
        "direct",
        Engine::Postgres,
        ProfileAddress::Network {
            host: "db.internal".into(),
            port: Some(6543),
        },
    );
    p.user = Some("app".into());
    p.database = Some("appdb".into());
    p.password = Some(SecretRef::Keychain {
        key: p.secret_key(SecretSlot::Password),
    });
    p.tls = TlsSettings {
        mode: TlsMode::VerifyFull,
        ca_cert: Some(PemRef::File {
            path: "/etc/db/ca.pem".into(),
        }),
        ..TlsSettings::default()
    };
    p.connect_timeout_secs = Some(7);
    p.options.insert("sslnegotiation".into(), "direct".into());
    p
}

/// `direct_profile` through bastion:2222 with SSH password auth (keychain).
fn ssh_profile() -> ConnectionProfile {
    let mut p = direct_profile();
    p.name = "via ssh".into();
    p.ssh = Some(SshSettings {
        host: "bastion".into(),
        port: 2222,
        user: "ops".into(),
        auth: SshAuthSettings::Password {
            password: SecretRef::Keychain {
                key: p.secret_key(SecretSlot::SshPassword),
            },
        },
        known_hosts: KnownHostsPolicy::Fingerprint {
            fingerprint: "SHA256:abc".into(),
        },
        connect_timeout_secs: Some(9),
        keepalive_secs: Some(15),
        extra: Extra::new(),
    });
    p
}

async fn store(h: &Harness, p: &ConnectionProfile, slot: SecretSlot, value: &str) {
    h.secrets
        .set(&p.secret_key(slot), &Secret::new(value))
        .await
        .unwrap();
}

fn token() -> CancellationToken {
    CancellationToken::new()
}

// ---------------------------------------------------------------------------------------------
// Secret resolution
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn resolves_stored_secrets_into_params() {
    let h = harness();
    let p = direct_profile();
    store(&h, &p, SecretSlot::Password, DB_PW).await;

    let r = h
        .mgr
        .resolve(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    assert!(r.tunnel.is_none());
    let params = &r.params;
    assert_eq!(params.engine, Engine::Postgres);
    assert_eq!(params.host.as_deref(), Some("db.internal"));
    assert_eq!(params.port, Some(6543));
    assert_eq!(params.dial_addr, None);
    assert_eq!(params.user.as_deref(), Some("app"));
    assert_eq!(params.database.as_deref(), Some("appdb"));
    assert_eq!(params.password.as_ref().map(Secret::expose), Some(DB_PW));
    assert_eq!(params.tls.mode, TlsMode::VerifyFull);
    assert_eq!(
        params.tls.ca_cert,
        Some(PemSource::File("/etc/db/ca.pem".into()))
    );
    assert_eq!(params.connect_timeout, Some(Duration::from_secs(7)));
    assert_eq!(params.application_name.as_deref(), Some(APPLICATION_NAME));
    assert_eq!(params.options["sslnegotiation"], "direct");
    assert!(params.ssh.is_none());
    let dbg = format!("{r:?}");
    assert!(!dbg.contains(DB_PW), "{dbg}");
}

#[tokio::test]
async fn provided_secrets_answer_prompts_and_override_stored_ones() {
    let h = harness();
    let mut p = ssh_profile();
    store(&h, &p, SecretSlot::Password, DB_PW).await;
    store(&h, &p, SecretSlot::SshPassword, SSH_PW).await;

    // Stored values.
    let r = h
        .mgr
        .resolve(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    assert_eq!(r.params.password.as_ref().map(Secret::expose), Some(DB_PW));
    let Some(SshAuth::Password(pw)) = r.params.ssh.as_ref().map(|s| &s.auth) else {
        panic!("password auth expected");
    };
    assert_eq!(pw.expose(), SSH_PW);
    drop(r);

    // Provided values win.
    let provided = ProvidedSecrets {
        password: Some(Secret::new(PROVIDED)),
        ssh_password: Some(Secret::new("provided-ssh")),
        ssh_key_passphrase: None,
    };
    let r = h.mgr.resolve(&p, &provided, token()).await.unwrap();
    assert_eq!(
        r.params.password.as_ref().map(Secret::expose),
        Some(PROVIDED)
    );
    let Some(SshAuth::Password(pw)) = r.params.ssh.as_ref().map(|s| &s.auth) else {
        panic!("password auth expected");
    };
    assert_eq!(pw.expose(), "provided-ssh");
    drop(r);

    // Prompt fields need a provided value.
    p.password = Some(SecretRef::Prompt);
    let err = h
        .mgr
        .resolve(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Auth);
    assert!(err.to_string().contains("password"), "{err}");
    let r = h.mgr.resolve(&p, &provided, token()).await.unwrap();
    assert_eq!(
        r.params.password.as_ref().map(Secret::expose),
        Some(PROVIDED)
    );
    drop(r);

    // A profile without a password ref still takes a provided one; without either, none.
    p.password = None;
    let r = h.mgr.resolve(&p, &provided, token()).await.unwrap();
    assert_eq!(
        r.params.password.as_ref().map(Secret::expose),
        Some(PROVIDED)
    );
    drop(r);
    let r = h
        .mgr
        .resolve(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    assert!(r.params.password.is_none());
}

#[tokio::test]
async fn missing_keychain_entries_are_auth_errors_naming_the_field() {
    let h = harness();
    let p = ssh_profile();
    // Nothing stored at all.
    let err = h
        .mgr
        .resolve(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Auth);
    assert!(err.to_string().contains("saved password"), "{err}");
    assert!(
        err.to_string()
            .contains(p.secret_key(SecretSlot::Password).as_str()),
        "{err}"
    );

    store(&h, &p, SecretSlot::Password, DB_PW).await;
    let err = h
        .mgr
        .resolve(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Auth);
    assert!(err.to_string().contains("SSH password"), "{err}");
    assert!(!err.to_string().contains(DB_PW));
    assert_eq!(h.tunnels.opened(), 0, "no tunnel before secrets resolve");
}

#[tokio::test]
async fn keys_resolve_from_files_and_keychain() {
    let h = harness();
    let mut p = direct_profile();
    p.password = None;
    p.tls = TlsSettings {
        mode: TlsMode::VerifyCa,
        ca_cert: Some(PemRef::Inline {
            pem: "-----BEGIN CERTIFICATE-----".into(),
        }),
        client_cert: Some(PemRef::File {
            path: "/c/client.pem".into(),
        }),
        client_key: Some(KeyRef::Keychain {
            key: p.secret_key(SecretSlot::TlsClientKey),
        }),
        server_name: Some("  pg.example  ".into()),
        extra: Extra::new(),
    };
    p.ssh = Some(SshSettings {
        host: " bastion ".into(),
        port: 22,
        user: "ops".into(),
        auth: SshAuthSettings::PrivateKey {
            key: KeyRef::Keychain {
                key: p.secret_key(SecretSlot::SshPrivateKey),
            },
            passphrase: Some(SecretRef::Keychain {
                key: p.secret_key(SecretSlot::SshKeyPassphrase),
            }),
        },
        known_hosts: KnownHostsPolicy::default(),
        connect_timeout_secs: None,
        keepalive_secs: None,
        extra: Extra::new(),
    });
    store(&h, &p, SecretSlot::TlsClientKey, TLS_KEY).await;
    store(&h, &p, SecretSlot::SshPrivateKey, SSH_KEY).await;
    store(&h, &p, SecretSlot::SshKeyPassphrase, PASSPHRASE).await;

    let r = h
        .mgr
        .resolve(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    let tls = &r.params.tls;
    assert_eq!(tls.mode, TlsMode::VerifyCa);
    assert_eq!(
        tls.ca_cert,
        Some(PemSource::Pem("-----BEGIN CERTIFICATE-----".into()))
    );
    assert_eq!(
        tls.client_cert,
        Some(PemSource::File("/c/client.pem".into()))
    );
    assert_eq!(tls.client_key, Some(KeySource::Pem(Secret::new(TLS_KEY))));
    assert_eq!(tls.server_name.as_deref(), Some("pg.example"));
    assert_eq!(r.params.tls_server_name(), Some("pg.example"));
    let ssh = r.params.ssh.as_ref().unwrap();
    assert_eq!(ssh.host, "bastion");
    assert_eq!(
        ssh.auth,
        SshAuth::PrivateKey {
            key: KeySource::Pem(Secret::new(SSH_KEY)),
            passphrase: Some(Secret::new(PASSPHRASE)),
        }
    );
    let dbg = format!("{r:?}");
    for marker in [TLS_KEY, SSH_KEY, PASSPHRASE] {
        assert!(!dbg.contains(marker), "{marker} leaked: {dbg}");
    }
    drop(r);

    // Key files stay files; a prompt passphrase takes the provided one.
    let ssh = p.ssh.as_mut().unwrap();
    ssh.auth = SshAuthSettings::PrivateKey {
        key: KeyRef::File {
            path: "/home/ops/.ssh/id_ed25519".into(),
        },
        passphrase: Some(SecretRef::Prompt),
    };
    p.tls.client_key = Some(KeyRef::File {
        path: "/c/client-key.pem".into(),
    });
    let provided = ProvidedSecrets {
        ssh_key_passphrase: Some(Secret::new(PROVIDED)),
        ..ProvidedSecrets::none()
    };
    let r = h.mgr.resolve(&p, &provided, token()).await.unwrap();
    assert_eq!(
        r.params.tls.client_key,
        Some(KeySource::File(PathBuf::from("/c/client-key.pem")))
    );
    assert_eq!(
        r.params.ssh.as_ref().unwrap().auth,
        SshAuth::PrivateKey {
            key: KeySource::File("/home/ops/.ssh/id_ed25519".into()),
            passphrase: Some(Secret::new(PROVIDED)),
        }
    );
}

// ---------------------------------------------------------------------------------------------
// Tunnel / no tunnel
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn without_ssh_no_tunnel_is_requested() {
    let h = harness();
    let p = direct_profile();
    store(&h, &p, SecretSlot::Password, DB_PW).await;

    let info = h
        .mgr
        .test_connection(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    assert_eq!(info.server_version, ServerVersion::new(16, 4, 0));
    assert!(!info.via_ssh);
    assert!(info.tls_in_use, "VerifyFull reports TLS");
    assert_eq!(h.tunnels.opened(), 0);
    let params = h.driver.last();
    assert_eq!(params.dial_addr, None);
    assert_eq!(params.dial_endpoint(), params.server_endpoint());
    assert_eq!(
        params.dial_endpoint(),
        Some(Endpoint::new("db.internal", 6543))
    );
    // test_connection leaves nothing open.
    assert_eq!(h.driver.open(), 0);
    assert_eq!(h.mgr.list_open(), Vec::<SessionSummary>::new());
}

#[tokio::test]
async fn ssh_dials_the_tunnel_but_keeps_the_server_name() {
    let h = harness();
    let p = ssh_profile();
    store(&h, &p, SecretSlot::Password, DB_PW).await;
    store(&h, &p, SecretSlot::SshPassword, SSH_PW).await;

    let info = h
        .mgr
        .test_connection(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    assert!(info.via_ssh);
    assert!(info.tls_in_use);
    assert_eq!(h.tunnels.opened(), 1);

    let params = h.driver.last();
    let tunnel_addr = SocketAddr::from(([127, 0, 0, 1], 40001));
    assert_eq!(params.dial_addr, Some(tunnel_addr));
    assert_eq!(params.dial_endpoint(), Some(Endpoint::from(tunnel_addr)));
    assert_eq!(params.host.as_deref(), Some("db.internal"));
    assert_eq!(
        params.server_endpoint(),
        Some(Endpoint::new("db.internal", 6543))
    );
    assert_eq!(params.tls_server_name(), Some("db.internal"));
    let ssh = params.ssh.as_ref().unwrap();
    assert_eq!(ssh.endpoint(), Endpoint::new("bastion", 2222));
    assert_eq!(ssh.connect_timeout, Some(Duration::from_secs(9)));
    assert_eq!(ssh.keepalive_interval, Some(Duration::from_secs(15)));
    assert_eq!(
        ssh.known_hosts,
        KnownHostsPolicy::Fingerprint {
            fingerprint: "SHA256:abc".into()
        }
    );
    // The tunnel forwarded to the server endpoint and is closed after the test, after the
    // connection.
    assert_eq!(h.mgr.open_tunnel_count(), 0);
    assert!(h.tunnels.all_closed());
    assert_eq!(
        log_entries(&h.log),
        vec![
            "tunnel opened to db.internal:6543",
            "connection closed",
            "tunnel closed"
        ]
    );
}

#[tokio::test]
async fn ssh_on_default_port_targets_the_engine_default() {
    let h = harness();
    let mut p = ssh_profile();
    p.address = ProfileAddress::Network {
        host: "postgres".into(),
        port: None,
    };
    store(&h, &p, SecretSlot::Password, DB_PW).await;
    store(&h, &p, SecretSlot::SshPassword, SSH_PW).await;
    let s = h
        .mgr
        .open(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    assert_eq!(
        s.tunnel().unwrap().key().target,
        Endpoint::new("postgres", 5432)
    );
    h.mgr.close(s.id()).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Session lifecycle
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn session_lifecycle_shares_and_releases_tunnels() {
    let hx = harness();
    let p = ssh_profile();
    store(&hx, &p, SecretSlot::Password, DB_PW).await;
    store(&hx, &p, SecretSlot::SshPassword, SSH_PW).await;
    let mut direct = direct_profile();
    direct.password = None;

    let a = hx
        .mgr
        .open(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    let b = hx
        .mgr
        .open(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    let c = hx
        .mgr
        .open(&direct, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    assert_ne!(a.id(), b.id());
    assert_eq!(hx.tunnels.opened(), 1, "same SSH key shares one tunnel");
    assert!(Arc::ptr_eq(a.tunnel().unwrap(), b.tunnel().unwrap()));
    assert!(c.tunnel().is_none());
    assert!(a.info().via_ssh && !c.info().via_ssh);
    assert_eq!(hx.driver.open(), 3);

    let listed = hx.mgr.list_open();
    assert_eq!(
        listed.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![a.id(), b.id(), c.id()]
    );
    assert_eq!(listed[0].profile_id, p.id);
    assert_eq!(listed[0].profile_name, "via ssh");
    assert_eq!(listed[0].engine, Engine::Postgres);
    assert_eq!(hx.mgr.get(b.id()).unwrap().id(), b.id());

    // The session works.
    let rows = oxbase_driver::collect(
        a.connection()
            .execute("SELECT 1", &[], token())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(rows.result_sets.len(), 1);

    // Closing one of two sessions keeps the shared tunnel.
    let a_id = a.id();
    drop(a);
    hx.mgr.close(a_id).await.unwrap();
    assert_eq!(hx.mgr.open_tunnel_count(), 1);
    assert!(!hx.tunnels.all_closed());

    // Closing the last one releases it (the caller dropped its handle).
    let b_conn = b.connection().clone();
    let b_id = b.id();
    drop(b);
    hx.mgr.close(b_id).await.unwrap();
    drop(b_conn);
    assert_eq!(hx.mgr.open_tunnel_count(), 0);
    assert!(hx.tunnels.all_closed());
    let log = log_entries(&hx.log);
    assert_eq!(log.last().map(String::as_str), Some("tunnel closed"));
    assert_eq!(
        log.iter().filter(|e| *e == "connection closed").count(),
        2,
        "{log:?}"
    );

    // A closed session's connection refuses work; unknown ids are NotFound.
    assert!(
        c.connection()
            .execute("SELECT 1", &[], token())
            .await
            .is_ok()
    );
    hx.mgr.close(c.id()).await.unwrap();
    assert!(
        c.connection()
            .execute("SELECT 1", &[], token())
            .await
            .is_err()
    );
    let err = hx.mgr.close(c.id()).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);
    assert_eq!(hx.mgr.list_open(), Vec::<SessionSummary>::new());
    assert_eq!(hx.driver.open(), 0);

    // close_all closes everything.
    hx.mgr
        .open(&direct, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    hx.mgr
        .open(&direct, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    assert_eq!(hx.mgr.list_open().len(), 2);
    hx.mgr.close_all().await.unwrap();
    assert_eq!(hx.mgr.list_open(), Vec::<SessionSummary>::new());
    assert_eq!(hx.driver.open(), 0);
}

#[tokio::test]
async fn failures_after_the_tunnel_opened_release_it() {
    let h = harness();
    let mut p = ssh_profile();
    p.user = Some("denied".into());
    store(&h, &p, SecretSlot::Password, DB_PW).await;
    store(&h, &p, SecretSlot::SshPassword, SSH_PW).await;

    let err = h
        .mgr
        .open(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Auth);
    assert_eq!(h.tunnels.opened(), 1);
    assert_eq!(h.mgr.open_tunnel_count(), 0);
    assert!(h.tunnels.all_closed());
    assert_eq!(h.mgr.list_open(), Vec::<SessionSummary>::new());
}

#[tokio::test]
async fn invalid_profiles_and_missing_drivers_fail_before_any_work() {
    let h = harness();
    let mut p = ssh_profile();
    p.name = "  ".into();
    let err = h
        .mgr
        .open(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);

    let mut mysql = ssh_profile();
    mysql.engine = Engine::MySql;
    let err = h
        .mgr
        .test_connection(&mysql, &ProvidedSecrets::none(), token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Unsupported);
    assert_eq!(h.tunnels.opened(), 0);
}

#[tokio::test]
async fn latency_covers_the_whole_connect() {
    let h = harness_with(|d, _| d.delay = Duration::from_millis(40));
    let mut p = direct_profile();
    p.password = None;
    let info = h
        .mgr
        .test_connection(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    assert!(info.latency_ms >= 40, "{info:?}");
}

// ---------------------------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn cancelled_before_start_does_nothing() {
    let h = harness();
    let p = ssh_profile();
    store(&h, &p, SecretSlot::Password, DB_PW).await;
    store(&h, &p, SecretSlot::SshPassword, SSH_PW).await;
    let cancel = token();
    cancel.cancel();
    for err in [
        h.mgr
            .resolve(&p, &ProvidedSecrets::none(), cancel.clone())
            .await
            .unwrap_err(),
        h.mgr
            .test_connection(&p, &ProvidedSecrets::none(), cancel.clone())
            .await
            .unwrap_err(),
        h.mgr
            .open(&p, &ProvidedSecrets::none(), cancel.clone())
            .await
            .unwrap_err(),
    ] {
        assert!(err.is_cancelled(), "{err}");
    }
    assert_eq!(h.tunnels.opened(), 0);
    assert!(h.driver.last.lock().unwrap().is_none());
}

#[tokio::test]
async fn cancel_while_the_tunnel_opens() {
    let h = harness_with(|_, t| t.delay = Duration::from_secs(30));
    let p = ssh_profile();
    store(&h, &p, SecretSlot::Password, DB_PW).await;
    store(&h, &p, SecretSlot::SshPassword, SSH_PW).await;
    let cancel = token();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let err = h
        .mgr
        .open(&p, &ProvidedSecrets::none(), cancel)
        .await
        .unwrap_err();
    assert!(err.is_cancelled(), "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(h.tunnels.opened(), 0);
    assert_eq!(h.mgr.list_open(), Vec::<SessionSummary>::new());
}

#[tokio::test]
async fn cancel_while_the_driver_connects_releases_the_tunnel() {
    let h = harness_with(|d, _| d.delay = Duration::from_secs(30));
    let p = ssh_profile();
    store(&h, &p, SecretSlot::Password, DB_PW).await;
    store(&h, &p, SecretSlot::SshPassword, SSH_PW).await;
    let cancel = token();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let err = h
        .mgr
        .test_connection(&p, &ProvidedSecrets::none(), cancel)
        .await
        .unwrap_err();
    assert!(err.is_cancelled(), "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(h.tunnels.opened(), 1);
    assert_eq!(h.mgr.open_tunnel_count(), 0);
    assert!(h.tunnels.all_closed());
    assert_eq!(h.driver.open(), 0);
    assert_eq!(h.mgr.list_open(), Vec::<SessionSummary>::new());
}

#[tokio::test]
async fn cancelling_after_open_does_not_touch_the_session() {
    let h = harness();
    let mut p = direct_profile();
    p.password = None;
    let cancel = token();
    let s = h
        .mgr
        .open(&p, &ProvidedSecrets::none(), cancel.clone())
        .await
        .unwrap();
    cancel.cancel();
    assert!(
        s.connection()
            .execute("SELECT 1", &[], token())
            .await
            .is_ok()
    );
    assert_eq!(h.mgr.list_open().len(), 1);
    h.mgr.close(s.id()).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Leaks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn secrets_never_appear_in_errors_or_debug_output() {
    let h = harness();
    let mut p = ssh_profile();
    store(&h, &p, SecretSlot::Password, DB_PW).await;
    store(&h, &p, SecretSlot::SshPassword, SSH_PW).await;

    let mut seen = Vec::new();
    let s = h
        .mgr
        .open(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    seen.push(format!("{s:?} {:?} {:?}", h.mgr, h.mgr.list_open()));
    seen.push(serde_json::to_string(&h.mgr.list_open()).unwrap());
    let r = h
        .mgr
        .resolve(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    seen.push(format!("{r:?} {r:#?}"));
    drop(r);
    h.mgr.close(s.id()).await.unwrap();

    p.user = Some("denied".into());
    let provided = ProvidedSecrets {
        password: Some(Secret::new(PROVIDED)),
        ssh_password: Some(Secret::new(PROVIDED)),
        ssh_key_passphrase: Some(Secret::new(PROVIDED)),
    };
    seen.push(format!("{provided:?}"));
    let err = h.mgr.open(&p, &provided, token()).await.unwrap_err();
    seen.push(format!("{err} {err:?}"));
    p.password = Some(SecretRef::Prompt);
    let err = h
        .mgr
        .test_connection(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap_err();
    seen.push(format!("{err} {err:?}"));

    let all = seen.join("\n");
    for marker in [DB_PW, SSH_PW, PROVIDED] {
        assert!(!all.contains(marker), "{marker} leaked:\n{all}");
    }
}

// ---------------------------------------------------------------------------------------------
// Tunnel sharing, keychain scope, deadline
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn tunnels_are_shared_only_within_one_policy_and_credential_scope() {
    let h = harness();
    let p = ssh_profile();
    store(&h, &p, SecretSlot::Password, DB_PW).await;
    store(&h, &p, SecretSlot::SshPassword, SSH_PW).await;

    let first = h
        .mgr
        .open(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    assert_eq!(h.tunnels.opened(), 1);

    // A test always authenticates on a tunnel of its own, even with a live pooled one.
    h.mgr
        .test_connection(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    assert_eq!(
        h.tunnels.opened(),
        2,
        "test_connection reused a pooled tunnel"
    );

    // A stricter host-key policy never rides on a tunnel checked under another one.
    let mut strict = p.clone();
    strict.ssh.as_mut().unwrap().known_hosts = KnownHostsPolicy::Strict { path: None };
    let strict_s = h
        .mgr
        .open(&strict, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    assert_eq!(
        h.tunnels.opened(),
        3,
        "host-key policy must be part of the share key"
    );
    assert!(!Arc::ptr_eq(
        first.tunnel().unwrap(),
        strict_s.tunnel().unwrap()
    ));

    // A caller-supplied (possibly wrong) SSH password gets a tunnel of its own.
    let provided = ProvidedSecrets {
        ssh_password: Some(Secret::new("MARKER-maybe-wrong-ssh-pw")),
        ..ProvidedSecrets::none()
    };
    let prompted = h.mgr.open(&p, &provided, token()).await.unwrap();
    assert_eq!(
        h.tunnels.opened(),
        4,
        "provided SSH secrets must authenticate"
    );
    assert!(!Arc::ptr_eq(
        first.tunnel().unwrap(),
        prompted.tunnel().unwrap()
    ));

    // Other credential references (a key file instead of the password) do not share either.
    let mut keyed = p.clone();
    keyed.ssh.as_mut().unwrap().auth = SshAuthSettings::PrivateKey {
        key: KeyRef::File {
            path: "/home/ops/.ssh/id_ed25519".into(),
        },
        passphrase: None,
    };
    let keyed_s = h
        .mgr
        .open(&keyed, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    assert_eq!(h.tunnels.opened(), 5);
    assert!(!Arc::ptr_eq(
        first.tunnel().unwrap(),
        keyed_s.tunnel().unwrap()
    ));

    // The same profile and stored credentials share.
    let again = h
        .mgr
        .open(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    assert_eq!(h.tunnels.opened(), 5);
    assert!(Arc::ptr_eq(
        first.tunnel().unwrap(),
        again.tunnel().unwrap()
    ));
    assert_eq!(h.mgr.open_tunnel_count(), 4);

    for s in [first, strict_s, prompted, keyed_s, again] {
        h.mgr.close(s.id()).await.unwrap();
    }
    assert_eq!(h.mgr.open_tunnel_count(), 0);
    assert!(h.tunnels.all_closed());
}

#[tokio::test]
async fn a_profile_cannot_read_another_profiles_keychain_entries() {
    let h = harness();
    let victim = direct_profile();
    store(&h, &victim, SecretSlot::Password, DB_PW).await;

    let mut p = direct_profile();
    p.password = Some(SecretRef::Keychain {
        key: victim.secret_key(SecretSlot::Password),
    });
    let err = h
        .mgr
        .test_connection(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput, "{err}");
    assert!(err.to_string().contains("another connection"), "{err}");
    assert!(!err.to_string().contains(DB_PW));
    assert!(h.driver.last.lock().unwrap().is_none(), "driver was called");

    // The same holds for keys (SSH private key in another profile's slot).
    let mut p = ssh_profile();
    store(&h, &p, SecretSlot::Password, DB_PW).await;
    p.ssh.as_mut().unwrap().auth = SshAuthSettings::PrivateKey {
        key: KeyRef::Keychain {
            key: victim.secret_key(SecretSlot::SshPrivateKey),
        },
        passphrase: None,
    };
    let err = h
        .mgr
        .open(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput, "{err}");
    assert_eq!(h.tunnels.opened(), 0);
}

#[tokio::test]
async fn the_deadline_covers_the_tunnel_step() {
    let h = harness_with(|_, t| t.delay = Duration::from_secs(30));
    let mut p = ssh_profile();
    p.connect_timeout_secs = Some(0);
    p.ssh.as_mut().unwrap().connect_timeout_secs = Some(0);
    store(&h, &p, SecretSlot::Password, DB_PW).await;
    store(&h, &p, SecretSlot::SshPassword, SSH_PW).await;

    let started = Instant::now();
    let err = h
        .mgr
        .open(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Timeout, "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(h.tunnels.opened(), 0);
    assert_eq!(h.mgr.open_tunnel_count(), 0);
    assert!(h.driver.last.lock().unwrap().is_none(), "driver was called");
}

#[tokio::test]
async fn the_deadline_covers_a_slow_driver() {
    let h = harness_with(|d, _| d.delay = Duration::from_secs(30));
    let mut p = ssh_profile();
    p.connect_timeout_secs = Some(0);
    p.ssh.as_mut().unwrap().connect_timeout_secs = Some(0);
    store(&h, &p, SecretSlot::Password, DB_PW).await;
    store(&h, &p, SecretSlot::SshPassword, SSH_PW).await;

    let err = h
        .mgr
        .test_connection(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Timeout, "{err}");
    assert_eq!(h.tunnels.opened(), 1);
    assert_eq!(
        h.mgr.open_tunnel_count(),
        0,
        "tunnel released after the timeout"
    );
    assert!(h.tunnels.all_closed());
    assert_eq!(h.driver.open(), 0);
}

// ---------------------------------------------------------------------------------------------
// M2: child sessions
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn children_reuse_params_and_tunnel_and_close_with_their_base() {
    let hx = harness();
    let p = ssh_profile();
    store(&hx, &p, SecretSlot::Password, DB_PW).await;
    store(&hx, &p, SecretSlot::SshPassword, SSH_PW).await;
    let base = hx
        .mgr
        .open(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    let base_params = hx.driver.last();
    // The keychain is emptied: a child must not ask for the secrets again.
    hx.secrets
        .delete(&p.secret_key(SecretSlot::Password))
        .await
        .unwrap();

    let child = hx
        .mgr
        .open_child(base.id(), Some("other_db"), token())
        .await
        .unwrap();
    let params = hx.driver.last();
    assert_eq!(params.database.as_deref(), Some("other_db"));
    assert_eq!(params.password, base_params.password);
    assert_eq!(params.dial_addr, base_params.dial_addr);
    assert_eq!(params.host, base_params.host);
    assert_eq!(
        hx.tunnels.opened(),
        1,
        "the child rides on the base's tunnel"
    );
    assert!(Arc::ptr_eq(base.tunnel().unwrap(), child.tunnel().unwrap()));
    assert_eq!(child.summary().parent, Some(base.id()));
    assert_eq!(child.summary().database.as_deref(), Some("other_db"));
    assert_eq!(child.summary().profile_id, p.id);
    assert!(child.info().via_ssh);

    // Without a database the child goes to the base's.
    let same = hx.mgr.open_child(base.id(), None, token()).await.unwrap();
    assert_eq!(hx.driver.last().database.as_deref(), Some("appdb"));
    assert_eq!(same.summary().database.as_deref(), Some("appdb"));

    // Children are found but not listed.
    assert!(hx.mgr.get(child.id()).is_some());
    assert_eq!(
        hx.mgr.list_open().iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![base.id()]
    );
    assert_eq!(hx.driver.open(), 3);

    // A child of a child, and of an unknown session, are refused.
    let err = hx
        .mgr
        .open_child(child.id(), None, token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    let err = hx
        .mgr
        .open_child(SessionId::new(), None, token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);

    // Closing a child closes only the child.
    hx.mgr.close(same.id()).await.unwrap();
    assert_eq!(hx.driver.open(), 2);
    assert!(hx.mgr.get(base.id()).is_some());

    // Closing the base closes its children first.
    hx.mgr.close(base.id()).await.unwrap();
    assert_eq!(hx.driver.open(), 0);
    assert!(hx.mgr.get(child.id()).is_none());
    let err = hx.mgr.close(child.id()).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);
    let err = hx
        .mgr
        .open_child(base.id(), None, token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);
    drop((base, child, same));
    assert!(
        hx.tunnels.all_closed(),
        "the tunnel closes with its last user"
    );
}

#[tokio::test]
async fn children_honour_cancel_and_close_all() {
    let hx = harness();
    let mut p = direct_profile();
    p.password = None;
    let base = hx
        .mgr
        .open(&p, &ProvidedSecrets::none(), token())
        .await
        .unwrap();
    let cancelled = token();
    cancelled.cancel();
    let err = hx
        .mgr
        .open_child(base.id(), None, cancelled)
        .await
        .unwrap_err();
    assert!(err.is_cancelled());
    assert_eq!(hx.driver.open(), 1);

    let _child = hx.mgr.open_child(base.id(), None, token()).await.unwrap();
    assert_eq!(hx.driver.open(), 2);
    hx.mgr.close_all().await.unwrap();
    assert_eq!(hx.driver.open(), 0);
    assert_eq!(hx.mgr.list_open(), Vec::<SessionSummary>::new());
    // Secrets of the base are never in the manager's Debug output.
    assert!(!format!("{:?}", hx.mgr).contains(DB_PW));
}
