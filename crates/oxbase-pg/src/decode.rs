//! Builds Arrow `RecordBatch`es from PostgreSQL rows, a bounded chunk at a time.
//!
//! Values arrive either as text (simple query protocol) or binary (extended protocol); both are
//! turned into a [`Cell`] and appended to the column's Arrow builder. A value that cannot be
//! represented (an infinite date, a `NaN` in a decimal column, an unexpected text format) becomes
//! NULL and the column gets one warning notice per result set.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Date32Builder, Decimal128Builder, FixedSizeBinaryBuilder,
    Float32Builder, Float64Builder, Int16Builder, Int32Builder, Int64Builder, StringBuilder,
    Time64MicrosecondBuilder, TimestampMicrosecondBuilder, UInt32Builder,
};
use arrow_array::{ArrayRef, RecordBatch, RecordBatchOptions};
use arrow_schema::{Schema, SchemaRef};
use oxbase_driver::{Notice, from_arrow};
use oxbase_types::Result;
use tokio_postgres::types::{Field as PgField, Kind, Type};

use crate::types::{ColKind, ColSpec};
use crate::values::{
    PG_EPOCH_DAYS, PG_EPOCH_MICROS, Parsed, Problem, format_date, format_interval, format_time,
    format_timestamp, numeric_bin_to_text, parse_bool, parse_bytea, parse_date, parse_decimal,
    parse_f32, parse_f64, parse_time, parse_timestamp, parse_timestamptz, parse_uuid,
};

/// Rows per batch.
pub const BATCH_ROWS: usize = 1024;
/// A batch is also cut once its values reach about this many bytes (wide rows).
pub(crate) const BATCH_BYTES: usize = 8 << 20;

/// One decoded value, ready to append to its column's builder.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Cell<'a> {
    Bool(bool),
    I16(i16),
    I32(i32),
    I64(i64),
    U32(u32),
    F32(f32),
    F64(f64),
    Decimal(i128),
    Str(Cow<'a, str>),
    Bin(Cow<'a, [u8]>),
    /// Days since 1970-01-01.
    Date(i32),
    /// Microseconds since midnight.
    Time(i64),
    /// Microseconds since 1970-01-01 (UTC for timestamptz).
    Timestamp(i64),
    Uuid([u8; 16]),
}

/// A value in the server's text format.
pub(crate) fn decode_text(kind: ColKind, s: &str) -> Parsed<Cell<'_>> {
    Ok(match kind {
        ColKind::Bool => Cell::Bool(parse_bool(s)?),
        ColKind::Int2 => Cell::I16(s.parse().map_err(|_| Problem::Unparsable)?),
        ColKind::Int4 => Cell::I32(s.parse().map_err(|_| Problem::Unparsable)?),
        ColKind::Int8 => Cell::I64(s.parse().map_err(|_| Problem::Unparsable)?),
        ColKind::Oid => Cell::U32(s.parse().map_err(|_| Problem::Unparsable)?),
        ColKind::Float4 => Cell::F32(parse_f32(s)?),
        ColKind::Float8 => Cell::F64(parse_f64(s)?),
        ColKind::Decimal { precision, scale } => Cell::Decimal(parse_decimal(s, precision, scale)?),
        ColKind::Bytea => Cell::Bin(Cow::Owned(parse_bytea(s)?)),
        ColKind::Date => Cell::Date(parse_date(s)?),
        ColKind::Time => Cell::Time(parse_time(s)?),
        ColKind::Timestamp => Cell::Timestamp(parse_timestamp(s)?),
        ColKind::TimestampTz => Cell::Timestamp(parse_timestamptz(s)?),
        ColKind::Uuid => Cell::Uuid(parse_uuid(s)?),
        ColKind::Numeric
        | ColKind::Text
        | ColKind::Json
        | ColKind::Jsonb
        | ColKind::Interval
        | ColKind::Other => Cell::Str(Cow::Borrowed(s)),
    })
}

fn be<const N: usize>(raw: &[u8]) -> Parsed<[u8; N]> {
    raw.try_into().map_err(|_| Problem::Unparsable)
}

fn pg_date(raw: &[u8]) -> Parsed<i32> {
    match i32::from_be_bytes(be(raw)?) {
        i32::MAX | i32::MIN => Err(Problem::Infinite),
        d => i32::try_from(i64::from(d) + PG_EPOCH_DAYS).map_err(|_| Problem::OutOfRange),
    }
}

fn pg_timestamp(raw: &[u8]) -> Parsed<i64> {
    match i64::from_be_bytes(be(raw)?) {
        i64::MAX | i64::MIN => Err(Problem::Infinite),
        t => t.checked_add(PG_EPOCH_MICROS).ok_or(Problem::OutOfRange),
    }
}

