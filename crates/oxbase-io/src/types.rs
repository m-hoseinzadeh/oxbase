//! Logical ↔ Arrow types at the file boundary, and file names in messages.

use std::path::Path;

use arrow_schema::{DataType as ArrowType, Field, TimeUnit};
use oxbase_types::DataType;

/// Arrow field metadata key that marks JSON text (Arrow's canonical extension type name).
pub(crate) const EXTENSION_NAME: &str = "ARROW:extension:name";
/// Extension name of JSON text columns.
pub(crate) const JSON_EXTENSION: &str = "arrow.json";

/// The Arrow type a reader produces for a column of logical type `data_type` (the canonical
/// load type, `oxbase_dialect::bulk::load_arrow_type`): `Int64`, `Float64`, `Utf8` for decimals
/// and text, `Date32`, `Time64(µs)`, `Timestamp(µs[, "+00:00"])`, `FixedSizeBinary(16)`, ...
pub fn arrow_type_for(data_type: &DataType) -> ArrowType {
    oxbase_dialect::bulk::load_arrow_type(data_type)
}

/// The logical type of an Arrow field (for `CREATE TABLE` in a SQL dump of a query result,
/// and for the column types of an export preview):
///
/// | Arrow | Logical |
/// |---|---|
/// | `Boolean` | `bool` |
/// | `Int8/16/32/64`, `UInt8/16/32/64` | `int` of that width, unsigned for `UInt*` |
/// | `Float16/32` | `float(32)`; `Float64`: `float(64)` |
/// | `Decimal*(p, s)` | `decimal(p, s)` |
/// | `Utf8`, `LargeUtf8`, `Utf8View` | `json` when the field's `ARROW:extension:name` is `arrow.json`, else `text` |
/// | `Binary`, `LargeBinary`, `BinaryView` | `var_binary` |
/// | `FixedSizeBinary(16)` | `uuid`; other widths `binary(n)` |
/// | `Date32`, `Date64` | `date` |
/// | `Time32/64(unit)` | `time` with 0 / 3 / 6 / 9 fractional digits |
/// | `Timestamp(unit, None)` | `timestamp`; with a zone `timestamp_tz` |
/// | `Duration` | `interval` |
/// | lists, structs, maps | `json` (written as JSON text) |
/// | anything else | `other(<arrow type>)` |
pub fn logical_type(field: &Field) -> DataType {
    let json_marked = field
        .metadata()
        .get(EXTENSION_NAME)
        .is_some_and(|n| n == JSON_EXTENSION);
    match field.data_type() {
        ArrowType::Boolean => DataType::Bool,
        ArrowType::Int8 => int(8, false),
        ArrowType::Int16 => int(16, false),
        ArrowType::Int32 => int(32, false),
        ArrowType::Int64 => int(64, false),
        ArrowType::UInt8 => int(8, true),
        ArrowType::UInt16 => int(16, true),
        ArrowType::UInt32 => int(32, true),
        ArrowType::UInt64 => int(64, true),
        ArrowType::Float16 | ArrowType::Float32 => DataType::Float { bits: 32 },
        ArrowType::Float64 => DataType::Float { bits: 64 },
        ArrowType::Decimal32(p, s)
        | ArrowType::Decimal64(p, s)
        | ArrowType::Decimal128(p, s)
        | ArrowType::Decimal256(p, s) => DataType::Decimal {
            precision: Some(u32::from(*p)),
            scale: u32::try_from(*s).ok(),
        },
        ArrowType::Utf8 | ArrowType::LargeUtf8 | ArrowType::Utf8View if json_marked => {
            DataType::Json { binary: false }
        }
        ArrowType::Utf8 | ArrowType::LargeUtf8 | ArrowType::Utf8View => DataType::Text,
        ArrowType::Binary | ArrowType::LargeBinary | ArrowType::BinaryView => {
            DataType::VarBinary { length: None }
        }
        ArrowType::FixedSizeBinary(16) => DataType::Uuid,
        ArrowType::FixedSizeBinary(n) => DataType::Binary {
            length: u32::try_from(*n).ok(),
        },
        ArrowType::Date32 | ArrowType::Date64 => DataType::Date,
        ArrowType::Time32(unit) | ArrowType::Time64(unit) => DataType::Time {
            precision: Some(digits(*unit)),
        },
        ArrowType::Timestamp(unit, None) => DataType::Timestamp {
            precision: Some(digits(*unit)),
        },
        ArrowType::Timestamp(unit, Some(_)) => DataType::TimestampTz {
            precision: Some(digits(*unit)),
        },
        ArrowType::Duration(_) | ArrowType::Interval(_) => DataType::Interval,
        ArrowType::List(_)
        | ArrowType::LargeList(_)
        | ArrowType::ListView(_)
        | ArrowType::LargeListView(_)
        | ArrowType::FixedSizeList(_, _)
        | ArrowType::Struct(_)
        | ArrowType::Map(_, _) => DataType::Json { binary: false },
        other => DataType::Other {
            name: other.to_string(),
        },
    }
}

fn int(bits: u8, unsigned: bool) -> DataType {
    DataType::Int { bits, unsigned }
}

fn digits(unit: TimeUnit) -> u8 {
    match unit {
        TimeUnit::Second => 0,
        TimeUnit::Millisecond => 3,
        TimeUnit::Microsecond => 6,
        TimeUnit::Nanosecond => 9,
    }
}

/// The base name of `path` for messages (`orders.csv`), never the directories: errors reach
/// the UI and logs.
pub fn file_label(path: &Path) -> String {
    path.file_name().map_or_else(
        || "the file".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;

    #[test]
    fn logical_types() {
        let f = |t: ArrowType| logical_type(&Field::new("c", t, true));
        assert_eq!(f(ArrowType::Int32), DataType::INT32);
        assert_eq!(
            f(ArrowType::UInt64),
            DataType::Int {
                bits: 64,
                unsigned: true
            }
        );
        assert_eq!(
            f(ArrowType::Decimal128(12, 2)),
            DataType::Decimal {
                precision: Some(12),
                scale: Some(2)
            }
        );
        assert_eq!(
            f(ArrowType::Timestamp(
                TimeUnit::Microsecond,
                Some("UTC".into())
            )),
            DataType::TimestampTz { precision: Some(6) }
        );
        assert_eq!(f(ArrowType::FixedSizeBinary(16)), DataType::Uuid);
        let item = Arc::new(Field::new("item", ArrowType::Int32, true));
        assert_eq!(f(ArrowType::List(item)), DataType::Json { binary: false });
        let json = Field::new("doc", ArrowType::Utf8, true).with_metadata(HashMap::from([(
            EXTENSION_NAME.to_owned(),
            JSON_EXTENSION.to_owned(),
        )]));
        assert_eq!(logical_type(&json), DataType::Json { binary: false });

        // Round trip through the canonical load types keeps the logical type for the types
        // readers produce.
        for t in [
            DataType::Bool,
            DataType::INT64,
            DataType::FLOAT64,
            DataType::Date,
            DataType::Uuid,
            DataType::Text,
        ] {
            assert_eq!(f(arrow_type_for(&t)), t);
        }
    }

    #[test]
    fn labels_hide_directories() {
        assert_eq!(
            file_label(Path::new("/home/someone/secret-project/orders.csv")),
            "orders.csv"
        );
        assert_eq!(file_label(Path::new("/")), "the file");
    }
}
