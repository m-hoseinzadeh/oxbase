//! Values of a data compare: Arrow cells as typed [`Value`]s for binding, the exact-match
//! form that decides "same" (module docs of [`crate::data`], step 4), and sample text.

use std::fmt::Write as _;
use std::num::FpCategory;

use arrow_array::Array;
use oxbase_types::{ColumnDef, DataType, Result, Value};

/// Row `row` of `array` as the [`Value`] that binds for `column` (when known): decimal and
/// boolean columns keep their own variants (PostgreSQL `numeric` arrives as text, MySQL
/// `tinyint(1)` as an integer). JSON stays the server's text, so numbers beyond `f64` keep
/// every digit when they are written back (the writers bind text into JSON columns).
pub(crate) fn cell(array: &dyn Array, row: usize, column: Option<&ColumnDef>) -> Result<Value> {
    Ok(typed(oxbase_driver::cell_value(array, row)?, column))
}

/// `value` in the variant that binds for `column`.
pub(crate) fn typed(value: Value, column: Option<&ColumnDef>) -> Value {
    let Some(column) = column else {
        return value;
    };
    match (value, &column.data_type) {
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
/// bit except `-0.0 == 0.0` and `NaN == NaN`; timestamps with a zone as instants (also the
/// RFC 3339 text SQL Server's driver reads `datetimeoffset` as, which keeps the offset); JSON (also
/// JSON text in a JSON column) as parsed values with sorted keys and numbers as exact decimals
/// (no `f64` rounding: `123456789012345678901234` and `1.2345678901234568e23` differ, `1.0`
/// and `1` match); NULL matches only NULL.
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
            Some(DataType::Json { .. }) => {
                json_text_canon(s).map_or_else(|| format!("t:{s}"), |j| format!("j:{j}"))
            }
            Some(DataType::TimestampTz { .. }) => chrono::DateTime::parse_from_rfc3339(s.trim())
                .map_or_else(|_| format!("t:{s}"), |t| format!("tz:{}", t.naive_utc())),
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
        Value::Json(j) => {
            let text = j.to_string();
            json_text_canon(&text).map_or_else(|| format!("t:{text}"), |c| format!("j:{c}"))
        }
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

/// JSON text in a canonical form (`None` when it is not one JSON value): object keys sorted at
/// every level (duplicates kept, in order), insignificant whitespace dropped, strings with
/// their escapes normalized, numbers as exact decimals ([`json_number`]).
pub(crate) fn json_text_canon(text: &str) -> Option<String> {
    let mut p = JsonCanon {
        s: text.as_bytes(),
        text,
        at: 0,
    };
    let out = p.value(0)?;
    p.space();
    (p.at == p.s.len()).then_some(out)
}

/// Nesting limit of [`json_text_canon`] (deeper documents compare as text).
const JSON_DEPTH: usize = 512;

struct JsonCanon<'a> {
    s: &'a [u8],
    text: &'a str,
    at: usize,
}

impl JsonCanon<'_> {
    fn space(&mut self) {
        while self
            .s
            .get(self.at)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.at += 1;
        }
    }

    fn value(&mut self, depth: usize) -> Option<String> {
        if depth > JSON_DEPTH {
            return None;
        }
        self.space();
        match *self.s.get(self.at)? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string(),
            b't' => self.word("true"),
            b'f' => self.word("false"),
            b'n' => self.word("null"),
            _ => {
                let from = self.at;
                while self
                    .s
                    .get(self.at)
                    .is_some_and(|b| matches!(b, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'))
                {
                    self.at += 1;
                }
                json_number(self.text.get(from..self.at)?)
            }
        }
    }

    /// `{...}` with its members sorted by key (then value).
    fn object(&mut self, depth: usize) -> Option<String> {
        self.at += 1;
        let mut members: Vec<(String, String)> = Vec::new();
        self.space();
        if self.s.get(self.at) == Some(&b'}') {
            self.at += 1;
            return Some("{}".to_owned());
        }
        loop {
            self.space();
            let key = self.string()?;
            self.space();
            if self.s.get(self.at) != Some(&b':') {
                return None;
            }
            self.at += 1;
            let value = self.value(depth + 1)?;
            members.push((key, value));
            self.space();
            match self.s.get(self.at)? {
                b',' => self.at += 1,
                b'}' => {
                    self.at += 1;
                    break;
                }
                _ => return None,
            }
        }
        members.sort();
        let mut out = String::from("{");
        for (pos, (key, value)) in members.iter().enumerate() {
            if pos > 0 {
                out.push(',');
            }
            out.push_str(key);
            out.push(':');
            out.push_str(value);
        }
        out.push('}');
        Some(out)
    }

    /// `[...]` in order.
    fn array(&mut self, depth: usize) -> Option<String> {
        self.at += 1;
        let mut out = String::from("[");
        self.space();
        if self.s.get(self.at) == Some(&b']') {
            self.at += 1;
            out.push(']');
            return Some(out);
        }
        let mut first = true;
        loop {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(&self.value(depth + 1)?);
            self.space();
            match self.s.get(self.at)? {
                b',' => self.at += 1,
                b']' => {
                    self.at += 1;
                    break;
                }
                _ => return None,
            }
        }
        out.push(']');
        Some(out)
    }

    fn word(&mut self, word: &str) -> Option<String> {
        let end = self.at + word.len();
        (self.s.get(self.at..end)? == word.as_bytes()).then(|| {
            self.at = end;
            word.to_owned()
        })
    }

    /// A string token, re-encoded with normalized escapes.
    fn string(&mut self) -> Option<String> {
        if self.s.get(self.at) != Some(&b'"') {
            return None;
        }
        let from = self.at;
        self.at += 1;
        loop {
            match *self.s.get(self.at)? {
                b'\\' => self.at += 2,
                b'"' => {
                    self.at += 1;
                    break;
                }
                _ => self.at += 1,
            }
        }
        let decoded: String = serde_json::from_str(self.text.get(from..self.at)?).ok()?;
        serde_json::to_string(&decoded).ok()
    }
}

