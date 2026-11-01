//! [`MsSqlConnection`]: connecting, statement execution, cancellation and transactions (driver
//! stream). See the [`crate::driver`] module docs for the contract.
//!
//! # Execution model
//!
//! The `tiberius::Client` lives behind an `Arc<tokio::sync::Mutex<..>>`. Every `execute` takes
//! the lock (an owned guard) and moves it into a spawned task, which runs the request and feeds
//! a bounded channel that is the returned [`ResultStream`]: the stream is `'static`, the channel
//! is the back-pressure, and nothing is collected in memory.
//!
//! # Cancellation
//!
//! The task watches the statement's token (which [`Connection::cancel_running`] cancels too)
//! and the consumer's end of the channel. On a cancel it stops emitting items and sends a TDS
//! ATTENTION on the session's own socket ([`super::wire::SharedWire::send_attention`]) while
//! tiberius keeps reading: the response then ends at the server's acknowledgement, so no token
//! is ever cut in half and the session stays usable. If the request is not completely written
//! yet, the attention waits until it is. Everything is bounded by [`CANCEL_DRAIN`]: if the
//! response has not ended by then (or the attention cannot be written), the socket is dropped
//! and every later call fails with "the connection was closed after a cancel". The stream ends
//! with one `Err(Error::Cancelled)` either way.

use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::{FutureExt as _, StreamExt as _, stream};
use oxbase_driver::{
    CancellationToken, ConnectParams, Connection, Notice, ResultItem, ResultStream, Tx,
};
use oxbase_types::{Endpoint, Engine, Error, Result, ServerVersion, TlsMode, Value};
use tiberius::{AuthMethod, Client, Command, CommandItem, QueryItem};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, OwnedMutexGuard, mpsc};
use tokio::time::Instant;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt as _};

use super::error::{Mapped, map_connect_error, map_error, with_server};
use super::tls::{self, ALPN_TDS8, TlsPlan, Transport};
use super::types::{
    BATCH_BYTES, BATCH_ROWS, BatchBuilder, Param, ResultShape, declarations, to_param,
};
use super::wire::SharedWire;
use crate::options;

/// Connect deadline when [`ConnectParams::connect_timeout`] is not set.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// LOGIN7 application name when [`ConnectParams::application_name`] is not set.
pub const DEFAULT_APPLICATION_NAME: &str = "oxbase";
/// How long a cancelled request may take to end before the socket is dropped.
pub const CANCEL_DRAIN: Duration = Duration::from_millis(1500);
/// How often a cancel re-checks whether the request is completely written.
const ATTENTION_POLL: Duration = Duration::from_millis(10);
/// Items buffered between the request task and the consumer.
const CHANNEL_CAPACITY: usize = 4;
/// Bound for the polite close of a session.
const CLOSE_GRACE: Duration = Duration::from_secs(1);

/// Run once after login (`docs/m4-contracts.md` §2.3).
const SESSION_QUERY: &str = "SET TEXTSIZE 2147483647; SELECT DB_NAME(), @@SPID, \
     CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)), @@VERSION";
const BEGIN_SQL: &str = "BEGIN TRANSACTION";
/// A transaction the server already ended (deadlock victim 1205, `XACT_ABORT`, a trigger's
/// `ROLLBACK`) fails the commit with error 50000 instead of reporting a commit that did not
/// happen (stricter than contract §2.4's `IF @@TRANCOUNT > 0 COMMIT`).
const COMMIT_SQL: &str = "IF @@TRANCOUNT = 0 RAISERROR(N'the transaction was already ended by \
     the server (rolled back after an error); nothing was committed', 16, 1) \
     ELSE COMMIT TRANSACTION";
const ROLLBACK_SQL: &str = "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION";
const CLOSED_AFTER_CANCEL: &str = "the connection was closed after a cancel";

type TdsClient = Client<Compat<SharedWire<Transport>>>;
type Sender = mpsc::Sender<Result<ResultItem>>;

fn lock<T>(m: &StdMutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- options ----------------------------------------------------------------------------------

/// The driver's reading of [`ConnectParams::options`] ([`crate::options`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settings {
    pub instance: Option<String>,
    pub strict: bool,
    pub read_only: bool,
    pub batch_rows: usize,
}

