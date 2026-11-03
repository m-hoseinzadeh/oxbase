//! SQLite type mapping (`docs/m8-contracts.md` §4.1).
//!
//! Required facts (transfer stream; public SQLite documentation, type affinity): integers →
//! `INTEGER` (unsigned 64-bit → `TEXT`, widened); floats → `REAL`; decimals → `NUMERIC` up
//! to 15 significant digits (narrowed beyond) or `DECIMAL_TEXT(p,s)` with
//! `DdlContext::exact_values` (widened: kept as text); text, char, varchar → `TEXT` (length not
//! enforced: widened); binary → `BLOB`; dates and times → `TEXT` ISO-8601 (widened);
//! `timestamp_tz` → `TEXT` with offset (widened); `uuid` → `TEXT` (widened); `json` → `TEXT`
//! (widened); arrays, enums, intervals → `TEXT` (widened); `other` → `TEXT` with the value's
//! text form (widened).
//!
//! # Target table (`map`)
//!
//! The logical type is kept wherever the SQLite generator's declared type maps back to it (so a
//! copied table reads back with the same logical types); the fidelity says what SQLite's
//! dynamic typing does to the values.
//!
//! | Logical | SQLite declared type | Fidelity |
//! |---|---|---|
//! | `int64` | `INTEGER` | exact |
//! | other integers (signed, or unsigned below 64 bits) | `INTEGER` | widened (64-bit; an auto-increment column must be `INTEGER`) |
//! | `int64 unsigned` | `TEXT` | widened (SQLite integers are signed 64-bit) |
//! | `bool` | `BOOLEAN` (0 / 1) | widened |
//! | `float64` | `REAL` | exact; `float32` widened |
//! | `decimal(p,s)`, `p` ≤ 15 | `DECIMAL(p,s)` | exact |
//! | longer or unconstrained decimals | `DECIMAL_TEXT(p,s)` (`TEXT` affinity) | widened |
//! | `char(n)`, `varchar(n)` | `CHAR(n)`, `VARCHAR(n)` (not enforced) | widened |
//! | `varchar`, `text` | `TEXT` | exact |
//! | `binary(n)`, `varbinary(n)` | `BLOB` | widened; `varbinary` exact |
//! | `date`, `time`, `timestamp`, `timestamptz` | `DATE`, `TIME`, `DATETIME`, `TIMESTAMPTZ` (ISO-8601 text) | widened |
//! | `uuid`, `json`, `jsonb` | `UUID`, `JSON`, `JSONB` (text) | widened |
//! | `interval`, `array`, `enum`, `other` | `TEXT` | widened |

use oxbase_dialect::{Fidelity, TypeMapper, TypeMapping};
use oxbase_types::{DataType, ServerVersion};

/// Significant digits a `NUMERIC`-affinity column keeps (a double).
const NUMERIC_DIGITS: u32 = 15;

/// The SQLite [`TypeMapper`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteTypeMapper;

impl TypeMapper for SqliteTypeMapper {
    fn map(&self, data_type: &DataType, version: &ServerVersion) -> TypeMapping {
        let _ = version;
        let widened =
            |to: DataType, reason: &str| TypeMapping::with(to, "", Fidelity::Widened, reason);
        let t = data_type;
        match t {
            DataType::Int {
                bits: 64,
                unsigned: true,
            } => widened(
                DataType::Text,
                "SQLite integers are signed 64-bit: stored as decimal text",
            ),
            DataType::Int { bits: 64, .. }
            | DataType::Float { bits: 64 }
            | DataType::Text
            | DataType::VarChar { length: None }
            | DataType::VarBinary { length: None } => TypeMapping::exact(t.clone()),
            DataType::Int { .. } => widened(
                DataType::INT64,
                "SQLite stores every integer as a 64-bit INTEGER",
            ),
            DataType::Bool => widened(
                DataType::Bool,
                "SQLite has no boolean type: stored as the integers 0 and 1",
            ),
            DataType::Float { .. } => widened(
                DataType::FLOAT64,
                "SQLite floats are 64-bit: every value is kept",
            ),
            DataType::Decimal { precision, scale } => match precision {
                Some(p) if *p <= NUMERIC_DIGITS => TypeMapping::exact(DataType::Decimal {
                    precision: Some(*p),
                    scale: Some(scale.unwrap_or(0)),
                }),
                _ => widened(
                    t.clone(),
                    "more than 15 digits: declared DECIMAL_TEXT, so the exact text is kept (it \
                     compares and sorts as text)",
                ),
            },
            DataType::Char { .. } | DataType::VarChar { .. } => widened(
                t.clone(),
                "SQLite does not enforce text lengths: every value is kept",
            ),
            DataType::Binary { .. } | DataType::VarBinary { .. } => widened(
                DataType::VarBinary { length: None },
                "stored as a BLOB without a length limit",
            ),
            DataType::Date
            | DataType::Time { .. }
            | DataType::Timestamp { .. }
            | DataType::TimestampTz { .. } => widened(
                t.clone(),
                "SQLite has no date and time types: stored as ISO-8601 text",
            ),
            DataType::Uuid => widened(
                DataType::Uuid,
                "SQLite has no uuid type: stored as its 36-character text",
            ),
            DataType::Json { .. } => {
                widened(t.clone(), "SQLite has no JSON type: stored as JSON text")
            }
            DataType::Interval => widened(
                DataType::Text,
                "SQLite has no interval type: stored as the source's text form",
            ),
            DataType::Array { .. } => widened(
                DataType::Text,
                "SQLite has no array types: stored as the source's text form",
            ),
            DataType::Enum { .. } => widened(
                DataType::Text,
                "SQLite has no enum types: the labels are stored as text",
            ),
            DataType::Other { .. } => widened(
                DataType::Text,
                "an engine-specific type: stored as the value's text form",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::representative_types;
    use pretty_assertions::assert_eq;

    #[test]
    fn total_with_reasons_and_never_unsupported() {
        let v = ServerVersion::new(3, 50, 4);
        for t in representative_types() {
            let m = SqliteTypeMapper.map(&t, &v);
            assert!(
                m.fidelity == Fidelity::Exact || m.reason.is_some(),
                "{t}: no reason"
            );
            assert_ne!(m.fidelity, Fidelity::Unsupported, "{t}");
        }
        assert_eq!(
            SqliteTypeMapper.map(&DataType::INT64, &v),
            TypeMapping::exact(DataType::INT64)
        );
        assert_eq!(
            SqliteTypeMapper
                .map(
                    &DataType::Decimal {
                        precision: Some(38),
                        scale: Some(10)
                    },
                    &v
                )
                .fidelity,
            Fidelity::Widened
        );
    }
}
