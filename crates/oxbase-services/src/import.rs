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

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use oxbase_dialect::{BulkMethod, DatabaseScope, DdlContext, DialectRegistry, LoadColumn};
use oxbase_io::{ColumnSpec, PreviewLimits, ReadOptions, SourcePreview, ValueFormats};
use oxbase_types::{
    ColumnDef, DataType, Engine, Error, ErrorCategory, ObjectDef, ObjectKind, ObjectRef,
    PrimaryKeyDef, Result, ScriptStep, ServerVersion, StepPhase, TableDef,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::query::ErrorInfo;
use crate::tree::TreeService;
use crate::{CancellationToken, ConnectionManager, SessionHandle, SessionId, check_cancelled};

mod convert;
mod run;

#[cfg(test)]
pub(crate) mod live_tests;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;

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
    convert::note(from, to)
}

/// Imports files into tables. See the module docs.
pub struct ImportService {
    manager: Arc<ConnectionManager>,
    tree: Arc<TreeService>,
    dialects: Arc<DialectRegistry>,
    config: ImportConfig,
    /// Cancel tokens of the running imports (shared with the run tasks, which remove theirs).
    running: Arc<Mutex<HashMap<ImportId, CancellationToken>>>,
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
            running: Arc::new(Mutex::new(HashMap::new())),
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
    pub async fn plan(
        &self,
        source: &ImportSource,
        request: &ImportRequest,
        cancel: CancellationToken,
    ) -> Result<ImportPlan> {
        Ok(self.prepare(source, request, &cancel).await?.plan)
    }

    /// Starts importing `source` per `request` (module docs, step 4) and returns once the plan
    /// checks passed and the import is registered; events follow on the handle. Fails before
    /// starting like [`ImportService::plan`].
    ///
    /// The file is read with `request.options` (the app builds `source.options` from the same
    /// request).
    pub async fn start(
        &self,
        source: ImportSource,
        request: ImportRequest,
    ) -> Result<ImportHandle> {
        let path = source.path.clone();
        let options = request.options.clone();
        let columns = request.columns.clone();
        let batch_rows = self.config.batch_rows;
        let open: run::Opener =
            Box::new(move || oxbase_io::open(&path, &options, &columns, batch_rows));
        self.start_with(&source, request, open).await
    }

    /// [`ImportService::start`] reading from `reader` instead of opening the file (tests:
    /// in-memory sources and slow sources for cancel). `source.path` still sizes the
    /// transaction policy.
    #[cfg(test)]
    pub(crate) async fn start_with_reader(
        &self,
        source: &ImportSource,
        request: ImportRequest,
        reader: Box<dyn oxbase_io::SourceReader>,
    ) -> Result<ImportHandle> {
        self.start_with(source, request, Box::new(move || Ok(reader)))
            .await
    }

    async fn start_with(
        &self,
        source: &ImportSource,
        request: ImportRequest,
        open: run::Opener,
    ) -> Result<ImportHandle> {
        let token = CancellationToken::new();
        let prepared = self.prepare(source, &request, &token).await?;
        let id = ImportId::new();
        let (events, receiver) = mpsc::channel(self.config.event_capacity.max(1));
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, token.clone());
        let job = run::Job {
            id,
            prepared,
            request,
            manager: Arc::clone(&self.manager),
            dialects: Arc::clone(&self.dialects),
            config: self.config.clone(),
            token,
            events,
            running: Arc::clone(&self.running),
        };
        tokio::spawn(job.run(open));
        Ok(ImportHandle {
            id,
            events: receiver,
        })
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

