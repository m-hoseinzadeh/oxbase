//! What every writer guarantees about files: nothing at the destination before `finish`, no
//! file after an abort or a failure, an existing file never damaged, overwrite only when asked,
//! progress counters, the async handle (docs/m5-contracts.md §3.5, §10.3, §12 write).

mod write_support;

use std::sync::Arc;

use arrow_array::{RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use oxbase_io::{
    CsvWriteOptions, JsonWriteOptions, ParquetWriteOptions, SqlDumpOptions, TextEncoding,
    WriteContext, WriteOptions, XlsxWriteOptions, create_writer, spawn_writer,
};
use oxbase_types::{Engine, ErrorCategory};
use pretty_assertions::assert_eq;
use write_support::{dialects, files, numbers, parts, sample, sample_schema};

fn every_format() -> Vec<(&'static str, WriteOptions)> {
    vec![
        ("out.csv", WriteOptions::Csv(CsvWriteOptions::default())),
        ("out.json", WriteOptions::Json(JsonWriteOptions::default())),
        ("out.xlsx", WriteOptions::Xlsx(XlsxWriteOptions::default())),
        (
            "out.parquet",
            WriteOptions::Parquet(ParquetWriteOptions::default()),
        ),
        (
            "out.sql",
            WriteOptions::SqlDump(SqlDumpOptions::new(Engine::Postgres)),
        ),
    ]
}

fn context() -> WriteContext {
    WriteContext {
        dialects: Some(dialects()),
        ..WriteContext::default()
    }
}

#[test]
fn nothing_appears_before_finish_and_abort_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    for (name, options) in every_format() {
        let path = dir.path().join(name);
        let mut writer =
            create_writer(&path, false, sample_schema(), &options, &context()).unwrap();
        writer.write(&sample()).unwrap();
        writer.write(&sample()).unwrap();
        assert_eq!(writer.rows_written(), 8, "{name}");
        assert!(!path.exists(), "{name}");
        assert_eq!(parts(dir.path()).len(), 1, "{name}");
        writer.abort();
        assert_eq!(files(dir.path()), Vec::<String>::new(), "{name}");

        // Dropping a writer is an abort too.
        let mut writer =
            create_writer(&path, false, sample_schema(), &options, &context()).unwrap();
        writer.write(&sample()).unwrap();
        drop(writer);
        assert_eq!(files(dir.path()), Vec::<String>::new(), "{name}");

        let mut writer =
            create_writer(&path, false, sample_schema(), &options, &context()).unwrap();
        writer.write(&sample()).unwrap();
        let report = writer.finish().unwrap();
        assert_eq!(report.rows, 4, "{name}");
        assert_eq!(files(dir.path()), vec![name.to_owned()]);
        assert_eq!(
            report.bytes,
            std::fs::metadata(&path).unwrap().len(),
            "{name}"
        );
        std::fs::remove_file(&path).unwrap();
    }
}

#[test]
fn existing_files_are_kept_unless_overwrite_and_never_damaged() {
    let dir = tempfile::tempdir().unwrap();
    for (name, options) in every_format() {
        let path = dir.path().join(name);
        std::fs::write(&path, b"old").unwrap();

        // Exists and no overwrite: refused before anything is created.
        let err = create_writer(&path, false, sample_schema(), &options, &context()).err();
        let err = err.expect("refused");
        assert_eq!(err.category(), ErrorCategory::InvalidInput, "{name}");
        assert!(!err.to_string().contains(&*dir.path().to_string_lossy()));
        assert_eq!(parts(dir.path()), Vec::<String>::new());

        // Overwrite: the old file stays until finish, and through an abort.
        let mut writer = create_writer(&path, true, sample_schema(), &options, &context()).unwrap();
        writer.write(&sample()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"old", "{name}");
        writer.abort();
        assert_eq!(std::fs::read(&path).unwrap(), b"old", "{name}");

        let mut writer = create_writer(&path, true, sample_schema(), &options, &context()).unwrap();
        writer.write(&sample()).unwrap();
        writer.finish().unwrap();
        assert_ne!(std::fs::read(&path).unwrap(), b"old", "{name}");
        assert_eq!(files(dir.path()), vec![name.to_owned()]);
        std::fs::remove_file(&path).unwrap();
    }
}