impl Settings {
    pub(crate) fn from_params(params: &ConnectParams) -> Result<Self> {
        let mut s = Self {
            instance: None,
            strict: false,
            read_only: false,
            batch_rows: BATCH_ROWS,
        };
        for (key, value) in &params.options {
            let v = value.trim();
            match key.as_str() {
                options::INSTANCE => {
                    s.instance = (!v.is_empty()).then(|| v.to_owned());
                }
                options::ENCRYPT => {
                    s.strict = if v.eq_ignore_ascii_case(options::ENCRYPT_STRICT) {
                        true
                    } else if v.is_empty() || v.eq_ignore_ascii_case(options::ENCRYPT_NEGOTIATE) {
                        false
                    } else {
                        return Err(Error::invalid_input(format!(
                            "option {key} must be \"strict\" or \"negotiate\", not '{v}'"
                        )));
                    };
                }
                options::APPLICATION_INTENT => {
                    s.read_only = match v.to_ascii_lowercase().as_str() {
                        "read_only" | "readonly" => true,
                        "" | "read_write" | "readwrite" => false,
                        _ => {
                            return Err(Error::invalid_input(format!(
                                "option {key} must be \"read_only\" or \"read_write\", not '{v}'"
                            )));
                        }
                    };
                }
                options::BATCH_ROWS => {
                    s.batch_rows = match v.parse::<usize>() {
                        Ok(n) if n > 0 => n,
                        _ => {
                            return Err(Error::invalid_input(format!(
                                "option {key} must be a positive integer, not '{v}'"
                            )));
                        }
                    };
                }
                other => {
                    return Err(Error::invalid_input(format!(
                        "unknown SQL Server connection option '{other}'"
                    )));
                }
            }
        }
        if s.instance.is_some() && params.port.is_none() {
            return Err(Error::invalid_input(
                "a named SQL Server instance needs a port: give the instance's port (SQL Server \
                 Browser lookups are not supported)",
            ));
        }
        if s.strict && matches!(params.tls.mode, TlsMode::Disable | TlsMode::Prefer) {
            return Err(Error::invalid_input(
                "strict (TDS 8.0) encryption needs the TLS mode \"require\", \"verify CA\" or \
                 \"verify full\"",
            ));
        }
        Ok(s)
    }
}

// ---- connect ----------------------------------------------------------------------------------

/// Builds the server version: the numbers of `ProductVersion`, the text of `@@VERSION`'s first
/// line.
pub(crate) fn server_version(product: &str, banner: &str) -> ServerVersion {
    let first = banner.lines().next().unwrap_or_default().trim();
    let product = ServerVersion::parse(product);
    let mut version = ServerVersion::parse(first)
        .or_else(|_| product.clone())
        .unwrap_or_else(|_| ServerVersion::new(0, 0, 0));
    if let Ok(p) = product {
        version.major = p.major;
        version.minor = p.minor;
        version.patch = p.patch;
        version.build = p.build;
    }
    version
}

fn tls_plan(params: &ConnectParams, settings: &Settings, host: &str) -> Result<TlsPlan> {
    let mode = params.tls.mode;
    let config = if mode == TlsMode::Disable {
        None
    } else {
        let base = oxbase_net::tls::client_config(&params.tls)?;
        Some(if settings.strict {
            let mut config = (*base).clone();
            config.alpn_protocols = vec![ALPN_TDS8.to_vec()];
            Arc::new(config)
        } else {
            base
        })
    };
    Ok(TlsPlan {
        mode,
        strict: settings.strict,
        config,
        server_name: oxbase_net::tls::server_name(&params.tls, host)?,
        instance: settings.instance.clone(),
    })
}

/// Opens a session (see the module docs of [`crate::driver`]).
pub(crate) async fn connect(params: &ConnectParams) -> Result<MsSqlConnection> {
    if params.engine != Engine::MsSql {
        return Err(Error::invalid_input(format!(
            "the SQL Server driver cannot open {} connections",
            params.engine.display_name()
        )));
    }
    params.validate()?;
    let settings = Settings::from_params(params)?;
    let user = params
        .user
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            Error::invalid_input(
                "SQL Server needs a login name (SQL authentication; Windows and Microsoft Entra \
                 ID logins are not supported yet)",
            )
        })?;
    if params.tls.client_cert.is_some() || params.tls.client_key.is_some() {
        return Err(Error::unsupported(
            "TLS client certificates are not supported for SQL Server",
        ));
    }
    let server = params
        .server_endpoint()
        .ok_or_else(|| Error::invalid_input("SQL Server needs a host"))?;
    let dial = params
        .dial_endpoint()
        .ok_or_else(|| Error::invalid_input("SQL Server needs a host"))?;
    let shown = if params.is_tunnelled() {
        format!("{server} (via {dial})")
    } else {
        server.to_string()
    };
    let plan = tls_plan(params, &settings, &server.host)?;
    let timeout = params.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT);
    match tokio::time::timeout(
        timeout,
        open(params, &settings, user, &server, &dial, &plan, &shown),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(Error::timeout(format!(
            "connecting to {shown} took longer than {}s",
            timeout.as_secs_f64()
        ))),
    }
}

