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
//!
//! # How a value converts
//!
//! Columns whose Arrow type already is the load type and whose target has nothing to check
//! (no length, range, label or JSON check) pass through as they are. Every other value is read
//! with [`oxbase_driver::cell_value`] and converted by the target column's logical type:
//!
//! | Target | Accepted values |
//! |---|---|
//! | `bool` | booleans, the integers 0 / 1, texts `t` / `f`, `true` / `false`, `yes` / `no`, `on` / `off`, `1` / `0` |
//! | `int` | integers in range, whole floats and decimals, integer texts, booleans (1 / 0) |
//! | `float` | numbers and numeric texts (`float32` range checked) |
//! | `decimal(p,s)` | numbers and numeric texts with at most `p - s` integer digits; decimal places beyond `s` must be zeros |
//! | text types, `interval`, `array`, `other` | the value's text form (see below), `char(n)` / `varchar(n)` lengths checked |
//! | `enum(...)` | the text form, one of the labels |
//! | `json` | JSON values; texts must be valid JSON; a PostgreSQL array literal (`{1,2}`, from a source column of logical type `array`) becomes a JSON array; other scalars become JSON scalars |
//! | `binary(n)`, `varbinary(n)` | bytes (length checked), UUIDs (16 bytes), texts (their UTF-8 bytes) |
//! | `date`, `time`, `timestamp` | the same, timestamps cut to a date or a time of day, instants in UTC, ISO-8601 texts |
//! | `timestamptz` | instants; timestamps without a zone and texts without an offset read as UTC |
//! | `uuid` | UUIDs, their text, 16 bytes |
//!
//! Text forms: numbers in their shortest exact decimal form, booleans `true` / `false`, bytes
//! as lowercase hex, `YYYY-MM-DD`, `HH:MM:SS[.ffffff]`, `YYYY-MM-DD HH:MM:SS[.ffffff]`, instants
//! with their offset (`+00:00`), hyphenated UUIDs, compact JSON. Digits below a microsecond are
//! cut (the load schema holds microseconds).

use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Date32Builder, FixedSizeBinaryBuilder, Float64Builder,
    Int64Builder, StringBuilder, Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
    UInt64Builder,
};
use arrow_array::{Array, ArrayRef, RecordBatch, RecordBatchOptions};
use arrow_schema::{DataType as ArrowType, Field, Schema, SchemaRef};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use oxbase_dialect::bulk::load_arrow_type;
use oxbase_dialect::{Fidelity, LoadColumn};
use oxbase_driver::cell_value;
use oxbase_types::{DataType, Error, Result, Value};
use serde::{Deserialize, Serialize};

use crate::mapping::{ColumnPlan, fidelity_between};

/// Longest part of a value quoted in an issue message (characters).
const SHOWN_CHARS: usize = 60;

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
    /// `from[i]`: the logical type of the source column that feeds `target[i]`.
    from: Vec<DataType>,
    /// The load schema with every field nullable (rows with issues keep a NULL).
    schema: SchemaRef,
}

impl BatchConverter {
    /// A converter for batches of `source` into `target` (in load order), fed per
    /// [`ColumnPlan`] (`plans[i].source` names the source field of `target[i]`; skipped plans
    /// are not in `target`). `InvalidInput` when a source field is missing or a pair has no
    /// conversion.
    ///
    /// `plans` may hold the skipped plans too (every column of a table plan): they are left
    /// out before pairing with `target`.
    pub fn new(source: SchemaRef, plans: &[ColumnPlan], target: Vec<LoadColumn>) -> Result<Self> {
        let active: Vec<&ColumnPlan> = plans.iter().filter(|p| !p.skipped).collect();
        if active.len() != target.len() {
            return Err(Error::invalid_input(format!(
                "{} columns are copied, but the load has {} target columns",
                active.len(),
                target.len()
            )));
        }
        let mut columns = Vec::with_capacity(target.len());
        let mut from = Vec::with_capacity(target.len());
        for (plan, column) in active.iter().zip(&target) {
            let index = source.index_of(&plan.source).map_err(|_| {
                Error::invalid_input(format!("the source has no column '{}'", plan.source))
            })?;
            if plan.source_type != column.data_type
                && fidelity_between(&plan.source_type, &column.data_type).0 == Fidelity::Unsupported
            {
                return Err(Error::invalid_input(format!(
                    "column '{}': there is no conversion from {} to {}",
                    plan.source, plan.source_type, column.data_type
                )));
            }
            columns.push(index);
            from.push(plan.source_type.clone());
        }
        let schema = Arc::new(Schema::new(
            target
                .iter()
                .map(|c| Field::new(&c.name, load_arrow_type(&c.data_type), true))
                .collect::<Vec<_>>(),
        ));
        Ok(Self {
            source,
            target,
            columns,
            from,
            schema,
        })
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
        let mut issues: Vec<(usize, RowIssue)> = Vec::new();
        let mut arrays = Vec::with_capacity(self.target.len());
        for (i, column) in self.target.iter().enumerate() {
            let source = batch.columns().get(self.columns[i]).ok_or_else(|| {
                Error::internal(format!(
                    "the source batch has {} columns, the transfer reads column {}",
                    batch.num_columns(),
                    self.columns[i] + 1
                ))
            })?;
            let mut column_issues = Vec::new();
            let array = convert_column(source, &self.from[i], column, &mut column_issues)?;
            for (row, message) in column_issues {
                issues.push((
                    row,
                    RowIssue {
                        row: first_row + u64::try_from(row).unwrap_or(u64::MAX),
                        column: Some(column.name.clone()),
                        message,
                    },
                ));
            }
            arrays.push(array);
        }
        let out = RecordBatch::try_new_with_options(
            Arc::clone(&self.schema),
            arrays,
            &RecordBatchOptions::new().with_row_count(Some(batch.num_rows())),
        )
        .map_err(|e| Error::internal(format!("building a load batch failed: {e}")))?;
        issues.sort_by_key(|(row, _)| *row);
        let mut bad_rows: Vec<usize> = issues.iter().map(|(row, _)| *row).collect();
        bad_rows.dedup();
        Ok(Converted {
            batch: out,
            bad_rows,
            issues: issues.into_iter().map(|(_, issue)| issue).collect(),
        })
    }
}

