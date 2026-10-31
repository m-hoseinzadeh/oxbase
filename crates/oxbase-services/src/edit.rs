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

use std::sync::Arc;

use oxbase_dialect::DialectRegistry;
use oxbase_types::{BoundStatement, DataType, Error, ObjectRef, Result, RowKey, TableDataQuery};
use serde::{Deserialize, Serialize};

use crate::query::{ErrorInfo, QueryService, RunHandle, RunId, RunOptions, RunState, RunTarget};
use crate::tree::TreeService;
use crate::{CancellationToken, SessionHandle};

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
}

// The async bodies below are skeletons until the services stream fills them.
#[allow(clippy::unused_async)]
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
        request: TableDataQuery,
        options: RunOptions,
        cancel: CancellationToken,
    ) -> Result<RunHandle> {
        let _ = (self, target, request, options, cancel);
        unimplemented!("m3: services")
    }

    /// Whether and how run `run`'s result can be edited. A run that is not table data is
    /// read-only with [`reasons::NOT_TABLE_DATA`]; `NotFound` for an unknown run.
    pub async fn begin(&self, run: RunId, cancel: CancellationToken) -> Result<Editability> {
        let _ = (self, run, cancel);
        unimplemented!("m3: services")
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
        let _ = (self, run, changes, cancel);
        unimplemented!("m3: services")
    }

    /// Row `row` of result `result_index` of `run` for the form / JSON view, with the values an
    /// earlier apply read back. Works for any run (read-only results too). `NotFound` for an
    /// unknown run, result or row, or a deleted row.
    pub async fn row_form(&self, run: RunId, result_index: usize, row: u64) -> Result<FormRow> {
        let _ = (self, run, result_index, row);
        unimplemented!("m3: services")
    }
}

impl std::fmt::Debug for EditService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditService").finish_non_exhaustive()
    }
}

/// M3 additions to the query service used by table data and editing. The **services** stream
/// implements them (and may move them into `query.rs`).
#[allow(clippy::unused_async)]
impl QueryService {
    /// Starts a run of exactly one statement with bound parameters. Same editor rules, events,
    /// paging, cancel and close as [`QueryService::run`]; the statement is not split, and its
    /// [`RunEvent::Started`](crate::query::RunEvent::Started) lists one statement spanning the
    /// whole SQL text. History records the SQL text (redacted, never the parameter values)
    /// unless `options.record_history` is off.
    pub async fn run_bound(
        &self,
        target: RunTarget,
        statement: BoundStatement,
        options: RunOptions,
    ) -> Result<RunHandle> {
        let _ = (self, target, statement, options);
        unimplemented!("m3: services")
    }

    /// The session run `run` executed on (its editor's session), for edits that must run on
    /// the same session. `NotFound` for an unknown or closed run, or when that session is gone.
    pub fn run_session(&self, run: RunId) -> Result<SessionHandle> {
        let _ = (self, run);
        unimplemented!("m3: services")
    }

    /// Where run `run` is; `None` when unknown or closed.
    pub fn run_state(&self, run: RunId) -> Option<RunState> {
        self.info(run).ok().map(|info| info.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn pending_changes_merge_per_row() {
        let mut changes = PendingChanges::new();
        assert!(changes.is_empty());
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
