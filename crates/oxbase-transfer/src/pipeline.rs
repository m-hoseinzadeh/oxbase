//! Planning and running a transfer on two connections (`docs/m8-contracts.md` §4.3-§4.5).
//! Owner of the bodies marked `unimplemented!("m8: transfer")`: the **transfer** stream.
//!
//! # Plan ([`plan`])
//!
//! 1. [`TransferSpec::validate`]; the target container must exist on the target connection.
//! 2. Per source object: load its definition through the source engine's fetchers (`Table`,
//!    `View`); for a `Query`, run it with a zero-row limit (`SELECT * FROM (<sql>) q WHERE 1 = 0`
//!    is **not** used: the query runs as typed with the driver's row limit 0 / first batch only)
//!    to read its Arrow schema; map every column ([`crate::mapping::map_column`]) and build the
//!    target table ([`crate::mapping::target_table`]).
//! 3. Per target table: does it exist (the target's `Table` fetcher)? Then check the mode:
//!    `create` refuses an existing table, `append` / `truncate` refuse a missing one and match
//!    columns by name; `replace` drops it. The DDL text (drop, create, truncate) comes from the
//!    target's `DdlGenerator` (`DdlContext::exact_values`) and its `drop_object`, rendered with
//!    `Script::to_sql`.
//! 4. Order: the foreign-key graph of the planned tables (`oxbase_dialect::table_dependencies`,
//!    using the **source** definitions), so parents load before children; cycles are reported
//!    and load in insertion order (their foreign keys come after the load anyway).
//! 5. `runnable` is false (with `blocked` reasons) when a column is `unsupported`, a mode check
//!    failed, or a table name collides under the target's case rules.
//!
//! Nothing is written during planning: [`plan`] is also the **dry run** (target DDL and the
//! mapping report).
//!
//! # Run ([`copy_table`], called per table by the services in plan order)
//!
//! - The target table's DDL runs first (inside the table's transaction when the engine has
//!   transactional DDL and `TransactionScope::PerTable`; MySQL / MariaDB DDL commits at once).
//! - The source rows stream from the source session (`SELECT` built with
//!   `oxbase_dialect::sql::select_table_data` and the object's filter, or the user's query) as
//!   Arrow batches; each is converted ([`crate::convert::BatchConverter`]), re-chunked to
//!   `TransferOptions::batch_rows` (and the loader's `rows_per_call`) and loaded with the
//!   target engine's `BulkLoader` (`prepare` / `load` / `finish`, identity and foreign-key
//!   hooks). Memory stays bounded: at most one source batch and one load batch per table.
//! - Commits follow `TransactionScope`; `ErrorMode::Continue` drops rows with conversion
//!   issues (reported) and goes on; a server error fails the table.
//! - Progress: `on_progress` at most every 250 ms and after each commit, with cumulative
//!   counters ([`TableProgress`]).
//! - Cancel: the token stops the source stream and the running load statement; the open
//!   transaction rolls back; the report says what stayed ([`TableReport::left_behind`]).
//!
//! After every table: the planned foreign keys ([`TablePlan::foreign_keys`]) run on the target
//! (services), then `BulkLoader::identity_resync` statements.

use oxbase_dialect::{BulkLoader, DialectRegistry, Fidelity, LoadColumn};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{BoundStatement, Engine, ObjectRef, Result, ServerVersion};
use serde::{Deserialize, Serialize};

use crate::convert::RowIssue;
use crate::mapping::ColumnPlan;
use crate::spec::{TableMode, TransferOptions, TransferSpec};

