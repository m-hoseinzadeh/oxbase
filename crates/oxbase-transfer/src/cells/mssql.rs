//! SQL Server type mapping (`docs/m8-contracts.md` §4.1).
//!
//! Required facts (transfer stream; public SQL Server documentation): `bool` → `bit`; `int8`
//! → `smallint` (SQL Server `tinyint` is unsigned); unsigned 32-bit → `bigint` (widened),
//! unsigned 64-bit → `decimal(20,0)` (widened); `decimal` up to 38 digits (unconstrained →
//! `decimal(38,10)`, narrowed); text → `nvarchar(n)` / `nvarchar(max)` (Unicode); binary →
//! `varbinary(n)` / `varbinary(max)`; `nchar(n)` / `nvarchar(n)` count UTF-16 code units, so a
//! source length counted in characters needs up to twice as many (`varchar(n)` →
//! `nvarchar(2n)`, widened; `char(n)` keeps its fixed length, narrowed); `time(p)` / `datetime2(p)` with p ≤ 7; `timestamp_tz` →
//! `datetimeoffset(p)` (exact); `interval` → `nvarchar(64)` (widened); `uuid` →
//! `uniqueidentifier`; `json` → `nvarchar(max)` with `ISJSON` not enforced (widened; the 2025
//! `json` type is used only when the server version has it: exact); arrays → `nvarchar(max)`
//! JSON text (widened); enums → `nvarchar(n)` (widened).
//!
//! # Target table (`map`)
//!
//! | Logical | SQL Server | Fidelity |
//! |---|---|---|
//! | `bool` | `bit` | exact |
//! | `int8` | `smallint` | widened |
//! | `int8 unsigned`, `int16`, `int32`, `int64` | `tinyint`, `smallint`, `int`, `bigint` | exact |
//! | `int16 unsigned` / `int32 unsigned` / `int64 unsigned` | `int` / `bigint` / `decimal(20,0)` | widened |
//! | `float32` / `float64` | `real` / `float` | exact |
//! | `decimal(p,s)`, `p` ≤ 38 | `decimal(p,s)` | exact; longer or unconstrained: `decimal(38,s)`, narrowed |
//! | `char(n)`, `n` ≤ 4 000 | `nchar(n)` | narrowed (lengths count UTF-16 code units: a character outside the BMP takes two); longer: `nvarchar(max)`, widened |
//! | `varchar(n)`, `2n` ≤ 4 000 | `nvarchar(2n)` | widened (room for `n` characters in UTF-16 code units); longer: `nvarchar(max)`, widened |
//! | `varchar`, `text` | `nvarchar(max)` | exact |
//! | `binary(n)` / `varbinary(n)`, `n` ≤ 8 000 | `binary(n)` / `varbinary(n)` | exact; longer: `varbinary(max)`, widened |
//! | `date`, `time(p)`, `timestamp(p)`, `timestamptz(p)` | `date`, `time(p)`, `datetime2(p)`, `datetimeoffset(p)` | exact |
//! | `interval` | `nvarchar(64)` | widened |
//! | `uuid` | `uniqueidentifier` | exact |
//! | `json`, `jsonb` | 2025+: `json` (exact); older: `nvarchar(max)` (widened) | |
//! | `array<e>` | `nvarchar(max)` holding a JSON array | widened |
//! | `enum(...)` | `nvarchar(n)`, `n` the longest label in UTF-16 code units | widened |
//! | `other(name)`, a SQL Server type | `name` | narrowed (text form) |
//! | `other(name)` otherwise | - | unsupported |

use oxbase_dialect::{Fidelity, TypeMapper, TypeMapping};
use oxbase_types::{ColumnDef, DataType, ServerVersion};

/// Longest `nchar(n)` / `nvarchar(n)` (characters).
const MAX_NCHAR: u32 = 4_000;
/// Longest `binary(n)` / `varbinary(n)` (bytes).
const MAX_BINARY: u32 = 8_000;
/// `decimal(p,s)` precision limit.
const MAX_PRECISION: u32 = 38;
/// Scale of unconstrained decimals.
const FREE_SCALE: u32 = 10;
/// Why a `char(n)` from another engine is narrowed on SQL Server.
const UTF16_FIXED: &str = "nchar lengths count UTF-16 code units, and characters \
     outside the BMP take two: a value with them can exceed nchar(n) and is refused";
/// Fractional-second digits SQL Server keeps.
const MAX_FRACTION: u8 = 7;

/// SQL Server types without a neutral equivalent (the text form is copied).
const NATIVE: &[&str] = &[
    "geometry",
    "geography",
    "hierarchyid",
    "xml",
    "sql_variant",
    "money",
    "smallmoney",
    "datetime",
    "smalldatetime",
];

