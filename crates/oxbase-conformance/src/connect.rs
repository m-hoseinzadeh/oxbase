//! M1 connection checks: one row per engine fixture, one column per [`ConnCheck`].
//!
//! Every check goes through the real stack the app uses: [`ConnectionManager`] with
//! [`default_drivers`], a [`MemorySecretStore`] holding the fixture passwords, and the russh
//! tunnel provider. SQL Server runs them since M4, MongoDB and Redis since M7 (every engine has
//! a driver now); an unreachable fixture reports its checks as skipped.
//!
//! | Check | What passes |
//! |---|---|
//! | `connect` | plain connect as the fixture user; the server version equals `version_prefix` |
//! | `tls` | `VerifyFull` with the fixture CA reports `tls_in_use` and a wrong `server_name` is refused by certificate verification ([`names_tls_failure`]); as the TLS-only user (`tls_user`, whose plaintext login is refused), else as the fixture user on the TLS-only port (`tls_port`, Redis: plaintext to it is refused) or on the shared port (MongoDB `allowTLS`: plaintext stays allowed) |
//! | `ssh` | through the bastion (password auth, `Strict` known_hosts) to the in-network address |
//! | `tls_over_ssh` | through the bastion (encrypted-key auth) to the `<name>_tls` target if there is one (Redis), else `<name>`, with `VerifyFull` against the service name while dialling `127.0.0.1`; a wrong `server_name` is refused |
//! | `cancel` | a long statement ends with `Cancelled`, and the same session answers its ping, within 2 s of the request, via the token and via `cancel_running`; MongoDB also: `currentOp` lists the statement before the cancel and no longer within 2 s after the ping (its client pools connections, so the ping alone does not prove a server-side kill) |
//! | `stream_200k` | 200 000 rows arrive complete in more than one batch |
//!
//! The statements per engine ([`Statements`]): SQL engines ping with `SELECT 1`, sleep 30 s and
//! generate 200 000 rows. MongoDB runs JSON requests of its driver (`docs/m7-contracts.md`
//! §4.2): a `ping` command, a `find` on `[m7.mongodb] big_collection` with a `$where` busy loop
//! (minutes, tagged with a marker field so `currentOp` finds exactly it), and that whole
//! collection. Redis runs console lines: `PING`, `BLPOP` on a key that
//! never exists (cancelled by `CLIENT UNBLOCK`), and `LRANGE <[m7.redis] big_list> 0 -1`.

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use serde::{Deserialize, Serialize};

use oxbase_driver::{CancellationToken, Connection, ResultItem};
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_services::{ConnectionManager, ConnectionProfile, ProvidedSecrets, default_drivers};
use oxbase_store::{
    Extra, KeyRef, KnownHostsPolicy, MemorySecretStore, PemRef, ProfileAddress, Secret, SecretRef,
    SecretSlot, SecretStore, SshAuthSettings, SshSettings, TlsMode, TlsSettings,
};
use oxbase_types::{Engine, ServerVersion};
use serde_json::json;

use crate::fixtures::{EngineFixture, Fixtures, ResolvedEngine};
use crate::matrix::CellStatus;
use crate::probe::Target;

/// Longest a cancelled statement may take to end after the cancel request.
pub const CANCEL_DEADLINE: Duration = Duration::from_secs(2);
/// Rows the streaming check reads.
pub const STREAM_ROWS: usize = 200_000;
/// Upper bound for one whole check, so a hung server cannot hang the run.
const CHECK_TIMEOUT: Duration = Duration::from_secs(90);
/// How long the long statement runs before the cancel is sent.
const CANCEL_AFTER: Duration = Duration::from_millis(300);
/// A `server_name` no fixture certificate covers.
const WRONG_SERVER_NAME: &str = "wrong-name.oxbase.invalid";

/// One connection-level check (a column of the second table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnCheck {
    Connect,
    Tls,
    Ssh,
    TlsOverSsh,
    Cancel,
    #[serde(rename = "stream_200k")]
    Stream200k,
}

impl ConnCheck {
    pub const ALL: [Self; 6] = [
        Self::Connect,
        Self::Tls,
        Self::Ssh,
        Self::TlsOverSsh,
        Self::Cancel,
        Self::Stream200k,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::Ssh => "ssh",
            Self::TlsOverSsh => "tls_over_ssh",
            Self::Cancel => "cancel",
            Self::Stream200k => "stream_200k",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Connect => "plain connect; server version matches the fixture",
            Self::Tls => {
                "VerifyFull with the fixture CA (tls_in_use; the TLS-only user or port where the \
                 fixture has one, whose plaintext use is refused); a wrong server name is refused"
            }
            Self::Ssh => "connect through the SSH bastion (password auth, Strict known_hosts)",
            Self::TlsOverSsh => {
                "VerifyFull through the SSH bastion (encrypted-key auth): the certificate matches \
                 the service name, not the tunnel address; a wrong server name is refused"
            }
            Self::Cancel => {
                "a long statement ends with Cancelled and the session answers again within 2 s \
                 (token and cancel_running)"
            }
            Self::Stream200k => "200k rows stream in more than one batch",
        }
    }

    /// Whether the check applies to `engine` at all (TLS and SSH need a network server).
    pub fn applies(self, engine: Engine) -> bool {
        match self {
            Self::Tls | Self::Ssh | Self::TlsOverSsh => engine.is_networked(),
            Self::Connect | Self::Cancel | Self::Stream200k => true,
        }
    }
}

impl fmt::Display for ConnCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of one (engine, connection check) pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnCell {
    /// The fixture name (`[engines.<name>]`).
    pub engine: String,
    pub engine_kind: Engine,
    pub check: ConnCheck,
    pub status: CellStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

impl ConnCell {
    fn new(
        e: &ResolvedEngine,
        check: ConnCheck,
        status: CellStatus,
        detail: Option<String>,
    ) -> Self {
        Self {
            engine: e.name.clone(),
            engine_kind: e.engine,
            check,
            status,
            detail,
            elapsed_ms: None,
        }
    }