/// `true` when values of a source column of logical type `from` and Arrow type `arrow` can be
/// handed to `to` unchanged: the Arrow type is the load type and the target checks nothing.
fn passes_through(arrow: &ArrowType, from: &DataType, to: &DataType) -> bool {
    if arrow != &load_arrow_type(to) {
        return false;
    }
    if from == to {
        // Same type: nothing to check (lengths and labels are the same).
        return true;
    }
    match to {
        DataType::Bool
        | DataType::Int { bits: 64, .. }
        | DataType::Float { bits: 64 }
        | DataType::Date
        | DataType::Time { .. }
        | DataType::Timestamp { .. }
        | DataType::TimestampTz { .. }
        | DataType::Uuid
        | DataType::Binary { length: None }
        | DataType::VarBinary { length: None }
        // Text targets take any text form as it is.
        | DataType::Text
        | DataType::VarChar { length: None }
        | DataType::Interval
        | DataType::Other { .. } => true,
        DataType::Decimal {
            precision: None, ..
        } => matches!(from, DataType::Decimal { .. }),
        DataType::Json { .. } => matches!(from, DataType::Json { .. }),
        DataType::Array { .. } => matches!(from, DataType::Array { .. }),
        _ => false,
    }
}

/// One output value.
enum Cell {
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    Str(String),
    Bin(Vec<u8>),
    /// Days since 1970-01-01.
    Date(i32),
    /// Microseconds since midnight.
    Time(i64),
    /// Microseconds since 1970-01-01 (UTC for zoned columns).
    Ts(i64),
    Uuid([u8; 16]),
}

/// A builder of one load-schema column.
enum Out {
    Bool(Box<BooleanBuilder>),
    I64(Box<Int64Builder>),
    U64(Box<UInt64Builder>),
    F64(Box<Float64Builder>),
    Str(Box<StringBuilder>),
    Bin(Box<BinaryBuilder>),
    Date(Box<Date32Builder>),
    Time(Box<Time64MicrosecondBuilder>),
    Ts(Box<TimestampMicrosecondBuilder>),
    Uuid(Box<FixedSizeBinaryBuilder>),
}

impl Out {
    fn new(arrow: &ArrowType, rows: usize) -> Result<Self> {
        Ok(match arrow {
            ArrowType::Boolean => Self::Bool(Box::new(BooleanBuilder::with_capacity(rows))),
            ArrowType::Int64 => Self::I64(Box::new(Int64Builder::with_capacity(rows))),
            ArrowType::UInt64 => Self::U64(Box::new(UInt64Builder::with_capacity(rows))),
            ArrowType::Float64 => Self::F64(Box::new(Float64Builder::with_capacity(rows))),
            ArrowType::Utf8 => Self::Str(Box::new(StringBuilder::with_capacity(rows, rows * 16))),
            ArrowType::Binary => Self::Bin(Box::new(BinaryBuilder::with_capacity(rows, rows * 16))),
            ArrowType::Date32 => Self::Date(Box::new(Date32Builder::with_capacity(rows))),
            ArrowType::Time64(_) => {
                Self::Time(Box::new(Time64MicrosecondBuilder::with_capacity(rows)))
            }
            ArrowType::Timestamp(_, _) => Self::Ts(Box::new(
                TimestampMicrosecondBuilder::with_capacity(rows).with_data_type(arrow.clone()),
            )),
            ArrowType::FixedSizeBinary(16) => {
                Self::Uuid(Box::new(FixedSizeBinaryBuilder::with_capacity(rows, 16)))
            }
            other => {
                return Err(Error::internal(format!(
                    "no load builder for Arrow type {other}"
                )));
            }
        })
    }

    fn append_null(&mut self) {
        match self {
            Self::Bool(b) => b.append_null(),
            Self::I64(b) => b.append_null(),
            Self::U64(b) => b.append_null(),
            Self::F64(b) => b.append_null(),
            Self::Str(b) => b.append_null(),
            Self::Bin(b) => b.append_null(),
            Self::Date(b) => b.append_null(),
            Self::Time(b) => b.append_null(),
            Self::Ts(b) => b.append_null(),
            Self::Uuid(b) => b.append_null(),
        }
    }

    fn append(&mut self, cell: Cell) -> Result<()> {
        match (self, cell) {
            (Self::Bool(b), Cell::Bool(v)) => b.append_value(v),
            (Self::I64(b), Cell::I64(v)) => b.append_value(v),
            (Self::U64(b), Cell::U64(v)) => b.append_value(v),
            (Self::F64(b), Cell::F64(v)) => b.append_value(v),
            (Self::Str(b), Cell::Str(v)) => b.append_value(v),
            (Self::Bin(b), Cell::Bin(v)) => b.append_value(v),
            (Self::Date(b), Cell::Date(v)) => b.append_value(v),
            (Self::Time(b), Cell::Time(v)) => b.append_value(v),
            (Self::Ts(b), Cell::Ts(v)) => b.append_value(v),
            (Self::Uuid(b), Cell::Uuid(v)) => b
                .append_value(v)
                .map_err(|e| Error::internal(format!("building a UUID column failed: {e}")))?,
            _ => return Err(Error::internal("a converted value has the wrong load type")),
        }
        Ok(())
    }

    fn finish(self) -> ArrayRef {
        match self {
            Self::Bool(mut b) => Arc::new(b.finish()),
            Self::I64(mut b) => Arc::new(b.finish()),
            Self::U64(mut b) => Arc::new(b.finish()),
            Self::F64(mut b) => Arc::new(b.finish()),
            Self::Str(mut b) => Arc::new(b.finish()),
            Self::Bin(mut b) => Arc::new(b.finish()),
            Self::Date(mut b) => Arc::new(b.finish()),
            Self::Time(mut b) => Arc::new(b.finish()),
            Self::Ts(mut b) => Arc::new(b.finish()),
            Self::Uuid(mut b) => Arc::new(b.finish()),
        }
    }
}