/// SQL Server as a transfer target: `nvarchar(max)` / `varbinary(max)` cannot be key columns
/// (keys become `nvarchar(450)` / `varbinary(450)`: 900 bytes, the clustered key limit);
/// literal defaults as they are; `bit` defaults `1` / `0`; `nvarchar(n)` counts UTF-16 code
/// units; `float` / `real` refuse NaN and infinities; `WITH CHECK CHECK CONSTRAINT` checks the
/// rows loaded while foreign keys were off.
pub(crate) const TARGET_RULES: super::TargetRules = super::TargetRules {
    keys_on_unbounded: false,
    key_text_length: 450,
    index_key_bytes: None,
    key_bytes_per_char: 2,
    text_defaults: super::TextDefaults::Plain,
    bool_literals: ("1", "0"),
    utf16_lengths: true,
    non_finite_floats: false,
    fk_reenable_validates: true,
};

/// The SQL Server [`TypeMapper`].
#[derive(Debug, Clone, Copy, Default)]
pub struct MsSqlTypeMapper;

/// SQL Server 2025 (version 17) has a `json` type.
fn has_json(version: &ServerVersion) -> bool {
    version.major >= 17
}

impl TypeMapper for MsSqlTypeMapper {
    /// The column's own type, `exact`, except the temporal types the load cannot carry whole:
    /// rows travel with microseconds, so `time(7)` / `datetime2(7)` (the default precision)
    /// lose their 7th fractional digit, and a `datetimeoffset` is stored as the same instant
    /// at offset +00:00.
    fn same_engine(&self, column: &ColumnDef, version: &ServerVersion) -> TypeMapping {
        let _ = version;
        let keep = TypeMapping::exact(column.data_type.clone()).native(column.native_type.clone());
        let reason = match &column.data_type {
            DataType::TimestampTz { .. } => Some(
                "values are copied as instants: the offset becomes +00:00 and digits below a \
                 microsecond are cut",
            ),
            DataType::Time { precision } | DataType::Timestamp { precision }
                if precision.is_none_or(|p| p > 6) =>
            {
                Some("values are copied with microseconds: the 7th fractional digit is cut")
            }
            _ => None,
        };
        match reason {
            Some(reason) => TypeMapping {
                fidelity: Fidelity::Narrowed,
                reason: Some(reason.to_owned()),
                ..keep
            },
            None => keep,
        }
    }

