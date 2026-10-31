//! Table data and grid editing (M3): open a table's rows in the grid with a filter and sort,
//! find out whether and how its rows can be edited, buffer edits in the UI, apply them in one
//! transaction, and read single rows for the form / JSON view.
//!
//! # Table data
//!
//! [`EditService::open_table_data`] loads the table's definition (the engine's `Table`
//! fetcher, through `TreeService::load`, so PostgreSQL reads it on a session of the table's
//! database), builds `SELECT * FROM t [WHERE ...] [ORDER BY ...] [LIMIT n]` with
//! `oxbase_dialect::sql::select_table_data` (filter values bound, never inlined) and starts it
//! as an ordinary query run ([`QueryService::run_bound`]) on the UI's editor id, with
//! `RunOptions::database` = the table's catalog and `record_history` off. Events, paging
//! (`query_page`), cancel and close are exactly the M2 run path. The run is remembered as a
//! *table-data source* (table ref, definition, request) for editing.
//!
//! # Which results are editable
//!
//! Only results of [`EditService::open_table_data`] (M3). [`EditService::begin`] answers with an
//! [`Editability`]: the key is the primary key, else the first unique index or constraint whose
//! columns are all `NOT NULL`, plain columns (no expressions) and not partial; the result's
//! columns must contain every key column (they do for `SELECT *`). Otherwise the grid is
//! read-only and `read_only_reason` says why ([`reasons`]). Generated columns and
//! `GENERATED ALWAYS` identity columns are read-only cells; views are read-only in M3.
//!
//! # Pending changes and apply
//!
//! The UI keeps the buffer ([`PendingChanges`]: cell edits of stored rows, new rows, deleted
//! rows) and sends it whole to [`EditService::apply`]:
//!
//! 1. **Validate everything first**, touching nothing: each edit's row exists and is not
//!    deleted, its columns exist and are editable, each [`CellInput`] parses for its column
//!    (`convert::parse_input`), key values are known and not NULL. Any problem: every invalid
//!    edit gets [`EditStatus::Failed`] with its error, the rest [`EditStatus::NotRun`],
//!    `committed = false`.
//! 2. **Plan** each edit with the engine's `DmlWriter` (key values of stored rows are read from
//!    the result store with `convert::cell_value`; rows changed or inserted by an earlier apply
//!    use the values that apply read back).
//! 3. **Run** on the run's own session (`QueryService::run_session`, holding
//!    `SessionHandle::exclusive`), in **one transaction** (`Connection::begin`), in buffer order.
//!    A write that does not affect exactly one row is a [`EditStatus::Conflict`]
//!    ("the row was changed or deleted by someone else"). The first failure or conflict rolls
//!    everything back: that edit gets its status and error, the ones that ran before it
//!    [`EditStatus::RolledBack`], the rest [`EditStatus::NotRun`].
//! 4. **Commit**, then report every edit [`EditStatus::Applied`] with its row read back
//!    (`RETURNING` or the writer's read-back query) as edit texts in result-column order. The
//!    service keeps these values as an overlay of the (immutable) result store: later edits,
//!    forms and key lookups of those rows use them. Inserted rows get row numbers after the
//!    stored rows (`row_count + n`), deleted rows are remembered and refuse later edits.
//!
//! Apply refuses with `InvalidInput` while the run is still running (the session is busy
//! streaming rows). The service forgets a run's state when `QueryService` no longer knows the
//! run (closed), checked on every call.
//!
//! # Values
//!
//! The grid edits **text**. [`CellInput::Text`] is parsed for the column's type by
//! `convert::parse_input` (grammar in `docs/m3-contracts.md` §5): `Null` is SQL NULL, an empty
//! text is an empty string (never NULL), `Default` omits the column from an `INSERT` (only
//! valid for new rows). Rows come back as text too (`convert::cell_text`), so what the UI shows
//! in an editor round-trips exactly.
//!
//! Owner of the bodies marked `unimplemented!("m3: services")`: the **services** stream.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use arrow_array::RecordBatch;
use oxbase_dialect::sql::select_table_data;
use oxbase_dialect::{DialectRegistry, DmlPlan, DmlTarget, DmlWriter};
use oxbase_driver::{Collected, Tx, collect};
use oxbase_types::{
    ColumnDef, ColumnValue, DataType, Engine, Error, ErrorCategory, Filter, KeyKind, ObjectDef,
    ObjectKind, ObjectRef, Result, RowEdit, RowKey, ServerVersion, TableDataQuery, TableDef, Value,
};
use serde::{Deserialize, Serialize};

use crate::convert::{JsonField, cell_text, cell_value, object_json, parse_input, value_text};
use crate::query::{
    ColumnInfo, ErrorInfo, QueryService, RunHandle, RunId, RunOptions, RunState, RunTarget,
};
use crate::tree::TreeService;
use crate::{CancellationToken, check_cancelled, until_cancelled};

/// Read-only reasons [`Editability::read_only_reason`] uses (the UI may match on them).
pub mod reasons {
    /// The result did not come from `open_table_data` (an editor query).
    pub const NOT_TABLE_DATA: &str =
        "only table data opened from the object tree can be edited (not query results)";
    /// The table has no primary key and no unique key on NOT NULL plain columns.
    pub const NO_KEY: &str =
        "the table has no primary key or NOT NULL unique key, so rows cannot be identified";
    /// The source is a view or materialized view.
    pub const VIEW: &str = "views are read-only";
    /// The result does not contain every key column.
    pub const KEY_NOT_IN_RESULT: &str = "the result does not include every key column";
    /// The engine registered no `DmlWriter`.
    pub const ENGINE: &str = "editing is not available for this database engine yet";
}

/// How the UI edits one column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum InputKind {
    Text,
    Integer,
    Decimal,
    Float,
    Boolean,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Uuid,
    Json,
    /// Hex (`0x...`, `\x...`) or `base64:...` text.
    Bytes,
    /// One of `labels`.
    Enum {
        labels: Vec<String>,
    },
    /// Engine-specific text (arrays, intervals, geometry, ...): sent as text, cast by the
    /// engine's DmlWriter.
    Other,
}

