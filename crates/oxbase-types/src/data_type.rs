//! Engine-neutral logical data types.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The logical type of a column, parameter or value.
///
/// This is the engine-neutral view used by transfer, compare and the grid. Each column also keeps
/// its engine-native type text (see [`crate::ColumnDef::native_type`]) so nothing is lost when
/// the logical type cannot express a detail; [`DataType::Other`] carries types with no neutral
/// equivalent (PostGIS `geometry`, MySQL `SET`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DataType {
    Bool,
    /// Integer of `bits` width (8, 16, 32 or 64).
    Int {
        bits: u8,
        unsigned: bool,
    },
    /// Binary floating point of `bits` width (32 or 64).
    Float {
        bits: u8,
    },
    /// Exact numeric. `None` precision means unconstrained (PostgreSQL bare `numeric`).
    Decimal {
        precision: Option<u32>,
        scale: Option<u32>,
    },
    /// Fixed-length character data.
    Char {
        length: Option<u32>,
    },
    /// Variable-length character data with an optional limit.
    VarChar {
        length: Option<u32>,
    },
    /// Unbounded text (`text`, `longtext`, `nvarchar(max)`).
    Text,
    /// Fixed-length binary data.
    Binary {
        length: Option<u32>,
    },
    /// Variable-length binary data with an optional limit (`bytea`, `varbinary`, `blob`).
    VarBinary {
        length: Option<u32>,
    },
    Date,
    /// Time of day without zone; `precision` is fractional-second digits.
    Time {
        precision: Option<u8>,
    },
    /// Date and time without zone.
    Timestamp {
        precision: Option<u8>,
    },
    /// Date and time with zone or offset.
    TimestampTz {
        precision: Option<u8>,
    },
    Interval,
    Uuid,
    /// JSON document. `binary` marks storage forms such as PostgreSQL `jsonb`.
    Json {
        binary: bool,
    },
    /// Array of an element type.
    Array {
        element: Box<DataType>,
    },
    /// Enumeration with its labels in declaration order.
    Enum {
        labels: Vec<String>,
    },
    /// Engine-specific type with no neutral equivalent; `name` is the native type name.
    Other {
        name: String,
    },
}

impl DataType {
    pub const INT16: DataType = DataType::Int {
        bits: 16,
        unsigned: false,
    };
    pub const INT32: DataType = DataType::Int {
        bits: 32,
        unsigned: false,
    };
    pub const INT64: DataType = DataType::Int {
        bits: 64,
        unsigned: false,
    };
    pub const FLOAT64: DataType = DataType::Float { bits: 64 };

    /// `true` for integer, float and decimal types.
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            Self::Int { .. } | Self::Float { .. } | Self::Decimal { .. }
        )
    }

    /// `true` for character types.
    pub fn is_textual(&self) -> bool {
        matches!(self, Self::Char { .. } | Self::VarChar { .. } | Self::Text)
    }

    /// `true` for date, time, timestamp and interval types.
    pub fn is_temporal(&self) -> bool {
        matches!(
            self,
            Self::Date
                | Self::Time { .. }
                | Self::Timestamp { .. }
                | Self::TimestampTz { .. }
                | Self::Interval
        )
    }
}

impl fmt::Display for DataType {
    /// A neutral, SQL-ish rendering for UI and logs. Not valid DDL for any particular engine;
    /// dialect crates produce real type names.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn len(f: &mut fmt::Formatter<'_>, name: &str, n: Option<u32>) -> fmt::Result {
            match n {
                Some(n) => write!(f, "{name}({n})"),
                None => f.write_str(name),
            }
        }
        fn prec(f: &mut fmt::Formatter<'_>, name: &str, p: Option<u8>) -> fmt::Result {
            len(f, name, p.map(u32::from))
        }
        match self {
            Self::Bool => f.write_str("boolean"),
            Self::Int { bits, unsigned } => {
                write!(f, "int{bits}")?;
                if *unsigned {
                    f.write_str(" unsigned")?;
                }
                Ok(())
            }
            Self::Float { bits } => write!(f, "float{bits}"),
            Self::Decimal { precision, scale } => match (precision, scale) {
                (Some(p), Some(s)) => write!(f, "decimal({p},{s})"),
                (Some(p), None) => write!(f, "decimal({p})"),
                _ => f.write_str("decimal"),
            },
            Self::Char { length } => len(f, "char", *length),
            Self::VarChar { length } => len(f, "varchar", *length),
            Self::Text => f.write_str("text"),
            Self::Binary { length } => len(f, "binary", *length),
            Self::VarBinary { length } => len(f, "varbinary", *length),
            Self::Date => f.write_str("date"),
            Self::Time { precision } => prec(f, "time", *precision),
            Self::Timestamp { precision } => prec(f, "timestamp", *precision),
            Self::TimestampTz { precision } => prec(f, "timestamptz", *precision),
            Self::Interval => f.write_str("interval"),
            Self::Uuid => f.write_str("uuid"),
            Self::Json { binary: false } => f.write_str("json"),
            Self::Json { binary: true } => f.write_str("jsonb"),
            Self::Array { element } => write!(f, "{element}[]"),
            Self::Enum { labels } => write!(f, "enum({})", labels.join(", ")),
            Self::Other { name } => f.write_str(name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display() {
        assert_eq!(DataType::INT64.to_string(), "int64");
        assert_eq!(
            DataType::Int {
                bits: 32,
                unsigned: true
            }
            .to_string(),
            "int32 unsigned"
        );
        assert_eq!(
            DataType::Decimal {
                precision: Some(10),
                scale: Some(2)
            }
            .to_string(),
            "decimal(10,2)"
        );
        assert_eq!(DataType::VarChar { length: None }.to_string(), "varchar");
        assert_eq!(
            DataType::Array {
                element: Box::new(DataType::Text)
            }
            .to_string(),
            "text[]"
        );
        assert_eq!(DataType::Json { binary: true }.to_string(), "jsonb");
    }

    #[test]
    fn classes() {
        assert!(DataType::FLOAT64.is_numeric());
        assert!(DataType::Text.is_textual());
        assert!(DataType::TimestampTz { precision: Some(6) }.is_temporal());
        assert!(!DataType::Uuid.is_numeric());
    }

    #[test]
    fn serde_round_trip() {
        let t = DataType::Array {
            element: Box::new(DataType::Decimal {
                precision: Some(12),
                scale: Some(4),
            }),
        };
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<DataType>(&json).unwrap(), t);
        assert_eq!(
            serde_json::to_string(&DataType::Bool).unwrap(),
            r#"{"type":"bool"}"#
        );
    }
}