async fn open(
    params: &ConnectParams,
    settings: &Settings,
    user: &str,
    server: &Endpoint,
    dial: &Endpoint,
    plan: &TlsPlan,
    shown: &str,
) -> Result<MsSqlConnection> {
    let host = dial.host.trim_start_matches('[').trim_end_matches(']');
    let tcp = TcpStream::connect((host, dial.port))
        .await
        .map_err(|e| with_server(Error::connection(format!("TCP: {e}")), shown))?;
    if let Err(e) = tcp.set_nodelay(true) {
        tracing::debug!(error = %e, "cannot set TCP_NODELAY");
    }
    let negotiated = tls::negotiate(tcp, plan)
        .await
        .map_err(|e| with_server(e, shown))?;
    let wire = SharedWire::new(
        negotiated.transport,
        Some(negotiated.server.replay_payload()),
    );

    // Never `Debug`-print this: it holds the password.
    let mut config = tiberius::Config::new();
    config.host(&server.host);
    config.port(server.port);
    if let Some(db) = params.database.as_deref().filter(|d| !d.is_empty()) {
        config.database(db);
    }
    config.application_name(
        params
            .application_name
            .as_deref()
            .unwrap_or(DEFAULT_APPLICATION_NAME),
    );
    config.encryption(tiberius::EncryptionLevel::NotSupported);
    config.authentication(AuthMethod::sql_server(
        user,
        params
            .password
            .as_ref()
            .map(oxbase_types::Secret::expose)
            .unwrap_or_default(),
    ));
    config.readonly(settings.read_only);
    // Bounded by connect's own deadline; statements run as long as they need (cancel stops them).
    config.handshake_timeout(None);
    config.command_timeout(None);
    // Unpaired UTF-16 surrogates in nvarchar data show as U+FFFD instead of failing the result.
    config.lossy_utf16_decoding(true);

    let mut client = Client::connect(config, wire.clone().compat())
        .await
        .map_err(|e| map_connect_error(e, shown))?;
    let info = session_info(&mut client)
        .await
        .map_err(|e| with_server(e, shown))?;
    tracing::debug!(
        server = %shown,
        spid = info.spid,
        tls = negotiated.tls,
        version = %info.version,
        "SQL Server session open"
    );
    Ok(MsSqlConnection {
        shared: Arc::new(Shared {
            session: Arc::new(Mutex::new(Some(Session { client, wire }))),
            running: StdMutex::new(None),
            next_run: AtomicU64::new(0),
            needs_rollback: AtomicBool::new(false),
            closed_reason: StdMutex::new(None),
            batch_rows: settings.batch_rows,
            spid: info.spid,
        }),
        version: info.version,
        tls: negotiated.tls,
        database: info.database,
        server: shown.to_owned(),
    })
}

struct SessionInfo {
    database: Option<String>,
    spid: i16,
    version: ServerVersion,
}

async fn session_info(client: &mut TdsClient) -> Result<SessionInfo> {
    let rows = client
        .simple_query(SESSION_QUERY)
        .await
        .map_err(|e| map_error(e).error)?
        .into_first_result()
        .await
        .map_err(|e| map_error(e).error)?;
    let row = rows
        .first()
        .ok_or_else(|| Error::internal("the session query returned no row"))?;
    let text = |i: usize| -> Option<String> {
        row.try_get::<&str, _>(i).ok().flatten().map(str::to_owned)
    };
    Ok(SessionInfo {
        database: text(0).filter(|d| !d.is_empty()),
        spid: row.try_get::<i16, _>(1).ok().flatten().unwrap_or_default(),
        version: server_version(&text(2).unwrap_or_default(), &text(3).unwrap_or_default()),
    })
}

// ---- the connection ---------------------------------------------------------------------------

struct Session {
    client: TdsClient,
    wire: SharedWire<Transport>,
}

/// State shared by a connection, its transactions and its running statement tasks.
struct Shared {
    /// The session; `None` once closed.
    session: Arc<Mutex<Option<Session>>>,
    /// Token of the statement currently running, with its run number.
    running: StdMutex<Option<(u64, CancellationToken)>>,
    next_run: AtomicU64,
    /// Set when an [`MsSqlTx`] was dropped without commit or rollback.
    needs_rollback: AtomicBool,
    /// Why the session was closed by the driver (after a cancel, or a broken stream).
    closed_reason: StdMutex<Option<String>>,
    batch_rows: usize,
    spid: i16,
}

impl Shared {
    fn closed(&self) -> Error {
        Error::connection(
            lock(&self.closed_reason)
                .clone()
                .unwrap_or_else(|| "the connection is closed".to_owned()),
        )
    }

    fn mark_closed(&self, reason: String) {
        lock(&self.closed_reason).get_or_insert(reason);
    }

    fn begin_run(&self, token: CancellationToken) -> u64 {
        let id = self.next_run.fetch_add(1, Ordering::Relaxed);
        *lock(&self.running) = Some((id, token));
        id
    }

