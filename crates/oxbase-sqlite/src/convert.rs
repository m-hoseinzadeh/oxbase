//! Values in and out of SQLite: statement parameters, the column type mapping and Arrow batches.
//!
//! The type mapping rules are documented in the crate docs (`# Type mapping`); [`infer`]
//! implements them.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use arrow_array::builder::{BinaryBuilder, Float64Builder, Int64Builder, StringBuilder};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use oxbase_driver::from_arrow;
use oxbase_types::{Result, Value};
use rusqlite::types::{Value as SqlValue, ValueRef};

/// Field metadata key holding the column's declared SQLite type (as written in `CREATE TABLE`).
pub const DECL_TYPE_METADATA_KEY: &str = "sqlite.decl_type";

/// Largest integer magnitude a `Float64` holds exactly (2^53).
const F64_EXACT_INT: i64 = 1 << 53;

/// Converts a parameter into an owned SQLite value.
///
/// Booleans bind as `0` / `1`. Unsigned values above `i64::MAX` bind as reals (what SQLite does
/// with such literals). Decimals bind as text, so a `NUMERIC` column converts them exactly
/// where it can. Dates and times bind as the ISO-8601 text SQLite's date functions use
/// (`YYYY-MM-DD`, `HH:MM:SS.SSS`, `YYYY-MM-DD HH:MM:SS.SSS`, plus `±HH:MM` with an offset).
/// UUIDs bind as hyphenated text and JSON as its text.
pub(crate) fn to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Value::Int(i) => SqlValue::Integer(*i),
        Value::UInt(u) => {
            if let Ok(i) = i64::try_from(*u) {
                SqlValue::Integer(i)
            } else {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "SQLite stores integers beyond i64 as reals itself"
                )]
                let f = *u as f64;
                SqlValue::Real(f)
            }
        }
        Value::Float(f) => SqlValue::Real(*f),
        Value::Decimal(s) | Value::Text(s) => SqlValue::Text(s.clone()),
        Value::Bytes(b) => SqlValue::Blob(b.clone()),
        Value::Date(d) => SqlValue::Text(d.format("%Y-%m-%d").to_string()),
        Value::Time(t) => SqlValue::Text(t.format("%H:%M:%S%.f").to_string()),
        Value::Timestamp(t) => SqlValue::Text(t.format("%Y-%m-%d %H:%M:%S%.f").to_string()),
        Value::TimestampTz(t) => SqlValue::Text(t.format("%Y-%m-%d %H:%M:%S%.f%:z").to_string()),
        Value::Uuid(u) => SqlValue::Text(u.hyphenated().to_string()),
        Value::Json(j) => SqlValue::Text(j.to_string()),
    }
}

/// One value read from a row (owned, so rows can be buffered until a batch is built).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Cell {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Cell {
    pub(crate) fn from_ref(v: ValueRef<'_>) -> Self {
        match v {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(i) => Self::Int(i),
            ValueRef::Real(f) => Self::Real(f),
            // SQLite does not enforce UTF-8 in TEXT; invalid sequences become U+FFFD.
            ValueRef::Text(t) => Self::Text(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Blob(b) => Self::Blob(b.to_vec()),
        }
    }

    /// Rough payload size in bytes, for batch sizing.
    pub(crate) fn size(&self) -> usize {
        match self {
            Self::Null | Self::Int(_) | Self::Real(_) => 8,
            Self::Text(s) => s.len() + 4,
            Self::Blob(b) => b.len() + 4,
        }
    }
}

/// Column affinity from a declared type, following SQLite's rules (section 3.1 of the
/// "Datatypes In SQLite" document). `None` means no declared type at all (an expression).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Affinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
    None,
}

pub(crate) fn affinity(decl: Option<&str>) -> Affinity {
    let Some(decl) = decl.map(str::trim).filter(|d| !d.is_empty()) else {
        return Affinity::None;
    };
    let d = decl.to_ascii_uppercase();
    if d.contains("INT") {
        Affinity::Integer
    } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
        Affinity::Text
    } else if d.contains("BLOB") {
        Affinity::Blob
    } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}

/// The Arrow type chosen for one result column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColType {
    Int64,
    Float64,
    Utf8,
    Binary,
}

impl ColType {
    pub(crate) fn arrow(self) -> DataType {
        match self {
            Self::Int64 => DataType::Int64,
            Self::Float64 => DataType::Float64,
            Self::Utf8 => DataType::Utf8,
            Self::Binary => DataType::Binary,
        }
    }
}