/// Converts one column; issues are `(row in batch, message)`.
fn convert_column(
    array: &ArrayRef,
    from: &DataType,
    to: &LoadColumn,
    issues: &mut Vec<(usize, String)>,
) -> Result<ArrayRef> {
    let not_null = "NULL in a column that does not allow NULL".to_owned();
    if passes_through(array.data_type(), from, &to.data_type) {
        if !to.nullable && array.null_count() > 0 {
            for row in (0..array.len()).filter(|&row| array.is_null(row)) {
                issues.push((row, not_null.clone()));
            }
        }
        return Ok(Arc::clone(array));
    }
    let mut out = Out::new(&load_arrow_type(&to.data_type), array.len())?;
    for row in 0..array.len() {
        if array.is_null(row) {
            out.append_null();
            if !to.nullable {
                issues.push((row, not_null.clone()));
            }
            continue;
        }
        let converted = match cell_value(array.as_ref(), row) {
            Ok(value) => convert_value(value, from, &to.data_type),
            Err(e) => Err(e.to_string()),
        };
        match converted {
            Ok(cell) => out.append(cell)?,
            Err(message) => {
                out.append_null();
                issues.push((row, message));
            }
        }
    }
    Ok(out.finish())
}

type CellResult = std::result::Result<Cell, String>;

/// `text` shortened for a message.
fn shown(text: &str) -> String {
    if text.chars().count() <= SHOWN_CHARS {
        format!("'{text}'")
    } else {
        let head: String = text.chars().take(SHOWN_CHARS).collect();
        format!("'{head}...'")
    }
}

/// One value into the load form of `to` (module docs, *How a value converts*).
fn convert_value(value: Value, from: &DataType, to: &DataType) -> CellResult {
    let cannot = |value: &Value| format!("{} cannot be stored as {to}", describe(value));
    match to {
        DataType::Bool => to_bool(&value)
            .ok_or_else(|| cannot(&value))
            .map(Cell::Bool),
        DataType::Int { bits, unsigned } => to_int(&value, *bits, *unsigned, to),
        DataType::Float { bits } => to_float(&value, *bits, to),
        DataType::Decimal { precision, scale } => to_decimal(&value, *precision, *scale, to),
        DataType::Char { length } | DataType::VarChar { length } => {
            let text = text_form(value);
            check_length(&text, *length)?;
            Ok(Cell::Str(text))
        }
        DataType::Text | DataType::Interval | DataType::Array { .. } | DataType::Other { .. } => {
            Ok(Cell::Str(text_form(value)))
        }
        DataType::Enum { labels } => {
            let text = text_form(value);
            if labels.is_empty() || labels.contains(&text) {
                Ok(Cell::Str(text))
            } else {
                Err(format!("{} is not one of the values of {to}", shown(&text)))
            }
        }
        DataType::Json { .. } => to_json(value, from).map(Cell::Str),
        DataType::Binary { length } | DataType::VarBinary { length } => {
            let bytes = match value {
                Value::Bytes(b) => b,
                Value::Uuid(u) => u.as_bytes().to_vec(),
                Value::Text(t) => t.into_bytes(),
                other => return Err(cannot(&other)),
            };
            if let Some(n) = length
                && bytes.len() > usize::try_from(*n).unwrap_or(usize::MAX)
            {
                return Err(format!(
                    "the value has {} bytes, more than the column's {n}",
                    bytes.len()
                ));
            }
            Ok(Cell::Bin(bytes))
        }
        DataType::Date => {
            let date = match &value {
                Value::Date(d) => Some(*d),
                Value::Timestamp(t) => Some(t.date()),
                Value::TimestampTz(t) => Some(t.naive_utc().date()),
                Value::Text(t) => parse_date(t),
                _ => None,
            };
            let date = date.ok_or_else(|| cannot(&value))?;
            let days = date.signed_duration_since(epoch_date()).num_days();
            i32::try_from(days)
                .map(Cell::Date)
                .map_err(|_| format!("{date} is out of range"))
        }
        DataType::Time { .. } => {
            let time = match &value {
                Value::Time(t) => Some(*t),
                Value::Timestamp(t) => Some(t.time()),
                Value::TimestampTz(t) => Some(t.naive_utc().time()),
                Value::Text(t) => parse_time(t),
                _ => None,
            };
            time.map(|t| Cell::Time(time_micros(t)))
                .ok_or_else(|| cannot(&value))
        }
        DataType::Timestamp { .. } | DataType::TimestampTz { .. } => {
            let instant = match &value {
                Value::Timestamp(t) => Some(*t),
                Value::TimestampTz(t) => Some(t.naive_utc()),
                Value::Date(d) => d.and_hms_opt(0, 0, 0),
                Value::Text(t) => parse_instant(t),
                _ => None,
            };
            instant
                .map(|t| Cell::Ts(t.and_utc().timestamp_micros()))
                .ok_or_else(|| cannot(&value))
        }
        DataType::Uuid => match &value {
            Value::Uuid(u) => Ok(Cell::Uuid(*u.as_bytes())),
            Value::Text(t) => uuid::Uuid::parse_str(t.trim())
                .map(|u| Cell::Uuid(*u.as_bytes()))
                .map_err(|_| format!("{} is not a UUID", shown(t))),
            Value::Bytes(b) => <[u8; 16]>::try_from(b.as_slice())
                .map(Cell::Uuid)
                .map_err(|_| format!("{} bytes are not a UUID", b.len())),
            other => Err(cannot(other)),
        },
    }
}

/// What a value is, for messages: its text (shortened), or its kind.
fn describe(value: &Value) -> String {
    match value {
        Value::Bytes(b) => format!("a {}-byte value", b.len()),
        Value::Json(_) => "a JSON value".to_owned(),
        other => shown(&text_form(other.clone())),
    }
}

fn epoch_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default()
}