    /// The plan checks of the module docs (step 3), and everything a run needs.
    async fn prepare(
        &self,
        source: &ImportSource,
        request: &ImportRequest,
        cancel: &CancellationToken,
    ) -> Result<Prepared> {
        check_cancelled(cancel)?;
        request.options.validate()?;
        check_shape(request)?;
        let session = request.target.session;
        let base = self
            .manager
            .get(session)
            .ok_or_else(|| Error::not_found(format!("no open session {session}")))?;
        if base.summary().parent.is_some() {
            return Err(Error::invalid_input(
                "an import targets a base session, not a child session",
            ));
        }
        let engine = base.summary().engine;
        let version = base.info().server_version.clone();
        let table = &request.target.table;
        if table.kind != ObjectKind::Table || table.name.trim().is_empty() {
            return Err(Error::invalid_input(format!(
                "{} is not a table",
                table.display_name()
            )));
        }
        let total_bytes = file_size(&source.path)?;
        let existing = match self.tree.load(session, table, cancel.clone()).await {
            Ok(ObjectDef::Table(def)) => Some(def),
            Ok(other) => {
                return Err(Error::invalid_input(format!(
                    "{} is not a table",
                    other.object_ref().display_name()
                )));
            }
            Err(e) if e.category() == ErrorCategory::NotFound => None,
            Err(e) => return Err(e),
        };
        check_cancelled(cancel)?;

        let mode = request.target.mode;
        let creates = matches!(mode, TargetMode::Create | TargetMode::Replace);
        let mut warnings = Vec::new();
        let def = match mode {
            TargetMode::Create if existing.is_some() => {
                return Err(Error::invalid_input(format!(
                    "{} already exists; append to it or replace it",
                    table.display_name()
                )));
            }
            TargetMode::Create | TargetMode::Replace => new_table(request)?,
            TargetMode::Append | TargetMode::Truncate | TargetMode::DeleteAll => {
                existing.clone().ok_or_else(|| {
                    Error::not_found(format!(
                        "{} does not exist; create it with the create mode",
                        table.display_name()
                    ))
                })?
            }
        };
        if !creates && !request.primary_key.is_empty() {
            warnings.push(
                "the primary key applies only when the import creates the table; it is ignored"
                    .to_owned(),
            );
        }
        let (columns, planned) = map_columns(request, &def)?;
        check_required(&def, &columns)?;

        let ddl = self.target_ddl(engine, &version, mode, table, &def, existing.is_some())?;
        warnings.extend(ddl.warnings.iter().cloned());
        let ddl_in_tx = ddl
            .steps
            .iter()
            .all(|s| s.transactional && s.phase == StepPhase::Main);
        let method = self.dialects.bulk_loader(engine)?.method(&version);
        let commit_every = match request.transaction {
            TransactionPolicy::Single => None,
            TransactionPolicy::Batches { rows } => Some(rows.max(1)),
            TransactionPolicy::Auto => (total_bytes > self.config.single_transaction_max_bytes)
                .then_some(self.config.default_commit_rows.max(1)),
        };
        let existed = existing.is_some();
        let destructive = match mode {
            TargetMode::Truncate => !ddl_in_tx,
            TargetMode::Replace => existed && (!ddl_in_tx || commit_every.is_some()),
            TargetMode::Create | TargetMode::Append | TargetMode::DeleteAll => false,
        };
        if creates && !ddl_in_tx {
            warnings.push(format!(
                "{} commits each DDL statement at once: if the import fails or is cancelled, \
                 the table it created is dropped",
                engine.display_name()
            ));
        }
        if mode == TargetMode::Truncate && !ddl_in_tx {
            warnings.push(
                "TRUNCATE commits at once: if the import fails or is cancelled, the table stays \
                 empty"
                    .to_owned(),
            );
        }
        if mode == TargetMode::Replace && existed && destructive {
            warnings.push(
                "the old table is dropped for good before every row is in: if the import fails \
                 or is cancelled, it is gone"
                    .to_owned(),
            );
        }
        let transactional_storage = match (&existing, creates) {
            (Some(def), false) => match self.dialects.dml_writer(engine) {
                Ok(writer) => writer.transactional(def),
                Err(_) => true,
            },
            _ => true,
        };
        if !transactional_storage {
            warnings.push(
                "the table's storage does not support transactions: rows inserted before a \
                 failure or a cancel stay in the table"
                    .to_owned(),
            );
        }
        let plan = ImportPlan {
            table: table.clone(),
            mode,
            creates_table: creates,
            ddl: ddl.steps.iter().map(|s| s.sql.clone()).collect(),
            columns: planned,
            method,
            transaction: ResolvedTransaction { commit_every },
            destructive,
            warnings,
        };
        Ok(Prepared {
            plan,
            engine,
            version,
            ddl: ddl.steps,
            drops: ddl.drops,
            ddl_in_tx,
            drop_new: ddl.drop_new,
            columns,
            total_bytes,
            existed,
            transactional_storage,
        })
    }

