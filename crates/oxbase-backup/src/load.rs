//! Table rows in the *load schema* (`oxbase_dialect::LoadTarget::schema`): the columns a backup
//! stores, and the conversion of the source driver's Arrow pages into load batches.
//!
//! A backup and its restore are the same engine, so the load columns come straight from the
//! table definition: every stored column, its logical type from the fetcher, its native type
//! kept for loaders that name types. Left out: generated columns, and columns the server
//! maintains and refuses to take values for (SQL Server `rowversion`, period columns), which
//! the engine's `DmlWriter` names by refusing an insert into them ([`server_maintained`]). The driver's pages are converted
//! column by column: an array already in the load type passes through untouched; anything else
//! goes value by value through `oxbase_driver::cell_value` into a builder of the load type (text
//! forms parsed where the driver reports a typed column as text). A value that cannot be
//! represented fails the backup with a message naming the table, column and row: a backup never
//! silently changes data.

use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Date32Builder, FixedSizeBinaryBuilder, Float64Builder,
    Int64Builder, StringBuilder, Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
    UInt64Builder,
};
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::{DataType as ArrowType, SchemaRef, TimeUnit};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use oxbase_dialect::{DmlTarget, DmlWriter, LoadColumn};
use oxbase_driver::cell_value;
use oxbase_types::{
    ColumnDef, ColumnValue, Error, ErrorCategory, KeyKind, ObjectRef, Result, RowEdit, RowKey,
    ServerVersion, TableDef, Value,
};

/// The columns a backup stores for `table`, in table order: every column that is not generated
/// and not server-maintained (`writer`: the engine's grid writer, when it has one). The second
/// list names the columns left out for being server-maintained, with the writer's reason.
pub(crate) fn load_columns(
    table: &TableDef,
    writer: Option<&dyn DmlWriter>,
    version: &ServerVersion,
) -> (Vec<LoadColumn>, Vec<(String, String)>) {
    let mut out = Vec::with_capacity(table.columns.len());
    let mut left_out = Vec::new();
    for c in table.columns.iter().filter(|c| c.generated.is_none()) {
        if let Some(why) = writer.and_then(|w| server_maintained(w, table, c, version)) {
            left_out.push((c.name.clone(), why));
            continue;
        }
        out.push(LoadColumn {
            name: c.name.clone(),
            data_type: c.data_type.clone(),
            native_type: c.native_type.clone(),
            nullable: c.nullable,
        });
    }
    (out, left_out)
}

/// `Some(reason)` when the server maintains `column` and refuses values for it: the engine's
/// `DmlWriter` refuses a one-column insert into it with `InvalidInput` "... cannot be written"
/// (the M3 write-back contract). Identity and auto-increment columns are never asked: restore
/// loads their values on purpose (`BulkLoader::prepare`).
pub(crate) fn server_maintained(
    writer: &dyn DmlWriter,
    table: &TableDef,
    column: &ColumnDef,
    version: &ServerVersion,
) -> Option<String> {
    if column.auto_increment || column.identity.is_some() {
        return None;
    }
    let key = RowKey::new(
        table.obj.clone(),
        KeyKind::Unique,
        vec![column.name.clone()],
    );
    let edit = RowEdit::Insert {
        values: vec![ColumnValue::new(column.name.clone(), Value::Null)],
    };
    let target = DmlTarget {
        table,
        key: &key,
        version,
        read_back: &[],
    };
    match writer.plan(&edit, &target) {
        Err(e)
            if e.category() == ErrorCategory::InvalidInput
                && e.to_string().contains("cannot be written") =>
        {
            Some(e.to_string())
        }
        _ => None,
    }
}

/// Where a value comes from, for messages.
struct At<'a> {
    table: &'a ObjectRef,
    column: &'a str,
    row: u64,
}

impl At<'_> {
    fn error(&self, what: impl std::fmt::Display) -> Error {
        Error::invalid_input(format!(
            "{}, column {}, row {}: {what}",
            self.table.display_name(),
            self.column,
            self.row
        ))
    }
}

