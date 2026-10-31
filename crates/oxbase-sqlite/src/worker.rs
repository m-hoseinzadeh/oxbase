//! The per-connection worker thread.
//!
//! A `rusqlite::Connection` is used from exactly one dedicated OS thread, so async callers never
//! block the runtime. The async side sends [`Command`]s over a std channel; statement results come
//! back over a bounded tokio channel, so a slow consumer applies backpressure to the worker
//! instead of letting rows pile up in memory.
//!
//! # Cancellation
//!
//! Each execution gets its own token (a child of the caller's token). It is cancelled by the
//! caller's token, by `cancel_running`, or by dropping the result stream. Two mechanisms stop
//! SQLite once it is cancelled:
//!
//! - `sqlite3_interrupt` through rusqlite's [`InterruptHandle`], called by whoever cancels. It is
//!   only called while the targeted execution is marked running (under a lock the worker also
//!   takes when it starts and finishes an execution), so it cannot hit a later one. SQLite
//!   itself clears a stale interrupt flag when the next statement starts.
//! - A progress handler that checks the execution token every [`PROGRESS_OPS`] virtual-machine
//!   instructions. It closes the window an interrupt alone leaves open: an interrupt that lands
//!   between two statements of a script is cleared when the next statement starts.
//!
//! The worker also checks the token before each statement and each row. An interrupted
//! statement surfaces as `SQLITE_INTERRUPT`, reported as `Error::Cancelled`.
//!
//! Two waits are outside the virtual machine, so neither mechanism above reaches them:
//!
//! - **Waiting for a lock** held by another connection. SQLite's built-in busy-timeout handler
//!   sleeps without looking at the interrupt flag, so the worker installs its own handler (see
//!   [`busy_handler`]): it sleeps in steps of [`BUSY_POLL`] and gives up as soon as the running
//!   execution is cancelled, or once the configured busy timeout has passed.
//! - **Waiting for room in the result channel** (a consumer that holds the stream but stops
//!   reading). Every send waits for either room or the execution's token (see [`deliver`]), so a
//!   cancel, `cancel_running` or `close` frees the worker even while nobody polls.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use arrow_schema::SchemaRef;
use oxbase_driver::{CancellationToken, Notice, ResultItem};
use oxbase_types::{Error, Result, ServerVersion, Value};
use rusqlite::fallible_iterator::FallibleIterator as _;
use rusqlite::{Batch, InterruptHandle, OpenFlags, Statement};
use tokio::sync::{mpsc, oneshot};

use crate::convert::{self, Cell, ColType};
use crate::error::map_err;
use crate::options::SqliteOptions;

/// Virtual-machine instructions between two progress-handler checks of the cancel token.
const PROGRESS_OPS: i32 = 1000;
/// Rows in the first batch of a result set. Its values also decide the column types.
const FIRST_BATCH_ROWS: usize = 2048;
/// Rows in later batches.
const BATCH_ROWS: usize = 8192;
/// A batch is sent early once its values reach about this many bytes.
const BATCH_BYTES: usize = 8 * 1024 * 1024;
/// A batch is sent early once it has been filling for this long, so slow queries show rows as
/// they arrive.
const FLUSH_AFTER: Duration = Duration::from_millis(250);
/// The first batch (whose values decide the column types) is sent early by [`FLUSH_AFTER`] only
/// once it holds at least this many rows...
const MIN_SAMPLE_ROWS: usize = 256;
/// ...or once it has been filling for this long (a query that yields rows slowly).
const SLOW_FIRST_FLUSH: Duration = Duration::from_secs(2);
/// Sleep between two attempts to take a lock held by another connection.
const BUSY_POLL: Duration = Duration::from_millis(10);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The execution running now (its id and token), if any. Shared by [`RunState`] and the busy
/// handler.
type Running = Arc<Mutex<Option<(u64, CancellationToken)>>>;

/// State shared by the worker and the async side: which execution is running, and the handle
/// that interrupts it.
pub(crate) struct RunState {
    interrupt: InterruptHandle,
    running: Running,
    /// Set by `close`: executions still queued fail instead of running.
    closing: AtomicBool,
}

impl RunState {
    /// Cancels the execution that is running now, if any.
    pub(crate) fn cancel_running(&self) {
        let running = lock(&self.running);
        if let Some((_, token)) = running.as_ref() {
            token.cancel();
            self.interrupt.interrupt();
        }
    }

    /// Interrupts SQLite if execution `id` is the one running now.
    pub(crate) fn interrupt_if_running(&self, id: u64) {
        let running = lock(&self.running);
        if running.as_ref().is_some_and(|(r, _)| *r == id) {
            self.interrupt.interrupt();
        }
    }

