//! SQL Server type mapping (`docs/m8-contracts.md` §4.1).
//!
//! Required facts (transfer stream; public SQL Server documentation): `bool` → `bit`; `int8`
//! → `smallint` (SQL Server `tinyint` is unsigned); unsigned 32-bit → `bigint` (widened),
//! unsigned 64-bit → `decimal(20,0)` (widened); `decimal` up to 38 digits (unconstrained →
//! `decimal(38,10)`, narrowed); text → `nvarchar(n)` / `nvarchar(max)` (Unicode); binary →
//! `varbinary(n)` / `varbinary(max)`; `time(p)` / `datetime2(p)` with p ≤ 7; `timestamp_tz` →
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
//! | `char(n)` / `varchar(n)`, `n` ≤ 4 000 | `nchar(n)` / `nvarchar(n)` | exact; longer: `nvarchar(max)`, widened |
//! | `varchar`, `text` | `nvarchar(max)` | exact |
//! | `binary(n)` / `varbinary(n)`, `n` ≤ 8 000 | `binary(n)` / `varbinary(n)` | exact; longer: `varbinary(max)`, widened |
//! | `date`, `time(p)`, `timestamp(p)`, `timestamptz(p)` | `date`, `time(p)`, `datetime2(p)`, `datetimeoffset(p)` | exact |
//! | `interval` | `nvarchar(64)` | widened |
//! | `uuid` | `uniqueidentifier` | exact |
//! | `json`, `jsonb` | 2025+: `json` (exact); older: `nvarchar(max)` (widened) | |
//! | `array<e>` | `nvarchar(max)` holding a JSON array | widened |
//! | `enum(...)` | `nvarchar(n)`, `n` the longest label | widened |
//! | `other(name)`, a SQL Server type | `name` | narrowed (text form) |
//! | `other(name)` otherwise | - | unsupported |

use oxbase_dialect::{Fidelity, TypeMapper, TypeMapping};
use oxbase_types::{DataType, ServerVersion};

/// Longest `nchar(n)` / `nvarchar(n)` (characters).
const MAX_NCHAR: u32 = 4_000;
/// Longest `binary(n)` / `varbinary(n)` (bytes).
const MAX_BINARY: u32 = 8_000;
/// `decimal(p,s)` precision limit.
const MAX_PRECISION: u32 = 38;
/// Scale of unconstrained decimals.
const FREE_SCALE: u32 = 10;
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

/// The SQL Server [`TypeMapper`].
#[derive(Debug, Clone, Copy, Default)]
pub struct MsSqlTypeMapper;

/// SQL Server 2025 (version 17) has a `json` type.
fn has_json(version: &ServerVersion) -> bool {
    version.major >= 17
}

impl TypeMapper for MsSqlTypeMapper {
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
                     integer digits are refused, more than 10 decimal places are rounded",
                ),
            },
            DataType::Char { length } => match length {
                Some(n) if *n <= MAX_NCHAR => TypeMapping::exact(t.clone()),
                _ => widened(
                    DataType::Text,
                    "longer than nchar(4000) or without a length: stored as nvarchar(max)",
                ),
            },
            DataType::VarChar { length } => match length {
                Some(n) if *n <= MAX_NCHAR => TypeMapping::exact(t.clone()),
                Some(_) => widened(
                    DataType::Text,
                    "longer than nvarchar(4000): stored as nvarchar(max)",
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
                    .map(|l| l.chars().count())
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
