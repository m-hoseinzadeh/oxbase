//! Archive round trips in memory and the reader's robustness: flipped bytes, missing entries,
//! wrong row counts, newer format versions, truncated and foreign files never panic and are
//! reported.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;

use arrow_array::{ArrayRef, Int64Array, StringArray};
use arrow_schema::{DataType as ArrowType, Field, Schema};
use chrono::{DateTime, Utc};
use oxbase_types::{
    ColumnDef, DataType, Engine, ErrorCategory, Extras, ObjectKind, ObjectRef, TableDef, ViewDef,
};
use pretty_assertions::assert_eq;

use super::*;
use crate::format::{BackupContent, BackupSource, ManifestObject, TableData};

fn table_def() -> TableDef {
    let mut t = TableDef::new(ObjectRef::table(
        Some("oxbase_test"),
        Some("m8_src"),
        "orders",
    ));
    t.columns = vec![
        ColumnDef::new("id", DataType::INT64).not_null(),
        ColumnDef::new("note", DataType::Text),
    ];
    t
}

fn view_def() -> ObjectDef {
    ObjectDef::View(ViewDef {
        obj: ObjectRef::schema(Some("oxbase_test"), "m8_src").child(ObjectKind::View, "v"),
        definition: "SELECT id FROM m8_src.orders".into(),
        columns: Vec::new(),
        comment: None,
        extras: Extras::default(),
    })
}

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", ArrowType::Int64, false),
        Field::new("note", ArrowType::Utf8, true),
    ]))
}

fn batch(from: i64, rows: usize) -> RecordBatch {
    let ids: Vec<i64> = (from..).take(rows).collect();
    let notes: Vec<Option<String>> = ids
        .iter()
        .map(|i| (i % 3 != 0).then(|| format!("note {i} \u{1F600}")))
        .collect();
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(ids)),
        Arc::new(StringArray::from(notes)),
    ];
    RecordBatch::try_new(schema(), columns).unwrap()
}

/// Lets a test change the manifest before it is written.
fn archive_with(
    compression: Compression,
    change: impl FnOnce(&mut Manifest),
) -> (Vec<u8>, Manifest) {
    let mut w = ArchiveWriter::new(Cursor::new(Vec::new()), compression);
    let mut entries = BTreeMap::new();
    let table = ObjectDef::Table(table_def());
    let (def1, ddl1) = w
        .add_object(1, &table, Some("CREATE TABLE orders (id bigint);\n"))
        .unwrap();
    entries.insert(definition_entry(1), def1);
    entries.insert(ddl_entry(1), ddl1.unwrap());
    w.begin_table(1, &schema()).unwrap();
    w.write_rows(&batch(0, 1000)).unwrap();
    w.write_rows(&batch(1000, 7)).unwrap();
    let (data1, rows) = w.end_table().unwrap();
    assert_eq!(rows, 1007);
    entries.insert(data_entry(1), data1);
    let (def2, ddl2) = w.add_object(2, &view_def(), None).unwrap();
    assert!(ddl2.is_none());
    entries.insert(definition_entry(2), def2);
    let mut manifest = Manifest {
        format: FORMAT_NAME.into(),
        format_version: FORMAT_VERSION,
        oxbase_version: "0.8.0".into(),
        created_at: DateTime::parse_from_rfc3339("2026-11-03T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        engine: Engine::Postgres,
        server_version: "18.6".into(),
        source: BackupSource {
            container: ObjectRef::schema(Some("oxbase_test"), "m8_src"),
        },
        content: BackupContent::StructureAndData,
        compression,
        objects: vec![
            ManifestObject {
                index: 1,
                kind: ObjectKind::Table,
                object: table.object_ref(),
                definition: definition_entry(1),
                ddl: Some(ddl_entry(1)),
                data: Some(TableData {
                    entry: data_entry(1),
                    rows,
                    columns: vec!["id".into(), "note".into()],
                }),
            },
            ManifestObject {
                index: 2,
                kind: ObjectKind::View,
                object: view_def().object_ref(),
                definition: definition_entry(2),
                ddl: None,
                data: None,
            },
        ],
        order: vec![1, 2],
        entries,
        warnings: Vec::new(),
        unknown: serde_json::Map::new(),
    };
    change(&mut manifest);
    let bytes = w.finish(&manifest).unwrap().into_inner();
    (bytes, manifest)
}

fn archive() -> (Vec<u8>, Manifest) {
    archive_with(Compression::Zstd, |_| {})
}

fn reader(bytes: &[u8]) -> Result<ArchiveReader<Cursor<Vec<u8>>>> {
    ArchiveReader::from_reader(Cursor::new(bytes.to_vec()))
}

fn verify(bytes: &[u8]) -> VerifyReport {
    reader(bytes)
        .unwrap()
        .verify(&mut |_| {}, &|| false)
        .unwrap()
}

/// The data offset of entry `name` inside the archive bytes.
fn data_start(bytes: &[u8], name: &str) -> usize {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes.to_vec())).unwrap();
    let entry = zip.by_name(name).unwrap();
    usize::try_from(entry.data_start().unwrap()).unwrap()
}