    pub(crate) fn set_closing(&self) {
        self.closing.store(true, Ordering::Release);
    }

    fn start(&self, id: u64, token: CancellationToken) {
        *lock(&self.running) = Some((id, token));
    }

    fn finish(&self) {
        *lock(&self.running) = None;
    }
}

/// One `execute` call.
pub(crate) struct Job {
    pub id: u64,
    pub sql: String,
    pub params: Vec<Value>,
    /// The transaction this runs in, when it came through a `Tx`.
    pub tx: Option<u64>,
    pub token: CancellationToken,
    pub out: mpsc::Sender<Result<ResultItem>>,
    /// Cancelled by the worker when the execution is over (stops the cancel watcher).
    pub done: CancellationToken,
}

pub(crate) enum Command {
    Execute(Job),
    Begin {
        tx: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    End {
        tx: u64,
        commit: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    /// A `Tx` was dropped without commit or rollback: roll it back if it is still open.
    DropTx {
        tx: u64,
    },
    Close {
        reply: oneshot::Sender<()>,
    },
}

/// What [`spawn`] hands back.
pub(crate) struct Opened {
    pub version: ServerVersion,
    pub state: Arc<RunState>,
    pub commands: std_mpsc::Sender<Command>,
}

/// Opens the database on a new worker thread.
pub(crate) async fn spawn(opts: SqliteOptions) -> Result<Opened> {
    let (cmd_tx, cmd_rx) = std_mpsc::channel::<Command>();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(ServerVersion, Arc<RunState>)>>();
    std::thread::Builder::new()
        .name("oxbase-sqlite".into())
        .spawn(move || {
            let running: Running = Arc::new(Mutex::new(None));
            let (conn, version) = match open(&opts, &running) {
                Ok(v) => v,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let state = Arc::new(RunState {
                interrupt: conn.get_interrupt_handle(),
                running,
                closing: AtomicBool::new(false),
            });
            if ready_tx.send(Ok((version, state.clone()))).is_err() {
                return; // the connect call went away
            }
            run(conn, &state, &cmd_rx);
        })
        .map_err(|e| Error::internal(format!("cannot start the SQLite worker thread: {e}")))?;
    let (version, state) = ready_rx
        .await
        .map_err(|_| Error::internal("the SQLite worker thread exited while opening"))??;
    Ok(Opened {
        version,
        state,
        commands: cmd_tx,
    })
}

fn open(opts: &SqliteOptions, running: &Running) -> Result<(rusqlite::Connection, ServerVersion)> {
    let mut flags = OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if opts.read_only {
        flags |= OpenFlags::SQLITE_OPEN_READ_ONLY;
    } else {
        flags |= OpenFlags::SQLITE_OPEN_READ_WRITE;
        if opts.create_if_missing {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
    }
    let fail = |e: rusqlite::Error| open_error(opts, e);
    let mut conn = rusqlite::Connection::open_with_flags(&opts.path, flags).map_err(fail)?;
    conn.busy_handler(Some(busy_handler(running.clone(), opts.busy_timeout)))
        .map_err(fail)?;
    // Reads the file header, so a file that is not a database fails here, not at first query.
    conn.query_row("PRAGMA schema_version", [], |_| Ok(()))
        .map_err(fail)?;
    let text: String = conn
        .query_row("SELECT sqlite_version()", [], |r| r.get(0))
        .map_err(fail)?;
    let version = ServerVersion::parse(&text)?;
    Ok((conn, version))
}

/// The busy handler: called by SQLite when a lock it needs is held by another connection, with
/// the number of earlier calls for the same lock wait. Returning `true` retries the lock;
/// `false` fails the statement with `SQLITE_BUSY` (reported as `Cancelled` when the execution
/// was cancelled).
///
/// It sleeps in [`BUSY_POLL`] steps and stops as soon as the running execution's token is
/// cancelled, or once `timeout` has passed since the wait began. (`PRAGMA busy_timeout` run as
/// SQL replaces this handler with SQLite's own, which a cancel does not stop.)
fn busy_handler(running: Running, timeout: Duration) -> impl FnMut(i32) -> bool + Send + 'static {
    let mut since = Instant::now();
    move |attempts| {
        if attempts == 0 {
            since = Instant::now();
        }
        let cancelled = || {
            lock(&running)
                .as_ref()
                .is_some_and(|(_, token)| token.is_cancelled())
        };
        if cancelled() {
            return false;
        }
        let left = timeout.saturating_sub(since.elapsed());
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(left.min(BUSY_POLL));
        !cancelled()
    }
}

fn open_error(opts: &SqliteOptions, e: rusqlite::Error) -> Error {
    let missing = e.sqlite_error_code() == Some(rusqlite::ErrorCode::CannotOpen)
        && !opts.is_memory()
        && !opts.path.to_string_lossy().starts_with("file:")
        && !opts.path.exists();
    let reason = match map_err(e) {
        Error::Sql { message, .. } => message,
        other => other.to_string(),
    };
    let path = opts.display_path();
    if missing {
        Error::not_found(format!("SQLite database file '{path}' does not exist"))
    } else {
        Error::connection(format!("cannot open SQLite database '{path}': {reason}"))
    }
}

/// The worker loop. Returns when `close` is called or every command sender is gone; dropping
/// the connection then rolls back any open transaction and closes the file.
fn run(mut conn: rusqlite::Connection, state: &RunState, commands: &std_mpsc::Receiver<Command>) {
    let mut active_tx: Option<u64> = None;
    let mut close_reply = None;
    while let Ok(cmd) = commands.recv() {
        match cmd {
            Command::Execute(job) => {
                let out = job.out.clone();
                let ran = catch_unwind(AssertUnwindSafe(|| {
                    run_job(&mut conn, state, &mut active_tx, job);
                }));
                if ran.is_err() {
                    state.finish();
                    // Never blocks: if the channel is full, the stream still ends with an error
                    // because the job never reported that it finished.
                    let _ = out.try_send(Err(Error::internal(
                        "the SQLite worker panicked; the connection is closed",
                    )));
                    break;
                }
            }
            Command::Begin { tx, reply } => {
                let r = conn.execute_batch("BEGIN").map_err(map_err);
                if r.is_ok() {
                    active_tx = Some(tx);
                }
                let _ = reply.send(r);
            }
            Command::End { tx, commit, reply } => {
                let _ = reply.send(end_tx(&conn, &mut active_tx, tx, commit));
            }
            Command::DropTx { tx } => {
                if active_tx == Some(tx) {
                    if !conn.is_autocommit() {
                        let _ = conn.execute_batch("ROLLBACK");
                    }
                    active_tx = None;
                }
            }
            Command::Close { reply } => {
                close_reply = Some(reply);
                break;
            }
        }
    }
    // `close` reports errors (busy statements); dropping is the fallback and never fails.
    if let Err((conn, _)) = conn.close() {
        drop(conn);
    }
    if let Some(reply) = close_reply {
        let _ = reply.send(());
    }
}

fn tx_gone() -> Error {
    Error::invalid_input(
        "the transaction is no longer active: a statement ended it, or SQLite rolled it back \
         (for example after a cancelled write)",
    )
}

fn end_tx(
    conn: &rusqlite::Connection,
    active_tx: &mut Option<u64>,
    tx: u64,
    commit: bool,
) -> Result<()> {
    if *active_tx != Some(tx) {
        return Err(tx_gone());
    }
    if conn.is_autocommit() {
        *active_tx = None;
        // Nothing left to roll back; a commit must not pretend it saved anything.
        return if commit { Err(tx_gone()) } else { Ok(()) };
    }
    conn.execute_batch(if commit { "COMMIT" } else { "ROLLBACK" })
        .map_err(map_err)?;
    *active_tx = None;
    Ok(())
}

/// Why a script stopped early.
enum Halt {
    Fail(Error),
    /// The result stream was dropped.
    Gone,
}

impl From<rusqlite::Error> for Halt {
    fn from(e: rusqlite::Error) -> Self {
        Self::Fail(map_err(e))
    }
}

impl From<Error> for Halt {
    fn from(e: Error) -> Self {
        Self::Fail(e)
    }
}

/// Sends one item, waiting for room in the channel or for the execution to be cancelled,
/// whichever comes first (the worker is a plain thread, so it blocks on a small executor).
/// Without the cancel branch, a consumer that holds the stream but stops reading would park the
/// worker for good: cancels, `cancel_running` and `close` could not reach it.
fn deliver(
    out: &mpsc::Sender<Result<ResultItem>>,
    token: &CancellationToken,
    item: Result<ResultItem>,
) -> Result<(), Halt> {
    futures::executor::block_on(async {
        tokio::select! {
            biased;
            () = token.cancelled() => Err(Halt::Fail(Error::Cancelled)),
            permit = out.reserve() => permit.map(|p| p.send(item)).map_err(|_| Halt::Gone),
        }
    })
}

fn send(
    out: &mpsc::Sender<Result<ResultItem>>,
    token: &CancellationToken,
    item: ResultItem,
) -> Result<(), Halt> {
    deliver(out, token, Ok(item))
}

fn run_job(
    conn: &mut rusqlite::Connection,
    state: &RunState,
    active_tx: &mut Option<u64>,
    job: Job,
) {
    let Job {
        id,
        sql,
        params,
        tx,
        token,
        out,
        done,
    } = job;
    let early = if state.closing.load(Ordering::Acquire) {
        Some(Error::connection("the SQLite connection is closed"))
    } else if token.is_cancelled() {
        Some(Error::Cancelled)
    } else if tx.is_some() && (*active_tx != tx || conn.is_autocommit()) {
        Some(tx_gone())
    } else {
        None
    };
    if let Some(e) = early {
        // The channel is new and empty: this never waits.
        let _ = out.try_send(Err(e));
        done.cancel();
        return;
    }

    state.start(id, token.clone());
    if state.closing.load(Ordering::Acquire) {
        // `close` ran between the check above and `start`, so its `cancel_running` missed us.
        token.cancel();
    }
    let watch = token.clone();
    let handler = conn.progress_handler(PROGRESS_OPS, Some(move || watch.is_cancelled()));
    let result = match handler {
        Ok(()) => run_script(conn, &sql, &params, &token, &out),
        Err(e) => Err(Halt::from(e)),
    };
    let _ = conn.progress_handler(0, None::<fn() -> bool>);
    if let Err(Halt::Fail(e)) = result {
        // Still marked running, so `cancel_running` and `close` can free a send that waits.
        if token.is_cancelled() {
            // The stream reports the cancel by itself; this is a best effort.
            let _ = out.try_send(Err(Error::Cancelled));
        } else {
            let _ = deliver(&out, &token, Err(e));
        }
    }
    state.finish();
    if conn.is_autocommit() {
        // A transaction ended by a statement (or rolled back by SQLite) is over.
        *active_tx = None;
    }
    // Before `out` drops: the stream treats an end without this as a worker that died.
    done.cancel();
    // `out` drops here, which ends the stream.
}

/// Runs every statement of `sql` in order, using SQLite's own parser to find statement
/// boundaries (the tail returned by each prepare), so `;` inside literals, identifiers, comments
/// and trigger bodies is handled correctly.
///
/// Parameters: every statement binds its parameters by index from the same `params` list
/// (statement parameter `?N` / `:name` number N takes `params[N-1]`). When parameters are given
/// and every statement can be prepared up front, a count that does not match what the statements
/// use fails before anything runs. Otherwise (a statement depends on DDL earlier in the script,
/// so it cannot be prepared yet) the count is checked as the script runs: a statement that needs
/// more parameters than given fails before it runs, but parameters no statement used are only
/// reported after the script, whose statements have then run (and, outside a transaction,
/// committed).
fn run_script(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[Value],
    token: &CancellationToken,
    out: &mpsc::Sender<Result<ResultItem>>,
) -> Result<(), Halt> {
    if !params.is_empty()
        && let Some(used) = params_used(conn, sql)
        && used != params.len()
    {
        return Err(Halt::Fail(Error::invalid_input(format!(
            "the statements use {used} parameter(s) but {} were given; nothing was run",
            params.len()
        ))));
    }
    let mut batch = Batch::new(conn, sql);
    let mut max_used = 0;
    loop {
        if token.is_cancelled() {
            return Err(Halt::Fail(Error::Cancelled));
        }
        let Some(mut stmt) = batch.next()? else {
            break;
        };
        let needed = stmt.parameter_count();
        if needed > params.len() {
            return Err(Halt::Fail(Error::invalid_input(format!(
                "a statement needs {needed} parameter(s) but {} were given",
                params.len()
            ))));
        }
        for (i, v) in params[..needed].iter().enumerate() {
            stmt.raw_bind_parameter(i + 1, convert::to_sql(v))?;
        }
        max_used = max_used.max(needed);
        if stmt.column_count() == 0 {
            let before = conn.total_changes();
            stmt.raw_execute()?;
            // `changes()` keeps the count of the last INSERT/UPDATE/DELETE; DDL does not reset it.
            let n = if conn.total_changes() == before {
                0
            } else {
                conn.changes()
            };
            send(out, token, ResultItem::RowsAffected(n))?;
        } else {
            stream_rows(&mut stmt, token, out)?;
        }
    }
    if max_used < params.len() {
        return Err(Halt::Fail(Error::invalid_input(format!(
            "{} parameter(s) were given but the statements use only {max_used} (the statements \
             have run)",
            params.len()
        ))));
    }
    Ok(())
}

/// The number of parameters the script's statements use (the largest count of any statement),
/// or `None` when a statement cannot be prepared before the earlier ones run.
fn params_used(conn: &rusqlite::Connection, sql: &str) -> Option<usize> {
    let mut batch = Batch::new(conn, sql);
    let mut used = 0;
    loop {
        match batch.next() {
            Ok(Some(stmt)) => used = used.max(stmt.parameter_count()),
            Ok(None) => return Some(used),
            Err(_) => return None,
        }
    }
}

/// Rows of one batch being filled, column-major.
struct Pending {
    columns: Vec<Vec<Cell>>,
    rows: usize,
    bytes: usize,
}

impl Pending {
    fn new(ncols: usize, rows: usize) -> Self {
        Self {
            columns: (0..ncols).map(|_| Vec::with_capacity(rows)).collect(),
            rows: 0,
            bytes: 0,
        }
    }
}

/// The result set being streamed.
struct ResultSetState<'a> {
    names: Vec<String>,
    decls: Vec<Option<String>>,
    /// Set once the schema went out.
    sent: Option<(SchemaRef, Vec<ColType>)>,
    mismatches: Vec<u64>,
    out: &'a mpsc::Sender<Result<ResultItem>>,
    token: &'a CancellationToken,
}

impl ResultSetState<'_> {
    /// Sends the schema (first call only; types inferred from `pending`) and the pending rows.
    fn flush(&mut self, pending: &mut Pending) -> Result<(), Halt> {
        if self.sent.is_none() {
            let types: Vec<ColType> = self
                .decls
                .iter()
                .zip(&pending.columns)
                .map(|(decl, cells)| convert::infer(decl.as_deref(), cells))
                .collect();
            let schema = convert::schema(&self.names, &self.decls, &types);
            send(self.out, self.token, ResultItem::Schema(schema.clone()))?;
            self.sent = Some((schema, types));
        }
        let Some((schema, types)) = &self.sent else {
            return Ok(());
        };
        if pending.rows == 0 {
            return Ok(());
        }
        let ncols = pending.columns.len();
        let full = std::mem::replace(pending, Pending::new(ncols, BATCH_ROWS));
        let batch = convert::build_batch(schema, types, full.columns, &mut self.mismatches)?;
        send(self.out, self.token, ResultItem::Batch(batch))
    }
}

fn stream_rows(
    stmt: &mut Statement<'_>,
    token: &CancellationToken,
    out: &mpsc::Sender<Result<ResultItem>>,
) -> Result<(), Halt> {
    let (names, decls): (Vec<String>, Vec<Option<String>>) = stmt
        .columns()
        .map(|c| (c.name().to_owned(), c.decl_type().map(str::to_owned)))
        .unzip();
    let ncols = names.len();
    let mut set = ResultSetState {
        mismatches: vec![0; ncols],
        names,
        decls,
        sent: None,
        out,
        token,
    };
    let mut pending = Pending::new(ncols, FIRST_BATCH_ROWS);
    // When the pending batch got its first row (not when the query started: a query that is slow
    // to produce its first row must still sample more than one row for the column types).
    let mut started = Instant::now();
    let mut rows = stmt.raw_query();
    loop {
        if token.is_cancelled() {
            return Err(Halt::Fail(Error::Cancelled));
        }
        let Some(row) = rows.next()? else {
            break;
        };
        for (i, column) in pending.columns.iter_mut().enumerate() {
            let cell = Cell::from_ref(row.get_ref(i)?);
            pending.bytes += cell.size();
            column.push(cell);
        }
        pending.rows += 1;
        if pending.rows == 1 {
            started = Instant::now();
        }
        let (limit, wait) = if set.sent.is_some() {
            (BATCH_ROWS, FLUSH_AFTER)
        } else if pending.rows < MIN_SAMPLE_ROWS {
            (FIRST_BATCH_ROWS, SLOW_FIRST_FLUSH)
        } else {
            (FIRST_BATCH_ROWS, FLUSH_AFTER)
        };
        if pending.rows >= limit || pending.bytes >= BATCH_BYTES || started.elapsed() >= wait {
            set.flush(&mut pending)?;
        }
    }
    drop(rows);
    set.flush(&mut pending)?;
    if let Some((schema, types)) = &set.sent {
        for ((field, ty), bad) in schema.fields().iter().zip(types).zip(&set.mismatches) {
            if *bad > 0 {
                send(
                    out,
                    token,
                    ResultItem::Notice(Notice::warning(format!(
                        "column \"{}\": {bad} value(s) did not fit the type {} chosen from the \
                         first rows and were returned as NULL",
                        field.name(),
                        ty.arrow()
                    ))),
                )?;
            }
        }
    }
    Ok(())
}
