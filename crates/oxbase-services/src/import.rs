//! Import (M5): a file into a table, through preview → mapping → target mode → run, with
//! progress, cancel, an error policy, a transaction policy and a final report.
//! Contract: `docs/m5-contracts.md` §5.
//!
//! # Pipeline
//!
//! 1. **Preview** ([`ImportService::preview`]): `oxbase_io::preview` on a blocking thread:
//!    columns with inferred types and warnings, the first rows as text, what was detected.
//! 2. **Request** ([`ImportRequest`]): one [`ColumnSpec`] per source column (types as inferred
//!    or overridden), a [`ColumnMapping`] per source column that is imported (target column
//!    name, or skipped; in create / replace mode the new column's type), the [`ImportTarget`]
//!    (session, table, [`TargetMode`]), [`ConversionOptions`], [`ErrorPolicy`] and
//!    [`TransactionPolicy`]. Expressions in mappings are not part of M5.
//! 3. **Plan** ([`ImportService::plan`]): checks the request against the table as the server
//!    has it now (loaded with the engine's `Table` fetcher) or the table it will create
//!    (`DdlGenerator::create` of a `TableDef` built from the mapping, `native_type` empty so the
//!    dialect derives it): every mapped target column exists exactly once, no generated column
//!    is written (identity `ALWAYS`: refused; `BY DEFAULT` / auto-increment: allowed), every
//!    NOT NULL column without a default is mapped, every source → target type pair has a
//!    conversion ([`conversion_note`]). Returns the [`ImportPlan`]: DDL text to run, columns,
//!    the bulk method, the resolved transaction policy, destructive flag, warnings. Plans
//!    nothing on a failure (`InvalidInput` naming the column).
//! 4. **Run** ([`ImportService::start`]): the same checks, then on a **dedicated child
//!    session** of the target's base session (`ConnectionManager::open_child` on the table's
//!    database; closed at the end), so the tree and the editors stay usable:
//!    - target mode DDL ([`TargetMode`]);
//!    - read: `oxbase_io::open` + `oxbase_io::spawn_reader` (at most
//!      [`ImportConfig::reader_capacity`] batches buffered: bounded memory);
//!    - per batch: apply [`ConversionOptions`], turn the source batch into the *load schema*
//!      of the target columns (`oxbase_dialect::LoadTarget::schema`, value conversions with
//!      range / length / NOT NULL checks), collect row issues of the reader and of the
//!      conversion, apply the [`ErrorPolicy`] (drop the rows with issues with `arrow_select`'s
//!      `filter_record_batch`, or stop), re-chunk to the loader's `rows_per_call` and call
//!      `BulkLoader::load` inside the current transaction;
//!    - commit per the [`TransactionPolicy`] and emit [`ImportEvent::Committed`].
//! 5. **Report** ([`ImportReport`]) in the terminal event.
//!
//! # Transactions and what a cancel or failure leaves behind
//!
//! [`TransactionPolicy::Auto`] resolves to `Single` when the file is at most
//! [`ImportConfig::single_transaction_max_bytes`] (64 MiB), else to `Batches` of
//! [`ImportConfig::default_commit_rows`] rows (100 000).
//!
//! | Mode / policy | Single transaction | Commit every N rows |
//! |---|---|---|
//! | append | nothing inserted | the committed batches stay (`rows_committed`, a multiple of N) |
//! | truncate | PostgreSQL, SQL Server, SQLite: rolled back (table as before; SQLite and SQL Server truncate with `DELETE` / `TRUNCATE` in the transaction). MySQL / MariaDB: `TRUNCATE` commits at once, so the table is **empty** | the table holds the committed batches |
//! | delete_all | `DELETE FROM` in the transaction: rolled back | the table holds the committed batches |
//! | create | PostgreSQL, SQL Server, SQLite: the `CREATE` is rolled back with the rows. MySQL / MariaDB (DDL commits): the service **drops the table it created** | the service **drops the table it created** (a partially filled new table is useless) |
//! | replace | PostgreSQL, SQL Server, SQLite: drop + create + rows roll back together (old table intact). MySQL / MariaDB: the old table is **gone** and the new one is dropped | old table **gone**; the new one is dropped |
//!
//! [`ImportReport::left_behind`] states the case that happened in words; [`ImportPlan`] marks
//! truncate on MySQL / MariaDB and replace outside a single transaction as `destructive`, and
//! the UI confirms those. A cancel stops reading at once, cancels the running load statement
//! (driver cancel) and rolls back the open transaction; the run ends with
//! [`ImportEvent::Cancelled`] **within 2 s** of [`ImportService::cancel`].
//!
//! # Errors
//!
//! - **Row issues** (unreadable value, conversion failure, NULL for a NOT NULL column, text
//!   longer than the column unless `truncate_text`, ragged row): with [`ErrorPolicy::Stop`] the
//!   import fails at the first one (rolled back per the table above); with
//!   [`ErrorPolicy::Skip`] the row is skipped and reported (row number, column, reason), and
//!   the import fails once more than `max_errors` rows were skipped. At most
//!   [`ImportConfig::max_reported_errors`] errors are kept in the report
//!   ([`ImportReport::errors_truncated`]), the first 100 also sent as [`ImportEvent::RowError`].
//! - **Server errors** (constraint violation, trigger, deadlock) fail the import whatever the
//!   policy: a bulk call cannot say which row failed. The error names the source row range of
//!   the failing call.
//! - A lost session fails the import (`Connection` category).
//!
//! # Progress
//!
//! [`ImportEvent::Progress`] at most every [`ImportConfig::progress_interval`] (250 ms) and
//! after each commit: file bytes read (monotonic, reaches `total_bytes`), rows read, inserted,
//! committed and skipped. Progress events are sent with `try_send` (dropped when the consumer
//! lags; the next carries the new totals); every other event is awaited.
//!
//! Owner of the bodies: the **services** stream.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use oxbase_dialect::{BulkMethod, DialectRegistry};
use oxbase_io::{ColumnSpec, PreviewLimits, ReadOptions, SourcePreview};
use oxbase_types::{DataType, Error, ObjectRef, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::query::ErrorInfo;
use crate::tree::TreeService;
use crate::{CancellationToken, ConnectionManager, SessionId, check_cancelled};

/// Identifies one import run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ImportId(pub Uuid);

impl ImportId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for ImportId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ImportId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The file to import. The path comes from the caller (the app resolves a file handle from a
/// native dialog; the CLI takes it from its arguments) and is never serialized or sent to the
/// UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSource {
    pub path: PathBuf,
    pub options: ReadOptions,
}

/// What happens to the target table before rows go in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetMode {
    /// Create a new table (fails when it exists).
    Create,
    /// Insert into the existing table.
    #[default]
    Append,
    /// Empty the table with `TRUNCATE` (SQLite: `DELETE FROM`), then insert. MySQL / MariaDB
    /// commit a `TRUNCATE` at once (module docs).
    Truncate,
    /// Empty the table with `DELETE FROM` in the import's transaction, then insert.
    DeleteAll,
    /// Drop the table and create it from the mapping, then insert.
    Replace,
}

