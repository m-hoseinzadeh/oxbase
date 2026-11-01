//! Connections, statement execution, cancellation and transactions.
//!
//! # How a statement runs
//!
//! Every `execute` spawns a task that waits for the connection's statement lock, runs the
//! statement and feeds a bounded channel (the returned stream), so at most a couple of batches
//! are buffered and nothing is ever collected in memory.
//!
//! - **No parameters, one statement**: the statement is prepared (Parse/Describe, which gives
//!   the column types) and then run with the simple query protocol, whose text values are
//!   decoded into typed Arrow columns. Text output means any type, including ones this driver
//!   does not know, arrives exactly as the server prints it.
//! - **No parameters, several statements** (a script): the simple query protocol, one result
//!   per statement. Column types are not available on this path, so every column is `Utf8`
//!   (the server's text form, no `oxbase.pg.*` metadata). Run statements one at a time to get
//!   typed columns.
//! - **With parameters**: the extended protocol. The statement is prepared, the server infers
//!   the parameter types and [`crate::encode`] converts each [`Value`] to that type in binary.
//!   Results arrive in binary and are decoded by [`crate::decode`].
//!
//! Server notices (`RAISE NOTICE`, warnings) are captured from the connection and interleaved
//! with the results as `ResultItem::Notice`, in arrival order.
//!
//! # Cancellation
//!
//! A statement is interrupted when its token fires, when `cancel_running` is called (which
//! interrupts every statement submitted before the call, the running one and any still queued
//! behind it), or when its stream is dropped. Then the task:
//!
//! 1. stops forwarding results, yields `Err(Cancelled)` (unless the stream was dropped) and
//!    drops its senders, so the stream ends right after that error;
//! 2. sends no further protocol message for this statement (a statement interrupted while its
//!    `Parse` or a pending `ROLLBACK` was in flight is never sent at all);
//! 3. sends a PostgreSQL `CancelRequest` through `tokio-postgres`' `CancelToken` and repeats it
//!    with backoff until the statement has ended, so a request that reached the backend just
//!    before the statement did is not lost. It dials the same IP and port as the session (the
//!    SSH tunnel's local end when tunnelled) with the session's TLS settings;
//! 4. keeps the statement lock until the statement has ended and the last cancel request has
//!    been written, so a cancel cannot target a later statement of this driver. What remains is
//!    the protocol's inherent race: `tokio-postgres` does not wait for the server to act on a
//!    cancel request, so one that the server processes late can still reach the next statement.
//!
//! A server-side `query_canceled` error this driver did not cause (`statement_timeout`,
//! `pg_cancel_backend()` from another session) is reported as an SQL error with code `57014`
//! and the server's message, not as `Cancelled`.

