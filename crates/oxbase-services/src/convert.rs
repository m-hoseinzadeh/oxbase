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
//! Details of the Arrow side:
//!
//! - Text cells (`Utf8`) are shown exactly as the driver delivered them (PostgreSQL `numeric`
//!   without a fitting precision, intervals, arrays and enums arrive as the server's text, JSON
//!   as the server's JSON text), so nothing is reformatted.
//! - `FixedSizeBinary(16)` is a UUID (PostgreSQL `uuid`, the only driver type of that shape).
//! - `Duration` is MySQL/MariaDB `TIME` (it may exceed 24 hours or be negative):
//!   `[-]HH:MM:SS[.ffffff]`.
//! - `Timestamp` with a time zone is an instant, shown in UTC with a `Z`.
//! - Fractional seconds are written with 6 digits when not zero.
//!
//! Owner of the bodies: the **services** stream.

use std::fmt::Write as _;

use arrow_array::cast::AsArray;
use arrow_array::temporal_conversions::{
    date32_to_datetime, date64_to_datetime, time32ms_to_time, time32s_to_time, time64ns_to_time,
    time64us_to_time, timestamp_ms_to_datetime, timestamp_ns_to_datetime, timestamp_s_to_datetime,
    timestamp_us_to_datetime,
};
use arrow_array::types::{
    Date32Type, Date64Type, Decimal32Type, Decimal64Type, Decimal128Type, Decimal256Type,
    DurationMicrosecondType, DurationMillisecondType, DurationNanosecondType, DurationSecondType,
    Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type, Time32MillisecondType,
    Time32SecondType, Time64MicrosecondType, Time64NanosecondType, TimestampMicrosecondType,
    TimestampMillisecondType, TimestampNanosecondType, TimestampSecondType, UInt8Type, UInt16Type,
    UInt32Type, UInt64Type,
};
use arrow_array::{Array, RecordBatch};
use arrow_schema::{DataType as ArrowType, TimeUnit};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use oxbase_types::{ColumnDef, DataType, Error, Result, Value};
use uuid::Uuid;

use crate::edit::CellInput;

/// Arrow's canonical extension-name metadata key.
const EXTENSION_NAME: &str = "ARROW:extension:name";
/// Arrow's canonical JSON extension.
const JSON_EXTENSION: &str = "arrow.json";
const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;

/// The edit text of row `row` of `array` (`None` for NULL), in the grammar of the module docs.
/// The Arrow type and field metadata decide the form (for example PostgreSQL `numeric` arrives as
/// text and stays exact).
pub fn cell_text(array: &dyn Array, row: usize) -> Result<Option<String>> {
    check_row(array, row)?;
    if array.is_null(row) {
        return Ok(None);
    }
    if matches!(array.data_type(), ArrowType::Float32) {
        let v = array.as_primitive::<Float32Type>().value(row);
        return Ok(Some(f32_text(v)));
    }
    Ok(value_text(&cell_value(array, row, None)?))
}