impl InputKind {
    /// The editor kind for a column type.
    pub fn for_type(data_type: &DataType) -> Self {
        match data_type {
            DataType::Bool => Self::Boolean,
            DataType::Int { .. } => Self::Integer,
            DataType::Float { .. } => Self::Float,
            DataType::Decimal { .. } => Self::Decimal,
            DataType::Char { .. } | DataType::VarChar { .. } | DataType::Text => Self::Text,
            DataType::Binary { .. } | DataType::VarBinary { .. } => Self::Bytes,
            DataType::Date => Self::Date,
            DataType::Time { .. } => Self::Time,
            DataType::Timestamp { .. } => Self::Timestamp,
            DataType::TimestampTz { .. } => Self::TimestampTz,
            DataType::Uuid => Self::Uuid,
            DataType::Json { .. } => Self::Json,
            DataType::Enum { labels } => Self::Enum {
                labels: labels.clone(),
            },
            DataType::Interval | DataType::Array { .. } | DataType::Other { .. } => Self::Other,
        }
    }
}

/// One result column as the editor sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditColumn {
    pub name: String,
    /// Position in the result (`SELECT *` order).
    pub index: usize,
    pub data_type: DataType,
    pub native_type: String,
    pub nullable: bool,
    /// The column has a default, identity or auto-increment (an insert may leave it out).
    pub has_default: bool,
    /// Part of the editing key.
    pub key: bool,
    /// Cells of this column can be changed.
    pub editable: bool,
    /// Why not, when `editable` is false ("generated column", "identity column", ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_reason: Option<String>,
    pub input: InputKind,
}

/// Whether and how a table-data result can be edited ([`EditService::begin`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Editability {
    pub run_id: RunId,
    /// Always 0 for table data (one result set).
    pub result_index: usize,
    pub table: ObjectRef,
    pub editable: bool,
    /// Why the grid is read-only (one of [`reasons`]), when `editable` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_reason: Option<String>,
    /// The key rows are matched by (core model, snake_case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<RowKey>,
    pub columns: Vec<EditColumn>,
}

/// What the user typed into a cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CellInput {
    /// SQL NULL.
    Null,
    /// The column's default: the column is left out of the `INSERT`. New rows only.
    Default,
    /// Text parsed for the column's type (an empty text is an empty string, not NULL).
    Text { text: String },
}

impl CellInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// One cell of a pending edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CellEdit {
    pub column: String,
    pub input: CellInput,
}

/// One pending edit. `row` is a grid row number: a row of the result store, or a row inserted
/// by an earlier apply (numbered after the stored rows).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum PendingEdit {
    /// Change cells of an existing row.
    Update { row: u64, cells: Vec<CellEdit> },
    /// A new row (columns not listed take their defaults).
    Insert { cells: Vec<CellEdit> },
    /// Delete an existing row.
    Delete { row: u64 },
}

/// The UI's buffer of unsaved edits, applied in order. At most one `Update` or `Delete` per
/// row: the helpers below merge (the UI mirrors these rules in TypeScript).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingChanges {
    pub edits: Vec<PendingEdit>,
}

impl PendingChanges {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    fn position(&self, wanted: u64) -> Option<usize> {
        self.edits.iter().position(|e| match e {
            PendingEdit::Update { row, .. } | PendingEdit::Delete { row } => *row == wanted,
            PendingEdit::Insert { .. } => false,
        })
    }

    /// Sets one cell of an existing row: replaces that column's earlier input in the row's
    /// update, or starts an update. `InvalidInput` when the row is pending deletion.
    pub fn set_cell(&mut self, row: u64, column: &str, input: CellInput) -> Result<()> {
        let edit = CellEdit {
            column: column.to_owned(),
            input,
        };
        let Some(pos) = self.position(row) else {
            self.edits.push(PendingEdit::Update {
                row,
                cells: vec![edit],
            });
            return Ok(());
        };
        match &mut self.edits[pos] {
            PendingEdit::Delete { .. } => Err(Error::invalid_input(format!(
                "row {row} is marked for deletion"
            ))),
            PendingEdit::Update { cells, .. } => {
                match cells.iter_mut().find(|cell| cell.column == column) {
                    Some(existing) => *existing = edit,
                    None => cells.push(edit),
                }
                Ok(())
            }
            PendingEdit::Insert { .. } => {
                Err(Error::internal("pending row lookup found an insert"))
            }
        }
    }

    /// Adds a new row and returns its index in [`PendingChanges::edits`].
    pub fn insert_row(&mut self, cells: Vec<CellEdit>) -> usize {
        self.edits.push(PendingEdit::Insert { cells });
        self.edits.len() - 1
    }

    /// Marks an existing row for deletion, dropping its pending update.
    pub fn delete_row(&mut self, row: u64) {
        if let Some(pos) = self.position(row) {
            self.edits[pos] = PendingEdit::Delete { row };
        } else {
            self.edits.push(PendingEdit::Delete { row });
        }
    }

    /// Forgets every pending edit of an existing row (update or delete).
    pub fn revert_row(&mut self, row: u64) {
        if let Some(pos) = self.position(row) {
            self.edits.remove(pos);
        }
    }
}

/// What happened to one pending edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EditStatus {
    /// Committed.
    Applied,
    /// The key matched no row or more than one row; everything was rolled back.
    Conflict,
    /// Invalid, or the server refused it; everything was rolled back (or nothing ran).
    Failed,
    /// It ran, but a later edit failed, so it was rolled back.
    RolledBack,
    /// It did not run (an earlier edit failed, or validation failed elsewhere).
    NotRun,
}

/// The outcome of one pending edit, in [`ApplyReport::outcomes`] at the edit's index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditOutcome {
    pub status: EditStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
    /// The grid row: the edited row, or the new row number of an applied insert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row: Option<u64>,
    /// The row as read back after an applied insert or update, as edit texts in result-column
    /// order (`None` entries are NULL). Absent for deletes and when the row could not be read
    /// back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<Option<String>>>,
    /// For example "the row was saved but could not be read back".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// The result of [`EditService::apply`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyReport {
    /// Everything was committed (all outcomes `applied`).
    pub committed: bool,
    /// One per pending edit, same order.
    pub outcomes: Vec<EditOutcome>,
}

