//! Query runs (M2): split a script, run its statements in order on an editor's session, stream
//! events, keep every result set pageable.
//!
//! # Sessions
//!
//! A run targets a base session and an **editor** ([`RunTarget`]). Each editor has its own child
//! session (`ConnectionManager::open_child`), opened on first use and kept until
//! [`QueryService::close_editor`], so transactions and session settings survive between runs of
//! one editor and a running query never blocks the object tree. When [`RunOptions::database`]
//! differs from the database the editor's child was opened on, the child is closed and a new
//! one is opened on that database (on [`DatabaseScope::Attached`](oxbase_dialect::DatabaseScope)
//! engines the option is ignored: every attached database is visible). One run at a time per
//! editor: starting a second one while the first is running is `InvalidInput`. Unless
//! [`RunOptions::keep_previous`], starting a run closes the editor's earlier (finished) runs.
//!
//! # Execution
//!
//! 1. Split the script with `oxbase_sql::split` (dialect from the session's engine); emit
//!    [`RunEvent::Started`] with every statement's [`StatementSpan`]. An empty script finishes
//!    right away.
//! 2. For each statement in order: [`RunEvent::StatementStarted`], then
//!    `Connection::execute(statement.text, &[], token)` under `SessionHandle::exclusive`, and
//!    for every item of the result stream:
//!    - `Schema` → new [`ResultStore`](crate::results::ResultStore), [`RunEvent::ResultSet`];
//!    - `Batch` → `ResultStore::append`; [`RunEvent::Rows`] at most every
//!      [`QueryConfig::rows_event_interval`] per result (lossy, the total is cumulative);
//!    - `RowsAffected` → summed into [`RunEvent::StatementDone`]'s `rows_affected`;
//!    - `Notice` → [`RunEvent::Notice`].
//!
//!    A result set ends with [`RunEvent::ResultDone`] (exact row count, `truncated`). When a
//!    result reaches [`RunOptions::max_rows`] or the disk cap, the run drops the statement's
//!    stream (the driver cancels it), marks the result truncated, emits a warning
//!    [`RunEvent::Notice`] and goes on with the next statement.
//! 3. A failing statement emits [`RunEvent::StatementFailed`]; with
//!    [`RunOptions::stop_on_error`] (the default) the run then ends with [`RunEvent::Failed`],
//!    otherwise it continues. A run that loses its session ends with `Failed`.
//! 4. The last event is exactly one of [`RunEvent::Finished`], [`RunEvent::Failed`],
//!    [`RunEvent::Cancelled`].
//!
//! History: one `HistoryService::record` per statement that started (status ok, error or
//! cancelled), unless [`RunOptions::record_history`] is off.
//!
//! # Events and backpressure
//!
//! Events go through a bounded channel ([`QueryConfig::event_capacity`]). `Rows` events are
//! sent with `try_send` and dropped when the channel is full (the next one carries the new
//! total). Every other event is awaited, so the UI never misses a structural event; while the
//! consumer lags, the run waits *between* items, which also bounds memory. A dropped receiver
//! does not stop the run: results stay pageable until closed.
//!
//! # Cancel, close, cleanup
//!
//! - [`QueryService::cancel`] cancels the run's token: the driver stops the running statement
//!   (server-side cancel), and the run ends with `Cancelled` **within 2 s**. Rows received so
//!   far stay pageable; their results are marked truncated.
//! - [`QueryService::close`] cancels if needed, drops the run's stores and deletes its spill
//!   directory. [`QueryService::close_editor`] closes the editor's runs and its child session.
//!   Dropping the service deletes every run directory.
//! - On start, the service removes `run-*` directories older than 24 h under
//!   [`QueryConfig::spill_dir`] (left behind by a crash).
//!
//! Owner of the bodies: the **services** stream (`docs/m2-contracts.md`).

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use arrow_schema::{Field, Schema};
use chrono::Utc;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use oxbase_dialect::{DatabaseScope, DialectRegistry};
use oxbase_driver::{Notice, NoticeSeverity, ResultItem, ResultStream};
use oxbase_sql::{SqlDialect, StatementKind};
use oxbase_store::HistoryStatus;
use oxbase_types::{Engine, Error, ErrorCategory, Result};

use crate::history::{HistoryRecord, HistoryService};
use crate::results::{AppendOutcome, ByteBudget, ResultStore, RunDir};
use crate::{CancellationToken, ConnectionManager, SessionHandle, SessionId};

#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod service_tests;
#[cfg(test)]
mod test_split;

/// How long a stopped statement's stream may take to wind down (server-side cancel, drain)
/// before it is dropped. Keeps "cancel ends the run within 2 s".
const STOP_GRACE: Duration = Duration::from_millis(1500);
/// How long `close` waits for a cancelled run to end before forgetting it anyway.
const CLOSE_WAIT: Duration = Duration::from_secs(3);
/// Spill directories older than this are left over from a crash and removed on start.
const STALE_RUN_DIR_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Identifies one run inside a [`QueryService`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub Uuid);

impl RunId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Identifies one query editor (a UI tab). Chosen by the UI; any non-empty string up to 200
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EditorId(pub String);

impl EditorId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `InvalidInput` unless the id is 1 to 200 bytes long.
    pub fn validate(&self) -> Result<()> {
        if self.0.is_empty() || self.0.len() > 200 {
            return Err(Error::invalid_input(
                "editor id must be 1 to 200 bytes long",
            ));
        }
        Ok(())
    }
}

impl fmt::Display for EditorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a run executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTarget {
    /// The base session (`ConnectionManager::open`).
    pub session: SessionId,
    pub editor: EditorId,
}

