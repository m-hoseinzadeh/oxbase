//! PostgreSQL type mapping (`docs/m8-contracts.md` §4.1).
//!
//! Required facts (transfer stream; public PostgreSQL documentation): every logical type has an
//! exact home (`boolean`, `smallint` / `integer` / `bigint`, unsigned 64-bit → `numeric(20,0)`
//! (widened), `numeric(p,s)` / bare `numeric`, `real` / `double precision`, `char(n)`,
//! `varchar(n)`, `text`, `bytea`, `date`, `time(p)`, `timestamp(p)`, `timestamptz(p)`,
//! `interval`, `uuid`, `json` / `jsonb`, arrays `<element>[]`); enums → `text` with a `CHECK`
//! is **not** generated: `text` (widened; a `CREATE TYPE` in a foreign schema is out of scope);
//! `other` → `unsupported` unless the name is a built-in PostgreSQL type. Source side
//! (`logical`): domains map to their base type, arrays of arrays stay `array`.
//!
//! # Target table (`map`)
//!
//! | Logical | PostgreSQL | Fidelity |
//! |---|---|---|
//! | `bool`, `int16/32/64`, `float32/64`, `decimal`, `char(n)`, `varchar`, `text`, `date`, `interval`, `uuid`, `json`, `jsonb` | same | exact |
//! | `int8` / `int8 unsigned` | `smallint` | widened (no 1-byte integer) |
//! | `int16 unsigned` / `int32 unsigned` | `integer` / `bigint` | widened |
//! | `int64 unsigned` | `numeric(20,0)` | widened |
//! | `numeric(p,s)` with `p` > 1000 | `numeric` | widened (no precision limit) |
//! | `char` without a length | `varchar` | widened |
//! | `varchar(n)` with `n` > 10 485 760 | `text` | widened |
//! | `binary(n)`, `varbinary(n)` | `bytea` | widened (no length limit); `varbinary` exact |
//! | `time(p)` / `timestamp(p)` / `timestamptz(p)` | same with `p` ≤ 6 | exact; `p` = 7: narrowed (100 ns digits cut) |
//! | `array<e>` | `e[]` | the element's fidelity |
//! | `enum(...)` | `text` | widened |
//! | `other(name)` | `name` when it is a built-in PostgreSQL type | narrowed (text form) |
//! | `other(name)` otherwise | - | unsupported |
//!
//! # Source columns (`logical`)
//!
//! The table fetcher reports enums, domains and other user-defined types as `other(<type
//! name>)`. The transfer plan merges the extras of the type object (the `Type` fetcher) into
//! the column it asks about: an enum's `enum_labels` (SQL literals, `'a', 'b'`) make it
//! `enum(a, b)`; a domain's `base_type` makes it the base type's logical type.

use oxbase_dialect::{Fidelity, TypeMapper, TypeMapping};
use oxbase_types::{ColumnDef, DataType, PropId, ServerVersion, Value};

use super::{cap_precision, quoted_labels};

/// PostgreSQL `varchar(n)` limit (characters).
const MAX_VARCHAR: u32 = 10_485_760;
/// PostgreSQL `numeric(p,s)` precision limit.
const MAX_NUMERIC: u32 = 1_000;
/// Fractional-second digits PostgreSQL keeps.
const MAX_FRACTION: u8 = 6;

/// Extra of a PostgreSQL `Type` object: the labels of an enum, as SQL literals.
const ENUM_LABELS: PropId = PropId::from_static("enum_labels");
/// Extra of a PostgreSQL `Type` object: the base type of a domain (`format_type` text).
const BASE_TYPE: PropId = PropId::from_static("base_type");

/// Built-in PostgreSQL types without a neutral equivalent that a column of another engine can
/// be created with (the text form is copied).
const BUILTIN: &[&str] = &[
    "money",
    "inet",
    "cidr",
    "macaddr",
    "macaddr8",
    "xml",
    "point",
    "line",
    "lseg",
    "box",
    "path",
    "polygon",
    "circle",
    "bit",
    "bit varying",
    "tsvector",
    "tsquery",
    "int4range",
    "int8range",
    "numrange",
    "tsrange",
    "tstzrange",
    "daterange",
    "time with time zone",
    "pg_lsn",
    "oid",
];

/// The PostgreSQL [`TypeMapper`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PgTypeMapper;

impl TypeMapper for PgTypeMapper {
    fn logical(&self, column: &ColumnDef, version: &ServerVersion) -> DataType {
        let _ = version;
        if let DataType::Other { .. } = column.data_type {
            if let Some(Value::Text(labels)) = column.extras.get(&ENUM_LABELS) {
                return DataType::Enum {
                    labels: quoted_labels(labels),
                };
            }
            if let Some(Value::Text(base)) = column.extras.get(&BASE_TYPE) {
                return pg_type(base);
            }
        }
        column.data_type.clone()
    }

    fn map(&self, data_type: &DataType, version: &ServerVersion) -> TypeMapping {
        let _ = version;
        map(data_type)
    }
}

