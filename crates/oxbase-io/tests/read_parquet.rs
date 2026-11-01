//! Parquet reader over files generated at runtime (`[m5.generated] parquet`): schema types,
//! exact decimals, zoned timestamps, lists as JSON, binary, NULLs, row groups and progress,
//! overrides and nanosecond values (docs/m5-contracts.md §3.3, §12 read).

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::builder::{ListBuilder, StringBuilder};
use arrow_array::cast::AsArray;
use arrow_array::types::TimestampMicrosecondType;
use arrow_array::{
    ArrayRef, BinaryArray, Date32Array, Decimal128Array, Int64Array, RecordBatch, StringArray,
    TimestampMicrosecondArray, TimestampNanosecondArray,
};
use arrow_schema::{DataType as ArrowType, Field, Schema, TimeUnit};
use common::{preview, preview_and_read, read_all};
use oxbase_io::{ColumnSpec, ParquetReadOptions, ReadOptions};
use oxbase_types::{DataType, ErrorCategory};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

const ROWS: i64 = 3_000;

fn parquet() -> ReadOptions {
    ReadOptions::Parquet(ParquetReadOptions::default())
}

fn write(path: &Path, batch: &RecordBatch, group_rows: usize) {
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .set_max_row_group_row_count(Some(group_rows))
        .build();
    let file = File::create(path).unwrap();
    let mut writer = ArrowWriter::try_new(file, batch.schema(), Some(props)).unwrap();
    writer.write(batch).unwrap();
    writer.close().unwrap();
}

/// `types.parquet`: id, price, at, day, tags, payload, note; 3 row groups of 1000 rows.
fn types_file(dir: &Path) -> PathBuf {
    let ids: Vec<i64> = (1..=ROWS).collect();
    let price = Decimal128Array::from(ids.iter().map(|i| i * 125 - 1_000).collect::<Vec<i128>>())
        .with_precision_and_scale(12, 2)
        .unwrap();
    let at = TimestampMicrosecondArray::from(
        ids.iter()
            .map(|i| 981_165_906_000_000 + i * 1_000_001)
            .collect::<Vec<_>>(),
    )
    .with_timezone("UTC");
    let day = Date32Array::from(
        ids.iter()
            .map(|i| i32::try_from(11_356 + i % 100).unwrap())
            .collect::<Vec<_>>(),
    );
    let mut tags = ListBuilder::new(StringBuilder::new());
    for i in &ids {
        if i % 10 == 0 {
            tags.append_null();
        } else {
            tags.values().append_value("a");
            tags.values().append_value(format!("t{i}"));
            tags.append(true);
        }
    }
    let payload = BinaryArray::from_iter_values(ids.iter().map(|i| i.to_le_bytes()));
    let note = StringArray::from(
        ids.iter()
            .map(|i| (i % 7 != 0).then(|| format!("note {i}")))
            .collect::<Vec<_>>(),
    );
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(ids.clone())),
        Arc::new(price),
        Arc::new(at),
        Arc::new(day),
        Arc::new(tags.finish()),
        Arc::new(payload),
        Arc::new(note),
    ];
    let schema = Schema::new(
        ["id", "price", "at", "day", "tags", "payload", "note"]
            .iter()
            .zip(&columns)
            .map(|(name, c)| Field::new(*name, c.data_type().clone(), *name != "id"))
            .collect::<Vec<_>>(),
    );
    let batch = RecordBatch::try_new(Arc::new(schema), columns).unwrap();
    let path = dir.join("types.parquet");
    write(&path, &batch, 1_000);
    path
}