/// One field of the form view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormField {
    pub column: String,
    /// The value as edit text; `None` is NULL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// One row for the form / JSON view ([`EditService::row_form`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormRow {
    pub row: u64,
    /// One per result column, in order.
    pub fields: Vec<FormField>,
    /// The row as a pretty-printed JSON object: numbers as JSON numbers (exact digits), JSON
    /// columns nested, bytes as `"0x..."`, temporal values as ISO 8601 strings, NULL as `null`.
    pub json: String,
}

/// Opens table data, reports editability, applies pending changes and reads form rows. See the
/// module docs.
pub struct EditService {
    query: Arc<QueryService>,
    tree: Arc<TreeService>,
    dialects: Arc<DialectRegistry>,
    /// Runs started by [`EditService::open_table_data`], with what editing them needs.
    sources: Mutex<HashMap<RunId, Arc<Source>>>,
}

/// A table-data run: the table, its definition and the values earlier applies wrote.
struct Source {
    table: ObjectRef,
    /// The definition the run was opened with; `None` for views.
    def: Option<TableDef>,
    engine: Engine,
    version: ServerVersion,
    /// Applies of one run take turns (each reads and updates the overlay).
    apply: tokio::sync::Mutex<()>,
    overlay: Mutex<Overlay>,
}

/// What earlier applies changed: the result store itself is immutable.
#[derive(Default)]
struct Overlay {
    /// Rows updated or inserted by an earlier apply, by grid row.
    rows: HashMap<u64, OverlayRow>,
    /// Rows deleted by an earlier apply.
    deleted: HashSet<u64>,
    /// Rows inserted so far (numbered after the stored rows).
    inserted: u64,
}

/// One row as an earlier apply left it, in result-column order.
#[derive(Debug, Clone, PartialEq)]
struct OverlayRow {
    values: Vec<Value>,
    texts: Vec<Option<String>>,
    /// `false` for a new row that could not be read back: its key is unknown, so it cannot be
    /// edited again before a reload.
    known: bool,
}

/// One validated, planned edit of an apply.
struct Prepared {
    /// The grid row of an update or delete.
    row: Option<u64>,
    kind: EditKind,
    /// The row before the edit (updates), in result-column order.
    before: Option<OverlayRow>,
    /// Columns the edit writes: result index and value.
    written: Vec<(usize, Value)>,
    plan: DmlPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Update,
    Insert,
    Delete,
}

/// What running one plan inside the transaction gave.
struct Executed {
    affected: u64,
    /// The row as returned or read back (first row of the batch), when there was one.
    row: Option<RecordBatch>,
}

/// Why an apply stopped inside the transaction.
struct Stop {
    index: usize,
    status: EditStatus,
    error: Error,
}