/// Unix: an exported file gets the mode any new file gets (0666 less the umask, compared with
/// a file `std::fs::File::create` makes next to it), not a temporary file's 0600; a replaced
/// file keeps its own mode.
#[cfg(unix)]
#[test]
fn exported_files_get_the_usual_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    let dir = tempfile::tempdir().unwrap();
    let reference = dir.path().join("reference");
    std::fs::File::create(&reference).unwrap();
    let usual = mode(&reference);
    std::fs::remove_file(&reference).unwrap();

    for (name, options) in every_format() {
        let path = dir.path().join(name);
        let mut writer =
            create_writer(&path, false, sample_schema(), &options, &context()).unwrap();
        writer.write(&sample()).unwrap();
        writer.finish().unwrap();
        assert_eq!(mode(&path), usual, "{name}: a new file");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let mut writer = create_writer(&path, true, sample_schema(), &options, &context()).unwrap();
        writer.write(&sample()).unwrap();
        writer.finish().unwrap();
        assert_eq!(mode(&path), 0o640, "{name}: a replaced file");
        std::fs::remove_file(&path).unwrap();
    }
}

#[test]
fn a_failed_export_keeps_the_old_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("report.csv");
    std::fs::write(&path, b"old").unwrap();
    let schema = Arc::new(Schema::new(vec![Field::new("s", DataType::Utf8, true)]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![Arc::new(StringArray::from(vec!["日本"]))],
    )
    .unwrap();
    let options = WriteOptions::Csv(CsvWriteOptions {
        encoding: TextEncoding::Latin1,
        ..CsvWriteOptions::default()
    });
    let mut writer =
        create_writer(&path, true, schema, &options, &WriteContext::default()).unwrap();
    let failed = writer.write(&batch).is_err();
    let err = if failed {
        writer.abort();
        None
    } else {
        writer.finish().err()
    };
    assert!(failed || err.is_some());
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
    assert_eq!(files(dir.path()), vec!["report.csv".to_owned()]);
}

#[test]
fn byte_counters_grow_and_match_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.csv");
    let schema = numbers(1).schema();
    let mut writer = create_writer(
        &path,
        false,
        schema,
        &WriteOptions::Csv(CsvWriteOptions::default()),
        &WriteContext::default(),
    )
    .unwrap();
    let mut last = 0;
    for _ in 0..20 {
        writer.write(&numbers(10_000)).unwrap();
        let now = writer.bytes_written();
        assert!(now >= last);
        last = now;
    }
    // The CSV writer buffers at most 64 KiB, so most bytes are counted before finish.
    assert!(last > 900_000, "{last}");
    let report = writer.finish().unwrap();
    assert_eq!(report.rows, 200_000);
    assert_eq!(report.bytes, std::fs::metadata(&path).unwrap().len());
    assert!(report.bytes >= last);
}

#[tokio::test]
async fn the_async_handle_writes_finishes_and_aborts() {
    let dir = tempfile::tempdir().unwrap();
    for (name, options) in every_format() {
        let path = dir.path().join(name);
        let writer = create_writer(&path, false, sample_schema(), &options, &context()).unwrap();
        let mut handle = spawn_writer(writer, 2);
        for _ in 0..5 {
            handle.write(sample()).await.unwrap();
        }
        let report = handle.finish().await.unwrap();
        assert_eq!(report.rows, 20, "{name}");
        assert!(path.exists(), "{name}");

        let other = dir.path().join(format!("aborted-{name}"));
        let writer = create_writer(&other, false, sample_schema(), &options, &context()).unwrap();
        let mut handle = spawn_writer(writer, 2);
        handle.write(sample()).await.unwrap();
        handle.abort().await;
        assert!(!other.exists(), "{name}");
        assert_eq!(parts(dir.path()), Vec::<String>::new(), "{name}");
    }
}
