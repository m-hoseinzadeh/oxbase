//! Source Arrow pages → load batches (`docs/m8-contracts.md` §4.4). Owner of the bodies marked
//! `unimplemented!("m8: transfer")`: the **transfer** stream.
//!
//! The source driver produces batches in its own Arrow types (PostgreSQL `numeric` as text,
//! arrays and intervals as text, MySQL `TIME` as `Duration`, SQL Server decimals as text, ...).
//! The target's [`oxbase_dialect::BulkLoader`] takes batches only in the *load schema* of its
//! columns (`LoadTarget::schema`, [`oxbase_dialect::bulk::load_arrow_type`] of each column's
//! logical type). [`BatchConverter`] bridges the two, per column:
//!
//! - same Arrow type: the array is passed through (no copy);
//! - a known widening (integers to `Int64`, floats to `Float64`, `Date64` to `Date32`, other
//!   time units to microseconds, a zoned timestamp to `+00:00`): Arrow cast semantics, checked;
//! - anything into `Utf8` (text, decimals, JSON, intervals, arrays, enums, `Other`): the value's
//!   canonical text (`oxbase_driver::cell_value`, decimals exact, JSON as JSON text, binary as
//!   lowercase hex only when the target is text);
//! - text into a typed column (a query result typed as text by its driver, a PostgreSQL array
//!   into a JSON column): parsed; failures are row issues;
//! - checks against the target column: NOT NULL, `char(n)` / `varchar(n)` length, integer and
//!   decimal range and scale (rounding is an issue only when digits would be lost).
//!
//! A value that does not convert is NULL in the output plus a [`RowIssue`]; the pipeline then
//! applies `ErrorMode` (stop at the first issue, or drop those rows and report them).
//! Conversion never panics and never reads the whole table: one batch in, one batch out.

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use oxbase_dialect::LoadColumn;
use oxbase_types::Result;
use serde::{Deserialize, Serialize};

use crate::mapping::ColumnPlan;

/// One value that did not convert (or one failed row).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RowIssue {
    /// Source row number in the table's read order, 1-based.
    pub row: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    pub message: String,
}

/// One converted batch.
#[derive(Debug, Clone)]
pub struct Converted {
    /// In the load schema; rows with issues still present (their bad values NULL).
    pub batch: RecordBatch,
    /// Rows (0-based within the batch) with at least one issue, ascending.
    pub bad_rows: Vec<usize>,
    pub issues: Vec<RowIssue>,
}

/// Converts batches of one source schema into the load schema of `target` columns.
#[derive(Debug)]
pub struct BatchConverter {
    source: SchemaRef,
    target: Vec<LoadColumn>,
    /// `columns[i]`: index of the source column that feeds `target[i]`.
    columns: Vec<usize>,
}

impl BatchConverter {
    /// A converter for batches of `source` into `target` (in load order), fed per
    /// [`ColumnPlan`] (`plans[i].source` names the source field of `target[i]`; skipped plans
    /// are not in `target`). `InvalidInput` when a source field is missing or a pair has no
    /// conversion.
    pub fn new(source: SchemaRef, plans: &[ColumnPlan], target: Vec<LoadColumn>) -> Result<Self> {
        let _ = (source, plans, target);
        unimplemented!("m8: transfer")
    }

    /// The source schema this converter reads.
    pub fn source_schema(&self) -> &SchemaRef {
        &self.source
    }

    /// The target columns, in load order.
    pub fn target(&self) -> &[LoadColumn] {
        &self.target
    }

    /// Converts one batch; `first_row` is the 1-based number of its first row (for issues).
    pub fn convert(&self, batch: &RecordBatch, first_row: u64) -> Result<Converted> {
        let _ = (batch, first_row, &self.columns);
        unimplemented!("m8: transfer")
    }
}