/// A value in binary format. `ty` is the column's PostgreSQL type (for [`ColKind::Other`]).
pub(crate) fn decode_binary<'a>(
    kind: ColKind,
    ty: Option<&Type>,
    raw: &'a [u8],
) -> Parsed<Cell<'a>> {
    Ok(match kind {
        ColKind::Bool => Cell::Bool(*raw.first().ok_or(Problem::Unparsable)? != 0),
        ColKind::Int2 => Cell::I16(i16::from_be_bytes(be(raw)?)),
        ColKind::Int4 => Cell::I32(i32::from_be_bytes(be(raw)?)),
        ColKind::Int8 => Cell::I64(i64::from_be_bytes(be(raw)?)),
        ColKind::Oid => Cell::U32(u32::from_be_bytes(be(raw)?)),
        ColKind::Float4 => Cell::F32(f32::from_be_bytes(be(raw)?)),
        ColKind::Float8 => Cell::F64(f64::from_be_bytes(be(raw)?)),
        ColKind::Decimal { precision, scale } => {
            Cell::Decimal(parse_decimal(&numeric_bin_to_text(raw)?, precision, scale)?)
        }
        ColKind::Numeric => Cell::Str(Cow::Owned(numeric_bin_to_text(raw)?)),
        ColKind::Text | ColKind::Json => Cell::Str(String::from_utf8_lossy(raw)),
        ColKind::Jsonb => match raw.split_first() {
            Some((1, doc)) => Cell::Str(String::from_utf8_lossy(doc)),
            _ => return Err(Problem::Unparsable),
        },
        ColKind::Bytea => Cell::Bin(Cow::Borrowed(raw)),
        ColKind::Date => Cell::Date(pg_date(raw)?),
        ColKind::Time => Cell::Time(i64::from_be_bytes(be(raw)?)),
        ColKind::Timestamp | ColKind::TimestampTz => Cell::Timestamp(pg_timestamp(raw)?),
        ColKind::Interval => Cell::Str(Cow::Owned(interval_text(raw)?)),
        ColKind::Uuid => Cell::Uuid(be(raw)?),
        ColKind::Other => Cell::Str(Cow::Owned(match ty {
            Some(ty) => binary_text(ty, raw),
            None => fallback_text(raw),
        })),
    })
}

fn interval_text(raw: &[u8]) -> Parsed<String> {
    if raw.len() != 16 {
        return Err(Problem::Unparsable);
    }
    let micros = i64::from_be_bytes(be(&raw[..8])?);
    let days = i32::from_be_bytes(be(&raw[8..12])?);
    let months = i32::from_be_bytes(be(&raw[12..])?);
    Ok(format_interval(months, days, micros))
}

// ---------------------------------------------------------------------------------------------
// Binary -> text for types without a dedicated Arrow mapping.

/// The text form of a binary value of type `ty`, as close to the server's output as practical:
/// scalars, arrays, ranges, multiranges, composites, enums and domains of them. Unknown types
/// fall back to their UTF-8 text, or `\x` + hex when the bytes are not printable text.
pub(crate) fn binary_text(ty: &Type, raw: &[u8]) -> String {
    let structured = match ty.kind() {
        Kind::Domain(base) => return binary_text(base, raw),
        Kind::Enum(_) => return String::from_utf8_lossy(raw).into_owned(),
        Kind::Array(elem) => array_text(elem, raw),
        Kind::Range(elem) => range_text(elem, raw),
        Kind::Multirange(elem) => multirange_text(elem, raw),
        Kind::Composite(fields) => record_text(fields, raw),
        _ => scalar_text(ty, raw),
    };
    structured.unwrap_or_else(|| fallback_text(raw))
}

fn fallback_text(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s)
            if !s
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')) =>
        {
            s.to_owned()
        }
        _ => hex_text(raw),
    }
}

