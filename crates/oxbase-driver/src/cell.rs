//! [`cell_value`]: one Arrow cell as a typed [`Value`] (M5).
//!
//! The inverse of what drivers produce, for code that must bind or print single values out of a
//! `RecordBatch`: bulk loaders that fall back to parameterized `INSERT`s and the SQL dump
//! writer (`docs/m5-contracts.md` §4, §3.6). The grid has its own text grammar
//! (`oxbase_services::convert`).

use arrow_array::Array;
use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Date64Type, Decimal32Type, Decimal64Type, Decimal128Type, Decimal256Type,
    DurationMicrosecondType, DurationMillisecondType, DurationNanosecondType, DurationSecondType,
    Float16Type, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type,
    Time32MillisecondType, Time32SecondType, Time64MicrosecondType, Time64NanosecondType,
    TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
    TimestampSecondType, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};
use arrow_schema::{DataType as ArrowType, TimeUnit};
use chrono::NaiveDateTime;
use oxbase_types::{Error, Result, Value};

/// Row `row` of `array` as a [`Value`].
///
/// | Arrow type | `Value` |
/// |---|---|
/// | null slot (any type), `Null` | `Null` |
/// | `Boolean` | `Bool` |
/// | `Int8`..`Int64`, `UInt8`..`UInt32` | `Int` |
/// | `UInt64` | `Int` when it fits `i64`, else `UInt` |
/// | `Float16`, `Float32`, `Float64` | `Float` |
/// | `Decimal32`..`Decimal256` | `Decimal` (exact text, e.g. `"12.50"`) |
/// | `Utf8`, `LargeUtf8`, `Utf8View` | `Text` |
/// | `Binary`, `LargeBinary`, `BinaryView`, `FixedSizeBinary(n != 16)` | `Bytes` |
/// | `FixedSizeBinary(16)` | `Uuid` (the only driver type of that shape) |
/// | `Date32`, `Date64` | `Date` |
/// | `Time32`, `Time64` | `Time` |
/// | `Timestamp(_, None)` | `Timestamp` |
/// | `Timestamp(_, Some(tz))` | `TimestampTz` at offset `+00:00` (Arrow stores the instant in UTC) |
/// | `Duration` (MySQL `TIME`) | `Text` `[-]HH:MM:SS[.ffffff]` |
///
/// Anything else (lists, structs, maps, unions, intervals) is `Error::Unsupported` naming the
/// type; drivers deliver those as text. `InvalidInput` when `row` is out of range, `Internal`
/// when a temporal value is out of chrono's range.
pub fn cell_value(array: &dyn Array, row: usize) -> Result<Value> {
    if row >= array.len() {
        return Err(Error::invalid_input(format!(
            "row {row} is out of range (the column has {} rows)",
            array.len()
        )));
    }
    if array.is_null(row) {
        return Ok(Value::Null);
    }
    let value = match array.data_type() {
        ArrowType::Null => Value::Null,
        ArrowType::Boolean => Value::Bool(array.as_boolean().value(row)),
        ArrowType::Int8 => Value::Int(array.as_primitive::<Int8Type>().value(row).into()),
        ArrowType::Int16 => Value::Int(array.as_primitive::<Int16Type>().value(row).into()),
        ArrowType::Int32 => Value::Int(array.as_primitive::<Int32Type>().value(row).into()),
        ArrowType::Int64 => Value::Int(array.as_primitive::<Int64Type>().value(row)),
        ArrowType::UInt8 => Value::Int(array.as_primitive::<UInt8Type>().value(row).into()),
        ArrowType::UInt16 => Value::Int(array.as_primitive::<UInt16Type>().value(row).into()),
        ArrowType::UInt32 => Value::Int(array.as_primitive::<UInt32Type>().value(row).into()),
        ArrowType::UInt64 => {
            let v = array.as_primitive::<UInt64Type>().value(row);
            i64::try_from(v).map_or(Value::UInt(v), Value::Int)
        }
        ArrowType::Float16 => Value::Float(array.as_primitive::<Float16Type>().value(row).to_f64()),
        ArrowType::Float32 => {
            Value::Float(f64::from(array.as_primitive::<Float32Type>().value(row)))
        }
        ArrowType::Float64 => Value::Float(array.as_primitive::<Float64Type>().value(row)),
        ArrowType::Decimal32(_, _) => {
            Value::Decimal(array.as_primitive::<Decimal32Type>().value_as_string(row))
        }
        ArrowType::Decimal64(_, _) => {
            Value::Decimal(array.as_primitive::<Decimal64Type>().value_as_string(row))
        }
        ArrowType::Decimal128(_, _) => {
            Value::Decimal(array.as_primitive::<Decimal128Type>().value_as_string(row))
        }
        ArrowType::Decimal256(_, _) => {
            Value::Decimal(array.as_primitive::<Decimal256Type>().value_as_string(row))
        }
        ArrowType::Utf8 => Value::Text(array.as_string::<i32>().value(row).to_owned()),
        ArrowType::LargeUtf8 => Value::Text(array.as_string::<i64>().value(row).to_owned()),
        ArrowType::Utf8View => Value::Text(array.as_string_view().value(row).to_owned()),
        ArrowType::Binary => Value::Bytes(array.as_binary::<i32>().value(row).to_vec()),
        ArrowType::LargeBinary => Value::Bytes(array.as_binary::<i64>().value(row).to_vec()),
        ArrowType::BinaryView => Value::Bytes(array.as_binary_view().value(row).to_vec()),
        ArrowType::FixedSizeBinary(16) => {
            let bytes = array.as_fixed_size_binary().value(row);
            let uuid = uuid::Uuid::from_slice(bytes)
                .map_err(|e| Error::internal(format!("bad UUID bytes: {e}")))?;
            Value::Uuid(uuid)
        }
        ArrowType::FixedSizeBinary(_) => {
            Value::Bytes(array.as_fixed_size_binary().value(row).to_vec())
        }
        ArrowType::Date32 => Value::Date(temporal(
            array.as_primitive::<Date32Type>().value_as_date(row),
        )?),
        ArrowType::Date64 => Value::Date(temporal(
            array.as_primitive::<Date64Type>().value_as_date(row),
        )?),
        ArrowType::Time32(TimeUnit::Second) => Value::Time(temporal(
            array.as_primitive::<Time32SecondType>().value_as_time(row),
        )?),
        ArrowType::Time32(TimeUnit::Millisecond) => Value::Time(temporal(
            array
                .as_primitive::<Time32MillisecondType>()
                .value_as_time(row),
        )?),
        ArrowType::Time64(TimeUnit::Microsecond) => Value::Time(temporal(
            array
                .as_primitive::<Time64MicrosecondType>()
                .value_as_time(row),
        )?),
        ArrowType::Time64(TimeUnit::Nanosecond) => Value::Time(temporal(
            array
                .as_primitive::<Time64NanosecondType>()
                .value_as_time(row),
        )?),
        ArrowType::Timestamp(unit, tz) => {
            let naive = timestamp(array, *unit, row)?;
            if tz.is_some() {
                Value::TimestampTz(naive.and_utc().fixed_offset())
            } else {
                Value::Timestamp(naive)
            }
        }
        ArrowType::Duration(unit) => {
            let micros = match unit {
                TimeUnit::Second => array
                    .as_primitive::<DurationSecondType>()
                    .value(row)
                    .saturating_mul(1_000_000),
                TimeUnit::Millisecond => array
                    .as_primitive::<DurationMillisecondType>()
                    .value(row)
                    .saturating_mul(1_000),
                TimeUnit::Microsecond => array.as_primitive::<DurationMicrosecondType>().value(row),
                TimeUnit::Nanosecond => {
                    array.as_primitive::<DurationNanosecondType>().value(row) / 1_000
                }
            };
            Value::Text(duration_text(micros))
        }
        other => {
            return Err(Error::unsupported(format!(
                "cannot read a single value of Arrow type {other}"
            )));
        }
    };
    Ok(value)
}