use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use oxbase_driver::{
    CancellationToken, ConnectParams, Connection, Notice, ResultItem, ResultStream, TlsMode, Tx,
    collect,
};
use oxbase_types::{Endpoint, Engine, Error, Result, ServerVersion, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_postgres::config::{ChannelBinding, SslNegotiation, TargetSessionAttrs};
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{FromSql, ToSql, Type};
use tokio_postgres::{AsyncMessage, CancelToken, Client, Config, SimpleQueryMessage};

use crate::decode::BatchBuilder;
use crate::encode::PgParam;
use crate::error::{from_pg, notice_from_db};
use crate::split::count_statements;
use crate::tls::{PgTls, ssl_mode, ssl_negotiation};
use crate::types::ColSpec;

/// Notices queued beyond this are dropped (and counted) instead of growing without bound.
const NOTICE_BACKLOG: usize = 10_000;
/// How long a cancelled statement may take to end before the lock is released anyway.
const DRAIN_LIMIT: Duration = Duration::from_secs(30);
/// Deadline for delivering a cancel request.
const CANCEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Delay before the first repeated cancel request; it doubles up to [`CANCEL_RESEND_MAX`].
const CANCEL_RESEND_FIRST: Duration = Duration::from_millis(250);
const CANCEL_RESEND_MAX: Duration = Duration::from_secs(2);
/// Whole-connect deadline when `ConnectParams::connect_timeout` is not set.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `close` waits for the session to say goodbye.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

fn lock<T>(m: &StdMutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------------
// Notices

struct NoticeTx {
    tx: mpsc::UnboundedSender<Notice>,
    queued: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

impl NoticeTx {
    fn push(&self, notice: Notice) {
        if self.queued.load(Ordering::Acquire) >= NOTICE_BACKLOG {
            self.dropped.fetch_add(1, Ordering::AcqRel);
            return;
        }
        self.queued.fetch_add(1, Ordering::AcqRel);
        if self.tx.send(notice).is_err() {
            self.queued.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// The receiving end of the connection's notices; owned by whoever holds the statement lock.
pub(crate) struct NoticeRx {
    rx: mpsc::UnboundedReceiver<Notice>,
    queued: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

impl NoticeRx {
    fn dropped_notice(&self) -> Option<Notice> {
        let n = self.dropped.swap(0, Ordering::AcqRel);
        (n > 0).then(|| {
            Notice::warning(format!(
                "{n} server notices were discarded because they arrived faster than they were read"
            ))
        })
    }

    async fn recv(&mut self) -> Option<Notice> {
        if let Some(n) = self.dropped_notice() {
            return Some(n);
        }
        let n = self.rx.recv().await?;
        self.queued.fetch_sub(1, Ordering::AcqRel);
        Some(n)
    }

    fn try_recv(&mut self) -> Option<Notice> {
        if let Some(n) = self.dropped_notice() {
            return Some(n);
        }
        let n = self.rx.try_recv().ok()?;
        self.queued.fetch_sub(1, Ordering::AcqRel);
        Some(n)
    }
}

fn notice_channel() -> (NoticeTx, NoticeRx) {
    let (tx, rx) = mpsc::unbounded_channel();
    let queued = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    (
        NoticeTx {
            tx,
            queued: queued.clone(),
            dropped: dropped.clone(),
        },
        NoticeRx {
            rx,
            queued,
            dropped,
        },
    )
}

// ---------------------------------------------------------------------------------------------
// Shared connection state

/// How `execute` sends SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Pick the protocol from the parameters and statement count (see the module docs).
    Auto,
    /// Internal commands (`BEGIN`, `COMMIT`, ...): simple protocol, no prepare.
    Script,
}

struct Shared {
    /// `None` after `close`.
    client: StdMutex<Option<Arc<Client>>>,
    cancel_token: CancelToken,
    /// The session's TLS settings, for cancel requests.
    cancel_tls: PgTls,
    /// The statement lock; it owns the notice receiver.
    exec: Arc<tokio::sync::Mutex<NoticeRx>>,
    /// Parent of the stop token of every statement submitted since the last `cancel_running`
    /// (or `close`). Each statement takes a child token in `execute`, before its task is
    /// spawned, so cancelling this one reaches running and queued statements alike.
    stop_all: StdMutex<CancellationToken>,
    /// Set when a `Tx` was dropped without commit / rollback.
    rollback_pending: AtomicBool,
    tx_open: AtomicBool,
    conn_task: StdMutex<Option<JoinHandle<()>>>,
}

impl Shared {
    fn client(&self) -> Result<Arc<Client>> {
        let client = lock(&self.client)
            .clone()
            .ok_or_else(|| Error::connection("the connection is closed"))?;
        if client.is_closed() {
            return Err(Error::connection("the connection to the server is closed"));
        }
        Ok(client)
    }

    fn execute(
        self: &Arc<Self>,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
        mode: Mode,
    ) -> Result<ResultStream> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let client = self.client()?;
        let (tx, rx) = mpsc::channel(2);
        let job = Job {
            shared: self.clone(),
            client,
            sql: sql.to_owned(),
            params: params.to_vec(),
            cancel,
            stop: lock(&self.stop_all).child_token(),
            mode,
        };
        tokio::spawn(job.run(tx));
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })
        .boxed())
    }

    /// Runs an internal command and waits for it to finish.
    async fn run_to_end(self: &Arc<Self>, sql: &str) -> Result<()> {
        let stream = self.execute(sql, &[], CancellationToken::new(), Mode::Script)?;
        collect(stream).await.map(|_| ())
    }

    /// Interrupts every statement submitted so far (running or queued).
    fn stop_all(&self) {
        let old = std::mem::replace(&mut *lock(&self.stop_all), CancellationToken::new());
        old.cancel();
    }

    async fn send_cancel_request(&self) {
        match tokio::time::timeout(
            CANCEL_REQUEST_TIMEOUT,
            self.cancel_token.cancel_query(self.cancel_tls.clone()),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %from_pg(&e), "PostgreSQL cancel request failed");
            }
            Err(_) => tracing::warn!("PostgreSQL cancel request timed out"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Statement jobs

/// Where a job sends its items. Once `halt` fires nothing more is sent, and no new protocol
/// message may be started for the statement ([`Sink::check`]).
struct Sink {
    out: StdMutex<Option<mpsc::Sender<Result<ResultItem>>>>,
    halt: CancellationToken,
}

impl Sink {
    async fn send(&self, item: Result<ResultItem>) {
        if self.halt.is_cancelled() {
            return;
        }
        // The clone lives only for this send, so taking `out` ends the stream promptly.
        let Some(tx) = lock(&self.out).clone() else {
            return;
        };
        tokio::select! {
            biased;
            () = self.halt.cancelled() => {}
            // A closed channel is noticed by the job's select.
            _ = tx.send(item) => {}
        }
    }

    async fn item(&self, item: ResultItem) {
        self.send(Ok(item)).await;
    }

    /// `Err(Cancelled)` once the statement was interrupted: called before every request that
    /// would start new work on the server.
    fn check(&self) -> Result<()> {
        if self.halt.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

struct Job {
    shared: Arc<Shared>,
    client: Arc<Client>,
    sql: String,
    params: Vec<Value>,
    /// The caller's token.
    cancel: CancellationToken,
    /// Child of `Shared::stop_all`.
    stop: CancellationToken,
    mode: Mode,
}

impl Job {
    async fn run(self, out: mpsc::Sender<Result<ResultItem>>) {
        let Job {
            shared,
            client,
            sql,
            params,
            cancel,
            stop,
            mode,
        } = self;
        // Wait for the previous statement on this connection to end. Nothing has been sent for
        // this one yet, so an interruption here needs no cancel request.
        let mut notices = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                let _ = out.send(Err(Error::Cancelled)).await;
                return;
            }
            () = stop.cancelled() => {
                let _ = out.send(Err(Error::Cancelled)).await;
                return;
            }
            () = out.closed() => return,
            guard = shared.exec.clone().lock_owned() => guard,
        };
        let sink = Sink {
            out: StdMutex::new(Some(out.clone())),
            halt: CancellationToken::new(),
        };
        let work = async {
            if shared.rollback_pending.swap(false, Ordering::AcqRel) {
                if let Err(e) = sink.check() {
                    // Not sent: the next statement rolls back instead.
                    shared.rollback_pending.store(true, Ordering::Release);
                    return Err(e);
                }
                if let Err(e) = client.batch_execute("ROLLBACK").await {
                    tracing::debug!(error = %from_pg(&e), "rollback of a dropped transaction failed");
                }
            }
            let result = run_statement(&client, &sql, &params, mode, &mut notices, &sink).await;
            drain_notices(&mut notices, &sink).await;
            result
        };
        tokio::pin!(work);
        let interrupted = tokio::select! {
            biased;
            result = &mut work => {
                if let Err(e) = result {
                    sink.send(Err(e)).await;
                }
                None
            }
            () = cancel.cancelled() => Some(true),
            () = stop.cancelled() => Some(true),
            () = out.closed() => Some(false),
        };
        if let Some(report) = interrupted {
            sink.halt.cancel();
            // Drop every sender (the job's, the sink's, and on its next poll the one inside an
            // in-flight `Sink::send`) so the stream ends right after `Err(Cancelled)`.
            drop(lock(&sink.out).take());
            if report {
                tokio::spawn(async move {
                    let _ = out.send(Err(Error::Cancelled)).await;
                });
            } else {
                drop(out);
            }
            if tokio::time::timeout(DRAIN_LIMIT, cancel_until_done(&shared, work))
                .await
                .is_err()
            {
                tracing::warn!(
                    "a cancelled PostgreSQL statement did not end within {DRAIN_LIMIT:?}"
                );
            }
        }
        // `work` and then `notices` (the statement lock) are dropped here.
    }
}

/// Sends cancel requests (with backoff) while driving the interrupted statement `work` to its
/// end. A request already on its way when `work` ends is completed before returning, so it
/// cannot reach a later statement while this one's lock is released.
async fn cancel_until_done<F: Future>(shared: &Shared, mut work: Pin<&mut F>) {
    let mut delay = CANCEL_RESEND_FIRST;
    loop {
        let started = AtomicBool::new(false);
        let send = async {
            started.store(true, Ordering::Relaxed);
            shared.send_cancel_request().await;
        };
        tokio::pin!(send);
        let finished = tokio::select! {
            biased;
            _ = work.as_mut() => true,
            () = send.as_mut() => false,
        };
        if finished {
            if started.load(Ordering::Relaxed) {
                send.await;
            }
            return;
        }
        tokio::select! {
            biased;
            _ = work.as_mut() => return,
            () = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(CANCEL_RESEND_MAX);
    }
}

async fn drain_notices(notices: &mut NoticeRx, sink: &Sink) {
    while let Some(n) = notices.try_recv() {
        sink.item(ResultItem::Notice(n)).await;
    }
}

async fn flush(builder: &mut BatchBuilder, sink: &Sink) -> Result<()> {
    if let Some(batch) = builder.finish()? {
        sink.item(ResultItem::Batch(batch)).await;
    }
    for n in builder.take_warnings() {
        sink.item(ResultItem::Notice(n)).await;
    }
    Ok(())
}

async fn run_statement(
    client: &Client,
    sql: &str,
    params: &[Value],
    mode: Mode,
    notices: &mut NoticeRx,
    sink: &Sink,
) -> Result<()> {
    drain_notices(notices, sink).await;
    if !params.is_empty() {
        return run_extended(client, sql, params, notices, sink).await;
    }
    let typed = match mode {
        Mode::Script => None,
        Mode::Auto => match count_statements(sql) {
            0 => return Ok(()),
            1 => match sink.check().map(|()| client.prepare(sql))?.await {
                Ok(stmt) => Some(
                    stmt.columns()
                        .iter()
                        .map(|c| ColSpec::typed(c.name(), c.type_(), c.type_modifier()))
                        .collect::<Vec<_>>(),
                ),
                // A syntax error, which includes "cannot insert multiple commands into a
                // prepared statement" (the lexer saw one statement where the server sees
                // several): run it as a script. The SQLSTATE is checked, not the message, so
                // this works with any `lc_messages`. A real syntax error is reported again by
                // the simple protocol, which parses the whole string before running any of it.
                Err(e) if e.code() == Some(&SqlState::SYNTAX_ERROR) => None,
                Err(e) => return Err(from_pg(&e)),
            },
            _ => None,
        },
    };
    run_simple(client, sql, typed, notices, sink).await
}

async fn run_simple(
    client: &Client,
    sql: &str,
    mut typed: Option<Vec<ColSpec>>,
    notices: &mut NoticeRx,
    sink: &Sink,
) -> Result<()> {
    sink.check()?;
    let stream = client
        .simple_query_raw(sql)
        .await
        .map_err(|e| from_pg(&e))?;
    tokio::pin!(stream);
    let mut current: Option<BatchBuilder> = None;
    loop {
        let msg = tokio::select! {
            biased;
            Some(n) = notices.recv() => {
                sink.item(ResultItem::Notice(n)).await;
                continue;
            }
            msg = stream.next() => msg,
        };
        match msg {
            None => break,
            Some(Err(e)) => {
                if let Some(b) = current.as_mut() {
                    flush(b, sink).await?;
                }
                return Err(from_pg(&e));
            }
            Some(Ok(SimpleQueryMessage::RowDescription(cols))) => {
                let specs = match typed.take() {
                    Some(specs) if specs.len() == cols.len() => specs,
                    _ => cols.iter().map(|c| ColSpec::untyped(c.name())).collect(),
                };
                let builder = BatchBuilder::new(specs)?;
                sink.item(ResultItem::Schema(builder.schema())).await;
                current = Some(builder);
            }
            Some(Ok(SimpleQueryMessage::Row(row))) => {
                let b = current
                    .as_mut()
                    .ok_or_else(|| Error::internal("PostgreSQL sent a row before its columns"))?;
                for i in 0..b.num_columns() {
                    let value = row.try_get(i).map_err(|e| from_pg(&e))?;
                    b.append_text(i, value);
                }
                b.end_row();
                if b.is_full() {
                    flush(b, sink).await?;
                }
            }
            Some(Ok(SimpleQueryMessage::CommandComplete(n))) => match current.take() {
                Some(mut b) => flush(&mut b, sink).await?,
                None => sink.item(ResultItem::RowsAffected(n)).await,
            },
            Some(Ok(_)) => {}
        }
    }
    Ok(())
}

/// Any column value as raw bytes (binary format).
struct RawValue<'a>(&'a [u8]);

impl<'a> FromSql<'a> for RawValue<'a> {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Self(raw))
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

async fn run_extended(
    client: &Client,
    sql: &str,
    params: &[Value],
    notices: &mut NoticeRx,
    sink: &Sink,
) -> Result<()> {
    sink.check()?;
    let stmt = client.prepare(sql).await.map_err(|e| from_pg(&e))?;
    if stmt.params().len() != params.len() {
        return Err(Error::invalid_input(format!(
            "the statement has {} parameter(s) but {} value(s) were given",
            stmt.params().len(),
            params.len()
        )));
    }
    let bound: Vec<PgParam<'_>> = params.iter().map(PgParam).collect();
    sink.check()?;
    let rows = client
        .query_raw(&stmt, bound.iter().map(|p| p as &(dyn ToSql + Sync)))
        .await
        .map_err(|e| from_pg(&e))?;
    tokio::pin!(rows);
    let specs: Vec<ColSpec> = stmt
        .columns()
        .iter()
        .map(|c| ColSpec::typed(c.name(), c.type_(), c.type_modifier()))
        .collect();
    let mut builder = if specs.is_empty() {
        None
    } else {
        let b = BatchBuilder::new(specs)?;
        sink.item(ResultItem::Schema(b.schema())).await;
        Some(b)
    };
    loop {
        let next = tokio::select! {
            biased;
            Some(n) = notices.recv() => {
                sink.item(ResultItem::Notice(n)).await;
                continue;
            }
            next = rows.next() => next,
        };
        match next {
            None => break,
            Some(Err(e)) => {
                if let Some(b) = builder.as_mut() {
                    flush(b, sink).await?;
                }
                return Err(from_pg(&e));
            }
            Some(Ok(row)) => {
                if let Some(b) = builder.as_mut() {
                    for i in 0..b.num_columns() {
                        let raw: Option<RawValue<'_>> = row.try_get(i).map_err(|e| from_pg(&e))?;
                        b.append_binary(i, raw.map(|r| r.0));
                    }
                    b.end_row();
                    if b.is_full() {
                        flush(b, sink).await?;
                    }
                }
            }
        }
    }
    match builder.as_mut() {
        Some(b) => flush(b, sink).await?,
        None => {
            sink.item(ResultItem::RowsAffected(rows.rows_affected().unwrap_or(0)))
                .await;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Connecting

/// An open PostgreSQL session.
pub struct PgConnection {
    shared: Arc<Shared>,
    version: ServerVersion,
    tls_in_use: bool,
    server: Endpoint,
    /// The connected database as the server reports it (`current_database()`); when that could
    /// not be read, `ConnectParams::database` or the user name (the server default).
    database: String,
}

impl std::fmt::Debug for PgConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgConnection")
            .field("server", &self.server)
            .field("version", &self.version.raw())
            .field("tls_in_use", &self.tls_in_use)
            .field("database", &self.database)
            .finish_non_exhaustive()
    }
}

fn option_value<'a>(params: &'a ConnectParams, key: &str) -> Option<&'a str> {
    params.options.get(key).map(String::as_str)
}

fn apply_options(cfg: &mut Config, params: &ConnectParams) -> Result<()> {
    // Output formats the text decoder expects (users may still change them in the session), and
    // standard string literals, which the SQL lexer (`check_expression`) and the generated DDL
    // assume. User options come last, so a profile may still override them.
    let mut startup =
        String::from("-c DateStyle=ISO -c bytea_output=hex -c standard_conforming_strings=on");
    if let Some(extra) = option_value(params, "options").filter(|o| !o.trim().is_empty()) {
        startup.push(' ');
        startup.push_str(extra);
    }
    cfg.options(startup);
    for (key, value) in &params.options {
        match key.as_str() {
            "options" | "sslnegotiation" => {}
            "channel_binding" => {
                cfg.channel_binding(match value.trim() {
                    "disable" => ChannelBinding::Disable,
                    "prefer" => ChannelBinding::Prefer,
                    "require" => ChannelBinding::Require,
                    other => {
                        return Err(Error::invalid_input(format!(
                            "unknown channel_binding '{other}' (expected disable, prefer or require)"
                        )));
                    }
                });
            }
            "target_session_attrs" => {
                cfg.target_session_attrs(match value.trim() {
                    "any" => TargetSessionAttrs::Any,
                    "read-write" => TargetSessionAttrs::ReadWrite,
                    "read-only" => TargetSessionAttrs::ReadOnly,
                    other => {
                        return Err(Error::invalid_input(format!(
                            "unknown target_session_attrs '{other}' (expected any, read-write or \
                             read-only)"
                        )));
                    }
                });
            }
            other => tracing::debug!(option = other, "ignoring unknown PostgreSQL option"),
        }
    }
    Ok(())
}

fn connect_error(err: &tokio_postgres::Error, server: &Endpoint, dial: &Endpoint) -> Error {
    let via = if dial == server {
        String::new()
    } else {
        format!(" via {dial}")
    };
    match from_pg(err) {
        Error::Connection { message } => {
            Error::connection(format!("PostgreSQL at {server}{via}: {message}"))
        }
        Error::Timeout { message } => {
            Error::timeout(format!("PostgreSQL at {server}{via}: {message}"))
        }
        other => other,
    }
}

impl PgConnection {
    /// Opens a session (see the crate docs for addressing, TLS and options).
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        params.validate()?;
        if params.engine != Engine::Postgres {
            return Err(Error::invalid_input(format!(
                "the PostgreSQL driver cannot open {} connections",
                params.engine.display_name()
            )));
        }
        oxbase_net::tls::install_default_crypto_provider();
        let server = params
            .server_endpoint()
            .ok_or_else(|| Error::invalid_input("PostgreSQL needs a host"))?;
        let dial = params
            .dial_endpoint()
            .ok_or_else(|| Error::invalid_input("PostgreSQL needs a host"))?;
        let user = params
            .user
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .ok_or_else(|| Error::invalid_input("PostgreSQL needs a user name"))?;

        let server_host = server.host.trim_start_matches('[').trim_end_matches(']');
        let unix_socket = server_host.starts_with('/') && params.dial_addr.is_none();
        let mut cfg = Config::new();
        match dial.ip() {
            // tokio-postgres dials `hostaddr`; `host` only names the server.
            Some(ip) => {
                if server_host.starts_with('/') {
                    cfg.host(ip.to_string());
                } else {
                    cfg.host(server_host);
                }
                cfg.hostaddr(ip);
            }
            None => {
                cfg.host(&dial.host);
            }
        }
        cfg.port(dial.port);
        cfg.user(user);
        if let Some(password) = &params.password {
            cfg.password(password.expose());
        }
        let database = if let Some(db) = params.database.as_deref().filter(|d| !d.trim().is_empty())
        {
            cfg.dbname(db);
            db.to_owned()
        } else {
            user.to_owned()
        };
        cfg.application_name(params.application_name.as_deref().unwrap_or("oxbase"));
        if let Some(t) = params.connect_timeout {
            cfg.connect_timeout(t);
        }

        // PostgreSQL has no TLS over Unix sockets: Prefer means plaintext there.
        let mut effective = params.clone();
        if unix_socket && effective.tls.mode == TlsMode::Prefer {
            effective.tls.mode = TlsMode::Disable;
        }
        let mode = effective.tls.mode;
        let negotiation = ssl_negotiation(option_value(params, "sslnegotiation"), mode)?;
        cfg.ssl_mode(ssl_mode(mode));
        cfg.ssl_negotiation(negotiation);
        apply_options(&mut cfg, params)?;

        let tls = PgTls::new(&effective, negotiation == SslNegotiation::Direct)?;
        let (session_tls, tls_used) = tls.tracked();
        let limit = params.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT);
        let (client, mut connection) =
            match tokio::time::timeout(limit, cfg.connect(session_tls)).await {
                Err(_) => {
                    return Err(Error::timeout(format!(
                        "connecting to PostgreSQL at {server} took longer than {limit:?}"
                    )));
                }
                Ok(Err(e)) => return Err(connect_error(&e, &server, &dial)),
                Ok(Ok(pair)) => pair,
            };

        let version = connection
            .parameter("server_version")
            .and_then(|v| ServerVersion::parse(v).ok())
            .unwrap_or_else(|| {
                tracing::warn!("PostgreSQL did not report a parsable server_version");
                ServerVersion::new(0, 0, 0)
            });

        let (notice_tx, notice_rx) = notice_channel();
        let conn_task = tokio::spawn(async move {
            loop {
                match poll_fn(|cx| connection.poll_message(cx)).await {
                    Some(Ok(AsyncMessage::Notice(db))) => notice_tx.push(notice_from_db(&db)),
                    // LISTEN/NOTIFY is not surfaced at M1.
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::debug!(error = %from_pg(&e), "PostgreSQL session ended");
                        break;
                    }
                    None => break,
                }
            }
        });

        // The database the server actually opened (a pooler alias such as pgbouncer's
        // `[databases] app = dbname=real` connects to `real`); the requested name is the fallback.
        let database = match tokio::time::timeout(
            limit,
            client.simple_query("SELECT pg_catalog.current_database()"),
        )
        .await
        {
            Ok(Ok(messages)) => messages
                .into_iter()
                .find_map(|m| match m {
                    SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
                    _ => None,
                })
                .unwrap_or(database),
            Ok(Err(e)) => {
                tracing::debug!(error = %from_pg(&e), "could not read current_database()");
                database
            }
            Err(_) => {
                tracing::debug!("reading current_database() timed out");
                database
            }
        };

        let shared = Arc::new(Shared {
            cancel_token: client.cancel_token(),
            client: StdMutex::new(Some(Arc::new(client))),
            cancel_tls: tls,
            exec: Arc::new(tokio::sync::Mutex::new(notice_rx)),
            stop_all: StdMutex::new(CancellationToken::new()),
            rollback_pending: AtomicBool::new(false),
            tx_open: AtomicBool::new(false),
            conn_task: StdMutex::new(Some(conn_task)),
        });
        Ok(Self {
            shared,
            version,
            tls_in_use: tls_used.load(Ordering::Acquire),
            server,
            database,
        })
    }
}

