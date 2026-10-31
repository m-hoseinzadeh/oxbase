//! Result-set column metadata to Arrow types, row values to Arrow arrays, and oxbase [`Value`]s
//! to MySQL statement parameters.
//!
//! # Type mapping
//!
//! | MySQL / MariaDB column | Arrow type | Notes |
//! |---|---|---|
//! | `TINYINT(1)` | `Boolean` | when [`TypeOptions::tinyint1_as_bool`] (default), else like `TINYINT` |
//! | `TINYINT` / `SMALLINT` / `MEDIUMINT`, `INT` / `BIGINT` | `Int8` / `Int16` / `Int32` / `Int64` | `UNSIGNED`: `UInt8` .. `UInt64` |
//! | `DECIMAL(p,s)` | `Decimal128(p,s)` when `p <= 38`, else `Utf8` | exact |
//! | `FLOAT` / `DOUBLE` | `Float32` / `Float64` | |
//! | `DATE` | `Date32` | zero / invalid dates become NULL (reported in a notice) |
//! | `TIME` | `Duration(µs)` | MySQL `TIME` spans -838:59:59..838:59:59, so it is a duration |
//! | `DATETIME` | `Timestamp(µs, None)` | wall-clock value, no zone |
//! | `TIMESTAMP` | `Timestamp(µs, "UTC")` | an instant; the session time zone is UTC (see [`TypeOptions::timestamp_utc()`]) |
//! | `YEAR` | `UInt16` | |
//! | `CHAR` / `VARCHAR` / `TEXT`, `ENUM` / `SET` | `Utf8` | the session character set is utf8mb4 |
//! | `BINARY` / `VARBINARY` / `BLOB` (binary charset) | `Binary` | |
//! | `JSON` | `Utf8` + `ARROW:extension:name = arrow.json` | MariaDB `JSON` is `LONGTEXT`: plain `Utf8` |
//! | `BIT(n)` | `UInt64` | big-endian bit value |
//! | `GEOMETRY` | `Binary` | MySQL internal format: 4-byte SRID + WKB |
//! | `NULL` (untyped literal) | `Null` | |
//!
//! Every field is nullable and carries the native type name under [`META_NATIVE_TYPE`].

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Date32Builder, Decimal128Builder, DurationMicrosecondBuilder,
    Float32Builder, Float64Builder, Int8Builder, Int16Builder, Int32Builder, Int64Builder,
    StringBuilder, TimestampMicrosecondBuilder, UInt8Builder, UInt16Builder, UInt32Builder,
    UInt64Builder,
};
use arrow_array::{ArrayRef, NullArray, RecordBatch};
use arrow_schema::{DataType as ArrowType, Field, Schema, SchemaRef, TimeUnit};
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use mysql_async::Column;
use mysql_async::Value as MyValue;
use mysql_async::consts::{ColumnFlags, ColumnType};
use oxbase_driver::from_arrow;
use oxbase_types::{Result, Value};

/// Field metadata key holding the server's type name (`INT UNSIGNED`, `DECIMAL(12,2)`, `JSON`, ...).
pub const META_NATIVE_TYPE: &str = "oxbase.native_type";
/// Arrow's canonical extension-name metadata key.
pub const META_EXTENSION_NAME: &str = "ARROW:extension:name";
/// Extension name for JSON text columns (Arrow canonical `arrow.json`).
pub const JSON_EXTENSION: &str = "arrow.json";

/// Collation id of the `binary` character set.
const BINARY_CHARSET: u16 = 63;
/// Widest decimal that fits `Decimal128`.
const MAX_DECIMAL128_PRECISION: u32 = 38;
/// Rough per-value size of fixed-width cells, for batch sizing.
const FIXED_CELL_BYTES: usize = 8;

/// Options that change how column types are mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeOptions {
    /// Map `TINYINT(1)` to Arrow `Boolean` (any non-zero value is `true`). Default `true`.
    pub tinyint1_as_bool: bool,
    /// `TIMESTAMP` columns are typed `Timestamp(µs, "UTC")` (instants) instead of
    /// `Timestamp(µs, None)`. Set by the driver only after the session's `time_zone` was set to
    /// UTC, because the server reads and writes `TIMESTAMP` values in the session time zone.
    pub(crate) timestamp_utc: bool,
}

impl Default for TypeOptions {
    fn default() -> Self {
        Self {
            tinyint1_as_bool: true,
            timestamp_utc: false,
        }
    }
}

impl TypeOptions {
    /// Whether `TIMESTAMP` columns are typed as UTC instants (see the module docs).
    pub fn timestamp_utc(self) -> bool {
        self.timestamp_utc
    }
}

/// Arrow time zone of `TIMESTAMP` columns in a UTC session.
pub const TIMESTAMP_TZ: &str = "UTC";

/// How one result column is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColKind {
    Null,
    Bool,
    Int8,
    Int16,
    Int32,
    Int64,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    Float32,
    Float64,
    Decimal {
        precision: u8,
        scale: i8,
    },
    /// A decimal too wide for `Decimal128`, kept as its exact text.
    DecimalText,
    Text,
    Json,
    Binary,
    Date,
    Time,
    DateTime,
    /// `TIMESTAMP` read in a session whose time zone is not known to be UTC.
    Timestamp,
    /// `TIMESTAMP` read in a UTC session: an instant.
    TimestampUtc,
    Year,
    Bit,
}

fn is_unsigned(col: &Column) -> bool {
    col.flags().contains(ColumnFlags::UNSIGNED_FLAG)
}

fn is_binary_charset(col: &Column) -> bool {
    col.character_set() == BINARY_CHARSET
}

/// Declared precision of a `DECIMAL` column, derived from its display length.
fn decimal_precision(col: &Column) -> u32 {
    let scale = u32::from(col.decimals());
    let mut overhead = 0;
    if scale > 0 {
        overhead += 1; // decimal point
    }
    if !is_unsigned(col) {
        overhead += 1; // sign
    }
    col.column_length().saturating_sub(overhead)
}

