//! Values read back from two engines (or two containers of one engine), compared as
//! normalized text: the independent checks of the M8 cells (transfer read-back, data after a
//! sync apply or a restore, rows that must survive a structure sync).
//!
//! Every column has a [`Kind`] that says how its values compare:
//!
//! | Kind | Equal when |
//! |---|---|
//! | `Int`, `Decimal` | the same exact decimal (`9.50` = `9.5`, `-0` = `0`); a binary float on either side compares as a float (relative 1e-12) |
//! | `Float` | relative difference at most 1e-6 (a `real` stays a `real`) |
//! | `Bool` | `true` / `t` / `1` = `1`, `false` / `f` / `0` = `0` |
//! | `Text` | the same text |
//! | `Char` | the same text without trailing spaces (`char(n)` padding) |
//! | `Bytes` | the same bytes (lowercase hex; `\x` / `0x` text prefixes dropped) |
//! | `Date`, `Time`, `Timestamp` | the same value (`T` or space, any fraction digits) |
//! | `Instant` | the same instant in UTC (a value without an offset reads as UTC) |
//! | `Json` | the same JSON value (object keys in any order, `1.0` = `1`) |
//! | `ArrayJson` | the same elements, as a PostgreSQL array literal (`{1,"y,z"}`) or a JSON array |
//! | `Uuid` | the same UUID (text in any case, or 16 bytes) |

use std::fmt;

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime};
use oxbase_types::Value;

/// How the values of a column compare (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int,
    Decimal,
    Float,
    Bool,
    Text,
    Char,
    Bytes,
    Date,
    Time,
    Timestamp,
    Instant,
    Json,
    ArrayJson,
    Uuid,
}

/// A value in its normalized form.
#[derive(Debug, Clone, PartialEq)]
pub enum Norm {
    Null,
    Exact(String),
    /// A binary float (compared with a tolerance).
    Approx(f64),
}

impl Norm {
    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Null => None,
            Self::Exact(s) => s.parse().ok(),
            Self::Approx(f) => Some(*f),
        }
    }

    /// Whether two normalized values of a `kind` column are equal (module docs).
    pub fn matches(&self, other: &Self, kind: Kind) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Exact(a), Self::Exact(b)) => a == b,
            (Self::Null, _) | (_, Self::Null) => false,
            (left, right) => match (left.as_f64(), right.as_f64()) {
                (Some(x), Some(y)) => close(x, y, kind),
                _ => false,
            },
        }
    }
}

impl fmt::Display for Norm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::Exact(s) => write!(f, "{s:?}"),
            Self::Approx(x) => write!(f, "~{x:e}"),
        }
    }
}

fn close(x: f64, y: f64, kind: Kind) -> bool {
    let tolerance = if kind == Kind::Float { 1e-6 } else { 1e-12 };
    (x - y).abs() <= tolerance * x.abs().max(y.abs())
}

/// `value` of a `kind` column in its normalized form.
pub fn normalize(kind: Kind, value: &Value) -> Norm {
    if value.is_null() {
        return Norm::Null;
    }
    match kind {
        Kind::Int | Kind::Decimal => number(value),
        Kind::Float => match number(value) {
            Norm::Exact(s) => s.parse().map_or(Norm::Exact(s), Norm::Approx),
            other => other,
        },
        Kind::Bool => boolean(value),
        Kind::Text => Norm::Exact(text(value)),
        Kind::Char => Norm::Exact(text(value).trim_end_matches(' ').to_owned()),
        Kind::Bytes => bytes(value),
        Kind::Date => date(value),
        Kind::Time => time(value),
        Kind::Timestamp | Kind::Instant => timestamp(value),
        Kind::Json => json(value),
        Kind::ArrayJson => array(value),
        Kind::Uuid => uuid(value),
    }
}

fn text(value: &Value) -> String {
    match value {
        Value::Text(s) | Value::Decimal(s) => s.clone(),
        Value::Json(j) => j.to_string(),
        other => other.to_string(),
    }
}

