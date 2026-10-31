//! Conversions between PostgreSQL's text / binary value formats and plain Rust numbers.
//!
//! Text parsers expect the server's default output settings (`DateStyle = ISO`,
//! `bytea_output = hex`, which the driver sets at connect time). A value they cannot read is
//! reported as a [`Problem`] so the caller can show it as NULL and warn once per column.

use std::fmt::Write as _;

/// Days from 1970-01-01 (Unix/Arrow epoch) to 2000-01-01 (PostgreSQL epoch).
pub(crate) const PG_EPOCH_DAYS: i64 = 10_957;
/// Microseconds from 1970-01-01 to 2000-01-01.
pub(crate) const PG_EPOCH_MICROS: i64 = PG_EPOCH_DAYS * MICROS_PER_DAY;
pub(crate) const MICROS_PER_DAY: i64 = 86_400_000_000;

/// Why a value could not be represented in its Arrow column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Problem {
    /// `infinity` / `-infinity` (dates, timestamps, numeric).
    Infinite,
    /// `NaN` in a decimal column.
    NaN,
    /// Outside the range of the Arrow type.
    OutOfRange,
    /// Not in the expected text or binary format.
    Unparsable,
}

impl Problem {
    /// Text for the warning notice (which never contains the value itself).
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Infinite => "infinite values cannot be represented and are shown as NULL",
            Self::NaN => "NaN cannot be represented as a decimal and is shown as NULL",
            Self::OutOfRange => "values outside the column type's range are shown as NULL",
            Self::Unparsable => {
                "values in an unexpected format are shown as NULL (DateStyle must be ISO and \
                 bytea_output hex)"
            }
        }
    }
}

pub(crate) type Parsed<T> = Result<T, Problem>;

// ---------------------------------------------------------------------------------------------
// Calendar arithmetic (proleptic Gregorian, any i64 day count).

/// Days since 1970-01-01 of the civil date `y-m-d` (astronomical year: 1 BC is year 0).
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date (astronomical year, month, day) of a day count since 1970-01-01.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let doe = shifted.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    // Both are small and positive by construction.
    (
        y,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

// ---------------------------------------------------------------------------------------------
// Text parsers.

pub(crate) fn parse_bool(s: &str) -> Parsed<bool> {
    match s {
        "t" | "true" | "TRUE" | "on" | "yes" | "1" => Ok(true),
        "f" | "false" | "FALSE" | "off" | "no" | "0" => Ok(false),
        _ => Err(Problem::Unparsable),
    }
}

pub(crate) fn parse_f64(s: &str) -> Parsed<f64> {
    match s {
        "NaN" => Ok(f64::NAN),
        "Infinity" => Ok(f64::INFINITY),
        "-Infinity" => Ok(f64::NEG_INFINITY),
        _ => s.parse().map_err(|_| Problem::Unparsable),
    }
}

pub(crate) fn parse_f32(s: &str) -> Parsed<f32> {
    match s {
        "NaN" => Ok(f32::NAN),
        "Infinity" => Ok(f32::INFINITY),
        "-Infinity" => Ok(f32::NEG_INFINITY),
        _ => s.parse().map_err(|_| Problem::Unparsable),
    }
}

/// Splits optional sign, integer digits and fraction digits of a plain decimal number.
fn split_decimal(s: &str) -> Parsed<(bool, &str, &str)> {
    let (neg, rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (int, frac) = rest.split_once('.').unwrap_or((rest, ""));
    let digits_ok = |p: &str| p.bytes().all(|c| c.is_ascii_digit());
    if (int.is_empty() && frac.is_empty()) || !digits_ok(int) || !digits_ok(frac) {
        return Err(Problem::Unparsable);
    }
    Ok((neg, int, frac))
}

/// Parses a decimal number into an integer scaled by `10^scale` (rounding half away from zero
/// when there are more fraction digits than `scale`), checking it fits `precision` digits.
pub(crate) fn parse_decimal(text: &str, precision: u8, scale: i8) -> Parsed<i128> {
    match text {
        "NaN" => return Err(Problem::NaN),
        "Infinity" | "-Infinity" => return Err(Problem::Infinite),
        _ => {}
    }
    let (neg, int, frac) = split_decimal(text)?;
    let mut acc: i128 = 0;
    for c in int.bytes().chain(frac.bytes()) {
        acc = acc
            .checked_mul(10)
            .and_then(|a| a.checked_add(i128::from(c - b'0')))
            .ok_or(Problem::OutOfRange)?;
    }
    let frac_len = i32::try_from(frac.len()).map_err(|_| Problem::OutOfRange)?;
    let shift = i32::from(scale) - frac_len;
    if shift >= 0 {
        let factor = pow10(shift).ok_or(Problem::OutOfRange)?;
        acc = acc.checked_mul(factor).ok_or(Problem::OutOfRange)?;
    } else {
        match pow10(-shift) {
            Some(divisor) => {
                let (quot, rem) = (acc / divisor, acc % divisor);
                acc = if rem * 2 >= divisor { quot + 1 } else { quot };
            }
            None => acc = 0,
        }
    }
    let limit = pow10(i32::from(precision)).ok_or(Problem::OutOfRange)?;
    if acc >= limit {
        return Err(Problem::OutOfRange);
    }
    Ok(if neg { -acc } else { acc })
}

fn pow10(exp: i32) -> Option<i128> {
    10i128.checked_pow(u32::try_from(exp).ok()?)
}

fn parse_u32(s: &str) -> Parsed<u32> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return Err(Problem::Unparsable);
    }
    s.parse().map_err(|_| Problem::Unparsable)
}