    /// The text of this cell in the rendered table.
    pub fn label(&self) -> String {
        match self.status {
            CellStatus::Pass => "pass".to_owned(),
            CellStatus::Fail => "FAIL".to_owned(),
            CellStatus::Pending => "pending".to_owned(),
            CellStatus::Skipped => "skipped".to_owned(),
            CellStatus::NotApplicable => "n/a".to_owned(),
        }
    }
}

/// The cells of an engine that cannot run any check: an unreachable fixture (skipped). `None`
/// when the engine's checks should run (every engine has a driver since M7).
pub fn static_cells(e: &ResolvedEngine, reachable: bool) -> Option<Vec<ConnCell>> {
    if reachable {
        return None;
    }
    Some(
        ConnCheck::ALL
            .iter()
            .map(|&check| {
                if check.applies(e.engine) {
                    ConnCell::new(
                        e,
                        check,
                        CellStatus::Skipped,
                        Some("fixture unreachable".to_owned()),
                    )
                } else {
                    ConnCell::new(e, check, CellStatus::NotApplicable, None)
                }
            })
            .collect(),
    )
}

/// Runs every connection check for `engines` (`reachable[i]` belongs to `engines[i]`). Engines
/// run concurrently; the checks of one engine run in order.
pub fn run_all(
    fixtures: &Fixtures,
    engines: &[ResolvedEngine],
    reachable: &[bool],
) -> Vec<ConnCell> {
    let mut out = Vec::new();
    let mut todo = Vec::new();
    for (e, &up) in engines.iter().zip(reachable) {
        if let Some(cells) = static_cells(e, up) {
            out.push(cells);
        } else {
            todo.push(e);
            out.push(Vec::new());
        }
    }
    if !todo.is_empty() {
        let results = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(async {
                let tasks: Vec<_> = todo
                    .iter()
                    .map(|&e| {
                        // One manager per engine, so its tunnel bookkeeping is the engine's own.
                        let ctx = Ctx::new(fixtures);
                        let e = e.clone();
                        tokio::spawn(async move { ctx.engine(&e).await })
                    })
                    .collect();
                let mut results = Vec::with_capacity(tasks.len());
                for (task, e) in tasks.into_iter().zip(&todo) {
                    results.push(task.await.unwrap_or_else(|err| {
                        failed_all(e, &format!("check task panicked: {err}"))
                    }));
                }
                results
            }),
            Err(err) => todo
                .iter()
                .map(|e| failed_all(e, &format!("cannot start the tokio runtime: {err}")))
                .collect(),
        };
        let mut results = results.into_iter();
        for cells in &mut out {
            if cells.is_empty() {
                *cells = results.next().unwrap_or_default();
            }
        }
    }
    out.into_iter().flatten().collect()
}

fn failed_all(e: &ResolvedEngine, why: &str) -> Vec<ConnCell> {
    ConnCheck::ALL
        .iter()
        .map(|&check| {
            if check.applies(e.engine) {
                ConnCell::new(e, check, CellStatus::Fail, Some(why.to_owned()))
            } else {
                ConnCell::new(e, check, CellStatus::NotApplicable, None)
            }
        })
        .collect()
}

/// Shared state of one run: the manager and the fixture definitions. The M2 fetch checks
/// ([`crate::fetch`]) open their sessions through it too.
pub(crate) struct Ctx {
    /// Shared so the M8 services ([`crate::m8`]) can hold it too.
    pub(crate) mgr: Arc<ConnectionManager>,
    secrets: Arc<MemorySecretStore>,
    // Boxed: every check future holds a Ctx, and the fixtures file is large.
    fixtures: Box<Fixtures>,
}

type CheckResult = Result<String, String>;

impl Ctx {
    pub(crate) fn new(fixtures: &Fixtures) -> Self {
        let secrets = Arc::new(MemorySecretStore::new());
        let mgr = Arc::new(ConnectionManager::new(
            default_drivers(),
            secrets.clone(),
            Arc::new(RusshTunnelProvider::new()),
        ));
        Self {
            mgr,
            secrets,
            fixtures: Box::new(fixtures.clone()),
        }
    }

    async fn engine(&self, e: &ResolvedEngine) -> Vec<ConnCell> {
        let mut cells = Vec::with_capacity(ConnCheck::ALL.len());
        for check in ConnCheck::ALL {
            if !check.applies(e.engine) {
                cells.push(ConnCell::new(e, check, CellStatus::NotApplicable, None));
                continue;
            }
            let started = Instant::now();
            let outcome = match tokio::time::timeout(CHECK_TIMEOUT, self.check(e, check)).await {
                Ok(r) => r,
                Err(_) => Err(format!(
                    "did not finish within {} s",
                    CHECK_TIMEOUT.as_secs()
                )),
            };
            let (status, detail) = match outcome {
                Ok(d) => (CellStatus::Pass, d),
                Err(d) => (CellStatus::Fail, d),
            };
            let mut cell = ConnCell::new(e, check, status, Some(detail));
            cell.elapsed_ms = Some(millis(started.elapsed()));
            cells.push(cell);
        }
        cells
    }

