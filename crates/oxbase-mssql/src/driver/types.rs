//! SQL Server → Arrow type mapping of result sets, and oxbase [`Value`](oxbase_types::Value)s
//! → statement parameters (driver stream). Contract: `docs/m4-contracts.md` §2.5.
//!
//! tiberius describes a result column only by its name and a coarse
//! [`tiberius::ColumnType`] (no length, precision, scale, nullability or width of the nullable
//! integer and float types), and the schema must be known before the first row. The mapping is
//! therefore by `ColumnType`:
//!
//! | SQL Server | `ColumnType` | Arrow | Notes |
//! |---|---|---|---|
//! | `bit` | `Bit`, `Bitn` | `Boolean` | |
//! | `tinyint`, `smallint`, `int`, `bigint` | `Int1`, `Int2`, `Int4`, `Int8`, `Intn` | `Int64` | one type for every integer (`Intn` hides the width) |
//! | `real`, `float` | `Float4`, `Float8`, `Floatn` | `Float64` | `real` widened exactly |
//! | `decimal(p,s)`, `numeric(p,s)` | `Decimaln`, `Numericn` | `Utf8` | exact text (`-12.50`); the scale is not in the metadata |
//! | `money`, `smallmoney` | `Money`, `Money4` | `Decimal128(19, 4)` | exact up to ±900 719 925 474.0991: tiberius 0.13 decodes money through `f64`, so larger amounts are rounded to the nearest `f64` (known gap, see the crate docs) |
//! | `date` | `Daten` | `Date32` | |
//! | `time(n)` | `Timen` | `Time64(µs)` | 7th fractional digit (100 ns) truncated |
//! | `datetime`, `smalldatetime` | `Datetime`, `Datetime4`, `Datetimen` | `Timestamp(µs, None)` | |
//! | `datetime2(n)` | `Datetime2` | `Timestamp(µs, None)` | 7th fractional digit truncated |
//! | `datetimeoffset(n)` | `DatetimeOffsetn` | `Utf8` | RFC 3339 text with the stored offset and 7 digits (`2026-01-15T10:00:00.1234567+03:30`); an instant type would drop the offset |
//! | `char`, `varchar`, `nchar`, `nvarchar` (incl. `max`), `text`, `ntext` | `BigChar`, `BigVarChar`, `NChar`, `NVarchar`, `Text`, `NText` | `Utf8` | non-Unicode text decoded from the column collation's code page |
//! | `json` (2025) | `NVarchar` | `Utf8` | sent as `nvarchar(max)` to clients that do not announce JSON support (tiberius does not) |
//! | `vector(n)` (2025) | `BigVarChar` | `Utf8` | JSON array text, same reason |
//! | `xml` | `Xml` | `Utf8` | |
//! | `binary`, `varbinary` (incl. `max`), `image`, `rowversion` / `timestamp` | `BigBinary`, `BigVarBin`, `Image` | `Binary` | |
//! | `uniqueidentifier` | `Guid` | `FixedSizeBinary(16)` + `ARROW:extension:name = arrow.uuid` | RFC 4122 byte order (as `uuid::Uuid` prints it) |
//! | `sql_variant` | `SSVariant` | `Utf8` | the inner value's text form |
//! | `hierarchyid`, `geometry`, `geography`, CLR types | `Udt` | `Binary` | the CLR serialization; `CAST(x AS nvarchar(max))` / `.ToString()` in SQL gives text |
//! | untyped `NULL` | `Null` | `Null` | |
//!
//! Every field is nullable and carries [`META_NATIVE_TYPE`]: the lower-case SQL Server family
//! name the `ColumnType` stands for (`int`, `decimal`, `nvarchar`, `datetime2`, `udt`, ...),
//! not the declared type (the catalog fetchers give that).
//!
//! # Parameters
//!
//! | `Value` | Declared as (`@params` of `sp_executesql`) |
//! |---|---|
//! | `Null` | `nvarchar(1)` NULL (no implicit conversion to `varbinary`, `image`, CLR types: the grid writer writes the keyword `NULL` instead of a parameter) |
//! | `Bool` | `bit` |
//! | `Int` | `bigint` |
//! | `UInt` | `bigint` when it fits, else `decimal(20, 0)` |
//! | `Float` | `float` (NaN and infinities: `InvalidInput`, SQL Server has none) |
//! | `Decimal` | `decimal(38, s)` with the text's scale (more than 38 digits: `InvalidInput`) |
//! | `Text`, `Json` | `nvarchar(max)` |
//! | `Bytes` | `varbinary(max)` |
//! | `Date` / `Time` / `Timestamp` / `TimestampTz` | `date` / `time(7)` / `datetime2(7)` / `datetimeoffset(7)` |
//! | `Uuid` | `uniqueidentifier` |

/// Field metadata key: the SQL Server type family of the column (see the module docs). Same key
/// as the MySQL driver's.
pub const META_NATIVE_TYPE: &str = "oxbase.native_type";
/// Arrow canonical extension-name metadata key.
pub const META_EXTENSION_NAME: &str = "ARROW:extension:name";
/// Extension name of `uniqueidentifier` columns (Arrow canonical `arrow.uuid`).
pub const UUID_EXTENSION: &str = "arrow.uuid";
/// Default rows per Arrow batch (option [`crate::options::BATCH_ROWS`]).
pub const BATCH_ROWS: usize = 4096;

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use arrow_array::builder::{
    ArrayBuilder, BinaryBuilder, BooleanBuilder, Date32Builder, Decimal128Builder,
    FixedSizeBinaryBuilder, Float64Builder, Int64Builder, NullBuilder, StringBuilder,
    Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
};
use arrow_array::{ArrayRef, RecordBatch, RecordBatchOptions};
use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use chrono::{Datelike, NaiveDate, NaiveTime, Offset, TimeDelta, Timelike};
use oxbase_driver::from_arrow;
use oxbase_types::{Error, Result, Value};
use tiberius::numeric::Numeric;
use tiberius::time::{Date, DateTime2, DateTimeOffset, Time};
use tiberius::{Column, ColumnData, ColumnType};

