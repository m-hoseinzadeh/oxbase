//! Values of a data compare: Arrow cells as typed [`Value`]s for binding, the exact-match
//! form that decides "same" (module docs of [`crate::data`], step 4), and sample text.

use std::fmt::Write as _;
use std::num::FpCategory;

use arrow_array::Array;
use oxbase_types::{ColumnDef, DataType, Result, Value};

/// Row `row` of `array` as the [`Value`] that binds for `column` (when known): JSON, decimal
/// and boolean columns keep their own variants (PostgreSQL `numeric` and JSON arrive as text,
/// MySQL `tinyint(1)` as an integer).
pub(crate) fn cell(array: &dyn Array, row: usize, column: Option<&ColumnDef>) -> Result<Value> {
    Ok(typed(oxbase_driver::cell_value(array, row)?, column))
}

/// `value` in the variant that binds for `column`.
pub(crate) fn typed(value: Value, column: Option<&ColumnDef>) -> Value {
    let Some(column) = column else {
        return value;
    };
    match (value, &column.data_type) {
        (Value::Text(s), DataType::Json { .. }) => {
            serde_json::from_str(&s).map_or(Value::Text(s), Value::Json)
        }
        (Value::Text(s), DataType::Decimal { .. }) if normal_decimal(&s).is_some() => {
            Value::Decimal(s)
        }
        (Value::Int(i), DataType::Bool) => Value::Bool(i != 0),
        (value, _) => value,
    }
}

/// The exact-match form of `value` in a column of type `data_type`: equal forms are equal
/// values. Numbers of every variant share one form (canonical decimal text), so an integer
/// and the same decimal match (SQLite stores `10.00` as an integer); floats compare bit for
/// bit except `-0.0 == 0.0` and `NaN == NaN`; timestamps with a zone as instants; JSON (also
/// JSON text in a JSON column) as parsed values with sorted keys; NULL matches only NULL.
pub(crate) fn canon(value: &Value, data_type: Option<&DataType>) -> String {
    match value {
        Value::Null => "\u{0}null".to_owned(),
        Value::Bool(b) => format!("b:{b}"),
        Value::Int(i) => format!("n:{i}"),
        Value::UInt(u) => format!("n:{u}"),
        Value::Decimal(s) => {
            normal_decimal(s).map_or_else(|| format!("s:{s}"), |n| format!("n:{n}"))
        }
        Value::Float(f) => float_canon(*f),
        Value::Text(s) => match data_type {
            Some(DataType::Json { .. }) => serde_json::from_str::<serde_json::Value>(s)
                .map_or_else(|_| format!("t:{s}"), |j| format!("j:{}", json_canon(&j))),
            _ => format!("t:{s}"),
        },
        Value::Bytes(b) => {
            let mut out = String::with_capacity(2 + b.len() * 2);
            out.push_str("x:");
            for byte in b {
                let _ = write!(out, "{byte:02x}");
            }
            out
        }
        Value::Date(d) => format!("d:{d}"),
        Value::Time(t) => format!("tm:{t}"),
        Value::Timestamp(t) => format!("ts:{t}"),
        Value::TimestampTz(t) => format!("tz:{}", t.naive_utc()),
        Value::Uuid(u) => format!("u:{u}"),
        Value::Json(j) => format!("j:{}", json_canon(j)),
    }
}

fn float_canon(f: f64) -> String {
    if f.is_nan() {
        return "f:nan".to_owned();
    }
    if f.classify() == FpCategory::Zero {
        return "n:0".to_owned();
    }
    // Whole floats that are exact integers share the number form (2^53 bounds exactness).
    if f.fract().classify() == FpCategory::Zero && f.abs() < 9_007_199_254_740_992.0 {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a whole float below 2^53 converts exactly"
        )]
        let whole = f as i64;
        return format!("n:{whole}");
    }
    format!("f:{:016x}", f.to_bits())
}