/// The decode kind for `col`.
pub(crate) fn column_kind(col: &Column, opts: TypeOptions) -> ColKind {
    let unsigned = is_unsigned(col);
    let pick = |signed: ColKind, unsigned_kind: ColKind| {
        if unsigned { unsigned_kind } else { signed }
    };
    match col.column_type() {
        ColumnType::MYSQL_TYPE_NULL => ColKind::Null,
        ColumnType::MYSQL_TYPE_TINY => {
            if opts.tinyint1_as_bool && col.column_length() == 1 {
                ColKind::Bool
            } else {
                pick(ColKind::Int8, ColKind::UInt8)
            }
        }
        ColumnType::MYSQL_TYPE_SHORT => pick(ColKind::Int16, ColKind::UInt16),
        ColumnType::MYSQL_TYPE_INT24 | ColumnType::MYSQL_TYPE_LONG => {
            pick(ColKind::Int32, ColKind::UInt32)
        }
        ColumnType::MYSQL_TYPE_LONGLONG => pick(ColKind::Int64, ColKind::UInt64),
        ColumnType::MYSQL_TYPE_FLOAT => ColKind::Float32,
        ColumnType::MYSQL_TYPE_DOUBLE => ColKind::Float64,
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => {
            let precision = decimal_precision(col);
            let scale = u32::from(col.decimals());
            match (u8::try_from(precision), i8::try_from(scale)) {
                (Ok(p), Ok(s))
                    if (1..=MAX_DECIMAL128_PRECISION).contains(&precision)
                        && scale <= precision =>
                {
                    ColKind::Decimal {
                        precision: p,
                        scale: s,
                    }
                }
                _ => ColKind::DecimalText,
            }
        }
        ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => ColKind::Date,
        ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2 => ColKind::Time,
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_DATETIME2 => ColKind::DateTime,
        ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2 => {
            if opts.timestamp_utc {
                ColKind::TimestampUtc
            } else {
                ColKind::Timestamp
            }
        }
        ColumnType::MYSQL_TYPE_YEAR => ColKind::Year,
        ColumnType::MYSQL_TYPE_BIT => ColKind::Bit,
        ColumnType::MYSQL_TYPE_JSON => ColKind::Json,
        ColumnType::MYSQL_TYPE_ENUM | ColumnType::MYSQL_TYPE_SET => ColKind::Text,
        ColumnType::MYSQL_TYPE_VARCHAR
        | ColumnType::MYSQL_TYPE_VAR_STRING
        | ColumnType::MYSQL_TYPE_STRING
        | ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB => {
            let flags = col.flags();
            if flags.intersects(ColumnFlags::ENUM_FLAG | ColumnFlags::SET_FLAG)
                || !is_binary_charset(col)
            {
                ColKind::Text
            } else {
                ColKind::Binary
            }
        }
        ColumnType::MYSQL_TYPE_GEOMETRY
        | ColumnType::MYSQL_TYPE_VECTOR
        | ColumnType::MYSQL_TYPE_TYPED_ARRAY
        | ColumnType::MYSQL_TYPE_UNKNOWN => ColKind::Binary,
    }
}

/// The Arrow type produced for `kind`.
pub(crate) fn arrow_type(kind: ColKind) -> ArrowType {
    match kind {
        ColKind::Null => ArrowType::Null,
        ColKind::Bool => ArrowType::Boolean,
        ColKind::Int8 => ArrowType::Int8,
        ColKind::Int16 => ArrowType::Int16,
        ColKind::Int32 => ArrowType::Int32,
        ColKind::Int64 => ArrowType::Int64,
        ColKind::UInt8 => ArrowType::UInt8,
        ColKind::UInt16 | ColKind::Year => ArrowType::UInt16,
        ColKind::UInt32 => ArrowType::UInt32,
        ColKind::UInt64 | ColKind::Bit => ArrowType::UInt64,
        ColKind::Float32 => ArrowType::Float32,
        ColKind::Float64 => ArrowType::Float64,
        ColKind::Decimal { precision, scale } => ArrowType::Decimal128(precision, scale),
        ColKind::DecimalText | ColKind::Text | ColKind::Json => ArrowType::Utf8,
        ColKind::Binary => ArrowType::Binary,
        ColKind::Date => ArrowType::Date32,
        ColKind::Time => ArrowType::Duration(TimeUnit::Microsecond),
        ColKind::DateTime | ColKind::Timestamp => ArrowType::Timestamp(TimeUnit::Microsecond, None),
        ColKind::TimestampUtc => {
            ArrowType::Timestamp(TimeUnit::Microsecond, Some(TIMESTAMP_TZ.into()))
        }
    }
}

/// The server-side type name of `col`, for the [`META_NATIVE_TYPE`] field metadata.
pub(crate) fn native_type_name(col: &Column) -> String {
    let unsigned = if is_unsigned(col) { " UNSIGNED" } else { "" };
    let binary = is_binary_charset(col);
    let flags = col.flags();
    let name = match col.column_type() {
        ColumnType::MYSQL_TYPE_NULL => "NULL",
        ColumnType::MYSQL_TYPE_TINY if col.column_length() == 1 => {
            return format!("TINYINT(1){unsigned}");
        }
        ColumnType::MYSQL_TYPE_TINY => return format!("TINYINT{unsigned}"),
        ColumnType::MYSQL_TYPE_SHORT => return format!("SMALLINT{unsigned}"),
        ColumnType::MYSQL_TYPE_INT24 => return format!("MEDIUMINT{unsigned}"),
        ColumnType::MYSQL_TYPE_LONG => return format!("INT{unsigned}"),
        ColumnType::MYSQL_TYPE_LONGLONG => return format!("BIGINT{unsigned}"),
        ColumnType::MYSQL_TYPE_FLOAT => return format!("FLOAT{unsigned}"),
        ColumnType::MYSQL_TYPE_DOUBLE => return format!("DOUBLE{unsigned}"),
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => {
            return format!(
                "DECIMAL({},{}){unsigned}",
                decimal_precision(col),
                col.decimals()
            );
        }
        ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => "DATE",
        ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2 => "TIME",
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_DATETIME2 => "DATETIME",
        ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2 => "TIMESTAMP",
        ColumnType::MYSQL_TYPE_YEAR => "YEAR",
        ColumnType::MYSQL_TYPE_BIT => return format!("BIT({})", col.column_length()),
        ColumnType::MYSQL_TYPE_JSON => "JSON",
        ColumnType::MYSQL_TYPE_ENUM => "ENUM",
        ColumnType::MYSQL_TYPE_SET => "SET",
        _ if flags.contains(ColumnFlags::ENUM_FLAG) => "ENUM",
        _ if flags.contains(ColumnFlags::SET_FLAG) => "SET",
        ColumnType::MYSQL_TYPE_STRING => {
            if binary {
                "BINARY"
            } else {
                "CHAR"
            }
        }
        ColumnType::MYSQL_TYPE_VARCHAR | ColumnType::MYSQL_TYPE_VAR_STRING => {
            if binary {
                "VARBINARY"
            } else {
                "VARCHAR"
            }
        }
        ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB => {
            if binary {
                "BLOB"
            } else {
                "TEXT"
            }
        }
        ColumnType::MYSQL_TYPE_GEOMETRY => "GEOMETRY",
        ColumnType::MYSQL_TYPE_VECTOR => "VECTOR",
        ColumnType::MYSQL_TYPE_TYPED_ARRAY | ColumnType::MYSQL_TYPE_UNKNOWN => "UNKNOWN",
    };
    name.to_owned()
}

