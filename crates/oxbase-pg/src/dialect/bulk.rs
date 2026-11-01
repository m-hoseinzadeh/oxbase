//! The PostgreSQL bulk loader (M5, `docs/m5-contracts.md` §4.3): `COPY ... FROM STDIN` in text
//! format on the import's own session.
//!
//! # How a batch is loaded
//!
//! `COPY <schema>.<table> (<columns>) FROM STDIN`, names quoted by [`PgQuoting`], run on the
//! client behind the session's transaction (or connection) through [`CopySession`], which the
//! loader reaches with `Tx::as_any` / `Connection::as_any`. Rows are encoded into chunks of about
//! [`COPY_CHUNK`] bytes while they are sent, so a batch is never encoded as a whole.
//!
//! Text format, one line per row, fields separated by tabs:
//!
//! | Load type | Field text |
//! |---|---|
//! | NULL (any type) | `\N` |
//! | `Boolean` | `t` / `f` |
//! | `Int64`, `UInt64` | decimal digits |
//! | `Float64` | shortest round-trip form (`0.1`, `1e-7`), `NaN`, `Infinity`, `-Infinity` |
//! | `Utf8` (text, decimals, JSON, enums, arrays, intervals, ...) | the text with `\` `\t` `\n` `\r` escaped as `\\` `\t` `\n` `\r` |
//! | `Binary` | `\\x` + lowercase hex (the escaped form of bytea's `\x...` input) |
//! | `Date32` | `YYYY-MM-DD`, years before 1 as `YYYY-MM-DD BC` |
//! | `Time64(µs)` | `HH:MM:SS.ffffff` (`24:00:00` allowed) |
//! | `Timestamp(µs)` | `YYYY-MM-DD HH:MM:SS.ffffff` |
//! | `Timestamp(µs, "+00:00")` | the same in UTC with `+00` |
//! | `FixedSizeBinary(16)` | hyphenated UUID |
//!
//! Text values reach the server as the column's input text, so every type the server can parse
//! from text loads (domains, enums, arrays, ranges, `jsonb`), with the server's own checks.
//!
//! # Failure and cancel
//!
//! A row the server refuses fails the whole `COPY` (the call), and the caller rolls back. A
//! cancel abandons the `COPY` at once (`CopyFail`, plus cancel requests if the server does not
//! stop by itself; see [`CopySession`]) and returns `Error::Cancelled`.
//!
//! When the session gives no access to the client (not a connection of this driver), the loader
//! falls back to the generic multi-row [`InsertLoader`].

use std::fmt::Write as _;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Float64Type, Int64Type, Time64MicrosecondType, TimestampMicrosecondType, UInt64Type,
};
use arrow_array::{
    Array, BinaryArray, BooleanArray, Date32Array, FixedSizeBinaryArray, Float64Array, Int64Array,
    RecordBatch, StringArray, Time64MicrosecondArray, TimestampMicrosecondArray, UInt64Array,
};
use arrow_schema::{DataType as ArrowType, TimeUnit};
use async_trait::async_trait;
use chrono::{Datelike, NaiveDate};
use oxbase_dialect::bulk::{BulkLimits, BulkLoader, BulkMethod, LoadSession, LoadTarget};
use oxbase_dialect::{InsertLoader, PgQuoting, Quoting};
use oxbase_driver::CancellationToken;
use oxbase_types::{Engine, Error, Result, ServerVersion};

use crate::conn::{COPY_CHUNK, CopySession};

/// Days from 0001-01-01 (day 1 of the common era) to 1970-01-01.
const UNIX_EPOCH_DAYS_FROM_CE: i32 = 719_163;
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// The PostgreSQL [`BulkLoader`]: [`BulkMethod::CopyText`]. See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct PgBulkLoader;

impl PgBulkLoader {
    /// The `COPY` statement for `target`.
    pub fn copy_statement(target: &LoadTarget<'_>) -> String {
        let q = PgQuoting;
        let columns: Vec<String> = target
            .columns
            .iter()
            .map(|c| q.quote_ident(&c.name))
            .collect();
        format!(
            "COPY {} ({}) FROM STDIN",
            q.qualify(target.table),
            columns.join(", ")
        )
    }

    /// Rows `rows` of `batch` (load schema) in `COPY` text format, appended to `out`: one line
    /// per row. Exposed for tests; [`BulkLoader::load`] encodes chunk by chunk.
    pub fn encode_rows(
        batch: &RecordBatch,
        rows: std::ops::Range<usize>,
        out: &mut String,
    ) -> Result<()> {
        let columns = CopyColumns::new(batch)?;
        for row in rows {
            columns.write_row(out, row)?;
        }
        Ok(())
    }
}