/// Microseconds since midnight (a leap second counts as the last microsecond of its second).
fn time_micros(t: NaiveTime) -> i64 {
    let micros = (t.nanosecond() / 1_000).min(999_999);
    i64::from(t.num_seconds_from_midnight()) * 1_000_000 + i64::from(micros)
}

/// The text form of a value (module docs).
fn text_form(value: Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Text(t) | Value::Decimal(t) => t,
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bytes(b) => b
            .iter()
            .fold(String::with_capacity(b.len() * 2), |mut s, byte| {
                use std::fmt::Write as _;
                let _ = write!(s, "{byte:02x}");
                s
            }),
        Value::Date(d) => d.format("%Y-%m-%d").to_string(),
        Value::Time(t) => t.format("%H:%M:%S%.f").to_string(),
        Value::Timestamp(t) => t.format("%Y-%m-%d %H:%M:%S%.f").to_string(),
        Value::TimestampTz(t) => t.format("%Y-%m-%d %H:%M:%S%.f%:z").to_string(),
        Value::Uuid(u) => u.hyphenated().to_string(),
        Value::Json(j) => j.to_string(),
    }
}

/// `Err` when `text` is longer than `length` characters.
fn check_length(text: &str, length: Option<u32>) -> std::result::Result<(), String> {
    let Some(limit) = length.and_then(|n| usize::try_from(n).ok()) else {
        return Ok(());
    };
    // Bytes bound the characters from above: most texts need no count.
    if text.len() <= limit {
        return Ok(());
    }
    let count = text.chars().count();
    if count <= limit {
        Ok(())
    } else {
        Err(format!(
            "the text has {count} characters, more than the column's {limit}"
        ))
    }
}

fn to_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::Int(0) | Value::UInt(0) => Some(false),
        Value::Int(1) | Value::UInt(1) => Some(true),
        Value::Text(t) | Value::Decimal(t) => {
            let t = t.trim().to_ascii_lowercase();
            match t.as_str() {
                "t" | "true" | "1" | "yes" | "y" | "on" => Some(true),
                "f" | "false" | "0" | "no" | "n" | "off" => Some(false),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `(min, max)` of an integer type.
fn int_range(bits: u8, unsigned: bool) -> (i128, i128) {
    let bits = u32::from(bits.clamp(1, 64));
    if unsigned {
        (0, (1_i128 << bits) - 1)
    } else {
        (-(1_i128 << (bits - 1)), (1_i128 << (bits - 1)) - 1)
    }
}

fn to_int(value: &Value, bits: u8, unsigned: bool, to: &DataType) -> CellResult {
    let not_whole = |shown_value: String| format!("{shown_value} is not a whole number");
    let n: i128 = match value {
        Value::Int(v) => i128::from(*v),
        Value::UInt(v) => i128::from(*v),
        Value::Bool(b) => i128::from(*b),
        Value::Float(f) => float_to_int(*f).ok_or_else(|| not_whole(f.to_string()))?,
        Value::Decimal(t) | Value::Text(t) => {
            let d = Dec::parse(t).ok_or_else(|| format!("{} is not a number", shown(t)))?;
            d.to_int().ok_or_else(|| not_whole(shown(t)))?
        }
        other => return Err(format!("{} cannot be stored as {to}", describe(other))),
    };
    let (min, max) = int_range(bits, unsigned);
    if n < min || n > max {
        return Err(format!("{n} is outside the range of {to}"));
    }
    if bits >= 64 && unsigned {
        u64::try_from(n)
            .map(Cell::U64)
            .map_err(|_| format!("{n} is outside the range of {to}"))
    } else {
        i64::try_from(n)
            .map(Cell::I64)
            .map_err(|_| format!("{n} is outside the range of {to}"))
    }
}

/// A whole float as an integer (`None` with a fraction, non-finite or beyond `i128`).
#[allow(
    clippy::cast_possible_truncation,
    reason = "the value is whole and checked to lie within the i128 range first"
)]
fn float_to_int(f: f64) -> Option<i128> {
    if !f.is_finite() || f.fract() != 0.0 || f.abs() >= 1.0e38 {
        return None;
    }
    Some(f as i128)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "integers into a float column round, as the mapping's fidelity says"
)]
fn to_float(value: &Value, bits: u8, to: &DataType) -> CellResult {
    let v = match value {
        Value::Float(f) => *f,
        Value::Int(i) => *i as f64,
        Value::UInt(u) => *u as f64,
        Value::Decimal(t) | Value::Text(t) => t
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("{} is not a number", shown(t)))?,
        other => return Err(format!("{} cannot be stored as {to}", describe(other))),
    };
    if bits <= 32 && v.is_finite() && v.abs() > f64::from(f32::MAX) {
        return Err(format!("{v} is outside the range of {to}"));
    }
    Ok(Cell::F64(v))
}

fn to_decimal(
    value: &Value,
    precision: Option<u32>,
    scale: Option<u32>,
    to: &DataType,
) -> CellResult {
    let text = match value {
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Bool(b) => u8::from(*b).to_string(),
        Value::Float(f) if f.is_finite() => f.to_string(),
        Value::Float(f) => return Err(format!("{f} is not a decimal number")),
        Value::Decimal(t) | Value::Text(t) => t.clone(),
        other => return Err(format!("{} cannot be stored as {to}", describe(other))),
    };
    let d = Dec::parse(&text).ok_or_else(|| format!("{} is not a number", shown(&text)))?;
    d.fit(precision, scale)
        .map(Cell::Str)
        .map_err(|why| format!("{}: {why} ({to})", shown(&text)))
}

/// An exact decimal number in text form.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Dec {
    negative: bool,
    /// Integer digits without leading zeros (`"0"` for none).
    int: String,
    /// Fraction digits as written.
    frac: String,
}