/// The Arrow field for `col` decoded as `kind`.
pub(crate) fn field_for(col: &Column, kind: ColKind) -> Field {
    let mut metadata = HashMap::from([(META_NATIVE_TYPE.to_owned(), native_type_name(col))]);
    if kind == ColKind::Json {
        metadata.insert(META_EXTENSION_NAME.to_owned(), JSON_EXTENSION.to_owned());
    }
    Field::new(col.name_str(), arrow_type(kind), true).with_metadata(metadata)
}

/// Decoding plan for one result set: its Arrow schema and per-column kinds.
#[derive(Debug, Clone)]
pub(crate) struct ResultShape {
    pub(crate) schema: SchemaRef,
    pub(crate) kinds: Vec<ColKind>,
}

impl ResultShape {
    pub(crate) fn new(columns: &[Column], opts: TypeOptions) -> Self {
        let kinds: Vec<ColKind> = columns.iter().map(|c| column_kind(c, opts)).collect();
        let fields: Vec<Field> = columns
            .iter()
            .zip(&kinds)
            .map(|(c, k)| field_for(c, *k))
            .collect();
        Self {
            schema: Arc::new(Schema::new(fields)),
            kinds,
        }
    }
}

/// Builds one Arrow column from MySQL values.
pub(crate) enum ColumnBuilder {
    Null(usize),
    Bool(BooleanBuilder),
    Int8(Int8Builder),
    Int16(Int16Builder),
    Int32(Int32Builder),
    Int64(Int64Builder),
    UInt8(UInt8Builder),
    UInt16(UInt16Builder),
    UInt32(UInt32Builder),
    UInt64(UInt64Builder, bool),
    Float32(Float32Builder),
    Float64(Float64Builder),
    Decimal(Decimal128Builder, u8, i8),
    Utf8(StringBuilder),
    Binary(BinaryBuilder),
    Date(Date32Builder),
    Duration(DurationMicrosecondBuilder),
    Timestamp(TimestampMicrosecondBuilder),
}

/// What happened to one appended value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Appended {
    /// Approximate bytes the value added to the batch.
    pub(crate) bytes: usize,
    /// `true` when the value could not be represented exactly and was stored as NULL (or, for
    /// text, with replacement characters).
    pub(crate) lossy: bool,
}

impl ColumnBuilder {
    pub(crate) fn new(kind: ColKind) -> Self {
        match kind {
            ColKind::Null => Self::Null(0),
            ColKind::Bool => Self::Bool(BooleanBuilder::new()),
            ColKind::Int8 => Self::Int8(Int8Builder::new()),
            ColKind::Int16 => Self::Int16(Int16Builder::new()),
            ColKind::Int32 => Self::Int32(Int32Builder::new()),
            ColKind::Int64 => Self::Int64(Int64Builder::new()),
            ColKind::UInt8 => Self::UInt8(UInt8Builder::new()),
            ColKind::UInt16 | ColKind::Year => Self::UInt16(UInt16Builder::new()),
            ColKind::UInt32 => Self::UInt32(UInt32Builder::new()),
            ColKind::UInt64 => Self::UInt64(UInt64Builder::new(), false),
            ColKind::Bit => Self::UInt64(UInt64Builder::new(), true),
            ColKind::Float32 => Self::Float32(Float32Builder::new()),
            ColKind::Float64 => Self::Float64(Float64Builder::new()),
            ColKind::Decimal { precision, scale } => Self::Decimal(
                Decimal128Builder::new().with_data_type(ArrowType::Decimal128(precision, scale)),
                precision,
                scale,
            ),
            ColKind::DecimalText | ColKind::Text | ColKind::Json => {
                Self::Utf8(StringBuilder::new())
            }
            ColKind::Binary => Self::Binary(BinaryBuilder::new()),
            ColKind::Date => Self::Date(Date32Builder::new()),
            ColKind::Time => Self::Duration(DurationMicrosecondBuilder::new()),
            ColKind::DateTime | ColKind::Timestamp => {
                Self::Timestamp(TimestampMicrosecondBuilder::new())
            }
            ColKind::TimestampUtc => {
                Self::Timestamp(TimestampMicrosecondBuilder::new().with_timezone(TIMESTAMP_TZ))
            }
        }
    }

    fn append_null(&mut self) {
        match self {
            Self::Null(n) => *n += 1,
            Self::Bool(b) => b.append_null(),
            Self::Int8(b) => b.append_null(),
            Self::Int16(b) => b.append_null(),
            Self::Int32(b) => b.append_null(),
            Self::Int64(b) => b.append_null(),
            Self::UInt8(b) => b.append_null(),
            Self::UInt16(b) => b.append_null(),
            Self::UInt32(b) => b.append_null(),
            Self::UInt64(b, _) => b.append_null(),
            Self::Float32(b) => b.append_null(),
            Self::Float64(b) => b.append_null(),
            Self::Decimal(b, _, _) => b.append_null(),
            Self::Utf8(b) => b.append_null(),
            Self::Binary(b) => b.append_null(),
            Self::Date(b) => b.append_null(),
            Self::Duration(b) => b.append_null(),
            Self::Timestamp(b) => b.append_null(),
        }
    }