#[async_trait]
impl Connection for PgConnection {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn server_version(&self) -> &ServerVersion {
        &self.version
    }

    fn tls_in_use(&self) -> bool {
        self.tls_in_use
    }

    /// The connected database, read from the server right after connecting
    /// (`ConnectParams::database` or the user name when that failed).
    fn current_database(&self) -> Option<String> {
        Some(self.database.clone())
    }

    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        self.shared.execute(sql, params, cancel, Mode::Auto)
    }

    /// Interrupts the running statement and any submitted after it that are still queued (see
    /// the module docs). A statement submitted after this call is not affected.
    async fn cancel_running(&self) -> Result<()> {
        self.shared.stop_all();
        Ok(())
    }

    async fn begin(&self) -> Result<Box<dyn Tx>> {
        if self.shared.tx_open.swap(true, Ordering::AcqRel) {
            return Err(Error::invalid_input(
                "a transaction is already open on this connection; commit or roll it back first",
            ));
        }
        match self.shared.run_to_end("BEGIN").await {
            Ok(()) => Ok(Box::new(PgTx {
                shared: self.shared.clone(),
                done: AtomicBool::new(false),
            })),
            Err(e) => {
                self.shared.tx_open.store(false, Ordering::Release);
                Err(e)
            }
        }
    }

    async fn close(&self) -> Result<()> {
        let client = lock(&self.shared.client).take();
        if client.is_none() {
            return Ok(());
        }
        self.shared.stop_all();
        // Dropping the last client handle makes the session send Terminate and end.
        drop(client);
        let handle = lock(&self.shared.conn_task).take();
        if let Some(mut handle) = handle
            && tokio::time::timeout(CLOSE_WAIT, &mut handle).await.is_err()
        {
            handle.abort();
        }
        Ok(())
    }

    /// M5: the bulk loader of this crate runs `COPY ... FROM STDIN` on the session
    /// ([`CopySession`]).
    fn as_any(&self) -> Option<&(dyn std::any::Any + Send + Sync)> {
        Some(self)
    }
}