impl Dec {
    /// `[+-]digits[.digits][e[+-]digits]` (surrounding spaces allowed).
    fn parse(text: &str) -> Option<Self> {
        let t = text.trim();
        let (negative, t) = match t.as_bytes().first() {
            Some(b'-') => (true, &t[1..]),
            Some(b'+') => (false, &t[1..]),
            _ => (false, t),
        };
        let (mantissa, exponent) = match t.find(['e', 'E']) {
            Some(pos) => (&t[..pos], t[pos + 1..].parse::<i32>().ok()?),
            None => (t, 0),
        };
        if exponent.abs() > 1_000 {
            return None;
        }
        let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        if (int.is_empty() && frac.is_empty())
            || !int.bytes().all(|b| b.is_ascii_digit())
            || !frac.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let mut digits = format!("{int}{frac}");
        // Position of the decimal point within `digits`.
        let mut point = i64::try_from(int.len()).ok()? + i64::from(exponent);
        if point < 0 {
            let pad = usize::try_from(-point).ok()?;
            digits = format!("{}{digits}", "0".repeat(pad));
            point = 0;
        }
        let point = usize::try_from(point).ok()?;
        if point > digits.len() {
            digits.push_str(&"0".repeat(point - digits.len()));
        }
        let (int, frac) = digits.split_at(point);
        let int = int.trim_start_matches('0');
        Some(Self {
            negative,
            int: if int.is_empty() {
                "0".to_owned()
            } else {
                int.to_owned()
            },
            frac: frac.to_owned(),
        })
    }

    fn is_zero(&self) -> bool {
        self.int == "0" && self.frac.bytes().all(|b| b == b'0')
    }

    /// The value as an integer, when it has no fraction (and fits `i128`).
    fn to_int(&self) -> Option<i128> {
        if !self.frac.bytes().all(|b| b == b'0') || self.int.len() > 38 {
            return None;
        }
        let n: i128 = self.int.parse().ok()?;
        Some(if self.negative { -n } else { n })
    }

    /// The canonical text of the value in `decimal(precision, scale)`: decimal places beyond
    /// `scale` must be zeros (dropped), the integer part must fit `precision - scale` digits.
    fn fit(
        &self,
        precision: Option<u32>,
        scale: Option<u32>,
    ) -> std::result::Result<String, String> {
        let mut frac = self.frac.as_str();
        if let Some(p) = precision {
            let s = usize::try_from(scale.unwrap_or(0)).unwrap_or(usize::MAX);
            if frac.len() > s {
                if !frac[s..].bytes().all(|b| b == b'0') {
                    return Err(format!("more than {s} decimal places"));
                }
                frac = &frac[..s];
            }
            let int_room = usize::try_from(p).unwrap_or(usize::MAX).saturating_sub(s);
            if self.int != "0" && self.int.len() > int_room {
                return Err(format!("more than {int_room} integer digits"));
            }
        }
        let mut out = String::with_capacity(self.int.len() + frac.len() + 2);
        if self.negative && !self.is_zero() {
            out.push('-');
        }
        out.push_str(&self.int);
        if !frac.is_empty() {
            out.push('.');
            out.push_str(frac);
        }
        Ok(out)
    }
}

/// JSON text for a JSON column (module docs).
fn to_json(value: Value, from: &DataType) -> std::result::Result<String, String> {
    let json = match value {
        Value::Text(text) => {
            if let DataType::Array { element } = from {
                return pg_array_json(&text, element).map(|j| j.to_string());
            }
            if serde_json::from_str::<serde::de::IgnoredAny>(&text).is_err() {
                return Err(format!("{} is not valid JSON", shown(&text)));
            }
            return Ok(text);
        }
        Value::Json(j) => j,
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(b),
        Value::Int(i) => serde_json::Value::from(i),
        Value::UInt(u) => serde_json::Value::from(u),
        Value::Float(f) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .ok_or_else(|| format!("{f} is not a JSON number"))?,
        Value::Decimal(d) => serde_json::from_str::<serde_json::Number>(&d)
            .map_or_else(|_| serde_json::Value::String(d), serde_json::Value::Number),
        other => serde_json::Value::String(text_form(other)),
    };
    Ok(json.to_string())
}

/// A PostgreSQL array literal (`{1,2}`, `{"a b",NULL}`, `{{1,2},{3,4}}`, with an optional
/// `[1:2]=` dimension prefix) as a JSON array; elements are numbers, booleans or JSON values
/// when the element type says so, else strings.
fn pg_array_json(text: &str, element: &DataType) -> std::result::Result<serde_json::Value, String> {
    let body = text.trim();
    let body = match (body.starts_with('['), body.find('=')) {
        (true, Some(eq)) => body[eq + 1..].trim(),
        _ => body,
    };
    let chars: Vec<char> = body.chars().collect();
    let mut pos = 0;
    let bad = || format!("{} is not an array literal", shown(text));
    let value = parse_array(&chars, &mut pos, element).ok_or_else(bad)?;
    if chars[pos..].iter().any(|c| !c.is_whitespace()) {
        return Err(bad());
    }
    Ok(value)
}

fn parse_array(chars: &[char], pos: &mut usize, element: &DataType) -> Option<serde_json::Value> {
    let skip_ws = |pos: &mut usize| {
        while chars.get(*pos).is_some_and(|c| c.is_whitespace()) {
            *pos += 1;
        }
    };
    skip_ws(pos);
    if chars.get(*pos) != Some(&'{') {
        return None;
    }
    *pos += 1;
    let mut items = Vec::new();
    skip_ws(pos);
    if chars.get(*pos) == Some(&'}') {
        *pos += 1;
        return Some(serde_json::Value::Array(items));
    }
    loop {
        skip_ws(pos);
        match chars.get(*pos)? {
            '{' => items.push(parse_array(chars, pos, element)?),
            '"' => {
                *pos += 1;
                let mut s = String::new();
                loop {
                    match chars.get(*pos)? {
                        '\\' => {
                            s.push(*chars.get(*pos + 1)?);
                            *pos += 2;
                        }
                        '"' => {
                            *pos += 1;
                            break;
                        }
                        c => {
                            s.push(*c);
                            *pos += 1;
                        }
                    }
                }
                items.push(element_json(&s, element));
            }
            _ => {
                let start = *pos;
                while chars.get(*pos).is_some_and(|c| *c != ',' && *c != '}') {
                    *pos += 1;
                }
                let raw: String = chars[start..*pos].iter().collect();
                let raw = raw.trim();
                if raw.eq_ignore_ascii_case("null") {
                    items.push(serde_json::Value::Null);
                } else {
                    items.push(element_json(raw, element));
                }
            }
        }
        skip_ws(pos);
        match chars.get(*pos)? {
            ',' => *pos += 1,
            '}' => {
                *pos += 1;
                return Some(serde_json::Value::Array(items));
            }
            _ => return None,
        }
    }
}