#[async_trait]
impl BulkLoader for PgBulkLoader {
    fn method(&self, _version: &ServerVersion) -> BulkMethod {
        BulkMethod::CopyText
    }

    fn limits(&self, _version: &ServerVersion) -> BulkLimits {
        BulkLimits::for_engine(Engine::Postgres)
    }

    async fn load(
        &self,
        session: LoadSession<'_>,
        target: &LoadTarget<'_>,
        batch: &RecordBatch,
        cancel: CancellationToken,
    ) -> Result<u64> {
        target.check_batch(batch)?;
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if target.columns.is_empty() {
            return Err(Error::invalid_input("a load needs at least one column"));
        }
        let any = match session.tx {
            Some(tx) => tx.as_any(),
            None => session.connection.as_any(),
        };
        let Some(copy) = any.and_then(CopySession::from_any) else {
            return InsertLoader::new(Arc::new(PgQuoting), self.limits(target.version))
                .load(session, target, batch, cancel)
                .await;
        };
        if batch.num_rows() == 0 {
            return Ok(0);
        }
        let columns = CopyColumns::new(batch)?;
        let total = batch.num_rows();
        let mut next = 0_usize;
        let fill = |out: &mut String| -> Result<bool> {
            while next < total && out.len() < COPY_CHUNK {
                columns.write_row(out, next)?;
                next += 1;
            }
            Ok(next < total)
        };
        let sql = Self::copy_statement(target);
        copy.copy_in(&sql, fill, &cancel).await
    }
}

