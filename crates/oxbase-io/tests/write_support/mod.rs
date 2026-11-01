//! Shared fixtures of the writer tests (`tests/write_*.rs`): one batch with every Arrow type
//! the drivers and readers produce, NULLs and awkward values, and small file helpers.
#![allow(dead_code)] // each test binary uses a part

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use arrow_array::builder::{Int32Builder, ListBuilder};
use arrow_array::types::Date32Type;
use arrow_array::{
    ArrayRef, BinaryArray, BooleanArray, Date32Array, Decimal128Array, FixedSizeBinaryArray,
    Float64Array, Int32Array, Int64Array, RecordBatch, StringArray, Time64MicrosecondArray,
    TimestampMicrosecondArray, UInt64Array,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use chrono::NaiveDate;
use oxbase_dialect::DialectRegistry;
use oxbase_io::{SinkReport, WriteContext, WriteOptions, create_writer};

/// Text with every character CSV, JSON and SQL writers must escape.
pub const TRICKY: &str = "it's \"quoted\", a;b\tc\r\nd\\e";
/// Non-ASCII text, including an astral-plane character.
pub const UNICODE: &str = "naïve café – 日本語 🚀";
/// 2^53 + 1: the first integer a double cannot hold.
pub const BEYOND_DOUBLE: i64 = 9_007_199_254_740_993;

pub const UUID_1: [u8; 16] = [
    0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0,
];
pub const UUID_1_TEXT: &str = "12345678-9abc-def0-1234-56789abcdef0";
pub const UUID_2: [u8; 16] = [0xff; 16];

pub fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).unwrap()
}

/// Microseconds since the epoch of a UTC wall-clock time.
pub fn micros(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    micro: u32,
) -> i64 {
    date(year, month, day)
        .and_hms_micro_opt(hour, minute, second, micro)
        .unwrap()
        .and_utc()
        .timestamp_micros()
}

/// A text field marked as JSON (`arrow.json`), as drivers mark JSON columns.
pub fn json_field(name: &str) -> Field {
    Field::new(name, DataType::Utf8, true).with_metadata(HashMap::from([(
        "ARROW:extension:name".to_owned(),
        "arrow.json".to_owned(),
    )]))
}

/// Column names of [`sample`], in order.
pub const COLUMNS: [&str; 16] = [
    "flag", "n", "big", "small", "f", "dec", "txt", "formula", "doc", "bin", "day", "clock", "at",
    "at_tz", "id", "tags",
];

/// Four rows: ordinary values, edge values, all NULL, more edge values.
pub fn sample() -> RecordBatch {
    let mut tags = ListBuilder::new(Int32Builder::new());
    tags.values().append_value(1);
    tags.values().append_value(2);
    tags.append(true);
    tags.append(true);
    tags.append(false);
    tags.values().append_value(3);
    tags.append(true);
    let tags = tags.finish();

    let columns: Vec<ArrayRef> = vec![
        Arc::new(BooleanArray::from(vec![
            Some(true),
            Some(false),
            None,
            Some(true),
        ])),
        Arc::new(Int64Array::from(vec![
            Some(1),
            Some(BEYOND_DOUBLE),
            None,
            Some(i64::MIN),
        ])),
        Arc::new(UInt64Array::from(vec![
            Some(0),
            Some(u64::MAX),
            None,
            Some(42),
        ])),
        Arc::new(Int32Array::from(vec![
            Some(-5),
            Some(i32::MAX),
            None,
            Some(0),
        ])),
        Arc::new(Float64Array::from(vec![
            Some(0.1),
            Some(-1.5e300),
            None,
            Some(1.0),
        ])),
        Arc::new(
            Decimal128Array::from(vec![
                Some(125_000_000_000_i128),
                Some(-123_456_789_012_345_678_900_123_456_789_i128),
                None,
                Some(0),
            ])
            .with_precision_and_scale(38, 10)
            .unwrap(),
        ),
        Arc::new(StringArray::from(vec![
            Some("plain"),
            Some(TRICKY),
            None,
            Some(UNICODE),
        ])),
        Arc::new(StringArray::from(vec![
            Some("=SUM(A1)"),
            Some("+1+1"),
            None,
            Some("-2-2"),
        ])),
        Arc::new(StringArray::from(vec![
            Some("{\"a\": [1, 2.50]}"),
            Some("[true,null]"),
            None,
            Some("not json"),
        ])),
        Arc::new(BinaryArray::from(vec![
            Some(&b"\x00\xff"[..]),
            Some(&b""[..]),
            None,
            Some(&b"abc"[..]),
        ])),
        Arc::new(Date32Array::from(vec![
            Some(Date32Type::from_naive_date(date(2024, 2, 29))),
            Some(Date32Type::from_naive_date(date(1899, 12, 31))),
            None,
            Some(Date32Type::from_naive_date(date(9999, 12, 31))),
        ])),
        Arc::new(Time64MicrosecondArray::from(vec![
            Some(11_045_123_456),
            Some(86_399_000_000),
            None,
            Some(0),
        ])),
        Arc::new(TimestampMicrosecondArray::from(vec![
            Some(micros(2024, 1, 2, 3, 4, 5, 123_456)),
            Some(0),
            None,
            Some(micros(2038, 1, 19, 3, 14, 8, 0)),
        ])),
        Arc::new(
            TimestampMicrosecondArray::from(vec![
                Some(1_700_000_000_123_456),
                Some(-1_000_000),
                None,
                Some(1_000),
            ])
            .with_timezone("+00:00"),
        ),
        Arc::new(
            FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                vec![Some(UUID_1), Some(UUID_2), None, Some([0_u8; 16])].into_iter(),
                16,
            )
            .unwrap(),
        ),
        Arc::new(tags),
    ];
    let fields: Vec<Field> = COLUMNS
        .iter()
        .zip(&columns)
        .map(|(name, column)| {
            if *name == "doc" {
                json_field(name)
            } else {
                Field::new(*name, column.data_type().clone(), true)
            }
        })
        .collect();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap()
}

pub fn sample_schema() -> SchemaRef {
    sample().schema()
}

/// The canonical timestamp-with-zone type of the load schema.
pub fn utc_micros() -> DataType {
    DataType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into()))
}

/// A one-column `Int64` batch `0..rows`.
pub fn numbers(rows: i64) -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, true)]));
    RecordBatch::try_new(
        schema,
        vec![Arc::new(Int64Array::from_iter_values(0..rows))],
    )
    .unwrap()
}

/// The dialect cells of every SQL engine.
pub fn dialects() -> Arc<DialectRegistry> {
    let mut reg = DialectRegistry::new();
    oxbase_pg::dialect::register(&mut reg);
    oxbase_mysql::dialect::register(&mut reg);
    oxbase_sqlite::dialect::register(&mut reg);
    oxbase_mssql::dialect::register(&mut reg);
    Arc::new(reg)
}

/// Writes `batches` to `path` with `options` and finishes the file.
pub fn write_all(
    path: &Path,
    options: &WriteOptions,
    context: &WriteContext,
    batches: &[RecordBatch],
) -> oxbase_types::Result<SinkReport> {
    let schema = batches
        .first()
        .map_or_else(sample_schema, RecordBatch::schema);
    let mut writer = create_writer(path, false, schema, options, context)?;
    for batch in batches {
        if let Err(e) = writer.write(batch) {
            writer.abort();
            return Err(e);
        }
    }
    writer.finish()
}

/// Names of the temporary `.part` files left in `dir`.
pub fn parts(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| {
            Path::new(n)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("part"))
        })
        .collect()
}

/// Names of every file in `dir`.
pub fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}
