//! MySQL and MariaDB type mapping (`docs/m8-contracts.md` §4.1). One mapper serves both engines; the
//! `mariadb` flag selects MariaDB's differences (`uuid` type on 10.7+, `inet6`, JSON stored
//! as `longtext` with a `json_valid` check).
//!
//! Required facts (transfer stream; public MySQL / MariaDB documentation): `bool` →
//! `tinyint(1)`; unsigned integers kept (`bigint unsigned`); `decimal` up to 65 digits and 30
//! scale (unconstrained → `decimal(65,30)`, narrowed); `varchar(n)` up to 16 383 characters
//! in `utf8mb4` else `longtext`; `timestamp_tz` → `datetime(6)` holding the UTC instant
//! (narrowed: the offset is dropped); `time` → `time(6)`; `interval` → `varchar(64)` text
//! (widened); `uuid` → `char(36)` (MariaDB 10.7+: `uuid`, exact); `json` / `jsonb` → `json`
//! (exact as a JSON value; key order and spacing normalized); arrays → `json` (widened); enums
//! → `enum(...)` (exact); binary → `varbinary(n)` / `longblob`; `other` → `unsupported`
//! unless the name is a MySQL type.
//!
//! # Target table (`map`)
//!
//! | Logical | MySQL / MariaDB | Fidelity |
//! |---|---|---|
//! | `bool` | `tinyint(1)` | exact |
//! | `int8..int64`, signed or unsigned | `tinyint` .. `bigint` (`unsigned` kept) | exact |
//! | `float32` / `float64` | `float` / `double` | exact |
//! | `decimal(p,s)`, `p` ≤ 65, `s` ≤ 30 | `decimal(p,s)` | exact |
//! | `decimal` beyond that, or unconstrained | `decimal(65,30)` (or capped) | narrowed |
//! | `char(n)`, `n` ≤ 255 | `char(n)` | exact; longer: `varchar(n)` / `longtext`, widened |
//! | `varchar(n)`, `n` ≤ 16 383 | `varchar(n)` | exact; longer: `longtext`, widened |
//! | `varchar`, `text` | `longtext` | exact |
//! | `binary(n)`, `n` ≤ 255 | `binary(n)` | exact; longer: `varbinary(n)` / `longblob`, widened |
//! | `varbinary(n)`, `n` ≤ 8 000 | `varbinary(n)` | exact; longer: `longblob`, widened |
//! | `varbinary` | `longblob` | exact |
//! | `date` | `date` | exact |
//! | `time(p)` / `timestamp(p)` | `time(p)` / `datetime(p)`, `p` default 6 | exact; `p` = 7 narrowed |
//! | `timestamptz(p)` | `datetime(p)` with the UTC instant | narrowed (offset dropped) |
//! | `interval` | `varchar(64)` (the source's text form) | widened |
//! | `uuid` | MySQL `char(36)` (widened); MariaDB 10.7+ `uuid` (exact) | |
//! | `json`, `jsonb` | `json` | exact |
//! | `array<e>` | `json` (a JSON array) | widened |
//! | `enum(...)` | `enum(...)` | exact |
//! | `other(name)`, a MySQL / MariaDB type | `name` | narrowed (text form) |
//! | `other(name)` otherwise | - | unsupported |

use oxbase_dialect::{Fidelity, TypeMapper, TypeMapping};
use oxbase_types::{ColumnDef, DataType, ServerVersion};

use super::cap_precision;

/// Longest `varchar(n)` (characters) a `utf8mb4` column can declare.
const MAX_VARCHAR: u32 = 16_383;
/// Longest `char(n)` / `binary(n)`.
const MAX_CHAR: u32 = 255;
/// Longest `varbinary(n)` the mapper declares (row size limit: 65 535 bytes for all columns).
const MAX_VARBINARY: u32 = 8_000;
/// `decimal(p,s)` limits.
const MAX_PRECISION: u32 = 65;
const MAX_SCALE: u32 = 30;
/// Fractional-second digits MySQL and MariaDB keep.
const MAX_FRACTION: u8 = 6;

