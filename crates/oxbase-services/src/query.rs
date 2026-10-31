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

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arrow_schema::{Field, Schema};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use oxbase_dialect::DialectRegistry;
use oxbase_driver::{Notice, NoticeSeverity};
use oxbase_sql::{SqlDialect, StatementKind};
use oxbase_types::{Engine, Error, ErrorCategory, Result};

use crate::history::HistoryService;
use crate::results::ByteBudget;
use crate::{ConnectionManager, SessionId};

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
}

impl QueryService {
    pub fn new(
        manager: Arc<ConnectionManager>,
        dialects: Arc<DialectRegistry>,
        history: Arc<HistoryService>,
        config: QueryConfig,
    ) -> Self {
        let disk = Arc::new(ByteBudget::new(config.disk_cap_bytes));
        Self {
            manager,
            dialects,
            history,
            config,
            disk,
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
    #[allow(clippy::unused_async)] // m2: services fills the body
    pub async fn run(
        &self,
        target: RunTarget,
        script: &str,
        options: RunOptions,
    ) -> Result<RunHandle> {
        let _ = (self, target, script, options);
        unimplemented!("m2: services")
    }

    /// Cancels a running run. `false` when it is unknown or already ended.
    pub fn cancel(&self, run: RunId) -> bool {
        let _ = (self, run);
        unimplemented!("m2: services")
    }

    /// Rows `[offset, offset + limit)` of result `result_index` of `run` (limit clamped to
    /// [`QueryConfig::max_page_rows`]), also while the result is still arriving. `NotFound`
    /// for an unknown or closed run or result index.
    #[allow(clippy::unused_async)] // m2: services fills the body
    pub async fn page(
        &self,
        run: RunId,
        result_index: usize,
        offset: u64,
        limit: usize,
    ) -> Result<arrow_array::RecordBatch> {
        let _ = (self, run, result_index, offset, limit);
        unimplemented!("m2: services")
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
        let _ = (self, run);
        unimplemented!("m2: services")
    }

    /// Cancels (if running) and forgets a run, deleting its spill files. Unknown runs are fine.
    #[allow(clippy::unused_async)] // m2: services fills the body
    pub async fn close(&self, run: RunId) -> Result<()> {
        let _ = (self, run);
        unimplemented!("m2: services")
    }

    /// Closes every run of `editor` and its child session. Unknown editors are fine.
    #[allow(clippy::unused_async)] // m2: services fills the body
    pub async fn close_editor(&self, editor: &EditorId) -> Result<()> {
        let _ = (self, editor);
        unimplemented!("m2: services")
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