/// The message of a conflict: the write did not affect exactly one row.
fn conflict_error(affected: u64) -> Error {
    if affected == 0 {
        Error::not_found("no row matched its key: the row was changed or deleted by someone else")
    } else {
        Error::invalid_input(format!(
            "{affected} rows matched its key instead of one: the row was changed by someone \
             else, or its key is not unique"
        ))
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl EditService {
    pub fn new(
        query: Arc<QueryService>,
        tree: Arc<TreeService>,
        dialects: Arc<DialectRegistry>,
    ) -> Self {
        Self {
            query,
            tree,
            dialects,
            sources: Mutex::new(HashMap::new()),
        }
    }

    pub fn query(&self) -> &Arc<QueryService> {
        &self.query
    }

    pub fn tree(&self) -> &Arc<TreeService> {
        &self.tree
    }

    pub fn dialects(&self) -> &Arc<DialectRegistry> {
        &self.dialects
    }

    /// Starts reading `request.table`'s rows in editor `target.editor` (see the module docs).
    /// Returns once the run started, like `QueryService::run`. `InvalidInput` for a bad filter
    /// or sort, an unknown column, or an object that is not a table or view; `NotFound` when the
    /// table is gone.
    pub async fn open_table_data(
        &self,
        target: RunTarget,
        mut request: TableDataQuery,
        mut options: RunOptions,
        cancel: CancellationToken,
    ) -> Result<RunHandle> {
        self.prune();
        check_cancelled(&cancel)?;
        let base = self
            .query
            .manager()
            .get(target.session)
            .ok_or_else(|| Error::not_found(format!("no open session {}", target.session)))?;
        let engine = base.summary().engine;
        let version = base.info().server_version.clone();
        if !matches!(
            request.table.kind,
            ObjectKind::Table | ObjectKind::View | ObjectKind::MatView
        ) {
            return Err(Error::invalid_input(format!(
                "{} is not a table or view",
                request.table.display_name()
            )));
        }
        let quoting = self.dialects.quoting(engine)?;
        let loaded = self
            .tree
            .load(target.session, &request.table, cancel.clone())
            .await;
        let def = match loaded {
            Ok(ObjectDef::Table(table)) => Some(table),
            Err(e)
                if request.table.kind == ObjectKind::Table
                    || e.category() != ErrorCategory::Unsupported =>
            {
                return Err(e);
            }
            // A view (also one without a fetcher, which can still be read).
            Ok(ObjectDef::View(_)) | Err(_) => None,
            Ok(other) => {
                return Err(Error::invalid_input(format!(
                    "{} is not a table or view",
                    other.object_ref().display_name()
                )));
            }
        };
        if let (Some(filter), Some(table)) = (request.filter.as_mut(), def.as_ref()) {
            typed_filter_values(filter, table)?;
        }
        let statement = select_table_data(quoting.as_ref(), &request, def.as_ref())?;
        if let Some(catalog) = &request.table.catalog {
            options.database = Some(catalog.clone());
        }
        options.record_history = false;
        check_cancelled(&cancel)?;
        drop(cancel);
        let handle = self.query.run_bound(target, statement, options).await?;
        let source = Source {
            table: request.table,
            def,
            engine,
            version,
            apply: tokio::sync::Mutex::new(()),
            overlay: Mutex::new(Overlay::default()),
        };
        lock(&self.sources).insert(handle.id, Arc::new(source));
        Ok(handle)
    }

    /// Whether and how run `run`'s result can be edited. A run that is not table data is
    /// read-only with [`reasons::NOT_TABLE_DATA`]; `NotFound` for an unknown run.
    pub async fn begin(&self, run: RunId, cancel: CancellationToken) -> Result<Editability> {
        self.prune();
        check_cancelled(&cancel)?;
        drop(cancel);
        // Lets a run that was just started register its result columns first.
        tokio::task::yield_now().await;
        let info = self.query.info(run)?;
        let source = lock(&self.sources).get(&run).cloned();
        let Some(source) = source else {
            let columns = info
                .results
                .first()
                .map(|result| {
                    result
                        .columns
                        .iter()
                        .enumerate()
                        .map(|(index, c)| query_column(index, c))
                        .collect()
                })
                .unwrap_or_default();
            return Ok(Editability {
                run_id: run,
                result_index: 0,
                table: ObjectRef::root(),
                editable: false,
                read_only_reason: Some(reasons::NOT_TABLE_DATA.to_owned()),
                key: None,
                columns,
            });
        };
        Ok(self.analyse(run, &source))
    }

    /// Applies `changes` to run `run`'s table (module docs, *Pending changes and apply*).
    /// Fails as a whole (`Err`) only when nothing can be attempted: unknown run, read-only
    /// result, run still running, session lost before the transaction began. Per-edit problems
    /// are in the report.
    pub async fn apply(
        &self,
        run: RunId,
        changes: &PendingChanges,
        cancel: CancellationToken,
    ) -> Result<ApplyReport> {
        self.prune();
        check_cancelled(&cancel)?;
        let source = self.source(run)?;
        let _turn = until_cancelled(&cancel, async { Ok(source.apply.lock().await) }).await?;
        match self.query.run_state(run) {
            None => return Err(Error::not_found(format!("no open query run {run}"))),
            Some(RunState::Running) => {
                return Err(Error::invalid_input(
                    "the rows are still loading; wait for them (or cancel the query) before \
                     saving changes",
                ));
            }
            Some(_) => {}
        }
        let editability = self.analyse(run, &source);
        if !editability.editable {
            return Err(Error::invalid_input(
                editability
                    .read_only_reason
                    .unwrap_or_else(|| reasons::NO_KEY.to_owned()),
            ));
        }
        let (Some(def), Some(key)) = (source.def.as_ref(), editability.key.as_ref()) else {
            return Err(Error::internal("an editable result has no table key"));
        };
        let stored_rows = self
            .query
            .info(run)?
            .results
            .first()
            .map(|r| r.rows)
            .ok_or_else(|| Error::invalid_input("the table data has no result to edit"))?;
        let writer = self.dialects.dml_writer(source.engine)?;
        let names: Vec<String> = editability.columns.iter().map(|c| c.name.clone()).collect();

        // 1-2. Validate and plan everything before touching the database.
        let context = PrepareContext {
            run,
            source: &source,
            def,
            key,
            columns: &editability.columns,
            names: &names,
            stored_rows,
            writer: writer.as_ref(),
        };
        let mut prepared = Vec::with_capacity(changes.edits.len());
        let mut invalid: Vec<Option<Error>> = Vec::with_capacity(changes.edits.len());
        let mut seen_rows = HashSet::new();
        for edit in &changes.edits {
            match self.prepare(&context, edit, &mut seen_rows).await {
                Ok(p) => {
                    prepared.push(p);
                    invalid.push(None);
                }
                Err(e) => invalid.push(Some(e)),
            }
        }
        if invalid.iter().any(Option::is_some) {
            let outcomes = changes
                .edits
                .iter()
                .zip(invalid)
                .map(|(edit, error)| match error {
                    Some(e) => outcome(EditStatus::Failed, Some(&e), edit_row(edit)),
                    None => outcome(EditStatus::NotRun, None, edit_row(edit)),
                })
                .collect();
            return Ok(ApplyReport {
                committed: false,
                outcomes,
            });
        }
        if prepared.is_empty() {
            return Ok(ApplyReport {
                committed: true,
                outcomes: Vec::new(),
            });
        }

        // 3. Run in one transaction on the run's own session.
        let session = self.query.run_session(run)?;
        let guard = until_cancelled(&cancel, async { Ok(session.exclusive().await) }).await?;
        let tx = session.connection().begin().await?;
        let mut executed: Vec<Executed> = Vec::with_capacity(prepared.len());
        let mut stop: Option<Stop> = None;
        for (index, p) in prepared.iter().enumerate() {
            if cancel.is_cancelled() {
                stop = Some(Stop {
                    index,
                    status: EditStatus::Failed,
                    error: Error::Cancelled,
                });
                break;
            }
            match execute_plan(tx.as_ref(), &p.plan, &cancel).await {
                Ok(done) if done.affected == 1 => executed.push(done),
                Ok(done) => {
                    stop = Some(Stop {
                        index,
                        status: EditStatus::Conflict,
                        error: conflict_error(done.affected),
                    });
                    break;
                }
                Err(error) => {
                    stop = Some(Stop {
                        index,
                        status: EditStatus::Failed,
                        error,
                    });
                    break;
                }
            }
        }
        if let Some(stop) = stop {
            if let Err(e) = tx.rollback().await {
                tracing::debug!(category = %e.category(), "rolling back table edits failed");
            }
            drop(guard);
            return Ok(rolled_back(&prepared, &stop));
        }
        if let Err(error) = tx.commit().await {
            // The server refused the commit: nothing was kept.
            drop(guard);
            let last = prepared.len() - 1;
            let error = Error::sql(
                None::<String>,
                format!("saving the changes failed: {error}"),
            );
            let outcomes = prepared
                .iter()
                .enumerate()
                .map(|(index, p)| {
                    if index == last {
                        outcome(EditStatus::Failed, Some(&error), p.row)
                    } else {
                        outcome(EditStatus::RolledBack, None, p.row)
                    }
                })
                .collect();
            return Ok(ApplyReport {
                committed: false,
                outcomes,
            });
        }
        drop(guard);

        // 4. Committed: remember what the rows look like now.
        let mut overlay = lock(&source.overlay);
        let outcomes = prepared
            .into_iter()
            .zip(executed)
            .map(|(p, done)| record_applied(&mut overlay, def, &names, stored_rows, p, done))
            .collect();
        Ok(ApplyReport {
            committed: true,
            outcomes,
        })
    }

    /// Row `row` of result `result_index` of `run` for the form / JSON view, with the values an
    /// earlier apply read back. Works for any run (read-only results too). `NotFound` for an
    /// unknown run, result or row, or a deleted row.
    pub async fn row_form(&self, run: RunId, result_index: usize, row: u64) -> Result<FormRow> {
        self.prune();
        let info = self.query.info(run)?;
        let result = info.results.get(result_index).ok_or_else(|| {
            Error::not_found(format!("query run {run} has no result {result_index}"))
        })?;
        let source = lock(&self.sources).get(&run).cloned();
        let def = source.as_ref().and_then(|s| s.def.as_ref());
        let names: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
        let column_def = |name: &str| def.and_then(|d| d.column(name));
        if let Some(source) = source.as_ref().filter(|_| result_index == 0) {
            let overlay = lock(&source.overlay);
            if overlay.deleted.contains(&row) {
                return Err(Error::not_found(format!("row {row} was deleted")));
            }
            if let Some(stored) = overlay.rows.get(&row) {
                let fields = names
                    .iter()
                    .zip(&stored.texts)
                    .map(|(name, text)| FormField {
                        column: name.clone(),
                        text: text.clone(),
                    })
                    .collect();
                let json = object_json(
                    &names
                        .iter()
                        .zip(&stored.values)
                        .map(|(name, value)| JsonField {
                            name: name.clone(),
                            value: value.clone(),
                            is_json: column_def(name)
                                .is_some_and(|c| matches!(c.data_type, DataType::Json { .. })),
                        })
                        .collect::<Vec<_>>(),
                );
                return Ok(FormRow { row, fields, json });
            }
        }
        if row >= result.rows {
            return Err(Error::not_found(format!(
                "result {result_index} of query run {run} has no row {row}"
            )));
        }
        let batch = self.query.page(run, result_index, row, 1).await?;
        if batch.num_rows() == 0 {
            return Err(Error::not_found(format!(
                "result {result_index} of query run {run} has no row {row}"
            )));
        }
        let schema = batch.schema();
        let mut fields = Vec::with_capacity(batch.num_columns());
        let mut json_fields = Vec::with_capacity(batch.num_columns());
        for (field, array) in schema.fields().iter().zip(batch.columns()) {
            let column = column_def(field.name());
            fields.push(FormField {
                column: field.name().clone(),
                text: cell_text(array.as_ref(), 0)?,
            });
            let is_json = column.is_some_and(|c| matches!(c.data_type, DataType::Json { .. }))
                || field
                    .metadata()
                    .get("ARROW:extension:name")
                    .is_some_and(|name| name == "arrow.json");
            json_fields.push(JsonField {
                name: field.name().clone(),
                value: cell_value(array.as_ref(), 0, column)?,
                is_json,
            });
        }
        Ok(FormRow {
            row,
            fields,
            json: object_json(&json_fields),
        })
    }

    /// Forgets the sources of runs the query service no longer knows (closed).
    fn prune(&self) {
        lock(&self.sources).retain(|run, _| self.query.run_state(*run).is_some());
    }

    /// The table-data source of `run`: `NotFound` for an unknown run, `InvalidInput` for a run
    /// that is not table data.
    fn source(&self, run: RunId) -> Result<Arc<Source>> {
        if let Some(source) = lock(&self.sources).get(&run) {
            return Ok(Arc::clone(source));
        }
        if self.query.run_state(run).is_some() {
            Err(Error::invalid_input(reasons::NOT_TABLE_DATA))
        } else {
            Err(Error::not_found(format!("no open query run {run}")))
        }
    }

    /// The editability of a table-data run (see the module docs).
    fn analyse(&self, run: RunId, source: &Source) -> Editability {
        let result_columns: Option<Vec<String>> = self.query.info(run).ok().and_then(|info| {
            info.results
                .first()
                .map(|r| r.columns.iter().map(|c| c.name.clone()).collect())
        });
        let read_only = |reason: &str, key: Option<RowKey>, columns: Vec<EditColumn>| Editability {
            run_id: run,
            result_index: 0,
            table: source.table.clone(),
            editable: false,
            read_only_reason: Some(reason.to_owned()),
            key,
            columns,
        };
        let Some(def) = &source.def else {
            let columns = result_columns
                .unwrap_or_default()
                .iter()
                .enumerate()
                .map(|(index, name)| EditColumn {
                    name: name.clone(),
                    index,
                    data_type: DataType::Other {
                        name: String::new(),
                    },
                    native_type: String::new(),
                    nullable: true,
                    has_default: false,
                    key: false,
                    editable: false,
                    read_only_reason: None,
                    input: InputKind::Other,
                })
                .collect();
            return read_only(reasons::VIEW, None, columns);
        };
        let names = result_columns.unwrap_or_else(|| {
            def.columns
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>()
        });
        let key = find_key(def, &source.table);
        let reason = if self.dialects.dml_writer(source.engine).is_err() {
            Some(reasons::ENGINE)
        } else {
            match &key {
                None => Some(reasons::NO_KEY),
                Some(k) if !k.columns.iter().all(|c| names.contains(c)) => {
                    Some(reasons::KEY_NOT_IN_RESULT)
                }
                Some(_) => None,
            }
        };
        let editable = reason.is_none();
        let columns = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let in_key = key.as_ref().is_some_and(|k| k.columns.contains(name));
                edit_column(index, name, def.column(name), in_key, editable)
            })
            .collect();
        match reason {
            Some(reason) => read_only(reason, key, columns),
            None => Editability {
                run_id: run,
                result_index: 0,
                table: source.table.clone(),
                editable: true,
                read_only_reason: None,
                key,
                columns,
            },
        }
    }

    /// The row `row` as it is now: from an earlier apply, or from the result store.
    async fn current_row(&self, context: &PrepareContext<'_>, row: u64) -> Result<OverlayRow> {
        {
            let overlay = lock(&context.source.overlay);
            if overlay.deleted.contains(&row) {
                return Err(Error::invalid_input(format!("row {row} was deleted")));
            }
            if let Some(stored) = overlay.rows.get(&row) {
                if !stored.known {
                    return Err(Error::invalid_input(format!(
                        "row {row} was added but could not be read back; reload the table data \
                         to edit it"
                    )));
                }
                return Ok(stored.clone());
            }
        }
        if row >= context.stored_rows {
            return Err(Error::invalid_input(format!("there is no row {row}")));
        }
        let batch = self.query.page(context.run, 0, row, 1).await?;
        if batch.num_rows() == 0 {
            return Err(Error::invalid_input(format!("there is no row {row}")));
        }
        Ok(row_of(&batch, context.def, context.names))
    }

    /// Validates and plans one pending edit (nothing touches the database).
    async fn prepare(
        &self,
        context: &PrepareContext<'_>,
        edit: &PendingEdit,
        seen_rows: &mut HashSet<u64>,
    ) -> Result<Prepared> {
        let (row, kind, cells): (Option<u64>, EditKind, &[CellEdit]) = match edit {
            PendingEdit::Update { row, cells } => (Some(*row), EditKind::Update, cells.as_slice()),
            PendingEdit::Insert { cells } => (None, EditKind::Insert, cells.as_slice()),
            PendingEdit::Delete { row } => (Some(*row), EditKind::Delete, <&[CellEdit]>::default()),
        };
        let mut written: Vec<(usize, Value)> = Vec::with_capacity(cells.len());
        for cell in cells {
            let column = context
                .columns
                .iter()
                .find(|c| c.name == cell.column)
                .ok_or_else(|| {
                    Error::invalid_input(format!("the table has no column '{}'", cell.column))
                })?;
            if !column.editable {
                return Err(Error::invalid_input(format!(
                    "column '{}' is read-only{}",
                    column.name,
                    column
                        .read_only_reason
                        .as_deref()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default()
                )));
            }
            if written.iter().any(|(index, _)| *index == column.index) {
                return Err(Error::invalid_input(format!(
                    "column '{}' is changed twice",
                    column.name
                )));
            }
            if kind == EditKind::Update && cell.input == CellInput::Default {
                return Err(Error::invalid_input(format!(
                    "column '{}': DEFAULT can only be used for new rows",
                    column.name
                )));
            }
            let def = context.def.column(&column.name).ok_or_else(|| {
                Error::invalid_input(format!("the table has no column '{}'", column.name))
            })?;
            if let Some(value) = parse_input(&cell.input, def)? {
                written.push((column.index, value));
            }
        }
        if let Some(row) = row
            && !seen_rows.insert(row)
        {
            return Err(Error::invalid_input(format!(
                "row {row} has more than one pending change"
            )));
        }
        let before = match row {
            Some(row) => Some(self.current_row(context, row).await?),
            None => None,
        };
        let changes = || {
            written
                .iter()
                .map(|(index, value)| {
                    ColumnValue::new(context.names[*index].clone(), value.clone())
                })
                .collect::<Vec<_>>()
        };
        let key_values = |row: &OverlayRow| -> Result<Vec<Value>> {
            context
                .key
                .columns
                .iter()
                .map(|key_column| {
                    let index = context
                        .names
                        .iter()
                        .position(|n| n == key_column)
                        .ok_or_else(|| Error::invalid_input(reasons::KEY_NOT_IN_RESULT))?;
                    match row.values.get(index) {
                        Some(Value::Null) | None => Err(Error::invalid_input(format!(
                            "key column '{key_column}' of the row is NULL"
                        ))),
                        Some(value) => Ok(value.clone()),
                    }
                })
                .collect()
        };
        let row_edit = match (kind, &before) {
            (EditKind::Update, Some(before)) => {
                if written.is_empty() {
                    return Err(Error::invalid_input("the change has no cells"));
                }
                RowEdit::Update {
                    key: key_values(before)?,
                    changes: changes(),
                }
            }
            (EditKind::Delete, Some(before)) => RowEdit::Delete {
                key: key_values(before)?,
            },
            (EditKind::Insert, _) => {
                for column in &context.def.columns {
                    let given = written
                        .iter()
                        .any(|(index, _)| context.names[*index] == column.name);
                    if !given && !column.nullable && !has_default(column) {
                        return Err(Error::invalid_input(format!(
                            "column '{}' is NOT NULL and has no default: give it a value",
                            column.name
                        )));
                    }
                }
                RowEdit::Insert { values: changes() }
            }
            _ => return Err(Error::internal("an update or delete without its row")),
        };
        context.key.validate(&row_edit)?;
        let target = DmlTarget {
            table: context.def,
            key: context.key,
            version: &context.source.version,
            read_back: context.names,
        };
        let plan = context.writer.plan(&row_edit, &target)?;
        Ok(Prepared {
            row,
            kind,
            before,
            written,
            plan,
        })
    }
}

