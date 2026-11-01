//! Text → typed values with [`ValueFormats`] (every reader), and the number scanner type
//! inference shares (`crate::infer`).
//!
//! Numbers: an optional sign, digits with the decimal separator, an optional exponent; the
//! thousands separator (when set) is accepted only in valid groups of three (`1.234.567`), so
//! `1.5` with `.` as the thousands separator is not `15`. Dates and times: the formats of
//! [`ValueFormats`], else ISO 8601 (`YYYY-MM-DD`, `HH:MM[:SS[.f]]`, a space or `T` between
//! date and time, `Z` / `+HH[:MM]` offsets).

use std::borrow::Cow;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};

use crate::ValueFormats;

/// The largest exponent accepted when writing a number as exact decimal text.
const MAX_EXPONENT: i64 = 4_096;

/// A number split into its parts (digits as written, grouping removed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NumberParts<'a> {
    pub negative: bool,
    /// Integer digits (may be empty: `.5`).
    pub int: Cow<'a, str>,
    /// Fraction digits (may be empty).
    pub frac: &'a str,
    /// A decimal separator was present.
    pub point: bool,
    /// Exponent (`1e3`).
    pub exponent: Option<i64>,
}

impl NumberParts<'_> {
    /// Integer digits without leading zeros (`0` and `007` count 0 and 1 digits).
    pub fn significant_int_digits(&self) -> usize {
        self.int.trim_start_matches('0').len()
    }

    /// `true` for `007` (but not `0` or `0.5`): identifiers, not numbers.
    pub fn has_leading_zero(&self) -> bool {
        self.int.len() > 1 && self.int.starts_with('0')
    }
}

/// `NaN`, `inf`, `infinity` with an optional sign (any case).
pub(crate) fn is_special_float(text: &str) -> bool {
    let t = text.trim();
    let t = t
        .strip_prefix('-')
        .or_else(|| t.strip_prefix('+'))
        .unwrap_or(t);
    ["nan", "inf", "infinity"]
        .iter()
        .any(|s| t.eq_ignore_ascii_case(s))
}

/// Splits `text` (trimmed) into number parts, or `None` when it is not a plain number.
pub(crate) fn scan_number(
    text: &str,
    decimal: char,
    thousands: Option<char>,
) -> Option<NumberParts<'_>> {
    let t = text.trim();
    let (negative, rest) = match t.as_bytes().first()? {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    let (mantissa, exponent) = match rest.find(['e', 'E']) {
        Some(at) => {
            let exp = &rest[at + 1..];
            let digits = exp
                .strip_prefix('-')
                .or_else(|| exp.strip_prefix('+'))
                .unwrap_or(exp);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // Absurd exponents are not plain numbers (and would not fit any column).
            let value: i64 = exp.parse().ok()?;
            (&rest[..at], Some(value))
        }
        None => (rest, None),
    };
    let (int_raw, frac, point) = match mantissa.find(decimal) {
        Some(at) => (&mantissa[..at], &mantissa[at + decimal.len_utf8()..], true),
        None => (mantissa, "", false),
    };
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let int = match thousands {
        Some(sep) if int_raw.contains(sep) => Cow::Owned(ungroup(int_raw, sep)?),
        _ => {
            if !int_raw.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Cow::Borrowed(int_raw)
        }
    };
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    Some(NumberParts {
        negative,
        int,
        frac,
        point,
        exponent,
    })
}

/// `1.234.567` → `1234567` for `.`; `None` unless the groups are 1-3 digits then 3 each.
fn ungroup(int: &str, sep: char) -> Option<String> {
    let mut out = String::with_capacity(int.len());
    for (i, group) in int.split(sep).enumerate() {
        let ok = group.bytes().all(|b| b.is_ascii_digit())
            && if i == 0 {
                (1..=3).contains(&group.len())
            } else {
                group.len() == 3
            };
        if !ok {
            return None;
        }
        out.push_str(group);
    }
    Some(out)
}

/// The number as exact decimal text (`-1234.50`, `1000` for `1e3`): leading zeros removed,
/// fraction digits kept as written, no exponent. `None` for anything else.
pub(crate) fn canonical_decimal(text: &str, formats: &ValueFormats) -> Option<String> {
    let parts = scan_number(text, formats.decimal_separator, formats.thousands_separator)?;
    canonical_from_parts(&parts)
}