/// Where the rows go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportTarget {
    /// A base session (`ConnectionManager::open`).
    pub session: SessionId,
    /// The table (a `Table` ref as the object tree lists it; for create mode the new name, with
    /// catalog / schema like its siblings).
    pub table: ObjectRef,
    #[serde(default)]
    pub mode: TargetMode,
}

/// One imported source column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnMapping {
    /// Index of the source column ([`ImportRequest::columns`]).
    pub source: usize,
    /// Target column name; `None` skips the source column (same as leaving it out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Create / replace mode: the new column's type (default: the source column's type).
    /// Ignored in the other modes (the table's type wins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_type: Option<DataType>,
}

/// Value clean-up before conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConversionOptions {
    /// Trim spaces around text values.
    pub trim_text: bool,
    /// Empty text is NULL (for every target type, not only non-text ones).
    pub empty_text_as_null: bool,
    /// Text longer than a `char(n)` / `varchar(n)` target is cut instead of a row issue (noted
    /// in the report).
    pub truncate_text: bool,
}

/// What a row issue does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ErrorPolicy {
    /// Fail the import at the first row issue.
    #[default]
    Stop,
    /// Skip rows with issues; fail once more than `max_errors` rows were skipped.
    Skip { max_errors: u64 },
}

/// When the import commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum TransactionPolicy {
    /// `Single` up to [`ImportConfig::single_transaction_max_bytes`], else `Batches` of
    /// [`ImportConfig::default_commit_rows`].
    #[default]
    Auto,
    /// One transaction for the whole import (all or nothing on transactional storage).
    Single,
    /// Commit after about every `rows` rows (at the end of the loader call that crosses it).
    Batches { rows: u64 },
}