fn map(t: &DataType) -> TypeMapping {
    let widened = |to: DataType, reason: &str| TypeMapping::with(to, "", Fidelity::Widened, reason);
    match t {
        DataType::Bool
        | DataType::Int {
            bits: 16 | 32 | 64,
            unsigned: false,
        }
        | DataType::Float { .. }
        | DataType::Char { length: Some(_) }
        | DataType::Text
        | DataType::Date
        | DataType::Interval
        | DataType::Uuid
        | DataType::Json { .. }
        | DataType::VarBinary { length: None } => TypeMapping::exact(t.clone()),
        DataType::Int { bits, unsigned } => match (*bits, *unsigned) {
            (64, true) => widened(
                DataType::Decimal {
                    precision: Some(20),
                    scale: Some(0),
                },
                "PostgreSQL has no unsigned integers: stored as numeric(20,0)",
            ),
            (32, true) => widened(
                DataType::INT64,
                "PostgreSQL has no unsigned integers: stored as bigint",
            ),
            (16, true) => widened(
                DataType::INT32,
                "PostgreSQL has no unsigned integers: stored as integer",
            ),
            _ => widened(
                DataType::INT16,
                "PostgreSQL has no 1-byte integer: stored as smallint",
            ),
        },
        DataType::Decimal { precision, scale } => match precision {
            Some(p) if *p > MAX_NUMERIC => widened(
                DataType::Decimal {
                    precision: None,
                    scale: None,
                },
                "more digits than numeric(p,s) allows: stored as numeric without a limit",
            ),
            Some(p) => TypeMapping::exact(DataType::Decimal {
                precision: Some(*p),
                scale: Some(scale.unwrap_or(0)),
            }),
            None => TypeMapping::exact(t.clone()),
        },
        DataType::Char { length: None } => widened(
            DataType::VarChar { length: None },
            "a fixed-length text without a length is stored as varchar",
        ),
        DataType::VarChar { length } => match length {
            Some(n) if *n > MAX_VARCHAR => widened(
                DataType::Text,
                "longer than varchar(n) allows: stored as text",
            ),
            _ => TypeMapping::exact(t.clone()),
        },
        DataType::Binary { .. } => widened(
            DataType::VarBinary { length: None },
            "bytea has no fixed length: the bytes are kept as they are",
        ),
        DataType::VarBinary { length: Some(_) } => widened(
            DataType::VarBinary { length: None },
            "bytea has no length limit",
        ),
        DataType::Time { precision } => fraction(*precision, |p| DataType::Time { precision: p }),
        DataType::Timestamp { precision } => {
            fraction(*precision, |p| DataType::Timestamp { precision: p })
        }
        DataType::TimestampTz { precision } => {
            fraction(*precision, |p| DataType::TimestampTz { precision: p })
        }
        DataType::Array { element } => {
            let inner = map(element);
            let data_type = DataType::Array {
                element: Box::new(inner.data_type),
            };
            match inner.fidelity {
                Fidelity::Exact => TypeMapping::exact(data_type),
                Fidelity::Unsupported => TypeMapping::unsupported(
                    t.clone(),
                    format!(
                        "no PostgreSQL array of {element}: {}",
                        inner.reason.unwrap_or_default()
                    ),
                ),
                fidelity => TypeMapping::with(
                    data_type,
                    "",
                    fidelity,
                    format!("array elements: {}", inner.reason.unwrap_or_default()),
                ),
            }
        }
        DataType::Enum { .. } => widened(
            DataType::Text,
            "PostgreSQL enums are types of their own: the labels are stored as text",
        ),
        DataType::Other { name } => {
            let lower = name.trim().to_ascii_lowercase();
            if BUILTIN.contains(&lower.as_str()) {
                TypeMapping::with(
                    t.clone(),
                    lower,
                    Fidelity::Narrowed,
                    "an engine-specific type: values are copied in their text form, which \
                     PostgreSQL must accept",
                )
            } else {
                TypeMapping::unsupported(
                    t.clone(),
                    format!("PostgreSQL has no built-in type '{name}'"),
                )
            }
        }
    }
}

/// `time` / `timestamp` types with at most [`MAX_FRACTION`] digits.
fn fraction(precision: Option<u8>, make: impl Fn(Option<u8>) -> DataType) -> TypeMapping {
    match cap_precision(precision, MAX_FRACTION) {
        (p, false) => TypeMapping::exact(make(p)),
        (p, true) => TypeMapping::with(
            make(p),
            "",
            Fidelity::Narrowed,
            "PostgreSQL keeps microseconds: digits below a microsecond are cut",
        ),
    }
}