    fn check<'a>(
        &'a self,
        e: &'a ResolvedEngine,
        check: ConnCheck,
    ) -> std::pin::Pin<Box<dyn Future<Output = CheckResult> + Send + 'a>> {
        match check {
            ConnCheck::Connect => Box::pin(self.connect(e)),
            ConnCheck::Tls => Box::pin(self.tls(e)),
            ConnCheck::Ssh => Box::pin(self.ssh(e)),
            ConnCheck::TlsOverSsh => Box::pin(self.tls_over_ssh(e)),
            ConnCheck::Cancel => Box::pin(self.cancel(e)),
            ConnCheck::Stream200k => Box::pin(self.stream(e)),
        }
    }

    fn fixture(&self, e: &ResolvedEngine) -> Result<&EngineFixture, String> {
        self.fixtures
            .engines
            .get(&e.name)
            .ok_or_else(|| format!("no [engines.{}] in the fixtures file", e.name))
    }

    /// A direct profile as the fixture user (`tls_user = false`) or the TLS-only user.
    pub(crate) async fn profile(
        &self,
        e: &ResolvedEngine,
        tls_user: bool,
    ) -> Result<ConnectionProfile, String> {
        let fx = self.fixture(e)?;
        let address = match &e.target {
            Target::Tcp { host, port } => ProfileAddress::Network {
                host: host.clone(),
                port: Some(*port),
            },
            Target::File { path } => ProfileAddress::File { path: path.clone() },
        };
        let mut p = ConnectionProfile::new(format!("conformance {}", e.name), e.engine, address);
        p.connect_timeout_secs = Some(15);
        p.tls = TlsSettings {
            mode: TlsMode::Disable,
            ..TlsSettings::default()
        };
        if !e.engine.is_networked() {
            return Ok(p);
        }
        let (user, password, database) = if tls_user {
            let c = fx
                .tls_user
                .as_ref()
                .ok_or_else(|| format!("[engines.{}.tls_user] is missing", e.name))?;
            (
                Some(c.user.clone()),
                Some(c.password.clone()),
                c.database.clone(),
            )
        } else {
            (fx.user.clone(), fx.password.clone(), fx.database.clone())
        };
        p.user = user;
        p.database = database;
        // MongoDB: the database the fixture user is defined in (profile option, §7.1 of the
        // M7 contracts).
        if let Some(source) = fx.extra.get("auth_source").and_then(toml::Value::as_str) {
            p.options
                .insert("auth_source".to_owned(), source.to_owned());
        }
        if let Some(pw) = password {
            let key = p.secret_key(SecretSlot::Password);
            self.secrets
                .set(&key, &Secret::new(pw))
                .await
                .map_err(|err| err.to_string())?;
            p.password = Some(SecretRef::Keychain { key });
        }
        Ok(p)
    }

    fn verify_full(&self) -> Result<TlsSettings, String> {
        let tls = self
            .fixtures
            .tls
            .as_ref()
            .ok_or("no [tls] in the fixtures file")?;
        let ca = tls.ca_cert_path();
        if !ca.is_file() {
            return Err(format!(
                "fixture CA {} is missing (scripts/fixtures-up.sh creates it)",
                ca.display()
            ));
        }
        Ok(TlsSettings {
            mode: TlsMode::VerifyFull,
            ca_cert: Some(PemRef::File { path: ca }),
            ..TlsSettings::default()
        })
    }

    /// The `[ssh.targets]` entry TLS over SSH uses for `e`: `<name>_tls` when the fixture has
    /// a separate TLS port (Redis), else `<name>`.
    fn tls_ssh_target(&self, e: &ResolvedEngine) -> String {
        let tls = format!("{}_tls", e.name);
        match &self.fixtures.ssh {
            Some(ssh) if ssh.targets.contains_key(&tls) => tls,
            _ => e.name.clone(),
        }
    }

    /// Routes `p` through the bastion to the in-network address `[ssh.targets] <target>`, with
    /// password auth (`encrypted_key = false`) or the passphrase-protected key.
    async fn through_ssh(
        &self,
        p: &mut ConnectionProfile,
        target: &str,
        encrypted_key: bool,
    ) -> Result<(), String> {
        let ssh = self
            .fixtures
            .ssh
            .as_ref()
            .ok_or("no [ssh] in the fixtures file")?;
        let target = ssh
            .targets
            .get(target)
            .ok_or_else(|| format!("no [ssh.targets] entry for {target}"))?;
        p.address = ProfileAddress::Network {
            host: target.host.clone(),
            port: Some(target.port),
        };
        let auth = if encrypted_key {
            let key_path = ssh.key_file(&ssh.client_key_encrypted);
            if !key_path.is_file() {
                return Err(format!("SSH client key {} is missing", key_path.display()));
            }
            let key = p.secret_key(SecretSlot::SshKeyPassphrase);
            self.secrets
                .set(&key, &Secret::new(ssh.client_key_passphrase.clone()))
                .await
                .map_err(|err| err.to_string())?;
            SshAuthSettings::PrivateKey {
                key: KeyRef::File { path: key_path },
                passphrase: Some(SecretRef::Keychain { key }),
            }
        } else {
            let key = p.secret_key(SecretSlot::SshPassword);
            self.secrets
                .set(&key, &Secret::new(ssh.password.clone()))
                .await
                .map_err(|err| err.to_string())?;
            SshAuthSettings::Password {
                password: SecretRef::Keychain { key },
            }
        };
        let known_hosts: PathBuf = ssh.key_file(&ssh.known_hosts);
        if !known_hosts.is_file() {
            return Err(format!("{} is missing", known_hosts.display()));
        }
        p.ssh = Some(SshSettings {
            host: ssh.host.clone(),
            port: ssh.port,
            user: ssh.user.clone(),
            auth,
            known_hosts: KnownHostsPolicy::Strict {
                path: Some(known_hosts),
            },
            connect_timeout_secs: Some(15),
            keepalive_secs: Some(30),
            extra: Extra::new(),
        });
        Ok(())
    }

    /// The statements the checks run on `e` ([`Statements::for_engine`]).
    fn statements(&self, e: &ResolvedEngine) -> Result<Statements, String> {
        Statements::for_engine(&self.fixtures, e.engine)
    }

    /// Opens a session for `p`, runs the ping statement, closes it. Returns (tls_in_use,
    /// via_ssh, server version, connect ms).
    async fn open_and_ping(
        &self,
        p: &ConnectionProfile,
        ping_sql: &str,
    ) -> Result<(bool, bool, ServerVersion, u64), String> {
        let session = self
            .mgr
            .open(p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .map_err(|err| format!("open failed: {err}"))?;
        let info = session.info().clone();
        let ping = ping(session.connection().as_ref(), ping_sql).await;
        let close = self.mgr.close(session.id()).await;
        ping?;
        close.map_err(|err| format!("close failed: {err}"))?;
        Ok((
            info.tls_in_use,
            info.via_ssh,
            info.server_version,
            info.latency_ms,
        ))
    }

    /// Requires `p` to be refused; with [`Refusal::TlsName`] the error must also come from
    /// certificate verification ([`names_tls_failure`]), not from a timeout, the tunnel or the
    /// login.
    async fn expect_refused(
        &self,
        p: &ConnectionProfile,
        what: &str,
        cause: Refusal,
    ) -> Result<(), String> {
        match self
            .mgr
            .test_connection(p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
        {
            Ok(info) => Err(format!("{what} was accepted ({info:?})")),
            Err(err) if cause == Refusal::TlsName && !names_tls_failure(&err) => Err(format!(
                "{what} was refused, but not by certificate verification ({}): {err}",
                err.category()
            )),
            Err(_) => Ok(()),
        }
    }

    async fn connect(&self, e: &ResolvedEngine) -> CheckResult {
        let st = self.statements(e)?;
        let p = self.profile(e, false).await?;
        let (tls, ssh, version, ms) = self.open_and_ping(&p, &st.ping).await?;
        if tls || ssh {
            return Err(format!(
                "expected plaintext without SSH, got tls={tls} ssh={ssh}"
            ));
        }
        if let Some(prefix) = e.version_prefix.as_deref()
            && !version_matches(e.engine, &version, prefix)?
        {
            return Err(format!(
                "server reports {}, fixture expects {prefix}",
                version.raw()
            ));
        }
        Ok(format!("server {} ({ms} ms)", version.raw()))
    }

    /// `VerifyFull` as the TLS-only user when the fixture has one (its plaintext login must be
    /// refused); else as the fixture user on the TLS-only port when there is one (plaintext to it
    /// must be refused), else on the shared port, where plaintext stays allowed (MongoDB
    /// `allowTLS`: the server cannot require TLS per user).
    async fn tls(&self, e: &ResolvedEngine) -> CheckResult {
        let st = self.statements(e)?;
        let fx = self.fixture(e)?;
        let tls_user = fx.tls_user.is_some();
        let tls_port = fx.tls_port.filter(|_| !tls_user);
        let mut p = self.profile(e, tls_user).await?;
        if let (Some(port), ProfileAddress::Network { port: p_port, .. }) =
            (tls_port, &mut p.address)
        {
            *p_port = Some(port);
        }
        p.tls = self.verify_full()?;
        let (tls, ssh, _, ms) = self.open_and_ping(&p, &st.ping).await?;
        if !tls || ssh {
            return Err(format!(
                "expected TLS without SSH, got tls_in_use={tls} via_ssh={ssh}"
            ));
        }
        let mut wrong = p.clone();
        wrong.tls.server_name = Some(WRONG_SERVER_NAME.to_owned());
        self.expect_refused(
            &wrong,
            "VerifyFull with a wrong server name",
            Refusal::TlsName,
        )
        .await?;
        let mut plain = p.clone();
        plain.tls = TlsSettings {
            mode: TlsMode::Disable,
            ..TlsSettings::default()
        };
        let plaintext = if tls_user {
            self.expect_refused(&plain, "the TLS-only user in plaintext", Refusal::Any)
                .await?;
            "plaintext and wrong name refused"
        } else if tls_port.is_some() {
            self.expect_refused(&plain, "plaintext to the TLS-only port", Refusal::Any)
                .await?;
            "plaintext to the TLS port and wrong name refused"
        } else {
            "wrong name refused; plaintext allowed on the shared port"
        };
        Ok(format!(
            "tls_in_use as {} at {} ({ms} ms); {plaintext}",
            p.user.as_deref().unwrap_or("?"),
            address(&p)
        ))
    }

    async fn ssh(&self, e: &ResolvedEngine) -> CheckResult {
        let st = self.statements(e)?;
        let mut p = self.profile(e, false).await?;
        self.through_ssh(&mut p, &e.name, false).await?;
        if e.engine == Engine::MsSql {
            // Its login carries the password itself, and the bastion reaches the in-network
            // host without TLS: refused unless the user allows an unverified server.
            self.expect_refused(&p, "a password login over SSH without TLS", Refusal::Any)
                .await?;
            p.tls.allow_unverified_password = true;
        }
        let (tls, ssh, _, ms) = self.open_and_ping(&p, &st.ping).await?;
        if !ssh || tls {
            return Err(format!(
                "expected SSH without TLS, got tls_in_use={tls} via_ssh={ssh}"
            ));
        }
        if self.mgr.open_tunnel_count() != 0 {
            return Err("the tunnel stayed open after the session closed".to_owned());
        }
        Ok(format!("via_ssh to {} ({ms} ms)", address(&p)))
    }

    async fn tls_over_ssh(&self, e: &ResolvedEngine) -> CheckResult {
        let st = self.statements(e)?;
        let tls_user = self.fixture(e)?.tls_user.is_some();
        let mut p = self.profile(e, tls_user).await?;
        self.through_ssh(&mut p, &self.tls_ssh_target(e), true)
            .await?;
        p.tls = self.verify_full()?;
        let (tls, ssh, _, ms) = self.open_and_ping(&p, &st.ping).await?;
        if !ssh || !tls {
            return Err(format!(
                "expected TLS over SSH, got tls_in_use={tls} via_ssh={ssh}"
            ));
        }
        let mut wrong = p.clone();
        wrong.tls.server_name = Some(WRONG_SERVER_NAME.to_owned());
        self.expect_refused(
            &wrong,
            "VerifyFull through SSH with a wrong server name",
            Refusal::TlsName,
        )
        .await?;
        Ok(format!(
            "VerifyFull as {} via tunnel ({ms} ms); wrong name refused",
            address(&p)
        ))
    }

    async fn cancel(&self, e: &ResolvedEngine) -> CheckResult {
        let st = self.statements(e)?;
        let p = self.profile(e, false).await?;
        let session = self
            .mgr
            .open(&p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .map_err(|err| format!("open failed: {err}"))?;
        let conn = session.connection().clone();
        let result = async {
            let token = cancel_once(&conn, &st, CancelBy::Token).await?;
            let call = cancel_once(&conn, &st, CancelBy::CancelRunning).await?;
            Ok(format!("token {token}; cancel_running {call}"))
        }
        .await;
        let _ = self.mgr.close(session.id()).await;
        result
    }

    async fn stream(&self, e: &ResolvedEngine) -> CheckResult {
        let st = self.statements(e)?;
        let p = self.profile(e, false).await?;
        let session = self
            .mgr
            .open(&p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .map_err(|err| format!("open failed: {err}"))?;
        let started = Instant::now();
        let result = async {
            let mut stream = session
                .connection()
                .execute(&st.rows, &[], CancellationToken::new())
                .await
                .map_err(|err| format!("execute failed: {err}"))?;
            let (mut rows, mut batches) = (0usize, 0usize);
            while let Some(item) = stream.next().await {
                if let ResultItem::Batch(b) = item.map_err(|err| format!("stream failed: {err}"))? {
                    rows += b.num_rows();
                    batches += 1;
                }
            }
            if rows != st.rows_expected {
                return Err(format!("got {rows} rows, expected {}", st.rows_expected));
            }
            if batches < 2 {
                return Err(format!("all {rows} rows came in {batches} batch(es)"));
            }
            Ok(format!(
                "{rows} rows in {batches} batches ({} ms)",
                millis(started.elapsed())
            ))
        }
        .await;
        let _ = self.mgr.close(session.id()).await;
        result
    }
}

/// What a refusal in [`Checker::expect_refused`] must be caused by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// Any error (plaintext to a TLS-only user or port: the drivers word these differently).
    Any,
    /// Certificate verification of the server name ([`names_tls_failure`]).
    TlsName,
}

/// Whether `err` reports a failed certificate check: a connection-level error (not a timeout,
/// a refused login or a server error) whose text names the certificate or its name. Every
/// driver passes the rustls reason through (`invalid peer certificate: certificate not valid
/// for name ...`), wrapped in its own words.
fn names_tls_failure(err: &oxbase_types::Error) -> bool {
    use oxbase_types::ErrorCategory as C;
    if !matches!(err.category(), C::Connection | C::Io) {
        return false;
    }
    let text = err.to_string().to_ascii_lowercase();
    ["certificate", "notvalidforname", "not valid for name"]
        .iter()
        .any(|w| text.contains(w))
}

fn address(p: &ConnectionProfile) -> String {
    match &p.address {
        ProfileAddress::Network { host, port } => {
            format!(
                "{host}:{}",
                port.map_or_else(|| "default".to_owned(), |p| p.to_string())
            )
        }
        ProfileAddress::File { path } => path.display().to_string(),
    }
}

/// Runs the engine's ping statement; it must return one result set with one row.
async fn ping(conn: &dyn Connection, sql: &str) -> Result<(), String> {
    let what = short(sql);
    let stream = conn
        .execute(sql, &[], CancellationToken::new())
        .await
        .map_err(|err| format!("{what} failed: {err}"))?;
    let out = oxbase_driver::collect(stream)
        .await
        .map_err(|err| format!("{what} failed: {err}"))?;
    match out.result_sets.as_slice() {
        [rs] if rs.num_rows() == 1 => Ok(()),
        [rs] => Err(format!("{what} returned {} rows", rs.num_rows())),
        other => Err(format!("{what} returned {} result set(s)", other.len())),
    }
}

/// A statement for messages: the whole text up to 40 characters, else its start.
fn short(sql: &str) -> String {
    clip(sql, 40)
}

/// The `$where` function of the MongoDB cancel check: 50 ms of busy loop per document, so a
/// `find` on the 200 000-document collection runs for hours.
const MONGO_SLOW_WHERE: &str =
    "function() { var t = Date.now(); while (Date.now() - t < 50) {} return true; }";
/// Prefix of the marker field the MongoDB cancel check adds to its `find` filter (with the
/// process id). The filter asks for it to be `null`, which every document matches (none has
/// the field), and `currentOp` finds the operation by that field's explicit `null` in the
/// filter, so other `$where` finds on the collection (other tests, other runs) never count.
const MONGO_CANCEL_MARK: &str = "oxbase_conformance_cancel_";

/// What the checks run on one engine: the ping (one row), a statement that runs until it is
/// cancelled, and a statement returning [`Statements::rows_expected`] rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statements {
    pub ping: String,
    pub long: String,
    pub rows: String,
    pub rows_expected: usize,
    /// A request listing the server operations of `long` still running (MongoDB: a `currentOp`
    /// command; its one row's `inprog` column is `[]` when none is left). `None` where the
    /// session proves it: a SQL or Redis session runs one statement at a time, so its ping
    /// answers only after the server stopped `long`.
    pub leftover: Option<String>,
}

impl Statements {
    /// The statements of `engine` (see the module docs). MongoDB and Redis read their names
    /// from `[m7.mongodb]` / `[m7.redis]`; `Err` says what is missing.
    pub fn for_engine(fixtures: &Fixtures, engine: Engine) -> Result<Self, String> {
        let expected = |count: Option<u64>| {
            count.map_or(Ok(STREAM_ROWS), |n| {
                usize::try_from(n).map_err(|err| format!("big_count {n}: {err}"))
            })
        };
        match engine {
            Engine::MongoDb => {
                let seed = fixtures
                    .m7_mongo(engine)
                    .ok_or("no [m7.mongodb] in the fixtures file")?;
                let db = seed
                    .database
                    .as_deref()
                    .ok_or("[m7.mongodb] has no database")?;
                let big = seed
                    .big_collection
                    .as_deref()
                    .ok_or("[m7.mongodb] has no big_collection")?;
                let mark = format!("{MONGO_CANCEL_MARK}{}", std::process::id());
                let mut long_filter = serde_json::Map::new();
                long_filter.insert(mark.clone(), serde_json::Value::Null);
                long_filter.insert("$where".to_owned(), json!(MONGO_SLOW_WHERE));
                let is_null = json!({"$type": "null"});
                Ok(Self {
                    ping: json!({"type": "command", "database": "admin", "command": {"ping": 1}})
                        .to_string(),
                    long: mongo_find(db, big, Some(serde_json::Value::Object(long_filter))),
                    rows: mongo_find(db, big, None),
                    rows_expected: expected(seed.big_count)?,
                    // The find itself, or a getMore of its cursor.
                    leftover: Some(
                        json!({"type": "command", "database": "admin", "command": {
                            "currentOp": 1,
                            "$ownOps": true,
                            "ns": format!("{db}.{big}"),
                            "$or": [
                                {format!("command.filter.{mark}"): is_null},
                                {format!("cursor.originatingCommand.filter.{mark}"): is_null},
                            ],
                        }})
                        .to_string(),
                    ),
                })
            }
            Engine::Redis => {
                let seed = fixtures
                    .m7_redis(engine)
                    .ok_or("no [m7.redis] in the fixtures file")?;
                let list = seed
                    .big_list
                    .as_deref()
                    .ok_or("[m7.redis] has no big_list")?;
                Ok(Self {
                    ping: "PING".to_owned(),
                    // A key nobody writes: BLPOP blocks until the cancel unblocks it.
                    long: format!("BLPOP oxbase_conformance:never:{} 0", std::process::id()),
                    rows: format!("LRANGE {list} 0 -1"),
                    rows_expected: expected(seed.big_count)?,
                    leftover: None,
                })
            }
            _ => Ok(Self {
                ping: "SELECT 1".to_owned(),
                long: long_statement(engine).to_owned(),
                rows: rows_statement(engine).to_owned(),
                rows_expected: STREAM_ROWS,
                leftover: None,
            }),
        }
    }
}

/// A MongoDB `find` request (the driver's JSON statement form) on `database.collection`.
fn mongo_find(database: &str, collection: &str, filter: Option<serde_json::Value>) -> String {
    let mut request = json!({
        "type": "find",
        "collection": {"kind": "table", "catalog": database, "name": collection},
    });
    if let Some(filter) = filter {
        request["filter"] = filter;
    }
    request.to_string()
}

#[derive(Clone, Copy)]
enum CancelBy {
    Token,
    CancelRunning,
}

/// Runs the long statement, cancels it after [`CANCEL_AFTER`], and checks that the stream ends
/// with `Cancelled` and that the same session then answers its ping, both within
/// [`CANCEL_DEADLINE`] of the request. For SQL and Redis the ping proves the server really
/// stopped the statement: their session runs one statement at a time. A pooled client
/// (MongoDB) can answer the ping on another connection while the statement still runs, so
/// there [`Statements::leftover`] must list the operation before the cancel (which also proves
/// its filter finds it) and no longer within [`CANCEL_DEADLINE`] after the ping. Returns the
/// timings as text.
async fn cancel_once(
    conn: &Arc<dyn Connection>,
    st: &Statements,
    by: CancelBy,
) -> Result<String, String> {
    let token = CancellationToken::new();
    let run = {
        let conn = conn.clone();
        let token = token.clone();
        let sql = st.long.clone();
        tokio::spawn(async move {
            let stream = conn.execute(&sql, &[], token).await?;
            oxbase_driver::collect(stream).await
        })
    };
    tokio::time::sleep(CANCEL_AFTER).await;
    if run.is_finished() {
        return Err(format!(
            "the long statement finished before the cancel: {:?}",
            run.await.map(|r| r.map(|_| ()))
        ));
    }
    if let Some(list) = &st.leftover
        && let Err(err) = await_ops(conn.as_ref(), list, true).await
    {
        token.cancel();
        // The statement is stopped by the token; its end does not matter any more.
        let _ = tokio::time::timeout(CANCEL_DEADLINE * 5, run).await;
        return Err(err);
    }
    let requested = Instant::now();
    match by {
        CancelBy::Token => token.cancel(),
        CancelBy::CancelRunning => conn
            .cancel_running()
            .await
            .map_err(|err| format!("cancel_running failed: {err}"))?,
    }
    let label = match by {
        CancelBy::Token => "token",
        CancelBy::CancelRunning => "cancel_running",
    };
    let outcome = tokio::time::timeout(CANCEL_DEADLINE * 5, run).await;
    let took = requested.elapsed();
    match outcome {
        Err(_) => Err(format!(
            "{label}: statement still running {} s after the cancel",
            (CANCEL_DEADLINE * 5).as_secs()
        )),
        Ok(Err(join)) => Err(format!("{label}: task failed: {join}")),
        Ok(Ok(Ok(_))) => Err(format!(
            "{label}: the statement completed instead of being cancelled"
        )),
        Ok(Ok(Err(err))) if !err.is_cancelled() => {
            Err(format!("{label}: expected Cancelled, got: {err}"))
        }
        Ok(Ok(Err(_))) if took > CANCEL_DEADLINE => Err(format!(
            "{label}: cancelled after {} ms (limit {} ms)",
            millis(took),
            millis(CANCEL_DEADLINE)
        )),
        Ok(Ok(Err(_))) => {
            match tokio::time::timeout(CANCEL_DEADLINE * 5, ping(conn.as_ref(), &st.ping)).await {
                Err(_) => return Err(format!("{label}: the ping hung after the cancel")),
                Ok(r) => r.map_err(|err| format!("{label}: after the cancel: {err}"))?,
            }
            let free = requested.elapsed();
            if free > CANCEL_DEADLINE {
                return Err(format!(
                    "{label}: the session answered again only {} ms after the cancel (limit {} \
                     ms)",
                    millis(free),
                    millis(CANCEL_DEADLINE)
                ));
            }
            let server = match &st.leftover {
                Some(list) => {
                    await_ops(conn.as_ref(), list, false)
                        .await
                        .map_err(|err| format!("{label}: {err}"))?;
                    format!(
                        ", server operation gone in {} ms",
                        millis(requested.elapsed())
                    )
                }
                None => String::new(),
            };
            Ok(format!(
                "ended in {} ms, session answered in {} ms{server}",
                millis(took),
                millis(free)
            ))
        }
    }
}

/// Polls the [`Statements::leftover`] request every 100 ms for at most [`CANCEL_DEADLINE`]
/// until it lists an operation (`running`) or none.
async fn await_ops(conn: &dyn Connection, request: &str, running: bool) -> Result<(), String> {
    let deadline = Instant::now() + CANCEL_DEADLINE;
    loop {
        let ops = in_progress(conn, request).await?;
        if (ops != "[]") == running {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(if running {
                "currentOp did not list the long statement before the cancel".to_owned()
            } else {
                format!(
                    "the server operation still runs {} ms after the session answered: {}",
                    millis(CANCEL_DEADLINE),
                    clip(&ops, 300)
                )
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The `inprog` list (JSON text) a `currentOp` command answers.
async fn in_progress(conn: &dyn Connection, request: &str) -> Result<String, String> {
    let stream = conn
        .execute(request, &[], CancellationToken::new())
        .await
        .map_err(|err| format!("currentOp failed: {err}"))?;
    let out = oxbase_driver::collect(stream)
        .await
        .map_err(|err| format!("currentOp failed: {err}"))?;
    let set = out
        .result_sets
        .first()
        .ok_or("currentOp returned no result set")?;
    let column = set
        .schema
        .index_of("inprog")
        .map_err(|_| "currentOp returned no inprog column".to_owned())?;
    let batch = set
        .batches
        .iter()
        .find(|b| b.num_rows() > 0)
        .ok_or("currentOp returned no row")?;
    crate::roundtrip::cell_text(batch.column(column).as_ref(), 0)
        .ok_or_else(|| "currentOp returned a NULL inprog".to_owned())
}

/// `text` up to `max` characters, else its start and `...`.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        format!("{}...", text.chars().take(max).collect::<String>())
    }
}

/// Whether the server version matches the fixture's `version_prefix`: the parsed version, or
/// for SQL Server the start of the `@@VERSION` banner ("Microsoft SQL Server 2025"), which is
/// what the driver keeps as the raw version text.
fn version_matches(engine: Engine, version: &ServerVersion, prefix: &str) -> Result<bool, String> {
    if engine == Engine::MsSql {
        return Ok(version.raw().starts_with(prefix));
    }
    let want =
        ServerVersion::parse(prefix).map_err(|err| format!("version_prefix {prefix:?}: {err}"))?;
    Ok(*version == want)
}

/// A statement that runs for 30 s (or forever, on SQLite) without returning rows.
const fn long_statement(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "SELECT pg_sleep(30)",
        Engine::MySql | Engine::MariaDb => "SELECT SLEEP(30)",
        Engine::MsSql => "WAITFOR DELAY '00:00:30'",
        _ => "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT max(x) FROM c",
    }
}

/// A statement that generates exactly [`STREAM_ROWS`] rows on the server.
const fn rows_statement(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "SELECT g AS n, md5(g::text) AS h FROM generate_series(1, 200000) AS g",
        Engine::MsSql => {
            "SELECT value AS n, CONVERT(char(32), HASHBYTES('MD5', CAST(value AS varchar(10))), 2) \
             AS h FROM GENERATE_SERIES(1, 200000)"
        }
        // Six digit tables give 0..999999; the top digit below 2 keeps 200 000 of them.
        Engine::MySql | Engine::MariaDb => {
            "SELECT d0.n + 10 * d1.n + 100 * d2.n + 1000 * d3.n + 10000 * d4.n + 100000 * d5.n \
             AS n, MD5(d0.n + 10 * d1.n + 100 * d2.n) AS h \
             FROM (SELECT 0 AS n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 \
             UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 \
             UNION ALL SELECT 8 UNION ALL SELECT 9) AS d0 \
             CROSS JOIN (SELECT 0 AS n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 \
             UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 \
             UNION ALL SELECT 8 UNION ALL SELECT 9) AS d1 \
             CROSS JOIN (SELECT 0 AS n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 \
             UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 \
             UNION ALL SELECT 8 UNION ALL SELECT 9) AS d2 \
             CROSS JOIN (SELECT 0 AS n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 \
             UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 \
             UNION ALL SELECT 8 UNION ALL SELECT 9) AS d3 \
             CROSS JOIN (SELECT 0 AS n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 \
             UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 \
             UNION ALL SELECT 8 UNION ALL SELECT 9) AS d4 \
             CROSS JOIN (SELECT 0 AS n UNION ALL SELECT 1) AS d5"
        }
        _ => {
            "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 200000) \
             SELECT n, 'row-' || n AS h FROM c"
        }
    }
}

pub(crate) fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(name: &str, engine: Engine) -> ResolvedEngine {
        ResolvedEngine {
            name: name.to_owned(),
            engine,
            target: Target::Tcp {
                host: "127.0.0.1".to_owned(),
                port: 1,
            },
            version_prefix: None,
        }
    }

    #[test]
    fn every_engine_runs_its_checks_since_m7() {
        for e in Engine::ALL {
            assert!(static_cells(&engine(e.as_str(), e), true).is_none(), "{e}");
        }
        let down = static_cells(&engine("redis", Engine::Redis), false).unwrap();
        assert_eq!(down.len(), ConnCheck::ALL.len());
        assert!(down.iter().all(|c| c.status == CellStatus::Skipped));
        assert_eq!(down[0].label(), "skipped");
    }

    #[test]
    fn mongodb_and_redis_statements_come_from_the_m7_seed() {
        let fx =
            Fixtures::from_path(&crate::fixtures::workspace_root().join("tests/fixtures.toml"))
                .unwrap();
        let mongo = Statements::for_engine(&fx, Engine::MongoDb).unwrap();
        let ping: serde_json::Value = serde_json::from_str(&mongo.ping).unwrap();
        assert_eq!(ping["type"], "command");
        assert_eq!(ping["command"]["ping"], 1);
        let long: serde_json::Value = serde_json::from_str(&mongo.long).unwrap();
        assert_eq!(long["type"], "find");
        assert_eq!(long["collection"]["catalog"], "oxbase_test");
        assert_eq!(long["collection"]["name"], "m7_big");
        assert!(long["filter"]["$where"].as_str().unwrap().contains("while"));
        assert!(mongo.long.trim_start().starts_with('{'), "{}", mongo.long);
        // The `type` tag comes first (serde_json preserve_order).
        assert!(
            mongo.rows.starts_with(r#"{"type":"find""#),
            "{}",
            mongo.rows
        );
        let rows: serde_json::Value = serde_json::from_str(&mongo.rows).unwrap();
        assert!(rows.get("filter").is_none());
        assert_eq!(mongo.rows_expected, STREAM_ROWS);
        // The long find carries the marker field the currentOp request looks for.
        let mark = format!("{MONGO_CANCEL_MARK}{}", std::process::id());
        assert_eq!(long["filter"][&mark], serde_json::Value::Null);
        assert!(long["filter"].as_object().unwrap().contains_key(&mark));
        let left: serde_json::Value =
            serde_json::from_str(mongo.leftover.as_deref().unwrap()).unwrap();
        assert_eq!(left["type"], "command");
        assert_eq!(left["database"], "admin");
        let op = &left["command"];
        assert!(
            mongo
                .leftover
                .as_deref()
                .unwrap()
                .contains(r#""command":{"currentOp":1"#),
            "the command name comes first"
        );
        assert_eq!(op["$ownOps"], true);
        assert_eq!(op["ns"], "oxbase_test.m7_big");
        assert_eq!(
            op["$or"][0][format!("command.filter.{mark}")]["$type"],
            "null"
        );
        assert_eq!(
            op["$or"][1][format!("cursor.originatingCommand.filter.{mark}")]["$type"],
            "null"
        );

        let redis = Statements::for_engine(&fx, Engine::Redis).unwrap();
        assert_eq!(redis.ping, "PING");
        assert!(
            redis.long.starts_with("BLPOP oxbase_conformance:never:"),
            "{}",
            redis.long
        );
        assert!(redis.long.ends_with(" 0"), "{}", redis.long);
        assert_eq!(redis.rows, "LRANGE m7:big:list 0 -1");
        assert_eq!(redis.rows_expected, STREAM_ROWS);
        assert_eq!(redis.leftover, None);

        let pg = Statements::for_engine(&fx, Engine::Postgres).unwrap();
        assert_eq!(pg.ping, "SELECT 1");
        assert_eq!(pg.long, "SELECT pg_sleep(30)");
        assert_eq!(pg.leftover, None);

        // Without [m7] the checks say what is missing.
        let bare = Fixtures::parse("schema_version = 1\n", "t").unwrap();
        assert_eq!(
            Statements::for_engine(&bare, Engine::Redis).unwrap_err(),
            "no [m7.redis] in the fixtures file"
        );
        assert_eq!(
            Statements::for_engine(&bare, Engine::MongoDb).unwrap_err(),
            "no [m7.mongodb] in the fixtures file"
        );
        assert_eq!(short("PING"), "PING");
        assert_eq!(short(&"x".repeat(50)), format!("{}...", "x".repeat(40)));
    }

    #[test]
    fn sql_server_version_matches_the_banner() {
        let v = ServerVersion::parse("Microsoft SQL Server 2025 (RTM-CU10) - 17.0.4005.7 (X64)")
            .unwrap();
        assert_eq!(
            version_matches(Engine::MsSql, &v, "Microsoft SQL Server 2025"),
            Ok(true)
        );
        assert_eq!(
            version_matches(Engine::MsSql, &v, "Microsoft SQL Server 2022"),
            Ok(false)
        );
        let pg = ServerVersion::parse("PostgreSQL 18.6 on x86_64").unwrap();
        assert_eq!(
            version_matches(Engine::Postgres, &pg, "PostgreSQL 18.6"),
            Ok(true)
        );
        assert!(version_matches(Engine::Postgres, &pg, "no digits").is_err());
    }

    #[test]
    fn m1_engines_run_unless_unreachable() {
        assert!(static_cells(&engine("pg", Engine::Postgres), true).is_none());
        let down = static_cells(&engine("pg", Engine::Postgres), false).unwrap();
        assert!(down.iter().all(|c| c.status == CellStatus::Skipped));
        let lite = ResolvedEngine {
            target: Target::File {
                path: PathBuf::from("x.sqlite"),
            },
            ..engine("sqlite", Engine::Sqlite)
        };
        let down = static_cells(&lite, false).unwrap();
        let na: Vec<_> = down
            .iter()
            .filter(|c| c.status == CellStatus::NotApplicable)
            .map(|c| c.check)
            .collect();
        assert_eq!(na, [ConnCheck::Tls, ConnCheck::Ssh, ConnCheck::TlsOverSsh]);
    }

    #[test]
    fn only_certificate_errors_count_as_a_tls_name_refusal() {
        use oxbase_types::Error;
        for yes in [
            Error::connection(
                "TLS handshake with mongodb:27017 failed: invalid peer certificate: certificate \
                 not valid for name \"wrong-name.oxbase.invalid\"",
            ),
            Error::connection("Redis connection: invalid peer certificate: NotValidForName"),
            Error::Io {
                kind: "InvalidData".to_owned(),
                message: "invalid peer certificate: UnknownIssuer".to_owned(),
            },
        ] {
            assert!(names_tls_failure(&yes), "{yes}");
        }
        for no in [
            Error::timeout("TLS handshake did not finish: certificate"),
            Error::auth("bad password"),
            Error::connection("ssh tunnel: connection refused"),
            Error::connection("TLS handshake did not finish within 10 s"),
            Error::sql(Some("X"), "certificate"),
        ] {
            assert!(!names_tls_failure(&no), "{no}");
        }
        assert_eq!(clip("abc", 5), "abc");
        assert_eq!(clip("abcdef", 3), "abc...");
    }

    #[test]
    fn check_ids_serialize_as_their_names() {
        for c in ConnCheck::ALL {
            assert_eq!(serde_json::to_value(c).unwrap(), c.as_str());
        }
    }
}