    /// What runs before the rows go in, per target mode.
    fn target_ddl(
        &self,
        engine: Engine,
        version: &ServerVersion,
        mode: TargetMode,
        table: &ObjectRef,
        def: &TableDef,
        exists: bool,
    ) -> Result<TargetDdl> {
        let ctx = DdlContext::new(version);
        let mut out = TargetDdl::default();
        match mode {
            TargetMode::Append => {}
            TargetMode::Create | TargetMode::Replace => {
                let generator = self.dialects.ddl(engine, ObjectKind::Table)?;
                if mode == TargetMode::Replace && exists {
                    let drop = generator.drop_object(table, &ctx)?;
                    out.warnings.extend(drop.warnings.iter().cloned());
                    out.steps.extend(drop.ordered().cloned());
                    out.drops = out.steps.len();
                }
                let create = generator.create(&ObjectDef::Table(def.clone()), &ctx)?;
                out.warnings.extend(create.warnings.iter().cloned());
                out.steps.extend(create.ordered().cloned());
                out.drop_new = generator
                    .drop_object(table, &ctx)?
                    .ordered()
                    .cloned()
                    .collect();
            }
            TargetMode::Truncate | TargetMode::DeleteAll => {
                let quoting = self.dialects.quoting(engine)?;
                let qualified = quoting.qualify(table);
                // File databases (`Attached` scope) have no TRUNCATE statement.
                let attached = self
                    .dialects
                    .tree_layout(engine)
                    .is_ok_and(|layout| layout.scope == DatabaseScope::Attached);
                let step = if mode == TargetMode::Truncate && !attached {
                    let transactional_ddl = match self.dialects.capabilities(engine) {
                        Ok(caps) => caps.designer(version).transactional_ddl,
                        Err(_) => true,
                    };
                    let step = ScriptStep::new(format!("TRUNCATE TABLE {qualified}"))
                        .described(format!("Empty table {}", table.name))
                        .destructive();
                    if transactional_ddl {
                        step
                    } else {
                        step.non_transactional()
                    }
                } else {
                    ScriptStep::new(format!("DELETE FROM {qualified}"))
                        .described(format!("Delete every row of {}", table.name))
                        .destructive()
                };
                out.steps.push(step);
            }
        }
        Ok(out)
    }
}

impl fmt::Debug for ImportService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImportService")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// The DDL of a target mode.
#[derive(Debug, Default)]
struct TargetDdl {
    steps: Vec<ScriptStep>,
    /// The first `drops` steps drop the old table (replace).
    drops: usize,
    /// Drops the table the import creates (create / replace clean-up).
    drop_new: Vec<ScriptStep>,
    warnings: Vec<String>,
}

/// A checked request: the plan and what the run needs besides it.
#[derive(Debug, Clone)]
struct Prepared {
    plan: ImportPlan,
    engine: Engine,
    version: ServerVersion,
    /// DDL steps in execution order.
    ddl: Vec<ScriptStep>,
    /// The first `drops` steps of `ddl` drop the old table.
    drops: usize,
    /// Every DDL step can run inside the import's first transaction.
    ddl_in_tx: bool,
    /// Drops the table the import creates (clean-up after a failure).
    drop_new: Vec<ScriptStep>,
    /// Per load column: the source column index, its source type and the target column.
    columns: Vec<(usize, DataType, LoadColumn)>,
    total_bytes: u64,
    /// The target table existed when the plan was made.
    existed: bool,
    /// A rollback undoes inserted rows (`DmlWriter::transactional`).
    transactional_storage: bool,
}

/// The value formats a reader uses (text parsing in conversions).
fn value_formats(options: &ReadOptions) -> ValueFormats {
    match options {
        ReadOptions::Csv(o) => o.formats.clone(),
        ReadOptions::Json(o) => o.formats.clone(),
        ReadOptions::Xlsx(o) => o.formats.clone(),
        ReadOptions::Parquet(_) => ValueFormats::default(),
    }
}