/// Splits a trailing ` BC` era marker.
fn strip_bc(s: &str) -> (&str, bool) {
    match s.strip_suffix(" BC") {
        Some(rest) => (rest, true),
        None => (s, false),
    }
}

/// `YYYY-MM-DD` (ISO, year may have more than 4 digits) into days since 1970-01-01.
fn parse_ymd(s: &str, bc: bool) -> Parsed<i64> {
    let mut parts = s.splitn(3, '-');
    let (Some(y), Some(m), Some(d)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(Problem::Unparsable);
    };
    let y = i64::from(parse_u32(y)?);
    let (m, d) = (parse_u32(m)?, parse_u32(d)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(Problem::Unparsable);
    }
    let y = if bc { 1 - y } else { y };
    let days = days_from_civil(y, m, d);
    if civil_from_days(days) != (y, m, d) {
        return Err(Problem::Unparsable);
    }
    Ok(days)
}

/// A date in ISO text form into Arrow `Date32` days since 1970-01-01.
pub(crate) fn parse_date(s: &str) -> Parsed<i32> {
    if s == "infinity" || s == "-infinity" {
        return Err(Problem::Infinite);
    }
    let (s, bc) = strip_bc(s);
    i32::try_from(parse_ymd(s, bc)?).map_err(|_| Problem::OutOfRange)
}

/// `HH:MM:SS[.ffffff]` into microseconds since midnight.
pub(crate) fn parse_time(s: &str) -> Parsed<i64> {
    let mut parts = s.splitn(3, ':');
    let (Some(h), Some(m), Some(sec)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(Problem::Unparsable);
    };
    let (sec, frac) = sec.split_once('.').unwrap_or((sec, ""));
    let (h, m, sec) = (parse_u32(h)?, parse_u32(m)?, parse_u32(sec)?);
    if h > 24 || m > 59 || sec > 60 {
        return Err(Problem::Unparsable);
    }
    let micros = parse_fraction_micros(frac)?;
    Ok((i64::from(h) * 3600 + i64::from(m) * 60 + i64::from(sec)) * 1_000_000 + micros)
}