/// What preparing the edits of one apply needs.
struct PrepareContext<'a> {
    run: RunId,
    source: &'a Source,
    def: &'a TableDef,
    key: &'a RowKey,
    columns: &'a [EditColumn],
    /// Result column names, in order.
    names: &'a [String],
    stored_rows: u64,
    writer: &'a dyn DmlWriter,
}

/// The grid row an edit names (updates and deletes).
fn edit_row(edit: &PendingEdit) -> Option<u64> {
    match edit {
        PendingEdit::Update { row, .. } | PendingEdit::Delete { row } => Some(*row),
        PendingEdit::Insert { .. } => None,
    }
}

fn outcome(status: EditStatus, error: Option<&Error>, row: Option<u64>) -> EditOutcome {
    EditOutcome {
        status,
        error: error.map(ErrorInfo::from),
        row,
        values: None,
        warning: None,
    }
}

/// The report of an apply that stopped at `stop` and rolled back.
fn rolled_back(prepared: &[Prepared], stop: &Stop) -> ApplyReport {
    let outcomes = prepared
        .iter()
        .enumerate()
        .map(|(index, p)| match index.cmp(&stop.index) {
            std::cmp::Ordering::Less => outcome(EditStatus::RolledBack, None, p.row),
            std::cmp::Ordering::Equal => outcome(stop.status, Some(&stop.error), p.row),
            std::cmp::Ordering::Greater => outcome(EditStatus::NotRun, None, p.row),
        })
        .collect();
    ApplyReport {
        committed: false,
        outcomes,
    }
}