/// MySQL / MariaDB types without a neutral equivalent (the text form is copied).
const NATIVE: &[&str] = &[
    "geometry",
    "point",
    "linestring",
    "polygon",
    "multipoint",
    "multilinestring",
    "multipolygon",
    "geometrycollection",
    "year",
    "bit",
    "set",
    "tinytext",
    "mediumtext",
    "tinyblob",
    "mediumblob",
    "inet4",
    "inet6",
];

/// MySQL / MariaDB as a transfer target: `TEXT` / `BLOB` columns cannot be key columns without
/// a prefix (keys become `varchar(255)` / `varbinary(255)`); an InnoDB index key holds at most
/// 3 072 bytes (4 per `utf8mb4` character); a literal default on a `TEXT` column is refused by
/// MySQL before 8.0.13 and must be an expression `('x')` from 8.0.13 (MariaDB accepts it
/// since 10.2.1); booleans are `1` / `0`; `float` / `double` refuse NaN and infinities;
/// `SET FOREIGN_KEY_CHECKS = 1` does not check the rows loaded meanwhile.
pub(crate) fn target_rules(mariadb: bool, version: &ServerVersion) -> super::TargetRules {
    let text_defaults = if mariadb {
        if version.at_least_patch(10, 2, 1) {
            super::TextDefaults::Plain
        } else {
            super::TextDefaults::Dropped
        }
    } else if version.at_least_patch(8, 0, 13) {
        super::TextDefaults::Parenthesized
    } else {
        super::TextDefaults::Dropped
    };
    super::TargetRules {
        keys_on_unbounded: false,
        key_text_length: 255,
        index_key_bytes: Some(3_072),
        key_bytes_per_char: 4,
        text_defaults,
        bool_literals: ("1", "0"),
        utf16_lengths: false,
        non_finite_floats: false,
        fk_reenable_validates: false,
    }
}

/// The MySQL / MariaDB [`TypeMapper`].
#[derive(Debug, Clone, Copy, Default)]
pub struct MySqlTypeMapper {
    pub mariadb: bool,
}

impl TypeMapper for MySqlTypeMapper {
    fn logical(&self, column: &ColumnDef, version: &ServerVersion) -> DataType {
        let _ = version;
        match &column.data_type {
            DataType::Other { name } if name.eq_ignore_ascii_case("year") => DataType::INT16,
            other => other.clone(),
        }
    }

    fn map(&self, data_type: &DataType, version: &ServerVersion) -> TypeMapping {
        self.map_type(data_type, version)
    }
}