/// Days from 0001-01-01 (day 0 of `date` / `datetime2`) to 1970-01-01.
const DAYS_0001_TO_EPOCH: i64 = 719_162;
/// Days from 1900-01-01 (day 0 of `datetime` / `smalldatetime`) to 1970-01-01.
const DAYS_1900_TO_EPOCH: i64 = 25_567;
const MICROS_PER_DAY: i64 = 86_400_000_000;
/// Fixed byte estimate of one non-text cell, for the batch size cap.
const FIXED_CELL_BYTES: usize = 8;
/// Approximate byte cap of one Arrow batch (large `(max)` values end a batch early).
pub(crate) const BATCH_BYTES: usize = 8 * 1024 * 1024;

/// How a result column is converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Null,
    Bool,
    Int,
    Float,
    /// `decimal` / `numeric` as exact text.
    DecimalText,
    Money,
    Date,
    Time,
    Timestamp,
    /// `datetimeoffset` as RFC 3339 text.
    OffsetText,
    Text,
    Binary,
    Uuid,
}

/// The conversion and the [`META_NATIVE_TYPE`] family of a tiberius column type.
pub(crate) fn column_kind(ty: ColumnType) -> (Kind, &'static str) {
    use ColumnType as C;
    match ty {
        C::Null => (Kind::Null, "null"),
        C::Bit | C::Bitn => (Kind::Bool, "bit"),
        C::Int1 => (Kind::Int, "tinyint"),
        C::Int2 => (Kind::Int, "smallint"),
        C::Int4 | C::Intn => (Kind::Int, "int"),
        C::Int8 => (Kind::Int, "bigint"),
        C::Float4 => (Kind::Float, "real"),
        C::Float8 | C::Floatn => (Kind::Float, "float"),
        C::Decimaln => (Kind::DecimalText, "decimal"),
        C::Numericn => (Kind::DecimalText, "numeric"),
        C::Money => (Kind::Money, "money"),
        C::Money4 => (Kind::Money, "smallmoney"),
        C::Daten => (Kind::Date, "date"),
        C::Timen => (Kind::Time, "time"),
        C::Datetime | C::Datetimen => (Kind::Timestamp, "datetime"),
        C::Datetime4 => (Kind::Timestamp, "smalldatetime"),
        C::Datetime2 => (Kind::Timestamp, "datetime2"),
        C::DatetimeOffsetn => (Kind::OffsetText, "datetimeoffset"),
        C::BigVarChar => (Kind::Text, "varchar"),
        C::BigChar => (Kind::Text, "char"),
        C::NVarchar => (Kind::Text, "nvarchar"),
        C::NChar => (Kind::Text, "nchar"),
        C::Text => (Kind::Text, "text"),
        C::NText => (Kind::Text, "ntext"),
        C::Xml => (Kind::Text, "xml"),
        C::SSVariant => (Kind::Text, "sql_variant"),
        C::BigVarBin => (Kind::Binary, "varbinary"),
        C::BigBinary => (Kind::Binary, "binary"),
        C::Image => (Kind::Binary, "image"),
        C::Udt => (Kind::Binary, "udt"),
        C::Guid => (Kind::Uuid, "uniqueidentifier"),
    }
}

fn arrow_type(kind: Kind) -> DataType {
    match kind {
        Kind::Null => DataType::Null,
        Kind::Bool => DataType::Boolean,
        Kind::Int => DataType::Int64,
        Kind::Float => DataType::Float64,
        Kind::DecimalText | Kind::OffsetText | Kind::Text => DataType::Utf8,
        Kind::Money => DataType::Decimal128(19, 4),
        Kind::Date => DataType::Date32,
        Kind::Time => DataType::Time64(TimeUnit::Microsecond),
        Kind::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
        Kind::Binary => DataType::Binary,
        Kind::Uuid => DataType::FixedSizeBinary(16),
    }
}

/// The Arrow field of a result column (always nullable, with [`META_NATIVE_TYPE`]).
pub(crate) fn field(column: &Column) -> Field {
    let (kind, native) = column_kind(column.column_type());
    let mut metadata = HashMap::from([(META_NATIVE_TYPE.to_owned(), native.to_owned())]);
    if kind == Kind::Uuid {
        metadata.insert(META_EXTENSION_NAME.to_owned(), UUID_EXTENSION.to_owned());
    }
    Field::new(column.name(), arrow_type(kind), true).with_metadata(metadata)
}

/// Schema and conversions of one result set.
#[derive(Debug, Clone)]
pub(crate) struct ResultShape {
    pub schema: SchemaRef,
    kinds: Vec<Kind>,
}

impl ResultShape {
    pub(crate) fn new(columns: &[Column]) -> Self {
        Self {
            schema: Arc::new(Schema::new(columns.iter().map(field).collect::<Vec<_>>())),
            kinds: columns
                .iter()
                .map(|c| column_kind(c.column_type()).0)
                .collect(),
        }
    }
}

enum ColumnBuilder {
    Null(NullBuilder),
    Bool(BooleanBuilder),
    Int(Int64Builder),
    Float(Float64Builder),
    Money(Decimal128Builder),
    Date(Date32Builder),
    Time(Time64MicrosecondBuilder),
    Timestamp(TimestampMicrosecondBuilder),
    Text(StringBuilder),
    Binary(BinaryBuilder),
    Uuid(FixedSizeBinaryBuilder),
}

/// Bytes stored, or `None` when the value did not fit the column and was stored as NULL.
type Appended = Option<usize>;

impl ColumnBuilder {
    fn new(kind: Kind) -> Self {
        match kind {
            Kind::Null => Self::Null(NullBuilder::new()),
            Kind::Bool => Self::Bool(BooleanBuilder::new()),
            Kind::Int => Self::Int(Int64Builder::new()),
            Kind::Float => Self::Float(Float64Builder::new()),
            Kind::Money => {
                Self::Money(Decimal128Builder::new().with_data_type(DataType::Decimal128(19, 4)))
            }
            Kind::Date => Self::Date(Date32Builder::new()),
            Kind::Time => Self::Time(Time64MicrosecondBuilder::new()),
            Kind::Timestamp => Self::Timestamp(TimestampMicrosecondBuilder::new()),
            Kind::DecimalText | Kind::OffsetText | Kind::Text => Self::Text(StringBuilder::new()),
            Kind::Binary => Self::Binary(BinaryBuilder::new()),
            Kind::Uuid => Self::Uuid(FixedSizeBinaryBuilder::new(16)),
        }
    }