fn temporal<T>(value: Option<T>) -> Result<T> {
    value.ok_or_else(|| Error::internal("temporal value out of range"))
}

fn timestamp(array: &dyn Array, unit: TimeUnit, row: usize) -> Result<NaiveDateTime> {
    temporal(match unit {
        TimeUnit::Second => array
            .as_primitive::<TimestampSecondType>()
            .value_as_datetime(row),
        TimeUnit::Millisecond => array
            .as_primitive::<TimestampMillisecondType>()
            .value_as_datetime(row),
        TimeUnit::Microsecond => array
            .as_primitive::<TimestampMicrosecondType>()
            .value_as_datetime(row),
        TimeUnit::Nanosecond => array
            .as_primitive::<TimestampNanosecondType>()
            .value_as_datetime(row),
    })
}

/// `[-]HH:MM:SS[.ffffff]`; hours may exceed 24 (MySQL `TIME` spans ±838 hours).
fn duration_text(micros: i64) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let total = micros.unsigned_abs();
    let frac = total % 1_000_000;
    let secs = total / 1_000_000;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if frac == 0 {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    } else {
        format!("{sign}{h:02}:{m:02}:{s:02}.{frac:06}")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::types::Int32Type as ListItem;
    use arrow_array::{
        ArrayRef, BinaryArray, BooleanArray, Date32Array, Decimal128Array,
        DurationMicrosecondArray, FixedSizeBinaryArray, Float64Array, Int32Array, ListArray,
        StringArray, Time64MicrosecondArray, TimestampMicrosecondArray, UInt64Array,
    };
    use chrono::{NaiveDate, NaiveTime};

    use super::*;

    fn at(array: &ArrayRef, row: usize) -> Value {
        cell_value(array.as_ref(), row).unwrap()
    }

    #[test]
    fn scalars() {
        let ints: ArrayRef = Arc::new(Int32Array::from(vec![Some(-7), None]));
        assert_eq!(at(&ints, 0), Value::Int(-7));
        assert_eq!(at(&ints, 1), Value::Null);

        let big: ArrayRef = Arc::new(UInt64Array::from(vec![5, u64::MAX]));
        assert_eq!(at(&big, 0), Value::Int(5));
        assert_eq!(at(&big, 1), Value::UInt(u64::MAX));

        let bools: ArrayRef = Arc::new(BooleanArray::from(vec![true]));
        assert_eq!(at(&bools, 0), Value::Bool(true));

        let floats: ArrayRef = Arc::new(Float64Array::from(vec![1.5]));
        assert_eq!(at(&floats, 0), Value::Float(1.5));

        let dec: ArrayRef = Arc::new(
            Decimal128Array::from(vec![12_345_i128])
                .with_precision_and_scale(10, 2)
                .unwrap(),
        );
        assert_eq!(at(&dec, 0), Value::Decimal("123.45".into()));

        let text: ArrayRef = Arc::new(StringArray::from(vec!["a'b"]));
        assert_eq!(at(&text, 0), Value::Text("a'b".into()));

        let bytes: ArrayRef = Arc::new(BinaryArray::from(vec![&b"\x00\xff"[..]]));
        assert_eq!(at(&bytes, 0), Value::Bytes(vec![0, 255]));

        let id = uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        let uuids: ArrayRef = Arc::new(
            FixedSizeBinaryArray::try_from_iter(std::iter::once(id.as_bytes().to_vec())).unwrap(),
        );
        assert_eq!(at(&uuids, 0), Value::Uuid(id));
    }

    #[test]
    fn temporal_values() {
        let dates: ArrayRef = Arc::new(Date32Array::from(vec![19_000]));
        assert_eq!(
            at(&dates, 0),
            Value::Date(NaiveDate::from_ymd_opt(2022, 1, 8).unwrap())
        );

        let times: ArrayRef = Arc::new(Time64MicrosecondArray::from(vec![3_723_000_001]));
        assert_eq!(
            at(&times, 0),
            Value::Time(NaiveTime::from_hms_micro_opt(1, 2, 3, 1).unwrap())
        );

        let naive: ArrayRef = Arc::new(TimestampMicrosecondArray::from(vec![1_000_000]));
        let expected = NaiveDate::from_ymd_opt(1970, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 1)
            .unwrap();
        assert_eq!(at(&naive, 0), Value::Timestamp(expected));

        let zoned: ArrayRef =
            Arc::new(TimestampMicrosecondArray::from(vec![1_000_000]).with_timezone("+00:00"));
        match at(&zoned, 0) {
            Value::TimestampTz(t) => {
                assert_eq!(t.naive_utc(), expected);
                assert_eq!(t.offset().local_minus_utc(), 0);
            }
            other => panic!("expected TimestampTz, got {other:?}"),
        }

        let durations: ArrayRef = Arc::new(DurationMicrosecondArray::from(vec![
            -(838 * 3600 + 59 * 60 + 59) * 1_000_000,
            1_500_000,
        ]));
        assert_eq!(at(&durations, 0), Value::Text("-838:59:59".into()));
        assert_eq!(at(&durations, 1), Value::Text("00:00:01.500000".into()));
    }

    #[test]
    fn errors() {
        let ints: ArrayRef = Arc::new(Int32Array::from(vec![1]));
        assert!(cell_value(ints.as_ref(), 1).is_err());

        let list: ArrayRef = Arc::new(ListArray::from_iter_primitive::<ListItem, _, _>(vec![
            Some(vec![Some(1)]),
        ]));
        let err = cell_value(list.as_ref(), 0).unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::Unsupported);
    }
}
