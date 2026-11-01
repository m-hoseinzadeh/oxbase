//! Type inference over a sample (read stream).
//!
//! Text formats (CSV, JSON strings, XLSX text cells) have no column types, so [`crate::preview`]
//! feeds every sampled value to a [`TypeInference`] and offers the result as the default
//! [`crate::ColumnSpec`]s; the user can override each one.
//!
//! # Rules (contract)
//!
//! For each column, the narrowest type in this order that **every non-NULL sampled value**
//! parses as (with the reader's [`ValueFormats`]):
//!
//! 1. `bool`: `true` / `false` only (any case; the other boolean tokens are accepted only in
//!    columns the user types `bool`, because `1` / `0` / `y` / `n` are ambiguous);
//! 2. `int(64)`: optional sign and digits, within `i64` (grouping characters removed when a
//!    thousands separator is set). Leading zeros (`007`) make it text: they are identifiers,
//!    not numbers;
//! 3. `decimal(p, s)`: a plain decimal with the decimal separator, at most 38 digits; `p` and
//!    `s` are the largest seen (`s` = most fraction digits, `p` = most integer digits + `s`);
//! 4. `float(64)`: exponent forms, `NaN`, `inf` / `Infinity`, or more than 38 digits;
//! 5. `date` / `timestamp` / `timestamp_tz` / `time`: the formats of [`ValueFormats`] (ISO 8601
//!    when unset); a column mixing dates and timestamps is `timestamp`;
//! 6. `uuid`: canonical hyphenated form;
//! 7. `json`: text starting with `{` or `[` that parses as JSON (JSON reader: nested values
//!    are always `json`);
//! 8. else `text`.
//!
//! A column whose sampled values are all NULL is `text`, nullable. `nullable` is `true` when a
//! NULL was seen (or the sample was not the whole file: a later NULL cannot be ruled out, so
//! only a complete sample may report `false`). Warnings: values that broke a narrower type
//! ("2 of 1000 values are not integers: 'n/a', '12a'; read as text"), and the longest text
//! seen when over 255 characters (helps choose `varchar(n)` vs `text`). Typed sources (XLSX
//! numbers, booleans and dates; JSON numbers and booleans) report their type with
//! [`TypeInference::observe_typed`] and widen the same way (`int` + `float` → `float`, anything +
//! text → text).

use oxbase_types::DataType;

use crate::ValueFormats;

/// What inference concluded for one column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredColumn {
    pub data_type: DataType,
    pub nullable: bool,
    pub warnings: Vec<String>,
}

/// Collects sampled values per column and infers their types (module docs).
#[derive(Debug, Clone)]
pub struct TypeInference {
    formats: ValueFormats,
    columns: usize,
}

impl TypeInference {
    /// Inference for `columns` columns read with `formats`.
    pub fn new(columns: usize, formats: ValueFormats) -> Self {
        Self { formats, columns }
    }

    /// Number of columns.
    pub fn columns(&self) -> usize {
        self.columns
    }

    /// Records one sampled text value of `column` (`None` = NULL). A column index past the
    /// end widens the inference (ragged rows add columns).
    pub fn observe(&mut self, column: usize, text: Option<&str>) {
        let _ = (&self.formats, column, text);
        unimplemented!("m5: read")
    }

    /// Records one value of `column` whose type the source states (XLSX cell types, JSON
    /// numbers and booleans).
    pub fn observe_typed(&mut self, column: usize, data_type: &DataType) {
        let _ = (&self.formats, column, data_type);
        unimplemented!("m5: read")
    }

    /// The inferred columns, in order. `complete` says whether the sample was the whole file
    /// (see the module docs on `nullable`).
    pub fn finish(&self, complete: bool) -> Vec<InferredColumn> {
        let _ = (&self.formats, complete);
        unimplemented!("m5: read")
    }
}