/// The canonical form of a plain decimal text (`-001.2300` → `-1.23`), or `None` when `text`
/// is not one (an exponent, letters).
pub fn canonical_decimal(text: &str) -> Option<String> {
    let t = text.trim();
    let (negative, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (int_part, frac_part) = digits.split_once('.').unwrap_or((digits, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let int_part = int_part.trim_start_matches('0');
    let frac_part = frac_part.trim_end_matches('0');
    let mut out = String::new();
    if negative && !(int_part.is_empty() && frac_part.is_empty()) {
        out.push('-');
    }
    out.push_str(if int_part.is_empty() { "0" } else { int_part });
    if !frac_part.is_empty() {
        out.push('.');
        out.push_str(frac_part);
    }
    Some(out)
}

fn number(value: &Value) -> Norm {
    match value {
        Value::Int(i) => Norm::Exact(i.to_string()),
        Value::UInt(u) => Norm::Exact(u.to_string()),
        Value::Float(f) => Norm::Approx(*f),
        Value::Bool(b) => Norm::Exact(if *b { "1" } else { "0" }.to_owned()),
        Value::Decimal(s) | Value::Text(s) => canonical_decimal(s).map_or_else(
            || {
                s.trim()
                    .parse::<f64>()
                    .map_or_else(|_| Norm::Exact(s.clone()), Norm::Approx)
            },
            Norm::Exact,
        ),
        other => Norm::Exact(other.to_string()),
    }
}

fn boolean(value: &Value) -> Norm {
    let bit = match value {
        Value::Bool(b) => Some(*b),
        Value::Int(0) | Value::UInt(0) => Some(false),
        Value::Int(1) | Value::UInt(1) => Some(true),
        Value::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
            "t" | "true" | "1" | "yes" | "on" => Some(true),
            "f" | "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    };
    bit.map_or_else(
        || Norm::Exact(text(value)),
        |b| Norm::Exact(if b { "1" } else { "0" }.to_owned()),
    )
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn bytes(value: &Value) -> Norm {
    match value {
        Value::Bytes(b) => Norm::Exact(hex(b)),
        Value::Text(s) => {
            let t = s.trim();
            let t = t
                .strip_prefix("\\x")
                .or_else(|| t.strip_prefix("0x"))
                .or_else(|| t.strip_prefix("0X"))
                .unwrap_or(t);
            Norm::Exact(t.to_ascii_lowercase())
        }
        other => Norm::Exact(text(other)),
    }
}

const DATE: &str = "%Y-%m-%d";
const TIME: &str = "%H:%M:%S%.6f";
const DATETIME: &str = "%Y-%m-%d %H:%M:%S%.6f";

fn date(value: &Value) -> Norm {
    let parsed = match value {
        Value::Date(d) => Some(*d),
        Value::Timestamp(ts) => Some(ts.date()),
        Value::TimestampTz(ts) => Some(ts.date_naive()),
        Value::Text(s) => s
            .trim()
            .get(..10)
            .and_then(|d| NaiveDate::parse_from_str(d, DATE).ok()),
        _ => None,
    };
    parsed.map_or_else(
        || Norm::Exact(text(value)),
        |d| Norm::Exact(d.format(DATE).to_string()),
    )
}

fn time(value: &Value) -> Norm {
    let parsed = match value {
        Value::Time(t) => Some(*t),
        Value::Timestamp(ts) => Some(ts.time()),
        Value::Text(s) => NaiveTime::parse_from_str(s.trim(), "%H:%M:%S%.f").ok(),
        _ => None,
    };
    parsed.map_or_else(
        || Norm::Exact(text(value)),
        |t| Norm::Exact(t.format(TIME).to_string()),
    )
}

/// A date-time text as UTC: with an offset (`+05:30`, `+00`, `Z`, ` +05:30`) converted, without
/// one read as UTC already.
pub fn parse_instant(text: &str) -> Option<NaiveDateTime> {
    let t = text.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(t) {
        return Some(dt.naive_utc());
    }
    for format in [
        "%Y-%m-%d %H:%M:%S%.f%#z",
        "%Y-%m-%dT%H:%M:%S%.f%#z",
        "%Y-%m-%d %H:%M:%S%.f %#z",
    ] {
        if let Ok(dt) = DateTime::parse_from_str(t, format) {
            return Some(dt.naive_utc());
        }
    }
    ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"]
        .into_iter()
        .find_map(|format| NaiveDateTime::parse_from_str(t, format).ok())
}

fn timestamp(value: &Value) -> Norm {
    let parsed = match value {
        Value::Timestamp(ts) => Some(*ts),
        Value::TimestampTz(ts) => Some(ts.naive_utc()),
        Value::Date(d) => d.and_hms_opt(0, 0, 0),
        Value::Text(s) => parse_instant(s),
        _ => None,
    };
    parsed.map_or_else(
        || Norm::Exact(text(value)),
        |ts| Norm::Exact(ts.format(DATETIME).to_string()),
    )
}

/// Compact JSON with object keys sorted and numbers in their canonical decimal form.
pub fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let inner: Vec<String> = entries
                .into_iter()
                .map(|(k, v)| {
                    format!(
                        "{}:{}",
                        serde_json::Value::String(k.clone()),
                        canonical_json(v)
                    )
                })
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        serde_json::Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", inner.join(","))
        }
        serde_json::Value::Number(n) => {
            let s = n.to_string();
            canonical_decimal(&s).unwrap_or(s)
        }
        other => other.to_string(),
    }
}

fn json(value: &Value) -> Norm {
    match value {
        Value::Json(j) => Norm::Exact(canonical_json(j)),
        Value::Text(s) => Norm::Exact(
            serde_json::from_str::<serde_json::Value>(s)
                .map_or_else(|_| s.clone(), |j| canonical_json(&j)),
        ),
        other => Norm::Exact(text(other)),
    }
}

/// The elements of a one-dimensional PostgreSQL array literal (`{1,NULL,"y,z"}`); `None` for
/// anything else (nested arrays included).
pub fn parse_pg_array(text: &str) -> Option<Vec<Option<String>>> {
    let inner = text.trim().strip_prefix('{')?.strip_suffix('}')?;
    let mut out = Vec::new();
    if inner.trim().is_empty() {
        return Some(out);
    }
    let mut chars = inner.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        match chars.peek() {
            Some('{') => return None,
            Some('"') => {
                chars.next();
                let mut element = String::new();
                loop {
                    match chars.next()? {
                        '\\' => element.push(chars.next()?),
                        '"' => break,
                        c => element.push(c),
                    }
                }
                out.push(Some(element));
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
            }
            _ => {
                let mut element = String::new();
                while let Some(&c) = chars.peek() {
                    if c == ',' {
                        break;
                    }
                    element.push(c);
                    chars.next();
                }
                let element = element.trim();
                if element.eq_ignore_ascii_case("null") {
                    out.push(None);
                } else {
                    out.push(Some(element.to_owned()));
                }
            }
        }
        match chars.next() {
            None => return Some(out),
            Some(',') => {}
            Some(_) => return None,
        }
    }
}

fn json_elements(value: &serde_json::Value) -> Option<Vec<Option<String>>> {
    let serde_json::Value::Array(items) = value else {
        return None;
    };
    Some(
        items
            .iter()
            .map(|item| match item {
                serde_json::Value::Null => None,
                serde_json::Value::String(s) => Some(s.clone()),
                other => Some(canonical_json(other)),
            })
            .collect(),
    )
}

fn array(value: &Value) -> Norm {
    let elements = match value {
        Value::Json(j) => json_elements(j),
        Value::Text(s) if s.trim_start().starts_with('{') => parse_pg_array(s),
        Value::Text(s) => serde_json::from_str::<serde_json::Value>(s)
            .ok()
            .and_then(|j| json_elements(&j)),
        _ => None,
    };
    elements.map_or_else(
        || Norm::Exact(text(value)),
        |items| {
            let list: Vec<serde_json::Value> = items
                .into_iter()
                .map(|e| e.map_or(serde_json::Value::Null, serde_json::Value::String))
                .collect();
            Norm::Exact(serde_json::Value::Array(list).to_string())
        },
    )
}

fn uuid(value: &Value) -> Norm {
    match value {
        Value::Uuid(u) => Norm::Exact(u.to_string()),
        Value::Bytes(b) if b.len() == 16 => {
            let h = hex(b);
            Norm::Exact(format!(
                "{}-{}-{}-{}-{}",
                &h[..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..]
            ))
        }
        other => Norm::Exact(text(other).trim().to_ascii_lowercase()),
    }
}

/// The columns of `want` and `got` (rows in the same key order) that differ: one line per
/// column, with the first differing row (`key` = the first column) and how many differ. A
/// different row count is reported first.
pub fn differences(columns: &[(&str, Kind)], want: &[Vec<Value>], got: &[Vec<Value>]) -> Vec<Diff> {
    let mut out: Vec<Diff> = Vec::new();
    if want.len() != got.len() {
        out.push(Diff {
            column: None,
            detail: format!("{} rows, expected {}", got.len(), want.len()),
        });
    }
    for (index, (name, kind)) in columns.iter().enumerate() {
        let mut first = None;
        let mut count = 0usize;
        for (want_row, got_row) in want.iter().zip(got) {
            let (Some(want_value), Some(got_value)) = (want_row.get(index), got_row.get(index))
            else {
                continue;
            };
            let expected = normalize(*kind, want_value);
            let read = normalize(*kind, got_value);
            if !expected.matches(&read, *kind) {
                count += 1;
                if first.is_none() {
                    let key = want_row
                        .first()
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    first = Some(format!("row {key}: expected {expected}, read {read}"));
                }
            }
        }
        if let Some(first) = first {
            out.push(Diff {
                column: Some((*name).to_owned()),
                detail: format!("{count} value(s) differ, first {first}"),
            });
        }
    }
    out
}

/// One difference [`differences`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    /// `None`: the row counts differ.
    pub column: Option<String>,
    pub detail: String,
}