/// `s` as a canonical decimal (`-12.5`, `0`, `100`): sign, no leading zeros, no trailing
/// fraction zeros, no `+`, `-0` as `0`. `None` when `s` is not a plain decimal number
/// (exponents, `NaN`, text).
pub(crate) fn normal_decimal(s: &str) -> Option<String> {
    let t = s.trim();
    let (negative, body) = match t.as_bytes().first()? {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if (int.is_empty() && frac.is_empty())
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let int = int.trim_start_matches('0');
    let frac = frac.trim_end_matches('0');
    let mut out = String::with_capacity(int.len() + frac.len() + 3);
    if negative && !(int.is_empty() && frac.is_empty()) {
        out.push('-');
    }
    out.push_str(if int.is_empty() { "0" } else { int });
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    Some(out)
}

/// JSON text with object keys sorted at every level (the workspace keeps insertion order).
fn json_canon(j: &serde_json::Value) -> String {
    let mut out = String::new();
    write_json(&mut out, j);
    out
}

fn write_json(out: &mut String, j: &serde_json::Value) {
    match j {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (pos, key) in keys.into_iter().enumerate() {
                if pos > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::Value::String(key.clone()).to_string());
                out.push(':');
                if let Some(v) = map.get(key) {
                    write_json(out, v);
                }
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (pos, v) in items.iter().enumerate() {
                if pos > 0 {
                    out.push(',');
                }
                write_json(out, v);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// `value` as sample text (`None` = NULL): bytes as `0x..`, timestamps with a zone in RFC 3339.
pub(crate) fn display(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDate, TimeZone};
    use pretty_assertions::assert_eq;

    fn same(a: &Value, b: &Value) -> bool {
        canon(a, None) == canon(b, None)
    }

    #[test]
    fn decimals_are_canonical() {
        assert_eq!(normal_decimal("0012.5000").as_deref(), Some("12.5"));
        assert_eq!(normal_decimal("-0.000").as_deref(), Some("0"));
        assert_eq!(normal_decimal("+7").as_deref(), Some("7"));
        assert_eq!(normal_decimal(".5").as_deref(), Some("0.5"));
        assert_eq!(
            normal_decimal("-1234567890123456789012345678.0123456789").as_deref(),
            Some("-1234567890123456789012345678.0123456789")
        );
        assert_eq!(normal_decimal("1e5"), None);
        assert_eq!(normal_decimal("NaN"), None);
        assert_eq!(normal_decimal(""), None);
        assert_eq!(normal_decimal("-"), None);
    }

    #[test]
    fn value_equality_rules() {
        // Numbers across variants.
        assert!(same(&Value::Int(10), &Value::Decimal("10.00".into())));
        assert!(same(&Value::Float(10.0), &Value::Int(10)));
        assert!(!same(
            &Value::Decimal("1.5".into()),
            &Value::Decimal("1.50001".into())
        ));
        // Floats: bit for bit, but -0 == 0 and NaN == NaN.
        assert!(same(&Value::Float(-0.0), &Value::Float(0.0)));
        assert!(same(&Value::Float(f64::NAN), &Value::Float(f64::NAN)));
        let next = f64::from_bits(0.1_f64.to_bits() + 1);
        assert!(!same(&Value::Float(0.1), &Value::Float(next)));
        // NULL is not the empty string, text is case-sensitive, bytes exact.
        assert!(!same(&Value::Null, &Value::Text(String::new())));
        assert!(same(&Value::Null, &Value::Null));
        assert!(!same(&Value::Text("a".into()), &Value::Text("A".into())));
        assert!(!same(
            &Value::Bytes(vec![0, 255]),
            &Value::Bytes(vec![0, 255, 16])
        ));
        assert!(!same(&Value::Text("1".into()), &Value::Int(1)));
        // Timestamps with a zone compare as instants.
        let ist = FixedOffset::east_opt(5 * 3600 + 1800).unwrap();
        let a = ist.with_ymd_and_hms(2026, 3, 1, 10, 0, 0).unwrap();
        let b = a.with_timezone(&FixedOffset::east_opt(0).unwrap());
        assert!(same(&Value::TimestampTz(a), &Value::TimestampTz(b)));
        let naive = NaiveDate::from_ymd_opt(2026, 3, 1)
            .unwrap()
            .and_hms_opt(4, 30, 0)
            .unwrap();
        assert!(!same(&Value::TimestampTz(a), &Value::Timestamp(naive)));
        // JSON as parsed values, key order ignored; JSON text in a JSON column too.
        let json = DataType::Json { binary: true };
        assert_eq!(
            canon(
                &Value::Text(r#"{"b": [1, 2], "a": null}"#.into()),
                Some(&json)
            ),
            canon(
                &Value::Json(serde_json::json!({"a": null, "b": [1, 2]})),
                Some(&json)
            )
        );
        assert_ne!(
            canon(&Value::Text(r#"{"a": 1}"#.into()), None),
            canon(&Value::Text(r#"{"a":1}"#.into()), None)
        );
    }

    #[test]
    fn typed_values_bind_for_their_column() {
        let mut c = ColumnDef::new(
            "amount",
            DataType::Decimal {
                precision: Some(38),
                scale: Some(10),
            },
        );
        assert_eq!(
            typed(Value::Text("12.50".into()), Some(&c)),
            Value::Decimal("12.50".into())
        );
        assert_eq!(
            typed(Value::Text("NaN".into()), Some(&c)),
            Value::Text("NaN".into())
        );
        c.data_type = DataType::Bool;
        assert_eq!(typed(Value::Int(1), Some(&c)), Value::Bool(true));
        c.data_type = DataType::Json { binary: false };
        assert_eq!(
            typed(Value::Text("[1]".into()), Some(&c)),
            Value::Json(serde_json::json!([1]))
        );
        assert_eq!(typed(Value::Int(1), None), Value::Int(1));
        assert_eq!(display(&Value::Null), None);
        assert_eq!(
            display(&Value::Bytes(vec![0, 255, 16])).as_deref(),
            Some("0x00ff10")
        );
    }
}