    fn append_null(&mut self) {
        match self {
            Self::Null(b) => b.append_null(),
            Self::Bool(b) => b.append_null(),
            Self::Int(b) => b.append_null(),
            Self::Float(b) => b.append_null(),
            Self::Money(b) => b.append_null(),
            Self::Date(b) => b.append_null(),
            Self::Time(b) => b.append_null(),
            Self::Timestamp(b) => b.append_null(),
            Self::Text(b) => b.append_null(),
            Self::Binary(b) => b.append_null(),
            Self::Uuid(b) => b.append_null(),
        }
    }

    /// Appends one value; a NULL of any type is a NULL.
    fn append(&mut self, value: &ColumnData<'static>) -> Appended {
        if is_null(value) {
            self.append_null();
            return Some(1);
        }
        let stored = self.append_value(value);
        if stored.is_none() {
            self.append_null();
        }
        stored
    }

    fn append_value(&mut self, value: &ColumnData<'static>) -> Appended {
        match self {
            Self::Null(_) => None,
            Self::Bool(b) => {
                let ColumnData::Bit(Some(v)) = value else {
                    return None;
                };
                b.append_value(*v);
                Some(FIXED_CELL_BYTES)
            }
            Self::Int(b) => {
                b.append_value(int_value(value)?);
                Some(FIXED_CELL_BYTES)
            }
            Self::Float(b) => {
                let v = match value {
                    ColumnData::F32(Some(v)) => f64::from(*v),
                    ColumnData::F64(Some(v)) => *v,
                    _ => return None,
                };
                b.append_value(v);
                Some(FIXED_CELL_BYTES)
            }
            Self::Money(b) => {
                let v = match value {
                    ColumnData::F64(Some(v)) => money_scaled(*v)?,
                    ColumnData::F32(Some(v)) => money_scaled(f64::from(*v))?,
                    _ => return None,
                };
                b.append_value(v);
                Some(FIXED_CELL_BYTES)
            }
            Self::Date(b) => {
                let ColumnData::Date(Some(d)) = value else {
                    return None;
                };
                b.append_value(date32(d.days()));
                Some(FIXED_CELL_BYTES)
            }
            Self::Time(b) => {
                let ColumnData::Time(Some(t)) = value else {
                    return None;
                };
                b.append_value(time_micros(t.increments(), t.scale()));
                Some(FIXED_CELL_BYTES)
            }
            Self::Timestamp(b) => {
                b.append_value(timestamp_micros(value)?);
                Some(FIXED_CELL_BYTES)
            }
            Self::Text(b) => {
                let text = value_text(value)?;
                let n = text.len();
                b.append_value(text);
                Some(n)
            }
            Self::Binary(b) => match value {
                ColumnData::Binary(Some(bytes)) => {
                    b.append_value(bytes.as_ref());
                    Some(bytes.len())
                }
                _ => None,
            },
            Self::Uuid(b) => match value {
                ColumnData::Guid(Some(u)) => b.append_value(u.as_bytes()).ok().map(|()| 16),
                _ => None,
            },
        }
    }

    fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Null(b) => ArrayBuilder::finish(b),
            Self::Bool(b) => ArrayBuilder::finish(b),
            Self::Int(b) => ArrayBuilder::finish(b),
            Self::Float(b) => ArrayBuilder::finish(b),
            Self::Money(b) => ArrayBuilder::finish(b),
            Self::Date(b) => ArrayBuilder::finish(b),
            Self::Time(b) => ArrayBuilder::finish(b),
            Self::Timestamp(b) => ArrayBuilder::finish(b),
            Self::Text(b) => ArrayBuilder::finish(b),
            Self::Binary(b) => ArrayBuilder::finish(b),
            Self::Uuid(b) => ArrayBuilder::finish(b),
        }
    }
}

/// Accumulates the rows of one result set into `RecordBatch`es.
pub(crate) struct BatchBuilder {
    schema: SchemaRef,
    columns: Vec<ColumnBuilder>,
    rows: usize,
    bytes: usize,
    lossy: Vec<u64>,
}

impl BatchBuilder {
    pub(crate) fn new(shape: &ResultShape) -> Self {
        Self {
            schema: shape.schema.clone(),
            columns: shape.kinds.iter().map(|k| ColumnBuilder::new(*k)).collect(),
            rows: 0,
            bytes: 0,
            lossy: vec![0; shape.kinds.len()],
        }
    }

    /// Appends one row (missing trailing values are NULL).
    pub(crate) fn push<I: IntoIterator<Item = ColumnData<'static>>>(&mut self, row: I) {
        let mut values = row.into_iter();
        for (i, builder) in self.columns.iter_mut().enumerate() {
            match values.next() {
                Some(value) => match builder.append(&value) {
                    Some(n) => self.bytes += n,
                    None => self.lossy[i] += 1,
                },
                None => builder.append_null(),
            }
        }
        self.rows += 1;
    }

    pub(crate) fn rows(&self) -> usize {
        self.rows
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Builds the pending rows into a batch and starts a new one.
    pub(crate) fn finish(&mut self) -> Result<RecordBatch> {
        let arrays: Vec<ArrayRef> = self.columns.iter_mut().map(ColumnBuilder::finish).collect();
        let options = RecordBatchOptions::new().with_row_count(Some(self.rows));
        self.rows = 0;
        self.bytes = 0;
        RecordBatch::try_new_with_options(self.schema.clone(), arrays, &options).map_err(from_arrow)
    }

    /// Names and counts of the columns with values stored as NULL because they did not fit
    /// (and resets the counts).
    pub(crate) fn take_lossy(&mut self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        for (i, n) in self.lossy.iter_mut().enumerate() {
            if *n > 0 {
                out.push((self.schema.field(i).name().clone(), std::mem::take(n)));
            }
        }
        out
    }
}

// ---- value conversion -----------------------------------------------------------------------