/// One array element as JSON by the element's logical type.
fn element_json(text: &str, element: &DataType) -> serde_json::Value {
    match element {
        DataType::Int { .. } | DataType::Float { .. } | DataType::Decimal { .. } => {
            serde_json::from_str::<serde_json::Number>(text).map_or_else(
                |_| serde_json::Value::String(text.to_owned()),
                serde_json::Value::Number,
            )
        }
        DataType::Bool => match text {
            "t" | "true" => serde_json::Value::Bool(true),
            "f" | "false" => serde_json::Value::Bool(false),
            other => serde_json::Value::String(other.to_owned()),
        },
        DataType::Json { .. } => serde_json::from_str(text)
            .unwrap_or_else(|_| serde_json::Value::String(text.to_owned())),
        _ => serde_json::Value::String(text.to_owned()),
    }
}

fn parse_date(text: &str) -> Option<NaiveDate> {
    let t = text.trim();
    NaiveDate::parse_from_str(t, "%Y-%m-%d")
        .ok()
        .or_else(|| parse_instant(t).map(|ts| ts.date()))
}

fn parse_time(text: &str) -> Option<NaiveTime> {
    let t = text.trim();
    ["%H:%M:%S%.f", "%H:%M"]
        .iter()
        .find_map(|f| NaiveTime::parse_from_str(t, f).ok())
}