/// The typed value of row `row` of `array`, for binding as a key or comparing a read-back.
/// `column` (when known) picks the [`Value`] variant the module docs bind for its type.
pub fn cell_value(array: &dyn Array, row: usize, column: Option<&ColumnDef>) -> Result<Value> {
    check_row(array, row)?;
    if array.is_null(row) {
        return Ok(Value::Null);
    }
    let wanted = column.map(|c| &c.data_type);
    let value = match array.data_type() {
        ArrowType::Null => Value::Null,
        ArrowType::Boolean => Value::Bool(array.as_boolean().value(row)),
        ArrowType::Int8 => int(array.as_primitive::<Int8Type>().value(row), wanted),
        ArrowType::Int16 => int(array.as_primitive::<Int16Type>().value(row), wanted),
        ArrowType::Int32 => int(array.as_primitive::<Int32Type>().value(row), wanted),
        ArrowType::Int64 => int(array.as_primitive::<Int64Type>().value(row), wanted),
        ArrowType::UInt8 => int(array.as_primitive::<UInt8Type>().value(row), wanted),
        ArrowType::UInt16 => int(array.as_primitive::<UInt16Type>().value(row), wanted),
        ArrowType::UInt32 => int(array.as_primitive::<UInt32Type>().value(row), wanted),
        ArrowType::UInt64 => {
            let v = array.as_primitive::<UInt64Type>().value(row);
            i64::try_from(v).map_or(Value::UInt(v), |i| int(i, wanted))
        }
        ArrowType::Float32 => {
            Value::Float(f64::from(array.as_primitive::<Float32Type>().value(row)))
        }
        ArrowType::Float64 => Value::Float(array.as_primitive::<Float64Type>().value(row)),
        ArrowType::Decimal32(..) => {
            Value::Decimal(array.as_primitive::<Decimal32Type>().value_as_string(row))
        }
        ArrowType::Decimal64(..) => {
            Value::Decimal(array.as_primitive::<Decimal64Type>().value_as_string(row))
        }
        ArrowType::Decimal128(..) => {
            Value::Decimal(array.as_primitive::<Decimal128Type>().value_as_string(row))
        }
        ArrowType::Decimal256(..) => {
            Value::Decimal(array.as_primitive::<Decimal256Type>().value_as_string(row))
        }
        ArrowType::Utf8 => text(array.as_string::<i32>().value(row), wanted),
        ArrowType::LargeUtf8 => text(array.as_string::<i64>().value(row), wanted),
        ArrowType::Utf8View => text(array.as_string_view().value(row), wanted),
        ArrowType::Binary => Value::Bytes(array.as_binary::<i32>().value(row).to_vec()),
        ArrowType::LargeBinary => Value::Bytes(array.as_binary::<i64>().value(row).to_vec()),
        ArrowType::BinaryView => Value::Bytes(array.as_binary_view().value(row).to_vec()),
        ArrowType::FixedSizeBinary(16) => {
            let bytes = array.as_fixed_size_binary().value(row);
            Uuid::from_slice(bytes).map_or_else(|_| Value::Bytes(bytes.to_vec()), Value::Uuid)
        }
        ArrowType::FixedSizeBinary(_) => {
            Value::Bytes(array.as_fixed_size_binary().value(row).to_vec())
        }
        ArrowType::Date32 => {
            let v = array.as_primitive::<Date32Type>().value(row);
            Value::Date(temporal(date32_to_datetime(v))?.date())
        }
        ArrowType::Date64 => {
            let v = array.as_primitive::<Date64Type>().value(row);
            Value::Date(temporal(date64_to_datetime(v))?.date())
        }
        ArrowType::Time32(unit) => {
            let time = if *unit == TimeUnit::Second {
                time32s_to_time(array.as_primitive::<Time32SecondType>().value(row))
            } else {
                time32ms_to_time(array.as_primitive::<Time32MillisecondType>().value(row))
            };
            Value::Time(temporal(time)?)
        }
        ArrowType::Time64(unit) => {
            let time = if *unit == TimeUnit::Nanosecond {
                time64ns_to_time(array.as_primitive::<Time64NanosecondType>().value(row))
            } else {
                time64us_to_time(array.as_primitive::<Time64MicrosecondType>().value(row))
            };
            Value::Time(temporal(time)?)
        }
        ArrowType::Timestamp(unit, tz) => {
            let naive = temporal(match unit {
                TimeUnit::Second => {
                    timestamp_s_to_datetime(array.as_primitive::<TimestampSecondType>().value(row))
                }
                TimeUnit::Millisecond => timestamp_ms_to_datetime(
                    array.as_primitive::<TimestampMillisecondType>().value(row),
                ),
                TimeUnit::Microsecond => timestamp_us_to_datetime(
                    array.as_primitive::<TimestampMicrosecondType>().value(row),
                ),
                TimeUnit::Nanosecond => timestamp_ns_to_datetime(
                    array.as_primitive::<TimestampNanosecondType>().value(row),
                ),
            })?;
            if tz.is_some() {
                Value::TimestampTz(naive.and_utc().fixed_offset())
            } else {
                Value::Timestamp(naive)
            }
        }
        ArrowType::Duration(unit) => {
            let micros = match unit {
                TimeUnit::Second => array
                    .as_primitive::<DurationSecondType>()
                    .value(row)
                    .saturating_mul(MICROS_PER_SECOND),
                TimeUnit::Millisecond => array
                    .as_primitive::<DurationMillisecondType>()
                    .value(row)
                    .saturating_mul(1000),
                TimeUnit::Microsecond => array.as_primitive::<DurationMicrosecondType>().value(row),
                TimeUnit::Nanosecond => {
                    array.as_primitive::<DurationNanosecondType>().value(row) / 1000
                }
            };
            duration(micros, wanted)
        }
        other => {
            return Err(Error::unsupported(format!(
                "values of type {other} cannot be shown or edited in the grid"
            )));
        }
    };
    Ok(value)
}

/// What `input` binds for `column`: `Some(value)`, or `None` for `CellInput::Default` (leave the
/// column out). `InvalidInput` naming the column and the expected form when the text does not
/// parse, or for NULL into a NOT NULL column.
pub fn parse_input(input: &CellInput, column: &ColumnDef) -> Result<Option<Value>> {
    match input {
        CellInput::Default => Ok(None),
        CellInput::Null if column.nullable => Ok(Some(Value::Null)),
        CellInput::Null => Err(Error::invalid_input(format!(
            "column '{}' is NOT NULL",
            column.name
        ))),
        CellInput::Text { text } => parse_text(text, column).map(Some),
    }
}

/// Row `row` of `batch` as a pretty-printed JSON object (the form's JSON view; see
/// `edit::FormRow::json`).
pub fn row_json(batch: &RecordBatch, row: usize) -> Result<String> {
    let schema = batch.schema();
    let mut fields = Vec::with_capacity(batch.num_columns());
    for (field, array) in schema.fields().iter().zip(batch.columns()) {
        let is_json = field
            .metadata()
            .get(EXTENSION_NAME)
            .is_some_and(|name| name == JSON_EXTENSION);
        let value = cell_value(array.as_ref(), row, None)?;
        fields.push(JsonField {
            name: field.name().clone(),
            value,
            is_json,
        });
    }
    Ok(object_json(&fields))
}

// ---------------------------------------------------------------------------------------------
// Values → text and JSON (shared with the edit service's overlay rows)
// ---------------------------------------------------------------------------------------------