fn is_null(v: &ColumnData<'_>) -> bool {
    matches!(
        v,
        ColumnData::U8(None)
            | ColumnData::I16(None)
            | ColumnData::I32(None)
            | ColumnData::I64(None)
            | ColumnData::F32(None)
            | ColumnData::F64(None)
            | ColumnData::Bit(None)
            | ColumnData::String(None)
            | ColumnData::Guid(None)
            | ColumnData::Binary(None)
            | ColumnData::Numeric(None)
            | ColumnData::Xml(None)
            | ColumnData::DateTime(None)
            | ColumnData::SmallDateTime(None)
            | ColumnData::Time(None)
            | ColumnData::Date(None)
            | ColumnData::DateTime2(None)
            | ColumnData::DateTimeOffset(None)
    )
}

fn int_value(v: &ColumnData<'_>) -> Option<i64> {
    match v {
        ColumnData::U8(Some(x)) => Some(i64::from(*x)),
        ColumnData::I16(Some(x)) => Some(i64::from(*x)),
        ColumnData::I32(Some(x)) => Some(i64::from(*x)),
        ColumnData::I64(Some(x)) => Some(*x),
        _ => None,
    }
}

/// A `money` value (decoded by tiberius as `f64`) as a `Decimal128(19, 4)` mantissa.
#[allow(
    clippy::cast_possible_truncation,
    reason = "bounded to |x| <= 1e20 before the cast"
)]
pub(crate) fn money_scaled(v: f64) -> Option<i128> {
    let scaled = (v * 10_000.0).round();
    (scaled.is_finite() && scaled.abs() <= 1e20).then_some(scaled as i128)
}

/// Exact text of a `decimal` mantissa and scale (`-12.50`, `0.001`).
pub(crate) fn numeric_text(value: i128, scale: u8) -> String {
    let digits = value.unsigned_abs().to_string();
    let scale = usize::from(scale);
    let mut out = String::with_capacity(digits.len() + 3);
    if value < 0 {
        out.push('-');
    }
    if scale == 0 {
        out.push_str(&digits);
        return out;
    }
    let padded = if digits.len() <= scale {
        format!("{}{digits}", "0".repeat(scale + 1 - digits.len()))
    } else {
        digits
    };
    let (int, frac) = padded.split_at(padded.len() - scale);
    out.push_str(int);
    out.push('.');
    out.push_str(frac);
    out
}

/// `date` days since 0001-01-01 as `Date32` days since 1970-01-01.
pub(crate) fn date32(days: u32) -> i32 {
    i32::try_from(i64::from(days) - DAYS_0001_TO_EPOCH).unwrap_or(i32::MIN)
}

/// `time(n)` increments of 10^-n s as microseconds (digits below 1 µs truncated).
pub(crate) fn time_micros(increments: u64, scale: u8) -> i64 {
    let scale = u32::from(scale.min(7));
    let micros = if scale <= 6 {
        increments.saturating_mul(10u64.pow(6 - scale))
    } else {
        increments / 10u64.pow(scale - 6)
    };
    i64::try_from(micros).unwrap_or(i64::MAX)
}

/// `time(n)` increments as 100 ns units.
fn time_100ns(increments: u64, scale: u8) -> u64 {
    let scale = u32::from(scale.min(7));
    increments.saturating_mul(10u64.pow(7 - scale))
}

/// `datetime`: days since 1900-01-01 and 1/300 s ticks, as microseconds since the epoch
/// (rounded to the nearest microsecond, as SQL Server converts to `datetime2(6)`).
pub(crate) fn datetime_micros(days: i32, ticks: u32) -> i64 {
    let ticks = i64::from(ticks);
    (i64::from(days) - DAYS_1900_TO_EPOCH) * MICROS_PER_DAY + (ticks * 10_000 + 1) / 3
}

/// `smalldatetime`: days since 1900-01-01 and minutes since midnight.
pub(crate) fn smalldatetime_micros(days: u16, minutes: u16) -> i64 {
    (i64::from(days) - DAYS_1900_TO_EPOCH) * MICROS_PER_DAY + i64::from(minutes) * 60_000_000
}

fn timestamp_micros(v: &ColumnData<'_>) -> Option<i64> {
    match v {
        ColumnData::DateTime(Some(d)) => Some(datetime_micros(d.days(), d.seconds_fragments())),
        ColumnData::SmallDateTime(Some(d)) => {
            Some(smalldatetime_micros(d.days(), d.seconds_fragments()))
        }
        ColumnData::DateTime2(Some(d)) => Some(
            (i64::from(d.date().days()) - DAYS_0001_TO_EPOCH) * MICROS_PER_DAY
                + time_micros(d.time().increments(), d.time().scale()),
        ),
        _ => None,
    }
}

fn naive_date(days: u32) -> Option<NaiveDate> {
    NaiveDate::from_num_days_from_ce_opt(i32::try_from(days).ok()?.checked_add(1)?)
}

/// `datetimeoffset`: the UTC date and time plus the offset in minutes, as RFC 3339 text in the
/// stored offset with 7 fractional digits (`2026-01-15T10:20:30.1234567+03:30`).
pub(crate) fn offset_text(days: u32, increments: u64, scale: u8, offset: i16) -> Option<String> {
    let ticks = time_100ns(increments, scale);
    let secs = i64::try_from(ticks / 10_000_000).ok()?;
    let frac = ticks % 10_000_000;
    let local = naive_date(days)?
        .and_hms_opt(0, 0, 0)?
        .checked_add_signed(TimeDelta::try_seconds(secs)?)?
        .checked_add_signed(TimeDelta::try_minutes(i64::from(offset))?)?;
    let sign = if offset < 0 { '-' } else { '+' };
    let abs = offset.unsigned_abs();
    Some(format!(
        "{}.{frac:07}{sign}{:02}:{:02}",
        local.format("%Y-%m-%dT%H:%M:%S"),
        abs / 60,
        abs % 60
    ))
}

fn time_text(increments: u64, scale: u8) -> String {
    let ticks = time_100ns(increments, scale);
    let secs = ticks / 10_000_000;
    format!(
        "{:02}:{:02}:{:02}.{:07}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        ticks % 10_000_000
    )
}

fn micros_text(micros: i64, digits: usize) -> Option<String> {
    let dt = chrono::DateTime::from_timestamp_micros(micros)?.naive_utc();
    Some(match digits {
        0 => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
    })
}