/// One load-schema column, typed for encoding.
enum CopyColumn<'a> {
    Bool(&'a BooleanArray),
    Int(&'a Int64Array),
    UInt(&'a UInt64Array),
    Float(&'a Float64Array),
    Text(&'a StringArray),
    Bytes(&'a BinaryArray),
    Date(&'a Date32Array),
    Time(&'a Time64MicrosecondArray),
    Timestamp(&'a TimestampMicrosecondArray, bool),
    Uuid(&'a FixedSizeBinaryArray),
}

struct CopyColumns<'a>(Vec<CopyColumn<'a>>);

impl<'a> CopyColumns<'a> {
    fn new(batch: &'a RecordBatch) -> Result<Self> {
        batch
            .columns()
            .iter()
            .map(|array| column(array.as_ref()))
            .collect::<Result<Vec<_>>>()
            .map(Self)
    }

    fn write_row(&self, out: &mut String, row: usize) -> Result<()> {
        for (pos, column) in self.0.iter().enumerate() {
            if pos > 0 {
                out.push('\t');
            }
            column.write(out, row)?;
        }
        out.push('\n');
        Ok(())
    }
}

fn unexpected(array: &dyn Array) -> Error {
    Error::internal(format!(
        "COPY cannot encode a {} load column",
        array.data_type()
    ))
}

fn column(array: &dyn Array) -> Result<CopyColumn<'_>> {
    let column = match array.data_type() {
        ArrowType::Boolean => array.as_boolean_opt().map(CopyColumn::Bool),
        ArrowType::Int64 => array.as_primitive_opt::<Int64Type>().map(CopyColumn::Int),
        ArrowType::UInt64 => array.as_primitive_opt::<UInt64Type>().map(CopyColumn::UInt),
        ArrowType::Float64 => array
            .as_primitive_opt::<Float64Type>()
            .map(CopyColumn::Float),
        ArrowType::Utf8 => array.as_string_opt::<i32>().map(CopyColumn::Text),
        ArrowType::Binary => array.as_binary_opt::<i32>().map(CopyColumn::Bytes),
        ArrowType::Date32 => array.as_primitive_opt::<Date32Type>().map(CopyColumn::Date),
        ArrowType::Time64(TimeUnit::Microsecond) => array
            .as_primitive_opt::<Time64MicrosecondType>()
            .map(CopyColumn::Time),
        ArrowType::Timestamp(TimeUnit::Microsecond, tz) => {
            let zoned = tz.is_some();
            array
                .as_primitive_opt::<TimestampMicrosecondType>()
                .map(|a| CopyColumn::Timestamp(a, zoned))
        }
        ArrowType::FixedSizeBinary(16) => array.as_fixed_size_binary_opt().map(CopyColumn::Uuid),
        _ => None,
    };
    column.ok_or_else(|| unexpected(array))
}

impl CopyColumn<'_> {
    fn is_null(&self, row: usize) -> bool {
        match self {
            Self::Bool(a) => a.is_null(row),
            Self::Int(a) => a.is_null(row),
            Self::UInt(a) => a.is_null(row),
            Self::Float(a) => a.is_null(row),
            Self::Text(a) => a.is_null(row),
            Self::Bytes(a) => a.is_null(row),
            Self::Date(a) => a.is_null(row),
            Self::Time(a) => a.is_null(row),
            Self::Timestamp(a, _) => a.is_null(row),
            Self::Uuid(a) => a.is_null(row),
        }
    }

    fn write(&self, out: &mut String, row: usize) -> Result<()> {
        if self.is_null(row) {
            out.push_str("\\N");
            return Ok(());
        }
        match self {
            Self::Bool(a) => out.push(if a.value(row) { 't' } else { 'f' }),
            Self::Int(a) => {
                let _ = write!(out, "{}", a.value(row));
            }
            Self::UInt(a) => {
                let _ = write!(out, "{}", a.value(row));
            }
            Self::Float(a) => write_float(out, a.value(row)),
            Self::Text(a) => write_text(out, a.value(row)),
            Self::Bytes(a) => write_bytes(out, a.value(row)),
            Self::Date(a) => write_date(out, a.value(row))?,
            Self::Time(a) => write_time(out, a.value(row))?,
            Self::Timestamp(a, zoned) => write_timestamp(out, a.value(row), *zoned)?,
            Self::Uuid(a) => write_uuid(out, a.value(row))?,
        }
        Ok(())
    }
}

/// Shortest round-trip text of `v`; PostgreSQL's spellings of the non-finite values.
pub(crate) fn write_float(out: &mut String, v: f64) {
    if v.is_nan() {
        out.push_str("NaN");
    } else if v.is_infinite() {
        out.push_str(if v > 0.0 { "Infinity" } else { "-Infinity" });
    } else {
        let _ = write!(out, "{v:?}");
    }
}

/// `text` with the four characters `COPY` text format gives a meaning escaped.
pub(crate) fn write_text(out: &mut String, text: &str) {
    if !text
        .bytes()
        .any(|b| matches!(b, b'\\' | b'\t' | b'\n' | b'\r'))
    {
        out.push_str(text);
        return;
    }
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
}

/// `\\x` + hex: after `COPY` unescapes `\\`, the server reads bytea hex input `\x...`.
pub(crate) fn write_bytes(out: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.reserve(bytes.len() * 2 + 3);
    out.push_str("\\\\x");
    for b in bytes {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
}

fn date_of(days: i32) -> Result<NaiveDate> {
    days.checked_add(UNIX_EPOCH_DAYS_FROM_CE)
        .and_then(NaiveDate::from_num_days_from_ce_opt)
        .ok_or_else(|| Error::invalid_input(format!("date out of range ({days} days from 1970)")))
}

/// `YYYY-MM-DD`; returns whether the date is BC (the caller appends ` BC` at the end).
fn write_ymd(out: &mut String, date: NaiveDate) -> bool {
    let year = date.year();
    let (shown, bc) = if year >= 1 {
        (year, false)
    } else {
        (1 - year, true)
    };
    let _ = write!(out, "{shown:04}-{:02}-{:02}", date.month(), date.day());
    bc
}

pub(crate) fn write_date(out: &mut String, days: i32) -> Result<()> {
    if write_ymd(out, date_of(days)?) {
        out.push_str(" BC");
    }
    Ok(())
}

fn write_clock(out: &mut String, micros: i64) {
    let secs = micros / 1_000_000;
    let _ = write!(
        out,
        "{:02}:{:02}:{:02}.{:06}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        micros % 1_000_000
    );
}

pub(crate) fn write_time(out: &mut String, micros: i64) -> Result<()> {
    if !(0..=MICROS_PER_DAY).contains(&micros) {
        return Err(Error::invalid_input(format!(
            "time of day out of range ({micros} µs)"
        )));
    }
    write_clock(out, micros);
    Ok(())
}

pub(crate) fn write_timestamp(out: &mut String, micros: i64, zoned: bool) -> Result<()> {
    let days = i32::try_from(micros.div_euclid(MICROS_PER_DAY))
        .map_err(|_| Error::invalid_input(format!("timestamp out of range ({micros} µs)")))?;
    let bc = write_ymd(out, date_of(days)?);
    out.push(' ');
    write_clock(out, micros.rem_euclid(MICROS_PER_DAY));
    if zoned {
        out.push_str("+00");
    }
    if bc {
        out.push_str(" BC");
    }
    Ok(())
}

pub(crate) fn write_uuid(out: &mut String, bytes: &[u8]) -> Result<()> {
    let uuid = uuid::Uuid::from_slice(bytes)
        .map_err(|e| Error::internal(format!("bad UUID bytes: {e}")))?;
    let _ = write!(out, "{}", uuid.hyphenated());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::ArrayRef;
    use oxbase_dialect::bulk::LoadColumn;
    use oxbase_types::{DataType, ObjectRef};

    fn text(f: impl FnOnce(&mut String)) -> String {
        let mut out = String::new();
        f(&mut out);
        out
    }

    #[test]
    fn field_forms() {
        assert_eq!(text(|o| write_text(o, "plain")), "plain");
        assert_eq!(
            text(|o| write_text(o, "a\\b\tc\nd\re \\N 🚀")),
            "a\\\\b\\tc\\nd\\re \\\\N 🚀"
        );
        assert_eq!(text(|o| write_bytes(o, &[0, 0xab, 0xff])), "\\\\x00abff");
        assert_eq!(text(|o| write_bytes(o, &[])), "\\\\x");
        assert_eq!(text(|o| write_float(o, 0.1)), "0.1");
        assert_eq!(text(|o| write_float(o, -2.5e-7)), "-2.5e-7");
        assert_eq!(text(|o| write_float(o, f64::NAN)), "NaN");
        assert_eq!(text(|o| write_float(o, f64::NEG_INFINITY)), "-Infinity");
        assert_eq!(text(|o| write_date(o, 0).unwrap()), "1970-01-01");
        assert_eq!(text(|o| write_date(o, -719_163).unwrap()), "0001-12-31 BC");
        assert_eq!(text(|o| write_date(o, 2_932_896).unwrap()), "9999-12-31");
        assert!(write_date(&mut String::new(), i32::MAX).is_err());
        assert_eq!(
            text(|o| write_time(o, 45_296_000_042).unwrap()),
            "12:34:56.000042"
        );
        assert_eq!(
            text(|o| write_time(o, MICROS_PER_DAY).unwrap()),
            "24:00:00.000000"
        );
        assert!(write_time(&mut String::new(), -1).is_err());
        assert_eq!(
            text(|o| write_timestamp(o, 946_684_800_000_001, false).unwrap()),
            "2000-01-01 00:00:00.000001"
        );
        assert_eq!(
            text(|o| write_timestamp(o, -1, true).unwrap()),
            "1969-12-31 23:59:59.999999+00"
        );
        assert_eq!(
            text(|o| write_uuid(o, &[0x12; 16]).unwrap()),
            "12121212-1212-1212-1212-121212121212"
        );
    }

    #[test]
    fn rows_and_statement() {
        let table = ObjectRef::table(None, Some("public"), "t x");
        let columns = vec![
            LoadColumn::new("id", DataType::INT64),
            LoadColumn::new("ok", DataType::Bool),
            LoadColumn::new("note", DataType::Text),
            LoadColumn::new("raw", DataType::VarBinary { length: None }),
        ];
        let version = ServerVersion::new(18, 0, 0);
        let target = LoadTarget {
            table: &table,
            columns: &columns,
            version: &version,
        };
        assert_eq!(
            PgBulkLoader::copy_statement(&target),
            "COPY \"public\".\"t x\" (\"id\", \"ok\", \"note\", \"raw\") FROM STDIN"
        );
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![Some(1), None])),
            Arc::new(BooleanArray::from(vec![Some(true), Some(false)])),
            Arc::new(StringArray::from(vec![Some("a\tb"), None])),
            Arc::new(BinaryArray::from(vec![Some(&b"\x01"[..]), None])),
        ];
        let batch = RecordBatch::try_new(target.schema(), arrays).unwrap();
        target.check_batch(&batch).unwrap();
        let mut out = String::new();
        PgBulkLoader::encode_rows(&batch, 0..2, &mut out).unwrap();
        assert_eq!(out, "1\tt\ta\\tb\t\\\\x01\n\\N\tf\t\\N\t\\N\n");
        assert_eq!(PgBulkLoader.method(&version), BulkMethod::CopyText);
    }
}
