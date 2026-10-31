//! Binds [`Value`] parameters in PostgreSQL's binary format.
//!
//! The server infers each parameter's type from the statement; the value is converted to that
//! type when the conversion is lossless (an `Int` into `int2` / `numeric` / `float8`, `Text`
//! into most scalar types by parsing, ...). Anything else is an error that suggests casting the
//! parameter in SQL (`$1::text`).

use std::error::Error as StdError;
use std::fmt;

use bytes::{BufMut, BytesMut};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use oxbase_types::Value;
use tokio_postgres::types::{IsNull, Kind, ToSql, Type};

use crate::types::type_name;
use crate::values::{
    PG_EPOCH_DAYS, PG_EPOCH_MICROS, numeric_text_to_bin, parse_bool, parse_date, parse_f64,
    parse_time, parse_timestamp, parse_timestamptz, parse_uuid,
};

type BoxError = Box<dyn StdError + Sync + Send>;

/// A [`Value`] bound to a statement parameter.
#[derive(Debug)]
pub(crate) struct PgParam<'a>(pub &'a Value);

#[derive(Debug)]
struct ConvertError(String);

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl StdError for ConvertError {}

fn mismatch(value: &Value, ty: &Type) -> BoxError {
    Box::new(ConvertError(format!(
        "cannot send a {} value as {}; cast the parameter in SQL (for example $1::text)",
        value.type_name(),
        type_name(ty)
    )))
}

fn out_of_range(value: &Value, ty: &Type) -> BoxError {
    Box::new(ConvertError(format!(
        "{} value does not fit {}",
        value.type_name(),
        type_name(ty)
    )))
}

fn is_text_like(ty: &Type) -> bool {
    matches!(
        *ty,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN | Type::XML
    ) || matches!(ty.kind(), Kind::Enum(_))
}

fn days_since_pg_epoch(d: NaiveDate) -> Option<i32> {
    let epoch = NaiveDate::from_ymd_opt(2000, 1, 1)?;
    i32::try_from((d - epoch).num_days()).ok()
}

fn micros_of_day(t: NaiveTime) -> i64 {
    // Leap-second nanos (>= 1e9) are clamped into the last microsecond of the second.
    let micros = i64::from(t.nanosecond().min(999_999_999) / 1000);
    i64::from(t.num_seconds_from_midnight()) * 1_000_000 + micros
}

fn pg_micros(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_micros() - PG_EPOCH_MICROS
}

/// Text into the binary form of `ty`, by parsing. `None` when unsupported or unparsable.
fn text_to_binary(s: &str, ty: &Type, out: &mut BytesMut) -> Option<()> {
    let t = s.trim();
    match *ty {
        Type::BOOL => out.put_u8(u8::from(parse_bool(&t.to_ascii_lowercase()).ok()?)),
        Type::INT2 => out.put_i16(t.parse().ok()?),
        Type::INT4 => out.put_i32(t.parse().ok()?),
        Type::INT8 => out.put_i64(t.parse().ok()?),
        Type::OID => out.put_u32(t.parse().ok()?),
        Type::FLOAT4 => {
            #[allow(clippy::cast_possible_truncation, reason = "float4 parameter")]
            out.put_f32(parse_f64(t).ok()? as f32);
        }
        Type::FLOAT8 => out.put_f64(parse_f64(t).ok()?),
        Type::NUMERIC => out.put_slice(&numeric_text_to_bin(t)?),
        Type::DATE => {
            let days = i64::from(parse_date(t).ok()?) - PG_EPOCH_DAYS;
            out.put_i32(i32::try_from(days).ok()?);
        }
        Type::TIME => out.put_i64(parse_time(t).ok()?),
        Type::TIMESTAMP => out.put_i64(parse_timestamp(t).ok()?.checked_sub(PG_EPOCH_MICROS)?),
        Type::TIMESTAMPTZ => {
            // A timestamp without offset is taken as UTC.
            let micros = parse_timestamptz(t).or_else(|_| parse_timestamp(t)).ok()?;
            out.put_i64(micros.checked_sub(PG_EPOCH_MICROS)?);
        }
        Type::UUID => out.put_slice(&parse_uuid(t).ok()?),
        Type::JSON => out.put_slice(s.as_bytes()),
        Type::JSONB => {
            out.put_u8(1);
            out.put_slice(s.as_bytes());
        }
        _ if is_text_like(ty) => out.put_slice(s.as_bytes()),
        _ => return None,
    }
    Some(())
}