#[test]
fn schema_types_and_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = types_file(dir.path());
    let p = preview(&path, &parquet()).unwrap();
    assert_eq!(p.detected.total_rows, Some(3_000));
    assert_eq!(p.sampled_rows, 1_000);
    assert!(!p.complete);
    assert_eq!(p.rows.len(), 100);
    let types: Vec<DataType> = p.columns.iter().map(|c| c.data_type.clone()).collect();
    assert_eq!(
        types,
        [
            DataType::INT64,
            DataType::Decimal {
                precision: Some(12),
                scale: Some(2)
            },
            DataType::TimestampTz { precision: Some(6) },
            DataType::Date,
            DataType::Json { binary: false },
            DataType::VarBinary { length: None },
            DataType::Text,
        ]
    );
    assert!(!p.columns[0].nullable);
    assert!(p.columns[6].nullable);
    assert_eq!(p.rows[0][1].as_deref(), Some("-8.75"));
    assert_eq!(p.rows[0][4].as_deref(), Some(r#"["a","t1"]"#));

    let all = read_all(&path, &parquet(), &ColumnSpec::from_preview(&p), 400).unwrap();
    assert_eq!(all.rows, 3_000);
    assert_eq!(all.issues.len(), 0);
    all.assert_progress();
    // Progress advances per row group, before the end.
    assert!(all.positions.iter().any(|&b| b > 0 && b < all.total_bytes));
    let prices = all.column(1);
    assert_eq!(prices[0].as_deref(), Some("-8.75"));
    assert_eq!(prices[2_999].as_deref(), Some("3740.00"));
    let at: Vec<Option<i64>> = all
        .batches
        .iter()
        .flat_map(|b| {
            b.batch
                .column(2)
                .as_primitive::<TimestampMicrosecondType>()
                .iter()
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(at[0], Some(981_165_906_000_000 + 1_000_001));
    assert_eq!(
        all.batches[0].batch.schema().field(2).data_type(),
        &ArrowType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into()))
    );
    let tags = all.column(4);
    assert_eq!(tags[1].as_deref(), Some(r#"["a","t2"]"#));
    assert_eq!(tags[9], None);
    let payload = all.batches[0].batch.column(5).as_binary::<i32>();
    assert_eq!(payload.value(0), 1_i64.to_le_bytes());
    let notes = all.column(6);
    assert_eq!(notes[6], None);
    assert_eq!(notes[7].as_deref(), Some("note 8"));
}

#[test]
fn overrides_parse_or_report() {
    let dir = tempfile::tempdir().unwrap();
    let path = types_file(dir.path());
    let p = preview(&path, &parquet()).unwrap();
    let mut specs = ColumnSpec::from_preview(&p);
    specs[0].data_type = DataType::Text;
    specs[6].data_type = DataType::INT64;
    specs[2].data_type = DataType::Timestamp { precision: None };
    let all = read_all(&path, &parquet(), &specs, 1_000).unwrap();
    assert_eq!(all.column(0)[0].as_deref(), Some("1"));
    // Every non-NULL note ("note 1") is not an integer.
    let non_null = (1..=ROWS).filter(|i| i % 7 != 0).count();
    assert_eq!(all.issues.len(), non_null);
    assert!(
        all.issues
            .iter()
            .all(|i| i.column.as_deref() == Some("note"))
    );
    assert!(all.issues[0].message.contains("'note 1' is not an integer"));
    assert_eq!(all.issues[0].row, 1);
    assert_eq!(
        all.batches[0].batch.schema().field(2).data_type(),
        &ArrowType::Timestamp(TimeUnit::Microsecond, None)
    );

    let err = oxbase_io::open(&path, &parquet(), &specs[..2], 10)
        .err()
        .unwrap();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
}

#[test]
fn nanoseconds_and_empty_files() {
    let dir = tempfile::tempdir().unwrap();
    let ns = TimestampNanosecondArray::from(vec![Some(1_000), Some(1_001), None]);
    let schema = Schema::new(vec![Field::new(
        "at",
        ArrowType::Timestamp(TimeUnit::Nanosecond, None),
        true,
    )]);
    let batch = RecordBatch::try_new(Arc::new(schema.clone()), vec![Arc::new(ns)]).unwrap();
    let path = dir.path().join("ns.parquet");
    write(&path, &batch, 1_000);
    let (p, all) = preview_and_read(&path, &parquet()).unwrap();
    assert_eq!(
        p.columns[0].data_type,
        DataType::Timestamp { precision: Some(6) }
    );
    assert_eq!(p.columns[0].warnings.len(), 1);
    assert_eq!(all.issues.len(), 1);
    assert_eq!(all.issues[0].row, 2);
    assert!(all.issues[0].message.contains("sub-microsecond"));

    let empty = RecordBatch::new_empty(Arc::new(schema));
    let path = dir.path().join("empty.parquet");
    write(&path, &empty, 1_000);
    let (p, all) = preview_and_read(&path, &parquet()).unwrap();
    assert_eq!(p.detected.total_rows, Some(0));
    assert!(p.complete);
    assert_eq!(all.rows, 0);
    all.assert_progress();

    let path = dir.path().join("fake.parquet");
    std::fs::copy(common::sample("basic.csv"), &path).unwrap();
    let err = preview(&path, &parquet()).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert!(err.to_string().contains("not a Parquet file"), "{err}");
}