/// Per-run options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RunOptions {
    /// Run in this database (see the module docs); `None` keeps the editor's current one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Stop reading a result set after this many rows (it is then marked truncated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<u64>,
    /// End the run at the first failing statement (default `true`).
    pub stop_on_error: bool,
    /// Record executed statements in the history (default `true`; the history's own
    /// `enabled` setting still applies).
    pub record_history: bool,
    /// Keep the editor's earlier runs open instead of closing them (default `false`).
    pub keep_previous: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            database: None,
            max_rows: None,
            stop_on_error: true,
            record_history: true,
            keep_previous: false,
        }
    }
}

/// A statement of the script, as the UI needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatementSpan {
    /// Index in the script (0-based).
    pub index: usize,
    /// Byte range of the statement text in the script.
    pub start_byte: usize,
    pub end_byte: usize,
    /// Editor positions (1-based, UTF-16 columns; end exclusive), see `oxbase_sql::Position`.
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    /// `true` when the statement probably returns rows (hint).
    pub returns_rows: bool,
}

impl StatementSpan {
    /// The span of `statement`, the `index`-th of its script.
    pub fn new(index: usize, statement: &oxbase_sql::Statement) -> Self {
        Self {
            index,
            start_byte: statement.range.start,
            end_byte: statement.range.end,
            start_line: statement.start.line,
            start_column: statement.start.column,
            end_line: statement.end.line,
            end_column: statement.end.column,
            returns_rows: statement.kind == StatementKind::Query,
        }
    }
}

/// One column of a result set, for headers before the first page arrives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnInfo {
    pub name: String,
    /// The Arrow type (`Int64`, `Utf8`, `Timestamp(Microsecond, None)`, ...), as Arrow prints it.
    pub data_type: String,
    pub nullable: bool,
    /// Arrow field metadata from the driver (`oxbase.pg.type`, `ARROW:extension:name`, ...).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl ColumnInfo {
    pub fn from_field(field: &Field) -> Self {
        Self {
            name: field.name().clone(),
            data_type: field.data_type().to_string(),
            nullable: field.is_nullable(),
            metadata: field
                .metadata()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }

    /// Every column of `schema`, in order.
    pub fn from_schema(schema: &Schema) -> Vec<Self> {
        schema
            .fields()
            .iter()
            .map(|f| Self::from_field(f.as_ref()))
            .collect()
    }
}

/// An error as the UI shows it: `{ category, message }`, like the app's `CommandError`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorInfo {
    pub category: ErrorCategory,
    pub message: String,
}

impl From<&Error> for ErrorInfo {
    fn from(error: &Error) -> Self {
        let message = match error {
            Error::Connection { message }
            | Error::Auth { message }
            | Error::Timeout { message }
            | Error::Unsupported { message }
            | Error::NotFound { message }
            | Error::InvalidInput { message }
            | Error::Internal { message }
            | Error::Io { message, .. }
                if !message.trim().is_empty() =>
            {
                message.clone()
            }
            Error::Cancelled => "the operation was cancelled".to_owned(),
            _ => error.to_string(),
        };
        Self {
            category: error.category(),
            message,
        }
    }
}

/// Severity of a [`RunEvent::Notice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Debug,
    Info,
    Warning,
}

impl From<NoticeSeverity> for NoticeLevel {
    fn from(s: NoticeSeverity) -> Self {
        match s {
            NoticeSeverity::Debug => Self::Debug,
            NoticeSeverity::Info => Self::Info,
            NoticeSeverity::Warning => Self::Warning,
        }
    }
}

/// What a run reports, in order. See the module docs for the sequence.
///
/// JSON: `{"type": "<camelCase variant>", ...camelCase fields}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RunEvent {
    /// First event: the run's id and the statements the script was split into.
    Started {
        run_id: RunId,
        statements: Vec<StatementSpan>,
    },
    StatementStarted {
        statement_index: usize,
    },
    /// A result set began. `result_index` counts result sets across the whole run (0-based);
    /// it is what `page` takes.
    ResultSet {
        result_index: usize,
        statement_index: usize,
        statement_range: StatementSpan,
        columns: Vec<ColumnInfo>,
    },
    /// Rows stored so far for a result (cumulative; lossy, see the module docs).
    Rows {
        result_index: usize,
        total_so_far: u64,
    },
    /// A result set is complete: exact row count; `truncated` when rows were left unread.
    ResultDone {
        result_index: usize,
        total_rows: u64,
        truncated: bool,
    },
    Notice {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        statement_index: Option<usize>,
        level: NoticeLevel,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    /// A statement finished. `rows_affected` is the sum of the driver's `RowsAffected` items,
    /// `None` when it reported none.
    StatementDone {
        statement_index: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rows_affected: Option<u64>,
        elapsed_ms: u64,
    },
    StatementFailed {
        statement_index: usize,
        error: ErrorInfo,
        elapsed_ms: u64,
    },
    /// Terminal: every statement ran (some may have failed without `stop_on_error`).
    Finished {
        elapsed_ms: u64,
    },
    /// Terminal: the run stopped on an error.
    Failed {
        error: ErrorInfo,
        elapsed_ms: u64,
    },
    /// Terminal: the run was cancelled.
    Cancelled {
        elapsed_ms: u64,
    },
}

impl RunEvent {
    /// `true` for `Finished`, `Failed` and `Cancelled`.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Finished { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }

    /// A `Notice` event from a driver notice.
    pub fn notice(statement_index: Option<usize>, notice: &Notice) -> Self {
        Self::Notice {
            statement_index,
            level: notice.severity.into(),
            message: notice.message.clone(),
            code: notice.code.clone(),
        }
    }
}

/// A started run: its id and its event stream (see the module docs).
#[derive(Debug)]
pub struct RunHandle {
    pub id: RunId,
    pub events: mpsc::Receiver<RunEvent>,
}