fn hex_text(raw: &[u8]) -> String {
    let mut s = String::with_capacity(2 + raw.len() * 2);
    s.push_str("\\x");
    for b in raw {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn float_text(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        (if v > 0.0 { "Infinity" } else { "-Infinity" }).into()
    } else {
        v.to_string()
    }
}

/// `+HH[:MM[:SS]]` for an offset in seconds east of UTC.
fn offset_text(east: i32) -> String {
    let sign = if east < 0 { '-' } else { '+' };
    let abs = east.unsigned_abs();
    let (h, m, s) = (abs / 3600, (abs / 60) % 60, abs % 60);
    match (m, s) {
        (0, 0) => format!("{sign}{h:02}"),
        (_, 0) => format!("{sign}{h:02}:{m:02}"),
        _ => format!("{sign}{h:02}:{m:02}:{s:02}"),
    }
}

fn scalar_text(ty: &Type, raw: &[u8]) -> Option<String> {
    let ok = |r: Parsed<String>| r.ok();
    Some(match *ty {
        Type::BOOL => (if *raw.first()? != 0 { "t" } else { "f" }).into(),
        Type::INT2 => i16::from_be_bytes(be(raw).ok()?).to_string(),
        Type::INT4 => i32::from_be_bytes(be(raw).ok()?).to_string(),
        Type::INT8 => i64::from_be_bytes(be(raw).ok()?).to_string(),
        Type::OID
        | Type::XID
        | Type::CID
        | Type::REGPROC
        | Type::REGPROCEDURE
        | Type::REGOPER
        | Type::REGOPERATOR
        | Type::REGCLASS
        | Type::REGTYPE
        | Type::REGCONFIG
        | Type::REGDICTIONARY
        | Type::REGNAMESPACE
        | Type::REGROLE
        | Type::REGCOLLATION => u32::from_be_bytes(be(raw).ok()?).to_string(),
        Type::XID8 => u64::from_be_bytes(be(raw).ok()?).to_string(),
        Type::FLOAT4 => float_text(f64::from(f32::from_be_bytes(be(raw).ok()?))),
        Type::FLOAT8 => float_text(f64::from_be_bytes(be(raw).ok()?)),
        Type::NUMERIC => ok(numeric_bin_to_text(raw))?,
        Type::TEXT
        | Type::VARCHAR
        | Type::BPCHAR
        | Type::NAME
        | Type::UNKNOWN
        | Type::XML
        | Type::JSON
        | Type::CHAR => String::from_utf8_lossy(raw).into_owned(),
        Type::JSONB => match raw.split_first()? {
            (1, doc) => String::from_utf8_lossy(doc).into_owned(),
            _ => return None,
        },
        Type::BYTEA => hex_text(raw),
        Type::DATE => match i32::from_be_bytes(be(raw).ok()?) {
            i32::MAX => "infinity".into(),
            i32::MIN => "-infinity".into(),
            d => format_date(i64::from(d) + PG_EPOCH_DAYS),
        },
        Type::TIME => format_time(i64::from_be_bytes(be(raw).ok()?)),
        Type::TIMETZ => {
            if raw.len() != 12 {
                return None;
            }
            let micros = i64::from_be_bytes(be(&raw[..8]).ok()?);
            // Stored as seconds *west* of UTC.
            let west = i32::from_be_bytes(be(&raw[8..]).ok()?);
            format!("{}{}", format_time(micros), offset_text(-west))
        }
        Type::TIMESTAMP | Type::TIMESTAMPTZ => {
            let utc = *ty == Type::TIMESTAMPTZ;
            match i64::from_be_bytes(be(raw).ok()?) {
                i64::MAX => "infinity".into(),
                i64::MIN => "-infinity".into(),
                t => format_timestamp(t.checked_add(PG_EPOCH_MICROS)?, utc),
            }
        }
        Type::INTERVAL => ok(interval_text(raw))?,
        Type::UUID => uuid::Uuid::from_slice(raw).ok()?.hyphenated().to_string(),
        Type::INET | Type::CIDR => inet_text(raw, *ty == Type::CIDR)?,
        Type::MACADDR | Type::MACADDR8 => {
            if raw.len() != 6 && raw.len() != 8 {
                return None;
            }
            raw.iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(":")
        }
        Type::MONEY => {
            let cents = i64::from_be_bytes(be(raw).ok()?);
            let sign = if cents < 0 { "-" } else { "" };
            let a = cents.unsigned_abs();
            format!("{sign}{}.{:02}", a / 100, a % 100)
        }
        Type::BIT | Type::VARBIT => {
            let mut r = Reader(raw);
            let bits = usize::try_from(r.i32()?).ok()?;
            let data = r.0;
            if data.len() * 8 < bits {
                return None;
            }
            (0..bits)
                .map(|i| {
                    if data[i / 8] & (0x80 >> (i % 8)) != 0 {
                        '1'
                    } else {
                        '0'
                    }
                })
                .collect()
        }
        Type::TID => {
            let mut r = Reader(raw);
            let block = r.u32()?;
            let offset = u16::from_be_bytes(be(r.take(2)?).ok()?);
            format!("({block},{offset})")
        }
        Type::POINT => {
            let x = f64::from_be_bytes(be(raw.get(..8)?).ok()?);
            let y = f64::from_be_bytes(be(raw.get(8..)?).ok()?);
            format!("({},{})", float_text(x), float_text(y))
        }
        _ => return None,
    })
}

fn inet_text(raw: &[u8], cidr: bool) -> Option<String> {
    let (&family, rest) = raw.split_first()?;
    let (&bits, rest) = rest.split_first()?;
    let (_is_cidr, rest) = rest.split_first()?;
    let (&len, addr) = rest.split_first()?;
    if addr.len() != usize::from(len) {
        return None;
    }
    let (text, max) = match family {
        2 => (
            Ipv4Addr::from(<[u8; 4]>::try_from(addr).ok()?).to_string(),
            32,
        ),
        3 => (
            Ipv6Addr::from(<[u8; 16]>::try_from(addr).ok()?).to_string(),
            128,
        ),
        _ => return None,
    };
    Some(if cidr || bits != max {
        format!("{text}/{bits}")
    } else {
        text
    })
}

/// Cursor over big-endian binary data.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn i32(&mut self) -> Option<i32> {
        self.take(4)
            .map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A length-prefixed value (`None` when the input is truncated).
    fn value(&mut self) -> Option<Field<'a>> {
        let len = self.i32()?;
        if len < 0 {
            return Some(Field::Null);
        }
        self.take(usize::try_from(len).ok()?).map(Field::Bytes)
    }
}

/// A length-prefixed value inside an array, range or record.
#[derive(Clone, Copy)]
enum Field<'a> {
    /// SQL NULL (length -1).
    Null,
    Bytes(&'a [u8]),
}