impl MySqlTypeMapper {
    fn engine(self) -> &'static str {
        if self.mariadb { "MariaDB" } else { "MySQL" }
    }

    fn map_type(self, t: &DataType, version: &ServerVersion) -> TypeMapping {
        let widened =
            |to: DataType, reason: String| TypeMapping::with(to, "", Fidelity::Widened, reason);
        let engine = self.engine();
        match t {
            DataType::Bool
            | DataType::Int { .. }
            | DataType::Float { .. }
            | DataType::Text
            | DataType::Date
            | DataType::Json { .. }
            | DataType::Enum { .. }
            | DataType::VarBinary { length: None } => TypeMapping::exact(t.clone()),
            DataType::Decimal { precision, scale } => match (precision, scale) {
                (Some(p), s) if *p <= MAX_PRECISION && s.unwrap_or(0) <= MAX_SCALE => {
                    TypeMapping::exact(DataType::Decimal {
                        precision: Some(*p),
                        scale: Some(s.unwrap_or(0)),
                    })
                }
                (Some(p), s) => {
                    let s = s.unwrap_or(0).min(MAX_SCALE);
                    TypeMapping::with(
                        DataType::Decimal {
                            precision: Some((*p).min(MAX_PRECISION)),
                            scale: Some(s),
                        },
                        "",
                        Fidelity::Narrowed,
                        format!(
                            "{engine} decimals have at most {MAX_PRECISION} digits and \
                             {MAX_SCALE} decimal places: values with more digits or more \
                             decimal places are refused"
                        ),
                    )
                }
                (None, _) => TypeMapping::with(
                    DataType::Decimal {
                        precision: Some(MAX_PRECISION),
                        scale: Some(MAX_SCALE),
                    },
                    "",
                    Fidelity::Narrowed,
                    format!(
                        "unconstrained decimals become decimal({MAX_PRECISION},{MAX_SCALE}): \
                         values with more than {} integer digits are refused, more than \
                         {MAX_SCALE} decimal places are refused",
                        MAX_PRECISION - MAX_SCALE
                    ),
                ),
            },
            DataType::Char { length } => match length {
                Some(n) if *n <= MAX_CHAR => TypeMapping::exact(t.clone()),
                Some(n) if *n <= MAX_VARCHAR => widened(
                    DataType::VarChar { length: Some(*n) },
                    format!(
                        "{engine} char(n) holds at most {MAX_CHAR} characters: stored as varchar({n})"
                    ),
                ),
                _ => widened(
                    DataType::Text,
                    "a fixed-length text without a length that fits char(n) is stored as longtext"
                        .to_owned(),
                ),
            },
            DataType::VarChar { length } => match length {
                Some(n) if *n <= MAX_VARCHAR => TypeMapping::exact(t.clone()),
                Some(_) => widened(
                    DataType::Text,
                    format!("longer than varchar({MAX_VARCHAR}) in utf8mb4: stored as longtext"),
                ),
                None => TypeMapping::exact(DataType::Text),
            },
            DataType::Binary { length } => match length {
                Some(n) if *n <= MAX_CHAR => TypeMapping::exact(t.clone()),
                Some(n) if *n <= MAX_VARBINARY => widened(
                    DataType::VarBinary { length: Some(*n) },
                    format!(
                        "{engine} binary(n) holds at most {MAX_CHAR} bytes: stored as varbinary({n})"
                    ),
                ),
                _ => widened(
                    DataType::VarBinary { length: None },
                    "stored as longblob (no fixed length)".to_owned(),
                ),
            },
            DataType::VarBinary { length: Some(n) } => {
                if *n <= MAX_VARBINARY {
                    TypeMapping::exact(t.clone())
                } else {
                    widened(
                        DataType::VarBinary { length: None },
                        "stored as longblob (no length limit)".to_owned(),
                    )
                }
            }
            DataType::Time { precision } => {
                fraction(*precision, |p| DataType::Time { precision: p })
            }
            DataType::Timestamp { precision } => {
                fraction(*precision, |p| DataType::Timestamp { precision: p })
            }
            DataType::TimestampTz { precision } => {
                let (p, _) = cap_precision(Some(precision.unwrap_or(MAX_FRACTION)), MAX_FRACTION);
                TypeMapping::with(
                    DataType::Timestamp { precision: p },
                    "",
                    Fidelity::Narrowed,
                    "stored as datetime holding the UTC instant: the time zone offset is dropped",
                )
            }
            DataType::Interval => widened(
                DataType::VarChar { length: Some(64) },
                format!("{engine} has no interval type: stored as the source's text form"),
            ),
            DataType::Uuid => {
                if self.mariadb && version.at_least(10, 7) {
                    TypeMapping::exact(DataType::Uuid)
                } else {
                    widened(
                        DataType::Char { length: Some(36) },
                        format!("{engine} has no uuid type: stored as its 36-character text"),
                    )
                }
            }
            DataType::Array { .. } => widened(
                DataType::Json { binary: false },
                format!("{engine} has no array types: stored as JSON arrays"),
            ),
            DataType::Other { name } => {
                let lower = name.trim().to_ascii_lowercase();
                let base = lower.split('(').next().unwrap_or("").trim();
                if NATIVE.contains(&base) {
                    TypeMapping::with(
                        t.clone(),
                        lower.clone(),
                        Fidelity::Narrowed,
                        format!(
                            "an engine-specific type: values are copied in their text form, \
                             which {engine} must accept"
                        ),
                    )
                } else {
                    TypeMapping::unsupported(t.clone(), format!("{engine} has no type '{name}'"))
                }
            }
        }
    }
}