/// Picks the Arrow type of a column from its declared type and a sample of its values (the
/// first batch). See the module docs for the rules.
pub(crate) fn infer(decl: Option<&str>, sample: &[Cell]) -> ColType {
    let aff = affinity(decl);
    let (mut int, mut wide, mut real, mut text, mut blob) = (false, false, false, false, false);
    for cell in sample {
        match cell {
            Cell::Null => {}
            Cell::Int(i) => {
                int = true;
                wide |= !(-F64_EXACT_INT..=F64_EXACT_INT).contains(i);
            }
            Cell::Real(_) => real = true,
            Cell::Text(_) => text = true,
            Cell::Blob(_) => blob = true,
        }
    }
    if !(int || real || text || blob) {
        return from_declared(aff, decl);
    }
    if text || blob {
        // Text only, or any mix with text or blobs, is Utf8; blobs only stay binary.
        return if blob && !(int || real || text) {
            ColType::Binary
        } else {
            ColType::Utf8
        };
    }
    if real {
        if wide {
            ColType::Utf8
        } else {
            ColType::Float64
        }
    } else if aff == Affinity::Real && !wide {
        ColType::Float64
    } else if aff == Affinity::Numeric && !is_bool(decl) {
        // NUMERIC affinity stores whole values (10.00) as integers and others (9.75) as reals,
        // so a sample of whole values says nothing about later rows: Int64 would turn the first
        // fractional value into NULL.
        if wide {
            ColType::Utf8
        } else {
            ColType::Float64
        }
    } else {
        ColType::Int64
    }
}

fn is_bool(decl: Option<&str>) -> bool {
    decl.is_some_and(|d| d.to_ascii_uppercase().contains("BOOL"))
}

/// The type of a column whose sample holds no values: its affinity decides.
fn from_declared(aff: Affinity, decl: Option<&str>) -> ColType {
    match aff {
        Affinity::Integer => ColType::Int64,
        Affinity::Real => ColType::Float64,
        Affinity::Text | Affinity::None => ColType::Utf8,
        Affinity::Blob => ColType::Binary,
        Affinity::Numeric => {
            let d = decl.unwrap_or_default().to_ascii_uppercase();
            if is_bool(decl) {
                ColType::Int64
            } else if d.contains("DATE") || d.contains("TIME") {
                ColType::Utf8
            } else {
                ColType::Float64
            }
        }
    }
}

/// Builds the schema of a result set.
pub(crate) fn schema(names: &[String], decls: &[Option<String>], types: &[ColType]) -> SchemaRef {
    let fields: Vec<Field> = names
        .iter()
        .zip(decls)
        .zip(types)
        .map(|((name, decl), ty)| {
            let field = Field::new(name.as_str(), ty.arrow(), true);
            match decl {
                Some(d) => field.with_metadata(HashMap::from([(
                    DECL_TYPE_METADATA_KEY.to_owned(),
                    d.clone(),
                )])),
                None => field,
            }
        })
        .collect();
    Arc::new(Schema::new(fields))
}

/// Builds one batch from column-major cells, converting each value to its column's type.
/// Values that do not fit become `NULL` and are counted in `mismatches` (one counter per column).
pub(crate) fn build_batch(
    schema: &SchemaRef,
    types: &[ColType],
    columns: Vec<Vec<Cell>>,
    mismatches: &mut [u64],
) -> Result<RecordBatch> {
    let arrays: Vec<ArrayRef> = columns
        .into_iter()
        .zip(types)
        .zip(mismatches.iter_mut())
        .map(|((cells, ty), bad)| build_array(*ty, cells, bad))
        .collect();
    RecordBatch::try_new(schema.clone(), arrays).map_err(from_arrow)
}

fn build_array(ty: ColType, cells: Vec<Cell>, bad: &mut u64) -> ArrayRef {
    let n = cells.len();
    match ty {
        ColType::Int64 => {
            let mut b = Int64Builder::with_capacity(n);
            for c in cells {
                match c {
                    Cell::Null => b.append_null(),
                    other => {
                        let v = as_i64(&other);
                        *bad += u64::from(v.is_none());
                        b.append_option(v);
                    }
                }
            }
            Arc::new(b.finish())
        }
        ColType::Float64 => {
            let mut b = Float64Builder::with_capacity(n);
            for c in cells {
                match c {
                    Cell::Null => b.append_null(),
                    other => {
                        let v = as_f64(&other);
                        *bad += u64::from(v.is_none());
                        b.append_option(v);
                    }
                }
            }
            Arc::new(b.finish())
        }
        ColType::Utf8 => {
            let bytes = cells.iter().map(Cell::size).sum();
            let mut b = StringBuilder::with_capacity(n, bytes);
            let mut buf = String::new();
            for c in cells {
                match c {
                    Cell::Null => b.append_null(),
                    Cell::Text(s) => b.append_value(s),
                    other => {
                        buf.clear();
                        render(&other, &mut buf);
                        b.append_value(&buf);
                    }
                }
            }
            Arc::new(b.finish())
        }
        ColType::Binary => {
            let bytes = cells.iter().map(Cell::size).sum();
            let mut b = BinaryBuilder::with_capacity(n, bytes);
            for c in cells {
                match c {
                    Cell::Null => b.append_null(),
                    Cell::Blob(v) => b.append_value(v),
                    Cell::Text(s) => b.append_value(s.as_bytes()),
                    Cell::Int(_) | Cell::Real(_) => {
                        *bad += 1;
                        b.append_null();
                    }
                }
            }
            Arc::new(b.finish())
        }
    }
}