/// The logical type of a PostgreSQL type text (`format_type` spelling, for a domain's base
/// type). Unknown names stay `other`.
fn pg_type(text: &str) -> DataType {
    let text = text.trim();
    if let Some(element) = text.strip_suffix("[]") {
        return DataType::Array {
            element: Box::new(pg_type(element)),
        };
    }
    let (base, args) = match (text.find('('), text.find(')')) {
        (Some(open), Some(close)) if open < close => {
            let args: Vec<u32> = text[open + 1..close]
                .split(',')
                .filter_map(|a| a.trim().parse().ok())
                .collect();
            (
                format!("{}{}", &text[..open], &text[close + 1..])
                    .trim()
                    .to_owned(),
                args,
            )
        }
        _ => (text.to_owned(), Vec::new()),
    };
    let first = args.first().copied();
    let precision = first.and_then(|n| u8::try_from(n).ok());
    match base.to_ascii_lowercase().as_str() {
        "boolean" | "bool" => DataType::Bool,
        "smallint" | "int2" => DataType::INT16,
        "integer" | "int" | "int4" => DataType::INT32,
        "bigint" | "int8" => DataType::INT64,
        "real" | "float4" => DataType::Float { bits: 32 },
        "double precision" | "float8" => DataType::FLOAT64,
        "numeric" | "decimal" => DataType::Decimal {
            precision: first,
            scale: match (first, args.get(1)) {
                (_, Some(s)) => Some(*s),
                (Some(_), None) => Some(0),
                (None, None) => None,
            },
        },
        "text" => DataType::Text,
        "character varying" | "varchar" => DataType::VarChar { length: first },
        "character" | "char" | "bpchar" => DataType::Char { length: first },
        "bytea" => DataType::VarBinary { length: None },
        "date" => DataType::Date,
        "time without time zone" | "time" => DataType::Time { precision },
        "timestamp without time zone" | "timestamp" => DataType::Timestamp { precision },
        "timestamp with time zone" | "timestamptz" => DataType::TimestampTz { precision },
        "uuid" => DataType::Uuid,
        "json" => DataType::Json { binary: false },
        "jsonb" => DataType::Json { binary: true },
        "interval" => DataType::Interval,
        _ => DataType::Other {
            name: text.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::representative_types;
    use oxbase_types::Extras;
    use pretty_assertions::assert_eq;

    fn v() -> ServerVersion {
        ServerVersion::new(18, 6, 0)
    }

    #[test]
    fn total_with_reasons() {
        for t in representative_types() {
            let m = PgTypeMapper.map(&t, &v());
            assert!(
                m.fidelity == Fidelity::Exact || m.reason.is_some(),
                "{t}: no reason"
            );
        }
    }

    #[test]
    fn maps_every_family() {
        let m = |t: DataType| PgTypeMapper.map(&t, &v());
        assert_eq!(m(DataType::INT32), TypeMapping::exact(DataType::INT32));
        let u64t = m(DataType::Int {
            bits: 64,
            unsigned: true,
        });
        assert_eq!(u64t.fidelity, Fidelity::Widened);
        assert_eq!(
            u64t.data_type,
            DataType::Decimal {
                precision: Some(20),
                scale: Some(0)
            }
        );
        assert_eq!(
            m(DataType::Enum {
                labels: vec!["a".into()]
            })
            .data_type,
            DataType::Text
        );
        assert_eq!(
            m(DataType::Time { precision: Some(7) }).fidelity,
            Fidelity::Narrowed
        );
        assert_eq!(
            m(DataType::Array {
                element: Box::new(DataType::Text)
            })
            .fidelity,
            Fidelity::Exact
        );
        assert_eq!(
            m(DataType::Other {
                name: "geometry".into()
            })
            .fidelity,
            Fidelity::Unsupported
        );
        assert_eq!(
            m(DataType::Other {
                name: "inet".into()
            })
            .native_type,
            "inet"
        );
    }

    #[test]
    fn enums_and_domains_from_type_extras() {
        let mut column = ColumnDef::new(
            "mood",
            DataType::Other {
                name: "s.m8_mood".into(),
            },
        );
        assert_eq!(
            PgTypeMapper.logical(&column, &v()),
            DataType::Other {
                name: "s.m8_mood".into()
            }
        );
        column.extras = Extras::new();
        column
            .extras
            .insert(ENUM_LABELS, "'sad', 'ok', 'it''s happy'");
        assert_eq!(
            PgTypeMapper.logical(&column, &v()),
            DataType::Enum {
                labels: vec!["sad".into(), "ok".into(), "it's happy".into()]
            }
        );
        let mut domain = ColumnDef::new(
            "d",
            DataType::Other {
                name: "s.positive".into(),
            },
        );
        domain.extras.insert(BASE_TYPE, "numeric(12,2)");
        assert_eq!(
            PgTypeMapper.logical(&domain, &v()),
            DataType::Decimal {
                precision: Some(12),
                scale: Some(2)
            }
        );
        assert_eq!(
            pg_type("character varying(20)[]"),
            DataType::Array {
                element: Box::new(DataType::VarChar { length: Some(20) })
            }
        );
        assert_eq!(
            pg_type("timestamp(3) with time zone"),
            DataType::TimestampTz { precision: Some(3) }
        );
    }
}