/// Runs one plan inside the transaction: the write, then its read-back.
async fn execute_plan(tx: &dyn Tx, plan: &DmlPlan, cancel: &CancellationToken) -> Result<Executed> {
    let stream = tx
        .execute(&plan.write.sql, &plan.write.params, cancel.clone())
        .await?;
    let written = collect(stream).await?;
    let affected: u64 = if plan.returning {
        written
            .result_sets
            .iter()
            .map(|set| u64::try_from(set.num_rows()).unwrap_or(u64::MAX))
            .sum()
    } else {
        written.rows_affected.iter().sum()
    };
    let mut row = if plan.returning {
        first_row(&written)
    } else {
        None
    };
    if affected == 1
        && !plan.returning
        && let Some(read_back) = &plan.read_back
    {
        let stream = tx
            .execute(&read_back.sql, &read_back.params, cancel.clone())
            .await?;
        row = first_row(&collect(stream).await?);
    }
    Ok(Executed { affected, row })
}

/// The first row of the first result set, as a one-row batch.
fn first_row(collected: &Collected) -> Option<RecordBatch> {
    collected
        .result_sets
        .first()?
        .batches
        .iter()
        .find(|batch| batch.num_rows() > 0)
        .map(|batch| batch.slice(0, 1))
}

/// Row 0 of `batch` in result-column order (`names`), matched by column name.
fn row_of(batch: &RecordBatch, def: &TableDef, names: &[String]) -> OverlayRow {
    let schema = batch.schema();
    let mut values = Vec::with_capacity(names.len());
    let mut texts = Vec::with_capacity(names.len());
    let mut known = true;
    for name in names {
        let cell = schema.index_of(name).ok().and_then(|index| {
            let array = batch.column(index);
            let value = cell_value(array.as_ref(), 0, def.column(name)).ok()?;
            let text = cell_text(array.as_ref(), 0).ok()?;
            Some((value, text))
        });
        if let Some((value, text)) = cell {
            values.push(value);
            texts.push(text);
        } else {
            known = false;
            values.push(Value::Null);
            texts.push(None);
        }
    }
    OverlayRow {
        values,
        texts,
        known,
    }
}