/// A timestamp text as a UTC date-time: with an offset (ISO-8601 / RFC 3339, `+05:30`,
/// `+0530`, `+05`) converted to UTC, without one read as it is.
fn parse_instant(text: &str) -> Option<NaiveDateTime> {
    let t = text.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(t) {
        return Some(dt.naive_utc());
    }
    for format in [
        "%Y-%m-%d %H:%M:%S%.f%#z",
        "%Y-%m-%dT%H:%M:%S%.f%#z",
        "%Y-%m-%d %H:%M:%S%.f %#z",
    ] {
        if let Ok(dt) = DateTime::parse_from_str(t, format) {
            return Some(dt.naive_utc());
        }
    }
    for format in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(t, format) {
            return Some(dt);
        }
    }
    NaiveDate::parse_from_str(t, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::cast::AsArray;
    use arrow_array::types::{
        Date32Type, Float64Type, Int64Type, Time64MicrosecondType, TimestampMicrosecondType,
        UInt64Type,
    };
    use arrow_array::{
        BinaryArray, BooleanArray, Decimal128Array, FixedSizeBinaryArray, Float32Array, Int16Array,
        Int32Array, Int64Array, StringArray, Time64MicrosecondArray, TimestampMicrosecondArray,
    };
    use arrow_schema::TimeUnit;
    use oxbase_dialect::TypeMapping;
    use pretty_assertions::assert_eq;

    fn plan(source: &str, source_type: DataType, target: &DataType) -> ColumnPlan {
        ColumnPlan {
            source: source.into(),
            source_type,
            source_native: String::new(),
            target: source.into(),
            mapping: TypeMapping::exact(target.clone()),
            overridden: false,
            skipped: false,
        }
    }

    /// Converts one column of `array` (source logical type `from`) into `to`.
    fn convert_one(array: ArrayRef, from: DataType, to: DataType, nullable: bool) -> Converted {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "c",
            array.data_type().clone(),
            true,
        )]));
        let batch = RecordBatch::try_new(Arc::clone(&schema), vec![array]).unwrap();
        let plans = [plan("c", from, &to)];
        let expected = load_arrow_type(&to);
        let mut column = LoadColumn::new("c", to);
        column.nullable = nullable;
        let converter = BatchConverter::new(schema, &plans, vec![column]).unwrap();
        let out = converter.convert(&batch, 1).unwrap();
        assert_eq!(out.batch.column(0).data_type(), &expected);
        out
    }

    fn texts(out: &Converted) -> Vec<Option<String>> {
        out.batch
            .column(0)
            .as_string::<i32>()
            .iter()
            .map(|v| v.map(str::to_owned))
            .collect()
    }

    #[test]
    fn integers_widen_and_check_their_range() {
        let out = convert_one(
            Arc::new(Int32Array::from(vec![Some(1), None, Some(-7)])),
            DataType::INT32,
            DataType::INT64,
            true,
        );
        assert_eq!(
            out.batch
                .column(0)
                .as_primitive::<Int64Type>()
                .iter()
                .collect::<Vec<_>>(),
            vec![Some(1), None, Some(-7)]
        );
        assert_eq!(out.issues, Vec::<RowIssue>::new());

        let out = convert_one(
            Arc::new(Int64Array::from(vec![1, 300, -129])),
            DataType::INT64,
            DataType::Int {
                bits: 8,
                unsigned: false,
            },
            true,
        );
        assert_eq!(out.bad_rows, vec![1, 2]);
        assert_eq!(out.issues[0].row, 2);
        assert_eq!(out.issues[0].column.as_deref(), Some("c"));
        assert!(
            out.issues[0].message.contains("outside the range"),
            "{:?}",
            out.issues
        );

        let out = convert_one(
            Arc::new(StringArray::from(vec!["18446744073709551615", "-1"])),
            DataType::Text,
            DataType::Int {
                bits: 64,
                unsigned: true,
            },
            true,
        );
        assert_eq!(
            out.batch.column(0).as_primitive::<UInt64Type>().value(0),
            u64::MAX
        );
        assert_eq!(out.bad_rows, vec![1]);
    }

    #[test]
    fn decimals_keep_their_digits_or_report_losses() {
        let decimals = Decimal128Array::from(vec![Some(1_234_500), None])
            .with_precision_and_scale(10, 4)
            .unwrap();
        let to = DataType::Decimal {
            precision: Some(12),
            scale: Some(2),
        };
        let out = convert_one(
            Arc::new(decimals),
            DataType::Decimal {
                precision: Some(10),
                scale: Some(4),
            },
            to.clone(),
            true,
        );
        assert_eq!(texts(&out), vec![Some("123.45".to_owned()), None]);
        assert_eq!(out.issues, Vec::<RowIssue>::new());

        let out = convert_one(
            Arc::new(StringArray::from(vec![
                "12345678901234567890.0123456789",
                "1.239",
                "123456789012",
                "-0.00",
                "1e3",
                "NaN",
            ])),
            DataType::Decimal {
                precision: None,
                scale: None,
            },
            to,
            true,
        );
        assert_eq!(
            texts(&out),
            vec![
                None,
                None,
                None,
                Some("0.00".to_owned()),
                Some("1000".to_owned()),
                None
            ]
        );
        assert_eq!(out.bad_rows, vec![0, 1, 2, 5]);

        let wide = DataType::Decimal {
            precision: Some(65),
            scale: Some(30),
        };
        let out = convert_one(
            Arc::new(StringArray::from(vec!["12345678901234567890.0123456789"])),
            DataType::Decimal {
                precision: None,
                scale: None,
            },
            wide,
            true,
        );
        assert_eq!(
            texts(&out),
            vec![Some("12345678901234567890.0123456789".to_owned())]
        );
    }

    #[test]
    fn texts_check_lengths_labels_and_not_null() {
        let out = convert_one(
            Arc::new(StringArray::from(vec![Some("abc"), Some("abcdef"), None])),
            DataType::Text,
            DataType::VarChar { length: Some(5) },
            false,
        );
        assert_eq!(texts(&out), vec![Some("abc".to_owned()), None, None]);
        assert_eq!(out.bad_rows, vec![1, 2]);
        assert!(out.issues[1].message.contains("NULL"));

        let out = convert_one(
            Arc::new(StringArray::from(vec!["😀😀", "ok"])),
            DataType::Text,
            DataType::Char { length: Some(2) },
            true,
        );
        assert_eq!(out.issues, Vec::<RowIssue>::new());

        let labels = DataType::Enum {
            labels: vec!["sad".into(), "ok".into(), "happy".into()],
        };
        let out = convert_one(
            Arc::new(StringArray::from(vec!["happy", "meh"])),
            DataType::Other {
                name: "s.m8_mood".into(),
            },
            labels,
            true,
        );
        assert_eq!(out.bad_rows, vec![1]);

        // Non-text values into text: their canonical text form.
        let out = convert_one(
            Arc::new(FixedSizeBinaryArray::try_from_iter(vec![[0xab_u8; 16]].into_iter()).unwrap()),
            DataType::Uuid,
            DataType::Char { length: Some(36) },
            true,
        );
        assert_eq!(
            texts(&out),
            vec![Some("abababab-abab-abab-abab-abababababab".to_owned())]
        );
        let out = convert_one(
            Arc::new(BooleanArray::from(vec![true])),
            DataType::Bool,
            DataType::Text,
            true,
        );
        assert_eq!(texts(&out), vec![Some("true".to_owned())]);
        let out = convert_one(
            Arc::new(BinaryArray::from(vec![&[0x00_u8, 0xff, 0x0a][..]])),
            DataType::VarBinary { length: None },
            DataType::Text,
            true,
        );
        assert_eq!(texts(&out), vec![Some("00ff0a".to_owned())]);
    }

    #[test]
    fn same_types_pass_through() {
        let array: ArrayRef = Arc::new(StringArray::from(vec![Some("x"), None]));
        let out = convert_one(Arc::clone(&array), DataType::Text, DataType::Text, true);
        assert!(Arc::ptr_eq(out.batch.column(0), &array));
        let array: ArrayRef = Arc::new(StringArray::from(vec!["{\"a\": 1}"]));
        let out = convert_one(
            Arc::clone(&array),
            DataType::Json { binary: true },
            DataType::Json { binary: false },
            true,
        );
        assert!(Arc::ptr_eq(out.batch.column(0), &array));
    }

    #[test]
    fn json_from_arrays_texts_and_scalars() {
        let out = convert_one(
            Arc::new(StringArray::from(vec![
                "{1,2,3}",
                "{}",
                "{{1,2},{3,NULL}}",
                "[0:1]={7,8}",
                "{1,x",
            ])),
            DataType::Array {
                element: Box::new(DataType::INT32),
            },
            DataType::Json { binary: false },
            true,
        );
        assert_eq!(
            texts(&out),
            vec![
                Some("[1,2,3]".to_owned()),
                Some("[]".to_owned()),
                Some("[[1,2],[3,null]]".to_owned()),
                Some("[7,8]".to_owned()),
                None
            ]
        );
        assert_eq!(out.bad_rows, vec![4]);

        let out = convert_one(
            Arc::new(StringArray::from(vec![
                r#"{x,"y,z","with \"quote\"",NULL,"NULL"}"#,
            ])),
            DataType::Array {
                element: Box::new(DataType::Text),
            },
            DataType::Json { binary: true },
            true,
        );
        assert_eq!(
            texts(&out),
            vec![Some(
                r#"["x","y,z","with \"quote\"",null,"NULL"]"#.to_owned()
            )]
        );

        let out = convert_one(
            Arc::new(StringArray::from(vec![r#"{"a": null}"#, "not json"])),
            DataType::Text,
            DataType::Json { binary: false },
            true,
        );
        assert_eq!(texts(&out)[0].as_deref(), Some(r#"{"a": null}"#));
        assert_eq!(out.bad_rows, vec![1]);

        let out = convert_one(
            Arc::new(Int64Array::from(vec![5])),
            DataType::INT64,
            DataType::Json { binary: false },
            true,
        );
        assert_eq!(texts(&out), vec![Some("5".to_owned())]);
    }

    #[test]
    fn temporal_values_convert_to_microseconds() {
        // 2026-03-01 10:00:00+05:30 = 04:30 UTC.
        let instant = 1_772_339_400_000_000_i64;
        let zoned = TimestampMicrosecondArray::from(vec![instant]).with_timezone("UTC");
        let out = convert_one(
            Arc::new(zoned),
            DataType::TimestampTz { precision: Some(6) },
            DataType::Timestamp { precision: Some(6) },
            true,
        );
        assert_eq!(
            out.batch.column(0).data_type(),
            &ArrowType::Timestamp(TimeUnit::Microsecond, None)
        );
        assert_eq!(
            out.batch
                .column(0)
                .as_primitive::<TimestampMicrosecondType>()
                .value(0),
            instant
        );

        let out = convert_one(
            Arc::new(StringArray::from(vec![
                "2026-03-01 10:00:00+05:30",
                "2026-03-01T04:30:00Z",
                "2026-03-01 04:30:00",
                "yesterday",
            ])),
            DataType::Text,
            DataType::TimestampTz { precision: Some(6) },
            true,
        );
        let values = out
            .batch
            .column(0)
            .as_primitive::<TimestampMicrosecondType>();
        assert_eq!(values.value(0), instant);
        assert_eq!(values.value(1), instant);
        assert_eq!(values.value(2), instant);
        assert_eq!(out.bad_rows, vec![3]);

        let out = convert_one(
            Arc::new(StringArray::from(vec!["23:59:59.999999", "838:59:59"])),
            DataType::Text,
            DataType::Time { precision: Some(6) },
            true,
        );
        assert_eq!(
            out.batch
                .column(0)
                .as_primitive::<Time64MicrosecondType>()
                .value(0),
            86_399_999_999
        );
        assert_eq!(out.bad_rows, vec![1]);

        let out = convert_one(
            Arc::new(Time64MicrosecondArray::from(vec![1_000_000])),
            DataType::Time { precision: Some(6) },
            DataType::Text,
            true,
        );
        assert_eq!(texts(&out), vec![Some("00:00:01".to_owned())]);

        let out = convert_one(
            Arc::new(StringArray::from(vec!["1970-01-02", "2026-02-30"])),
            DataType::Text,
            DataType::Date,
            true,
        );
        assert_eq!(out.batch.column(0).as_primitive::<Date32Type>().value(0), 1);
        assert_eq!(out.bad_rows, vec![1]);
    }

    #[test]
    fn floats_booleans_uuids_and_binary() {
        let out = convert_one(
            Arc::new(Float32Array::from(vec![1.5_f32])),
            DataType::Float { bits: 32 },
            DataType::Float { bits: 32 },
            true,
        );
        assert!(
            (out.batch.column(0).as_primitive::<Float64Type>().value(0) - 1.5).abs() < f64::EPSILON
        );
        let out = convert_one(
            Arc::new(StringArray::from(vec!["1e300", "abc"])),
            DataType::Text,
            DataType::Float { bits: 32 },
            true,
        );
        assert_eq!(out.bad_rows, vec![0, 1]);

        let out = convert_one(
            Arc::new(Int16Array::from(vec![1, 0, 2])),
            DataType::INT16,
            DataType::Bool,
            true,
        );
        assert_eq!(
            out.batch.column(0).as_boolean().iter().collect::<Vec<_>>(),
            vec![Some(true), Some(false), None]
        );
        assert_eq!(out.bad_rows, vec![2]);

        let out = convert_one(
            Arc::new(StringArray::from(vec![
                "6f1c2a3e-0000-4000-8000-00000000abcd",
                "nope",
            ])),
            DataType::Char { length: Some(36) },
            DataType::Uuid,
            true,
        );
        assert_eq!(
            out.batch.column(0).as_fixed_size_binary().value(0)[15],
            0xcd
        );
        assert_eq!(out.bad_rows, vec![1]);

        let out = convert_one(
            Arc::new(BinaryArray::from(vec![&[1_u8, 2, 3][..]])),
            DataType::VarBinary { length: None },
            DataType::Binary { length: Some(2) },
            true,
        );
        assert_eq!(out.bad_rows, vec![0]);
    }

    #[test]
    fn converters_refuse_missing_columns_and_impossible_pairs() {
        let schema = Arc::new(Schema::new(vec![Field::new("a", ArrowType::Int64, true)]));
        let to = DataType::INT64;
        let missing = BatchConverter::new(
            Arc::clone(&schema),
            &[plan("b", DataType::INT64, &to)],
            vec![LoadColumn::new("b", to.clone())],
        );
        assert!(missing.is_err());
        let impossible = BatchConverter::new(
            Arc::clone(&schema),
            &[plan("a", DataType::Json { binary: false }, &DataType::Date)],
            vec![LoadColumn::new("a", DataType::Date)],
        );
        assert!(impossible.is_err());
        let mut skipped = plan("z", DataType::INT64, &to);
        skipped.skipped = true;
        let converter = BatchConverter::new(
            schema,
            &[plan("a", DataType::INT64, &to), skipped],
            vec![LoadColumn::new("a", to)],
        )
        .unwrap();
        assert_eq!(converter.target().len(), 1);
        assert_eq!(converter.source_schema().fields().len(), 1);
    }

    #[test]
    fn decimal_texts_parse_exactly() {
        let d = Dec::parse(" -0012.5000 ").unwrap();
        assert_eq!(d.fit(Some(6), Some(2)).unwrap(), "-12.50");
        assert_eq!(
            Dec::parse("1.5e-3").unwrap().fit(None, None).unwrap(),
            "0.0015"
        );
        assert_eq!(Dec::parse("12e2").unwrap().to_int(), Some(1200));
        assert!(Dec::parse("1e99999").is_none());
        assert!(Dec::parse(".").is_none());
        assert_eq!(Dec::parse(".5").unwrap().fit(None, None).unwrap(), "0.5");
    }
}