    fn end_run(&self, id: u64) {
        let mut running = lock(&self.running);
        if running.as_ref().is_some_and(|(r, _)| *r == id) {
            *running = None;
        }
    }

    fn cancel_running(&self) {
        if let Some((_, token)) = lock(&self.running).as_ref() {
            token.cancel();
        }
    }
}

/// One SQL Server session. `Debug` never shows the password.
pub struct MsSqlConnection {
    shared: Arc<Shared>,
    version: ServerVersion,
    tls: bool,
    database: Option<String>,
    server: String,
}

impl fmt::Debug for MsSqlConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MsSqlConnection")
            .field("server", &self.server)
            .field("version", &self.version.raw())
            .field("spid", &self.shared.spid)
            .field("tls", &self.tls)
            .field("database", &self.database)
            .finish_non_exhaustive()
    }
}

impl MsSqlConnection {
    /// The server session id (`@@SPID`) read at connect time.
    pub fn spid(&self) -> i16 {
        self.shared.spid
    }
}

#[async_trait]
impl Connection for MsSqlConnection {
    fn engine(&self) -> Engine {
        Engine::MsSql
    }

    fn server_version(&self) -> &ServerVersion {
        &self.version
    }

    fn tls_in_use(&self) -> bool {
        self.tls
    }

    fn current_database(&self) -> Option<String> {
        self.database.clone()
    }

    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        execute(&self.shared, sql, params, cancel).await
    }

    async fn cancel_running(&self) -> Result<()> {
        self.shared.cancel_running();
        Ok(())
    }

    async fn begin(&self) -> Result<Box<dyn Tx>> {
        run_simple(&self.shared, Some(BEGIN_SQL)).await?;
        Ok(Box::new(MsSqlTx {
            shared: Arc::clone(&self.shared),
            finished: AtomicBool::new(false),
        }))
    }

    async fn close(&self) -> Result<()> {
        // A running statement is cancelled first; its task releases the session when done.
        self.shared.cancel_running();
        let session = self.shared.session.lock().await.take();
        if let Some(session) = session {
            self.shared
                .mark_closed("the connection is closed".to_owned());
            let wire = session.wire.clone();
            if let Ok(Err(e)) = tokio::time::timeout(CLOSE_GRACE, session.client.close()).await {
                tracing::debug!(error = %e, "SQL Server close failed");
            }
            wire.kill();
        }
        Ok(())
    }
}

/// A transaction started with `BEGIN TRANSACTION`.
struct MsSqlTx {
    shared: Arc<Shared>,
    finished: AtomicBool,
}

impl Drop for MsSqlTx {
    fn drop(&mut self) {
        if self.finished.load(Ordering::SeqCst) {
            return;
        }
        // Rolled back before the connection's next statement, or right away when possible.
        self.shared.needs_rollback.store(true, Ordering::SeqCst);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let shared = Arc::clone(&self.shared);
            handle.spawn(async move {
                if let Err(e) = run_simple(&shared, None).await
                {
                    tracing::debug!(error = %e, "rollback of a dropped SQL Server transaction failed");
                }
            });
        }
    }
}

#[async_trait]
impl Tx for MsSqlTx {
    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        if self.finished.load(Ordering::SeqCst) {
            return Err(Error::invalid_input("the transaction has already ended"));
        }
        execute(&self.shared, sql, params, cancel).await
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        self.finished.store(true, Ordering::SeqCst);
        run_simple(&self.shared, Some(COMMIT_SQL)).await
    }

    async fn rollback(self: Box<Self>) -> Result<()> {
        self.finished.store(true, Ordering::SeqCst);
        run_simple(&self.shared, Some(ROLLBACK_SQL)).await
    }
}

// ---- simple statements (transactions) ---------------------------------------------------------

async fn simple(client: &mut TdsClient, sql: &str) -> std::result::Result<(), Mapped> {
    client
        .simple_query(sql)
        .await
        .map_err(map_error)?
        .into_results()
        .await
        .map_err(map_error)?;
    Ok(())
}

/// Rolls back a transaction whose [`MsSqlTx`] was dropped, if any.
async fn rollback_if_needed(
    shared: &Shared,
    client: &mut TdsClient,
) -> std::result::Result<(), Mapped> {
    if shared.needs_rollback.swap(false, Ordering::SeqCst) {
        simple(client, ROLLBACK_SQL).await?;
    }
    Ok(())
}