/// Where a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Finished,
    Failed,
    Cancelled,
}

/// One result set of a run, for `info`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultInfo {
    pub result_index: usize,
    pub statement_index: usize,
    pub columns: Vec<ColumnInfo>,
    pub rows: u64,
    pub complete: bool,
    pub truncated: bool,
}

/// A snapshot of a run (for a UI that reattaches after a reload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunInfo {
    pub run_id: RunId,
    pub editor_id: EditorId,
    pub state: RunState,
    pub statements: Vec<StatementSpan>,
    pub results: Vec<ResultInfo>,
}

/// Tunables of a [`QueryService`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryConfig {
    /// Base directory for run spill directories (`run-*`).
    pub spill_dir: PathBuf,
    /// Batch memory per run before spilling.
    pub memory_budget_bytes: u64,
    /// Spill bytes across all runs of the service.
    pub disk_cap_bytes: u64,
    /// Capacity of each run's event channel.
    pub event_capacity: usize,
    /// Minimum time between two `Rows` events of one result.
    pub rows_event_interval: Duration,
    /// Largest `limit` a page request may ask for (larger requests are clamped).
    pub max_page_rows: usize,
}

impl QueryConfig {
    pub const DEFAULT_MEMORY_BUDGET: u64 = 64 * 1024 * 1024;
    pub const DEFAULT_DISK_CAP: u64 = 8 * 1024 * 1024 * 1024;
}

impl Default for QueryConfig {
    /// Spills into `<system temp dir>/oxbase-results`.
    fn default() -> Self {
        Self {
            spill_dir: std::env::temp_dir().join("oxbase-results"),
            memory_budget_bytes: Self::DEFAULT_MEMORY_BUDGET,
            disk_cap_bytes: Self::DEFAULT_DISK_CAP,
            event_capacity: 256,
            rows_event_interval: Duration::from_millis(100),
            max_page_rows: 50_000,
        }
    }
}

/// Runs scripts and serves their result pages. See the module docs.
pub struct QueryService {
    manager: Arc<ConnectionManager>,
    dialects: Arc<DialectRegistry>,
    history: Arc<HistoryService>,
    config: QueryConfig,
    disk: Arc<ByteBudget>,
    registry: Arc<Registry>,
}

/// Runs and editors of a service, shared with the run tasks.
#[derive(Default)]
struct Registry {
    runs: Mutex<HashMap<RunId, Arc<Run>>>,
    editors: Mutex<HashMap<EditorId, Editor>>,
}

impl Registry {
    fn run(&self, id: RunId) -> Option<Arc<Run>> {
        lock(&self.runs).get(&id).cloned()
    }

    /// Frees `editor` for its next run if `run` is the one holding it.
    fn release_editor(&self, editor: &EditorId, run: RunId) {
        if let Some(e) = lock(&self.editors).get_mut(editor)
            && e.busy == Some(run)
        {
            e.busy = None;
        }
    }
}

/// One query editor: its child session and its runs.
struct Editor {
    /// The base session the child belongs to.
    base: SessionId,
    child: Option<SessionHandle>,
    /// The database the child was opened on (`None`: the base's).
    database: Option<String>,
    /// The run that holds the editor (also while its session is being opened).
    busy: Option<RunId>,
    /// Open runs, oldest first.
    runs: Vec<RunId>,
}

/// One run.
struct Run {
    id: RunId,
    editor: EditorId,
    token: CancellationToken,
    state: watch::Sender<RunState>,
    statements: Mutex<Vec<StatementSpan>>,
    results: Mutex<Vec<RunResult>>,
    /// Taken by `close` (and the service's drop), which delete the directory right away.
    dir: Mutex<Option<Arc<RunDir>>>,
}

impl Run {
    fn state(&self) -> RunState {
        *self.state.borrow()
    }
}

struct RunResult {
    statement_index: usize,
    store: Arc<ResultStore>,
}

impl QueryService {
    pub fn new(
        manager: Arc<ConnectionManager>,
        dialects: Arc<DialectRegistry>,
        history: Arc<HistoryService>,
        config: QueryConfig,
    ) -> Self {
        let disk = Arc::new(ByteBudget::new(config.disk_cap_bytes));
        remove_stale_run_dirs(&config.spill_dir, STALE_RUN_DIR_AGE);
        Self {
            manager,
            dialects,
            history,
            config,
            disk,
            registry: Arc::new(Registry::default()),
        }
    }

    pub fn manager(&self) -> &Arc<ConnectionManager> {
        &self.manager
    }

    pub fn dialects(&self) -> &Arc<DialectRegistry> {
        &self.dialects
    }

    pub fn history(&self) -> &Arc<HistoryService> {
        &self.history
    }

    pub fn config(&self) -> &QueryConfig {
        &self.config
    }

    /// The spill disk budget shared by every run.
    pub fn disk_budget(&self) -> &Arc<ByteBudget> {
        &self.disk
    }