/// Up to 6 fraction digits as microseconds (further digits are ignored).
fn parse_fraction_micros(frac: &str) -> Parsed<i64> {
    if !frac.bytes().all(|c| c.is_ascii_digit()) {
        return Err(Problem::Unparsable);
    }
    let mut micros = 0i64;
    for i in 0..6 {
        let digit = frac.as_bytes().get(i).map_or(0, |c| i64::from(c - b'0'));
        micros = micros * 10 + digit;
    }
    Ok(micros)
}

/// `±HH[:MM[:SS]]` UTC offset into seconds east of UTC.
fn parse_offset(s: &str) -> Parsed<i64> {
    let (sign, rest) = match s.as_bytes().first() {
        Some(b'+') => (1, &s[1..]),
        Some(b'-') => (-1, &s[1..]),
        _ => return Err(Problem::Unparsable),
    };
    let mut secs = 0i64;
    let mut n = 0;
    for (i, part) in rest.split(':').enumerate() {
        if i > 2 {
            return Err(Problem::Unparsable);
        }
        secs = secs * 60 + i64::from(parse_u32(part)?);
        n = i + 1;
    }
    // Scale missing minutes / seconds.
    for _ in n..3 {
        secs *= 60;
    }
    Ok(sign * secs)
}

/// A `timestamp` (no zone) in ISO text form into microseconds since 1970-01-01.
pub(crate) fn parse_timestamp(s: &str) -> Parsed<i64> {
    timestamp_parts(s, false)
}

/// A `timestamptz` in ISO text form (any UTC offset) into UTC microseconds since 1970-01-01.
pub(crate) fn parse_timestamptz(s: &str) -> Parsed<i64> {
    timestamp_parts(s, true)
}

fn timestamp_parts(s: &str, with_offset: bool) -> Parsed<i64> {
    if s == "infinity" || s == "-infinity" {
        return Err(Problem::Infinite);
    }
    let (s, bc) = strip_bc(s);
    let (date, time) = s
        .split_once(' ')
        .or_else(|| s.split_once('T'))
        .ok_or(Problem::Unparsable)?;
    let (time, offset) = if with_offset {
        let at = time.rfind(['+', '-']).ok_or(Problem::Unparsable)?;
        (&time[..at], parse_offset(&time[at..])?)
    } else {
        (time, 0)
    };
    let days = parse_ymd(date, bc)?;
    let micros = parse_time(time)?;
    days.checked_mul(MICROS_PER_DAY)
        .and_then(|d| d.checked_add(micros))
        .and_then(|t| t.checked_sub(offset * 1_000_000))
        .ok_or(Problem::OutOfRange)
}

/// `bytea` text output: hex (`\x0102`) or the legacy escape format.
pub(crate) fn parse_bytea(s: &str) -> Parsed<Vec<u8>> {
    if let Some(hex) = s.strip_prefix("\\x") {
        let b = hex.as_bytes();
        if b.len() % 2 != 0 {
            return Err(Problem::Unparsable);
        }
        return b
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&[hi, lo]| Ok((hex_val(hi)? << 4) | hex_val(lo)?))
            .collect();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
        } else if b.get(i + 1) == Some(&b'\\') {
            out.push(b'\\');
            i += 2;
        } else {
            let oct = b.get(i + 1..i + 4).ok_or(Problem::Unparsable)?;
            let mut v: u32 = 0;
            for &c in oct {
                if !(b'0'..=b'7').contains(&c) {
                    return Err(Problem::Unparsable);
                }
                v = v * 8 + u32::from(c - b'0');
            }
            out.push(u8::try_from(v).map_err(|_| Problem::Unparsable)?);
            i += 4;
        }
    }
    Ok(out)
}

fn hex_val(c: u8) -> Parsed<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(Problem::Unparsable),
    }
}

pub(crate) fn parse_uuid(s: &str) -> Parsed<[u8; 16]> {
    uuid::Uuid::parse_str(s)
        .map(|u| *u.as_bytes())
        .map_err(|_| Problem::Unparsable)
}