/// Runs a short statement without results on the session, after a pending rollback (`None`:
/// only the pending rollback).
async fn run_simple(shared: &Shared, sql: Option<&str>) -> Result<()> {
    let mut guard = shared.session.lock().await;
    let Some(session) = guard.as_mut() else {
        return Err(shared.closed());
    };
    let mut result = rollback_if_needed(shared, &mut session.client).await;
    if result.is_ok()
        && let Some(sql) = sql
    {
        result = simple(&mut session.client, sql).await;
    }
    match result {
        Ok(()) => Ok(()),
        Err(m) => {
            if m.fatal
                && let Some(session) = guard.take()
            {
                session.wire.kill();
                shared.mark_closed(format!("the connection was closed: {}", m.error));
            }
            Err(m.error)
        }
    }
}

// ---- statements -------------------------------------------------------------------------------

/// Starts `sql` on the session and returns its result stream (see the module docs).
async fn execute(
    shared: &Arc<Shared>,
    sql: &str,
    params: &[Value],
    cancel: CancellationToken,
) -> Result<ResultStream> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let params = params.iter().map(to_param).collect::<Result<Vec<_>>>()?;
    let guard = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(Error::Cancelled),
        guard = Arc::clone(&shared.session).lock_owned() => guard,
    };
    if guard.is_none() {
        return Err(shared.closed());
    }
    let token = cancel.child_token();
    let run = shared.begin_run(token.clone());
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    tokio::spawn(run_task(
        Arc::clone(shared),
        guard,
        sql.to_owned(),
        params,
        token,
        run,
        tx,
    ));
    Ok(stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
    .boxed())
}

/// How a run ended: the items to send after the session is released, and why the session must
/// be closed, if it must.
struct Outcome {
    tail: Vec<Result<ResultItem>>,
    close: Option<String>,
}

impl Outcome {
    fn items(tail: Vec<Result<ResultItem>>) -> Self {
        Self { tail, close: None }
    }
}

async fn run_task(
    shared: Arc<Shared>,
    mut guard: OwnedMutexGuard<Option<Session>>,
    sql: String,
    params: Vec<Param>,
    token: CancellationToken,
    run: u64,
    tx: Sender,
) {
    let outcome = match guard.as_mut() {
        Some(session) => {
            AssertUnwindSafe(run_statement(session, &shared, &sql, params, &token, &tx))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    tracing::error!(spid = shared.spid, "the SQL Server statement task panicked");
                    Outcome {
                        tail: vec![Err(Error::internal(
                            "the statement task panicked; the session was closed",
                        ))],
                        close: Some("the connection was closed after an internal error".into()),
                    }
                })
        }
        None => Outcome::items(vec![Err(shared.closed())]),
    };
    if let Some(reason) = outcome.close
        && let Some(session) = guard.take()
    {
        session.wire.kill();
        shared.mark_closed(reason);
    }
    shared.end_run(run);
    // Release the session before the last items, so a consumer that sees the end of the stream
    // can run the next statement right away.
    drop(guard);
    for item in outcome.tail {
        if tx.send(item).await.is_err() {
            break;
        }
    }
}

async fn run_statement(
    session: &mut Session,
    shared: &Shared,
    sql: &str,
    params: Vec<Param>,
    token: &CancellationToken,
    tx: &Sender,
) -> Outcome {
    let Session { client, wire } = session;
    if let Err(m) = rollback_if_needed(shared, client).await {
        return Outcome {
            close: m
                .fatal
                .then(|| format!("the connection was closed: {}", m.error)),
            tail: vec![Err(m.error)],
        };
    }
    let mut ctl = Ctl {
        token,
        tx,
        wire: &*wire,
        baseline: wire.client_messages(),
        state: State::Running,
        batch_rows: shared.batch_rows,
    };
    let result = if params.is_empty() {
        run_batch(client, sql, &mut ctl).await
    } else {
        run_rpc(client, sql, params, &mut ctl).await
    };
    ctl.finish(result).await
}

/// `execute(sql, [])`: one SQL batch in the session's scope.
async fn run_batch(
    client: &mut TdsClient,
    sql: &str,
    ctl: &mut Ctl<'_>,
) -> std::result::Result<(), Mapped> {
    let mut request = pin!(client.simple_query(sql));
    let mut results = ctl.wait(&mut request).await.map_err(map_error)?;
    let mut set = None;
    loop {
        let item = ctl.wait(&mut results.next()).await;
        match item {
            None => break,
            Some(Err(e)) => {
                ctl.flush(&mut set).await?;
                return Err(map_error(e));
            }
            Some(Ok(QueryItem::Metadata(meta))) => ctl.start_set(&mut set, meta.columns()).await?,
            Some(Ok(QueryItem::Row(row))) => ctl.push_row(&mut set, row).await?,
        }
    }
    ctl.flush(&mut set).await
}