    /// Starts running `script` (see the module docs) and returns once the run is registered
    /// and its editor session is ready; events follow on the handle. Fails before starting
    /// with `NotFound` (unknown session), `InvalidInput` (bad editor id, editor busy) or the
    /// error of opening the editor's session.
    pub async fn run(
        &self,
        target: RunTarget,
        script: &str,
        options: RunOptions,
    ) -> Result<RunHandle> {
        target.editor.validate()?;
        let base = self
            .manager
            .get(target.session)
            .ok_or_else(|| Error::not_found(format!("no open session {}", target.session)))?;
        let engine = base.summary().engine;
        let dialect = SqlDialect::for_engine(engine).ok_or_else(|| {
            Error::unsupported(format!("{} has no SQL editor", engine.display_name()))
        })?;
        let run_id = RunId::new();

        // Reserve the editor.
        let (previous, earlier) = {
            let mut editors = lock(&self.registry.editors);
            let editor = editors
                .entry(target.editor.clone())
                .or_insert_with(|| Editor {
                    base: target.session,
                    child: None,
                    database: None,
                    busy: None,
                    runs: Vec::new(),
                });
            if editor.busy.is_some() {
                return Err(Error::invalid_input(
                    "this editor is already running a query; cancel it or wait for it to end",
                ));
            }
            editor.busy = Some(run_id);
            let earlier = if options.keep_previous {
                Vec::new()
            } else {
                std::mem::take(&mut editor.runs)
            };
            let previous = (editor.base, editor.child.clone(), editor.database.clone());
            (previous, earlier)
        };
        for id in earlier {
            if let Err(e) = self.close(id).await {
                tracing::debug!(run = %id, error = %e, "closing an earlier run failed");
            }
        }

        let prepared = async {
            let (session, database) = self
                .editor_session(&target, engine, previous, &options)
                .await?;
            let dir = Arc::new(RunDir::create(&self.config.spill_dir)?);
            Ok::<_, Error>((session, database, dir))
        }
        .await;
        let (session, database, dir) = match prepared {
            Ok(ready) => ready,
            Err(e) => {
                let mut editors = lock(&self.registry.editors);
                if let Some(editor) = editors.get_mut(&target.editor)
                    && editor.busy == Some(run_id)
                {
                    editor.busy = None;
                }
                return Err(e);
            }
        };

        let run = Arc::new(Run {
            id: run_id,
            editor: target.editor.clone(),
            token: CancellationToken::new(),
            state: watch::Sender::new(RunState::Running),
            statements: Mutex::new(Vec::new()),
            results: Mutex::new(Vec::new()),
            dir: Mutex::new(Some(Arc::clone(&dir))),
        });
        lock(&self.registry.runs).insert(run_id, Arc::clone(&run));
        {
            let mut editors = lock(&self.registry.editors);
            let editor = editors
                .entry(target.editor.clone())
                .or_insert_with(|| Editor {
                    base: target.session,
                    child: None,
                    database: None,
                    busy: Some(run_id),
                    runs: Vec::new(),
                });
            editor.base = target.session;
            editor.child = Some(session.clone());
            editor.database.clone_from(&database);
            editor.runs.push(run_id);
        }

        let (tx, rx) = mpsc::channel(self.config.event_capacity.max(1));
        let context = RunContext {
            run: Arc::clone(&run),
            registry: Arc::clone(&self.registry),
            database: session.current_database().or(database),
            profile_id: base.summary().profile_id,
            session,
            dialect,
            script: script.to_owned(),
            options,
            dir,
            memory: Arc::new(ByteBudget::new(self.config.memory_budget_bytes)),
            disk: Arc::clone(&self.disk),
            history: Arc::clone(&self.history),
            rows_interval: self.config.rows_event_interval,
            tx,
        };
        tokio::spawn(supervise(context));
        Ok(RunHandle {
            id: run_id,
            events: rx,
        })
    }

    /// The editor's child session for this run, reopened when it is gone, belongs to another
    /// base, or sits on another database than the one asked for. Returns it with the database
    /// it was opened on.
    async fn editor_session(
        &self,
        target: &RunTarget,
        engine: Engine,
        (old_base, old_child, old_database): (SessionId, Option<SessionHandle>, Option<String>),
        options: &RunOptions,
    ) -> Result<(SessionHandle, Option<String>)> {
        // On `Attached` engines every database is visible from one session.
        let attached = self
            .dialects
            .tree_layout(engine)
            .is_ok_and(|layout| layout.scope == DatabaseScope::Attached);
        let wanted = if attached {
            None
        } else {
            options.database.clone()
        };
        let same_base = old_base == target.session;
        if let Some(child) = &old_child {
            let alive = same_base && self.manager.get(child.id()).is_some();
            let on_wanted = wanted.as_deref().is_none_or(|w| {
                old_database.as_deref() == Some(w) || child.current_database().as_deref() == Some(w)
            });
            if alive && on_wanted {
                return Ok((child.clone(), old_database));
            }
            match self.manager.close(child.id()).await {
                Ok(()) => {}
                Err(e) if e.category() == ErrorCategory::NotFound => {}
                Err(e) => tracing::debug!(error = %e, "closing an editor session failed"),
            }
        }
        let database = wanted.or(if same_base { old_database } else { None });
        let child = self
            .manager
            .open_child(
                target.session,
                database.as_deref(),
                CancellationToken::new(),
            )
            .await?;
        Ok((child, database))
    }

    /// Cancels a running run. `false` when it is unknown or already ended.
    pub fn cancel(&self, run: RunId) -> bool {
        let Some(run) = self.registry.run(run) else {
            return false;
        };
        if run.state() != RunState::Running {
            return false;
        }
        run.token.cancel();
        true
    }

    /// Rows `[offset, offset + limit)` of result `result_index` of `run` (limit clamped to
    /// [`QueryConfig::max_page_rows`]), also while the result is still arriving. `NotFound`
    /// for an unknown or closed run or result index.
    pub async fn page(
        &self,
        run: RunId,
        result_index: usize,
        offset: u64,
        limit: usize,
    ) -> Result<arrow_array::RecordBatch> {
        let store = {
            let found = self
                .registry
                .run(run)
                .ok_or_else(|| Error::not_found(format!("no open query run {run}")))?;
            let results = lock(&found.results);
            results
                .get(result_index)
                .map(|r| Arc::clone(&r.store))
                .ok_or_else(|| {
                    Error::not_found(format!("query run {run} has no result {result_index}"))
                })?
        };
        let limit = limit.min(self.config.max_page_rows);
        tokio::task::spawn_blocking(move || store.page(offset, limit))
            .await
            .map_err(|_| Error::internal("reading a result page stopped unexpectedly"))?
    }