    fn map(&self, data_type: &DataType, version: &ServerVersion) -> TypeMapping {
        let widened =
            |to: DataType, reason: &str| TypeMapping::with(to, "", Fidelity::Widened, reason);
        let t = data_type;
        match t {
            DataType::Bool
            | DataType::Int {
                bits: 8,
                unsigned: true,
            }
            | DataType::Int {
                bits: 16 | 32 | 64,
                unsigned: false,
            }
            | DataType::Float { .. }
            | DataType::Text
            | DataType::Date
            | DataType::Uuid
            | DataType::VarBinary { length: None } => TypeMapping::exact(t.clone()),
            DataType::Int { bits, unsigned } => match (*bits, *unsigned) {
                (64, true) => widened(
                    DataType::Decimal {
                        precision: Some(20),
                        scale: Some(0),
                    },
                    "SQL Server has no unsigned bigint: stored as decimal(20,0)",
                ),
                (32, true) => widened(
                    DataType::INT64,
                    "SQL Server has no unsigned int: stored as bigint",
                ),
                (16, true) => widened(
                    DataType::INT32,
                    "SQL Server has no unsigned smallint: stored as int",
                ),
                _ => widened(
                    DataType::INT16,
                    "SQL Server tinyint is unsigned: stored as smallint",
                ),
            },
            DataType::Decimal { precision, scale } => match precision {
                Some(p) if *p <= MAX_PRECISION => TypeMapping::exact(DataType::Decimal {
                    precision: Some(*p),
                    scale: Some(scale.unwrap_or(0)),
                }),
                Some(_) => TypeMapping::with(
                    DataType::Decimal {
                        precision: Some(MAX_PRECISION),
                        scale: Some(scale.unwrap_or(0).min(MAX_PRECISION)),
                    },
                    "",
                    Fidelity::Narrowed,
                    "SQL Server decimals have at most 38 digits: longer values are refused",
                ),
                None => TypeMapping::with(
                    DataType::Decimal {
                        precision: Some(MAX_PRECISION),
                        scale: Some(FREE_SCALE),
                    },
                    "",
                    Fidelity::Narrowed,
                    "unconstrained decimals become decimal(38,10): values with more than 28 \
                     integer digits are refused, more than 10 decimal places are refused",
                ),
            },
            // Source lengths count characters; SQL Server's count UTF-16 code units, where a
            // character outside the Basic Multilingual Plane (an emoji) takes two.
            DataType::Char { length } => match length {
                Some(n) if *n <= MAX_NCHAR => {
                    TypeMapping::with(t.clone(), "", Fidelity::Narrowed, UTF16_FIXED)
                }
                _ => widened(
                    DataType::Text,
                    "longer than nchar(4000) or without a length: stored as nvarchar(max)",
                ),
            },
            DataType::VarChar { length } => match length {
                Some(n) if n.saturating_mul(2) <= MAX_NCHAR => {
                    let units = n * 2;
                    widened(
                        DataType::VarChar {
                            length: Some(units),
                        },
                        &format!(
                            "nvarchar lengths count UTF-16 code units, and characters outside \
                             the BMP take two: stored as nvarchar({units}) to hold {n} characters"
                        ),
                    )
                }
                Some(_) => widened(
                    DataType::Text,
                    "nvarchar lengths count UTF-16 code units, and characters outside the BMP \
                     take two: longer than nvarchar(4000) allows, so stored as nvarchar(max)",
                ),
                None => TypeMapping::exact(DataType::Text),
            },
            DataType::Binary { length } => match length {
                Some(n) if *n <= MAX_BINARY => TypeMapping::exact(t.clone()),
                _ => widened(
                    DataType::VarBinary { length: None },
                    "longer than binary(8000) or without a length: stored as varbinary(max)",
                ),
            },
            DataType::VarBinary { length: Some(n) } => {
                if *n <= MAX_BINARY {
                    TypeMapping::exact(t.clone())
                } else {
                    widened(
                        DataType::VarBinary { length: None },
                        "longer than varbinary(8000): stored as varbinary(max)",
                    )
                }
            }
            DataType::Time { precision } => TypeMapping::exact(DataType::Time {
                precision: precision.map(|p| p.min(MAX_FRACTION)),
            }),
            DataType::Timestamp { precision } => TypeMapping::exact(DataType::Timestamp {
                precision: precision.map(|p| p.min(MAX_FRACTION)),
            }),
            DataType::TimestampTz { precision } => TypeMapping::exact(DataType::TimestampTz {
                precision: precision.map(|p| p.min(MAX_FRACTION)),
            }),
            DataType::Interval => widened(
                DataType::VarChar { length: Some(64) },
                "SQL Server has no interval type: stored as the source's text form",
            ),
            DataType::Json { binary } => {
                if has_json(version) {
                    TypeMapping::exact(DataType::Json { binary: *binary })
                } else {
                    widened(
                        DataType::Text,
                        "this server has no json type: stored as JSON text in nvarchar(max)",
                    )
                }
            }
            DataType::Array { .. } => TypeMapping::with(
                DataType::Json { binary: false },
                "nvarchar(max)",
                Fidelity::Widened,
                "SQL Server has no array types: stored as JSON arrays in nvarchar(max)",
            ),
            DataType::Enum { labels } => {
                let longest = labels
                    .iter()
                    .map(|l| l.encode_utf16().count())
                    .max()
                    .unwrap_or(1)
                    .max(1);
                let length = u32::try_from(longest).unwrap_or(MAX_NCHAR).min(MAX_NCHAR);
                widened(
                    DataType::VarChar {
                        length: Some(length),
                    },
                    "SQL Server has no enum types: the labels are stored as text",
                )
            }
            DataType::Other { name } => {
                let lower = name.trim().to_ascii_lowercase();
                if NATIVE.contains(&lower.as_str()) {
                    TypeMapping::with(
                        t.clone(),
                        lower,
                        Fidelity::Narrowed,
                        "an engine-specific type: values are copied in their text form, which \
                         SQL Server must accept",
                    )
                } else {
                    TypeMapping::unsupported(t.clone(), format!("SQL Server has no type '{name}'"))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::representative_types;
    use pretty_assertions::assert_eq;

    #[test]
    fn total_with_reasons() {
        for v in [ServerVersion::new(16, 0, 0), ServerVersion::new(17, 0, 0)] {
            for t in representative_types() {
                let m = MsSqlTypeMapper.map(&t, &v);
                assert!(
                    m.fidelity == Fidelity::Exact || m.reason.is_some(),
                    "{t}: no reason"
                );
            }
        }
    }

    #[test]
    fn same_engine_temporal_types_with_100_ns_digits_are_narrowed() {
        let v = ServerVersion::new(16, 0, 0);
        let column = |data_type: DataType, native: &str| {
            let mut c = ColumnDef::new("c", data_type);
            c.native_type = native.to_owned();
            c
        };
        let cases = [
            (
                column(DataType::Timestamp { precision: Some(7) }, "datetime2(7)"),
                Fidelity::Narrowed,
            ),
            (
                column(DataType::Timestamp { precision: None }, "datetime2"),
                Fidelity::Narrowed,
            ),
            (
                column(DataType::Time { precision: Some(7) }, "time(7)"),
                Fidelity::Narrowed,
            ),
            (
                column(DataType::Timestamp { precision: Some(3) }, "datetime2(3)"),
                Fidelity::Exact,
            ),
            (
                column(
                    DataType::TimestampTz { precision: Some(3) },
                    "datetimeoffset(3)",
                ),
                Fidelity::Narrowed,
            ),
            (column(DataType::INT32, "int"), Fidelity::Exact),
        ];
        for (c, fidelity) in cases {
            let m = MsSqlTypeMapper.same_engine(&c, &v);
            assert_eq!(m.fidelity, fidelity, "{}", c.native_type);
            assert_eq!(m.native_type, c.native_type);
            assert_eq!(m.reason.is_some(), fidelity != Fidelity::Exact);
        }
    }

    #[test]
    fn text_lengths_leave_room_for_utf16_surrogate_pairs() {
        let v = ServerVersion::new(17, 0, 0);
        // '😀😀' fits a PostgreSQL / MySQL varchar(3) (2 characters) but takes 4 UTF-16 code
        // units: nvarchar(3) would refuse it, so varchar(3) becomes nvarchar(6), widened.
        let m = MsSqlTypeMapper.map(&DataType::VarChar { length: Some(3) }, &v);
        assert_eq!(m.data_type, DataType::VarChar { length: Some(6) });
        assert_eq!(m.fidelity, Fidelity::Widened);
        assert!(
            m.reason.as_deref().is_some_and(|r| r.contains("UTF-16")),
            "{m:?}"
        );
        assert!("😀😀".encode_utf16().count() <= 6);
        // Past nvarchar(4000) in code units: nvarchar(max).
        for n in [2_001, 4_000] {
            let m = MsSqlTypeMapper.map(&DataType::VarChar { length: Some(n) }, &v);
            assert_eq!(m.data_type, DataType::Text, "{n}");
            assert_eq!(m.fidelity, Fidelity::Widened);
        }
        let m = MsSqlTypeMapper.map(
            &DataType::VarChar {
                length: Some(2_000),
            },
            &v,
        );
        assert_eq!(
            m.data_type,
            DataType::VarChar {
                length: Some(4_000)
            }
        );
        // char(n) keeps its fixed length (and padding) and says it can refuse values.
        let m = MsSqlTypeMapper.map(&DataType::Char { length: Some(3) }, &v);
        assert_eq!(m.data_type, DataType::Char { length: Some(3) });
        assert_eq!(m.fidelity, Fidelity::Narrowed);
        assert!(
            m.reason.as_deref().is_some_and(|r| r.contains("UTF-16")),
            "{m:?}"
        );
        // Enum labels are measured in code units.
        let m = MsSqlTypeMapper.map(
            &DataType::Enum {
                labels: vec!["a".to_owned(), "😀😀".to_owned()],
            },
            &v,
        );
        assert_eq!(m.data_type, DataType::VarChar { length: Some(4) });
        // Between two SQL Servers the lengths already count UTF-16 code units: exact.
        let mut c = ColumnDef::new("c", DataType::VarChar { length: Some(3) });
        c.native_type = "nvarchar(3)".to_owned();
        assert_eq!(
            MsSqlTypeMapper.same_engine(&c, &v).fidelity,
            Fidelity::Exact
        );
    }

    #[test]
    fn json_depends_on_the_version() {
        let json = DataType::Json { binary: true };
        assert_eq!(
            MsSqlTypeMapper.map(&json, &ServerVersion::new(17, 0, 0)),
            TypeMapping::exact(json.clone())
        );
        let old = MsSqlTypeMapper.map(&json, &ServerVersion::new(16, 0, 0));
        assert_eq!(old.data_type, DataType::Text);
        assert_eq!(old.fidelity, Fidelity::Widened);
        let int8 = MsSqlTypeMapper.map(
            &DataType::Int {
                bits: 8,
                unsigned: false,
            },
            &ServerVersion::new(17, 0, 0),
        );
        assert_eq!(int8.data_type, DataType::INT16);
        let labels = MsSqlTypeMapper.map(
            &DataType::Enum {
                labels: vec!["a".into(), "medium".into()],
            },
            &ServerVersion::new(17, 0, 0),
        );
        assert_eq!(labels.data_type, DataType::VarChar { length: Some(6) });
    }
}