/// The text form of any non-NULL value (text columns, `decimal`, `datetimeoffset`, and the
/// inner value of `sql_variant`).
pub(crate) fn value_text(v: &ColumnData<'_>) -> Option<String> {
    Some(match v {
        ColumnData::String(Some(s)) => s.to_string(),
        ColumnData::Xml(Some(x)) => x.to_string(),
        ColumnData::Numeric(Some(n)) => numeric_text(n.value(), n.scale()),
        ColumnData::DateTimeOffset(Some(d)) => offset_text(
            d.datetime2().date().days(),
            d.datetime2().time().increments(),
            d.datetime2().time().scale(),
            d.offset(),
        )?,
        ColumnData::U8(Some(_))
        | ColumnData::I16(Some(_))
        | ColumnData::I32(Some(_))
        | ColumnData::I64(Some(_)) => int_value(v)?.to_string(),
        ColumnData::F32(Some(x)) => x.to_string(),
        ColumnData::F64(Some(x)) => x.to_string(),
        ColumnData::Bit(Some(b)) => String::from(if *b { "1" } else { "0" }),
        ColumnData::Guid(Some(u)) => u.hyphenated().to_string().to_uppercase(),
        ColumnData::Binary(Some(b)) => {
            let mut s = String::with_capacity(2 + b.len() * 2);
            s.push_str("0x");
            for byte in b.iter() {
                let _ = write!(s, "{byte:02X}");
            }
            s
        }
        ColumnData::DateTime(Some(_)) => micros_text(timestamp_micros(v)?, 3)?,
        ColumnData::SmallDateTime(Some(_)) => micros_text(timestamp_micros(v)?, 0)?,
        ColumnData::Date(Some(d)) => naive_date(d.days())?.format("%Y-%m-%d").to_string(),
        ColumnData::Time(Some(t)) => time_text(t.increments(), t.scale()),
        ColumnData::DateTime2(Some(d)) => format!(
            "{} {}",
            naive_date(d.date().days())?.format("%Y-%m-%d"),
            time_text(d.time().increments(), d.time().scale())
        ),
        _ => return None,
    })
}

// ---- parameters -----------------------------------------------------------------------------

/// One statement parameter: its `@params` declaration and its RPC value.
#[derive(Debug)]
pub(crate) struct Param {
    pub decl: String,
    pub data: ColumnData<'static>,
}

fn param(decl: impl Into<String>, data: ColumnData<'static>) -> Param {
    Param {
        decl: decl.into(),
        data,
    }
}

/// Converts `v` to a parameter (see the module docs for the declared types).
pub(crate) fn to_param(v: &Value) -> Result<Param> {
    Ok(match v {
        Value::Null => param("nvarchar(1)", ColumnData::String(None)),
        Value::Bool(b) => param("bit", ColumnData::Bit(Some(*b))),
        Value::Int(i) => param("bigint", ColumnData::I64(Some(*i))),
        Value::UInt(u) => match i64::try_from(*u) {
            Ok(i) => param("bigint", ColumnData::I64(Some(i))),
            Err(_) => param(
                "decimal(20, 0)",
                ColumnData::Numeric(Some(Numeric::new_with_scale(i128::from(*u), 0))),
            ),
        },
        Value::Float(f) => {
            if !f.is_finite() {
                return Err(Error::invalid_input(format!(
                    "SQL Server has no {f} float value"
                )));
            }
            param("float", ColumnData::F64(Some(*f)))
        }
        Value::Decimal(text) => {
            let (value, scale) = parse_decimal(text)?;
            param(
                format!("decimal(38, {scale})"),
                ColumnData::Numeric(Some(Numeric::new_with_scale(value, scale))),
            )
        }
        Value::Text(s) => param(
            "nvarchar(max)",
            ColumnData::String(Some(Cow::Owned(s.clone()))),
        ),
        Value::Json(j) => param(
            "nvarchar(max)",
            ColumnData::String(Some(Cow::Owned(j.to_string()))),
        ),
        Value::Bytes(b) => param(
            "varbinary(max)",
            ColumnData::Binary(Some(Cow::Owned(b.clone()))),
        ),
        Value::Date(d) => param("date", ColumnData::Date(Some(sql_date(*d)?))),
        Value::Time(t) => param("time(7)", ColumnData::Time(Some(sql_time(*t)))),
        Value::Timestamp(ts) => param(
            "datetime2(7)",
            ColumnData::DateTime2(Some(DateTime2::new(
                sql_date(ts.date())?,
                sql_time(ts.time()),
            ))),
        ),
        Value::TimestampTz(dt) => {
            let secs = dt.offset().fix().local_minus_utc();
            if secs % 60 != 0 || secs.abs() > 14 * 3600 {
                return Err(Error::invalid_input(format!(
                    "SQL Server offsets are whole minutes within ±14:00, not {}",
                    dt.offset()
                )));
            }
            let offset = i16::try_from(secs / 60).unwrap_or_default();
            let utc = dt.naive_utc();
            param(
                "datetimeoffset(7)",
                ColumnData::DateTimeOffset(Some(DateTimeOffset::new(
                    DateTime2::new(sql_date(utc.date())?, sql_time(utc.time())),
                    offset,
                ))),
            )
        }
        Value::Uuid(u) => param(
            "uniqueidentifier",
            ColumnData::Guid(Some(tiberius::Uuid::from_bytes(*u.as_bytes()))),
        ),
    })
}

/// The `@params` argument of `sp_executesql`: `@P1 bigint, @P2 nvarchar(max), ...`.
pub(crate) fn declarations(params: &[Param]) -> String {
    params
        .iter()
        .enumerate()
        .map(|(i, p)| format!("@P{} {}", i + 1, p.decl))
        .collect::<Vec<_>>()
        .join(", ")
}

fn sql_date(d: NaiveDate) -> Result<Date> {
    if !(1..=9999).contains(&d.year()) {
        return Err(Error::invalid_input(format!(
            "SQL Server dates are within 0001-01-01 .. 9999-12-31, not {d}"
        )));
    }
    let days = u32::try_from(d.num_days_from_ce() - 1)
        .map_err(|_| Error::invalid_input(format!("date {d} is out of range")))?;
    Ok(Date::new(days))
}

fn sql_time(t: NaiveTime) -> Time {
    // A leap second (nanosecond >= 1e9) is clamped into the same second.
    let nanos = u64::from(t.nanosecond().min(999_999_999));
    Time::new(
        u64::from(t.num_seconds_from_midnight()) * 10_000_000 + nanos / 100,
        7,
    )
}