// ---------------------------------------------------------------------------------------------
// Text formatting (the server's ISO / postgres styles), for values received in binary.

/// `YYYY-MM-DD[ BC]` for days since 1970-01-01.
pub(crate) fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    let (year, bc) = if y <= 0 { (1 - y, true) } else { (y, false) };
    let mut s = format!("{year:04}-{m:02}-{d:02}");
    if bc {
        s.push_str(" BC");
    }
    s
}

/// `HH:MM:SS[.ffffff]` (trailing fraction zeros trimmed) for microseconds since midnight.
pub(crate) fn format_time(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let mut s = format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    );
    push_fraction(&mut s, frac);
    s
}

fn push_fraction(s: &mut String, frac_micros: i64) {
    if frac_micros != 0 {
        let digits = format!("{frac_micros:06}");
        s.push('.');
        s.push_str(digits.trim_end_matches('0'));
    }
}

/// A timestamp (microseconds since 1970-01-01) as `YYYY-MM-DD HH:MM:SS[.f]`, with `+00` when
/// `utc` (timestamptz values are rendered in UTC).
pub(crate) fn format_timestamp(micros: i64, utc: bool) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let time = micros.rem_euclid(MICROS_PER_DAY);
    let date = format_date(days);
    let (date, bc) = match date.strip_suffix(" BC") {
        Some(d) => (d.to_owned(), true),
        None => (date, false),
    };
    let mut s = format!("{date} {}", format_time(time));
    if utc {
        s.push_str("+00");
    }
    if bc {
        s.push_str(" BC");
    }
    s
}