/// `batch` (the source driver's page of the `SELECT` of `columns`, same column order) in the
/// load `schema`. `first_row` is the 0-based table row of the batch's first row (messages).
pub(crate) fn to_load_batch(
    table: &ObjectRef,
    batch: &RecordBatch,
    schema: &SchemaRef,
    columns: &[LoadColumn],
    first_row: u64,
) -> Result<RecordBatch> {
    if batch.num_columns() != columns.len() || schema.fields().len() != columns.len() {
        return Err(Error::internal(format!(
            "{}: the query returned {} columns, the backup stores {}",
            table.display_name(),
            batch.num_columns(),
            columns.len()
        )));
    }
    let arrays = batch
        .columns()
        .iter()
        .zip(schema.fields())
        .zip(columns)
        .map(|((array, field), column)| {
            convert(array, field.data_type(), table, &column.name, first_row)
        })
        .collect::<Result<Vec<_>>>()?;
    RecordBatch::try_new(Arc::clone(schema), arrays)
        .map_err(|e| Error::internal(format!("{}: {e}", table.display_name())))
}

fn convert(
    array: &ArrayRef,
    target: &ArrowType,
    table: &ObjectRef,
    column: &str,
    first_row: u64,
) -> Result<ArrayRef> {
    if array.data_type() == target {
        return Ok(Arc::clone(array));
    }
    let len = array.len();
    let values = (0..len).map(move |i| {
        let at = At {
            table,
            column,
            row: first_row + u64::try_from(i).unwrap_or(u64::MAX) + 1,
        };
        cell_value(array.as_ref(), i).map(|v| (v, at)).map_err(|e| {
            Error::invalid_input(format!("{}, column {column}: {e}", table.display_name()))
        })
    });
    let out: ArrayRef = match target {
        ArrowType::Boolean => {
            let mut b = BooleanBuilder::with_capacity(len);
            for item in values {
                let (v, at) = item?;
                b.append_option(to_bool(v, &at)?);
            }
            Arc::new(b.finish())
        }
        ArrowType::Int64 => {
            let mut b = Int64Builder::with_capacity(len);
            for item in values {
                let (v, at) = item?;
                b.append_option(to_i64(v, &at)?);
            }
            Arc::new(b.finish())
        }
        ArrowType::UInt64 => {
            let mut b = UInt64Builder::with_capacity(len);
            for item in values {
                let (v, at) = item?;
                b.append_option(to_u64(v, &at)?);
            }
            Arc::new(b.finish())
        }
        ArrowType::Float64 => {
            let mut b = Float64Builder::with_capacity(len);
            for item in values {
                let (v, at) = item?;
                b.append_option(to_f64(v, &at)?);
            }
            Arc::new(b.finish())
        }
        ArrowType::Utf8 => {
            let mut b = StringBuilder::with_capacity(len, len * 8);
            for item in values {
                let (v, _) = item?;
                b.append_option(to_text(v));
            }
            Arc::new(b.finish())
        }
        ArrowType::Binary => {
            let mut b = BinaryBuilder::with_capacity(len, len * 8);
            for item in values {
                let (v, at) = item?;
                b.append_option(to_bytes(v, &at)?);
            }
            Arc::new(b.finish())
        }
        ArrowType::Date32 => {
            let mut b = Date32Builder::with_capacity(len);
            for item in values {
                let (v, at) = item?;
                b.append_option(to_date(v, &at)?.map(days_since_epoch).transpose()?);
            }
            Arc::new(b.finish())
        }
        ArrowType::Time64(TimeUnit::Microsecond) => {
            let mut b = Time64MicrosecondBuilder::with_capacity(len);
            for item in values {
                let (v, at) = item?;
                b.append_option(to_time(v, &at)?.map(micros_of_day));
            }
            Arc::new(b.finish())
        }
        ArrowType::Timestamp(TimeUnit::Microsecond, tz) => {
            let mut b = TimestampMicrosecondBuilder::with_capacity(len);
            for item in values {
                let (v, at) = item?;
                let micros = if tz.is_some() {
                    to_instant(v, &at)?
                } else {
                    to_timestamp(v, &at)?
                };
                b.append_option(micros.map(|t| t.and_utc().timestamp_micros()));
            }
            let array = b.finish();
            match tz {
                Some(tz) => Arc::new(array.with_timezone(Arc::clone(tz))),
                None => Arc::new(array),
            }
        }
        ArrowType::FixedSizeBinary(16) => {
            let mut b = FixedSizeBinaryBuilder::with_capacity(len, 16);
            for item in values {
                let (v, at) = item?;
                match to_uuid(v, &at)? {
                    Some(u) => b.append_value(u.as_bytes()).map_err(|e| at.error(e))?,
                    None => b.append_null(),
                }
            }
            Arc::new(b.finish())
        }
        other => {
            return Err(Error::unsupported(format!(
                "{}, column {column}: Oxbase cannot store values as {other}",
                table.display_name()
            )));
        }
    };
    Ok(out)
}