/// Parses decimal text (`-123.4500`, `+1`, `.5`) into a mantissa and scale; at most 38
/// significant digits and a scale of at most 38.
pub(crate) fn parse_decimal(text: &str) -> Result<(i128, u8)> {
    let bad = || Error::invalid_input(format!("'{text}' is not a decimal SQL Server can store"));
    let t = text.trim();
    let (negative, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if (int.is_empty() && frac.is_empty())
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(bad());
    }
    let digits = format!("{int}{frac}");
    let significant = digits.trim_start_matches('0').len();
    let scale = u8::try_from(frac.len()).map_err(|_| bad())?;
    if significant > 38 || scale > 38 {
        return Err(bad());
    }
    let magnitude: i128 = if significant == 0 {
        0
    } else {
        digits.trim_start_matches('0').parse().map_err(|_| bad())?
    };
    Ok((if negative { -magnitude } else { magnitude }, scale))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Array;
    use arrow_array::cast::AsArray;
    use arrow_array::types::{
        Date32Type, Decimal128Type, Float64Type, Int64Type, Time64MicrosecondType,
        TimestampMicrosecondType,
    };
    use chrono::{FixedOffset, TimeZone};
    use pretty_assertions::assert_eq;
    use tiberius::time::{DateTime as SqlDateTime, SmallDateTime};

    fn col(name: &str, ty: ColumnType) -> Column {
        Column::new(name.to_owned(), ty)
    }

    fn naive(text: &str) -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f").unwrap()
    }

    fn micros(text: &str) -> i64 {
        naive(text).and_utc().timestamp_micros()
    }

    #[test]
    fn schema_fields_carry_native_types_and_the_uuid_extension() {
        let shape = ResultShape::new(&[
            col("i", ColumnType::Intn),
            col("g", ColumnType::Guid),
            col("d", ColumnType::Decimaln),
            col("", ColumnType::Null),
        ]);
        let s = &shape.schema;
        assert_eq!(s.field(0).data_type(), &DataType::Int64);
        assert_eq!(s.field(0).metadata()[META_NATIVE_TYPE], "int");
        assert!(s.fields().iter().all(|f| f.is_nullable()));
        assert_eq!(s.field(1).data_type(), &DataType::FixedSizeBinary(16));
        assert_eq!(s.field(1).metadata()[META_EXTENSION_NAME], UUID_EXTENSION);
        assert_eq!(s.field(2).data_type(), &DataType::Utf8);
        assert_eq!(s.field(2).metadata()[META_NATIVE_TYPE], "decimal");
        assert_eq!(s.field(3).data_type(), &DataType::Null);
        assert_eq!(column_kind(ColumnType::Money4), (Kind::Money, "smallmoney"));
        assert_eq!(arrow_type(Kind::Money), DataType::Decimal128(19, 4));
    }

    #[test]
    fn golden_values() {
        let shape = ResultShape::new(&[
            col("tiny", ColumnType::Int1),
            col("small", ColumnType::Int2),
            col("big", ColumnType::Int8),
            col("bit", ColumnType::Bitn),
            col("dec", ColumnType::Decimaln),
            col("num", ColumnType::Numericn),
            col("money", ColumnType::Money),
            col("smallmoney", ColumnType::Money4),
            col("real", ColumnType::Float4),
            col("date", ColumnType::Daten),
            col("time", ColumnType::Timen),
            col("datetime", ColumnType::Datetimen),
            col("dt2", ColumnType::Datetime2),
            col("smalldt", ColumnType::Datetime4),
            col("dto", ColumnType::DatetimeOffsetn),
            col("nchar", ColumnType::NChar),
            col("max", ColumnType::NVarchar),
            col("bin", ColumnType::BigBinary),
            col("guid", ColumnType::Guid),
            col("xml", ColumnType::Xml),
            col("variant", ColumnType::SSVariant),
        ]);
        let long = "ab".repeat(5000);
        let guid = tiberius::Uuid::parse_str("6F9619FF-8B86-D011-B42D-00C04FC964FF").unwrap();
        // 0001-01-01 is day 0; 9999-12-31 is day 3 652 058.
        let max_dt2 = DateTime2::new(Date::new(3_652_058), Time::new(863_999_999_999, 7));
        let dto = DateTimeOffset::new(
            // 2026-01-15T10:20:30.1234567+03:30 is 06:50:30.1234567 UTC.
            DateTime2::new(
                Date::new(
                    u32::try_from(
                        NaiveDate::from_ymd_opt(2026, 1, 15)
                            .unwrap()
                            .num_days_from_ce()
                            - 1,
                    )
                    .unwrap(),
                ),
                Time::new((6 * 3600 + 50 * 60 + 30) * 10_000_000 + 1_234_567, 7),
            ),
            210,
        );
        let days_2026 = i32::try_from(
            NaiveDate::from_ymd_opt(2026, 1, 15)
                .unwrap()
                .signed_duration_since(NaiveDate::from_ymd_opt(1900, 1, 1).unwrap())
                .num_days(),
        )
        .unwrap();
        let row = vec![
            ColumnData::U8(Some(255)),
            ColumnData::I16(Some(-32768)),
            ColumnData::I64(Some(i64::MIN)),
            ColumnData::Bit(Some(true)),
            ColumnData::Numeric(Some(Numeric::new_with_scale(123_456_789_012_345_678, 4))),
            ColumnData::Numeric(Some(Numeric::new_with_scale(
                12_345_678_901_234_567_890_123_456_780_123_456_789,
                10,
            ))),
            ColumnData::F64(Some(123_456.789_1)),
            ColumnData::F64(Some(-214_748.364_8)),
            ColumnData::F32(Some(3.25)),
            ColumnData::Date(Some(Date::new(0))),
            ColumnData::Time(Some(Time::new(863_991_234_567, 7))),
            // 10:20:30.997 is tick 299 of second 37230 (1/300 s).
            ColumnData::DateTime(Some(SqlDateTime::new(days_2026, 37_230 * 300 + 299))),
            ColumnData::DateTime2(Some(max_dt2)),
            ColumnData::SmallDateTime(Some(SmallDateTime::new(
                u16::try_from(days_2026).unwrap(),
                10 * 60 + 21,
            ))),
            ColumnData::DateTimeOffset(Some(dto)),
            ColumnData::String(Some(Cow::Owned("☃         ".to_owned()))),
            ColumnData::String(Some(Cow::Owned(long.clone()))),
            ColumnData::Binary(Some(Cow::Owned(vec![1, 2, 0, 0]))),
            ColumnData::Guid(Some(guid)),
            ColumnData::Xml(Some(Cow::Owned(tiberius::xml::XmlData::new(
                "<a b=\"1\" />",
            )))),
            ColumnData::I32(Some(42)),
        ];
        let mut b = BatchBuilder::new(&shape);
        b.push(row);
        // Row 2: all NULL, typed as the server sends them.
        b.push(vec![
            ColumnData::U8(None),
            ColumnData::I16(None),
            ColumnData::I64(None),
            ColumnData::Bit(None),
            ColumnData::Numeric(None),
            ColumnData::Numeric(None),
            ColumnData::F64(None),
            ColumnData::F64(None),
            ColumnData::F32(None),
            ColumnData::Date(None),
            ColumnData::Time(None),
            ColumnData::DateTime(None),
            ColumnData::DateTime2(None),
            ColumnData::SmallDateTime(None),
            ColumnData::DateTimeOffset(None),
            ColumnData::String(None),
            ColumnData::String(None),
            ColumnData::Binary(None),
            ColumnData::Guid(None),
            ColumnData::Xml(None),
            ColumnData::String(None),
        ]);
        assert_eq!(b.take_lossy(), Vec::<(String, u64)>::new());
        let batch = b.finish().unwrap();
        assert_eq!(batch.num_rows(), 2);
        let int = |i: usize| batch.column(i).as_primitive::<Int64Type>().value(0);
        let text = |i: usize| batch.column(i).as_string::<i32>().value(0).to_owned();
        assert_eq!(int(0), 255);
        assert_eq!(int(1), -32768);
        assert_eq!(int(2), i64::MIN);
        assert!(batch.column(3).as_boolean().value(0));
        assert_eq!(text(4), "12345678901234.5678");
        assert_eq!(text(5), "1234567890123456789012345678.0123456789");
        let money = batch.column(6).as_primitive::<Decimal128Type>();
        assert_eq!(money.value(0), 1_234_567_891);
        assert_eq!(money.value_as_string(0), "123456.7891");
        assert_eq!(
            batch
                .column(7)
                .as_primitive::<Decimal128Type>()
                .value_as_string(0),
            "-214748.3648"
        );
        assert_eq!(
            batch
                .column(8)
                .as_primitive::<Float64Type>()
                .value(0)
                .to_bits(),
            3.25f64.to_bits()
        );
        assert_eq!(
            batch.column(9).as_primitive::<Date32Type>().value(0),
            -719_162,
            "0001-01-01"
        );
        assert_eq!(
            batch
                .column(10)
                .as_primitive::<Time64MicrosecondType>()
                .value(0),
            86_399_123_456,
            "23:59:59.1234567 truncated to µs"
        );
        let ts = |i: usize| {
            batch
                .column(i)
                .as_primitive::<TimestampMicrosecondType>()
                .value(0)
        };
        assert_eq!(ts(11), micros("2026-01-15 10:20:30.996667"));
        assert_eq!(ts(12), micros("9999-12-31 23:59:59.999999"));
        assert_eq!(ts(13), micros("2026-01-15 10:21:00.0"));
        assert_eq!(text(14), "2026-01-15T10:20:30.1234567+03:30");
        assert_eq!(text(15), "☃         ");
        assert_eq!(text(16).chars().count(), 10_000);
        assert_eq!(batch.column(17).as_binary::<i32>().value(0), &[1, 2, 0, 0]);
        assert_eq!(
            batch.column(18).as_fixed_size_binary().value(0),
            uuid::Uuid::parse_str("6f9619ff-8b86-d011-b42d-00c04fc964ff")
                .unwrap()
                .as_bytes()
        );
        assert_eq!(text(19), "<a b=\"1\" />");
        assert_eq!(text(20), "42");
        for i in 0..batch.num_columns() {
            assert!(batch.column(i).is_null(1), "row 2 column {i} is NULL");
        }
    }

    #[test]
    fn mismatched_values_are_null_and_reported() {
        let shape = ResultShape::new(&[col("n", ColumnType::Int4), col("u", ColumnType::Null)]);
        let mut b = BatchBuilder::new(&shape);
        b.push(vec![ColumnData::String(Some(Cow::Borrowed("x")))]);
        assert_eq!(b.rows(), 1);
        assert_eq!(b.take_lossy(), [("n".to_owned(), 1)]);
        let batch = b.finish().unwrap();
        assert!(batch.column(0).is_null(0));
        // NullArray has no validity buffer: every slot is logically NULL.
        assert_eq!(batch.column(1).data_type(), &DataType::Null);
        assert_eq!(batch.column(1).logical_null_count(), 1);
        assert_eq!(b.rows(), 0);
        assert_eq!(b.bytes(), 0);
    }

    #[test]
    fn numeric_text_edges() {
        assert_eq!(numeric_text(-1250, 2), "-12.50");
        assert_eq!(numeric_text(1, 3), "0.001");
        assert_eq!(numeric_text(-5, 1), "-0.5");
        assert_eq!(numeric_text(0, 2), "0.00");
        assert_eq!(numeric_text(42, 0), "42");
        assert_eq!(
            numeric_text(-99_999_999_999_999_999_999_999_999_999_999_999_999, 38),
            "-0.99999999999999999999999999999999999999"
        );
    }

    #[test]
    fn money_scaling() {
        assert_eq!(money_scaled(0.0001), Some(1));
        assert_eq!(money_scaled(-214_748.364_8), Some(-2_147_483_648));
        assert_eq!(money_scaled(f64::NAN), None);
        assert_eq!(money_scaled(1e30), None);
        assert_eq!(money_scaled(123_456_789.123_4), Some(1_234_567_891_234));
    }

    #[test]
    fn date_and_time_math() {
        assert_eq!(date32(719_162), 0);
        assert_eq!(datetime_micros(25_567, 0), 0);
        assert_eq!(datetime_micros(25_567, 1), 3_333);
        assert_eq!(datetime_micros(25_567, 2), 6_667);
        assert_eq!(datetime_micros(25_567, 300), 1_000_000);
        assert_eq!(smalldatetime_micros(25_567, 1), 60_000_000);
        assert_eq!(time_micros(5, 0), 5_000_000);
        assert_eq!(time_micros(1_234_567, 7), 123_456);
        assert_eq!(time_micros(123, 3), 123_000);
        assert_eq!(time_text(1_234_567, 7), "00:00:00.1234567");
        assert_eq!(time_text(12, 1), "00:00:01.2000000");
        assert_eq!(
            offset_text(1, 0, 3, -300).as_deref(),
            Some("0001-01-01T19:00:00.0000000-05:00")
        );
        assert_eq!(
            offset_text(3_652_058, 863_999_999_999, 7, -840).as_deref(),
            Some("9999-12-31T09:59:59.9999999-14:00")
        );
    }

    #[test]
    fn value_text_forms() {
        assert_eq!(value_text(&ColumnData::Bit(Some(false))).unwrap(), "0");
        assert_eq!(
            value_text(&ColumnData::Binary(Some(Cow::Owned(vec![0xCA, 0xFE])))).unwrap(),
            "0xCAFE"
        );
        let guid = tiberius::Uuid::parse_str("6f9619ff-8b86-d011-b42d-00c04fc964ff").unwrap();
        assert_eq!(
            value_text(&ColumnData::Guid(Some(guid))).unwrap(),
            "6F9619FF-8B86-D011-B42D-00C04FC964FF"
        );
        assert_eq!(
            value_text(&ColumnData::Date(Some(Date::new(0)))).unwrap(),
            "0001-01-01"
        );
        assert_eq!(
            value_text(&ColumnData::DateTime(Some(SqlDateTime::new(25_567, 300)))).unwrap(),
            "1970-01-01 00:00:01.000"
        );
        assert_eq!(value_text(&ColumnData::I64(None)), None);
    }

    #[test]
    fn parameters_and_declarations() {
        let tz = FixedOffset::east_opt(3 * 3600 + 30 * 60).unwrap();
        let values = [
            Value::Null,
            Value::Bool(true),
            Value::Int(-7),
            Value::UInt(u64::MAX),
            Value::UInt(5),
            Value::Float(1.5),
            Value::Decimal("-123.4500".into()),
            Value::Text("x".into()),
            Value::Json(serde_json::json!({"a": 1})),
            Value::Bytes(vec![1]),
            Value::Date(NaiveDate::from_ymd_opt(2026, 1, 15).unwrap()),
            Value::Time(NaiveTime::from_hms_nano_opt(23, 59, 59, 123_456_700).unwrap()),
            Value::Timestamp(naive("2026-01-15 10:20:30.5")),
            Value::TimestampTz(tz.with_ymd_and_hms(2026, 1, 15, 10, 20, 30).unwrap()),
            Value::Uuid(uuid::Uuid::nil()),
        ];
        let params: Vec<Param> = values.iter().map(|v| to_param(v).unwrap()).collect();
        assert_eq!(
            declarations(&params),
            "@P1 nvarchar(1), @P2 bit, @P3 bigint, @P4 decimal(20, 0), @P5 bigint, @P6 float, \
             @P7 decimal(38, 4), @P8 nvarchar(max), @P9 nvarchar(max), @P10 varbinary(max), \
             @P11 date, @P12 time(7), @P13 datetime2(7), @P14 datetimeoffset(7), \
             @P15 uniqueidentifier"
        );
        assert!(matches!(params[0].data, ColumnData::String(None)));
        match &params[3].data {
            ColumnData::Numeric(Some(n)) => assert_eq!(n.value(), i128::from(u64::MAX)),
            other => panic!("{other:?}"),
        }
        match &params[6].data {
            ColumnData::Numeric(Some(n)) => assert_eq!((n.value(), n.scale()), (-1_234_500, 4)),
            other => panic!("{other:?}"),
        }
        match &params[8].data {
            ColumnData::String(Some(s)) => assert_eq!(s.as_ref(), r#"{"a":1}"#),
            other => panic!("{other:?}"),
        }
        match &params[11].data {
            ColumnData::Time(Some(t)) => {
                assert_eq!((t.increments(), t.scale()), (863_991_234_567, 7));
            }
            other => panic!("{other:?}"),
        }
        match &params[13].data {
            ColumnData::DateTimeOffset(Some(d)) => {
                assert_eq!(d.offset(), 210);
                assert_eq!(
                    offset_text(
                        d.datetime2().date().days(),
                        d.datetime2().time().increments(),
                        7,
                        d.offset()
                    )
                    .as_deref(),
                    Some("2026-01-15T10:20:30.0000000+03:30")
                );
            }
            other => panic!("{other:?}"),
        }

        assert!(to_param(&Value::Float(f64::NAN)).is_err());
        assert!(to_param(&Value::Float(f64::INFINITY)).is_err());
        assert!(to_param(&Value::Date(NaiveDate::from_ymd_opt(10_000, 1, 1).unwrap())).is_err());
        let odd = FixedOffset::east_opt(30).unwrap();
        assert!(
            to_param(&Value::TimestampTz(
                odd.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
            ))
            .is_err()
        );
    }

    #[test]
    fn decimal_text_parsing() {
        assert_eq!(parse_decimal("12.50").unwrap(), (1250, 2));
        assert_eq!(parse_decimal("+1").unwrap(), (1, 0));
        assert_eq!(parse_decimal(".5").unwrap(), (5, 1));
        assert_eq!(parse_decimal("-0.000").unwrap(), (0, 3));
        assert_eq!(parse_decimal("000123").unwrap(), (123, 0));
        let max = "9".repeat(38);
        assert_eq!(parse_decimal(&max).unwrap().0, max.parse::<i128>().unwrap());
        assert!(parse_decimal(&"9".repeat(39)).is_err());
        assert!(parse_decimal(&format!("0.{}", "1".repeat(39))).is_err());
        for bad in ["", "-", ".", "1e5", "1.2.3", "abc", "1,5", " - 1"] {
            assert!(parse_decimal(bad).is_err(), "{bad}");
        }
    }
}