    /// Appends one value (text- or binary-protocol representation).
    #[allow(
        clippy::cast_possible_truncation,
        reason = "FLOAT columns hold f32 values; the f64 detour of the text protocol is exact"
    )]
    pub(crate) fn append(&mut self, value: MyValue) -> Appended {
        if matches!(value, MyValue::NULL) {
            self.append_null();
            return Appended {
                bytes: 1,
                lossy: false,
            };
        }
        let fixed = |ok: Option<()>| ok.map(|()| FIXED_CELL_BYTES);
        let stored: Option<usize> = match self {
            Self::Null(_) => None,
            Self::Bool(b) => fixed(to_i64(&value).map(|v| b.append_value(v != 0))),
            Self::Int8(b) => fixed(int(&value).map(|v| b.append_value(v))),
            Self::Int16(b) => fixed(int(&value).map(|v| b.append_value(v))),
            Self::Int32(b) => fixed(int(&value).map(|v| b.append_value(v))),
            Self::Int64(b) => fixed(to_i64(&value).map(|v| b.append_value(v))),
            Self::UInt8(b) => fixed(uint(&value).map(|v| b.append_value(v))),
            Self::UInt16(b) => fixed(uint(&value).map(|v| b.append_value(v))),
            Self::UInt32(b) => fixed(uint(&value).map(|v| b.append_value(v))),
            Self::UInt64(b, true) => fixed(to_bit(&value).map(|v| b.append_value(v))),
            Self::UInt64(b, false) => fixed(to_u64(&value).map(|v| b.append_value(v))),
            Self::Float32(b) => fixed(to_f64(&value).map(|v| b.append_value(v as f32))),
            Self::Float64(b) => fixed(to_f64(&value).map(|v| b.append_value(v))),
            Self::Decimal(b, precision, scale) => {
                fixed(to_decimal(&value, *precision, *scale).map(|v| b.append_value(v)))
            }
            Self::Utf8(b) => {
                let (text, lossy) = to_text(value);
                let n = text.len();
                b.append_value(text);
                return Appended { bytes: n, lossy };
            }
            Self::Binary(b) => {
                let bytes = to_bytes(value);
                let n = bytes.len();
                b.append_value(bytes);
                Some(n)
            }
            Self::Date(b) => fixed(to_date_days(&value).map(|v| b.append_value(v))),
            Self::Duration(b) => fixed(to_time_micros(&value).map(|v| b.append_value(v))),
            Self::Timestamp(b) => fixed(to_datetime_micros(&value).map(|v| b.append_value(v))),
        };
        if let Some(bytes) = stored {
            Appended {
                bytes,
                lossy: false,
            }
        } else {
            self.append_null();
            Appended {
                bytes: 1,
                lossy: true,
            }
        }
    }

    pub(crate) fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Null(n) => {
                let len = std::mem::take(n);
                Arc::new(NullArray::new(len))
            }
            Self::Bool(b) => Arc::new(b.finish()),
            Self::Int8(b) => Arc::new(b.finish()),
            Self::Int16(b) => Arc::new(b.finish()),
            Self::Int32(b) => Arc::new(b.finish()),
            Self::Int64(b) => Arc::new(b.finish()),
            Self::UInt8(b) => Arc::new(b.finish()),
            Self::UInt16(b) => Arc::new(b.finish()),
            Self::UInt32(b) => Arc::new(b.finish()),
            Self::UInt64(b, _) => Arc::new(b.finish()),
            Self::Float32(b) => Arc::new(b.finish()),
            Self::Float64(b) => Arc::new(b.finish()),
            Self::Decimal(b, _, _) => Arc::new(b.finish()),
            Self::Utf8(b) => Arc::new(b.finish()),
            Self::Binary(b) => Arc::new(b.finish()),
            Self::Date(b) => Arc::new(b.finish()),
            Self::Duration(b) => Arc::new(b.finish()),
            Self::Timestamp(b) => Arc::new(b.finish()),
        }
    }
}

/// Accumulates rows of one result set into `RecordBatch`es.
pub(crate) struct BatchBuilder {
    schema: SchemaRef,
    columns: Vec<ColumnBuilder>,
    rows: usize,
    bytes: usize,
    /// Values stored lossily in this result set, per column index.
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
    pub(crate) fn push(&mut self, values: Vec<Option<MyValue>>) {
        let mut values = values.into_iter();
        for (i, builder) in self.columns.iter_mut().enumerate() {
            let value = values.next().flatten().unwrap_or(MyValue::NULL);
            let appended = builder.append(value);
            self.bytes += appended.bytes;
            if appended.lossy {
                self.lossy[i] += 1;
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
        self.rows = 0;
        self.bytes = 0;
        RecordBatch::try_new(self.schema.clone(), arrays).map_err(from_arrow)
    }

    /// Names and counts of the columns that had lossy values so far (and resets the counts).
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

fn ascii(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok().map(str::trim)
}

fn to_i64(v: &MyValue) -> Option<i64> {
    match v {
        MyValue::Int(i) => Some(*i),
        MyValue::UInt(u) => i64::try_from(*u).ok(),
        MyValue::Bytes(b) => ascii(b)?.parse().ok(),
        _ => None,
    }
}

fn to_u64(v: &MyValue) -> Option<u64> {
    match v {
        MyValue::Int(i) => u64::try_from(*i).ok(),
        MyValue::UInt(u) => Some(*u),
        MyValue::Bytes(b) => ascii(b)?.parse().ok(),
        _ => None,
    }
}

fn int<T: TryFrom<i64>>(v: &MyValue) -> Option<T> {
    to_i64(v).and_then(|i| T::try_from(i).ok())
}

fn uint<T: TryFrom<u64>>(v: &MyValue) -> Option<T> {
    to_u64(v).and_then(|i| T::try_from(i).ok())
}

#[allow(
    clippy::cast_precision_loss,
    reason = "integers only reach float columns through server-side casts"
)]
fn to_f64(v: &MyValue) -> Option<f64> {
    match v {
        MyValue::Float(f) => Some(f64::from(*f)),
        MyValue::Double(d) => Some(*d),
        MyValue::Int(i) => Some(*i as f64),
        MyValue::UInt(u) => Some(*u as f64),
        MyValue::Bytes(b) => ascii(b)?.parse().ok(),
        _ => None,
    }
}

/// `BIT(n)` values arrive as big-endian bytes in both protocols.
fn to_bit(v: &MyValue) -> Option<u64> {
    match v {
        MyValue::Bytes(b) if b.len() <= 8 => Some(
            b.iter()
                .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte)),
        ),
        MyValue::Int(i) => u64::try_from(*i).ok(),
        MyValue::UInt(u) => Some(*u),
        _ => None,
    }
}