/// The edit text of `value` (`None` for NULL), in the grammar of the module docs.
pub(crate) fn value_text(value: &Value) -> Option<String> {
    Some(match value {
        Value::Null => return None,
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f64_text(*f),
        Value::Decimal(s) | Value::Text(s) => s.clone(),
        Value::Bytes(b) => hex_text(b),
        Value::Date(d) => date_text(*d),
        Value::Time(t) => time_text(*t),
        Value::Timestamp(ts) => format!("{} {}", date_text(ts.date()), time_text(ts.time())),
        Value::TimestampTz(ts) => timestamptz_text(ts),
        Value::Uuid(u) => u.hyphenated().to_string(),
        Value::Json(j) => j.to_string(),
    })
}

/// One field of a JSON object built by [`object_json`].
pub(crate) struct JsonField {
    pub name: String,
    pub value: Value,
    /// The value is JSON text (a JSON column delivered as text): nest it.
    pub is_json: bool,
}

/// `fields` as a pretty-printed JSON object (two-space indent): numbers as JSON numbers with
/// their exact digits, JSON nested, bytes as `"0x..."`, temporal values as ISO 8601 strings,
/// NULL as `null`.
pub(crate) fn object_json(fields: &[JsonField]) -> String {
    if fields.is_empty() {
        return "{}".to_owned();
    }
    let mut out = String::from("{\n");
    for (pos, field) in fields.iter().enumerate() {
        out.push_str("  ");
        out.push_str(&json_string(&field.name));
        out.push_str(": ");
        let fragment = value_json(&field.value, field.is_json);
        // Nested documents are indented one level further.
        out.push_str(&fragment.replace('\n', "\n  "));
        if pos + 1 < fields.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push('}');
    out
}

fn value_json(value: &Value, is_json: bool) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) if f.is_finite() => f64_text(*f),
        Value::Decimal(s) if is_json_number(s) => s.clone(),
        Value::Json(j) => pretty(j),
        Value::Text(s) if is_json => serde_json::from_str::<serde_json::Value>(s)
            .map_or_else(|_| json_string(s), |j| pretty(&j)),
        Value::Timestamp(ts) => json_string(&format!(
            "{}T{}",
            date_text(ts.date()),
            time_text(ts.time())
        )),
        other => json_string(&value_text(other).unwrap_or_default()),
    }
}

fn pretty(j: &serde_json::Value) -> String {
    serde_json::to_string_pretty(j).unwrap_or_else(|_| j.to_string())
}

fn json_string(s: &str) -> String {
    serde_json::Value::String(s.to_owned()).to_string()
}

/// `true` for text that is a valid JSON number (`-?(0|[1-9]d*)(.d+)?([eE][+-]?d+)?`).
fn is_json_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if b.first() == Some(&b'-') {
        i += 1;
    }
    let digits = |from: usize| b[from..].iter().take_while(|c| c.is_ascii_digit()).count();
    let int_len = digits(i);
    if int_len == 0 || (int_len > 1 && b[i] == b'0') {
        return false;
    }
    i += int_len;
    if b.get(i) == Some(&b'.') {
        let frac = digits(i + 1);
        if frac == 0 {
            return false;
        }
        i += 1 + frac;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp = digits(i);
        if exp == 0 {
            return false;
        }
        i += exp;
    }
    i == b.len()
}