fn to_bool(v: Value, at: &At<'_>) -> Result<Option<bool>> {
    Ok(match v {
        Value::Null => None,
        Value::Bool(b) => Some(b),
        Value::Int(i) => Some(i != 0),
        Value::UInt(u) => Some(u != 0),
        Value::Text(t) | Value::Decimal(t) => match t.trim().to_ascii_lowercase().as_str() {
            "t" | "true" | "1" | "y" | "yes" | "on" => Some(true),
            "f" | "false" | "0" | "n" | "no" | "off" => Some(false),
            _ => return Err(at.error(format!("'{t}' is not a boolean"))),
        },
        other => return Err(at.error(format!("{other:?} is not a boolean"))),
    })
}

fn to_i64(v: Value, at: &At<'_>) -> Result<Option<i64>> {
    Ok(match v {
        Value::Null => None,
        Value::Int(i) => Some(i),
        Value::Bool(b) => Some(i64::from(b)),
        Value::UInt(u) => {
            Some(i64::try_from(u).map_err(|_| at.error(format!("{u} is out of range")))?)
        }
        Value::Text(t) | Value::Decimal(t) => {
            Some(parse_integer(&t).ok_or_else(|| at.error(format!("'{t}' is not an integer")))?)
        }
        other => return Err(at.error(format!("{other:?} is not an integer"))),
    })
}

fn to_u64(v: Value, at: &At<'_>) -> Result<Option<u64>> {
    Ok(match v {
        Value::Null => None,
        Value::UInt(u) => Some(u),
        Value::Int(i) => {
            Some(u64::try_from(i).map_err(|_| at.error(format!("{i} is out of range")))?)
        }
        Value::Bool(b) => Some(u64::from(b)),
        Value::Text(t) | Value::Decimal(t) => Some(
            t.trim()
                .parse::<u64>()
                .map_err(|_| at.error(format!("'{t}' is not an unsigned integer")))?,
        ),
        other => return Err(at.error(format!("{other:?} is not an integer"))),
    })
}

/// An integer, also from text with a zero fraction (`12.000`).
fn parse_integer(text: &str) -> Option<i64> {
    let t = text.trim();
    if let Ok(i) = t.parse::<i64>() {
        return Some(i);
    }
    let (whole, fraction) = t.split_once('.')?;
    if fraction.bytes().all(|b| b == b'0') {
        whole.parse().ok()
    } else {
        None
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "an integer read into a floating-point column is as exact as the column"
)]
fn to_f64(v: Value, at: &At<'_>) -> Result<Option<f64>> {
    Ok(match v {
        Value::Null => None,
        Value::Float(f) => Some(f),
        Value::Int(i) => Some(i as f64),
        Value::UInt(u) => Some(u as f64),
        Value::Text(t) | Value::Decimal(t) => {
            Some(parse_float(&t).ok_or_else(|| at.error(format!("'{t}' is not a number")))?)
        }
        other => return Err(at.error(format!("{other:?} is not a number"))),
    })
}