/// Everything about an import except the file path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportRequest {
    pub options: ReadOptions,
    /// One spec per source column, in source order (from the preview, types maybe overridden).
    pub columns: Vec<ColumnSpec>,
    /// The imported columns; source columns not listed (or with `target: None`) are skipped.
    pub mapping: Vec<ColumnMapping>,
    pub target: ImportTarget,
    /// Create / replace mode: primary key columns of the new table (target names).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub primary_key: Vec<String>,
    #[serde(default)]
    pub conversion: ConversionOptions,
    #[serde(default)]
    pub errors: ErrorPolicy,
    #[serde(default)]
    pub transaction: TransactionPolicy,
}

/// One column of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedColumn {
    pub source: usize,
    pub source_name: String,
    pub source_type: DataType,
    pub target: String,
    pub target_type: DataType,
    /// How values convert, when they do not pass through unchanged ([`conversion_note`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversion: Option<String>,
}

/// The resolved transaction policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedTransaction {
    /// `None`: one transaction; `Some(n)`: commit about every `n` rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_every: Option<u64>,
}

/// What a run of the request will do ([`ImportService::plan`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPlan {
    pub table: ObjectRef,
    pub mode: TargetMode,
    /// The table is created by the import (create, replace).
    pub creates_table: bool,
    /// DDL the run executes before inserting (drop / create / truncate / delete), as SQL text
    /// for the preview.
    pub ddl: Vec<String>,
    pub columns: Vec<PlannedColumn>,
    pub method: BulkMethod,
    pub transaction: ResolvedTransaction,
    /// Existing data can be lost in a way a rollback does not undo (module docs).
    pub destructive: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// One skipped or failing row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportRowError {
    /// Source row number (1-based data rows, `oxbase_io::RowIssue::row`).
    pub row: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    pub message: String,
}

/// The outcome of an import (in the terminal event).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub import_id: ImportId,
    pub table: ObjectRef,
    pub method: BulkMethod,
    pub rows_read: u64,
    /// Rows sent to the server.
    pub rows_inserted: u64,
    /// Rows that are in the table for good (committed).
    pub rows_committed: u64,
    pub rows_skipped: u64,
    pub errors: Vec<ImportRowError>,
    /// More errors happened than the report keeps.
    pub errors_truncated: bool,
    pub table_created: bool,
    /// The import dropped the table it had created (cancel or failure).
    pub table_dropped: bool,
    /// What the target table holds now, in words (module docs, *Transactions*).
    pub left_behind: String,
    pub bytes_read: u64,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// What an import reports, in order: `Started`, then `Progress` / `RowError` / `Committed`,