fn hex_text(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn date_text(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

fn time_text(t: NaiveTime) -> String {
    let base = t.format("%H:%M:%S").to_string();
    // A leap second carries nanoseconds past 1e9.
    let micros = (t.nanosecond() % 1_000_000_000) / 1000;
    if micros == 0 {
        base
    } else {
        format!("{base}.{micros:06}")
    }
}

fn timestamptz_text(ts: &DateTime<FixedOffset>) -> String {
    let local = ts.naive_local();
    let offset = if ts.offset().local_minus_utc() == 0 {
        "Z".to_owned()
    } else {
        ts.offset().to_string()
    };
    format!(
        "{}T{}{offset}",
        date_text(local.date()),
        time_text(local.time())
    )
}

fn f64_text(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_owned()
    } else if v.is_infinite() {
        if v > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else if uses_exponent(v.abs()) {
        format!("{v:e}")
    } else {
        v.to_string()
    }
}

/// Very large and very small magnitudes are written with an exponent.
fn uses_exponent(magnitude: f64) -> bool {
    magnitude > 0.0 && !(1e-5..1e16).contains(&magnitude)
}

fn f32_text(v: f32) -> String {
    if v.is_nan() {
        "NaN".to_owned()
    } else if v.is_infinite() {
        if v > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else if uses_exponent(f64::from(v.abs())) {
        format!("{v:e}")
    } else {
        v.to_string()
    }
}

/// `[-]HH:MM:SS[.ffffff]` of a signed duration in microseconds (hours may exceed 24).
fn duration_text(micros: i64) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let abs = micros.unsigned_abs();
    let micros_per_second = MICROS_PER_SECOND.unsigned_abs();
    let secs = abs / micros_per_second;
    let frac = abs % micros_per_second;
    let base = format!(
        "{sign}{:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    );
    if frac == 0 {
        base
    } else {
        format!("{base}.{frac:06}")
    }
}

// ---------------------------------------------------------------------------------------------
// Arrow cells → values
// ---------------------------------------------------------------------------------------------

fn check_row(array: &dyn Array, row: usize) -> Result<()> {
    if row < array.len() {
        Ok(())
    } else {
        Err(Error::internal(format!(
            "row {row} is outside a column of {} rows",
            array.len()
        )))
    }
}

fn temporal<T>(value: Option<T>) -> Result<T> {
    value.ok_or_else(|| Error::invalid_input("a date or time value is out of range"))
}

/// An integer cell; a boolean column stored as an integer (SQLite, MySQL `TINYINT(1)` when not
/// mapped to `Boolean`) becomes `Bool`.
fn int(v: impl Into<i64>, wanted: Option<&DataType>) -> Value {
    let v = v.into();
    if matches!(wanted, Some(DataType::Bool)) {
        Value::Bool(v != 0)
    } else {
        Value::Int(v)
    }
}

/// A text cell: JSON, decimal and UUID columns keep their own variants (the text is the
/// server's exact form); anything else is text.
fn text(s: &str, wanted: Option<&DataType>) -> Value {
    match wanted {
        Some(DataType::Json { .. }) => {
            serde_json::from_str(s).map_or_else(|_| Value::Text(s.to_owned()), Value::Json)
        }
        Some(DataType::Decimal { .. }) if is_decimal(s) => Value::Decimal(s.to_owned()),
        Some(DataType::Uuid) => {
            Uuid::parse_str(s).map_or_else(|_| Value::Text(s.to_owned()), Value::Uuid)
        }
        _ => Value::Text(s.to_owned()),
    }
}

/// A duration cell (MySQL `TIME`): a time of day for a `time` column when it is one, else its
/// text.
fn duration(micros: i64, wanted: Option<&DataType>) -> Value {
    if matches!(wanted, Some(DataType::Time { .. })) && (0..MICROS_PER_DAY).contains(&micros) {
        let secs = u32::try_from(micros / MICROS_PER_SECOND).unwrap_or_default();
        let nanos = u32::try_from((micros % MICROS_PER_SECOND) * 1000).unwrap_or_default();
        if let Some(t) = NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos) {
            return Value::Time(t);
        }
    }
    Value::Text(duration_text(micros))
}

// ---------------------------------------------------------------------------------------------
// Edit text → values
// ---------------------------------------------------------------------------------------------

fn bad(column: &ColumnDef, text: &str, expected: &str) -> Error {
    let shown: String = text.chars().take(40).collect();
    let ellipsis = if text.chars().count() > 40 { "..." } else { "" };
    Error::invalid_input(format!(
        "column '{}': '{shown}{ellipsis}' is not {expected}",
        column.name
    ))
}

fn parse_text(text: &str, column: &ColumnDef) -> Result<Value> {
    let trimmed = text.trim();
    match &column.data_type {
        DataType::Bool => parse_bool(trimmed)
            .map(Value::Bool)
            .ok_or_else(|| bad(column, text, "a boolean (true or false)")),
        DataType::Int { bits, unsigned } => parse_int(trimmed, *bits, *unsigned)
            .ok_or_else(|| bad(column, text, &int_expected(*bits, *unsigned))),
        DataType::Float { .. } => parse_float(trimmed)
            .map(Value::Float)
            .ok_or_else(|| bad(column, text, "a number (or NaN, Infinity, -Infinity)")),
        DataType::Decimal { .. } => {
            if is_decimal(trimmed) {
                Ok(Value::Decimal(trimmed.to_owned()))
            } else {
                Err(bad(column, text, "a decimal number"))
            }
        }
        DataType::Char { .. }
        | DataType::VarChar { .. }
        | DataType::Text
        | DataType::Interval
        | DataType::Array { .. }
        | DataType::Other { .. } => Ok(Value::Text(text.to_owned())),
        DataType::Binary { .. } | DataType::VarBinary { .. } => {
            parse_bytes(trimmed).map(Value::Bytes).ok_or_else(|| {
                bad(
                    column,
                    text,
                    "bytes (0x followed by hex digits, or base64:...)",
                )
            })
        }
        DataType::Date => NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
            .map(Value::Date)
            .map_err(|_| bad(column, text, "a date (YYYY-MM-DD)")),
        DataType::Time { .. } => parse_time(trimmed)
            .map(Value::Time)
            .or_else(|| is_duration(trimmed).then(|| Value::Text(trimmed.to_owned())))
            .ok_or_else(|| bad(column, text, "a time (HH:MM:SS[.ffffff])")),
        DataType::Timestamp { .. } => parse_timestamp(trimmed)
            .map(Value::Timestamp)
            .ok_or_else(|| bad(column, text, "a timestamp (YYYY-MM-DD HH:MM:SS[.ffffff])")),
        DataType::TimestampTz { .. } => parse_timestamptz(trimmed)
            .map(Value::TimestampTz)
            .ok_or_else(|| {
                bad(
                    column,
                    text,
                    "a timestamp with an offset (YYYY-MM-DDTHH:MM:SS[.ffffff]Z or +HH:MM)",
                )
            }),
        DataType::Uuid => Uuid::parse_str(trimmed)
            .map(Value::Uuid)
            .map_err(|_| bad(column, text, "a UUID")),
        DataType::Json { .. } => serde_json::from_str(text).map(Value::Json).map_err(|e| {
            Error::invalid_input(format!("column '{}': invalid JSON: {e}", column.name))
        }),
        DataType::Enum { labels } => {
            if labels.iter().any(|label| label == text) {
                Ok(Value::Text(text.to_owned()))
            } else {
                Err(Error::invalid_input(format!(
                    "column '{}': '{text}' is not one of {}",
                    column.name,
                    labels.join(", ")
                )))
            }
        }
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "t" | "1" | "yes" | "y" | "on" => Some(true),
        "false" | "f" | "0" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

fn int_bounds(bits: u8, unsigned: bool) -> (i128, i128) {
    let bits = if (1..=64).contains(&bits) { bits } else { 64 };
    if unsigned {
        (0, (1_i128 << bits) - 1)
    } else {
        (-(1_i128 << (bits - 1)), (1_i128 << (bits - 1)) - 1)
    }
}

fn int_expected(bits: u8, unsigned: bool) -> String {
    let (low, high) = int_bounds(bits, unsigned);
    format!("an integer from {low} to {high}")
}

fn parse_int(s: &str, bits: u8, unsigned: bool) -> Option<Value> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: i128 = s.parse().ok()?;
    let (low, high) = int_bounds(bits, unsigned);
    if !(low..=high).contains(&v) {
        return None;
    }
    match i64::try_from(v) {
        Ok(i) => Some(Value::Int(i)),
        Err(_) => u64::try_from(v).ok().map(Value::UInt),
    }
}

fn parse_float(s: &str) -> Option<f64> {
    match s.to_ascii_lowercase().as_str() {
        "nan" => return Some(f64::NAN),
        "infinity" | "+infinity" | "inf" | "+inf" => return Some(f64::INFINITY),
        "-infinity" | "-inf" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    if s.is_empty() || !s.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok().filter(|v: &f64| v.is_finite())
}

/// `[+-]digits[.digits][e[+-]digits]`, with at least one digit before or after the point.
fn is_decimal(s: &str) -> bool {
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(pos) => (&body[..pos], Some(&body[pos + 1..])),
        None => (body, None),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    let all_digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = !(int_part.is_empty() && frac_part.is_empty())
        && all_digits(int_part)
        && all_digits(frac_part);
    let exponent_ok = exponent.is_none_or(|e| {
        let digits = e.strip_prefix(['+', '-']).unwrap_or(e);
        !digits.is_empty() && all_digits(digits)
    });
    mantissa_ok && exponent_ok
}

fn parse_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let nibble = |c: u8| {
        char::from(c)
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
    };
    (0..bytes.len())
        .step_by(2)
        .map(|i| Some((nibble(bytes[i])? << 4) | nibble(bytes[i + 1])?))
        .collect()
}

/// Standard base64 (`A-Z a-z 0-9 + /`), padding optional, whitespace ignored.
fn parse_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0_u32;
    let mut padding = false;
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace()) {
        if c == b'=' {
            padding = true;
            continue;
        }
        if padding {
            return None;
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).ok()?);
        }
    }
    // Leftover bits must be zero padding (at most 4 of them).
    if bits >= 6 || buffer & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