/// `execute(sql, params)`: `sp_executesql` over RPC, which also reports row counts.
async fn run_rpc(
    client: &mut TdsClient,
    sql: &str,
    params: Vec<Param>,
    ctl: &mut Ctl<'_>,
) -> std::result::Result<(), Mapped> {
    let mut cmd = Command::new("sp_executesql");
    cmd.bind_param("@stmt", std::borrow::Cow::<str>::Owned(sql.to_owned()));
    cmd.bind_param(
        "@params",
        std::borrow::Cow::<str>::Owned(declarations(&params)),
    );
    for (i, p) in params.into_iter().enumerate() {
        cmd.bind_param(format!("@P{}", i + 1), p.data);
    }
    let mut request = pin!(cmd.exec(client));
    let mut results = ctl.wait(&mut request).await.map_err(map_error)?;
    let mut set = None;
    let mut just_closed = false;
    loop {
        let item = ctl.wait(&mut results.next()).await;
        let closed_before = std::mem::take(&mut just_closed);
        match item {
            None => break,
            Some(Err(e)) => {
                ctl.flush(&mut set).await?;
                return Err(map_error(e));
            }
            Some(Ok(CommandItem::Metadata(meta))) => {
                ctl.start_set(&mut set, meta.columns()).await?;
            }
            Some(Ok(CommandItem::Row(row))) => ctl.push_row(&mut set, row).await?,
            Some(Ok(CommandItem::RowsAffected(n))) => {
                if set.is_some() {
                    // The DONE that closes a result set counts its rows: not a write.
                    ctl.flush(&mut set).await?;
                    just_closed = true;
                } else if n != 0 || !closed_before {
                    // tiberius does not expose the DONE_COUNT bit (a DONE without a count
                    // reads as 0). A 0 right after a closed result set is the DONEPROC /
                    // DONEINPROC of the statement that produced it (`EXEC proc @P1`), not a
                    // write, so it is skipped.
                    ctl.emit(ResultItem::RowsAffected(n)).await;
                }
            }
            Some(Ok(_)) => {}
        }
    }
    ctl.flush(&mut set).await
}

#[derive(Debug, Clone, Copy)]
enum State {
    Running,
    /// Cancel requested; the request is not completely written yet.
    Waiting(Instant),
    /// Attention sent (or not needed); draining until the deadline.
    Sent(Instant),
    /// The socket was dropped.
    Dead,
}

/// Watches the cancel sources while a request runs (see the module docs).
struct Ctl<'a> {
    token: &'a CancellationToken,
    tx: &'a Sender,
    wire: &'a SharedWire<Transport>,
    baseline: u64,
    state: State,
    batch_rows: usize,
}

