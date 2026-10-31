//! Typed scalar values (query parameters, edited cells, default values, extras).

use std::fmt;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::DataType;

/// One typed scalar.
///
/// Bulk row data travels as Arrow `RecordBatch`es; `Value` is for single values: statement
/// parameters, grid edits, row keys, default values and extras.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    /// Unsigned 64-bit integers that do not fit `i64` (MySQL `BIGINT UNSIGNED`).
    UInt(u64),
    /// Finite values serialize as JSON numbers; NaN and the infinities as the strings `"NaN"`,
    /// `"Infinity"` and `"-Infinity"` (JSON numbers cannot hold them).
    Float(#[serde(with = "float_serde")] f64),
    /// Exact decimal kept as its canonical text so no precision is lost (`"12345.6789"`).
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Date(NaiveDate),
    Time(NaiveTime),
    /// Date and time without zone.
    Timestamp(NaiveDateTime),
    /// Date and time with the offset reported by the server.
    TimestampTz(DateTime<FixedOffset>),
    Uuid(Uuid),
    Json(serde_json::Value),
}

/// Serde adapter for `f64` that keeps NaN and the infinities (which `serde_json` would write as
/// `null`).
mod float_serde {
    use serde::{Deserialize, Deserializer, Serializer, de};

    // serde's `with` protocol passes the field by reference.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub(super) fn serialize<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
        if v.is_finite() {
            s.serialize_f64(*v)
        } else if v.is_nan() {
            s.serialize_str("NaN")
        } else if v.is_sign_positive() {
            s.serialize_str("Infinity")
        } else {
            s.serialize_str("-Infinity")
        }
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Num(f64),
        Text(String),
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        match Repr::deserialize(d)? {
            Repr::Num(v) => Ok(v),
            Repr::Text(t) => match t.as_str() {
                "NaN" => Ok(f64::NAN),
                "Infinity" | "inf" | "+Infinity" => Ok(f64::INFINITY),
                "-Infinity" | "-inf" => Ok(f64::NEG_INFINITY),
                other => Err(de::Error::invalid_value(
                    de::Unexpected::Str(other),
                    &"a number, \"NaN\", \"Infinity\" or \"-Infinity\"",
                )),
            },
        }
    }
}

impl Value {
    /// `true` for [`Value::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Short lowercase name of the variant (`"int"`, `"text"`, ...).
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::UInt(_) => "uint",
            Self::Float(_) => "float",
            Self::Decimal(_) => "decimal",
            Self::Text(_) => "text",
            Self::Bytes(_) => "bytes",
            Self::Date(_) => "date",
            Self::Time(_) => "time",
            Self::Timestamp(_) => "timestamp",
            Self::TimestampTz(_) => "timestamptz",
            Self::Uuid(_) => "uuid",
            Self::Json(_) => "json",
        }
    }

    /// The natural logical type of this value, or `None` for `Null` (which fits any type).
    pub fn data_type(&self) -> Option<DataType> {
        Some(match self {
            Self::Null => return None,
            Self::Bool(_) => DataType::Bool,
            Self::Int(_) => DataType::INT64,
            Self::UInt(_) => DataType::Int {
                bits: 64,
                unsigned: true,
            },
            Self::Float(_) => DataType::FLOAT64,
            Self::Decimal(_) => DataType::Decimal {
                precision: None,
                scale: None,
            },
            Self::Text(_) => DataType::Text,
            Self::Bytes(_) => DataType::VarBinary { length: None },
            Self::Date(_) => DataType::Date,
            Self::Time(_) => DataType::Time { precision: None },
            Self::Timestamp(_) => DataType::Timestamp { precision: None },
            Self::TimestampTz(_) => DataType::TimestampTz { precision: None },
            Self::Uuid(_) => DataType::Uuid,
            Self::Json(_) => DataType::Json { binary: false },
        })
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Integer view of `Int` and in-range `UInt` values.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            Self::UInt(u) => i64::try_from(*u).ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(s) | Self::Decimal(s) => Some(s),
            _ => None,
        }
    }
}

impl fmt::Display for Value {
    /// Plain rendering for the grid and logs (not a SQL literal; see the dialect `Quoting` trait).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Int(i) => write!(f, "{i}"),
            Self::UInt(u) => write!(f, "{u}"),
            Self::Float(x) => write!(f, "{x}"),
            Self::Decimal(s) | Self::Text(s) => f.write_str(s),
            Self::Bytes(b) => {
                f.write_str("0x")?;
                for byte in b {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
            Self::Date(d) => write!(f, "{d}"),
            Self::Time(t) => write!(f, "{t}"),
            Self::Timestamp(t) => write!(f, "{t}"),
            Self::TimestampTz(t) => write!(f, "{}", t.to_rfc3339()),
            Self::Uuid(u) => write!(f, "{u}"),
            Self::Json(j) => write!(f, "{j}"),
        }
    }
}