fn int_to_binary(v: i128, ty: &Type, out: &mut BytesMut) -> Option<Result<(), ()>> {
    let fits = |r: bool| Some(if r { Ok(()) } else { Err(()) });
    match *ty {
        Type::INT2 => match i16::try_from(v) {
            Ok(n) => out.put_i16(n),
            Err(_) => return fits(false),
        },
        Type::INT4 => match i32::try_from(v) {
            Ok(n) => out.put_i32(n),
            Err(_) => return fits(false),
        },
        Type::INT8 => match i64::try_from(v) {
            Ok(n) => out.put_i64(n),
            Err(_) => return fits(false),
        },
        Type::OID => match u32::try_from(v) {
            Ok(n) => out.put_u32(n),
            Err(_) => return fits(false),
        },
        #[allow(
            clippy::cast_precision_loss,
            reason = "the server would round the same way"
        )]
        Type::FLOAT4 => out.put_f32(v as f32),
        #[allow(
            clippy::cast_precision_loss,
            reason = "the server would round the same way"
        )]
        Type::FLOAT8 => out.put_f64(v as f64),
        Type::NUMERIC => out.put_slice(&numeric_text_to_bin(&v.to_string())?),
        _ => return None,
    }
    fits(true)
}

impl ToSql for PgParam<'_> {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        if let Kind::Domain(base) = ty.kind() {
            return self.to_sql(base, out);
        }
        let value = self.0;
        match value {
            Value::Null => return Ok(IsNull::Yes),
            Value::Bool(b) => match *ty {
                Type::BOOL => out.put_u8(u8::from(*b)),
                _ if is_text_like(ty) => out.put_slice(if *b { b"true" } else { b"false" }),
                _ => return Err(mismatch(value, ty)),
            },
            Value::Int(_) | Value::UInt(_) => {
                let v = match value {
                    Value::Int(i) => i128::from(*i),
                    Value::UInt(u) => i128::from(*u),
                    _ => unreachable!("matched above"),
                };
                if is_text_like(ty) {
                    out.put_slice(v.to_string().as_bytes());
                } else {
                    match int_to_binary(v, ty, out) {
                        Some(Ok(())) => {}
                        Some(Err(())) => return Err(out_of_range(value, ty)),
                        None => return Err(mismatch(value, ty)),
                    }
                }
            }
            Value::Float(f) => match *ty {
                #[allow(clippy::cast_possible_truncation, reason = "float4 parameter")]
                Type::FLOAT4 => out.put_f32(*f as f32),
                Type::FLOAT8 => out.put_f64(*f),
                Type::NUMERIC => {
                    let text = if f.is_finite() {
                        f.to_string()
                    } else if f.is_nan() {
                        "NaN".into()
                    } else if *f > 0.0 {
                        "Infinity".into()
                    } else {
                        "-Infinity".into()
                    };
                    out.put_slice(&numeric_text_to_bin(&text).ok_or_else(|| mismatch(value, ty))?);
                }
                _ if is_text_like(ty) => out.put_slice(f.to_string().as_bytes()),
                _ => return Err(mismatch(value, ty)),
            },
            Value::Decimal(s) | Value::Text(s) => {
                if text_to_binary(s, ty, out).is_none() {
                    return Err(mismatch(value, ty));
                }
            }
            Value::Bytes(b) => match *ty {
                Type::BYTEA => out.put_slice(b),
                _ => return Err(mismatch(value, ty)),
            },
            Value::Date(d) => match *ty {
                Type::DATE => {
                    out.put_i32(days_since_pg_epoch(*d).ok_or_else(|| out_of_range(value, ty))?);
                }
                _ if is_text_like(ty) => out.put_slice(d.to_string().as_bytes()),
                _ => return Err(mismatch(value, ty)),
            },
            Value::Time(t) => match *ty {
                Type::TIME => out.put_i64(micros_of_day(*t)),
                _ if is_text_like(ty) => out.put_slice(t.to_string().as_bytes()),
                _ => return Err(mismatch(value, ty)),
            },
            Value::Timestamp(dt) => match *ty {
                // A timestamp without zone bound to timestamptz is taken as UTC.
                Type::TIMESTAMP | Type::TIMESTAMPTZ => out.put_i64(pg_micros(*dt)),
                _ if is_text_like(ty) => out.put_slice(dt.to_string().as_bytes()),
                _ => return Err(mismatch(value, ty)),
            },
            Value::TimestampTz(dt) => match *ty {
                // timestamp (no zone) gets the UTC wall time.
                Type::TIMESTAMPTZ | Type::TIMESTAMP => out.put_i64(pg_micros(dt.naive_utc())),
                _ if is_text_like(ty) => out.put_slice(dt.to_rfc3339().as_bytes()),
                _ => return Err(mismatch(value, ty)),
            },
            Value::Uuid(u) => match *ty {
                Type::UUID => out.put_slice(u.as_bytes()),
                _ if is_text_like(ty) => out.put_slice(u.hyphenated().to_string().as_bytes()),
                _ => return Err(mismatch(value, ty)),
            },
            Value::Json(j) => {
                let text = j.to_string();
                match *ty {
                    Type::JSONB => {
                        out.put_u8(1);
                        out.put_slice(text.as_bytes());
                    }
                    Type::JSON => out.put_slice(text.as_bytes()),
                    _ if is_text_like(ty) => out.put_slice(text.as_bytes()),
                    _ => return Err(mismatch(value, ty)),
                }
            }
        }
        Ok(IsNull::No)
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn to_sql_checked(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        self.to_sql(ty, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, NaiveDate};

    fn enc(v: &Value, ty: &Type) -> Result<Vec<u8>, String> {
        let mut out = BytesMut::new();
        match PgParam(v).to_sql(ty, &mut out) {
            Ok(IsNull::No) => Ok(out.to_vec()),
            Ok(IsNull::Yes) => Ok(b"<null>".to_vec()),
            Err(e) => Err(e.to_string()),
        }
    }

    #[test]
    fn integers_convert_to_the_inferred_type() {
        assert_eq!(enc(&Value::Int(7), &Type::INT2), Ok(vec![0, 7]));
        assert_eq!(
            enc(&Value::Int(7), &Type::INT8),
            Ok(7i64.to_be_bytes().to_vec())
        );
        assert_eq!(
            enc(&Value::Int(2), &Type::FLOAT8),
            Ok(2f64.to_be_bytes().to_vec())
        );
        assert_eq!(enc(&Value::Int(42), &Type::TEXT), Ok(b"42".to_vec()));
        assert_eq!(
            enc(&Value::Int(15), &Type::NUMERIC),
            Ok(numeric_text_to_bin("15").unwrap())
        );
        let err = enc(&Value::Int(70_000), &Type::INT2).unwrap_err();
        assert!(err.contains("does not fit int2"), "{err}");
        let err = enc(&Value::Int(1), &Type::BYTEA).unwrap_err();
        assert!(err.contains("$1::text"), "{err}");
        assert_eq!(
            enc(&Value::UInt(u64::MAX), &Type::INT8).unwrap_err(),
            "uint value does not fit int8"
        );
    }

    #[test]
    fn text_is_parsed_for_scalar_types() {
        assert_eq!(
            enc(&Value::Text("12".into()), &Type::INT4),
            Ok(12i32.to_be_bytes().to_vec())
        );
        assert_eq!(enc(&Value::Text("true".into()), &Type::BOOL), Ok(vec![1]));
        assert_eq!(
            enc(&Value::Text("2000-01-02".into()), &Type::DATE),
            Ok(1i32.to_be_bytes().to_vec())
        );
        assert_eq!(
            enc(
                &Value::Text("2000-01-01 00:00:01+00".into()),
                &Type::TIMESTAMPTZ
            ),
            Ok(1_000_000i64.to_be_bytes().to_vec())
        );
        assert_eq!(
            enc(&Value::Text("{\"a\":1}".into()), &Type::JSONB).unwrap()[0],
            1
        );
        assert_eq!(
            enc(&Value::Decimal("1.5".into()), &Type::NUMERIC)
                .unwrap()
                .len(),
            12
        );
        assert!(enc(&Value::Text("abc".into()), &Type::INT4).is_err());
        assert!(enc(&Value::Text("10.0.0.1".into()), &Type::INET).is_err());
        let mood = Type::new("mood".into(), 90_010, Kind::Enum(vec![]), "public".into());
        assert_eq!(
            enc(&Value::Text("happy".into()), &mood),
            Ok(b"happy".to_vec())
        );
    }

    #[test]
    fn temporal_and_other_values() {
        let d = NaiveDate::from_ymd_opt(2000, 1, 3).unwrap();
        assert_eq!(
            enc(&Value::Date(d), &Type::DATE),
            Ok(2i32.to_be_bytes().to_vec())
        );
        let dt = d.and_hms_opt(0, 0, 0).unwrap();
        assert_eq!(
            enc(&Value::Timestamp(dt), &Type::TIMESTAMP),
            Ok((2 * 86_400_000_000i64).to_be_bytes().to_vec())
        );
        let tz = DateTime::parse_from_rfc3339("2000-01-01T02:00:00+02:00").unwrap();
        assert_eq!(
            enc(&Value::TimestampTz(tz), &Type::TIMESTAMPTZ),
            Ok(vec![0; 8])
        );
        let t = NaiveTime::from_hms_micro_opt(0, 0, 1, 5).unwrap();
        assert_eq!(
            enc(&Value::Time(t), &Type::TIME),
            Ok(1_000_005i64.to_be_bytes().to_vec())
        );
        assert_eq!(enc(&Value::Null, &Type::INT4), Ok(b"<null>".to_vec()));
        assert_eq!(enc(&Value::Bytes(vec![1, 2]), &Type::BYTEA), Ok(vec![1, 2]));
        assert_eq!(enc(&Value::Bool(true), &Type::BOOL), Ok(vec![1]));
        let u = uuid::Uuid::nil();
        assert_eq!(enc(&Value::Uuid(u), &Type::UUID), Ok(vec![0; 16]));
        let j = Value::Json(serde_json::json!([1]));
        assert_eq!(enc(&j, &Type::JSON), Ok(b"[1]".to_vec()));
        let domain = Type::new(
            "pos".into(),
            90_011,
            Kind::Domain(Type::INT4),
            "public".into(),
        );
        assert_eq!(
            enc(&Value::Int(3), &domain),
            Ok(3i32.to_be_bytes().to_vec())
        );
    }
}