/// `time` / `datetime` with [`MAX_FRACTION`] digits by default (the source's default precision
/// is unknown, so microseconds are kept).
fn fraction(precision: Option<u8>, make: impl Fn(Option<u8>) -> DataType) -> TypeMapping {
    match cap_precision(Some(precision.unwrap_or(MAX_FRACTION)), MAX_FRACTION) {
        (p, false) => TypeMapping::exact(make(p)),
        (p, true) => TypeMapping::with(
            make(p),
            "",
            Fidelity::Narrowed,
            "MySQL and MariaDB keep microseconds: digits below a microsecond are cut",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::representative_types;
    use pretty_assertions::assert_eq;

    const MYSQL: MySqlTypeMapper = MySqlTypeMapper { mariadb: false };
    const MARIADB: MySqlTypeMapper = MySqlTypeMapper { mariadb: true };

    fn v() -> ServerVersion {
        ServerVersion::new(8, 4, 13)
    }

    #[test]
    fn total_with_reasons() {
        for mapper in [MYSQL, MARIADB] {
            for t in representative_types() {
                let m = mapper.map(&t, &ServerVersion::new(11, 8, 9));
                assert!(
                    m.fidelity == Fidelity::Exact || m.reason.is_some(),
                    "{t}: no reason"
                );
            }
        }
    }

    #[test]
    fn the_postgres_exit_check_set() {
        let m = |t: DataType| MYSQL.map(&t, &v());
        assert_eq!(m(DataType::Bool), TypeMapping::exact(DataType::Bool));
        let free = m(DataType::Decimal {
            precision: None,
            scale: None,
        });
        assert_eq!(free.fidelity, Fidelity::Narrowed);
        assert_eq!(
            free.data_type,
            DataType::Decimal {
                precision: Some(65),
                scale: Some(30)
            }
        );
        let tz = m(DataType::TimestampTz { precision: Some(6) });
        assert_eq!(tz.data_type, DataType::Timestamp { precision: Some(6) });
        assert_eq!(tz.fidelity, Fidelity::Narrowed);
        assert_eq!(
            m(DataType::Time { precision: None }).data_type,
            DataType::Time { precision: Some(6) }
        );
        assert_eq!(
            m(DataType::Interval).data_type,
            DataType::VarChar { length: Some(64) }
        );
        assert_eq!(
            m(DataType::Uuid).data_type,
            DataType::Char { length: Some(36) }
        );
        assert_eq!(m(DataType::Uuid).fidelity, Fidelity::Widened);
        assert_eq!(
            MARIADB.map(&DataType::Uuid, &ServerVersion::new(11, 8, 9)),
            TypeMapping::exact(DataType::Uuid)
        );
        let array = m(DataType::Array {
            element: Box::new(DataType::INT32),
        });
        assert_eq!(array.data_type, DataType::Json { binary: false });
        assert_eq!(array.fidelity, Fidelity::Widened);
        assert_eq!(m(DataType::Text), TypeMapping::exact(DataType::Text));
        assert_eq!(
            m(DataType::VarChar { length: None }),
            TypeMapping::exact(DataType::Text)
        );
        assert_eq!(
            m(DataType::Other {
                name: "money".into()
            })
            .fidelity,
            Fidelity::Unsupported
        );
        assert_eq!(
            m(DataType::Other {
                name: "geometry".into()
            })
            .native_type,
            "geometry"
        );
        let year = ColumnDef::new(
            "y",
            DataType::Other {
                name: "year".into(),
            },
        );
        assert_eq!(MYSQL.logical(&year, &v()), DataType::INT16);
    }
}