macro_rules! from_direct {
    ($($t:ty => $variant:ident),* $(,)?) => {
        $(impl From<$t> for Value {
            fn from(v: $t) -> Self {
                Value::$variant(v)
            }
        })*
    };
}

macro_rules! from_widen {
    ($($t:ty => $variant:ident($wide:ty)),* $(,)?) => {
        $(impl From<$t> for Value {
            fn from(v: $t) -> Self {
                Value::$variant(<$wide>::from(v))
            }
        })*
    };
}

from_direct! {
    bool => Bool,
    i64 => Int,
    u64 => UInt,
    f64 => Float,
    String => Text,
    Vec<u8> => Bytes,
    NaiveDate => Date,
    NaiveTime => Time,
    NaiveDateTime => Timestamp,
    DateTime<FixedOffset> => TimestampTz,
    Uuid => Uuid,
    serde_json::Value => Json,
}

from_widen! {
    i8 => Int(i64),
    i16 => Int(i64),
    i32 => Int(i64),
    u8 => Int(i64),
    u16 => Int(i64),
    u32 => Int(i64),
    f32 => Float(f64),
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_owned())
    }
}

impl From<&[u8]> for Value {
    fn from(v: &[u8]) -> Self {
        Value::Bytes(v.to_vec())
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn conversions() {
        assert_eq!(Value::from(5_i32), Value::Int(5));
        assert_eq!(Value::from(u64::MAX), Value::UInt(u64::MAX));
        assert_eq!(Value::from("a"), Value::Text("a".into()));
        assert_eq!(Value::from(None::<i64>), Value::Null);
        assert_eq!(Value::from(Some(true)), Value::Bool(true));
        assert_eq!(Value::UInt(7).as_i64(), Some(7));
        assert_eq!(Value::UInt(u64::MAX).as_i64(), None);
        assert!(Value::default().is_null());
    }

    #[test]
    fn display_and_types() {
        assert_eq!(Value::Bytes(vec![0xde, 0xad]).to_string(), "0xdead");
        assert_eq!(Value::Null.to_string(), "NULL");
        let d = NaiveDate::from_ymd_opt(2026, 10, 31).unwrap();
        assert_eq!(Value::Date(d).to_string(), "2026-10-31");
        assert_eq!(Value::Date(d).data_type(), Some(DataType::Date));
        assert_eq!(Value::Null.data_type(), None);
        assert_eq!(Value::Decimal("1.50".into()).type_name(), "decimal");
    }

    #[test]
    fn serde_round_trip() {
        let ts = DateTime::parse_from_rfc3339("2026-10-31T12:00:00+02:00").unwrap();
        let values = vec![
            Value::Null,
            Value::Bool(false),
            Value::Int(-3),
            Value::UInt(u64::MAX),
            Value::Float(1.5),
            Value::Decimal("99999999999999999999.01".into()),
            Value::Text("hi".into()),
            Value::Bytes(vec![1, 2, 3]),
            Value::Timestamp(ts.naive_utc()),
            Value::TimestampTz(ts),
            Value::Uuid(Uuid::nil()),
            Value::Json(serde_json::json!({"a": [1, 2]})),
        ];
        let json = serde_json::to_string(&values).unwrap();
        let back: Vec<Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, values);
        assert_eq!(
            serde_json::to_string(&Value::Int(1)).unwrap(),
            r#"{"type":"int","value":1}"#
        );
    }

    #[test]
    fn non_finite_floats_round_trip() {
        for (v, text) in [
            (f64::NAN, r#"{"type":"float","value":"NaN"}"#),
            (f64::INFINITY, r#"{"type":"float","value":"Infinity"}"#),
            (f64::NEG_INFINITY, r#"{"type":"float","value":"-Infinity"}"#),
        ] {
            let json = serde_json::to_string(&Value::Float(v)).unwrap();
            assert_eq!(json, text);
            let Value::Float(back) = serde_json::from_str::<Value>(&json).unwrap() else {
                panic!("not a float: {json}");
            };
            assert!(back.to_bits() == v.to_bits() || (back.is_nan() && v.is_nan()));
        }
        assert_eq!(
            serde_json::to_string(&Value::Float(-0.5)).unwrap(),
            r#"{"type":"float","value":-0.5}"#
        );
        assert!(serde_json::from_str::<Value>(r#"{"type":"float","value":"abc"}"#).is_err());
        assert!(serde_json::from_str::<Value>(r#"{"type":"float","value":null}"#).is_err());
    }
}