impl<'a> Field<'a> {
    fn bytes(self) -> Option<&'a [u8]> {
        match self {
            Self::Null => None,
            Self::Bytes(b) => Some(b),
        }
    }
}

/// Quotes an element / field in array, range or record text when it contains a special
/// character (`specials`), whitespace, is empty, or (arrays) spells NULL.
fn quote_into(out: &mut String, s: &str, specials: &[char], quote_null: bool) {
    let needs = s.is_empty()
        || (quote_null && s.eq_ignore_ascii_case("NULL"))
        || s.chars()
            .any(|c| c.is_whitespace() || specials.contains(&c));
    if !needs {
        out.push_str(s);
        return;
    }
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
}

fn array_text(elem: &Type, raw: &[u8]) -> Option<String> {
    let mut r = Reader(raw);
    let ndim = usize::try_from(r.i32()?).ok()?;
    let _has_nulls = r.i32()?;
    let _elem_oid = r.u32()?;
    if ndim == 0 {
        return Some("{}".into());
    }
    let mut dims = Vec::with_capacity(ndim);
    let mut lower = Vec::with_capacity(ndim);
    for _ in 0..ndim {
        dims.push(usize::try_from(r.i32()?).ok()?);
        lower.push(r.i32()?);
    }
    let total: usize = dims.iter().product();
    let mut items = Vec::with_capacity(total.min(1 << 16));
    for _ in 0..total {
        items.push(r.value()?.bytes().map(|v| binary_text(elem, v)));
    }
    let delim = if *elem == Type::BOX { ';' } else { ',' };
    let mut out = String::new();
    if lower.iter().any(|&l| l != 1) {
        for (len, lb) in dims.iter().zip(&lower) {
            let len = i64::try_from(*len).ok()?;
            let _ = write!(out, "[{lb}:{}]", i64::from(*lb) + len - 1);
        }
        out.push('=');
    }
    let mut it = items.into_iter();
    nest_array(&mut out, &dims, &mut it, delim);
    Some(out)
}

fn nest_array(
    out: &mut String,
    dims: &[usize],
    items: &mut impl Iterator<Item = Option<String>>,
    delim: char,
) {
    out.push('{');
    for i in 0..dims[0] {
        if i > 0 {
            out.push(delim);
        }
        if dims.len() > 1 {
            nest_array(out, &dims[1..], items, delim);
        } else {
            match items.next().flatten() {
                Some(s) => quote_into(out, &s, &['{', '}', '"', '\\', ',', ';'], true),
                None => out.push_str("NULL"),
            }
        }
    }
    out.push('}');
}

fn range_text(elem: &Type, raw: &[u8]) -> Option<String> {
    const EMPTY: u8 = 0x01;
    const LB_INC: u8 = 0x02;
    const UB_INC: u8 = 0x04;
    const LB_INF: u8 = 0x08;
    const UB_INF: u8 = 0x10;
    let mut r = Reader(raw);
    let flags = r.u8()?;
    if flags & EMPTY != 0 {
        return Some("empty".into());
    }
    let specials = ['"', '\\', '(', ')', '[', ']', ','];
    let mut out = String::new();
    out.push(if flags & LB_INC != 0 { '[' } else { '(' });
    if flags & LB_INF == 0 {
        let v = r.value()?.bytes()?;
        quote_into(&mut out, &binary_text(elem, v), &specials, false);
    }
    out.push(',');
    if flags & UB_INF == 0 {
        let v = r.value()?.bytes()?;
        quote_into(&mut out, &binary_text(elem, v), &specials, false);
    }
    out.push(if flags & UB_INC != 0 { ']' } else { ')' });
    Some(out)
}

fn multirange_text(elem: &Type, raw: &[u8]) -> Option<String> {
    let mut r = Reader(raw);
    let count = usize::try_from(r.i32()?).ok()?;
    let mut parts = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        parts.push(range_text(elem, r.value()?.bytes()?)?);
    }
    Some(format!("{{{}}}", parts.join(",")))
}