impl Ctl<'_> {
    fn cancelled(&self) -> bool {
        !matches!(self.state, State::Running)
    }

    /// Polls `fut` to completion, handling a cancel that arrives meanwhile without dropping it.
    async fn wait<F: Future + Unpin>(&mut self, fut: &mut F) -> F::Output {
        loop {
            match self.state {
                State::Running => {
                    let (token, tx) = (self.token, self.tx);
                    let done = tokio::select! {
                        biased;
                        () = token.cancelled() => None,
                        () = tx.closed() => None,
                        out = &mut *fut => Some(out),
                    };
                    match done {
                        Some(out) => return out,
                        None => self.begin_cancel().await,
                    }
                }
                State::Waiting(deadline) => {
                    let done = tokio::select! {
                        biased;
                        out = &mut *fut => Some(out),
                        () = tokio::time::sleep(ATTENTION_POLL) => None,
                    };
                    if let Some(out) = done {
                        return out;
                    }
                    if Instant::now() >= deadline {
                        self.kill();
                    } else {
                        self.try_attention().await;
                    }
                }
                State::Sent(deadline) => {
                    let done = tokio::select! {
                        biased;
                        out = &mut *fut => Some(out),
                        () = tokio::time::sleep_until(deadline) => None,
                    };
                    match done {
                        Some(out) => return out,
                        None => self.kill(),
                    }
                }
                State::Dead => return fut.await,
            }
        }
    }

    async fn begin_cancel(&mut self) {
        if matches!(self.state, State::Running) {
            self.state = State::Waiting(Instant::now() + CANCEL_DRAIN);
            self.try_attention().await;
        }
    }

    /// Sends the attention once the request is completely written (and still running).
    async fn try_attention(&mut self) {
        let State::Waiting(deadline) = self.state else {
            return;
        };
        if !self.wire.request_sent(self.baseline) {
            return;
        }
        if !self.wire.response_pending() {
            // Already answered: just drain what is left.
            self.state = State::Sent(deadline);
            return;
        }
        match tokio::time::timeout_at(deadline, self.wire.send_attention()).await {
            Ok(Ok(())) => self.state = State::Sent(deadline),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "cannot send the SQL Server attention; closing the session");
                self.kill();
            }
            Err(_) => self.kill(),
        }
    }

    fn kill(&mut self) {
        tracing::warn!("the cancelled SQL Server request did not end in time; closing the session");
        self.wire.kill();
        self.state = State::Dead;
    }

    /// Sends `item` unless the run was cancelled; a dropped consumer cancels.
    async fn emit(&mut self, item: ResultItem) {
        if self.cancelled() {
            return;
        }
        let (token, tx) = (self.token, self.tx);
        let sent = tokio::select! {
            biased;
            () = token.cancelled() => false,
            r = tx.send(Ok(item)) => r.is_ok(),
        };
        if !sent {
            self.begin_cancel().await;
        }
    }

    async fn start_set(
        &mut self,
        set: &mut Option<BatchBuilder>,
        columns: &[tiberius::Column],
    ) -> std::result::Result<(), Mapped> {
        self.flush(set).await?;
        let shape = ResultShape::new(columns);
        self.emit(ResultItem::Schema(shape.schema.clone())).await;
        *set = Some(BatchBuilder::new(&shape));
        Ok(())
    }

    async fn push_row(
        &mut self,
        set: &mut Option<BatchBuilder>,
        row: tiberius::Row,
    ) -> std::result::Result<(), Mapped> {
        if self.cancelled() {
            return Ok(()); // drain without converting
        }
        let Some(builder) = set.as_mut() else {
            return Err(Mapped {
                error: Error::internal("SQL Server sent a row before its columns"),
                fatal: true,
            });
        };
        builder.push(row);
        if builder.rows() >= self.batch_rows || builder.bytes() >= BATCH_BYTES {
            let batch = builder.finish().map_err(|error| Mapped {
                error,
                fatal: false,
            })?;
            self.emit(ResultItem::Batch(batch)).await;
        }
        Ok(())
    }

    /// Ends the current result set: its last rows and a warning about values stored as NULL.
    async fn flush(&mut self, set: &mut Option<BatchBuilder>) -> std::result::Result<(), Mapped> {
        let Some(mut builder) = set.take() else {
            return Ok(());
        };
        if self.cancelled() {
            return Ok(());
        }
        if builder.rows() > 0 {
            let batch = builder.finish().map_err(|error| Mapped {
                error,
                fatal: false,
            })?;
            self.emit(ResultItem::Batch(batch)).await;
        }
        let lossy = builder.take_lossy();
        if !lossy.is_empty() {
            let detail: Vec<String> = lossy.iter().map(|(c, n)| format!("{c} ({n})")).collect();
            self.emit(ResultItem::Notice(Notice::warning(format!(
                "some values could not be converted and are shown as NULL: {}",
                detail.join(", ")
            ))))
            .await;
        }
        Ok(())
    }

    /// The items after the request ended, and whether the session must close.
    async fn finish(self, result: std::result::Result<(), Mapped>) -> Outcome {
        let state = if self.wire.is_dead() && self.cancelled() {
            State::Dead
        } else {
            self.state
        };
        let deadline = match state {
            State::Running => return finished(result),
            State::Dead => {
                return Outcome {
                    tail: vec![Err(Error::Cancelled)],
                    close: Some(CLOSED_AFTER_CANCEL.to_owned()),
                };
            }
            State::Waiting(d) | State::Sent(d) => d,
        };
        if let Err(m) = &result
            && m.fatal
        {
            self.wire.kill();
            return Outcome {
                tail: vec![Err(Error::Cancelled)],
                close: Some(CLOSED_AFTER_CANCEL.to_owned()),
            };
        }
        if self.wire.acks_outstanding() > 0
            && !matches!(
                tokio::time::timeout_at(deadline, self.wire.wait_acks()).await,
                Ok(Ok(()))
            )
        {
            tracing::warn!("no acknowledgement of the SQL Server attention; closing the session");
            self.wire.kill();
            return Outcome {
                tail: vec![Err(Error::Cancelled)],
                close: Some(CLOSED_AFTER_CANCEL.to_owned()),
            };
        }
        Outcome::items(vec![Err(Error::Cancelled)])
    }
}