    /// [`QueryService::page`] encoded as an Arrow IPC stream (what `query_page` returns).
    pub async fn page_ipc(
        &self,
        run: RunId,
        result_index: usize,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<u8>> {
        let batch = self.page(run, result_index, offset, limit).await?;
        crate::results::encode_ipc_stream(&batch)
    }

    /// Snapshot of a run; `NotFound` when unknown or closed.
    pub fn info(&self, run: RunId) -> Result<RunInfo> {
        let found = self
            .registry
            .run(run)
            .ok_or_else(|| Error::not_found(format!("no open query run {run}")))?;
        let statements = lock(&found.statements).clone();
        let results = lock(&found.results)
            .iter()
            .enumerate()
            .map(|(result_index, r)| ResultInfo {
                result_index,
                statement_index: r.statement_index,
                columns: ColumnInfo::from_schema(r.store.schema()),
                rows: r.store.row_count(),
                complete: r.store.is_complete(),
                truncated: r.store.is_truncated(),
            })
            .collect();
        Ok(RunInfo {
            run_id: found.id,
            editor_id: found.editor.clone(),
            state: found.state(),
            statements,
            results,
        })
    }

    /// Cancels (if running) and forgets a run, deleting its spill files. Unknown runs are fine.
    pub async fn close(&self, run: RunId) -> Result<()> {
        let Some(found) = self.registry.run(run) else {
            return Ok(());
        };
        found.token.cancel();
        let mut state = found.state.subscribe();
        // A run whose driver does not stop is forgotten anyway; its task ends on its own.
        let _ = tokio::time::timeout(CLOSE_WAIT, state.wait_for(|s| *s != RunState::Running)).await;
        lock(&self.registry.runs).remove(&run);
        if let Some(editor) = lock(&self.registry.editors).get_mut(&found.editor) {
            editor.runs.retain(|id| *id != run);
        }
        // Drops the stores (their files go now) and the directory, even if a run task that
        // did not stop in time still holds a reference to it.
        lock(&found.results).clear();
        delete_run_dir(&found);
        Ok(())
    }

    /// Closes every run of `editor` and its child session. Unknown editors are fine.
    pub async fn close_editor(&self, editor: &EditorId) -> Result<()> {
        let runs: Vec<RunId> = lock(&self.registry.runs)
            .values()
            .filter(|r| r.editor == *editor)
            .map(|r| r.id)
            .collect();
        for run in runs {
            self.close(run).await?;
        }
        let removed = lock(&self.registry.editors).remove(editor);
        if let Some(child) = removed.and_then(|e| e.child) {
            match self.manager.close(child.id()).await {
                Ok(()) => {}
                Err(e) if e.category() == ErrorCategory::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// The statement under the cursor for "run current statement": `utf16_offset` is the
    /// editor's offset in UTF-16 code units (Monaco `getOffsetAt`). `None` when the text has no
    /// statement; `Unsupported` for engines without SQL.
    pub fn statement_at(
        engine: Engine,
        text: &str,
        utf16_offset: usize,
    ) -> Result<Option<StatementSpan>> {
        let dialect = SqlDialect::for_engine(engine).ok_or_else(|| {
            Error::unsupported(format!("{} has no SQL editor", engine.display_name()))
        })?;
        let byte = oxbase_sql::byte_offset_from_utf16(text, utf16_offset);
        let statements = oxbase_sql::split(dialect, text);
        Ok(oxbase_sql::pick_statement(&statements, byte).map(|picked| {
            let index = statements
                .iter()
                .position(|s| s.range == picked.range)
                .unwrap_or_default();
            StatementSpan::new(index, picked)
        }))
    }
}

impl fmt::Debug for QueryService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryService")
            .field("config", &self.config)
            .field("disk_used", &self.disk.used())
            .finish_non_exhaustive()
    }
}

impl Drop for QueryService {
    /// Cancels every run and deletes every run directory now (running tasks wind down on
    /// their own).
    fn drop(&mut self) {
        let runs: Vec<Arc<Run>> = lock(&self.registry.runs)
            .drain()
            .map(|(_, run)| run)
            .collect();
        for run in runs {
            run.token.cancel();
            lock(&run.results).clear();
            delete_run_dir(&run);
        }
    }
}

/// Everything a run task needs.
struct RunContext {
    run: Arc<Run>,
    registry: Arc<Registry>,
    session: SessionHandle,
    /// The database the statements run in, for history.
    database: Option<String>,
    profile_id: Uuid,
    dialect: SqlDialect,
    script: String,
    options: RunOptions,
    dir: Arc<RunDir>,
    memory: Arc<ByteBudget>,
    disk: Arc<ByteBudget>,
    history: Arc<HistoryService>,
    rows_interval: Duration,
    tx: mpsc::Sender<RunEvent>,
}

/// How a statement ended.
enum Outcome {
    Done {
        rows_affected: Option<u64>,
        /// Rows stored over its result sets, when it had any.
        rows_returned: Option<u64>,
    },
    Failed(Error),
    Cancelled {
        rows_returned: Option<u64>,
    },
}

/// How the run ends.
enum Terminal {
    Finished,
    Failed(Error),
    Cancelled,
}

/// The result set a statement is currently reading.
struct Current {
    result_index: usize,
    store: Arc<ResultStore>,
    last_rows_event: Option<Instant>,
}

/// Runs the run task and turns a panic inside it into a `Failed` end, so the UI never waits
/// for a terminal event that does not come.
async fn supervise(context: RunContext) {
    let run = Arc::clone(&context.run);
    let registry = Arc::clone(&context.registry);
    let tx = context.tx.clone();
    let started = Instant::now();
    if tokio::spawn(execute(context)).await.is_err() {
        tracing::error!(run = %run.id, "a query run stopped unexpectedly");
        lock(&run.results)
            .iter()
            .filter(|r| !r.store.is_complete())
            .for_each(|r| r.store.finish(true));
        run.state.send_replace(RunState::Failed);
        registry.release_editor(&run.editor, run.id);
        let _ = tx
            .send(RunEvent::Failed {
                error: ErrorInfo {
                    category: ErrorCategory::Internal,
                    message: "the query run stopped unexpectedly".to_owned(),
                },
                elapsed_ms: millis(started.elapsed()),
            })
            .await;
    }
}

/// The run task: see the module docs.
async fn execute(context: RunContext) {
    let started = Instant::now();
    let statements = split_script(context.dialect, &context.script);
    let spans: Vec<StatementSpan> = statements
        .iter()
        .enumerate()
        .map(|(i, s)| StatementSpan::new(i, s))
        .collect();
    *lock(&context.run.statements) = spans.clone();
    context
        .emit(RunEvent::Started {
            run_id: context.run.id,
            statements: spans.clone(),
        })
        .await;

    let mut terminal = Terminal::Finished;
    for (index, (statement, span)) in statements.iter().zip(&spans).enumerate() {
        if context.run.token.is_cancelled() {
            terminal = Terminal::Cancelled;
            break;
        }
        context
            .emit(RunEvent::StatementStarted {
                statement_index: index,
            })
            .await;
        let started_at = Utc::now();
        let clock = Instant::now();
        let outcome = context.run_statement(index, span, &statement.text).await;
        let elapsed = clock.elapsed();
        context
            .record_history(&statement.text, started_at, elapsed, &outcome)
            .await;
        let elapsed_ms = millis(elapsed);
        match outcome {
            Outcome::Done { rows_affected, .. } => {
                context
                    .emit(RunEvent::StatementDone {
                        statement_index: index,
                        rows_affected,
                        elapsed_ms,
                    })
                    .await;
            }
            Outcome::Failed(error) => {
                context
                    .emit(RunEvent::StatementFailed {
                        statement_index: index,
                        error: ErrorInfo::from(&error),
                        elapsed_ms,
                    })
                    .await;
                // A lost session cannot run the rest.
                let lost = error.category() == ErrorCategory::Connection;
                if context.options.stop_on_error || lost {
                    terminal = Terminal::Failed(error);
                    break;
                }
            }
            Outcome::Cancelled { .. } => {
                context
                    .emit(RunEvent::StatementFailed {
                        statement_index: index,
                        error: ErrorInfo::from(&Error::Cancelled),
                        elapsed_ms,
                    })
                    .await;
                terminal = Terminal::Cancelled;
                break;
            }
        }
    }

    let elapsed_ms = millis(started.elapsed());
    let (state, event) = match terminal {
        Terminal::Finished => (RunState::Finished, RunEvent::Finished { elapsed_ms }),
        Terminal::Failed(error) => (
            RunState::Failed,
            RunEvent::Failed {
                error: ErrorInfo::from(&error),
                elapsed_ms,
            },
        ),
        Terminal::Cancelled => (RunState::Cancelled, RunEvent::Cancelled { elapsed_ms }),
    };
    // State and editor first, so a caller that sees the terminal event can start the next run.
    context.run.state.send_replace(state);
    context
        .registry
        .release_editor(&context.run.editor, context.run.id);
    context.emit(event).await;
}

impl RunContext {
    /// Sends a structural event, waiting while the channel is full. A gone receiver is fine.
    async fn emit(&self, event: RunEvent) {
        let _ = self.tx.send(event).await;
    }

    /// Runs one statement: see the module docs.
    async fn run_statement(&self, index: usize, span: &StatementSpan, sql: &str) -> Outcome {
        let token = &self.run.token;
        let _guard = tokio::select! {
            biased;
            () = token.cancelled() => return Outcome::Cancelled { rows_returned: None },
            guard = self.session.exclusive() => guard,
        };
        let statement_token = token.child_token();
        let executed = tokio::select! {
            biased;
            () = token.cancelled() => return Outcome::Cancelled { rows_returned: None },
            executed = self.session.connection().execute(sql, &[], statement_token.clone()) => executed,
        };
        let mut stream = match executed {
            Ok(stream) => stream,
            Err(e) if token.is_cancelled() && e.is_cancelled() => {
                return Outcome::Cancelled {
                    rows_returned: None,
                };
            }
            Err(e) => return Outcome::Failed(e),
        };

        let mut current: Option<Current> = None;
        let mut rows_affected: Option<u64> = None;
        let mut rows_returned: Option<u64> = None;
        loop {
            let item = tokio::select! {
                biased;
                () = token.cancelled() => None,
                item = stream.next() => Some(item),
            };
            let Some(item) = item else {
                // Cancelled: stop the statement, keep what arrived.
                stop(stream, &statement_token).await;
                self.end_result(current.take(), true, &mut rows_returned)
                    .await;
                return Outcome::Cancelled { rows_returned };
            };
            match item {
                None => {
                    self.end_result(current.take(), false, &mut rows_returned)
                        .await;
                    return Outcome::Done {
                        rows_affected,
                        rows_returned,
                    };
                }
                Some(Err(e)) => {
                    self.end_result(current.take(), true, &mut rows_returned)
                        .await;
                    if token.is_cancelled() {
                        return Outcome::Cancelled { rows_returned };
                    }
                    return Outcome::Failed(e);
                }
                Some(Ok(ResultItem::Notice(notice))) => {
                    self.emit(RunEvent::notice(Some(index), &notice)).await;
                }
                Some(Ok(ResultItem::RowsAffected(n))) => {
                    rows_affected = Some(rows_affected.unwrap_or(0).saturating_add(n));
                }
                Some(Ok(ResultItem::Schema(schema))) => {
                    self.end_result(current.take(), false, &mut rows_returned)
                        .await;
                    let store = Arc::new(ResultStore::new(
                        Arc::clone(&schema),
                        Arc::clone(&self.dir),
                        Arc::clone(&self.memory),
                        Arc::clone(&self.disk),
                    ));
                    let result_index = {
                        let mut results = lock(&self.run.results);
                        results.push(RunResult {
                            statement_index: index,
                            store: Arc::clone(&store),
                        });
                        results.len() - 1
                    };
                    self.emit(RunEvent::ResultSet {
                        result_index,
                        statement_index: index,
                        statement_range: span.clone(),
                        columns: ColumnInfo::from_schema(&schema),
                    })
                    .await;
                    current = Some(Current {
                        result_index,
                        store,
                        last_rows_event: None,
                    });
                }
                Some(Ok(ResultItem::Batch(batch))) => {
                    let Some(open) = current.as_mut() else {
                        stop(stream, &statement_token).await;
                        return Outcome::Failed(Error::internal(
                            "the driver sent rows before their columns",
                        ));
                    };
                    match self.store_batch(index, open, batch).await {
                        Ok(true) => {}
                        Ok(false) => {
                            // Row limit or disk cap: drop the rest of this statement.
                            stop(stream, &statement_token).await;
                            self.end_result(current.take(), true, &mut rows_returned)
                                .await;
                            return Outcome::Done {
                                rows_affected,
                                rows_returned,
                            };
                        }
                        Err(e) => {
                            stop(stream, &statement_token).await;
                            self.end_result(current.take(), true, &mut rows_returned)
                                .await;
                            return Outcome::Failed(e);
                        }
                    }
                }
            }
        }
    }

    /// Stores `batch` in the current result. `Ok(false)`: the result is full (row limit or
    /// disk cap; a warning notice was sent) and the statement must stop.
    async fn store_batch(
        &self,
        index: usize,
        open: &mut Current,
        mut batch: arrow_array::RecordBatch,
    ) -> Result<bool> {
        let mut limit_reached = false;
        if let Some(max) = self.options.max_rows {
            let room = max.saturating_sub(open.store.row_count());
            let rows = u64::try_from(batch.num_rows()).unwrap_or(u64::MAX);
            if rows > room {
                batch = batch.slice(0, usize::try_from(room).unwrap_or(usize::MAX));
                limit_reached = true;
            }
        }
        let outcome = if batch.num_rows() == 0 {
            AppendOutcome::Stored
        } else {
            let store = Arc::clone(&open.store);
            tokio::task::spawn_blocking(move || store.append(batch))
                .await
                .map_err(|_| Error::internal("storing result rows stopped unexpectedly"))??
        };
        let total = open.store.row_count();
        let due = open
            .last_rows_event
            .is_none_or(|last| last.elapsed() >= self.rows_interval);
        if due {
            // Lossy: the next one carries the new total.
            let _ = self.tx.try_send(RunEvent::Rows {
                result_index: open.result_index,
                total_so_far: total,
            });
            open.last_rows_event = Some(Instant::now());
        }
        let message = match outcome {
            AppendOutcome::DiskFull => format!(
                "result {} stopped after {total} rows: the disk space for query results ({} MiB) \
                 is used up; close other results or lower the row count",
                open.result_index + 1,
                self.disk.limit() / (1024 * 1024)
            ),
            AppendOutcome::Stored if limit_reached => format!(
                "result {} stopped after {total} rows: the row limit was reached",
                open.result_index + 1
            ),
            AppendOutcome::Stored => return Ok(true),
        };
        self.emit(RunEvent::Notice {
            statement_index: Some(index),
            level: NoticeLevel::Warning,
            message,
            code: None,
        })
        .await;
        Ok(false)
    }

    /// Completes the result set being read, if any, and emits its `ResultDone`.
    async fn end_result(
        &self,
        current: Option<Current>,
        truncated: bool,
        rows_returned: &mut Option<u64>,
    ) {
        let Some(current) = current else {
            return;
        };
        current.store.finish(truncated);
        let total_rows = current.store.row_count();
        *rows_returned = Some(rows_returned.unwrap_or(0).saturating_add(total_rows));
        self.emit(RunEvent::ResultDone {
            result_index: current.result_index,
            total_rows,
            truncated: current.store.is_truncated(),
        })
        .await;
    }

    /// One history entry for a statement that started. Failures (and panics) of the history
    /// are logged without SQL text and never fail the run.
    async fn record_history(
        &self,
        sql: &str,
        started_at: chrono::DateTime<Utc>,
        duration: Duration,
        outcome: &Outcome,
    ) {
        if !self.options.record_history {
            return;
        }
        let (status, rows, error) = match outcome {
            Outcome::Done {
                rows_affected,
                rows_returned,
            } => (HistoryStatus::Ok, rows_returned.or(*rows_affected), None),
            Outcome::Failed(e) => (HistoryStatus::Error, None, Some(ErrorInfo::from(e).message)),
            Outcome::Cancelled { rows_returned } => {
                (HistoryStatus::Cancelled, *rows_returned, None)
            }
        };
        let record = HistoryRecord {
            profile_id: self.profile_id,
            database: self.database.clone(),
            dialect: self.dialect,
            sql: sql.to_owned(),
            started_at,
            duration,
            status,
            rows,
            error,
        };
        let history = Arc::clone(&self.history);
        match tokio::spawn(async move { history.record(record).await }).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::warn!(category = %e.category(), "recording query history failed");
            }
            Err(_) => tracing::warn!("recording query history stopped unexpectedly"),
        }
    }
}

