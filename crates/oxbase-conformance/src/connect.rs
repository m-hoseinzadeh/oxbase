//! M1 connection checks: one row per engine fixture, one column per [`ConnCheck`].
//!
//! Every check goes through the real stack the app uses: [`ConnectionManager`] with
//! [`default_drivers`], a [`MemorySecretStore`] holding the fixture passwords, and the russh
//! tunnel provider. Engines without a driver yet (mongodb, redis) report every check as pending
//! with the milestone that brings their driver; SQL Server runs them since M4.
//!
//! | Check | What passes |
//! |---|---|
//! | `connect` | plain connect as the fixture user; the server version equals `version_prefix` |
//! | `tls` | `VerifyFull` with the fixture CA as the TLS-only user reports `tls_in_use`; the same user in plaintext and a wrong `server_name` are refused |
//! | `ssh` | through the bastion (password auth, `Strict` known_hosts) to the in-network address |
//! | `tls_over_ssh` | through the bastion (encrypted-key auth) with `VerifyFull` against the service name while dialling `127.0.0.1`; a wrong `server_name` is refused |
//! | `cancel` | a 30 s statement ends with `Cancelled`, and the same session answers `SELECT 1`, within 2 s of the request, via the token and via `cancel_running` |
//! | `stream_200k` | 200 000 generated rows arrive complete in more than one batch |

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

use crate::fixtures::{EngineFixture, Fixtures, ResolvedEngine};
use crate::matrix::{CellStatus, driver_milestone};
use crate::probe::Target;

/// Longest a cancelled statement may take to end after the cancel request.
pub const CANCEL_DEADLINE: Duration = Duration::from_secs(2);
/// Rows the streaming check generates.
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
                "VerifyFull with the fixture CA as the TLS-only user (tls_in_use); plaintext and \
                 a wrong server name are refused"
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
            Self::Stream200k => "200k generated rows stream in more than one batch",
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
            CellStatus::Pending => driver_milestone(self.engine_kind)
                .map_or_else(|| "pending".to_owned(), |m| format!("pending ({m})")),
            CellStatus::Skipped => "skipped".to_owned(),
            CellStatus::NotApplicable => "n/a".to_owned(),
        }
    }
}

