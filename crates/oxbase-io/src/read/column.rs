//! One column of a batch being read: cells in, the canonical load type out
//! ([`crate::arrow_type_for`]). A cell that cannot be read as the column's type is an `Err`
//! with the row-issue message; the caller appends NULL and records the issue.

use std::sync::{Arc, LazyLock};

use arrow_array::ArrayRef;
use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Date32Builder, FixedSizeBinaryBuilder, Float64Builder,
    Int64Builder, StringBuilder, Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
    UInt64Builder,
};
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use oxbase_types::DataType;

use super::quote;
use super::values::{
    canonical_decimal, is_json, parse_binary, parse_bool, parse_date, parse_f64, parse_i64,
    parse_time, parse_timestamp, parse_timestamp_tz, parse_u64, parse_uuid,
};
use crate::ValueFormats;

/// Days from 0001-01-01 (day 1) to 1970-01-01.
const UNIX_EPOCH_DAYS_FROM_CE: i32 = 719_163;

/// The time zone of `timestamp_tz` load columns.
const UTC_OFFSET: &str = "+00:00";

/// Formats for JSON numbers (always `.` and no grouping).
static PLAIN: LazyLock<ValueFormats> = LazyLock::new(ValueFormats::default);

/// One source value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Cell<'a> {
    Null,
    /// Text to parse per the column type (CSV fields, JSON strings, XLSX text cells).
    Text(&'a str),
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A JSON number exactly as written.
    Number(&'a str),
    /// A spreadsheet date / time cell.
    DateTime(NaiveDateTime),
    /// Nested JSON (an object or array) as compact text.
    Json(&'a str),
    /// A spreadsheet error value (`#DIV/0!`).
    Error(&'a str),
}

enum Kind {
    Bool(BooleanBuilder),
    Int(Int64Builder),
    UInt(UInt64Builder),
    Float(Float64Builder),
    Decimal(StringBuilder),
    Text(StringBuilder),
    Json(StringBuilder),
    Binary(BinaryBuilder),
    Date(Date32Builder),
    Time(Time64MicrosecondBuilder),
    Timestamp(TimestampMicrosecondBuilder),
    TimestampTz(TimestampMicrosecondBuilder),
    Uuid(FixedSizeBinaryBuilder),
}

/// Builds one column of the load schema.
pub(crate) struct ColumnBuilder {
    kind: Kind,
}

impl ColumnBuilder {
    /// A builder for a column of logical type `data_type` (its Arrow type is
    /// [`crate::arrow_type_for`] of it).
    pub(crate) fn new(data_type: &DataType, capacity: usize) -> Self {
        let kind = match data_type {
            DataType::Bool => Kind::Bool(BooleanBuilder::with_capacity(capacity)),
            DataType::Int {
                bits: 64,
                unsigned: true,
            } => Kind::UInt(UInt64Builder::with_capacity(capacity)),
            DataType::Int { .. } => Kind::Int(Int64Builder::with_capacity(capacity)),
            DataType::Float { .. } => Kind::Float(Float64Builder::with_capacity(capacity)),
            DataType::Decimal { .. } => Kind::Decimal(StringBuilder::new()),
            DataType::Json { .. } => Kind::Json(StringBuilder::new()),
            DataType::Binary { .. } | DataType::VarBinary { .. } => {
                Kind::Binary(BinaryBuilder::new())
            }
            DataType::Date => Kind::Date(Date32Builder::with_capacity(capacity)),
            DataType::Time { .. } => Kind::Time(Time64MicrosecondBuilder::with_capacity(capacity)),
            DataType::Timestamp { .. } => {
                Kind::Timestamp(TimestampMicrosecondBuilder::with_capacity(capacity))
            }
            DataType::TimestampTz { .. } => Kind::TimestampTz(
                TimestampMicrosecondBuilder::with_capacity(capacity).with_timezone(UTC_OFFSET),
            ),
            DataType::Uuid => Kind::Uuid(FixedSizeBinaryBuilder::with_capacity(capacity, 16)),
            DataType::Char { .. }
            | DataType::VarChar { .. }
            | DataType::Text
            | DataType::Interval
            | DataType::Array { .. }
            | DataType::Enum { .. }
            | DataType::Other { .. } => Kind::Text(StringBuilder::new()),
        };
        Self { kind }
    }

    pub(crate) fn append_null(&mut self) {
        match &mut self.kind {
            Kind::Bool(b) => b.append_null(),
            Kind::Int(b) => b.append_null(),
            Kind::UInt(b) => b.append_null(),
            Kind::Float(b) => b.append_null(),
            Kind::Decimal(b) | Kind::Text(b) | Kind::Json(b) => b.append_null(),
            Kind::Binary(b) => b.append_null(),
            Kind::Date(b) => b.append_null(),
            Kind::Time(b) => b.append_null(),
            Kind::Timestamp(b) | Kind::TimestampTz(b) => b.append_null(),
            Kind::Uuid(b) => b.append_null(),
        }
    }

    /// Appends `cell`, or returns the row-issue message (nothing appended).
    pub(crate) fn append(&mut self, cell: Cell<'_>, formats: &ValueFormats) -> Result<(), String> {
        if matches!(cell, Cell::Null) {
            self.append_null();
            return Ok(());
        }
        if let Cell::Error(error) = cell {
            return Err(format!("the cell holds the error {error}"));
        }
        match &mut self.kind {
            Kind::Bool(b) => b.append_value(to_bool(cell, formats)?),
            Kind::Int(b) => b.append_value(to_i64(cell, formats)?),
            Kind::UInt(b) => b.append_value(to_u64(cell, formats)?),
            Kind::Float(b) => b.append_value(to_f64(cell, formats)?),
            Kind::Decimal(b) => b.append_value(to_decimal(cell, formats)?),
            Kind::Text(b) => match cell {
                Cell::Text(t) | Cell::Number(t) | Cell::Json(t) => b.append_value(t),
                other => b.append_value(to_text(other)),
            },
            Kind::Json(b) => b.append_value(to_json(cell)?),
            Kind::Binary(b) => match cell {
                Cell::Text(t) => b.append_value(
                    parse_binary(t)
                        .ok_or_else(|| format!("{} is not hex (0x...) or base64", quote(t)))?,
                ),
                other => return Err(mismatch(other, "binary data")),
            },
            Kind::Date(b) => b.append_value(days(to_date(cell, formats)?)?),
            Kind::Time(b) => b.append_value(micros_of_day(to_time(cell, formats)?)),
            Kind::Timestamp(b) => b.append_value(to_timestamp(cell, formats, false)?),
            Kind::TimestampTz(b) => b.append_value(to_timestamp(cell, formats, true)?),
            Kind::Uuid(b) => match cell {
                Cell::Text(t) => {
                    let id = parse_uuid(t).ok_or_else(|| format!("{} is not a UUID", quote(t)))?;
                    b.append_value(id)
                        .map_err(|e| format!("{} is not a UUID: {e}", quote(t)))?;
                }
                other => return Err(mismatch(other, "a UUID")),
            },
        }
        Ok(())
    }

    /// The column so far (the builder starts over).
    pub(crate) fn finish(&mut self) -> ArrayRef {
        match &mut self.kind {
            Kind::Bool(b) => Arc::new(b.finish()),
            Kind::Int(b) => Arc::new(b.finish()),
            Kind::UInt(b) => Arc::new(b.finish()),
            Kind::Float(b) => Arc::new(b.finish()),
            Kind::Decimal(b) | Kind::Text(b) | Kind::Json(b) => Arc::new(b.finish()),
            Kind::Binary(b) => Arc::new(b.finish()),
            Kind::Date(b) => Arc::new(b.finish()),
            Kind::Time(b) => Arc::new(b.finish()),
            Kind::Timestamp(b) | Kind::TimestampTz(b) => Arc::new(b.finish()),
            Kind::Uuid(b) => Arc::new(b.finish()),
        }
    }
}

/// A cell as display text (preview rows, text columns).
pub(crate) fn to_text(cell: Cell<'_>) -> String {
    match cell {
        Cell::Null => String::new(),
        Cell::Text(t) | Cell::Number(t) | Cell::Json(t) | Cell::Error(t) => t.to_owned(),
        Cell::Bool(v) => v.to_string(),
        Cell::Int(v) => v.to_string(),
        Cell::Float(v) => v.to_string(),
        Cell::DateTime(v) => iso_datetime(v),
    }
}

/// `2001-02-03` at midnight, else `2001-02-03 04:05:06[.ffffff]`.
pub(crate) fn iso_datetime(v: NaiveDateTime) -> String {
    if v.time() == NaiveTime::MIN {
        v.date().format("%Y-%m-%d").to_string()
    } else if v.nanosecond() == 0 {
        v.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        v.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
    }
}

fn describe(cell: Cell<'_>) -> String {
    match cell {
        Cell::Null => "NULL".to_owned(),
        Cell::Text(t) | Cell::Number(t) | Cell::Json(t) | Cell::Error(t) => quote(t),
        other => quote(&to_text(other)),
    }
}

fn mismatch(cell: Cell<'_>, what: &str) -> String {
    format!("{} is not {what}", describe(cell))
}

fn to_bool(cell: Cell<'_>, formats: &ValueFormats) -> Result<bool, String> {
    let value = match cell {
        Cell::Bool(v) => Some(v),
        Cell::Text(t) | Cell::Number(t) => parse_bool(t, formats),
        Cell::Int(v) => bit(v),
        Cell::Float(f) => integral_f64(f).and_then(bit),
        _ => None,
    };
    value.ok_or_else(|| mismatch(cell, "a boolean"))
}

fn bit(v: i64) -> Option<bool> {
    match v {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/// `f` as an `i64` when it is integral and in range.
#[allow(clippy::cast_possible_truncation, clippy::float_cmp)]
pub(crate) fn integral_f64(f: f64) -> Option<i64> {
    // 2^63 as f64; every integral f64 below it in magnitude fits.
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    (f.is_finite() && f.trunc() == f && (-LIMIT..LIMIT).contains(&f)).then_some(f as i64)
}

fn to_i64(cell: Cell<'_>, formats: &ValueFormats) -> Result<i64, String> {
    let value = match cell {
        Cell::Int(v) => Some(v),
        Cell::Bool(v) => Some(i64::from(v)),
        Cell::Float(f) => integral_f64(f),
        Cell::Text(t) => parse_i64(t, formats),
        Cell::Number(t) => parse_i64(t, &PLAIN),
        _ => None,
    };
    value.ok_or_else(|| mismatch(cell, "an integer"))
}

fn to_u64(cell: Cell<'_>, formats: &ValueFormats) -> Result<u64, String> {
    let value = match cell {
        Cell::Int(v) => u64::try_from(v).ok(),
        Cell::Bool(v) => Some(u64::from(v)),
        Cell::Float(f) => integral_f64(f).and_then(|v| u64::try_from(v).ok()),
        Cell::Text(t) => parse_u64(t, formats),
        Cell::Number(t) => parse_u64(t, &PLAIN),
        _ => None,
    };
    value.ok_or_else(|| mismatch(cell, "an unsigned integer"))
}

#[allow(clippy::cast_precision_loss)]
fn to_f64(cell: Cell<'_>, formats: &ValueFormats) -> Result<f64, String> {
    let value = match cell {
        Cell::Float(f) => Some(f),
        Cell::Int(v) => Some(v as f64),
        Cell::Text(t) => parse_f64(t, formats),
        Cell::Number(t) => t.trim().parse().ok(),
        _ => None,
    };
    value.ok_or_else(|| mismatch(cell, "a number"))
}

fn to_decimal(cell: Cell<'_>, formats: &ValueFormats) -> Result<String, String> {
    let value = match cell {
        Cell::Int(v) => Some(v.to_string()),
        Cell::Float(f) if f.is_finite() => canonical_decimal(&f.to_string(), &PLAIN),
        Cell::Text(t) => canonical_decimal(t, formats),
        Cell::Number(t) => canonical_decimal(t, &PLAIN),
        _ => None,
    };
    value.ok_or_else(|| mismatch(cell, "a decimal number"))
}

fn to_json(cell: Cell<'_>) -> Result<String, String> {
    match cell {
        Cell::Json(t) | Cell::Number(t) => Ok(t.to_owned()),
        Cell::Text(t) => {
            let t = t.trim();
            if is_json(t) {
                Ok(t.to_owned())
            } else {
                Err(format!("{} is not JSON", quote(t)))
            }
        }
        Cell::Bool(v) => Ok(v.to_string()),
        Cell::Int(v) => Ok(v.to_string()),
        Cell::Float(f) if f.is_finite() => Ok(f.to_string()),
        Cell::DateTime(v) => Ok(serde_json::Value::String(iso_datetime(v)).to_string()),
        other => Err(mismatch(other, "JSON")),
    }
}

fn to_date(cell: Cell<'_>, formats: &ValueFormats) -> Result<NaiveDate, String> {
    let value = match cell {
        Cell::Text(t) => parse_date(t, formats),
        Cell::DateTime(v) if v.time() == NaiveTime::MIN => Some(v.date()),
        _ => None,
    };
    value.ok_or_else(|| mismatch(cell, "a date"))
}

fn days(date: NaiveDate) -> Result<i32, String> {
    date.num_days_from_ce()
        .checked_sub(UNIX_EPOCH_DAYS_FROM_CE)
        .ok_or_else(|| format!("{date} is out of range"))
}

/// Text with digits below the microsecond (`10:00:00.1234567`) cannot be kept: the load
/// types hold microseconds. Spreadsheet date-times are not checked (their nanoseconds are
/// float rounding, not digits of the source).
fn whole_micros(cell: Cell<'_>, nanosecond: u32) -> Result<(), String> {
    if matches!(cell, Cell::Text(_)) && nanosecond % 1_000 != 0 {
        Err(format!("{} has sub-microsecond digits", describe(cell)))
    } else {
        Ok(())
    }
}

fn to_time(cell: Cell<'_>, formats: &ValueFormats) -> Result<NaiveTime, String> {
    let value = match cell {
        Cell::Text(t) => parse_time(t, formats),
        // Spreadsheet times are date-times on the epoch day (1899-12-30 / 31, 1904-01-01).
        Cell::DateTime(v) if v.date().year() < 1900 || v.date() == epoch_1904() => Some(v.time()),
        _ => None,
    };
    let time = value.ok_or_else(|| mismatch(cell, "a time of day"))?;
    whole_micros(cell, time.nanosecond())?;
    Ok(time)
}

fn epoch_1904() -> NaiveDate {
    NaiveDate::from_ymd_opt(1904, 1, 1).unwrap_or_default()
}

fn micros_of_day(time: NaiveTime) -> i64 {
    i64::from(time.num_seconds_from_midnight()) * 1_000_000
        + i64::from(time.nanosecond().min(999_999_999) / 1_000)
}

fn to_timestamp(cell: Cell<'_>, formats: &ValueFormats, with_zone: bool) -> Result<i64, String> {
    let value = match cell {
        Cell::DateTime(v) => Some(v.and_utc()),
        Cell::Text(t) if with_zone => parse_timestamp_tz(t, formats)
            .map(|v| v.to_utc())
            // Text without an offset is read as UTC.
            .or_else(|| parse_timestamp(t, formats).map(|v| v.and_utc())),
        Cell::Text(t) => parse_timestamp(t, formats).map(|v| v.and_utc()),
        _ => None,
    };
    if let Some(v) = value {
        whole_micros(cell, v.nanosecond())?;
    }
    value.map(|v| v.timestamp_micros()).ok_or_else(|| {
        mismatch(
            cell,
            if with_zone {
                "a timestamp with a time zone"
            } else {
                "a timestamp"
            },
        )
    })
}

#[cfg(test)]
mod tests {
    use arrow_array::cast::AsArray;
    use arrow_array::types::{
        Date32Type, Float64Type, Int64Type, Time64MicrosecondType, TimestampMicrosecondType,
        UInt64Type,
    };
    use arrow_array::{Array, BooleanArray};

    use super::*;
    use crate::arrow_type_for;

    fn build(data_type: &DataType, cells: &[Cell<'_>]) -> (ArrayRef, Vec<Option<String>>) {
        let formats = ValueFormats::default();
        let mut b = ColumnBuilder::new(data_type, cells.len());
        let mut issues = Vec::new();
        for &cell in cells {
            match b.append(cell, &formats) {
                Ok(()) => issues.push(None),
                Err(message) => {
                    b.append_null();
                    issues.push(Some(message));
                }
            }
        }
        let array = b.finish();
        assert_eq!(array.data_type(), &arrow_type_for(data_type));
        assert_eq!(array.len(), cells.len());
        (array, issues)
    }

    #[test]
    fn every_canonical_type_has_its_load_type() {
        for t in [
            DataType::Bool,
            DataType::INT32,
            DataType::Int {
                bits: 64,
                unsigned: true,
            },
            DataType::FLOAT64,
            DataType::Decimal {
                precision: Some(9),
                scale: Some(2),
            },
            DataType::Text,
            DataType::VarChar { length: Some(5) },
            DataType::Json { binary: true },
            DataType::VarBinary { length: None },
            DataType::Date,
            DataType::Time { precision: Some(6) },
            DataType::Timestamp { precision: None },
            DataType::TimestampTz { precision: None },
            DataType::Uuid,
            DataType::Interval,
        ] {
            let (array, _) = build(&t, &[Cell::Null]);
            assert!(array.is_null(0));
        }
    }

    #[test]
    fn integers() {
        let (array, issues) = build(
            &DataType::INT64,
            &[
                Cell::Text("42"),
                Cell::Text("abc"),
                Cell::Int(7),
                Cell::Float(3.0),
                Cell::Float(3.5),
                Cell::Number("9007199254740993"),
                Cell::Bool(true),
                Cell::Error("#DIV/0!"),
            ],
        );
        let ints = array.as_primitive::<Int64Type>();
        assert_eq!(ints.value(0), 42);
        assert!(ints.is_null(1));
        assert_eq!(issues[1].as_deref(), Some("'abc' is not an integer"));
        assert_eq!(ints.value(2), 7);
        assert_eq!(ints.value(3), 3);
        assert_eq!(issues[4].as_deref(), Some("'3.5' is not an integer"));
        assert_eq!(ints.value(5), 9_007_199_254_740_993);
        assert_eq!(ints.value(6), 1);
        assert_eq!(
            issues[7].as_deref(),
            Some("the cell holds the error #DIV/0!")
        );

        let (array, issues) = build(
            &DataType::Int {
                bits: 64,
                unsigned: true,
            },
            &[Cell::Text("18446744073709551615"), Cell::Int(-1)],
        );
        assert_eq!(array.as_primitive::<UInt64Type>().value(0), u64::MAX);
        assert!(issues[1].is_some());
    }

    #[test]
    fn numbers_decimals_booleans() {
        let (array, _) = build(
            &DataType::FLOAT64,
            &[Cell::Text("1.5"), Cell::Int(2), Cell::Number("1e3")],
        );
        let floats = array.as_primitive::<Float64Type>();
        assert!((floats.value(0) - 1.5).abs() < f64::EPSILON);
        assert!((floats.value(1) - 2.0).abs() < f64::EPSILON);
        assert!((floats.value(2) - 1000.0).abs() < f64::EPSILON);

        let (array, issues) = build(
            &DataType::Decimal {
                precision: None,
                scale: None,
            },
            &[
                Cell::Text("12.50"),
                Cell::Number("1e3"),
                Cell::Int(-4),
                Cell::Float(0.1),
                Cell::Text("x"),
            ],
        );
        let text = array.as_string::<i32>();
        assert_eq!(text.value(0), "12.50");
        assert_eq!(text.value(1), "1000");
        assert_eq!(text.value(2), "-4");
        assert_eq!(text.value(3), "0.1");
        assert_eq!(issues[4].as_deref(), Some("'x' is not a decimal number"));

        let (array, issues) = build(
            &DataType::Bool,
            &[Cell::Text("yes"), Cell::Int(0), Cell::Text("2")],
        );
        let bools = array.as_any().downcast_ref::<BooleanArray>().unwrap();
        assert!(bools.value(0));
        assert!(!bools.value(1));
        assert!(issues[2].is_some());
    }

    #[test]
    fn temporal() {
        let at = NaiveDate::from_ymd_opt(2001, 2, 3)
            .unwrap()
            .and_hms_micro_opt(4, 5, 6, 7)
            .unwrap();
        let midnight = at.date().and_time(NaiveTime::MIN);
        let (array, issues) = build(
            &DataType::Date,
            &[
                Cell::Text("2001-02-03"),
                Cell::DateTime(midnight),
                Cell::DateTime(at),
            ],
        );
        let dates = array.as_primitive::<Date32Type>();
        assert_eq!(dates.value(0), 11_356);
        assert_eq!(dates.value(1), 11_356);
        assert!(issues[2].is_some());

        let (array, _) = build(
            &DataType::Time { precision: None },
            &[Cell::Text("04:05:06.000007")],
        );
        assert_eq!(
            array.as_primitive::<Time64MicrosecondType>().value(0),
            14_706_000_007
        );

        let (array, issues) = build(
            &DataType::Timestamp { precision: None },
            &[
                Cell::Text("2001-02-03 04:05:06.000007"),
                Cell::Text("2001-02-03"),
                Cell::DateTime(at),
                Cell::Int(5),
            ],
        );
        let ts = array.as_primitive::<TimestampMicrosecondType>();
        assert_eq!(ts.value(0), at.and_utc().timestamp_micros());
        assert_eq!(ts.value(1), midnight.and_utc().timestamp_micros());
        assert_eq!(ts.value(2), at.and_utc().timestamp_micros());
        assert!(issues[3].is_some());

        let (array, _) = build(
            &DataType::TimestampTz { precision: None },
            &[
                Cell::Text("2001-02-03T06:05:06.000007+02:00"),
                Cell::Text("2001-02-03 04:05:06.000007"),
            ],
        );
        let ts = array.as_primitive::<TimestampMicrosecondType>();
        assert_eq!(ts.value(0), at.and_utc().timestamp_micros());
        assert_eq!(ts.value(1), at.and_utc().timestamp_micros());
        assert_eq!(iso_datetime(midnight), "2001-02-03");
        assert_eq!(iso_datetime(at), "2001-02-03 04:05:06.000007");
    }

    #[test]
    fn text_json_binary_uuid() {
        let (array, _) = build(
            &DataType::Text,
            &[
                Cell::Text("a"),
                Cell::Int(1),
                Cell::Float(3.0),
                Cell::Bool(false),
                Cell::Json("[1,2]"),
            ],
        );
        let text = array.as_string::<i32>();
        assert_eq!(text.value(1), "1");
        assert_eq!(text.value(2), "3");
        assert_eq!(text.value(3), "false");
        assert_eq!(text.value(4), "[1,2]");

        let (array, issues) = build(
            &DataType::Json { binary: false },
            &[
                Cell::Text(" {\"a\": 1} "),
                Cell::Text("nope"),
                Cell::Json("{}"),
            ],
        );
        let text = array.as_string::<i32>();
        assert_eq!(text.value(0), "{\"a\": 1}");
        assert_eq!(issues[1].as_deref(), Some("'nope' is not JSON"));
        assert_eq!(text.value(2), "{}");

        let (array, issues) = build(
            &DataType::VarBinary { length: None },
            &[Cell::Text("0x0102"), Cell::Text("zz")],
        );
        assert_eq!(array.as_binary::<i32>().value(0), &[1_u8, 2][..]);
        assert!(issues[1].is_some());

        let (array, issues) = build(
            &DataType::Uuid,
            &[
                Cell::Text("67e55044-10b1-426f-9247-bb680e5fe0c8"),
                Cell::Text("nope"),
            ],
        );
        assert_eq!(array.as_fixed_size_binary().value(0)[0], 0x67);
        assert_eq!(issues[1].as_deref(), Some("'nope' is not a UUID"));
    }
}