fn record_text(fields: &[PgField], raw: &[u8]) -> Option<String> {
    let mut r = Reader(raw);
    let count = usize::try_from(r.i32()?).ok()?;
    let mut out = String::from("(");
    for i in 0..count {
        if i > 0 {
            out.push(',');
        }
        let _oid = r.u32()?;
        if let Some(v) = r.value()?.bytes() {
            let text = match fields.get(i) {
                Some(f) => binary_text(f.type_(), v),
                None => fallback_text(v),
            };
            quote_into(&mut out, &text, &['"', '\\', '(', ')', ','], false);
        }
    }
    out.push(')');
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Batch building.

enum ColBuilder {
    Bool(BooleanBuilder),
    I16(Int16Builder),
    I32(Int32Builder),
    I64(Int64Builder),
    U32(UInt32Builder),
    F32(Float32Builder),
    F64(Float64Builder),
    Decimal(Decimal128Builder),
    Str(StringBuilder),
    Bin(BinaryBuilder),
    Date(Date32Builder),
    Time(Time64MicrosecondBuilder),
    Timestamp(TimestampMicrosecondBuilder),
    Uuid(FixedSizeBinaryBuilder),
}

impl ColBuilder {
    fn new(kind: ColKind) -> Result<Self> {
        Ok(match kind {
            ColKind::Bool => Self::Bool(BooleanBuilder::new()),
            ColKind::Int2 => Self::I16(Int16Builder::new()),
            ColKind::Int4 => Self::I32(Int32Builder::new()),
            ColKind::Int8 => Self::I64(Int64Builder::new()),
            ColKind::Oid => Self::U32(UInt32Builder::new()),
            ColKind::Float4 => Self::F32(Float32Builder::new()),
            ColKind::Float8 => Self::F64(Float64Builder::new()),
            ColKind::Decimal { precision, scale } => Self::Decimal(
                Decimal128Builder::new()
                    .with_precision_and_scale(precision, scale)
                    .map_err(from_arrow)?,
            ),
            ColKind::Bytea => Self::Bin(BinaryBuilder::new()),
            ColKind::Date => Self::Date(Date32Builder::new()),
            ColKind::Time => Self::Time(Time64MicrosecondBuilder::new()),
            ColKind::Timestamp => Self::Timestamp(TimestampMicrosecondBuilder::new()),
            ColKind::TimestampTz => {
                Self::Timestamp(TimestampMicrosecondBuilder::new().with_timezone("UTC"))
            }
            ColKind::Uuid => Self::Uuid(FixedSizeBinaryBuilder::new(16)),
            ColKind::Numeric
            | ColKind::Text
            | ColKind::Json
            | ColKind::Jsonb
            | ColKind::Interval
            | ColKind::Other => Self::Str(StringBuilder::new()),
        })
    }

    /// Appends `cell`; a cell that does not match the builder (a bug) is stored as NULL.
    fn push(&mut self, cell: Cell<'_>) {
        match (self, cell) {
            (Self::Bool(b), Cell::Bool(v)) => b.append_value(v),
            (Self::I16(b), Cell::I16(v)) => b.append_value(v),
            (Self::I32(b), Cell::I32(v)) => b.append_value(v),
            (Self::I64(b), Cell::I64(v)) => b.append_value(v),
            (Self::U32(b), Cell::U32(v)) => b.append_value(v),
            (Self::F32(b), Cell::F32(v)) => b.append_value(v),
            (Self::F64(b), Cell::F64(v)) => b.append_value(v),
            (Self::Decimal(b), Cell::Decimal(v)) => b.append_value(v),
            (Self::Str(b), Cell::Str(v)) => b.append_value(v),
            (Self::Bin(b), Cell::Bin(v)) => b.append_value(v),
            (Self::Date(b), Cell::Date(v)) => b.append_value(v),
            (Self::Time(b), Cell::Time(v)) => b.append_value(v),
            (Self::Timestamp(b), Cell::Timestamp(v)) => b.append_value(v),
            (Self::Uuid(b), Cell::Uuid(v)) => {
                // Cannot fail: the builder width is 16.
                if b.append_value(v).is_err() {
                    b.append_null();
                }
            }
            (this, _) => this.push_null(),
        }
    }

    fn push_null(&mut self) {
        match self {
            Self::Bool(b) => b.append_null(),
            Self::I16(b) => b.append_null(),
            Self::I32(b) => b.append_null(),
            Self::I64(b) => b.append_null(),
            Self::U32(b) => b.append_null(),
            Self::F32(b) => b.append_null(),
            Self::F64(b) => b.append_null(),
            Self::Decimal(b) => b.append_null(),
            Self::Str(b) => b.append_null(),
            Self::Bin(b) => b.append_null(),
            Self::Date(b) => b.append_null(),
            Self::Time(b) => b.append_null(),
            Self::Timestamp(b) => b.append_null(),
            Self::Uuid(b) => b.append_null(),
        }
    }

    fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Bool(b) => Arc::new(b.finish()),
            Self::I16(b) => Arc::new(b.finish()),
            Self::I32(b) => Arc::new(b.finish()),
            Self::I64(b) => Arc::new(b.finish()),
            Self::U32(b) => Arc::new(b.finish()),
            Self::F32(b) => Arc::new(b.finish()),
            Self::F64(b) => Arc::new(b.finish()),
            Self::Decimal(b) => Arc::new(b.finish()),
            Self::Str(b) => Arc::new(b.finish()),
            Self::Bin(b) => Arc::new(b.finish()),
            Self::Date(b) => Arc::new(b.finish()),
            Self::Time(b) => Arc::new(b.finish()),
            Self::Timestamp(b) => Arc::new(b.finish()),
            Self::Uuid(b) => Arc::new(b.finish()),
        }
    }
}

struct Col {
    spec: ColSpec,
    builder: ColBuilder,
    problem: Option<Problem>,
    warned: bool,
}

/// Accumulates rows of one result set and cuts them into batches.
pub(crate) struct BatchBuilder {
    schema: SchemaRef,
    cols: Vec<Col>,
    rows: usize,
    bytes: usize,
}