/// The size of the file to import; errors name the file's base name only.
fn file_size(path: &Path) -> Result<u64> {
    std::fs::metadata(path)
        .as_ref()
        .map(std::fs::Metadata::len)
        .map_err(|e| Error::Io {
            kind: e.kind().to_string(),
            message: format!("{}: {e}", oxbase_io::file_label(path)),
        })
}

/// Checks that need no database: columns, mapping indexes, target names.
fn check_shape(request: &ImportRequest) -> Result<()> {
    if request.columns.is_empty() {
        return Err(Error::invalid_input("the file has no columns to import"));
    }
    for (pos, mapping) in request.mapping.iter().enumerate() {
        if mapping.source >= request.columns.len() {
            return Err(Error::invalid_input(format!(
                "mapping {} names source column {}, the file has {}",
                pos + 1,
                mapping.source + 1,
                request.columns.len()
            )));
        }
        if mapping
            .target
            .as_deref()
            .is_some_and(|t| t.trim().is_empty())
        {
            return Err(Error::invalid_input(format!(
                "source column '{}' is mapped to an empty column name",
                request.columns[mapping.source].name
            )));
        }
    }
    if !request.mapping.iter().any(|m| m.target.is_some()) {
        return Err(Error::invalid_input(
            "map at least one source column to a target column",
        ));
    }
    Ok(())
}

/// The table create / replace mode makes: one column per mapping (type from the mapping, else
/// the source's), `native_type` empty so the target dialect derives it, the primary key from
/// the request.
fn new_table(request: &ImportRequest) -> Result<TableDef> {
    let mut def = TableDef::new(request.target.table.clone());
    for mapping in &request.mapping {
        let Some(name) = mapping.target.as_deref() else {
            continue;
        };
        if def.column(name).is_some() {
            return Err(Error::invalid_input(format!(
                "column '{name}' is mapped more than once"
            )));
        }
        let data_type = mapping
            .data_type
            .clone()
            .unwrap_or_else(|| request.columns[mapping.source].data_type.clone());
        def.columns.push(ColumnDef::new(name, data_type));
    }
    if !request.primary_key.is_empty() {
        let mut seen = HashSet::new();
        for key in &request.primary_key {
            if !seen.insert(key.as_str()) {
                return Err(Error::invalid_input(format!(
                    "primary key column '{key}' is listed twice"
                )));
            }
            let column = def
                .columns
                .iter_mut()
                .find(|c| c.name == *key)
                .ok_or_else(|| {
                    Error::invalid_input(format!(
                        "primary key column '{key}' is not one of the imported columns"
                    ))
                })?;
            column.nullable = false;
        }
        def.primary_key = Some(PrimaryKeyDef {
            name: None,
            columns: request.primary_key.clone(),
        });
    }
    Ok(def)
}

/// The column of `def` named `name`: exactly, else the only one equal ignoring ASCII case.
fn find_column<'a>(def: &'a TableDef, name: &str) -> Option<&'a ColumnDef> {
    def.column(name).or_else(|| {
        let mut matches = def
            .columns
            .iter()
            .filter(|c| c.name.eq_ignore_ascii_case(name));
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    })
}