/// then exactly one of `Finished`, `Failed`, `Cancelled`.
///
/// JSON: `{"type": "<camelCase variant>", ...camelCase fields}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ImportEvent {
    Started {
        import_id: ImportId,
        total_bytes: u64,
        plan: ImportPlan,
    },
    /// Cumulative counters (lossy, see the module docs).
    Progress {
        bytes_read: u64,
        total_bytes: u64,
        rows_read: u64,
        rows_inserted: u64,
        rows_committed: u64,
        rows_skipped: u64,
        elapsed_ms: u64,
    },
    /// One of the first 100 row errors.
    RowError { error: ImportRowError },
    /// A transaction committed; `rows_committed` is the new total.
    Committed { rows_committed: u64 },
    /// Terminal: every row was read and the last transaction committed.
    Finished { report: ImportReport },
    /// Terminal: stopped by an error (`report.left_behind` says what stayed).
    Failed {
        error: ErrorInfo,
        report: ImportReport,
    },
    /// Terminal: stopped by [`ImportService::cancel`].
    Cancelled { report: ImportReport },
}

impl ImportEvent {
    /// `true` for `Finished`, `Failed` and `Cancelled`.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Finished { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }
}

/// A started import: its id and its event stream.
#[derive(Debug)]
pub struct ImportHandle {
    pub id: ImportId,
    pub events: mpsc::Receiver<ImportEvent>,
}

/// Tunables of an [`ImportService`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportConfig {
    /// Rows per source batch.
    pub batch_rows: usize,
    /// Source batches buffered ahead of the loader.
    pub reader_capacity: usize,
    pub preview: PreviewLimits,
    /// Minimum time between two `Progress` events.
    pub progress_interval: Duration,
    /// `TransactionPolicy::Auto` uses one transaction up to this file size.
    pub single_transaction_max_bytes: u64,
    /// `TransactionPolicy::Auto` commits every this many rows above it.
    pub default_commit_rows: u64,
    /// Errors kept in the report.
    pub max_reported_errors: usize,
    /// Capacity of each import's event channel.
    pub event_capacity: usize,
}

impl Default for ImportConfig {
    fn default() -> Self {
        Self {
            batch_rows: 8_192,
            reader_capacity: 4,
            preview: PreviewLimits::default(),
            progress_interval: Duration::from_millis(250),
            single_transaction_max_bytes: 64 * 1024 * 1024,
            default_commit_rows: 100_000,
            max_reported_errors: 1_000,
            event_capacity: 256,
        }
    }
}

/// How values of `from` convert to a column of `to`: `None` when they pass unchanged (same
/// logical type family and the target is at least as wide), a short note otherwise ("text →
/// int: values that are not integers are row errors", "float → decimal(10,2): rounded to 2
/// places"). `Err(InvalidInput)` when there is no conversion (binary → int, json → date).
///
/// The rules (services stream): anything → text / json (text form as the CSV writer writes
/// it); text → any type by parsing with the reader's `ValueFormats`; int ↔ float ↔ decimal with
/// range checks (overflow is a row error, precision loss is a note); date → timestamp
/// (midnight); timestamp → date only with the note "time of day dropped"; timestamp ↔
/// timestamp_tz (UTC); bool ↔ int (0 / 1); uuid ↔ text; binary ↔ text (hex `0x...`).
pub fn conversion_note(from: &DataType, to: &DataType) -> Result<Option<String>> {
    let _ = (from, to);
    unimplemented!("m5: services")
}

/// Imports files into tables. See the module docs.
pub struct ImportService {
    manager: Arc<ConnectionManager>,
    tree: Arc<TreeService>,
    dialects: Arc<DialectRegistry>,
    config: ImportConfig,
    /// Cancel tokens of the running imports.
    running: Mutex<HashMap<ImportId, CancellationToken>>,
}

impl ImportService {
    pub fn new(
        manager: Arc<ConnectionManager>,
        tree: Arc<TreeService>,
        dialects: Arc<DialectRegistry>,
        config: ImportConfig,
    ) -> Self {
        Self {
            manager,
            tree,
            dialects,
            config,
            running: Mutex::new(HashMap::new()),
        }
    }

    pub fn manager(&self) -> &Arc<ConnectionManager> {
        &self.manager
    }

    pub fn tree(&self) -> &Arc<TreeService> {
        &self.tree
    }

    pub fn dialects(&self) -> &Arc<DialectRegistry> {
        &self.dialects
    }