/// Stops a statement: cancels its token (the driver cancels it on the server), lets the
/// stream wind down for at most [`STOP_GRACE`], then drops it.
async fn stop(mut stream: ResultStream, token: &CancellationToken) {
    token.cancel();
    let _ =
        tokio::time::timeout(STOP_GRACE, async { while stream.next().await.is_some() {} }).await;
    drop(stream);
}

/// The statements of `script`.
fn split_script(dialect: SqlDialect, script: &str) -> Vec<oxbase_sql::Statement> {
    // Unit tests of this crate run before the splitter of the `sql` stream may be merged; they
    // fall back to a minimal splitter when `oxbase_sql::split` is not implemented yet.
    #[cfg(test)]
    if let Some(statements) = test_split::fallback_split(dialect, script) {
        return statements;
    }
    oxbase_sql::split(dialect, script)
}

/// Deletes the spill directory of `run` now.
fn delete_run_dir(run: &Run) {
    if let Some(dir) = lock(&run.dir).take() {
        let path = dir.path().to_owned();
        drop(dir);
        if path.exists() {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Removes `run-*` directories under `base` that were last modified at least `age` ago.
fn remove_stale_run_dirs(base: &Path, age: Duration) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("run-") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let stale = meta.is_dir()
            && meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|elapsed| elapsed >= age);
        if stale && std::fs::remove_dir_all(entry.path()).is_ok() {
            tracing::debug!("removed a stale query result directory");
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_schema::DataType;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn events_serialize_camel_case_with_a_type_tag() {
        let run_id = RunId(Uuid::nil());
        let e = RunEvent::Started {
            run_id,
            statements: vec![StatementSpan {
                index: 0,
                start_byte: 0,
                end_byte: 8,
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 9,
                returns_rows: true,
            }],
        };
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({
                "type": "started",
                "runId": Uuid::nil().to_string(),
                "statements": [{
                    "index": 0, "startByte": 0, "endByte": 8, "startLine": 1, "startColumn": 1,
                    "endLine": 1, "endColumn": 9, "returnsRows": true
                }]
            })
        );
        let rows = RunEvent::Rows {
            result_index: 2,
            total_so_far: 1_000_000,
        };
        assert_eq!(
            serde_json::to_value(&rows).unwrap(),
            json!({"type": "rows", "resultIndex": 2, "totalSoFar": 1_000_000})
        );
        let done = RunEvent::StatementDone {
            statement_index: 1,
            rows_affected: None,
            elapsed_ms: 5,
        };
        assert_eq!(
            serde_json::to_value(&done).unwrap(),
            json!({"type": "statementDone", "statementIndex": 1, "elapsedMs": 5})
        );
        let failed = RunEvent::Failed {
            error: ErrorInfo::from(&Error::sql(Some("42P01"), "relation \"x\" does not exist")),
            elapsed_ms: 1,
        };
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(json["error"]["category"], "sql");
        assert_eq!(
            json["error"]["message"],
            "SQL error [42P01]: relation \"x\" does not exist"
        );
        assert!(failed.is_terminal());
        assert!(!rows.is_terminal());
        for e in [e, rows, done, failed] {
            let back: RunEvent = serde_json::from_value(serde_json::to_value(&e).unwrap()).unwrap();
            assert_eq!(back, e);
        }
        let n = RunEvent::notice(Some(0), &Notice::warning("careful"));
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            json!({"type": "notice", "statementIndex": 0, "level": "warning", "message": "careful"})
        );
    }

    #[test]
    fn options_and_ids() {
        let o: RunOptions = serde_json::from_str(r#"{"maxRows": 1000}"#).unwrap();
        assert_eq!(o.max_rows, Some(1000));
        assert!(o.stop_on_error && o.record_history && !o.keep_previous);
        assert_eq!(
            serde_json::to_value(RunOptions::default()).unwrap(),
            json!({"stopOnError": true, "recordHistory": true, "keepPrevious": false})
        );
        assert!(EditorId::new("tab-1").validate().is_ok());
        assert!(EditorId::new("").validate().is_err());
        assert!(EditorId::new("x".repeat(201)).validate().is_err());
        assert_eq!(
            serde_json::to_value(EditorId::new("t")).unwrap(),
            json!("t")
        );
        assert_eq!(
            serde_json::to_value(RunId(Uuid::nil())).unwrap(),
            json!(Uuid::nil().to_string())
        );
    }

    #[test]
    fn column_info_from_arrow_fields() {
        let mut meta = std::collections::HashMap::new();
        meta.insert("oxbase.pg.type".to_owned(), "int8".to_owned());
        let schema = Schema::new(vec![
            Field::new("id", DataType::Int64, false).with_metadata(meta),
            Field::new("name", DataType::Utf8, true),
        ]);
        let cols = ColumnInfo::from_schema(&schema);
        assert_eq!(cols[0].data_type, "Int64");
        assert!(!cols[0].nullable);
        assert_eq!(cols[0].metadata["oxbase.pg.type"], "int8");
        let json = serde_json::to_value(&cols[1]).unwrap();
        assert_eq!(
            json,
            json!({"name": "name", "dataType": "Utf8", "nullable": true})
        );
    }

    #[test]
    fn statement_at_rejects_engines_without_sql() {
        let err = QueryService::statement_at(Engine::Redis, "GET x", 0).unwrap_err();
        assert_eq!(err.category(), ErrorCategory::Unsupported);
    }

    #[test]
    fn default_config() {
        let c = QueryConfig::default();
        assert!(c.spill_dir.ends_with("oxbase-results"));
        assert_eq!(c.memory_budget_bytes, 64 * 1024 * 1024);
        assert!(c.event_capacity > 0 && c.max_page_rows > 0);
    }
}