/// The load columns and plan columns of the mapping, checked against `def`.
#[allow(
    clippy::type_complexity,
    reason = "a private pair of the two views of the same columns"
)]
fn map_columns(
    request: &ImportRequest,
    def: &TableDef,
) -> Result<(Vec<(usize, DataType, LoadColumn)>, Vec<PlannedColumn>)> {
    let mut seen = HashSet::new();
    let mut columns = Vec::new();
    let mut planned = Vec::new();
    for mapping in &request.mapping {
        let Some(target) = mapping.target.as_deref() else {
            continue;
        };
        let spec = &request.columns[mapping.source];
        let column = find_column(def, target).ok_or_else(|| {
            Error::invalid_input(format!(
                "{} has no column '{target}'",
                def.obj.display_name()
            ))
        })?;
        if !seen.insert(column.name.clone()) {
            return Err(Error::invalid_input(format!(
                "column '{}' is mapped more than once",
                column.name
            )));
        }
        if column.generated.is_some() {
            return Err(Error::invalid_input(format!(
                "column '{}' is generated: its values cannot be imported",
                column.name
            )));
        }
        if column.identity.as_ref().is_some_and(|i| i.always) {
            return Err(Error::invalid_input(format!(
                "column '{}' is an identity column GENERATED ALWAYS: the server makes its values",
                column.name
            )));
        }
        let Ok(conversion) = conversion_note(&spec.data_type, &column.data_type) else {
            return Err(Error::invalid_input(format!(
                "column '{}': there is no conversion from {} (source column '{}') to {}",
                column.name, spec.data_type, spec.name, column.data_type
            )));
        };
        planned.push(PlannedColumn {
            source: mapping.source,
            source_name: spec.name.clone(),
            source_type: spec.data_type.clone(),
            target: column.name.clone(),
            target_type: column.data_type.clone(),
            conversion,
        });
        columns.push((
            mapping.source,
            spec.data_type.clone(),
            LoadColumn {
                name: column.name.clone(),
                data_type: column.data_type.clone(),
                native_type: column.native_type.clone(),
                nullable: column.nullable,
            },
        ));
    }
    if columns.is_empty() {
        return Err(Error::invalid_input(
            "map at least one source column to a target column",
        ));
    }
    Ok((columns, planned))
}

/// Every NOT NULL column without a default (or a generated value) must be mapped.
fn check_required(def: &TableDef, columns: &[(usize, DataType, LoadColumn)]) -> Result<()> {
    let missing: Vec<&str> = def
        .columns
        .iter()
        .filter(|c| {
            !c.nullable
                && c.default.is_none()
                && !c.auto_increment
                && c.identity.is_none()
                && c.generated.is_none()
                && !columns.iter().any(|(_, _, l)| l.name == c.name)
        })
        .map(|c| c.name.as_str())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Error::invalid_input(format!(
            "these NOT NULL columns have no default and no source column: {}",
            missing.join(", ")
        )))
    }
}

/// A session an import or export works on: a dedicated child of the base session (closed
/// afterwards), or the base itself when its children would not see its data (a private
/// in-memory database).
pub(crate) struct WorkSession {
    pub handle: SessionHandle,
    pub dedicated: bool,
}

/// Opens the [`WorkSession`] for base session `base` on `database` (ignored on engines whose
/// databases are all visible from one session).
pub(crate) async fn work_session(
    manager: &ConnectionManager,
    dialects: &DialectRegistry,
    base: SessionId,
    database: Option<&str>,
    cancel: &CancellationToken,
) -> Result<WorkSession> {
    let handle = manager
        .get(base)
        .ok_or_else(|| Error::not_found(format!("no open session {base}")))?;
    if !manager.children_share_data(base) {
        return Ok(WorkSession {
            handle,
            dedicated: false,
        });
    }
    let attached = dialects
        .tree_layout(handle.summary().engine)
        .is_ok_and(|layout| layout.scope == DatabaseScope::Attached);
    let database = if attached { None } else { database };
    let child = manager.open_child(base, database, cancel.clone()).await?;
    Ok(WorkSession {
        handle: child,
        dedicated: true,
    })
}

/// Closes a dedicated work session, waiting at most `wait` (the close goes on in the
/// background after that).
pub(crate) async fn close_work_session(
    manager: &Arc<ConnectionManager>,
    session: WorkSession,
    wait: Duration,
) {
    if !session.dedicated {
        return;
    }
    let manager = Arc::clone(manager);
    let id = session.handle.id();
    drop(session);
    let closing = tokio::spawn(async move {
        if let Err(e) = manager.close(id).await
            && e.category() != ErrorCategory::NotFound
        {
            tracing::debug!(session = %id, error = %e, "closing a work session failed");
        }
    });
    let _ = tokio::time::timeout(wait, closing).await;
}

#[cfg(test)]
mod shape_tests {
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