/// Records an applied edit in the overlay and reports it.
fn record_applied(
    overlay: &mut Overlay,
    def: &TableDef,
    names: &[String],
    stored_rows: u64,
    prepared: Prepared,
    done: Executed,
) -> EditOutcome {
    let read = done.row.map(|batch| row_of(&batch, def, names));
    let mut report = outcome(EditStatus::Applied, None, prepared.row);
    match prepared.kind {
        EditKind::Delete => {
            if let Some(row) = prepared.row {
                overlay.rows.remove(&row);
                overlay.deleted.insert(row);
            }
        }
        EditKind::Update | EditKind::Insert => {
            let row = match prepared.row {
                Some(row) => row,
                None => {
                    let row = stored_rows.saturating_add(overlay.inserted);
                    overlay.inserted += 1;
                    row
                }
            };
            report.row = Some(row);
            let stored = match read {
                Some(read) => {
                    report.values = Some(read.texts.clone());
                    read
                }
                None => {
                    report.warning = Some(
                        "the row was saved but could not be read back; it shows the values as \
                         entered until the data is reloaded"
                            .to_owned(),
                    );
                    typed_row(prepared.before, &prepared.written, names.len())
                }
            };
            overlay.rows.insert(row, stored);
        }
    }
    report
}

/// The row an edit wrote when it could not be read back: the row before (an update, still
/// identifiable) or only the typed values (an insert, whose key is unknown).
fn typed_row(before: Option<OverlayRow>, written: &[(usize, Value)], width: usize) -> OverlayRow {
    let mut row = before.unwrap_or_else(|| OverlayRow {
        values: vec![Value::Null; width],
        texts: vec![None; width],
        known: false,
    });
    for (index, value) in written {
        if let (Some(slot), Some(text)) = (row.values.get_mut(*index), row.texts.get_mut(*index)) {
            *text = value_text(value);
            *slot = value.clone();
        }
    }
    row
}

/// The column has a default, identity, auto-increment or generation expression.
fn has_default(column: &ColumnDef) -> bool {
    column.default.is_some()
        || column.auto_increment
        || column.identity.is_some()
        || column.generated.is_some()
}

/// The editing key of `def`: the primary key, else the first unique index or constraint on
/// NOT NULL plain columns without a predicate.
fn find_key(def: &TableDef, table: &ObjectRef) -> Option<RowKey> {
    if let Some(pk) = def.primary_key.as_ref().filter(|pk| !pk.columns.is_empty()) {
        return Some(RowKey::new(
            table.clone(),
            KeyKind::Primary,
            pk.columns.clone(),
        ));
    }
    def.indexes
        .iter()
        .find(|index| {
            index.unique
                && index.predicate.is_none()
                && !index.columns.is_empty()
                && index.columns.iter().all(|part| {
                    !part.is_expression
                        && def
                            .column(&part.name)
                            .is_some_and(|c| !c.nullable && c.generated.is_none())
                })
        })
        .map(|index| {
            RowKey::new(
                table.clone(),
                KeyKind::Unique,
                index.columns.iter().map(|part| part.name.clone()).collect(),
            )
        })
}

/// One result column of a table-data run.
fn edit_column(
    index: usize,
    name: &str,
    def: Option<&ColumnDef>,
    key: bool,
    grid_editable: bool,
) -> EditColumn {
    let Some(def) = def else {
        return EditColumn {
            name: name.to_owned(),
            index,
            data_type: DataType::Other {
                name: String::new(),
            },
            native_type: String::new(),
            nullable: true,
            has_default: false,
            key,
            editable: false,
            read_only_reason: Some("not a column of the table".to_owned()),
            input: InputKind::Other,
        };
    };
    let reason = if def.generated.is_some() {
        Some("generated column")
    } else if def
        .identity
        .as_ref()
        .is_some_and(|identity| identity.always)
    {
        Some("identity column (GENERATED ALWAYS)")
    } else {
        None
    };
    EditColumn {
        name: name.to_owned(),
        index,
        data_type: def.data_type.clone(),
        native_type: def.native_type.clone(),
        nullable: def.nullable,
        has_default: has_default(def),
        key,
        editable: grid_editable && reason.is_none(),
        read_only_reason: reason.map(ToOwned::to_owned),
        input: InputKind::for_type(&def.data_type),
    }
}