fn finished(result: std::result::Result<(), Mapped>) -> Outcome {
    match result {
        Ok(()) => Outcome::items(Vec::new()),
        Err(m) => Outcome {
            close: m
                .fatal
                .then(|| format!("the connection was closed: {}", m.error)),
            tail: vec![Err(m.error)],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::TlsOptions;

    fn params() -> ConnectParams {
        ConnectParams::network(Engine::MsSql, "db.example")
            .with_user("sa")
            .with_password("pw")
    }

    #[test]
    fn options_are_parsed_and_unknown_keys_refused() {
        let s = Settings::from_params(&params()).unwrap();
        assert_eq!(
            s,
            Settings {
                instance: None,
                strict: false,
                read_only: false,
                batch_rows: BATCH_ROWS
            }
        );

        let mut p = params()
            .with_port(1435)
            .with_tls(TlsOptions::new(TlsMode::VerifyFull));
        p.options
            .insert(options::INSTANCE.into(), "SQLEXPRESS".into());
        p.options.insert(options::ENCRYPT.into(), "Strict".into());
        p.options
            .insert(options::APPLICATION_INTENT.into(), "read_only".into());
        p.options.insert(options::BATCH_ROWS.into(), "100".into());
        let s = Settings::from_params(&p).unwrap();
        assert_eq!(s.instance.as_deref(), Some("SQLEXPRESS"));
        assert!(s.strict && s.read_only);
        assert_eq!(s.batch_rows, 100);

        let mut bad = params();
        bad.options
            .insert("trustServerCertificate".into(), "true".into());
        let err = Settings::from_params(&bad).unwrap_err();
        assert!(err.to_string().contains("trustServerCertificate"), "{err}");

        for (key, value) in [
            (options::ENCRYPT, "maybe"),
            (options::APPLICATION_INTENT, "write"),
            (options::BATCH_ROWS, "0"),
        ] {
            let mut p = params();
            p.options.insert(key.into(), value.into());
            assert!(Settings::from_params(&p).is_err(), "{key}={value}");
        }
    }

    #[test]
    fn strict_needs_a_tls_mode_and_instances_need_a_port() {
        for mode in [TlsMode::Disable, TlsMode::Prefer] {
            let mut p = params().with_tls(TlsOptions::new(mode));
            p.options.insert(options::ENCRYPT.into(), "strict".into());
            assert!(matches!(
                Settings::from_params(&p),
                Err(Error::InvalidInput { .. })
            ));
        }
        let mut p = params().with_tls(TlsOptions::new(TlsMode::Require));
        p.options.insert(options::ENCRYPT.into(), "strict".into());
        assert!(Settings::from_params(&p).unwrap().strict);

        let mut p = params();
        p.options
            .insert(options::INSTANCE.into(), "SQLEXPRESS".into());
        let err = Settings::from_params(&p).unwrap_err();
        assert!(err.to_string().contains("port"), "{err}");
    }

    #[test]
    fn version_numbers_come_from_product_version() {
        let banner = "Microsoft SQL Server 2025 (RTM-CU10) (KB5050000) - 17.0.4005.7 (X64) \n\
                      \tOct 1 2026 12:00:00 \n\tCopyright (C) 2025 Microsoft Corporation";
        let v = server_version("17.0.4005.7", banner);
        assert_eq!((v.major, v.minor, v.patch, v.build), (17, 0, 4005, 7));
        assert!(
            v.raw().starts_with("Microsoft SQL Server 2025"),
            "{}",
            v.raw()
        );
        assert!(!v.raw().contains('\n'));
        let v = server_version("16.0.1000.6", "");
        assert_eq!(v.major, 16);
        assert_eq!(server_version("", "").major, 0);
    }

    #[tokio::test]
    async fn refuses_bad_parameters_before_dialling() {
        let pg = ConnectParams::network(Engine::Postgres, "db");
        assert!(matches!(
            connect(&pg).await,
            Err(Error::InvalidInput { .. })
        ));
        let no_user = ConnectParams::network(Engine::MsSql, "db");
        assert!(matches!(
            connect(&no_user).await,
            Err(Error::InvalidInput { .. })
        ));
        let ssh = oxbase_types::SshOptions::new("bastion", "ops", oxbase_types::SshAuth::Agent);
        assert!(matches!(
            connect(&params().with_ssh(ssh)).await,
            Err(Error::InvalidInput { .. })
        ));
        let cert = params().with_tls(TlsOptions::new(TlsMode::Require).with_client_identity(
            oxbase_types::PemSource::Pem("x".into()),
            oxbase_types::KeySource::Pem(oxbase_types::Secret::new("k")),
        ));
        assert!(matches!(
            connect(&cert).await,
            Err(Error::Unsupported { .. } | Error::InvalidInput { .. })
        ));
    }

    #[tokio::test]
    async fn connect_errors_name_the_server_and_hide_the_password() {
        // Port 1 on loopback refuses connections.
        let marker = "MARKER-password-5678";
        let p = ConnectParams::network(Engine::MsSql, "127.0.0.1")
            .with_port(1)
            .with_user("u")
            .with_password(marker)
            .with_tls(TlsOptions::new(TlsMode::Disable));
        let err = connect(&p).await.unwrap_err();
        let text = format!("{err} {err:?}");
        assert!(matches!(err, Error::Connection { .. }), "{text}");
        assert!(text.contains("127.0.0.1:1"), "{text}");
        assert!(!text.contains(marker), "{text}");
    }
}