/// An interval in the server's default `postgres` style (`1 year 2 mons 3 days 04:05:06`).
pub(crate) fn format_interval(months: i32, days: i32, micros: i64) -> String {
    fn part(out: &mut String, value: i64, unit: &str, before: &mut bool) {
        if value == 0 {
            return;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        if *before && value > 0 {
            out.push('+');
        }
        let plural = if value == 1 { "" } else { "s" };
        let _ = write!(out, "{value} {unit}{plural}");
        *before = value < 0;
    }
    let mut out = String::new();
    let mut before = false;
    part(&mut out, i64::from(months / 12), "year", &mut before);
    part(&mut out, i64::from(months % 12), "mon", &mut before);
    part(&mut out, i64::from(days), "day", &mut before);
    if out.is_empty() || micros != 0 {
        if !out.is_empty() {
            out.push(' ');
        }
        if micros < 0 {
            out.push('-');
        } else if before {
            out.push('+');
        }
        let abs = micros.unsigned_abs();
        let secs = abs / 1_000_000;
        let _ = write!(
            out,
            "{:02}:{:02}:{:02}",
            secs / 3600,
            (secs / 60) % 60,
            secs % 60
        );
        push_fraction(&mut out, i64::try_from(abs % 1_000_000).unwrap_or(0));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// numeric binary format: ndigits u16, weight i16, sign u16, dscale u16, then base-10000 digits.

const NUMERIC_POS: u16 = 0x0000;
const NUMERIC_NEG: u16 = 0x4000;
const NUMERIC_NAN: u16 = 0xC000;
const NUMERIC_PINF: u16 = 0xD000;
const NUMERIC_NINF: u16 = 0xF000;
/// Decimal digits a `numeric` can have on either side of the point: the weight is an `i16`
/// counting base-10000 digits.
const NUMERIC_MAX_POINT: i64 = 4 * 32_768;
/// The largest display scale the server accepts (`NUMERIC_DSCALE_MASK`).
const NUMERIC_MAX_DSCALE: i64 = 0x3FFF;

fn be_u16(raw: &[u8], at: usize) -> Parsed<u16> {
    raw.get(at..at + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .ok_or(Problem::Unparsable)
}

/// The server's text form of a binary `numeric` (`NaN`, `Infinity`, `-1.50`, ...).
pub(crate) fn numeric_bin_to_text(raw: &[u8]) -> Parsed<String> {
    let ndigits = usize::from(be_u16(raw, 0)?);
    let weight = i32::from(be_u16(raw, 2)?.cast_signed());
    let sign = be_u16(raw, 4)?;
    let dscale = usize::from(be_u16(raw, 6)?);
    match sign {
        NUMERIC_NAN => return Ok("NaN".into()),
        NUMERIC_PINF => return Ok("Infinity".into()),
        NUMERIC_NINF => return Ok("-Infinity".into()),
        NUMERIC_POS | NUMERIC_NEG => {}
        _ => return Err(Problem::Unparsable),
    }
    let mut digits = Vec::with_capacity(ndigits);
    for i in 0..ndigits {
        digits.push(be_u16(raw, 8 + 2 * i)?);
    }
    let digit = |i: i32| -> u16 {
        usize::try_from(i)
            .ok()
            .and_then(|i| digits.get(i).copied())
            .unwrap_or(0)
    };
    let mut s = String::new();
    if sign == NUMERIC_NEG {
        s.push('-');
    }
    if weight < 0 {
        s.push('0');
    } else {
        for i in 0..=weight {
            if i == 0 {
                let _ = write!(s, "{}", digit(i));
            } else {
                let _ = write!(s, "{:04}", digit(i));
            }
        }
    }
    if dscale > 0 {
        let mut frac = String::with_capacity(dscale + 4);
        let mut i = weight + 1;
        while frac.len() < dscale {
            let _ = write!(frac, "{:04}", digit(i));
            i += 1;
        }
        frac.truncate(dscale);
        s.push('.');
        s.push_str(&frac);
    }
    Ok(s)
}

/// Encodes a decimal number (`-12.50`, `1e-3`, `NaN`, `Infinity`) as binary `numeric`.
/// `None` when the text is not a number or does not fit the format.
pub(crate) fn numeric_text_to_bin(text: &str) -> Option<Vec<u8>> {
    let t = text.trim();
    let special = |sign: u16| {
        let mut out = vec![0u8; 8];
        out[4..6].copy_from_slice(&sign.to_be_bytes());
        Some(out)
    };
    match t.to_ascii_lowercase().as_str() {
        "nan" => return special(NUMERIC_NAN),
        "infinity" | "+infinity" | "inf" | "+inf" => return special(NUMERIC_PINF),
        "-infinity" | "-inf" => return special(NUMERIC_NINF),
        _ => {}
    }
    let (mantissa, exp) = match t.find(['e', 'E']) {
        Some(at) => (&t[..at], t[at + 1..].parse::<i64>().ok()?),
        None => (t, 0),
    };
    let (neg, int, frac) = split_decimal(mantissa).ok()?;
    let mut point = i64::try_from(int.len()).ok()?.checked_add(exp)?;
    let dscale = i64::try_from(frac.len()).ok()?.checked_sub(exp)?.max(0);
    // Check the format's limits before allocating anything sized by the exponent, so a value
    // such as `1e100000000000` is a conversion error rather than a huge allocation.
    if !(-NUMERIC_MAX_POINT..=NUMERIC_MAX_POINT).contains(&point) || dscale > NUMERIC_MAX_DSCALE {
        return None;
    }
    let dscale = u16::try_from(dscale).ok()?;
    let mut digits: Vec<u8> = int.bytes().chain(frac.bytes()).map(|c| c - b'0').collect();
    if point < 0 {
        let pad = usize::try_from(-point).ok()?;
        digits.splice(0..0, std::iter::repeat_n(0, pad));
        point = 0;
    }
    let point_usize = usize::try_from(point).ok()?;
    if point_usize > digits.len() {
        digits.resize(point_usize, 0);
    }
    let pad_left = usize::try_from((4 - point % 4) % 4).ok()?;
    digits.splice(0..0, std::iter::repeat_n(0, pad_left));
    let point = point + i64::try_from(pad_left).ok()?;
    digits.resize(digits.len().div_ceil(4) * 4, 0);
    let mut groups: Vec<u16> = digits
        .as_chunks::<4>()
        .0
        .iter()
        .map(|g| g.iter().fold(0u16, |acc, &d| acc * 10 + u16::from(d)))
        .collect();
    let mut weight = point / 4 - 1;
    let lead = groups.iter().take_while(|&&g| g == 0).count();
    groups.drain(..lead);
    weight -= i64::try_from(lead).ok()?;
    while groups.last() == Some(&0) {
        groups.pop();
    }
    let sign = if groups.is_empty() {
        weight = 0;
        NUMERIC_POS
    } else if neg {
        NUMERIC_NEG
    } else {
        NUMERIC_POS
    };
    let mut out = Vec::with_capacity(8 + 2 * groups.len());
    out.extend_from_slice(&u16::try_from(groups.len()).ok()?.to_be_bytes());
    out.extend_from_slice(&i16::try_from(weight).ok()?.to_be_bytes());
    out.extend_from_slice(&sign.to_be_bytes());
    out.extend_from_slice(&dscale.to_be_bytes());
    for g in groups {
        out.extend_from_slice(&g.to_be_bytes());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_round_trips() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 1, 1), PG_EPOCH_DAYS);
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        for d in [-1_000_000, -719_468, -1, 0, 59, 60, 11_016, 2_000_000] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
    }

    #[test]
    fn dates_and_times() {
        assert_eq!(parse_date("1970-01-02"), Ok(1));
        assert_eq!(parse_date("2000-01-01"), Ok(10_957));
        assert_eq!(parse_date("0001-12-31 BC"), Ok(-719_163));
        assert_eq!(parse_date("2023-02-29"), Err(Problem::Unparsable));
        assert_eq!(parse_date("infinity"), Err(Problem::Infinite));
        assert_eq!(parse_date("15/01/2024"), Err(Problem::Unparsable));
        assert_eq!(format_date(-719_163), "0001-12-31 BC");
        assert_eq!(format_date(-719_162), "0001-01-01");
        assert_eq!(format_date(19_737), "2024-01-15");

        assert_eq!(parse_time("00:00:01.5"), Ok(1_500_000));
        assert_eq!(parse_time("23:59:59.999999"), Ok(86_399_999_999));
        assert_eq!(parse_time("24:00:00"), Ok(MICROS_PER_DAY));
        assert_eq!(format_time(1_500_000), "00:00:01.5");
        assert_eq!(format_time(45_296_000_001), "12:34:56.000001");

        assert_eq!(parse_timestamp("1970-01-01 00:00:00"), Ok(0));
        assert_eq!(
            parse_timestamp("2024-01-15 10:20:30.123456"),
            Ok(1_705_314_030_123_456)
        );
        assert_eq!(parse_timestamp("-infinity"), Err(Problem::Infinite));
        assert_eq!(parse_timestamptz("1970-01-01 05:30:00+05:30"), Ok(0));
        assert_eq!(parse_timestamptz("1969-12-31 19:00:00-05"), Ok(0));
        assert_eq!(
            parse_timestamptz("1970-01-01 00:00:00-00:00:10"),
            Ok(10_000_000)
        );
        assert_eq!(
            parse_timestamptz("0044-03-15 12:00:00+00 BC"),
            Ok(days_from_civil(-43, 3, 15) * MICROS_PER_DAY + 43_200_000_000)
        );
        assert_eq!(
            format_timestamp(1_705_314_030_123_456, true),
            "2024-01-15 10:20:30.123456+00"
        );
        assert_eq!(format_timestamp(-1, false), "1969-12-31 23:59:59.999999");
    }

    #[test]
    fn intervals() {
        assert_eq!(format_interval(0, 0, 0), "00:00:00");
        assert_eq!(
            format_interval(14, 3, 14_706_789_000),
            "1 year 2 mons 3 days 04:05:06.789"
        );
        assert_eq!(format_interval(0, -1, 7_200_000_000), "-1 days +02:00:00");
        assert_eq!(format_interval(-12, 0, 0), "-1 years");
        assert_eq!(format_interval(1, 0, -1_000_000), "1 mon -00:00:01");
        assert_eq!(format_interval(0, 0, 360_000_000_000), "100:00:00");
    }

    #[test]
    fn decimals() {
        assert_eq!(parse_decimal("123.45", 10, 2), Ok(12_345));
        assert_eq!(parse_decimal("-0.5", 10, 2), Ok(-50));
        assert_eq!(parse_decimal("7", 10, 3), Ok(7_000));
        assert_eq!(parse_decimal("1.005", 10, 2), Ok(101));
        assert_eq!(parse_decimal("1200", 5, -2), Ok(12));
        assert_eq!(parse_decimal("NaN", 10, 2), Err(Problem::NaN));
        assert_eq!(parse_decimal("Infinity", 10, 2), Err(Problem::Infinite));
        assert_eq!(parse_decimal("12345", 4, 0), Err(Problem::OutOfRange));
        assert_eq!(parse_decimal("1.2.3", 4, 0), Err(Problem::Unparsable));
        assert_eq!(
            parse_decimal(&"9".repeat(38), 38, 0),
            Ok(10i128.pow(38) - 1)
        );
    }

    #[test]
    fn numeric_binary_round_trips() {
        for (text, back) in [
            ("0", "0"),
            ("1", "1"),
            ("-1.50", "-1.50"),
            ("12345678.9", "12345678.9"),
            ("0.0001", "0.0001"),
            ("0.00001234", "0.00001234"),
            ("10000", "10000"),
            (
                "99999999999999999999.123456789",
                "99999999999999999999.123456789",
            ),
            ("1e3", "1000"),
            ("1.5e-3", "0.0015"),
            ("NaN", "NaN"),
            ("Infinity", "Infinity"),
            ("-Infinity", "-Infinity"),
            ("000.000", "0.000"),
        ] {
            let bin = numeric_text_to_bin(text).unwrap_or_else(|| panic!("{text}"));
            assert_eq!(numeric_bin_to_text(&bin).as_deref(), Ok(back), "{text}");
        }
        // 1.5 = weight 0, digits [1, 5000], dscale 1
        assert_eq!(
            numeric_text_to_bin("1.5").unwrap(),
            [0, 2, 0, 0, 0, 0, 0, 1, 0, 1, 0x13, 0x88]
        );
        assert!(numeric_text_to_bin("abc").is_none());
        // Huge exponents are rejected before anything is allocated for them.
        assert!(numeric_text_to_bin("1e100000000000").is_none());
        assert!(numeric_text_to_bin("1e-100000000000").is_none());
        assert!(numeric_text_to_bin("1e131073").is_none());
        assert!(numeric_text_to_bin("1e1000").is_some());
        assert!(numeric_text_to_bin("").is_none());
        assert_eq!(numeric_bin_to_text(&[0, 1]), Err(Problem::Unparsable));
    }

    #[test]
    fn bytea_and_misc() {
        assert_eq!(parse_bytea("\\x00ff10"), Ok(vec![0, 255, 16]));
        assert_eq!(parse_bytea("\\x0"), Err(Problem::Unparsable));
        assert_eq!(parse_bytea("a\\\\b\\001"), Ok(vec![b'a', b'\\', b'b', 1]));
        assert_eq!(parse_bool("t"), Ok(true));
        assert_eq!(parse_bool("x"), Err(Problem::Unparsable));
        assert!(parse_f64("NaN").unwrap().is_nan());
        assert_eq!(parse_f32("-Infinity"), Ok(f32::NEG_INFINITY));
        assert_eq!(
            parse_uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11").unwrap()[0],
            0xa0
        );
    }
}