/// [`canonical_decimal`] of already split parts.
pub(crate) fn canonical_from_parts(parts: &NumberParts<'_>) -> Option<String> {
    let mut digits = String::with_capacity(parts.int.len() + parts.frac.len());
    digits.push_str(&parts.int);
    digits.push_str(parts.frac);
    // Position of the decimal point within `digits`.
    let mut point = i64::try_from(parts.int.len()).ok()?;
    if let Some(exp) = parts.exponent {
        if exp.abs() > MAX_EXPONENT {
            return None;
        }
        point += exp;
    }
    let len = i64::try_from(digits.len()).ok()?;
    let (int_part, frac_part) = if point <= 0 {
        let zeros = usize::try_from(-point).ok()?;
        (String::new(), format!("{}{digits}", "0".repeat(zeros)))
    } else if point >= len {
        let zeros = usize::try_from(point - len).ok()?;
        (format!("{digits}{}", "0".repeat(zeros)), String::new())
    } else {
        let at = usize::try_from(point).ok()?;
        (digits[..at].to_owned(), digits[at..].to_owned())
    };
    let int_part = int_part.trim_start_matches('0');
    let int_part = if int_part.is_empty() { "0" } else { int_part };
    let zero = int_part == "0" && frac_part.bytes().all(|b| b == b'0');
    let mut out = String::with_capacity(int_part.len() + frac_part.len() + 2);
    if parts.negative && !zero {
        out.push('-');
    }
    out.push_str(int_part);
    if !frac_part.is_empty() {
        out.push('.');
        out.push_str(&frac_part);
    }
    Some(out)
}

/// An integer (grouping and a zero fraction allowed: `1.234`, `12.0`, `1e3`).
pub(crate) fn parse_i64(text: &str, formats: &ValueFormats) -> Option<i64> {
    let t = text.trim();
    if let Ok(v) = t.parse::<i64>() {
        return Some(v);
    }
    integral_text(t, formats)?.parse().ok()
}

/// An unsigned integer (see [`parse_i64`]).
pub(crate) fn parse_u64(text: &str, formats: &ValueFormats) -> Option<u64> {
    let t = text.trim();
    if let Ok(v) = t.parse::<u64>() {
        return Some(v);
    }
    integral_text(t, formats)?.parse().ok()
}

/// The canonical decimal text of `text` when it has no fraction.
fn integral_text(text: &str, formats: &ValueFormats) -> Option<String> {
    let canonical = canonical_decimal(text, formats)?;
    match canonical.split_once('.') {
        Some((int, frac)) if frac.bytes().all(|b| b == b'0') => Some(int.to_owned()),
        Some(_) => None,
        None => Some(canonical),
    }
}

/// A binary floating-point number (also `NaN`, `inf`, `Infinity`).
pub(crate) fn parse_f64(text: &str, formats: &ValueFormats) -> Option<f64> {
    let t = text.trim();
    if formats.decimal_separator == '.'
        && formats.thousands_separator.is_none()
        && let Ok(v) = t.parse::<f64>()
    {
        return Some(v);
    }
    if is_special_float(t) {
        return t.parse().ok();
    }
    let parts = scan_number(t, formats.decimal_separator, formats.thousands_separator)?;
    let mut plain = String::with_capacity(t.len() + 2);
    if parts.negative {
        plain.push('-');
    }
    plain.push_str(if parts.int.is_empty() {
        "0"
    } else {
        &parts.int
    });
    if !parts.frac.is_empty() {
        plain.push('.');
        plain.push_str(parts.frac);
    }
    if let Some(exp) = parts.exponent {
        plain.push('e');
        plain.push_str(&exp.to_string());
    }
    plain.parse().ok()
}

/// `true` / `false` per the formats' token lists (any case).
pub(crate) fn parse_bool(text: &str, formats: &ValueFormats) -> Option<bool> {
    let t = text.trim();
    if formats
        .true_values
        .iter()
        .any(|v| v.eq_ignore_ascii_case(t))
    {
        Some(true)
    } else if formats
        .false_values
        .iter()
        .any(|v| v.eq_ignore_ascii_case(t))
    {
        Some(false)
    } else {
        None
    }
}