/// A JSON number token as an exact decimal: `[-]<digits>e<exponent>` with the digits stripped
/// of leading and trailing zeros (`0` for zero), so `1`, `1.0`, `10e-1` and `0.1e1` share a
/// form and no digit is rounded away. `None` when `token` is not a JSON number.
pub(crate) fn json_number(token: &str) -> Option<String> {
    let (negative, body) = match token.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, token),
    };
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(pos) => (&body[..pos], Some(&body[pos + 1..])),
        None => (body, None),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.is_empty()
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
        || (mantissa.contains('.') && frac.is_empty())
    {
        return None;
    }
    let exponent: i128 = match exponent {
        None => 0,
        Some(e) => {
            let digits = e.strip_prefix(['+', '-']).unwrap_or(e);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // Longer exponents than this are not numbers any server keeps.
            if digits.len() > 30 {
                return None;
            }
            e.parse().ok()?
        }
    };
    let digits = format!("{int}{frac}");
    let lead = digits.bytes().take_while(|b| *b == b'0').count();
    let digits = digits[lead..].trim_end_matches('0');
    if digits.is_empty() {
        return Some("0".to_owned());
    }
    // value = 0.<digits> * 10^point
    let point = i128::try_from(int.len()).ok()? - i128::try_from(lead).ok()? + exponent;
    Some(format!(
        "{}{digits}e{point}",
        if negative { "-" } else { "" }
    ))
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
    fn json_numbers_keep_every_digit() {
        let json = DataType::Json { binary: true };
        let form = |text: &str| canon(&Value::Text(text.into()), Some(&json));
        // Beyond f64: no rounding, so a lossy copy is a difference.
        assert_ne!(
            form(r#"{"n": 123456789012345678901234}"#),
            form(r#"{"n": 1.2345678901234568e23}"#)
        );
        assert_ne!(form("0.30000000000000000001"), form("0.3"));
        assert_ne!(form("[1, 2]"), form("[1, 2.0000000000000000001]"));
        // The same number spelled differently is the same value.
        assert_eq!(form("1"), form("1.0"));
        assert_eq!(form("100"), form("1e2"));
        assert_eq!(form("0.001"), form("1E-3"));
        assert_eq!(form("-0"), form("0.0"));
        assert_eq!(
            form(r#"{"b": "\u00e9", "a": [true, null, -1.50]}"#),
            form(r#"{"a":[true,null,-1.5],"b":"é"}"#)
        );
        assert_eq!(
            json_number("123456789012345678901234").as_deref(),
            Some("123456789012345678901234e24")
        );
        assert_eq!(json_number("-0.0012").as_deref(), Some("-12e-2"));
        assert_eq!(json_number("1."), None);
        assert_eq!(json_number("e5"), None);
        assert_eq!(json_number("1e"), None);
        // Not JSON: compared as text.
        assert_eq!(json_text_canon("{\"a\": }"), None);
        assert_eq!(json_text_canon("[1] x"), None);
        assert_eq!(json_text_canon(" [ ] ").as_deref(), Some("[]"));
        assert!(form("{bad").starts_with("t:"));
        // Value::Json goes through the same form.
        assert_eq!(
            canon(&Value::Json(serde_json::json!({"x": 1.5})), Some(&json)),
            form(r#"{ "x" : 1.50 }"#)
        );
    }

    #[test]
    fn offset_text_compares_as_an_instant() {
        let tz = DataType::TimestampTz { precision: Some(7) };
        let form = |text: &str| canon(&Value::Text(text.into()), Some(&tz));
        // The same instant at another offset (a restore writes +00:00) is the same value.
        assert_eq!(
            form("2026-01-01T10:00:00.0000000+05:30"),
            form("2026-01-01T04:30:00.0000000+00:00")
        );
        assert_eq!(
            form("2026-01-01T10:00:00+05:30"),
            canon(
                &Value::TimestampTz(
                    chrono::DateTime::parse_from_rfc3339("2026-01-01T04:30:00Z").unwrap()
                ),
                Some(&tz)
            )
        );
        assert_ne!(
            form("2026-01-01T10:00:00+05:30"),
            form("2026-01-01T11:00:00+05:30")
        );
        // Not a timestamp: compared as text.
        assert_eq!(form("soon"), "t:soon");
        assert_eq!(
            canon(&Value::Text("2026-01-01T10:00:00+05:30".into()), None),
            "t:2026-01-01T10:00:00+05:30"
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
        // JSON keeps the server's text (every digit of its numbers).
        c.data_type = DataType::Json { binary: false };
        assert_eq!(
            typed(Value::Text("[1]".into()), Some(&c)),
            Value::Text("[1]".into())
        );
        assert_eq!(typed(Value::Int(1), None), Value::Int(1));
        assert_eq!(display(&Value::Null), None);
        assert_eq!(
            display(&Value::Bytes(vec![0, 255, 16])).as_deref(),
            Some("0x00ff10")
        );
    }
}