/// Exact integer view of a value, if there is one.
#[allow(clippy::float_cmp, reason = "exact integrality test")]
fn as_i64(c: &Cell) -> Option<i64> {
    // i64::MIN as f64 is exactly -2^63; i64::MAX as f64 rounds up to 2^63, so use `<`.
    const LOW: f64 = -9_223_372_036_854_775_808.0;
    const HIGH: f64 = 9_223_372_036_854_775_808.0;
    match c {
        Cell::Int(i) => Some(*i),
        Cell::Real(f) if f.fract() == 0.0 && *f >= LOW && *f < HIGH => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "integral and range-checked just above"
            )]
            let i = *f as i64;
            Some(i)
        }
        Cell::Text(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Numeric view of a value, if there is one.
fn as_f64(c: &Cell) -> Option<f64> {
    match c {
        Cell::Int(i) => {
            #[allow(
                clippy::cast_precision_loss,
                reason = "the column is Float64; integers beyond 2^53 round as in SQLite arithmetic"
            )]
            let f = *i as f64;
            Some(f)
        }
        Cell::Real(f) => Some(*f),
        Cell::Text(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        Cell::Null | Cell::Blob(_) => None,
    }
}

/// Text form of a value in a `Utf8` column: integers in decimal, reals in their shortest exact
/// form (always with a `.` or exponent, like `1.0`), blobs as `0x` + lowercase hex.
fn render(c: &Cell, out: &mut String) {
    match c {
        Cell::Null => {}
        Cell::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Cell::Real(f) => {
            let _ = write!(out, "{f:?}");
        }
        Cell::Text(s) => out.push_str(s),
        Cell::Blob(b) => {
            out.reserve(2 + b.len() * 2);
            out.push_str("0x");
            for byte in b {
                let _ = write!(out, "{byte:02x}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Array, BinaryArray, Float64Array, Int64Array, StringArray};

    fn t(s: &str) -> Cell {
        Cell::Text(s.into())
    }

    #[test]
    fn affinity_rules() {
        assert_eq!(affinity(Some("INTEGER")), Affinity::Integer);
        assert_eq!(affinity(Some("unsigned big int")), Affinity::Integer);
        // "POINT" contains "INT": SQLite's rule, quirk included.
        assert_eq!(affinity(Some("FLOATING POINT")), Affinity::Integer);
        assert_eq!(affinity(Some("VARCHAR(20)")), Affinity::Text);
        assert_eq!(affinity(Some("clob")), Affinity::Text);
        assert_eq!(affinity(Some("BLOB")), Affinity::Blob);
        assert_eq!(affinity(Some("DOUBLE PRECISION")), Affinity::Real);
        assert_eq!(affinity(Some("DECIMAL(10,5)")), Affinity::Numeric);
        assert_eq!(affinity(Some("BOOLEAN")), Affinity::Numeric);
        assert_eq!(affinity(Some("")), Affinity::None);
        assert_eq!(affinity(None), Affinity::None);
    }

    #[test]
    fn inference_from_values() {
        use ColType::*;
        let int = Some("INTEGER");
        assert_eq!(infer(int, &[Cell::Int(1), Cell::Null]), Int64);
        assert_eq!(infer(None, &[Cell::Int(1)]), Int64);
        assert_eq!(infer(Some("REAL"), &[Cell::Int(1)]), Float64);
        assert_eq!(infer(Some("REAL"), &[Cell::Int(i64::MAX)]), Int64);
        assert_eq!(infer(None, &[Cell::Int(1), Cell::Real(1.5)]), Float64);
        assert_eq!(infer(None, &[Cell::Int(i64::MAX), Cell::Real(1.5)]), Utf8);
        assert_eq!(infer(int, &[t("a")]), Utf8);
        assert_eq!(infer(None, &[Cell::Blob(vec![1])]), Binary);
        assert_eq!(infer(None, &[Cell::Blob(vec![1]), t("a")]), Utf8);
        assert_eq!(infer(None, &[Cell::Int(1), t("a")]), Utf8);
        // NUMERIC affinity: whole values may be followed by fractional ones.
        assert_eq!(infer(Some("DECIMAL(10,2)"), &[Cell::Int(10)]), Float64);
        assert_eq!(infer(Some("NUMERIC"), &[Cell::Int(i64::MAX)]), Utf8);
        assert_eq!(infer(Some("BOOLEAN"), &[Cell::Int(1)]), Int64);
    }

    #[test]
    fn inference_from_declared_type() {
        use ColType::*;
        assert_eq!(infer(Some("BIGINT"), &[]), Int64);
        assert_eq!(infer(Some("FLOAT"), &[Cell::Null]), Float64);
        assert_eq!(infer(Some("NVARCHAR(5)"), &[]), Utf8);
        assert_eq!(infer(Some("BLOB"), &[]), Binary);
        assert_eq!(infer(Some("BOOLEAN"), &[]), Int64);
        assert_eq!(infer(Some("DATETIME"), &[]), Utf8);
        assert_eq!(infer(Some("NUMERIC(10,2)"), &[]), Float64);
        assert_eq!(infer(None, &[]), Utf8);
    }

    #[test]
    fn batch_conversion_and_mismatches() {
        let types = [
            ColType::Int64,
            ColType::Float64,
            ColType::Utf8,
            ColType::Binary,
        ];
        let names: Vec<String> = ["i", "f", "s", "b"].map(String::from).into();
        let decls = vec![Some("INTEGER".to_owned()), None, None, None];
        let schema = schema(&names, &decls, &types);
        assert_eq!(
            schema.field(0).metadata().get(DECL_TYPE_METADATA_KEY),
            Some(&"INTEGER".to_owned())
        );
        assert!(schema.field(1).metadata().is_empty());
        let cols = vec![
            vec![
                Cell::Int(1),
                Cell::Real(2.0),
                t(" 3 "),
                Cell::Real(2.5),
                Cell::Null,
            ],
            vec![Cell::Int(1), Cell::Real(2.5), t("3e2"), t("x"), Cell::Null],
            vec![
                Cell::Int(-7),
                Cell::Real(1.0),
                t("hi"),
                Cell::Blob(vec![0, 0xff]),
                Cell::Null,
            ],
            vec![
                Cell::Blob(vec![1]),
                t("ab"),
                Cell::Int(1),
                Cell::Null,
                Cell::Null,
            ],
        ];
        let mut bad = [0; 4];
        let batch = build_batch(&schema, &types, cols, &mut bad).unwrap();
        assert_eq!(bad, [1, 1, 0, 1]);
        let i = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(
            i.iter().collect::<Vec<_>>(),
            [Some(1), Some(2), Some(3), None, None]
        );
        let f = batch
            .column(1)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert_eq!(
            f.iter().collect::<Vec<_>>(),
            [Some(1.0), Some(2.5), Some(300.0), None, None]
        );
        let s = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(
            s.iter().collect::<Vec<_>>(),
            [Some("-7"), Some("1.0"), Some("hi"), Some("0x00ff"), None]
        );
        let b = batch
            .column(3)
            .as_any()
            .downcast_ref::<BinaryArray>()
            .unwrap();
        assert_eq!(b.value(0), [1_u8].as_slice());
        assert_eq!(b.value(1), b"ab".as_slice());
        assert!(b.is_null(2) && b.is_null(3));
    }

    #[test]
    fn params() {
        use rusqlite::types::Value as V;
        assert_eq!(to_sql(&Value::Bool(true)), V::Integer(1));
        assert_eq!(to_sql(&Value::UInt(7)), V::Integer(7));
        assert!(matches!(to_sql(&Value::UInt(u64::MAX)), V::Real(_)));
        assert_eq!(
            to_sql(&Value::Decimal("1.50".into())),
            V::Text("1.50".into())
        );
        let tz = chrono::DateTime::parse_from_rfc3339("2026-10-31T12:30:00.250+02:00").unwrap();
        assert_eq!(
            to_sql(&Value::TimestampTz(tz)),
            V::Text("2026-10-31 12:30:00.250+02:00".into())
        );
        let naive = tz.naive_local();
        assert_eq!(
            to_sql(&Value::Timestamp(naive)),
            V::Text("2026-10-31 12:30:00.250".into())
        );
        assert_eq!(
            to_sql(&Value::Date(naive.date())),
            V::Text("2026-10-31".into())
        );
        assert_eq!(
            to_sql(&Value::Time(naive.time())),
            V::Text("12:30:00.250".into())
        );
        assert_eq!(to_sql(&Value::Null), V::Null);
        assert_eq!(to_sql(&Value::Bytes(vec![1])), V::Blob(vec![1]));
        assert_eq!(
            to_sql(&Value::Uuid(uuid::Uuid::nil())),
            V::Text("00000000-0000-0000-0000-000000000000".into())
        );
    }
}