#[test]
fn archives_round_trip_in_memory() {
    for compression in [Compression::Zstd, Compression::None] {
        let (bytes, manifest) = archive_with(compression, |_| {});
        let mut r = reader(&bytes).unwrap();
        assert_eq!(r.manifest(), &manifest);
        assert_eq!(r.definition(1).unwrap(), ObjectDef::Table(table_def()));
        assert_eq!(r.definition(2).unwrap(), view_def());
        assert_eq!(
            r.ddl(1).unwrap().as_deref(),
            Some("CREATE TABLE orders (id bigint);\n")
        );
        assert_eq!(r.ddl(2).unwrap(), None);
        let batches: Vec<RecordBatch> = r
            .table_rows(1)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(batches, vec![batch(0, 1000), batch(1000, 7)]);
        assert!(r.table_rows(2).is_err(), "a view has no rows");
        assert!(r.definition(3).is_err());

        let mut seen = 0;
        let report = r.verify(&mut |b| seen = b, &|| false).unwrap();
        assert!(report.ok, "{report:?}");
        assert_eq!(report.entries_checked, 4);
        assert_eq!(report.bytes_checked, seen);
        assert_eq!(
            report.bytes_checked,
            manifest.entries.values().map(|e| e.size).sum::<u64>()
        );
    }
}

#[test]
fn zstd_makes_repetitive_rows_smaller() {
    let (zstd, _) = archive_with(Compression::Zstd, |_| {});
    let (plain, _) = archive_with(Compression::None, |_| {});
    assert!(zstd.len() < plain.len(), "{} < {}", zstd.len(), plain.len());
}

#[test]
fn verify_catches_a_flipped_byte() {
    let (mut bytes, _) = archive();
    let at = data_start(&bytes, &data_entry(1)) + 200;
    bytes[at] ^= 0x5a;
    let report = verify(&bytes);
    assert!(!report.ok);
    assert_eq!(report.problems.len(), 1, "{report:?}");
    assert_eq!(report.problems[0].entry, data_entry(1));
    assert!(
        report.problems[0]
            .problem
            .as_deref()
            .unwrap()
            .contains("checksum"),
        "{report:?}"
    );

    // A flipped byte in a definition: the definition is refused.
    let (mut bytes, _) = archive();
    let at = data_start(&bytes, &definition_entry(2)) + 3;
    bytes[at] ^= 0xff;
    assert!(!verify(&bytes).ok);
    assert_eq!(
        reader(&bytes)
            .unwrap()
            .definition(2)
            .unwrap_err()
            .category(),
        ErrorCategory::InvalidInput
    );
}