const ISO_TIMESTAMPS: [&str; 4] = [
    "%Y-%m-%d %H:%M:%S%.f",
    "%Y-%m-%dT%H:%M:%S%.f",
    "%Y-%m-%d %H:%M",
    "%Y-%m-%dT%H:%M",
];

const ISO_TIMESTAMPS_TZ: [&str; 4] = [
    "%Y-%m-%d %H:%M:%S%.f%#z",
    "%Y-%m-%dT%H:%M:%S%.f%#z",
    "%Y-%m-%d %H:%M%#z",
    "%Y-%m-%dT%H:%M%#z",
];

const ISO_TIMES: [&str; 2] = ["%H:%M:%S%.f", "%H:%M"];

/// A date (`date_format`, else `YYYY-MM-DD`).
pub(crate) fn parse_date(text: &str, formats: &ValueFormats) -> Option<NaiveDate> {
    let t = text.trim();
    match &formats.date_format {
        Some(format) => NaiveDate::parse_from_str(t, format).ok(),
        None => NaiveDate::parse_from_str(t, "%Y-%m-%d").ok(),
    }
}

/// A time of day (`time_format`, else `HH:MM[:SS[.f]]`).
pub(crate) fn parse_time(text: &str, formats: &ValueFormats) -> Option<NaiveTime> {
    let t = text.trim();
    match &formats.time_format {
        Some(format) => NaiveTime::parse_from_str(t, format).ok(),
        None => ISO_TIMES
            .iter()
            .find_map(|f| NaiveTime::parse_from_str(t, f).ok()),
    }
}

/// A date and time without an offset (`timestamp_format`, else ISO 8601). Text with an offset
/// is not one.
pub(crate) fn parse_naive_timestamp(text: &str, formats: &ValueFormats) -> Option<NaiveDateTime> {
    let t = text.trim();
    match &formats.timestamp_format {
        Some(format) => NaiveDateTime::parse_from_str(t, format).ok(),
        None => ISO_TIMESTAMPS
            .iter()
            .find_map(|f| NaiveDateTime::parse_from_str(t, f).ok()),
    }
}

/// A date and time with an offset (`timestamp_format` with `%z`, else ISO 8601 with `Z` or
/// `+HH[:MM]`).
pub(crate) fn parse_timestamp_tz(
    text: &str,
    formats: &ValueFormats,
) -> Option<DateTime<FixedOffset>> {
    let t = text.trim();
    match &formats.timestamp_format {
        Some(format) => DateTime::parse_from_str(t, format).ok(),
        None => ISO_TIMESTAMPS_TZ
            .iter()
            .find_map(|f| DateTime::parse_from_str(t, f).ok()),
    }
}

/// A timestamp column value: a date and time without offset, or a date (midnight).
pub(crate) fn parse_timestamp(text: &str, formats: &ValueFormats) -> Option<NaiveDateTime> {
    parse_naive_timestamp(text, formats)
        .or_else(|| parse_date(text, formats).map(|d| d.and_time(NaiveTime::MIN)))
}

/// A UUID: 32 hex digits, hyphenated canonically or not at all, optionally in braces.
pub(crate) fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    let t = text.trim();
    let t = t
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(t);
    let hex: String = match t.len() {
        36 => {
            let bytes = t.as_bytes();
            if [8, 13, 18, 23].iter().any(|&i| bytes[i] != b'-') {
                return None;
            }
            t.chars().filter(|&c| c != '-').collect()
        }
        32 => t.to_owned(),
        _ => return None,
    };
    let bytes = decode_hex(&hex)?;
    bytes.try_into().ok()
}

/// `true` for the canonical hyphenated form (what inference accepts).
pub(crate) fn is_canonical_uuid(text: &str) -> bool {
    text.len() == 36 && !text.starts_with('{') && parse_uuid(text).is_some()
}

/// Binary from text: `0x` / `\x` followed by hex digits, else standard base64 (padded).
pub(crate) fn parse_binary(text: &str) -> Option<Vec<u8>> {
    let t = text.trim();
    if let Some(hex) = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .or_else(|| t.strip_prefix("\\x"))
    {
        return decode_hex(hex);
    }
    decode_base64(t)
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    let (pairs, rest) = hex.as_bytes().as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|&[high, low]| Some((hex_value(high)? << 4) | hex_value(low)?))
        .collect()
}