/// The plan of one target table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TablePlan {
    /// Index of the source object in `TransferSpec::objects`.
    pub object: usize,
    /// The source table or view (`None` for a query).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ObjectRef>,
    /// The target table.
    pub target: ObjectRef,
    /// The target table exists now.
    pub exists: bool,
    pub mode: TableMode,
    /// Every source column, in source order.
    pub columns: Vec<ColumnPlan>,
    /// The worst fidelity of the columns that are not skipped.
    pub fidelity: Fidelity,
    /// DDL run before the load (drop, create, truncate), as preview text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ddl: Vec<String>,
    /// Foreign keys added after every table is loaded, as preview text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreign_keys: Vec<String>,
    /// The table's rows, when the source can tell cheaply (catalog estimate; `None` else).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_rows: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// What a transfer will do ([`plan`]; also the dry run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferPlan {
    pub source_engine: Engine,
    pub target_engine: Engine,
    /// In `TransferSpec::objects` order.
    pub tables: Vec<TablePlan>,
    /// Load order: indexes into `tables`, parents before children.
    pub order: Vec<usize>,
    /// Groups of tables whose foreign keys form a cycle (indexes into `tables`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cycles: Vec<Vec<usize>>,
    /// The worst fidelity over every table.
    pub fidelity: Fidelity,
    /// Something is dropped or emptied (`replace`, `truncate`): the UI asks first.
    pub destructive: bool,
    /// The target runs DDL inside transactions (false on MySQL / MariaDB: a failed table can
    /// leave a created table behind; the services drop it).
    pub transactional_ddl: bool,
    /// The plan can run (no unsupported column, no failed mode check).
    pub runnable: bool,
    /// Why not, when `runnable` is false.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// The two connections a plan reads. Both are sessions on the right databases (the services
/// open them).
#[derive(Clone, Copy)]
pub struct PlanInputs<'a> {
    pub dialects: &'a DialectRegistry,
    pub source: &'a dyn Connection,
    pub target: &'a dyn Connection,
    pub spec: &'a TransferSpec,
}

/// Builds the plan (module docs, *Plan*). Reads catalogs only.
pub async fn plan(inputs: PlanInputs<'_>, cancel: CancellationToken) -> Result<TransferPlan> {
    let _ = (inputs.dialects, inputs.spec, cancel);
    // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
    std::future::ready(()).await;
    unimplemented!("m8: transfer")
}

/// Cumulative counters of one table (lossy progress: consumers must not rely on every event).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableProgress {
    /// Index into `TransferPlan::tables`.
    pub table: usize,
    pub rows_read: u64,
    pub rows_written: u64,
    pub rows_committed: u64,
    pub rows_skipped: u64,
    /// Bytes of the source batches read (Arrow memory size).
    pub bytes: u64,
    pub errors: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_rows: Option<u64>,
    pub elapsed_ms: u64,
}

/// How a table ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableStatus {
    Done,
    Failed,
    Cancelled,
    /// Not started (an earlier table failed with `ErrorMode::Stop`, or the transfer was
    /// cancelled before it).
    NotRun,
}

/// The outcome of one table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableReport {
    pub table: usize,
    pub target: ObjectRef,
    pub status: TableStatus,
    pub progress: TableProgress,
    /// Reported row issues (at most `TransferOptions::max_reported_errors`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<RowIssue>,
    /// More issues happened than the report keeps.
    #[serde(default)]
    pub issues_truncated: bool,
    /// Why it failed (the error message; never connection details).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What the target table holds now, in words ("rolled back: the table is as before",
    /// "3 of 5 batches committed (30 000 rows)", "the created table was dropped").
    pub left_behind: String,
}

/// The outcome of a whole transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferReport {
    /// In `TransferPlan::tables` order.
    pub tables: Vec<TableReport>,
    /// Foreign keys added after the load.
    pub foreign_keys_added: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub elapsed_ms: u64,
}

/// Everything [`copy_table`] needs for one table.
#[derive(Clone, Copy)]
pub struct CopyTable<'a> {
    pub plan: &'a TablePlan,
    /// The DDL steps to run first (from the plan's generator call; executed in order).
    pub ddl: &'a [BoundStatement],
    /// A dedicated source session.
    pub source: &'a dyn Connection,
    /// The rows to copy (`SELECT` with bound values).
    pub select: &'a BoundStatement,
    /// A dedicated target session (the loader's `prepare` changes session settings).
    pub target: &'a dyn Connection,
    pub target_version: &'a ServerVersion,
    pub loader: &'a dyn BulkLoader,
    /// The load columns (not skipped), in load order.
    pub columns: &'a [LoadColumn],
    pub options: &'a TransferOptions,
}

/// Copies one table (module docs, *Run*). Returns its report (also for a failed or cancelled
/// table: `Err` only when the report itself cannot be made, e.g. the target session is gone
/// before anything ran).
pub async fn copy_table(
    job: CopyTable<'_>,
    on_progress: &(dyn Fn(&TableProgress) + Send + Sync),
    cancel: CancellationToken,
) -> Result<TableReport> {
    let _ = (job.plan, job.options, on_progress, cancel);
    // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
    std::future::ready(()).await;
    unimplemented!("m8: transfer")
}