impl fmt::Display for Diff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.column {
            Some(c) => write!(f, "{c}: {}", self.detail),
            None => f.write_str(&self.detail),
        }
    }
}

/// The columns of the `m8_data` pairs (`[m8] data_tables`), in table order.
pub const M8_DATA: [(&str, Kind); 6] = [
    ("id", Kind::Int),
    ("name", Kind::Text),
    ("amount", Kind::Decimal),
    ("payload", Kind::Bytes),
    ("happened_at", Kind::Instant),
    ("note", Kind::Text),
];

/// The columns of `m8_composite` (key first).
pub const M8_COMPOSITE: [(&str, Kind); 3] = [
    ("tenant", Kind::Int),
    ("code", Kind::Text),
    ("qty", Kind::Int),
];

/// The columns of `customers` both sides of the pair keep through a structure sync.
pub const CUSTOMERS_KEPT: [(&str, Kind); 3] = [
    ("id", Kind::Int),
    ("name", Kind::Text),
    ("email", Kind::Text),
];

/// The columns of `products` both sides keep (types and nullability change).
pub const PRODUCTS_KEPT: [(&str, Kind); 5] = [
    ("id", Kind::Int),
    ("name", Kind::Text),
    ("sku", Kind::Text),
    ("price", Kind::Decimal),
    ("stock", Kind::Int),
];