#[test]
fn verify_catches_a_missing_entry_and_a_wrong_row_count() {
    let (bytes, _) = archive_with(Compression::Zstd, |m| {
        m.entries.insert(
            "objects/0009.sql".into(),
            EntryInfo {
                size: 3,
                sha256: sha256_hex(b"abc"),
            },
        );
    });
    let report = verify(&bytes);
    assert!(!report.ok);
    assert_eq!(report.problems[0].entry, "objects/0009.sql");
    assert!(
        report.problems[0]
            .problem
            .as_deref()
            .unwrap()
            .contains("missing")
    );

    let (bytes, _) = archive_with(Compression::Zstd, |m| {
        m.objects[0].data.as_mut().unwrap().rows = 1008;
    });
    let report = verify(&bytes);
    assert!(!report.ok);
    assert_eq!(report.problems[0].entry, data_entry(1));
    assert!(
        report.problems[0]
            .problem
            .as_deref()
            .unwrap()
            .contains("1007 rows"),
        "{report:?}"
    );
    let mut r = reader(&bytes).unwrap();
    let last = r.table_rows(1).unwrap().last().unwrap();
    assert!(last.unwrap_err().to_string().contains("1007 rows"));
}

#[test]
fn verify_selected_objects_only_and_stop_on_cancel() {
    let (mut bytes, _) = archive();
    let at = data_start(&bytes, &data_entry(1)) + 100;
    bytes[at] ^= 1;
    let mut r = reader(&bytes).unwrap();
    let view_only = r
        .verify_objects(&BTreeSet::from([2]), true, &mut |_| {}, &|| false)
        .unwrap();
    assert!(view_only.ok);
    assert_eq!(view_only.entries_checked, 1);
    let structure = r
        .verify_objects(&BTreeSet::from([1]), false, &mut |_| {}, &|| false)
        .unwrap();
    assert!(structure.ok, "the rows are not checked: {structure:?}");
    let all = r
        .verify_objects(&BTreeSet::from([1]), true, &mut |_| {}, &|| false)
        .unwrap();
    assert!(!all.ok);
    let cancelled = r.verify(&mut |_| {}, &|| true).unwrap_err();
    assert!(cancelled.is_cancelled());
}

#[test]
fn newer_or_foreign_files_are_refused() {
    let (bytes, _) = archive_with(Compression::Zstd, |m| m.format_version = FORMAT_VERSION + 1);
    let err = reader(&bytes).err().unwrap();
    assert_eq!(err.category(), ErrorCategory::Unsupported);
    assert!(err.to_string().contains("format version 2"), "{err}");

    let (bytes, _) = archive_with(Compression::Zstd, |m| m.format = "something".into());
    assert_eq!(
        reader(&bytes).err().unwrap().category(),
        ErrorCategory::InvalidInput
    );

    // Not a ZIP file, an empty file, a ZIP without a manifest.
    for bad in [&b"hello, this is not a backup"[..], &[][..]] {
        assert_eq!(
            reader(bad).err().unwrap().category(),
            ErrorCategory::InvalidInput
        );
    }
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("hello.txt", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"hi").unwrap();
    let other = zip.finish().unwrap().into_inner();
    assert!(
        reader(&other)
            .err()
            .unwrap()
            .to_string()
            .contains("no manifest")
    );
}

#[test]
fn truncated_archives_fail_cleanly() {
    let (bytes, _) = archive();
    for cut in [10, bytes.len() / 3, bytes.len() / 2, bytes.len() - 30] {
        let short = &bytes[..cut];
        // Opening may fail (no central directory) or verification may: never a panic.
        if let Ok(mut r) = reader(short) {
            let report = r.verify(&mut |_| {}, &|| false);
            assert!(
                !matches!(report, Ok(VerifyReport { ok: true, .. })),
                "cut at {cut}"
            );
        }
    }
}

#[test]
fn no_byte_flip_panics_the_reader() {
    let (bytes, _) = archive_with(Compression::Zstd, |_| {});
    // Positions all over the archive (every byte of the manifest and the ZIP directory at the
    // end), one at a time: open, verify and definitions end in Ok or Err. Table data is never
    // decoded unless its checksum matched (verify), so no corrupted IPC reaches Arrow here.
    let tail = bytes.len().saturating_sub(1500);
    let positions = (0..tail).step_by(5).chain(tail..bytes.len());
    for at in positions {
        let mut copy = bytes.clone();
        copy[at] ^= 0xa5;
        if let Ok(mut r) = reader(&copy) {
            let _ = r.verify(&mut |_| {}, &|| false);
            let _ = r.definition(1);
            let _ = r.definition(2);
        }
    }
}