    pub fn config(&self) -> &ImportConfig {
        &self.config
    }

    /// Previews `source` (`oxbase_io::preview` with [`ImportConfig::preview`] on a blocking
    /// thread). `Cancelled` when `cancel` fires.
    pub async fn preview(
        &self,
        source: &ImportSource,
        cancel: CancellationToken,
    ) -> Result<SourcePreview> {
        check_cancelled(&cancel)?;
        let ImportSource { path, options } = source.clone();
        let limits = self.config.preview;
        tokio::task::spawn_blocking(move || oxbase_io::preview(&path, &options, limits, &cancel))
            .await
            .map_err(|_| Error::internal("reading the file stopped unexpectedly"))?
    }

    /// Checks `request` against the target and returns what a run would do (module docs,
    /// step 3). Runs nothing that changes the database.
    #[allow(
        clippy::unused_async,
        reason = "skeleton: the services stream fills it in"
    )]
    pub async fn plan(
        &self,
        source: &ImportSource,
        request: &ImportRequest,
        cancel: CancellationToken,
    ) -> Result<ImportPlan> {
        let _ = (self, source, request, cancel);
        unimplemented!("m5: services")
    }

    /// Starts importing `source` per `request` (module docs, step 4) and returns once the plan
    /// checks passed and the import is registered; events follow on the handle. Fails before
    /// starting like [`ImportService::plan`].
    #[allow(
        clippy::unused_async,
        reason = "skeleton: the services stream fills it in"
    )]
    pub async fn start(
        &self,
        source: ImportSource,
        request: ImportRequest,
    ) -> Result<ImportHandle> {
        let _ = (self, source, request);
        unimplemented!("m5: services")
    }

    /// Cancels a running import. `false` when it is unknown or already ended.
    pub fn cancel(&self, id: ImportId) -> bool {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
            .inspect(CancellationToken::cancel)
            .is_some()
    }

    /// `true` while import `id` runs.
    pub fn is_running(&self, id: ImportId) -> bool {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_io::CsvReadOptions;
    use pretty_assertions::assert_eq;

    #[test]
    fn request_json_shape() {
        let session = SessionId::new();
        let request = ImportRequest {
            options: ReadOptions::Csv(CsvReadOptions::default()),
            columns: vec![ColumnSpec::new("id", DataType::INT64)],
            mapping: vec![ColumnMapping {
                source: 0,
                target: Some("id".into()),
                data_type: None,
            }],
            target: ImportTarget {
                session,
                table: ObjectRef::table(Some("db"), Some("public"), "t"),
                mode: TargetMode::DeleteAll,
            },
            primary_key: Vec::new(),
            conversion: ConversionOptions::default(),
            errors: ErrorPolicy::Skip { max_errors: 10 },
            transaction: TransactionPolicy::Batches { rows: 1000 },
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["target"]["mode"], "delete_all");
        assert_eq!(
            json["errors"],
            serde_json::json!({"type": "skip", "maxErrors": 10})
        );
        assert_eq!(
            json["transaction"],
            serde_json::json!({"type": "batches", "rows": 1000})
        );
        assert_eq!(json["options"]["format"], "csv");
        let back: ImportRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, request);

        // Defaults: append, stop, auto.
        let minimal = serde_json::json!({
            "options": {"format": "parquet"},
            "columns": [],
            "mapping": [],
            "target": {"session": session, "table": {"kind": "table", "name": "t"}}
        });
        let parsed: ImportRequest = serde_json::from_value(minimal).unwrap();
        assert_eq!(parsed.target.mode, TargetMode::Append);
        assert_eq!(parsed.errors, ErrorPolicy::Stop);
        assert_eq!(parsed.transaction, TransactionPolicy::Auto);
    }

    #[test]
    fn events_are_tagged() {
        let event = ImportEvent::Committed {
            rows_committed: 100_000,
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::json!({"type": "committed", "rowsCommitted": 100_000})
        );
        assert!(!event.is_terminal());
    }
}