fn parse_bytes(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() {
        return Some(Vec::new());
    }
    if let Some(hex) = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .or_else(|| s.strip_prefix("\\x"))
    {
        return parse_hex(hex);
    }
    s.strip_prefix("base64:").and_then(parse_base64)
}

fn parse_time(s: &str) -> Option<NaiveTime> {
    ["%H:%M:%S%.f", "%H:%M"]
        .iter()
        .find_map(|format| NaiveTime::parse_from_str(s, format).ok())
}

/// `[-]H+:MM:SS[.f]`: a MySQL `TIME` beyond one day (sent as text).
fn is_duration(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    let mut parts = body.splitn(3, ':');
    let (Some(h), Some(m), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let (sec, frac) = rest.split_once('.').unwrap_or((rest, "0"));
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    digits(h) && m.len() == 2 && digits(m) && sec.len() == 2 && digits(sec) && digits(frac)
}

fn parse_timestamp(s: &str) -> Option<NaiveDateTime> {
    [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ]
    .iter()
    .find_map(|format| NaiveDateTime::parse_from_str(s, format).ok())
}

fn parse_timestamptz(s: &str) -> Option<DateTime<FixedOffset>> {
    if let Ok(ts) = DateTime::parse_from_rfc3339(s) {
        return Some(ts);
    }
    for format in [
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f%#z",
        "%Y-%m-%dT%H:%M:%S%.f%#z",
    ] {
        if let Ok(ts) = DateTime::parse_from_str(s, format) {
            return Some(ts);
        }
    }
    // `Z` without the `T`, or no offset at all: UTC.
    let naive = s.strip_suffix(['Z', 'z']).unwrap_or(s);
    parse_timestamp(naive.trim_end()).map(|ts| ts.and_utc().fixed_offset())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use arrow_array::builder::{
        BinaryBuilder, BooleanBuilder, Date32Builder, Decimal128Builder,
        DurationMicrosecondBuilder, FixedSizeBinaryBuilder, Float32Builder, Float64Builder,
        Int16Builder, StringBuilder, Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
        UInt64Builder,
    };
    use arrow_array::{ArrayRef, Int64Array, StringArray};
    use arrow_schema::{Field, Schema};
    use pretty_assertions::assert_eq;

    use super::*;

    fn col(name: &str, data_type: DataType) -> ColumnDef {
        ColumnDef::new(name, data_type)
    }

    fn parse(text: &str, data_type: DataType) -> Result<Value> {
        parse_input(&CellInput::text(text), &col("c", data_type)).map(Option::unwrap)
    }

    /// `text` parses for `data_type`, and the value writes back as `expected` text.
    fn round_trip(text: &str, data_type: DataType, expected: &str) {
        let value = parse(text, data_type.clone())
            .unwrap_or_else(|e| panic!("{text:?} as {data_type}: {e}"));
        assert_eq!(value_text(&value).as_deref(), Some(expected), "{text:?}");
        // The written form parses to a value written the same way (NaN is not equal to itself).
        let again = parse(expected, data_type).unwrap();
        assert_eq!(
            value_text(&again).as_deref(),
            Some(expected),
            "{expected:?}"
        );
        assert_eq!(again.type_name(), value.type_name());
    }

    fn one<T: Array + 'static>(array: T) -> ArrayRef {
        Arc::new(array)
    }

    #[test]
    fn null_default_and_empty_text() {
        let nullable = col("note", DataType::Text);
        let required = col("code", DataType::Text).not_null();
        assert_eq!(
            parse_input(&CellInput::Null, &nullable).unwrap(),
            Some(Value::Null)
        );
        let err = parse_input(&CellInput::Null, &required).unwrap_err();
        assert!(err.to_string().contains("'code' is NOT NULL"), "{err}");
        assert_eq!(parse_input(&CellInput::Default, &required).unwrap(), None);
        // An empty text is an empty string, never NULL.
        assert_eq!(
            parse_input(&CellInput::text(""), &required).unwrap(),
            Some(Value::Text(String::new()))
        );
        assert_eq!(
            parse_input(&CellInput::text(" a "), &required).unwrap(),
            Some(Value::Text(" a ".into()))
        );
    }

    #[test]
    fn every_grammar_row_round_trips() {
        for (text, expected) in [
            ("true", "true"),
            ("T", "true"),
            ("yes", "true"),
            ("1", "true"),
            ("f", "false"),
            ("NO", "false"),
            ("0", "false"),
        ] {
            round_trip(text, DataType::Bool, expected);
        }
        round_trip("-32768", DataType::INT16, "-32768");
        round_trip("+42", DataType::INT32, "42");
        round_trip(
            "9223372036854775807",
            DataType::INT64,
            "9223372036854775807",
        );
        let u64_type = DataType::Int {
            bits: 64,
            unsigned: true,
        };
        round_trip(
            "18446744073709551615",
            u64_type.clone(),
            "18446744073709551615",
        );
        assert_eq!(
            parse("18446744073709551615", u64_type).unwrap(),
            Value::UInt(u64::MAX)
        );
        for (text, expected) in [
            ("1.5", "1.5"),
            ("-0.25", "-0.25"),
            ("1e300", "1e300"),
            ("0.000001", "1e-6"),
            ("NaN", "NaN"),
            ("Infinity", "Infinity"),
            ("-Infinity", "-Infinity"),
        ] {
            round_trip(text, DataType::FLOAT64, expected);
        }
        let decimal = DataType::Decimal {
            precision: Some(30),
            scale: Some(10),
        };
        round_trip(
            "12345678901234567890.0123456789",
            decimal.clone(),
            "12345678901234567890.0123456789",
        );
        round_trip(" -1.50 ", decimal, "-1.50");
        round_trip("it's \\ ü", DataType::Text, "it's \\ ü");
        round_trip("0xDEad00", DataType::VarBinary { length: None }, "0xdead00");
        round_trip("\\x0102", DataType::VarBinary { length: None }, "0x0102");
        round_trip(
            "base64:AQID",
            DataType::VarBinary { length: None },
            "0x010203",
        );
        round_trip("base64:AQI=", DataType::Binary { length: None }, "0x0102");
        round_trip("2026-10-31", DataType::Date, "2026-10-31");
        round_trip("08:30:00", DataType::Time { precision: None }, "08:30:00");
        round_trip(
            "23:59:59.000123",
            DataType::Time { precision: Some(6) },
            "23:59:59.000123",
        );
        round_trip(
            "2026-10-31T12:34:56.5",
            DataType::Timestamp { precision: None },
            "2026-10-31 12:34:56.500000",
        );
        round_trip(
            "2026-10-31 12:34:56",
            DataType::Timestamp { precision: None },
            "2026-10-31 12:34:56",
        );
        round_trip(
            "2026-10-31T12:00:00+02:00",
            DataType::TimestampTz { precision: None },
            "2026-10-31T12:00:00+02:00",
        );
        round_trip(
            "2026-10-31 12:00:00",
            DataType::TimestampTz { precision: None },
            "2026-10-31T12:00:00Z",
        );
        round_trip(
            "2026-10-31T12:00:00.25Z",
            DataType::TimestampTz { precision: None },
            "2026-10-31T12:00:00.250000Z",
        );
        round_trip(
            "A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11",
            DataType::Uuid,
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
        );
        round_trip(
            r#"{"a": [1, 2]}"#,
            DataType::Json { binary: true },
            r#"{"a":[1,2]}"#,
        );
        round_trip(
            "big",
            DataType::Enum {
                labels: vec!["small".into(), "big".into()],
            },
            "big",
        );
        round_trip("1 day 02:00:00", DataType::Interval, "1 day 02:00:00");
        round_trip(
            "{1,2}",
            DataType::Array {
                element: Box::new(DataType::INT32),
            },
            "{1,2}",
        );
        // MySQL TIME beyond a day stays text.
        assert_eq!(
            parse("-838:59:59", DataType::Time { precision: None }).unwrap(),
            Value::Text("-838:59:59".into())
        );
    }

    #[test]
    fn bad_input_names_the_column_and_the_form() {
        for (text, data_type, expected) in [
            ("maybe", DataType::Bool, "a boolean"),
            ("32768", DataType::INT16, "an integer from -32768 to 32767"),
            (
                "-1",
                DataType::Int {
                    bits: 8,
                    unsigned: true,
                },
                "an integer from 0 to 255",
            ),
            ("1.5", DataType::INT64, "an integer"),
            ("", DataType::INT64, "an integer"),
            ("one", DataType::FLOAT64, "a number"),
            (
                "1,5",
                DataType::Decimal {
                    precision: None,
                    scale: None,
                },
                "a decimal number",
            ),
            ("0xabc", DataType::VarBinary { length: None }, "bytes"),
            ("zz", DataType::VarBinary { length: None }, "bytes"),
            ("base64:A", DataType::VarBinary { length: None }, "bytes"),
            ("31/10/2026", DataType::Date, "a date"),
            ("25:00", DataType::Time { precision: None }, "a time"),
            (
                "2026-10-31",
                DataType::Timestamp { precision: None },
                "a timestamp",
            ),
            ("not-a-uuid", DataType::Uuid, "a UUID"),
            ("{", DataType::Json { binary: false }, "invalid JSON"),
            (
                "huge",
                DataType::Enum {
                    labels: vec!["small".into()],
                },
                "is not one of small",
            ),
        ] {
            let err = parse(text, data_type.clone()).unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains("column 'c'") && message.contains(expected),
                "{text:?} as {data_type}: {message}"
            );
        }
    }

    #[test]
    fn cells_become_text_and_values() {
        let mut ints = Int16Builder::new();
        ints.append_value(-7);
        ints.append_null();
        let ints = one(ints.finish());
        assert_eq!(cell_text(ints.as_ref(), 0).unwrap().as_deref(), Some("-7"));
        assert_eq!(cell_text(ints.as_ref(), 1).unwrap(), None);
        assert_eq!(cell_value(ints.as_ref(), 1, None).unwrap(), Value::Null);
        assert!(cell_text(ints.as_ref(), 2).is_err());

        let mut flags = BooleanBuilder::new();
        flags.append_value(true);
        let flags = one(flags.finish());
        assert_eq!(
            cell_text(flags.as_ref(), 0).unwrap().as_deref(),
            Some("true")
        );

        // SQLite stores booleans as integers: the column type decides.
        let stored = one(Int64Array::from(vec![1_i64]));
        let bool_col = col("flag", DataType::Bool);
        assert_eq!(
            cell_value(stored.as_ref(), 0, Some(&bool_col)).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(cell_value(stored.as_ref(), 0, None).unwrap(), Value::Int(1));

        let mut big = UInt64Builder::new();
        big.append_value(u64::MAX);
        big.append_value(5);
        let big = one(big.finish());
        assert_eq!(
            cell_value(big.as_ref(), 0, None).unwrap(),
            Value::UInt(u64::MAX)
        );
        assert_eq!(cell_value(big.as_ref(), 1, None).unwrap(), Value::Int(5));

        let mut floats = Float32Builder::new();
        floats.append_value(0.1);
        floats.append_value(f32::NEG_INFINITY);
        let floats = one(floats.finish());
        assert_eq!(
            cell_text(floats.as_ref(), 0).unwrap().as_deref(),
            Some("0.1")
        );
        assert_eq!(
            cell_text(floats.as_ref(), 1).unwrap().as_deref(),
            Some("-Infinity")
        );
        let mut doubles = Float64Builder::new();
        doubles.append_value(f64::NAN);
        let doubles = one(doubles.finish());
        assert_eq!(
            cell_text(doubles.as_ref(), 0).unwrap().as_deref(),
            Some("NaN")
        );

        let mut decimals = Decimal128Builder::new()
            .with_precision_and_scale(12, 2)
            .unwrap();
        decimals.append_value(-12_345);
        let decimals = one(decimals.finish());
        assert_eq!(
            cell_value(decimals.as_ref(), 0, None).unwrap(),
            Value::Decimal("-123.45".into())
        );
        assert_eq!(
            cell_text(decimals.as_ref(), 0).unwrap().as_deref(),
            Some("-123.45")
        );

        let mut texts = StringBuilder::new();
        texts.append_value("99999999999999999999999.5");
        texts.append_value(r#"{"b": 1, "a": 2}"#);
        let texts = one(texts.finish());
        // PostgreSQL numeric without a typmod arrives as text and stays exact.
        let decimal_col = col(
            "n",
            DataType::Decimal {
                precision: None,
                scale: None,
            },
        );
        assert_eq!(
            cell_value(texts.as_ref(), 0, Some(&decimal_col)).unwrap(),
            Value::Decimal("99999999999999999999999.5".into())
        );
        assert_eq!(
            cell_text(texts.as_ref(), 1).unwrap().as_deref(),
            Some(r#"{"b": 1, "a": 2}"#)
        );
        let json_col = col("doc", DataType::Json { binary: true });
        assert_eq!(
            cell_value(texts.as_ref(), 1, Some(&json_col)).unwrap(),
            Value::Json(serde_json::json!({"a": 2, "b": 1}))
        );

        let mut bytes = BinaryBuilder::new();
        bytes.append_value([0xde, 0xad]);
        let bytes = one(bytes.finish());
        assert_eq!(
            cell_text(bytes.as_ref(), 0).unwrap().as_deref(),
            Some("0xdead")
        );

        let id = Uuid::parse_str("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11").unwrap();
        let mut uuids = FixedSizeBinaryBuilder::new(16);
        uuids.append_value(id.as_bytes()).unwrap();
        let uuids = one(uuids.finish());
        assert_eq!(
            cell_value(uuids.as_ref(), 0, None).unwrap(),
            Value::Uuid(id)
        );

        let mut dates = Date32Builder::new();
        dates.append_value(20_757);
        let dates = one(dates.finish());
        assert_eq!(
            cell_text(dates.as_ref(), 0).unwrap().as_deref(),
            Some("2026-10-31")
        );

        let mut times = Time64MicrosecondBuilder::new();
        times.append_value(3_600_000_001);
        let times = one(times.finish());
        assert_eq!(
            cell_text(times.as_ref(), 0).unwrap().as_deref(),
            Some("01:00:00.000001")
        );

        let mut stamps = TimestampMicrosecondBuilder::new();
        stamps.append_value(1_793_448_000_000_000);
        let naive = one(stamps.finish());
        assert_eq!(
            cell_text(naive.as_ref(), 0).unwrap().as_deref(),
            Some("2026-10-31 12:00:00")
        );
        let mut stamps = TimestampMicrosecondBuilder::new().with_timezone("UTC");
        stamps.append_value(1_793_448_000_500_000);
        let utc = one(stamps.finish());
        assert_eq!(
            cell_text(utc.as_ref(), 0).unwrap().as_deref(),
            Some("2026-10-31T12:00:00.500000Z")
        );

        // MySQL TIME arrives as a duration.
        let mut durations = DurationMicrosecondBuilder::new();
        durations.append_value(-3_020_399_000_000);
        durations.append_value(45_296_000_000);
        let durations = one(durations.finish());
        assert_eq!(
            cell_text(durations.as_ref(), 0).unwrap().as_deref(),
            Some("-838:59:59")
        );
        let time_col = col("t", DataType::Time { precision: None });
        assert_eq!(
            cell_value(durations.as_ref(), 1, Some(&time_col)).unwrap(),
            Value::Time(NaiveTime::from_hms_opt(12, 34, 56).unwrap())
        );
    }

    #[test]
    fn rows_become_pretty_json() {
        let mut meta = HashMap::new();
        meta.insert(EXTENSION_NAME.to_owned(), JSON_EXTENSION.to_owned());
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", ArrowType::Int64, false),
            Field::new("amount", ArrowType::Utf8, true),
            Field::new("doc", ArrowType::Utf8, true).with_metadata(meta),
            Field::new("note", ArrowType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                one(Int64Array::from(vec![7_i64])),
                one(StringArray::from(vec![Some("12.50")])),
                one(StringArray::from(vec![Some(r#"{"a":[1,2]}"#)])),
                one(StringArray::from(vec![None::<&str>])),
            ],
        )
        .unwrap();
        let json = row_json(&batch, 0).unwrap();
        assert_eq!(
            json,
            "{\n  \"id\": 7,\n  \"amount\": \"12.50\",\n  \"doc\": {\n    \"a\": [\n      1,\n      2\n    ]\n  },\n  \"note\": null\n}"
        );
        // The result parses back.
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["doc"]["a"][1], 2);

        let fields = [
            JsonField {
                name: "exact".into(),
                value: Value::Decimal("12345678901234567890.01".into()),
                is_json: false,
            },
            JsonField {
                name: "odd".into(),
                value: Value::Decimal("NaN".into()),
                is_json: false,
            },
            JsonField {
                name: "bytes".into(),
                value: Value::Bytes(vec![1, 255]),
                is_json: false,
            },
            JsonField {
                name: "at".into(),
                value: Value::Timestamp(
                    NaiveDate::from_ymd_opt(2026, 10, 31)
                        .unwrap()
                        .and_hms_opt(1, 2, 3)
                        .unwrap(),
                ),
                is_json: false,
            },
            JsonField {
                name: "inf".into(),
                value: Value::Float(f64::INFINITY),
                is_json: false,
            },
            JsonField {
                name: "quote\"d".into(),
                value: Value::Text("line\nbreak".into()),
                is_json: false,
            },
        ];
        assert_eq!(
            object_json(&fields),
            "{\n  \"exact\": 12345678901234567890.01,\n  \"odd\": \"NaN\",\n  \"bytes\": \"0x01ff\",\n  \"at\": \"2026-10-31T01:02:03\",\n  \"inf\": \"Infinity\",\n  \"quote\\\"d\": \"line\\nbreak\"\n}"
        );
        assert_eq!(object_json(&[]), "{}");
        assert!(is_json_number("-0.5e10"));
        assert!(!is_json_number("007"));
        assert!(!is_json_number("1."));
        assert!(!is_json_number("+1"));
    }
}