/// A read-only column of a result that is not table data.
fn query_column(index: usize, column: &ColumnInfo) -> EditColumn {
    EditColumn {
        name: column.name.clone(),
        index,
        data_type: DataType::Other {
            name: column.data_type.clone(),
        },
        native_type: String::new(),
        nullable: column.nullable,
        has_default: false,
        key: false,
        editable: false,
        read_only_reason: None,
        input: InputKind::Other,
    }
}

/// `true` for column types whose filter values are converted from typed text before binding.
fn converts_filter_values(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Bool
            | DataType::Int { .. }
            | DataType::Float { .. }
            | DataType::Decimal { .. }
            | DataType::Date
            | DataType::Time { .. }
            | DataType::Timestamp { .. }
            | DataType::TimestampTz { .. }
            | DataType::Uuid
            | DataType::Binary { .. }
            | DataType::VarBinary { .. }
    )
}

/// Converts the text values the UI typed into a comparison filter to the column's type
/// (`convert::parse_input`), so they bind with the right type. Pattern operators keep text.
fn typed_filter_values(filter: &mut Filter, table: &TableDef) -> Result<()> {
    match filter {
        Filter::And { items } | Filter::Or { items } => items
            .iter_mut()
            .try_for_each(|item| typed_filter_values(item, table)),
        Filter::Not { item } => typed_filter_values(item, table),
        Filter::Condition { column, op, values } => {
            let Some(def) = table.column(column) else {
                // Unknown columns are reported by the SELECT builder.
                return Ok(());
            };
            if op.is_pattern() || !converts_filter_values(&def.data_type) {
                return Ok(());
            }
            let mut as_value = def.clone();
            as_value.nullable = true;
            for value in values.iter_mut() {
                if let Value::Text(text) = value {
                    let input = CellInput::text(text.clone());
                    if let Some(typed) = parse_input(&input, &as_value)? {
                        *value = typed;
                    }
                }
            }
            Ok(())
        }
        Filter::RawSql { .. } => Ok(()),
    }
}

impl std::fmt::Debug for EditService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditService").finish_non_exhaustive()
    }
}

#[cfg(test)]
pub(crate) mod live_tests;
#[cfg(test)]
mod service_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn pending_changes_merge_per_row() {
        let mut changes = PendingChanges::new();
        assert_eq!(changes.edits, []);
        changes.set_cell(3, "name", CellInput::text("a")).unwrap();
        changes.set_cell(3, "name", CellInput::text("b")).unwrap();
        changes.set_cell(3, "note", CellInput::Null).unwrap();
        let new_row = changes.insert_row(vec![CellEdit {
            column: "name".into(),
            input: CellInput::Default,
        }]);
        assert_eq!(new_row, 1);
        changes.delete_row(7);
        assert_eq!(changes.edits.len(), 3);
        assert_eq!(
            changes.edits[0],
            PendingEdit::Update {
                row: 3,
                cells: vec![
                    CellEdit {
                        column: "name".into(),
                        input: CellInput::text("b")
                    },
                    CellEdit {
                        column: "note".into(),
                        input: CellInput::Null
                    },
                ]
            }
        );
        // Deleting a row replaces its update; editing a deleted row is refused.
        changes.delete_row(3);
        assert_eq!(changes.edits[0], PendingEdit::Delete { row: 3 });
        assert!(changes.set_cell(3, "name", CellInput::text("c")).is_err());
        changes.revert_row(3);
        changes.revert_row(7);
        assert_eq!(changes.edits.len(), 1);
        assert!(matches!(changes.edits[0], PendingEdit::Insert { .. }));
    }

    #[test]
    fn payloads_are_camel_case() {
        let changes: PendingChanges = serde_json::from_value(serde_json::json!({
            "edits": [
                {"op": "update", "row": 4, "cells": [
                    {"column": "a", "input": {"type": "text", "text": ""}},
                    {"column": "b", "input": {"type": "null"}}
                ]},
                {"op": "insert", "cells": [{"column": "c", "input": {"type": "default"}}]},
                {"op": "delete", "row": 9}
            ]
        }))
        .unwrap();
        assert_eq!(changes.edits.len(), 3);
        assert_eq!(changes.edits[2], PendingEdit::Delete { row: 9 });

        let outcome = EditOutcome {
            status: EditStatus::RolledBack,
            error: None,
            row: Some(4),
            values: Some(vec![Some("1".into()), None]),
            warning: None,
        };
        let json = serde_json::to_value(&outcome).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"status": "rolledBack", "row": 4, "values": ["1", null]})
        );
        assert_eq!(
            serde_json::to_value(InputKind::Enum {
                labels: vec!["x".into()]
            })
            .unwrap(),
            serde_json::json!({"type": "enum", "labels": ["x"]})
        );
        assert_eq!(
            serde_json::to_value(InputKind::TimestampTz).unwrap(),
            serde_json::json!({"type": "timestampTz"})
        );
    }

    #[test]
    fn input_kinds_follow_column_types() {
        assert_eq!(InputKind::for_type(&DataType::INT64), InputKind::Integer);
        assert_eq!(
            InputKind::for_type(&DataType::VarChar { length: Some(3) }),
            InputKind::Text
        );
        assert_eq!(
            InputKind::for_type(&DataType::Json { binary: true }),
            InputKind::Json
        );
        assert_eq!(
            InputKind::for_type(&DataType::VarBinary { length: None }),
            InputKind::Bytes
        );
        assert_eq!(
            InputKind::for_type(&DataType::Array {
                element: Box::new(DataType::Text)
            }),
            InputKind::Other
        );
    }
}