/// The columns of `orders` (both sides).
pub const ORDERS: [(&str, Kind); 4] = [
    ("id", Kind::Int),
    ("customer_id", Kind::Int),
    ("created_at", Kind::Timestamp),
    ("total", Kind::Decimal),
];

/// How the `[m8.transfer]` `m8_types` columns compare after PostgreSQL → MySQL (the
/// `[m8.transfer.expected_mysql]` comment: instants in UTC, interval / uuid / arrays as their
/// PostgreSQL text or JSON form, JSON as an equal value).
pub fn types_kind(column: &str) -> Kind {
    match column {
        "id" | "c_int2" | "c_int4" | "c_int8" => Kind::Int,
        "c_bool" => Kind::Bool,
        "c_numeric" | "c_numeric_free" => Kind::Decimal,
        "c_real" | "c_double" => Kind::Float,
        "c_char" => Kind::Char,
        "c_bytea" => Kind::Bytes,
        "c_date" => Kind::Date,
        "c_time" => Kind::Time,
        "c_timestamp" => Kind::Timestamp,
        "c_timestamptz" => Kind::Instant,
        "c_uuid" => Kind::Uuid,
        "c_json" | "c_jsonb" => Kind::Json,
        "c_int_array" | "c_text_array" => Kind::ArrayJson,
        _ => Kind::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn n(kind: Kind, v: &Value) -> Norm {
        normalize(kind, v)
    }

    fn same(kind: Kind, left: &Value, right: &Value) -> bool {
        n(kind, left).matches(&n(kind, right), kind)
    }

    fn text(s: &str) -> Value {
        Value::Text(s.to_owned())
    }

    #[test]
    fn decimals_compare_exactly() {
        assert_eq!(canonical_decimal("-001.2300"), Some("-1.23".to_owned()));
        assert_eq!(canonical_decimal("0.000"), Some("0".to_owned()));
        assert_eq!(canonical_decimal("-0"), Some("0".to_owned()));
        assert_eq!(canonical_decimal("+5."), Some("5".to_owned()));
        assert_eq!(canonical_decimal("1e5"), None);
        assert_eq!(canonical_decimal("."), None);
        let big = "1234567890123456789012345678.0123456789";
        assert!(same(
            Kind::Decimal,
            &Value::Decimal(big.to_owned()),
            &Value::Text(format!("{big}00000"))
        ));
        // One digit off in the 38th place is a difference.
        assert!(!same(
            Kind::Decimal,
            &Value::Decimal(big.to_owned()),
            &Value::Decimal("1234567890123456789012345678.0123456788".to_owned())
        ));
        // A float on one side (SQLite REAL) compares as a float.
        assert!(same(
            Kind::Decimal,
            &Value::Float("1.2345678901234568e27".parse().unwrap()),
            &Value::Decimal("1234567890123456800000000000".to_owned())
        ));
        assert!(same(Kind::Decimal, &Value::Int(5), &Value::Float(5.0)));
        assert!(same(
            Kind::Decimal,
            &text("9.50"),
            &Value::Decimal("9.5".to_owned())
        ));
        assert!(!same(Kind::Decimal, &Value::Null, &Value::Int(0)));
        assert!(same(Kind::Float, &Value::Float(1.5), &text("1.5")));
        assert!(same(Kind::Float, &Value::Float(-1e300), &text("-1e+300")));
    }

    #[test]
    fn instants_compare_in_utc() {
        let utc = "2026-01-01 04:30:00.000000";
        for raw in [
            "2026-01-01T10:00:00+05:30",
            "2026-01-01 10:00:00+05:30",
            "2026-01-01 04:30:00+00",
            "2026-01-01 10:00:00.0000000 +05:30",
            "2026-01-01T04:30:00Z",
            "2026-01-01 04:30:00",
        ] {
            assert_eq!(
                n(Kind::Instant, &text(raw)),
                Norm::Exact(utc.to_owned()),
                "{raw}"
            );
        }
        let at = NaiveDate::from_ymd_opt(2026, 1, 1)
            .and_then(|d| d.and_hms_opt(4, 30, 0))
            .unwrap();
        assert_eq!(
            n(Kind::Instant, &Value::Timestamp(at)),
            Norm::Exact(utc.to_owned())
        );
        assert_eq!(
            n(
                Kind::Instant,
                &Value::TimestampTz(at.and_utc().fixed_offset())
            ),
            Norm::Exact(utc.to_owned())
        );
        assert!(!same(
            Kind::Instant,
            &text("2026-01-01T10:00:00+05:30"),
            &text("2026-01-01T10:00:00+00:00")
        ));
        assert!(same(
            Kind::Time,
            &text("23:59:59.999999"),
            &Value::Time(NaiveTime::from_hms_micro_opt(23, 59, 59, 999_999).unwrap())
        ));
        assert!(same(
            Kind::Timestamp,
            &text("1970-01-01T00:00:00"),
            &text("1970-01-01 00:00:00.000000")
        ));
        assert!(same(
            Kind::Date,
            &text("2026-03-01"),
            &Value::Date(NaiveDate::from_ymd_opt(2026, 3, 1).unwrap())
        ));
    }

    #[test]
    fn bytes_bools_chars_and_uuids() {
        assert!(same(
            Kind::Bytes,
            &Value::Bytes(vec![0, 0xff, 0x10]),
            &text("\\x00FF10")
        ));
        assert!(!same(Kind::Bytes, &Value::Bytes(Vec::new()), &Value::Null));
        assert!(same(Kind::Bytes, &Value::Bytes(Vec::new()), &text("")));
        assert!(same(Kind::Bool, &Value::Bool(true), &Value::Int(1)));
        assert!(same(Kind::Bool, &text("f"), &Value::Int(0)));
        assert!(!same(Kind::Bool, &Value::Bool(true), &Value::Int(0)));
        assert!(same(Kind::Char, &text("abc       "), &text("abc")));
        assert!(!same(Kind::Text, &text("abc "), &text("abc")));
        assert!(same(
            Kind::Uuid,
            &Value::Bytes((0u8..16).collect()),
            &text("00010203-0405-0607-0809-0A0B0C0D0E0F")
        ));
    }

    #[test]
    fn json_and_arrays() {
        assert!(same(
            Kind::Json,
            &Value::Text(r#"{"b": [1, 2], "a": null}"#.to_owned()),
            &Value::Json(serde_json::json!({"a": null, "b": [1.0, 2]}))
        ));
        assert!(!same(
            Kind::Json,
            &Value::Text(r#"{"b": [2, 1]}"#.to_owned()),
            &Value::Text(r#"{"b": [1, 2]}"#.to_owned())
        ));
        assert_eq!(
            parse_pg_array(r#"{x,"y,z", NULL ,"q\"uote"}"#),
            Some(vec![
                Some("x".to_owned()),
                Some("y,z".to_owned()),
                None,
                Some("q\"uote".to_owned())
            ])
        );
        assert_eq!(parse_pg_array("{}"), Some(Vec::new()));
        assert_eq!(parse_pg_array("{{1},{2}}"), None);
        assert!(same(Kind::ArrayJson, &text("{1,2,3}"), &text("[1, 2, 3]")));
        assert!(same(
            Kind::ArrayJson,
            &Value::Text(r#"{x,"y,z"}"#.to_owned()),
            &Value::Json(serde_json::json!(["x", "y,z"]))
        ));
        assert!(same(Kind::ArrayJson, &text("{}"), &text("[]")));
        assert!(!same(Kind::ArrayJson, &text("{1,2}"), &text("[2, 1]")));
    }

    #[test]
    fn differences_name_the_column_and_first_row() {
        let cols = [("id", Kind::Int), ("amount", Kind::Decimal)];
        let want = vec![
            vec![Value::Int(1), Value::Decimal("1.50".to_owned())],
            vec![Value::Int(2), Value::Decimal("2".to_owned())],
        ];
        let got = vec![
            vec![Value::Int(1), text("1.5")],
            vec![Value::Int(2), Value::Float(2.000_001)],
        ];
        let diffs = differences(&cols, &want, &got);
        assert_eq!(diffs.len(), 1, "{diffs:?}");
        assert_eq!(diffs[0].column.as_deref(), Some("amount"));
        assert!(
            diffs[0]
                .detail
                .starts_with("1 value(s) differ, first row 2"),
            "{}",
            diffs[0]
        );
        assert_eq!(differences(&cols, &want, &want), Vec::<Diff>::new());
        let short = differences(&cols, &want, &got[..1]);
        assert_eq!(short[0].column, None);
    }

    #[test]
    fn transfer_type_kinds() {
        assert_eq!(types_kind("c_timestamptz"), Kind::Instant);
        assert_eq!(types_kind("c_interval"), Kind::Text);
        assert_eq!(types_kind("c_text_array"), Kind::ArrayJson);
    }
}