/// Parses decimal text (`-123.4500`) into an `i128` scaled by `scale`, checking it fits
/// `precision` digits.
pub(crate) fn parse_decimal(text: &str, precision: u8, scale: i8) -> Option<i128> {
    let scale = usize::try_from(scale).ok()?;
    let text = text.trim();
    let (negative, digits) = match text.as_bytes().first()? {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let (int_part, frac_part) = digits.split_once('.').unwrap_or((digits, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part
        .bytes()
        .chain(frac_part.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    // Extra fraction digits must be zeros (the server sends exactly `scale` digits).
    let (kept, dropped) = frac_part.split_at(frac_part.len().min(scale));
    if dropped.bytes().any(|b| b != b'0') {
        return None;
    }
    let mut value: i128 = 0;
    for b in int_part.bytes().chain(kept.bytes()) {
        value = value.checked_mul(10)?.checked_add(i128::from(b - b'0'))?;
    }
    for _ in kept.len()..scale {
        value = value.checked_mul(10)?;
    }
    let limit = 10i128.checked_pow(u32::from(precision))?;
    if value >= limit {
        return None;
    }
    Some(if negative { -value } else { value })
}

fn to_decimal(v: &MyValue, precision: u8, scale: i8) -> Option<i128> {
    match v {
        MyValue::Bytes(b) => parse_decimal(ascii(b)?, precision, scale),
        MyValue::Int(i) => parse_decimal(&i.to_string(), precision, scale),
        MyValue::UInt(u) => parse_decimal(&u.to_string(), precision, scale),
        _ => None,
    }
}

fn epoch() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default()
}

fn ymd(y: u16, m: u8, d: u8) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(i32::from(y), u32::from(m), u32::from(d))
}

fn parse_date_text(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()
}

fn to_date_days(v: &MyValue) -> Option<i32> {
    let date = match v {
        MyValue::Date(y, m, d, ..) => ymd(*y, *m, *d)?,
        MyValue::Bytes(b) => parse_date_text(ascii(b)?)?,
        _ => return None,
    };
    i32::try_from(date.signed_duration_since(epoch()).num_days()).ok()
}

fn to_datetime_micros(v: &MyValue) -> Option<i64> {
    let dt = match v {
        MyValue::Date(year, month, day, hour, minute, second, micros) => ymd(*year, *month, *day)?
            .and_hms_micro_opt(
                u32::from(*hour),
                u32::from(*minute),
                u32::from(*second),
                *micros,
            )?,
        MyValue::Bytes(raw) => {
            let text = ascii(raw)?;
            match NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f") {
                Ok(dt) => dt,
                // A DATETIME may come back as a bare date from some expressions.
                Err(_) => parse_date_text(text)?.and_hms_opt(0, 0, 0)?,
            }
        }
        _ => return None,
    };
    Some(dt.and_utc().timestamp_micros())
}

/// Parses MySQL `TIME` text (`[-]HHH:MM:SS[.ffffff]`) into signed microseconds.
pub(crate) fn parse_time_text(s: &str) -> Option<i64> {
    let s = s.trim();
    let (negative, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let mut parts = body.splitn(3, ':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = parts.next()?.parse().ok()?;
    let sec_text = parts.next()?;
    let (secs, frac) = sec_text.split_once('.').unwrap_or((sec_text, ""));
    let seconds: i64 = secs.parse().ok()?;
    if !(0..60).contains(&minutes) || !(0..60).contains(&seconds) || frac.len() > 6 {
        return None;
    }
    let mut micros: i64 = 0;
    if !frac.is_empty() {
        micros = frac.parse().ok()?;
        for _ in frac.len()..6 {
            micros *= 10;
        }
    }
    let total = ((hours * 60 + minutes) * 60 + seconds)
        .checked_mul(1_000_000)?
        .checked_add(micros)?;
    Some(if negative { -total } else { total })
}

fn to_time_micros(v: &MyValue) -> Option<i64> {
    match v {
        MyValue::Time(negative, days, h, m, s, us) => {
            let secs =
                ((i64::from(*days) * 24 + i64::from(*h)) * 60 + i64::from(*m)) * 60 + i64::from(*s);
            let total = secs.checked_mul(1_000_000)?.checked_add(i64::from(*us))?;
            Some(if *negative { -total } else { total })
        }
        MyValue::Bytes(b) => parse_time_text(ascii(b)?),
        _ => None,
    }
}

/// Text rendering of any value, plus whether invalid UTF-8 had to be replaced.
fn to_text(v: MyValue) -> (String, bool) {
    match v {
        MyValue::Bytes(b) => match String::from_utf8(b) {
            Ok(s) => (s, false),
            Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
        },
        MyValue::NULL => (String::new(), false),
        MyValue::Int(i) => (i.to_string(), false),
        MyValue::UInt(u) => (u.to_string(), false),
        MyValue::Float(f) => (f.to_string(), false),
        MyValue::Double(d) => (d.to_string(), false),
        MyValue::Date(..) | MyValue::Time(..) => {
            (v.as_sql(true).trim_matches('\'').to_owned(), false)
        }
    }
}

fn to_bytes(v: MyValue) -> Vec<u8> {
    match v {
        MyValue::Bytes(b) => b,
        other => to_text(other).0.into_bytes(),
    }
}

// ---- parameters -----------------------------------------------------------------------------

fn micros_of(t: NaiveTime) -> u32 {
    // Leap-second nanoseconds (>= 1e9) clamp to the last microsecond.
    (t.nanosecond() / 1000).min(999_999)
}

fn datetime_param(dt: &NaiveDateTime) -> MyValue {
    let date = dt.date();
    let time = dt.time();
    match (
        u16::try_from(date.year()),
        u8::try_from(date.month()),
        u8::try_from(date.day()),
        u8::try_from(time.hour()),
        u8::try_from(time.minute()),
        u8::try_from(time.second()),
    ) {
        (Ok(year), Ok(month), Ok(day), Ok(hour), Ok(minute), Ok(second)) => {
            MyValue::Date(year, month, day, hour, minute, second, micros_of(time))
        }
        _ => MyValue::Bytes(dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string().into_bytes()),
    }
}

/// Converts an oxbase parameter to a MySQL binary-protocol value.
///
/// `Decimal`, `Uuid` and `Json` are sent as text; `TimestampTz` is converted to UTC (MySQL has
/// no zoned type), which is the instant a `TIMESTAMP` column stores because the driver sets the
/// session time zone to UTC (see [`crate::MySqlOptions::utc_session`]); `Bool` is sent as
/// `0`/`1`.
pub(crate) fn to_param(v: &Value) -> MyValue {
    match v {
        Value::Null => MyValue::NULL,
        Value::Bool(b) => MyValue::Int(i64::from(*b)),
        Value::Int(i) => MyValue::Int(*i),
        Value::UInt(u) => MyValue::UInt(*u),
        Value::Float(f) => MyValue::Double(*f),
        Value::Decimal(s) | Value::Text(s) => MyValue::Bytes(s.as_bytes().to_vec()),
        Value::Bytes(b) => MyValue::Bytes(b.clone()),
        Value::Date(d) => datetime_param(&d.and_time(NaiveTime::MIN)),
        Value::Time(time) => match (
            u8::try_from(time.hour()),
            u8::try_from(time.minute()),
            u8::try_from(time.second()),
        ) {
            (Ok(hour), Ok(minute), Ok(second)) => {
                MyValue::Time(false, 0, hour, minute, second, micros_of(*time))
            }
            _ => MyValue::Bytes(time.format("%H:%M:%S%.6f").to_string().into_bytes()),
        },
        Value::Timestamp(ts) => datetime_param(ts),
        Value::TimestampTz(ts) => datetime_param(&ts.naive_utc()),
        Value::Uuid(u) => MyValue::Bytes(u.to_string().into_bytes()),
        Value::Json(j) => MyValue::Bytes(j.to_string().into_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::cast::AsArray;
    use arrow_array::types::{
        Date32Type, Decimal128Type, DurationMicrosecondType, Int8Type, Int32Type, Int64Type,
        TimestampMicrosecondType, UInt8Type, UInt16Type, UInt64Type,
    };
    use arrow_array::{Array, BooleanArray};

    fn col(ty: ColumnType) -> Column {
        Column::new(ty).with_name(b"c").with_character_set(45)
    }

    fn unsigned(c: Column) -> Column {
        let flags = c.flags() | ColumnFlags::UNSIGNED_FLAG;
        c.with_flags(flags)
    }

    fn kind(c: &Column) -> ColKind {
        column_kind(c, TypeOptions::default())
    }

    #[test]
    fn integer_mapping() {
        let tiny1 = col(ColumnType::MYSQL_TYPE_TINY).with_column_length(1);
        assert_eq!(kind(&tiny1), ColKind::Bool);
        let no_bool = TypeOptions {
            tinyint1_as_bool: false,
            ..TypeOptions::default()
        };
        assert_eq!(column_kind(&tiny1, no_bool), ColKind::Int8);
        assert_eq!(native_type_name(&tiny1), "TINYINT(1)");

        let tiny = col(ColumnType::MYSQL_TYPE_TINY).with_column_length(4);
        assert_eq!(kind(&tiny), ColKind::Int8);
        assert_eq!(kind(&unsigned(tiny.clone())), ColKind::UInt8);
        assert_eq!(native_type_name(&unsigned(tiny)), "TINYINT UNSIGNED");

        for (ty, signed, uns) in [
            (
                ColumnType::MYSQL_TYPE_SHORT,
                ColKind::Int16,
                ColKind::UInt16,
            ),
            (
                ColumnType::MYSQL_TYPE_INT24,
                ColKind::Int32,
                ColKind::UInt32,
            ),
            (ColumnType::MYSQL_TYPE_LONG, ColKind::Int32, ColKind::UInt32),
            (
                ColumnType::MYSQL_TYPE_LONGLONG,
                ColKind::Int64,
                ColKind::UInt64,
            ),
        ] {
            let c = col(ty).with_column_length(11);
            assert_eq!(kind(&c), signed, "{ty:?}");
            assert_eq!(kind(&unsigned(c)), uns, "{ty:?}");
        }
        assert_eq!(arrow_type(ColKind::UInt64), ArrowType::UInt64);
        assert_eq!(arrow_type(ColKind::Int16), ArrowType::Int16);
    }

    #[test]
    fn decimal_mapping() {
        // DECIMAL(12,2) signed: length = 12 + point + sign.
        let d = col(ColumnType::MYSQL_TYPE_NEWDECIMAL)
            .with_column_length(14)
            .with_decimals(2);
        assert_eq!(
            kind(&d),
            ColKind::Decimal {
                precision: 12,
                scale: 2
            }
        );
        assert_eq!(native_type_name(&d), "DECIMAL(12,2)");
        // DECIMAL(10,0) UNSIGNED: no point, no sign.
        let d = unsigned(col(ColumnType::MYSQL_TYPE_NEWDECIMAL).with_column_length(10));
        assert_eq!(
            kind(&d),
            ColKind::Decimal {
                precision: 10,
                scale: 0
            }
        );
        // DECIMAL(65,10) does not fit Decimal128.
        let wide = col(ColumnType::MYSQL_TYPE_NEWDECIMAL)
            .with_column_length(67)
            .with_decimals(10);
        assert_eq!(kind(&wide), ColKind::DecimalText);
        assert_eq!(arrow_type(ColKind::DecimalText), ArrowType::Utf8);
        assert_eq!(
            arrow_type(ColKind::Decimal {
                precision: 12,
                scale: 2
            }),
            ArrowType::Decimal128(12, 2)
        );
    }

    #[test]
    fn temporal_and_misc_mapping() {
        assert_eq!(kind(&col(ColumnType::MYSQL_TYPE_FLOAT)), ColKind::Float32);
        assert_eq!(kind(&col(ColumnType::MYSQL_TYPE_DOUBLE)), ColKind::Float64);
        assert_eq!(kind(&col(ColumnType::MYSQL_TYPE_DATE)), ColKind::Date);
        assert_eq!(kind(&col(ColumnType::MYSQL_TYPE_TIME)), ColKind::Time);
        assert_eq!(
            kind(&col(ColumnType::MYSQL_TYPE_DATETIME)),
            ColKind::DateTime
        );
        assert_eq!(
            kind(&col(ColumnType::MYSQL_TYPE_TIMESTAMP)),
            ColKind::Timestamp
        );
        assert_eq!(kind(&col(ColumnType::MYSQL_TYPE_YEAR)), ColKind::Year);
        assert_eq!(kind(&col(ColumnType::MYSQL_TYPE_BIT)), ColKind::Bit);
        assert_eq!(kind(&col(ColumnType::MYSQL_TYPE_NULL)), ColKind::Null);
        assert_eq!(kind(&col(ColumnType::MYSQL_TYPE_GEOMETRY)), ColKind::Binary);
        assert_eq!(arrow_type(ColKind::Date), ArrowType::Date32);
        assert_eq!(
            arrow_type(ColKind::Time),
            ArrowType::Duration(TimeUnit::Microsecond)
        );
        assert_eq!(
            arrow_type(ColKind::Timestamp),
            ArrowType::Timestamp(TimeUnit::Microsecond, None)
        );
        let utc = TypeOptions {
            timestamp_utc: true,
            ..TypeOptions::default()
        };
        let ts = col(ColumnType::MYSQL_TYPE_TIMESTAMP);
        assert_eq!(column_kind(&ts, utc), ColKind::TimestampUtc);
        // DATETIME has no zone, whatever the session.
        let dt = col(ColumnType::MYSQL_TYPE_DATETIME);
        assert_eq!(column_kind(&dt, utc), ColKind::DateTime);
        assert_eq!(
            arrow_type(ColKind::TimestampUtc),
            ArrowType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
        assert_eq!(arrow_type(ColKind::Year), ArrowType::UInt16);
        assert_eq!(arrow_type(ColKind::Bit), ArrowType::UInt64);
        assert_eq!(arrow_type(ColKind::Null), ArrowType::Null);
    }

    #[test]
    fn string_mapping_by_charset_and_flags() {
        let varchar = col(ColumnType::MYSQL_TYPE_VAR_STRING);
        assert_eq!(kind(&varchar), ColKind::Text);
        assert_eq!(native_type_name(&varchar), "VARCHAR");
        let varbinary = varchar.clone().with_character_set(BINARY_CHARSET);
        assert_eq!(kind(&varbinary), ColKind::Binary);
        assert_eq!(native_type_name(&varbinary), "VARBINARY");
        let text = col(ColumnType::MYSQL_TYPE_BLOB);
        assert_eq!(kind(&text), ColKind::Text);
        let blob = text.clone().with_character_set(BINARY_CHARSET);
        assert_eq!(kind(&blob), ColKind::Binary);
        assert_eq!(native_type_name(&blob), "BLOB");
        let char_col = col(ColumnType::MYSQL_TYPE_STRING);
        assert_eq!(native_type_name(&char_col), "CHAR");
        let enum_col = col(ColumnType::MYSQL_TYPE_STRING).with_flags(ColumnFlags::ENUM_FLAG);
        assert_eq!(kind(&enum_col), ColKind::Text);
        assert_eq!(native_type_name(&enum_col), "ENUM");
        let set_col = col(ColumnType::MYSQL_TYPE_STRING)
            .with_flags(ColumnFlags::SET_FLAG)
            .with_character_set(BINARY_CHARSET);
        assert_eq!(kind(&set_col), ColKind::Text);
        assert_eq!(native_type_name(&set_col), "SET");
    }

    #[test]
    fn json_field_has_extension_metadata() {
        let json = col(ColumnType::MYSQL_TYPE_JSON).with_character_set(BINARY_CHARSET);
        let k = kind(&json);
        assert_eq!(k, ColKind::Json);
        let field = field_for(&json, k);
        assert_eq!(field.data_type(), &ArrowType::Utf8);
        assert!(field.is_nullable());
        assert_eq!(
            field
                .metadata()
                .get(META_EXTENSION_NAME)
                .map(String::as_str),
            Some(JSON_EXTENSION)
        );
        assert_eq!(
            field.metadata().get(META_NATIVE_TYPE).map(String::as_str),
            Some("JSON")
        );
        assert_eq!(field.name(), "c");
    }

    #[test]
    fn decimal_parsing() {
        assert_eq!(parse_decimal("123.45", 12, 2), Some(12345));
        assert_eq!(parse_decimal("-0.50", 12, 2), Some(-50));
        assert_eq!(parse_decimal("7", 12, 2), Some(700));
        assert_eq!(parse_decimal("1.5", 12, 3), Some(1500));
        assert_eq!(parse_decimal(".25", 4, 2), Some(25));
        assert_eq!(parse_decimal("1.230", 4, 2), Some(123));
        assert_eq!(parse_decimal("1.234", 4, 2), None, "would round");
        assert_eq!(parse_decimal("100.00", 4, 2), None, "exceeds precision");
        assert_eq!(parse_decimal("99.99", 4, 2), Some(9999));
        assert_eq!(parse_decimal("abc", 4, 2), None);
        assert_eq!(parse_decimal("", 4, 2), None);
        assert_eq!(parse_decimal("-", 4, 2), None);
        let max38 = "9".repeat(38);
        assert_eq!(
            parse_decimal(&max38, 38, 0),
            Some(10i128.pow(38) - 1),
            "38 nines fit"
        );
    }

    #[test]
    fn time_parsing() {
        assert_eq!(parse_time_text("00:00:01"), Some(1_000_000));
        assert_eq!(parse_time_text("-01:02:03.5"), Some(-3_723_500_000));
        assert_eq!(
            parse_time_text("838:59:59.000000"),
            Some(((838 * 60 + 59) * 60 + 59) * 1_000_000)
        );
        assert_eq!(parse_time_text("1:60:00"), None);
        assert_eq!(parse_time_text("x"), None);
        assert_eq!(
            to_time_micros(&MyValue::Time(true, 1, 2, 3, 4, 5)),
            Some(-(((24 + 2) * 60 + 3) * 60 + 4) * 1_000_000 - 5)
        );
    }

    fn build(kind: ColKind, values: Vec<MyValue>) -> (ArrayRef, Vec<bool>) {
        let mut b = ColumnBuilder::new(kind);
        let lossy = values.into_iter().map(|v| b.append(v).lossy).collect();
        (b.finish(), lossy)
    }

    fn bytes(s: &str) -> MyValue {
        MyValue::Bytes(s.as_bytes().to_vec())
    }

    #[test]
    fn builds_integers_from_both_protocols() {
        let (a, lossy) = build(
            ColKind::Int8,
            vec![bytes("-5"), MyValue::Int(7), MyValue::NULL, bytes("300")],
        );
        let a = a.as_primitive::<Int8Type>();
        assert_eq!(a.value(0), -5);
        assert_eq!(a.value(1), 7);
        assert!(a.is_null(2));
        assert!(a.is_null(3), "out of range");
        assert_eq!(lossy, [false, false, false, true]);

        let (a, _) = build(
            ColKind::UInt64,
            vec![bytes("18446744073709551615"), MyValue::UInt(1)],
        );
        assert_eq!(a.as_primitive::<UInt64Type>().value(0), u64::MAX);
        let (a, _) = build(ColKind::UInt8, vec![MyValue::Int(200)]);
        assert_eq!(a.as_primitive::<UInt8Type>().value(0), 200);
        let (a, _) = build(ColKind::Int32, vec![bytes("-2147483648")]);
        assert_eq!(a.as_primitive::<Int32Type>().value(0), i32::MIN);
        let (a, _) = build(ColKind::Int64, vec![MyValue::Int(i64::MIN)]);
        assert_eq!(a.as_primitive::<Int64Type>().value(0), i64::MIN);

        let (a, _) = build(ColKind::Bool, vec![bytes("1"), MyValue::Int(0), bytes("5")]);
        let a = a.as_any().downcast_ref::<BooleanArray>().unwrap();
        assert!(a.value(0) && !a.value(1) && a.value(2));

        let (a, _) = build(ColKind::Year, vec![bytes("2026"), MyValue::Int(1999)]);
        assert_eq!(a.as_primitive::<UInt16Type>().value(0), 2026);
        assert_eq!(a.as_primitive::<UInt16Type>().value(1), 1999);

        let (a, _) = build(
            ColKind::Bit,
            vec![MyValue::Bytes(vec![0x01, 0x02]), MyValue::Bytes(vec![0x05])],
        );
        assert_eq!(a.as_primitive::<UInt64Type>().value(0), 0x0102);
        assert_eq!(a.as_primitive::<UInt64Type>().value(1), 5);
    }

    #[test]
    fn builds_decimal_temporal_and_text() {
        let (a, lossy) = build(
            ColKind::Decimal {
                precision: 12,
                scale: 2,
            },
            vec![bytes("120.50"), bytes("-0.01"), bytes("1e5")],
        );
        let d = a.as_primitive::<Decimal128Type>();
        assert_eq!(d.value(0), 12050);
        assert_eq!(d.value(1), -1);
        assert!(d.is_null(2));
        assert_eq!(lossy, [false, false, true]);
        assert_eq!(a.data_type(), &ArrowType::Decimal128(12, 2));

        let (a, lossy) = build(
            ColKind::Date,
            vec![
                bytes("1970-01-02"),
                MyValue::Date(2026, 10, 31, 0, 0, 0, 0),
                bytes("0000-00-00"),
            ],
        );
        let d = a.as_primitive::<Date32Type>();
        assert_eq!(d.value(0), 1);
        assert_eq!(d.value(1), 20_757);
        assert!(d.is_null(2));
        assert_eq!(lossy, [false, false, true]);

        let (a, _) = build(
            ColKind::DateTime,
            vec![
                bytes("1970-01-01 00:00:01.5"),
                MyValue::Date(1970, 1, 1, 0, 1, 0, 7),
                bytes("1970-01-01 00:00:02"),
            ],
        );
        let t = a.as_primitive::<TimestampMicrosecondType>();
        assert_eq!(t.value(0), 1_500_000);
        assert_eq!(t.value(1), 60_000_007);
        assert_eq!(t.value(2), 2_000_000);

        let (a, _) = build(
            ColKind::Time,
            vec![bytes("-00:00:01"), MyValue::Time(false, 0, 1, 0, 0, 0)],
        );
        let t = a.as_primitive::<DurationMicrosecondType>();
        assert_eq!(t.value(0), -1_000_000);
        assert_eq!(t.value(1), 3_600_000_000);

        let (a, lossy) = build(
            ColKind::Text,
            vec![bytes("héllo"), MyValue::Bytes(vec![0xff]), MyValue::Int(3)],
        );
        let s = a.as_string::<i32>();
        assert_eq!(s.value(0), "héllo");
        assert_eq!(s.value(1), "\u{fffd}");
        assert_eq!(s.value(2), "3");
        assert_eq!(lossy, [false, true, false]);

        let (a, _) = build(ColKind::Binary, vec![MyValue::Bytes(vec![0, 1, 2])]);
        assert_eq!(a.as_binary::<i32>().value(0), [0, 1, 2]);

        let (a, lossy) = build(ColKind::Null, vec![MyValue::NULL, MyValue::NULL]);
        assert_eq!(a.len(), 2);
        assert_eq!(a.data_type(), &ArrowType::Null);
        assert_eq!(lossy, [false, false]);
    }

    #[test]
    fn batch_builder_splits_and_reports_lossy() {
        let cols = [
            Column::new(ColumnType::MYSQL_TYPE_LONG)
                .with_name(b"id")
                .with_column_length(11),
            Column::new(ColumnType::MYSQL_TYPE_DATE).with_name(b"d"),
        ];
        let shape = ResultShape::new(&cols, TypeOptions::default());
        assert_eq!(shape.schema.field(0).name(), "id");
        assert_eq!(shape.schema.field(1).data_type(), &ArrowType::Date32);
        let mut b = BatchBuilder::new(&shape);
        b.push(vec![Some(bytes("1")), Some(bytes("2026-01-01"))]);
        b.push(vec![Some(bytes("2")), Some(bytes("0000-00-00"))]);
        b.push(vec![Some(bytes("3"))]);
        assert_eq!(b.rows(), 3);
        assert!(b.bytes() > 0);
        let batch = b.finish().unwrap();
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(b.rows(), 0);
        assert_eq!(b.take_lossy(), [("d".to_owned(), 1)]);
        assert_eq!(b.take_lossy(), []);
        b.push(vec![Some(bytes("4")), None]);
        assert_eq!(b.finish().unwrap().num_rows(), 1);
    }

    #[test]
    fn params_convert() {
        use chrono::{DateTime, NaiveDate};
        assert_eq!(to_param(&Value::Null), MyValue::NULL);
        assert_eq!(to_param(&Value::Bool(true)), MyValue::Int(1));
        assert_eq!(to_param(&Value::UInt(u64::MAX)), MyValue::UInt(u64::MAX));
        assert_eq!(to_param(&Value::Float(1.5)), MyValue::Double(1.5));
        assert_eq!(
            to_param(&Value::Decimal("1.50".into())),
            MyValue::Bytes(b"1.50".to_vec())
        );
        let d = NaiveDate::from_ymd_opt(2026, 10, 31).unwrap();
        assert_eq!(
            to_param(&Value::Date(d)),
            MyValue::Date(2026, 10, 31, 0, 0, 0, 0)
        );
        let t = NaiveTime::from_hms_micro_opt(1, 2, 3, 4).unwrap();
        assert_eq!(
            to_param(&Value::Time(t)),
            MyValue::Time(false, 0, 1, 2, 3, 4)
        );
        let tz = DateTime::parse_from_rfc3339("2026-10-31T12:00:00+02:00").unwrap();
        assert_eq!(
            to_param(&Value::TimestampTz(tz)),
            MyValue::Date(2026, 10, 31, 10, 0, 0, 0)
        );
        let json = Value::Json(serde_json::json!([1, 2]));
        assert_eq!(to_param(&json), MyValue::Bytes(b"[1,2]".to_vec()));
    }
}
