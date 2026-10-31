//! Value conversion for grid editing (M3): Arrow cells → edit text and [`Value`], and edit text
//! → [`Value`] for a column.
//!
//! The grid shows and edits **text**; the database gets typed, bound [`Value`]s. One grammar
//! per column type, used in both directions so a value shown in an editor round-trips exactly
//! (`docs/m3-contracts.md` §5):
//!
//! | Column type (`DataType`) | Edit text (`cell_text` writes it, `parse_input` reads it) | Bound as |
//! |---|---|---|
//! | `bool` | `true` / `false` (reads also `t`/`f`, `1`/`0`, `yes`/`no`, any case) | `Bool` |
//! | `int` | decimal digits with optional sign; out of range for the width is an error | `Int` (`UInt` above `i64::MAX` for unsigned 64-bit) |
//! | `float` | Rust/JSON decimal or exponent form, `NaN`, `Infinity`, `-Infinity` | `Float` |
//! | `decimal` | plain decimal text, kept exactly (no float rounding) | `Decimal` |
//! | `char` / `var_char` / `text` | as is; empty text is an empty string, never NULL | `Text` |
//! | `binary` / `var_binary` | `0x` + hex (written), also `\x` + hex and `base64:` + base64 (read) | `Bytes` |
//! | `date` | `YYYY-MM-DD` | `Date` |
//! | `time` | `HH:MM:SS[.ffffff]` | `Time` |
//! | `timestamp` | `YYYY-MM-DD HH:MM:SS[.ffffff]` (`T` accepted) | `Timestamp` |
//! | `timestamp_tz` | RFC 3339 with offset (`Z` or `±HH:MM`); without an offset it is UTC | `TimestampTz` |
//! | `uuid` | canonical hyphenated form (any case read) | `Uuid` |
//! | `json` | JSON text, validated (written compact for cells, pretty in the form's JSON) | `Json` |
//! | `enum` | one of the labels (exact) | `Text` |
//! | `interval`, `array`, `other` | the engine's text form | `Text` (the engine's DmlWriter casts it) |
//!
//! `CellInput::Null` is `Value::Null` (refused for NOT NULL columns before anything runs);
//! `CellInput::Default` leaves the column out of an `INSERT` (`parse_input` returns `None`).
//!
//! Owner of the bodies: the **services** stream.

use arrow_array::{Array, RecordBatch};
use oxbase_types::{ColumnDef, Result, Value};

use crate::edit::CellInput;

/// The edit text of row `row` of `array` (`None` for NULL), in the grammar of the module docs.
/// The Arrow type and field metadata decide the form (for example PostgreSQL `numeric` arrives as
/// text and stays exact).
pub fn cell_text(array: &dyn Array, row: usize) -> Result<Option<String>> {
    let _ = (array, row);
    unimplemented!("m3: services")
}

/// The typed value of row `row` of `array`, for binding as a key or comparing a read-back.
/// `column` (when known) picks the [`Value`] variant the module docs bind for its type.
pub fn cell_value(array: &dyn Array, row: usize, column: Option<&ColumnDef>) -> Result<Value> {
    let _ = (array, row, column);
    unimplemented!("m3: services")
}

/// What `input` binds for `column`: `Some(value)`, or `None` for `CellInput::Default` (leave the
/// column out). `InvalidInput` naming the column and the expected form when the text does not
/// parse, or for NULL into a NOT NULL column.
pub fn parse_input(input: &CellInput, column: &ColumnDef) -> Result<Option<Value>> {
    let _ = (input, column);
    unimplemented!("m3: services")
}

/// Row `row` of `batch` as a pretty-printed JSON object (the form's JSON view; see
/// `edit::FormRow::json`).
pub fn row_json(batch: &RecordBatch, row: usize) -> Result<String> {
    let _ = (batch, row);
    unimplemented!("m3: services")
}