impl BatchBuilder {
    pub(crate) fn new(specs: Vec<ColSpec>) -> Result<Self> {
        let schema = Arc::new(Schema::new(
            specs.iter().map(ColSpec::arrow_field).collect::<Vec<_>>(),
        ));
        let cols = specs
            .into_iter()
            .map(|spec| {
                Ok(Col {
                    builder: ColBuilder::new(spec.kind)?,
                    spec,
                    problem: None,
                    warned: false,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            schema,
            cols,
            rows: 0,
            bytes: 0,
        })
    }

    pub(crate) fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    pub(crate) fn num_columns(&self) -> usize {
        self.cols.len()
    }

    fn push(&mut self, idx: usize, value: Option<Parsed<Cell<'_>>>, len: usize) {
        let Some(col) = self.cols.get_mut(idx) else {
            return;
        };
        self.bytes += len;
        match value {
            None => col.builder.push_null(),
            Some(Ok(cell)) => col.builder.push(cell),
            Some(Err(problem)) => {
                col.problem.get_or_insert(problem);
                col.builder.push_null();
            }
        }
    }

    /// Appends column `idx` of the current row from its text form (`None` is NULL).
    pub(crate) fn append_text(&mut self, idx: usize, value: Option<&str>) {
        let Some(kind) = self.cols.get(idx).map(|c| c.spec.kind) else {
            return;
        };
        let len = value.map_or(0, str::len);
        self.push(idx, value.map(|s| decode_text(kind, s)), len);
    }

    /// Appends column `idx` of the current row from its binary form (`None` is NULL).
    pub(crate) fn append_binary(&mut self, idx: usize, value: Option<&[u8]>) {
        let Some(col) = self.cols.get(idx) else {
            return;
        };
        let kind = col.spec.kind;
        let ty = col.spec.ty.clone();
        let len = value.map_or(0, <[u8]>::len);
        self.push(
            idx,
            value.map(|raw| decode_binary(kind, ty.as_ref(), raw)),
            len,
        );
    }

    /// Marks the end of a row.
    pub(crate) fn end_row(&mut self) {
        self.rows += 1;
    }

    /// `true` once the pending rows should be sent as a batch.
    pub(crate) fn is_full(&self) -> bool {
        self.rows >= BATCH_ROWS || self.bytes >= BATCH_BYTES
    }

    /// The pending rows as a batch (`None` when there are none); the builder starts over.
    pub(crate) fn finish(&mut self) -> Result<Option<RecordBatch>> {
        if self.rows == 0 {
            return Ok(None);
        }
        let arrays: Vec<ArrayRef> = self.cols.iter_mut().map(|c| c.builder.finish()).collect();
        let options = RecordBatchOptions::new().with_row_count(Some(self.rows));
        let batch = RecordBatch::try_new_with_options(self.schema.clone(), arrays, &options)
            .map_err(from_arrow)?;
        self.rows = 0;
        self.bytes = 0;
        Ok(Some(batch))
    }

    /// One warning per column that had unrepresentable values since the last call (and never
    /// before in this result set). Messages name the column and type, never the value.
    pub(crate) fn take_warnings(&mut self) -> Vec<Notice> {
        let mut out = Vec::new();
        for col in &mut self.cols {
            if let Some(problem) = col.problem.take()
                && !col.warned
            {
                col.warned = true;
                out.push(Notice::warning(format!(
                    "column \"{}\" ({}): {}",
                    col.spec.name,
                    col.spec.type_name(),
                    problem.describe()
                )));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Array;
    use arrow_array::cast::AsArray;
    use arrow_array::types::{
        Date32Type, Decimal128Type, Int32Type, Int64Type, Time64MicrosecondType,
        TimestampMicrosecondType,
    };

    fn typmod(p: i32, s: i32) -> i32 {
        ((p << 16) | (s & 0x7ff)) + 4
    }

    #[test]
    fn text_values_become_typed_arrays() {
        let specs = vec![
            ColSpec::typed("i", &Type::INT4, -1),
            ColSpec::typed("d", &Type::NUMERIC, typmod(10, 2)),
            ColSpec::typed("ts", &Type::TIMESTAMPTZ, -1),
            ColSpec::typed("day", &Type::DATE, -1),
            ColSpec::typed("t", &Type::TIME, -1),
            ColSpec::typed("b", &Type::BYTEA, -1),
            ColSpec::typed("u", &Type::UUID, -1),
            ColSpec::typed("arr", &Type::INT4_ARRAY, -1),
        ];
        let mut b = BatchBuilder::new(specs).unwrap();
        let row1 = [
            Some("42"),
            Some("123.45"),
            Some("2024-01-15 10:20:30.5+02"),
            Some("2024-01-15"),
            Some("12:00:00"),
            Some("\\x0102"),
            Some("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"),
            Some("{1,2,NULL}"),
        ];
        for (i, v) in row1.iter().enumerate() {
            b.append_text(i, *v);
        }
        b.end_row();
        for i in 0..8 {
            b.append_text(i, None);
        }
        b.end_row();
        let batch = b.finish().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.column(0).as_primitive::<Int32Type>().value(0), 42);
        assert!(batch.column(0).is_null(1));
        assert_eq!(
            batch.column(1).as_primitive::<Decimal128Type>().value(0),
            12_345
        );
        assert_eq!(
            batch
                .column(2)
                .as_primitive::<TimestampMicrosecondType>()
                .value(0),
            1_705_306_830_500_000
        );
        assert_eq!(
            batch.column(3).as_primitive::<Date32Type>().value(0),
            19_737
        );
        assert_eq!(
            batch
                .column(4)
                .as_primitive::<Time64MicrosecondType>()
                .value(0),
            43_200_000_000
        );
        assert_eq!(batch.column(5).as_binary::<i32>().value(0), [1, 2]);
        assert_eq!(batch.column(6).as_fixed_size_binary().value(0)[0], 0xa0);
        assert_eq!(batch.column(7).as_string::<i32>().value(0), "{1,2,NULL}");
        let warnings = b.take_warnings();
        assert_eq!(warnings.len(), 0, "{warnings:?}");
        assert!(b.finish().unwrap().is_none(), "builder starts over");
    }

    #[test]
    fn unrepresentable_values_are_null_with_one_warning() {
        let mut b = BatchBuilder::new(vec![
            ColSpec::typed("when", &Type::TIMESTAMP, -1),
            ColSpec::typed("amount", &Type::NUMERIC, typmod(6, 2)),
        ])
        .unwrap();
        for (ts, amount) in [
            ("infinity", "NaN"),
            ("-infinity", "1.00"),
            ("2000-01-01 00:00:00", "NaN"),
        ] {
            b.append_text(0, Some(ts));
            b.append_text(1, Some(amount));
            b.end_row();
        }
        let batch = b.finish().unwrap().unwrap();
        assert_eq!(batch.column(0).null_count(), 2);
        assert_eq!(batch.column(1).null_count(), 2);
        let warnings = b.take_warnings();
        assert_eq!(warnings.len(), 2);
        assert!(
            warnings[0].message.contains("\"when\" (timestamp)"),
            "{}",
            warnings[0]
        );
        assert!(warnings[1].message.contains("NaN"), "{}", warnings[1]);
        b.append_text(0, Some("infinity"));
        b.append_text(1, Some("NaN"));
        b.end_row();
        let again = b.take_warnings();
        assert_eq!(again.len(), 0, "one warning per column: {again:?}");
    }

    #[test]
    fn batches_are_cut_by_rows() {
        let mut b = BatchBuilder::new(vec![ColSpec::typed("n", &Type::INT8, -1)]).unwrap();
        let mut batches = Vec::new();
        for i in 0..2500 {
            b.append_text(0, Some(&i.to_string()));
            b.end_row();
            if b.is_full() {
                batches.push(b.finish().unwrap().unwrap());
            }
        }
        batches.extend(b.finish().unwrap());
        let sizes: Vec<_> = batches.iter().map(RecordBatch::num_rows).collect();
        assert_eq!(sizes, [1024, 1024, 452]);
        assert_eq!(
            batches[2].column(0).as_primitive::<Int64Type>().value(451),
            2499
        );
    }

    #[test]
    fn zero_column_rows_keep_their_count() {
        let mut b = BatchBuilder::new(Vec::new()).unwrap();
        b.end_row();
        b.end_row();
        assert_eq!(b.finish().unwrap().unwrap().num_rows(), 2);
    }

    #[test]
    fn binary_values() {
        let pg = |kind, raw: &[u8]| decode_binary(kind, None, raw).map(|c| format!("{c:?}"));
        assert_eq!(
            decode_binary(ColKind::Int4, None, &7i32.to_be_bytes()),
            Ok(Cell::I32(7))
        );
        assert_eq!(
            decode_binary(ColKind::Bool, None, &[1]),
            Ok(Cell::Bool(true))
        );
        // 2000-01-02 is day 1 of the PostgreSQL epoch.
        assert_eq!(
            decode_binary(ColKind::Date, None, &1i32.to_be_bytes()),
            Ok(Cell::Date(10_958))
        );
        assert_eq!(
            decode_binary(ColKind::Date, None, &i32::MAX.to_be_bytes()),
            Err(Problem::Infinite)
        );
        assert_eq!(
            decode_binary(ColKind::TimestampTz, None, &0i64.to_be_bytes()),
            Ok(Cell::Timestamp(PG_EPOCH_MICROS))
        );
        let numeric = crate::values::numeric_text_to_bin("123.45").unwrap();
        assert_eq!(
            decode_binary(
                ColKind::Decimal {
                    precision: 10,
                    scale: 2
                },
                None,
                &numeric
            ),
            Ok(Cell::Decimal(12_345))
        );
        assert_eq!(
            decode_binary(ColKind::Numeric, None, &numeric),
            Ok(Cell::Str("123.45".into()))
        );
        assert_eq!(
            decode_binary(ColKind::Jsonb, None, b"\x01{\"a\":1}"),
            Ok(Cell::Str("{\"a\":1}".into()))
        );
        let mut interval = Vec::new();
        interval.extend_from_slice(&3_600_000_000i64.to_be_bytes());
        interval.extend_from_slice(&2i32.to_be_bytes());
        interval.extend_from_slice(&13i32.to_be_bytes());
        assert_eq!(
            decode_binary(ColKind::Interval, None, &interval),
            Ok(Cell::Str("1 year 1 mon 2 days 01:00:00".into()))
        );
        assert!(pg(ColKind::Int8, &[0, 1]).is_err());
    }

    fn array_bin(elem_oid: u32, dims: &[(i32, i32)], elems: &[Option<&[u8]>]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&i32::try_from(dims.len()).unwrap().to_be_bytes());
        v.extend_from_slice(&1i32.to_be_bytes());
        v.extend_from_slice(&elem_oid.to_be_bytes());
        for (len, lb) in dims {
            v.extend_from_slice(&len.to_be_bytes());
            v.extend_from_slice(&lb.to_be_bytes());
        }
        for e in elems {
            match e {
                Some(b) => {
                    v.extend_from_slice(&i32::try_from(b.len()).unwrap().to_be_bytes());
                    v.extend_from_slice(b);
                }
                None => v.extend_from_slice(&(-1i32).to_be_bytes()),
            }
        }
        v
    }

    #[test]
    fn binary_arrays_ranges_and_records_as_text() {
        let one = 1i32.to_be_bytes();
        let two = 2i32.to_be_bytes();
        let raw = array_bin(23, &[(3, 1)], &[Some(&one), None, Some(&two)]);
        assert_eq!(binary_text(&Type::INT4_ARRAY, &raw), "{1,NULL,2}");
        let raw = array_bin(
            23,
            &[(2, 1), (2, 1)],
            &[Some(&one), Some(&two), Some(&two), Some(&one)],
        );
        assert_eq!(binary_text(&Type::INT4_ARRAY, &raw), "{{1,2},{2,1}}");
        let raw = array_bin(23, &[(1, 0)], &[Some(&one)]);
        assert_eq!(binary_text(&Type::INT4_ARRAY, &raw), "[0:0]={1}");
        let raw = array_bin(25, &[(3, 1)], &[Some(b"a b"), Some(b""), Some(b"null")]);
        assert_eq!(binary_text(&Type::TEXT_ARRAY, &raw), r#"{"a b","","null"}"#);
        assert_eq!(
            binary_text(&Type::INT4_ARRAY, &array_bin(23, &[], &[])),
            "{}"
        );

        let mut range = vec![0x02];
        for b in [one, two] {
            range.extend_from_slice(&4i32.to_be_bytes());
            range.extend_from_slice(&b);
        }
        assert_eq!(binary_text(&Type::INT4_RANGE, &range), "[1,2)");
        assert_eq!(binary_text(&Type::INT4_RANGE, &[0x01]), "empty");
        assert_eq!(binary_text(&Type::INT4_RANGE, &[0x08 | 0x10]), "(,)");

        let fields = vec![
            PgField::new("a".into(), Type::INT4),
            PgField::new("b".into(), Type::TEXT),
        ];
        let composite = Type::new(
            "pair".into(),
            90_002,
            Kind::Composite(fields),
            "public".into(),
        );
        let mut rec = Vec::new();
        rec.extend_from_slice(&2i32.to_be_bytes());
        rec.extend_from_slice(&23u32.to_be_bytes());
        rec.extend_from_slice(&4i32.to_be_bytes());
        rec.extend_from_slice(&one);
        rec.extend_from_slice(&25u32.to_be_bytes());
        rec.extend_from_slice(&3i32.to_be_bytes());
        rec.extend_from_slice(b"x,y");
        assert_eq!(binary_text(&composite, &rec), r#"(1,"x,y")"#);

        let mood = Type::new(
            "mood".into(),
            90_003,
            Kind::Enum(vec!["ok".into()]),
            "public".into(),
        );
        assert_eq!(binary_text(&mood, b"ok"), "ok");
    }

    #[test]
    fn binary_scalars_as_text() {
        assert_eq!(
            binary_text(&Type::INET, &[2, 32, 0, 4, 10, 0, 0, 1]),
            "10.0.0.1"
        );
        assert_eq!(
            binary_text(&Type::CIDR, &[2, 8, 1, 4, 10, 0, 0, 0]),
            "10.0.0.0/8"
        );
        assert_eq!(
            binary_text(&Type::MONEY, &(-1234i64).to_be_bytes()),
            "-12.34"
        );
        assert_eq!(binary_text(&Type::OID, &1259u32.to_be_bytes()), "1259");
        assert_eq!(binary_text(&Type::BYTEA, &[0xde, 0xad]), "\\xdead");
        let mut timetz = 3_600_000_000i64.to_be_bytes().to_vec();
        timetz.extend_from_slice(&(-19_800i32).to_be_bytes());
        assert_eq!(binary_text(&Type::TIMETZ, &timetz), "01:00:00+05:30");
        let mut bits = 3i32.to_be_bytes().to_vec();
        bits.push(0b1010_0000);
        assert_eq!(binary_text(&Type::VARBIT, &bits), "101");
        let unknown = Type::new("blob".into(), 90_004, Kind::Simple, "ext".into());
        assert_eq!(binary_text(&unknown, b"plain"), "plain");
        assert_eq!(binary_text(&unknown, &[0, 1]), "\\x0001");
    }
}
