//! Parquet writer: every type preserved through each codec, row groups, the JSON marker
//! (docs/m5-contracts.md §3.5, §12 write).

mod write_support;

use std::fs::File;

use arrow_array::RecordBatch;
use oxbase_io::{ParquetCompression, ParquetWriteOptions, WriteContext, WriteOptions};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use pretty_assertions::assert_eq;
use write_support::{numbers, parts, sample, write_all};

fn read(path: &std::path::Path) -> (Vec<RecordBatch>, usize) {
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap()).unwrap();
    let row_groups = builder.metadata().num_row_groups();
    let batches = builder
        .build()
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    (batches, row_groups)
}

#[test]
fn every_codec_round_trips_every_type() {
    let dir = tempfile::tempdir().unwrap();
    let original = sample();
    for (name, compression) in [
        ("none", ParquetCompression::None),
        ("snappy", ParquetCompression::Snappy),
        ("gzip", ParquetCompression::Gzip { level: Some(6) }),
        ("brotli", ParquetCompression::Brotli { level: Some(1) }),
        ("lz4", ParquetCompression::Lz4Raw),
        ("zstd", ParquetCompression::Zstd { level: Some(3) }),
        ("zstd-default", ParquetCompression::Zstd { level: None }),
    ] {
        let path = dir.path().join(format!("{name}.parquet"));
        let options = WriteOptions::Parquet(ParquetWriteOptions {
            compression,
            ..ParquetWriteOptions::default()
        });
        let report = write_all(
            &path,
            &options,
            &WriteContext::default(),
            &[original.clone(), original.clone()],
        )
        .unwrap();
        assert_eq!(report.rows, 8, "{name}");
        assert_eq!(report.bytes, std::fs::metadata(&path).unwrap().len());

        let (batches, row_groups) = read(&path);
        assert_eq!(row_groups, 1, "{name}");
        let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
        assert_eq!(rows, 8, "{name}");
        let back = &batches[0];
        // Types and field metadata (the JSON marker) as written.
        for (got, want) in back
            .schema()
            .fields()
            .iter()
            .zip(original.schema().fields())
        {
            assert_eq!(got.data_type(), want.data_type(), "{name}: {}", want.name());
            assert_eq!(got.metadata(), want.metadata(), "{name}: {}", want.name());
        }
        for (i, field) in original.schema().fields().iter().enumerate() {
            let got = back.column(i).slice(0, 4);
            assert_eq!(&got, original.column(i), "{name}: {}", field.name());
        }
    }
    assert_eq!(parts(dir.path()), Vec::<String>::new());
}

#[test]
fn row_groups_follow_the_option() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.parquet");
    let options = WriteOptions::Parquet(ParquetWriteOptions {
        row_group_rows: 1000,
        ..ParquetWriteOptions::default()
    });
    let report = write_all(
        &path,
        &options,
        &WriteContext::default(),
        &[numbers(2500), numbers(1500)],
    )
    .unwrap();
    assert_eq!(report.rows, 4000);
    let (batches, row_groups) = read(&path);
    assert_eq!(row_groups, 4);
    let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
    assert_eq!(rows, 4000);
}

#[test]
fn bad_levels_are_refused_before_creating_anything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.parquet");
    let options = WriteOptions::Parquet(ParquetWriteOptions {
        compression: ParquetCompression::Gzip { level: Some(42) },
        ..ParquetWriteOptions::default()
    });
    assert!(write_all(&path, &options, &WriteContext::default(), &[sample()]).is_err());
    assert_eq!(write_support::files(dir.path()), Vec::<String>::new());
}