/// An open transaction. Statements run on the same session as the connection's.
struct PgTx {
    shared: Arc<Shared>,
    done: AtomicBool,
}

impl PgTx {
    async fn finish(&self, sql: &str) -> Result<()> {
        self.done.store(true, Ordering::Release);
        let result = self.shared.run_to_end(sql).await;
        self.shared.tx_open.store(false, Ordering::Release);
        result
    }
}

#[async_trait]
impl Tx for PgTx {
    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        self.shared.execute(sql, params, cancel, Mode::Auto)
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        self.finish("COMMIT").await
    }

    async fn rollback(self: Box<Self>) -> Result<()> {
        self.finish("ROLLBACK").await
    }

    /// M5: the transaction's session, for `COPY ... FROM STDIN` ([`CopySession`]).
    fn as_any(&self) -> Option<&(dyn std::any::Any + Send + Sync)> {
        Some(self)
    }
}

impl Drop for PgTx {
    fn drop(&mut self) {
        if !self.done.load(Ordering::Acquire) {
            // Rolled back before the next statement on this connection.
            self.shared.rollback_pending.store(true, Ordering::Release);
            self.shared.tx_open.store(false, Ordering::Release);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// COPY FROM STDIN (M5 bulk load)

/// About how many bytes of `COPY` data go into one `CopyData` message.
pub(crate) const COPY_CHUNK: usize = 256 * 1024;
/// How long an aborted `COPY` may take to end by itself before cancel requests follow.
const COPY_ABORT_GRACE: Duration = Duration::from_millis(200);

/// The session behind a [`PgConnection`] or one of its transactions, for
/// `COPY ... FROM STDIN` (M5, `docs/m5-contracts.md` §4.3). The bulk loader of this crate
/// reaches it through `Connection::as_any` / `Tx::as_any`; nothing outside the crate can.
///
/// # How a `COPY` runs
///
/// - The `COPY` runs on its own task ([`CopyJob`]), like every statement of this driver, so
///   its cleanup also happens when the caller's future is dropped (an aborted import task)
///   instead of awaited to the end.
/// - It takes the connection's statement lock like any statement (so it queues behind a
///   running one and nothing interleaves with it), after a pending rollback of a dropped `Tx`.
///   An interruption before that rollback is sent leaves it pending for the next statement;
///   one while it runs lets it finish and starts no `COPY`.
/// - Rows travel as `CopyData` messages of about [`COPY_CHUNK`] bytes, produced on demand by the
///   caller's `fill`, so only one chunk is held in memory. `tokio-postgres` applies the
///   socket's backpressure to every send.
/// - On the caller's token, [`Connection::cancel_running`], or the caller dropping the
///   `copy_in` future, the copy is abandoned: `copy_in` returns `Err(Cancelled)` at once, and
///   the task drops the sink, which makes `tokio-postgres` send `CopyFail`, so the server
///   aborts the `COPY` (the transaction is then failed and must be rolled back). The task keeps
///   the statement lock until the server has ended the `COPY` and sends cancel requests (with
///   the statement path's backoff) if that takes longer than [`COPY_ABORT_GRACE`] (a `COPY`
///   waiting for a lock reads no more input, so `CopyFail` alone would not reach it). The next
///   statement on the connection (the caller's `ROLLBACK`) runs after that.
/// - Notices raised during the `COPY` (triggers) are discarded: no result stream carries them.
#[derive(Clone)]
pub(crate) struct CopySession(Arc<Shared>);

impl CopySession {
    /// The session of `any` when it is a [`PgConnection`] or a transaction of one.
    pub(crate) fn from_any(any: &(dyn std::any::Any + Send + Sync)) -> Option<Self> {
        if let Some(conn) = any.downcast_ref::<PgConnection>() {
            return Some(Self(conn.shared.clone()));
        }
        any.downcast_ref::<PgTx>().map(|tx| Self(tx.shared.clone()))
    }

    /// Runs `sql` (a `COPY ... FROM STDIN` in text format) and feeds it the data `fill`
    /// appends: it is called repeatedly with an empty buffer and returns `true` while more data
    /// follows. Returns the row count the server reports. See the type docs for cancellation.
    pub(crate) async fn copy_in<F>(
        &self,
        sql: &str,
        fill: F,
        cancel: &CancellationToken,
    ) -> Result<u64>
    where
        F: FnMut(&mut String) -> Result<bool> + Send + 'static,
    {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let shared = self.0.clone();
        let client = shared.client()?;
        let stop = lock(&shared.stop_all).child_token();
        let (out, done) = tokio::sync::oneshot::channel();
        let job = CopyJob {
            shared,
            client,
            sql: sql.to_owned(),
            cancel: cancel.clone(),
            stop: stop.clone(),
        };
        tokio::spawn(job.run(fill, out));
        tokio::select! {
            biased;
            result = done => result.unwrap_or_else(|_| {
                Err(Error::internal("the PostgreSQL COPY task ended without a result"))
            }),
            () = cancel.cancelled() => Err(Error::Cancelled),
            () = stop.cancelled() => Err(Error::Cancelled),
        }
    }
}

/// One `COPY ... FROM STDIN` on its own task (see [`CopySession`]).
struct CopyJob {
    shared: Arc<Shared>,
    client: Arc<Client>,
    sql: String,
    /// The caller's token.
    cancel: CancellationToken,
    /// Child of `Shared::stop_all`.
    stop: CancellationToken,
}

impl CopyJob {
    fn interrupted(&self, out: &tokio::sync::oneshot::Sender<Result<u64>>) -> bool {
        self.cancel.is_cancelled() || self.stop.is_cancelled() || out.is_closed()
    }

    async fn run<F>(self, fill: F, mut out: tokio::sync::oneshot::Sender<Result<u64>>)
    where
        F: FnMut(&mut String) -> Result<bool> + Send + 'static,
    {
        // Wait for the previous statement on this connection to end. Nothing has been sent for
        // the `COPY` yet, so an interruption here needs no cancel request.
        let mut notices = tokio::select! {
            biased;
            () = self.cancel.cancelled() => {
                let _ = out.send(Err(Error::Cancelled));
                return;
            }
            () = self.stop.cancelled() => {
                let _ = out.send(Err(Error::Cancelled));
                return;
            }
            () = out.closed() => return,
            guard = self.shared.exec.clone().lock_owned() => guard,
        };
        if self.shared.rollback_pending.swap(false, Ordering::AcqRel) {
            if self.interrupted(&out) {
                // Not sent: the next statement rolls back instead.
                self.shared.rollback_pending.store(true, Ordering::Release);
                let _ = out.send(Err(Error::Cancelled));
                return;
            }
            // Short and never blocked by locks; the `COPY` must not start before it ends.
            if let Err(e) = self.client.batch_execute("ROLLBACK").await {
                tracing::debug!(error = %from_pg(&e), "rollback of a dropped transaction failed");
            }
        }
        if self.interrupted(&out) {
            let _ = out.send(Err(Error::Cancelled));
            return;
        }
        let mut work = Box::pin(copy_rows(&self.client, &self.sql, fill));
        let outcome = tokio::select! {
            biased;
            result = &mut work => Some(result),
            () = self.cancel.cancelled() => None,
            () = self.stop.cancelled() => None,
            () = out.closed() => None,
        };
        if let Some(result) = outcome {
            while notices.try_recv().is_some() {}
            let _ = out.send(result);
            return;
        }
        let _ = out.send(Err(Error::Cancelled));
        // Dropping the sink (inside `work`) sends `CopyFail`.
        drop(work);
        let mut ended = Box::pin(async {
            // Answered after the server has processed the end of the `COPY`.
            let _ = self.client.batch_execute("").await;
        });
        if tokio::time::timeout(COPY_ABORT_GRACE, ended.as_mut())
            .await
            .is_err()
            && tokio::time::timeout(DRAIN_LIMIT, cancel_until_done(&self.shared, ended.as_mut()))
                .await
                .is_err()
        {
            tracing::warn!("a cancelled PostgreSQL COPY did not end within {DRAIN_LIMIT:?}");
        }
        while notices.try_recv().is_some() {}
        // `notices` (the statement lock) is dropped here.
    }
}

/// The `COPY` itself: start it, stream the chunks `fill` produces, finish.
async fn copy_rows<F>(client: &Client, sql: &str, mut fill: F) -> Result<u64>
where
    F: FnMut(&mut String) -> Result<bool> + Send,
{
    use futures::SinkExt as _;

    let sink = client
        .copy_in::<_, bytes::Bytes>(sql)
        .await
        .map_err(|e| from_pg(&e))?;
    let mut sink = std::pin::pin!(sink);
    loop {
        let mut chunk = String::with_capacity(COPY_CHUNK + COPY_CHUNK / 8);
        let more = fill(&mut chunk)?;
        if !chunk.is_empty() {
            sink.send(bytes::Bytes::from(chunk))
                .await
                .map_err(|e| from_pg(&e))?;
        }
        if !more {
            break;
        }
    }
    sink.as_mut().finish().await.map_err(|e| from_pg(&e))
}