fn parse_float(text: &str) -> Option<f64> {
    let t = text.trim();
    match t.to_ascii_lowercase().as_str() {
        "nan" => Some(f64::NAN),
        "infinity" | "+infinity" | "inf" => Some(f64::INFINITY),
        "-infinity" | "-inf" => Some(f64::NEG_INFINITY),
        _ => t.parse().ok(),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The canonical text of a value (decimals exact, JSON as JSON text).
fn to_text(v: Value) -> Option<String> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Decimal(t) | Value::Text(t) => t,
        Value::Bytes(b) => match String::from_utf8(b) {
            Ok(text) => text,
            Err(e) => hex(e.as_bytes()),
        },
        Value::Date(d) => d.format("%Y-%m-%d").to_string(),
        Value::Time(t) => t.format("%H:%M:%S%.f").to_string(),
        Value::Timestamp(t) => t.format("%Y-%m-%d %H:%M:%S%.f").to_string(),
        Value::TimestampTz(t) => t.format("%Y-%m-%d %H:%M:%S%.f%:z").to_string(),
        Value::Uuid(u) => u.hyphenated().to_string(),
        Value::Json(j) => j.to_string(),
    })
}

fn to_bytes(v: Value, at: &At<'_>) -> Result<Option<Vec<u8>>> {
    Ok(match v {
        Value::Null => None,
        Value::Bytes(b) => Some(b),
        Value::Text(t) => Some(t.into_bytes()),
        Value::Uuid(u) => Some(u.as_bytes().to_vec()),
        other => return Err(at.error(format!("{other:?} is not binary data"))),
    })
}

fn to_date(v: Value, at: &At<'_>) -> Result<Option<NaiveDate>> {
    Ok(match v {
        Value::Null => None,
        Value::Date(d) => Some(d),
        Value::Timestamp(t) => Some(t.date()),
        Value::Text(t) => Some(
            NaiveDate::parse_from_str(t.trim(), "%Y-%m-%d")
                .map_err(|_| at.error(format!("'{t}' is not a date")))?,
        ),
        other => return Err(at.error(format!("{other:?} is not a date"))),
    })
}

/// Days since 1970-01-01 (`NaiveDate::default()`).
fn days_since_epoch(d: NaiveDate) -> Result<i32> {
    let days = d.signed_duration_since(NaiveDate::default()).num_days();
    i32::try_from(days).map_err(|_| Error::invalid_input(format!("the date {d} is out of range")))
}

fn to_time(v: Value, at: &At<'_>) -> Result<Option<NaiveTime>> {
    Ok(match v {
        Value::Null => None,
        Value::Time(t) => Some(t),
        Value::Text(t) => Some(
            NaiveTime::parse_from_str(t.trim(), "%H:%M:%S%.f")
                .map_err(|_| at.error(format!("'{t}' is not a time of day")))?,
        ),
        other => return Err(at.error(format!("{other:?} is not a time of day"))),
    })
}

fn micros_of_day(t: NaiveTime) -> i64 {
    i64::from(t.num_seconds_from_midnight()) * 1_000_000 + i64::from(t.nanosecond() / 1_000)
}

fn parse_naive(text: &str) -> Option<NaiveDateTime> {
    let t = text.trim();
    ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(t, f).ok())
        .or_else(|| {
            NaiveDate::parse_from_str(t, "%Y-%m-%d")
                .ok()
                .map(|d| d.and_time(NaiveTime::MIN))
        })
}

/// A timestamp without zone.
fn to_timestamp(v: Value, at: &At<'_>) -> Result<Option<NaiveDateTime>> {
    Ok(match v {
        Value::Null => None,
        Value::Timestamp(t) => Some(t),
        Value::Date(d) => Some(d.and_time(NaiveTime::MIN)),
        Value::TimestampTz(t) => Some(t.naive_utc()),
        Value::Text(t) => {
            Some(parse_naive(&t).ok_or_else(|| at.error(format!("'{t}' is not a timestamp")))?)
        }
        other => return Err(at.error(format!("{other:?} is not a timestamp"))),
    })
}