/// The cells of engines that cannot run any check: no M1 driver (pending), or an unreachable
/// fixture (skipped). `None` when the engine's checks should run.
pub fn static_cells(e: &ResolvedEngine, reachable: bool) -> Option<Vec<ConnCell>> {
    let pending = driver_milestone(e.engine);
    if pending.is_none() && reachable {
        return None;
    }
    Some(
        ConnCheck::ALL
            .iter()
            .map(|&check| {
                if !check.applies(e.engine) {
                    ConnCell::new(e, check, CellStatus::NotApplicable, None)
                } else if let Some(m) = pending {
                    ConnCell::new(
                        e,
                        check,
                        CellStatus::Pending,
                        Some(format!("no driver until {m}")),
                    )
                } else {
                    ConnCell::new(
                        e,
                        check,
                        CellStatus::Skipped,
                        Some("fixture unreachable".to_owned()),
                    )
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
    pub(crate) mgr: ConnectionManager,
    secrets: Arc<MemorySecretStore>,
    fixtures: Fixtures,
}

type CheckResult = Result<String, String>;

impl Ctx {
    pub(crate) fn new(fixtures: &Fixtures) -> Self {
        let secrets = Arc::new(MemorySecretStore::new());
        let mgr = ConnectionManager::new(
            default_drivers(),
            secrets.clone(),
            Arc::new(RusshTunnelProvider::new()),
        );
        Self {
            mgr,
            secrets,
            fixtures: fixtures.clone(),
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

    /// Routes `p` through the bastion to the in-network address of `e`, with password auth
    /// (`encrypted_key = false`) or the passphrase-protected key.
    async fn through_ssh(
        &self,
        p: &mut ConnectionProfile,
        e: &ResolvedEngine,
        encrypted_key: bool,
    ) -> Result<(), String> {
        let ssh = self
            .fixtures
            .ssh
            .as_ref()
            .ok_or("no [ssh] in the fixtures file")?;
        let target = ssh
            .targets
            .get(&e.name)
            .ok_or_else(|| format!("no [ssh.targets] entry for {}", e.name))?;
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

    /// Opens a session for `p`, runs `SELECT 1`, closes it. Returns (tls_in_use, via_ssh,
    /// server version, connect ms).
    async fn open_and_ping(
        &self,
        p: &ConnectionProfile,
    ) -> Result<(bool, bool, ServerVersion, u64), String> {
        let session = self
            .mgr
            .open(p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .map_err(|err| format!("open failed: {err}"))?;
        let info = session.info().clone();
        let ping = ping(session.connection().as_ref()).await;
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

    async fn expect_refused(&self, p: &ConnectionProfile, what: &str) -> Result<(), String> {
        match self
            .mgr
            .test_connection(p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
        {
            Ok(info) => Err(format!("{what} was accepted ({info:?})")),
            Err(_) => Ok(()),
        }
    }

    async fn connect(&self, e: &ResolvedEngine) -> CheckResult {
        let p = self.profile(e, false).await?;
        let (tls, ssh, version, ms) = self.open_and_ping(&p).await?;
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

    async fn tls(&self, e: &ResolvedEngine) -> CheckResult {
        let mut p = self.profile(e, true).await?;
        p.tls = self.verify_full()?;
        let (tls, ssh, _, ms) = self.open_and_ping(&p).await?;
        if !tls || ssh {
            return Err(format!(
                "expected TLS without SSH, got tls_in_use={tls} via_ssh={ssh}"
            ));
        }
        let mut wrong = p.clone();
        wrong.tls.server_name = Some(WRONG_SERVER_NAME.to_owned());
        self.expect_refused(&wrong, "VerifyFull with a wrong server name")
            .await?;
        let mut plain = p.clone();
        plain.tls = TlsSettings {
            mode: TlsMode::Disable,
            ..TlsSettings::default()
        };
        self.expect_refused(&plain, "the TLS-only user in plaintext")
            .await?;
        Ok(format!(
            "tls_in_use as {} ({ms} ms); plaintext and wrong name refused",
            p.user.as_deref().unwrap_or("?")
        ))
    }

    async fn ssh(&self, e: &ResolvedEngine) -> CheckResult {
        let mut p = self.profile(e, false).await?;
        self.through_ssh(&mut p, e, false).await?;
        let (tls, ssh, _, ms) = self.open_and_ping(&p).await?;
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
        let mut p = self.profile(e, true).await?;
        self.through_ssh(&mut p, e, true).await?;
        p.tls = self.verify_full()?;
        let (tls, ssh, _, ms) = self.open_and_ping(&p).await?;
        if !ssh || !tls {
            return Err(format!(
                "expected TLS over SSH, got tls_in_use={tls} via_ssh={ssh}"
            ));
        }
        let mut wrong = p.clone();
        wrong.tls.server_name = Some(WRONG_SERVER_NAME.to_owned());
        self.expect_refused(&wrong, "VerifyFull through SSH with a wrong server name")
            .await?;
        Ok(format!(
            "VerifyFull as {} via tunnel ({ms} ms); wrong name refused",
            address(&p)
        ))
    }

    async fn cancel(&self, e: &ResolvedEngine) -> CheckResult {
        let p = self.profile(e, false).await?;
        let session = self
            .mgr
            .open(&p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .map_err(|err| format!("open failed: {err}"))?;
        let conn = session.connection().clone();
        let result = async {
            let sql = long_statement(e.engine);
            let token = cancel_once(&conn, sql, CancelBy::Token).await?;
            let call = cancel_once(&conn, sql, CancelBy::CancelRunning).await?;
            Ok(format!("token {token}; cancel_running {call}"))
        }
        .await;
        let _ = self.mgr.close(session.id()).await;
        result
    }

    async fn stream(&self, e: &ResolvedEngine) -> CheckResult {
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
                .execute(rows_statement(e.engine), &[], CancellationToken::new())
                .await
                .map_err(|err| format!("execute failed: {err}"))?;
            let (mut rows, mut batches) = (0usize, 0usize);
            while let Some(item) = stream.next().await {
                if let ResultItem::Batch(b) = item.map_err(|err| format!("stream failed: {err}"))? {
                    rows += b.num_rows();
                    batches += 1;
                }
            }
            if rows != STREAM_ROWS {
                return Err(format!("got {rows} rows, expected {STREAM_ROWS}"));
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

async fn ping(conn: &dyn Connection) -> Result<(), String> {
    let stream = conn
        .execute("SELECT 1", &[], CancellationToken::new())
        .await
        .map_err(|err| format!("SELECT 1 failed: {err}"))?;
    let out = oxbase_driver::collect(stream)
        .await
        .map_err(|err| format!("SELECT 1 failed: {err}"))?;
    match out.result_sets.as_slice() {
        [rs] if rs.num_rows() == 1 => Ok(()),
        other => Err(format!("SELECT 1 returned {} result set(s)", other.len())),
    }
}

#[derive(Clone, Copy)]
enum CancelBy {
    Token,
    CancelRunning,
}

/// Runs `sql`, cancels it after [`CANCEL_AFTER`], and checks that the stream ends with
/// `Cancelled` and that the same session then answers `SELECT 1`, both within
/// [`CANCEL_DEADLINE`] of the request. The `SELECT 1` proves the server really stopped the
/// statement: a session runs one statement at a time. Returns both timings as text.
async fn cancel_once(
    conn: &Arc<dyn Connection>,
    sql: &'static str,
    by: CancelBy,
) -> Result<String, String> {
    let token = CancellationToken::new();
    let run = {
        let conn = conn.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let stream = conn.execute(sql, &[], token).await?;
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
            match tokio::time::timeout(CANCEL_DEADLINE * 5, ping(conn.as_ref())).await {
                Err(_) => return Err(format!("{label}: SELECT 1 hung after the cancel")),
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
            Ok(format!(
                "ended in {} ms, session answered in {} ms",
                millis(took),
                millis(free)
            ))
        }
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
    fn engines_without_a_driver_are_pending_with_their_milestone() {
        let cells = static_cells(&engine("redis", Engine::Redis), true).unwrap();
        assert_eq!(cells.len(), ConnCheck::ALL.len());
        assert!(cells.iter().all(|c| c.status == CellStatus::Pending));
        assert_eq!(cells[0].label(), "pending (M7)");
        // SQL Server has its driver since M4: its checks run.
        assert!(static_cells(&engine("mssql", Engine::MsSql), true).is_none());
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
    fn check_ids_serialize_as_their_names() {
        for c in ConnCheck::ALL {
            assert_eq!(serde_json::to_value(c).unwrap(), c.as_str());
        }
    }
}