fn base64_value(b: u8) -> Option<u32> {
    let v = match b {
        b'A'..=b'Z' => b - b'A',
        b'a'..=b'z' => b - b'a' + 26,
        b'0'..=b'9' => b - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    };
    Some(u32::from(v))
}

fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let (quads, rest) = text.as_bytes().as_chunks::<4>();
    if !rest.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(quads.len() * 3);
    for (index, chunk) in quads.iter().enumerate() {
        let last = index + 1 == quads.len();
        let pad = chunk.iter().rev().take_while(|&&byte| byte == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut bits = 0_u32;
        for &byte in &chunk[..4 - pad] {
            bits = (bits << 6) | base64_value(byte)?;
        }
        bits <<= 6 * u32::try_from(pad).ok()?;
        let [_, first, second, third] = bits.to_be_bytes();
        out.push(first);
        if pad < 2 {
            out.push(second);
        }
        if pad < 1 {
            out.push(third);
        }
    }
    Some(out)
}

/// `true` when `text` is one JSON document.
pub(crate) fn is_json(text: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comma() -> ValueFormats {
        ValueFormats {
            decimal_separator: ',',
            thousands_separator: Some('.'),
            ..ValueFormats::default()
        }
    }

    #[test]
    fn decimals_are_exact_text() {
        let f = ValueFormats::default();
        let d = |t: &str| canonical_decimal(t, &f);
        assert_eq!(d("12.50").as_deref(), Some("12.50"));
        assert_eq!(d("-0.75").as_deref(), Some("-0.75"));
        assert_eq!(d("+007.5").as_deref(), Some("7.5"));
        assert_eq!(d(".5").as_deref(), Some("0.5"));
        assert_eq!(d("1e3").as_deref(), Some("1000"));
        assert_eq!(d("1.50e1").as_deref(), Some("15.0"));
        assert_eq!(d("12.5E-3").as_deref(), Some("0.0125"));
        assert_eq!(d("-0.00").as_deref(), Some("0.00"));
        assert_eq!(d("9007199254740993").as_deref(), Some("9007199254740993"));
        assert_eq!(d("1e99999"), None);
        assert_eq!(d("1.2.3"), None);
        assert_eq!(d("abc"), None);
        assert_eq!(d("-"), None);
        assert_eq!(d(""), None);
        assert_eq!(
            canonical_decimal("1.234,56", &comma()).as_deref(),
            Some("1234.56")
        );
        assert_eq!(
            canonical_decimal("-7,25", &comma()).as_deref(),
            Some("-7.25")
        );
        // Grouping must be in threes.
        assert_eq!(canonical_decimal("1.5", &comma()), None);
        assert_eq!(canonical_decimal("12.34,5", &comma()), None);
    }

    #[test]
    fn integers_and_floats() {
        let f = ValueFormats::default();
        assert_eq!(parse_i64(" 42 ", &f), Some(42));
        assert_eq!(parse_i64("-9223372036854775808", &f), Some(i64::MIN));
        assert_eq!(parse_i64("9223372036854775808", &f), None);
        assert_eq!(parse_i64("12.0", &f), Some(12));
        assert_eq!(parse_i64("1e3", &f), Some(1000));
        assert_eq!(parse_i64("12.5", &f), None);
        assert_eq!(parse_i64("abc", &f), None);
        assert_eq!(parse_i64("1.234", &comma()), Some(1234));
        assert_eq!(parse_u64("18446744073709551615", &f), Some(u64::MAX));
        assert_eq!(parse_u64("-1", &f), None);
        assert_eq!(parse_f64("1.5", &f), Some(1.5));
        assert_eq!(parse_f64("1,5", &comma()), Some(1.5));
        assert_eq!(parse_f64("1.234,5e1", &comma()), Some(12_345.0));
        assert!(parse_f64("NaN", &f).is_some_and(f64::is_nan));
        assert_eq!(parse_f64("-Infinity", &comma()), Some(f64::NEG_INFINITY));
        assert_eq!(parse_f64("x", &f), None);
        assert!(is_special_float("inf"));
        assert!(!is_special_float("info"));
    }

    #[test]
    fn number_parts() {
        let p = scan_number("007", '.', None).unwrap();
        assert!(p.has_leading_zero());
        assert_eq!(p.significant_int_digits(), 1);
        let p = scan_number("0.75", '.', None).unwrap();
        assert!(!p.has_leading_zero());
        assert_eq!(p.significant_int_digits(), 0);
        assert_eq!(p.frac, "75");
        assert!(p.point);
        assert_eq!(scan_number("1e5", '.', None).unwrap().exponent, Some(5));
        assert_eq!(scan_number("1e", '.', None), None);
        assert_eq!(scan_number("1,5", '.', None), None);
    }

    #[test]
    fn booleans() {
        let f = ValueFormats::default();
        assert_eq!(parse_bool("TRUE", &f), Some(true));
        assert_eq!(parse_bool("n", &f), Some(false));
        assert_eq!(parse_bool("0", &f), Some(false));
        assert_eq!(parse_bool("maybe", &f), None);
    }

    #[test]
    fn dates_and_times() {
        let f = ValueFormats::default();
        let date = NaiveDate::from_ymd_opt(2001, 2, 3).unwrap();
        assert_eq!(parse_date("2001-02-03", &f), Some(date));
        assert_eq!(parse_date("2001-02-03 04:05:06", &f), None);
        assert_eq!(
            parse_naive_timestamp("2001-02-03T04:05:06.123456", &f),
            Some(date.and_hms_micro_opt(4, 5, 6, 123_456).unwrap())
        );
        assert_eq!(
            parse_timestamp("2001-02-03", &f),
            Some(date.and_time(NaiveTime::MIN))
        );
        assert_eq!(parse_naive_timestamp("2001-02-03 04:05:06Z", &f), None);
        let tz = parse_timestamp_tz("2001-02-03 04:05:06+02:00", &f).unwrap();
        assert_eq!(tz.timestamp(), 981_165_906);
        assert!(parse_timestamp_tz("2001-02-03T04:05:06Z", &f).is_some());
        assert!(parse_timestamp_tz("2001-02-03 04:05:06", &f).is_none());
        assert_eq!(parse_time("04:05", &f), NaiveTime::from_hms_opt(4, 5, 0));
        let german = ValueFormats {
            date_format: Some("%d.%m.%Y".into()),
            ..ValueFormats::default()
        };
        assert_eq!(parse_date("03.02.2001", &german), Some(date));
        assert_eq!(parse_date("2001-02-03", &german), None);
    }

    #[test]
    fn uuids_binary_json() {
        let id = parse_uuid("67e55044-10b1-426f-9247-bb680e5fe0c8").unwrap();
        assert_eq!(id[0], 0x67);
        assert_eq!(id[15], 0xc8);
        assert_eq!(parse_uuid("67E5504410B1426F9247BB680E5FE0C8"), Some(id));
        assert_eq!(
            parse_uuid("{67e55044-10b1-426f-9247-bb680e5fe0c8}"),
            Some(id)
        );
        assert_eq!(parse_uuid("67e55044-10b1-426f-9247-bb680e5fe0c"), None);
        assert!(is_canonical_uuid("67e55044-10b1-426f-9247-bb680e5fe0c8"));
        assert!(!is_canonical_uuid("67E5504410B1426F9247BB680E5FE0C8"));

        assert_eq!(parse_binary("0x00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(parse_binary("\\xABcd"), Some(vec![0xab, 0xcd]));
        assert_eq!(parse_binary("0x0"), None);
        assert_eq!(parse_binary("aGk="), Some(b"hi".to_vec()));
        assert_eq!(parse_binary("aGVsbG8h"), Some(b"hello!".to_vec()));
        assert_eq!(parse_binary("aA=="), Some(b"h".to_vec()));
        assert_eq!(parse_binary("a=Gk"), None);
        assert_eq!(parse_binary("not base64"), None);

        assert!(is_json(r#"{"a": [1, 2]}"#));
        assert!(is_json("[]"));
        assert!(!is_json("{oops"));
    }
}