/// An instant, as its UTC date and time.
fn to_instant(v: Value, at: &At<'_>) -> Result<Option<NaiveDateTime>> {
    Ok(match v {
        Value::Null => None,
        Value::TimestampTz(t) => Some(t.naive_utc()),
        Value::Timestamp(t) => Some(t),
        Value::Text(t) => {
            let trimmed = t.trim();
            let zoned = ["%Y-%m-%d %H:%M:%S%.f%#z", "%Y-%m-%dT%H:%M:%S%.f%#z"]
                .iter()
                .find_map(|f| DateTime::parse_from_str(trimmed, f).ok())
                .or_else(|| DateTime::parse_from_rfc3339(trimmed).ok());
            match zoned {
                Some(z) => Some(z.naive_utc()),
                None => Some(
                    parse_naive(trimmed)
                        .ok_or_else(|| at.error(format!("'{t}' is not a timestamp")))?,
                ),
            }
        }
        other => return Err(at.error(format!("{other:?} is not a timestamp"))),
    })
}

fn to_uuid(v: Value, at: &At<'_>) -> Result<Option<uuid::Uuid>> {
    Ok(match v {
        Value::Null => None,
        Value::Uuid(u) => Some(u),
        Value::Text(t) => Some(
            uuid::Uuid::parse_str(t.trim())
                .map_err(|_| at.error(format!("'{t}' is not a UUID")))?,
        ),
        Value::Bytes(b) => {
            Some(uuid::Uuid::from_slice(&b).map_err(|_| at.error("the bytes are not a UUID"))?)
        }
        other => return Err(at.error(format!("{other:?} is not a UUID"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::cast::AsArray;
    use arrow_array::types::{
        Date32Type, Int64Type, Time64MicrosecondType, TimestampMicrosecondType,
    };
    use arrow_array::{Decimal128Array, Int32Array, StringArray};
    use arrow_schema::{Field, Schema};
    use oxbase_dialect::{DmlPlan, LoadTarget};
    use oxbase_types::{BoundStatement, DataType, GeneratedColumn};
    use pretty_assertions::assert_eq;

    fn table() -> TableDef {
        let mut t = TableDef::new(ObjectRef::table(None, Some("s"), "t"));
        t.columns = vec![
            ColumnDef::new("id", DataType::INT64).not_null(),
            ColumnDef::new(
                "amount",
                DataType::Decimal {
                    precision: Some(12),
                    scale: Some(2),
                },
            ),
            ColumnDef::new("day", DataType::Date),
            ColumnDef::new("at", DataType::Time { precision: None }),
            ColumnDef::new("ts", DataType::TimestampTz { precision: None }),
            ColumnDef::new("u", DataType::Uuid),
        ];
        let mut generated = ColumnDef::new("twice", DataType::INT64);
        generated.generated = Some(GeneratedColumn {
            expression: "id * 2".into(),
            stored: true,
        });
        t.columns.push(generated);
        t
    }

    #[test]
    fn stored_columns_convert_into_the_load_schema() {
        let t = table();
        let version = ServerVersion::new(1, 0, 0);
        let (columns, left_out) = load_columns(&t, None, &version);
        assert_eq!(columns.len(), 6, "the generated column is left out");
        assert_eq!(left_out, Vec::<(String, String)>::new());
        let schema = LoadTarget {
            table: &t.obj,
            columns: &columns,
            version: &version,
        }
        .schema();
        let source = Arc::new(Schema::new(vec![
            Field::new("id", ArrowType::Int32, false),
            Field::new("amount", ArrowType::Decimal128(12, 2), true),
            Field::new("day", ArrowType::Utf8, true),
            Field::new("at", ArrowType::Utf8, true),
            Field::new("ts", ArrowType::Utf8, true),
            Field::new("u", ArrowType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            source,
            vec![
                Arc::new(Int32Array::from(vec![1, 2])),
                Arc::new(
                    Decimal128Array::from(vec![Some(1234), None])
                        .with_precision_and_scale(12, 2)
                        .unwrap(),
                ),
                Arc::new(StringArray::from(vec![Some("2026-03-01"), None])),
                Arc::new(StringArray::from(vec![Some("23:59:59.999999"), None])),
                Arc::new(StringArray::from(vec![
                    Some("2026-03-01 10:00:00+05:30"),
                    None,
                ])),
                Arc::new(StringArray::from(vec![
                    Some("6f1c2a3e-0000-4000-8000-00000000abcd"),
                    None,
                ])),
            ],
        )
        .unwrap();
        let out = to_load_batch(&t.obj, &batch, &schema, &columns, 0).unwrap();
        let check = LoadTarget {
            table: &t.obj,
            columns: &columns,
            version: &version,
        };
        check.check_batch(&out).unwrap();
        assert_eq!(out.column(0).as_primitive::<Int64Type>().value(1), 2);
        assert_eq!(out.column(1).as_string::<i32>().value(0), "12.34");
        assert!(out.column(1).is_null(1));
        assert_eq!(
            out.column(2).as_primitive::<Date32Type>().value(0),
            days_since_epoch(NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()).unwrap()
        );
        assert_eq!(
            out.column(3)
                .as_primitive::<Time64MicrosecondType>()
                .value(0),
            86_399_999_999
        );
        let utc = NaiveDate::from_ymd_opt(2026, 3, 1)
            .unwrap()
            .and_hms_opt(4, 30, 0)
            .unwrap()
            .and_utc()
            .timestamp_micros();
        assert_eq!(
            out.column(4)
                .as_primitive::<TimestampMicrosecondType>()
                .value(0),
            utc
        );
        assert_eq!(
            uuid::Uuid::from_slice(out.column(5).as_fixed_size_binary().value(0))
                .unwrap()
                .to_string(),
            "6f1c2a3e-0000-4000-8000-00000000abcd"
        );
    }

    /// Refuses writes to `rv` like an engine whose server maintains it.
    struct Refusing;

    impl DmlWriter for Refusing {
        fn plan(&self, edit: &RowEdit, target: &DmlTarget<'_>) -> Result<DmlPlan> {
            target.key.validate(edit)?;
            if let RowEdit::Insert { values } = edit
                && values.iter().any(|v| v.column == "rv")
            {
                return Err(Error::invalid_input(
                    "column 'rv' is a rowversion column and cannot be written",
                ));
            }
            if let RowEdit::Insert { values } = edit
                && values.iter().any(|v| v.column == "odd")
            {
                return Err(Error::invalid_input(
                    "column 'odd': 'x y' is not a collation",
                ));
            }
            Ok(DmlPlan::write_only(BoundStatement::new(
                "INSERT",
                Vec::new(),
            )))
        }
    }

    #[test]
    fn server_maintained_columns_are_left_out() {
        let mut t = TableDef::new(ObjectRef::table(None, Some("dbo"), "t"));
        let mut id = ColumnDef::new("id", DataType::INT64).not_null();
        id.auto_increment = true;
        t.columns = vec![
            id,
            ColumnDef::new("rv", DataType::Binary { length: Some(8) }).not_null(),
            ColumnDef::new("odd", DataType::Text),
            ColumnDef::new("name", DataType::Text),
        ];
        let version = ServerVersion::new(16, 0, 0);
        let (columns, left_out) = load_columns(&t, Some(&Refusing), &version);
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "odd", "name"], "only refusals to write count");
        assert_eq!(left_out.len(), 1);
        assert_eq!(left_out[0].0, "rv");
    }

    #[test]
    fn values_that_do_not_fit_fail_with_their_place() {
        let mut t = TableDef::new(ObjectRef::table(None, None, "t"));
        t.columns = vec![ColumnDef::new("n", DataType::INT64)];
        let version = ServerVersion::new(1, 0, 0);
        let (columns, _) = load_columns(&t, None, &version);
        let schema = LoadTarget {
            table: &t.obj,
            columns: &columns,
            version: &version,
        }
        .schema();
        let source = Arc::new(Schema::new(vec![Field::new("n", ArrowType::Utf8, true)]));
        let batch = RecordBatch::try_new(
            source,
            vec![Arc::new(StringArray::from(vec!["1", "2.000", "x"]))],
        )
        .unwrap();
        let err = to_load_batch(&t.obj, &batch, &schema, &columns, 10).unwrap_err();
        assert!(err.to_string().contains("column n, row 13"), "{err}");
    }
}
