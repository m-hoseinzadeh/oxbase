//! JSON / NDJSON reader: exact numbers, nested values as JSON text, keys after the sample,
//! malformed files and elements (docs/m5-contracts.md §3.3, §12 read).

mod common;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use common::{preview, preview_and_read, read_all, sample};
use oxbase_io::{ColumnSpec, JsonLayout, JsonReadOptions, ReadOptions};
use oxbase_types::{DataType, ErrorCategory};

fn json() -> ReadOptions {
    ReadOptions::Json(JsonReadOptions::default())
}

fn write(dir: &Path, name: &str, data: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, data).unwrap();
    path
}

fn some(values: &[&str]) -> Vec<Option<String>> {
    values.iter().map(|v| Some((*v).to_owned())).collect()
}

#[test]
fn array_values_keep_their_exact_form() {
    let (p, all) = preview_and_read(&sample("array.json"), &json()).unwrap();
    assert_eq!(p.detected.json_lines, Some(false));
    let names: Vec<&str> = p.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "id",
            "name",
            "amount",
            "big",
            "flag",
            "tags",
            "meta",
            "born",
            "extra_late"
        ]
    );
    assert_eq!(all.column(3)[0].as_deref(), Some("9007199254740993"));
    assert_eq!(
        all.column(1)[1].as_deref(),
        Some("quote \" and \\ backslash")
    );
    assert_eq!(all.column(1)[2].as_deref(), Some("naïve 日本語 🚀"));
    assert_eq!(
        all.column(5),
        vec![Some(r#"["a","b"]"#.to_owned()), Some("[]".to_owned()), None]
    );
    assert_eq!(
        all.column(6),
        vec![Some(r#"{"k":"v","n":[1,2]}"#.to_owned()), None, None]
    );
    assert_eq!(
        all.column(7),
        vec![Some("2001-02-03".to_owned()), None, None]
    );
    assert_eq!(
        all.column(8),
        vec![None, None, Some("only here".to_owned())]
    );
    assert_eq!(
        all.column(4),
        vec![Some("true".to_owned()), Some("false".to_owned()), None]
    );
    assert!(p.columns[3].nullable);

    // Decimals as written when the column is decimal.
    let mut specs = ColumnSpec::from_preview(&p);
    specs[2].data_type = DataType::Decimal {
        precision: None,
        scale: None,
    };
    let all = read_all(&sample("array.json"), &json(), &specs, 10).unwrap();
    assert_eq!(all.column(2), some(&["12.5", "-0.75", "1000"]));
    assert_eq!(all.issues.len(), 0);
}

#[test]
fn ndjson_lines_blank_lines_and_broken_lines() {
    let (p, all) = preview_and_read(&sample("lines.ndjson"), &json()).unwrap();
    assert_eq!(p.detected.json_lines, Some(true));
    assert_eq!(
        all.column(2),
        vec![
            Some(r#"{"x":1}"#.to_owned()),
            Some(r#"[1,{"y":2}]"#.to_owned()),
            None
        ]
    );

    let dir = tempfile::tempdir().unwrap();
    let path = write(
        dir.path(),
        "broken.ndjson",
        "{\"a\": 1}\n{\"a\": oops}\n\n{\"a\": 3}\r\n",
    );
    let (p, all) = preview_and_read(&path, &json()).unwrap();
    assert!(p.warnings.iter().any(|w| w.contains("not valid JSON")));
    assert_eq!(all.rows, 3);
    assert_eq!(
        all.column(0),
        vec![Some("1".to_owned()), None, Some("3".to_owned())]
    );
    assert_eq!(all.issues.len(), 1);
    assert_eq!(all.issues[0].row, 2);
    assert!(all.issues[0].message.contains("not valid JSON"));
    // Row 3 is on line 4 (a blank line before it).
    let mut specs = ColumnSpec::from_preview(&p);
    specs[0].data_type = DataType::Bool;
    let all = read_all(&path, &json(), &specs, 10).unwrap();
    let last = all.issues.last().unwrap();
    assert_eq!(last.row, 3);
    assert!(last.message.ends_with("(line 4)"), "{last:?}");
}

#[test]
fn not_objects_and_unknown_keys_are_row_issues() {
    let (_, all) = preview_and_read(&sample("not_objects.json"), &json()).unwrap();
    assert_eq!(all.column(0), vec![None, None, Some("3".to_owned())]);
    assert!(all.issues[0].message.contains("a number, not an object"));
    assert!(all.issues[1].message.contains("a string, not an object"));

    // A key first seen after the 1000-row sample.
    let dir = tempfile::tempdir().unwrap();
    let mut text = String::new();
    for i in 1..=1_000 {
        writeln!(text, "{{\"id\": {i}}}").unwrap();
    }
    text.push_str("{\"id\": 1001, \"late\": true}\n");
    let path = write(dir.path(), "late.ndjson", &text);
    let p = preview(&path, &json()).unwrap();
    assert_eq!(p.columns.len(), 1);
    assert!(!p.complete);
    let all = read_all(&path, &json(), &ColumnSpec::from_preview(&p), 300).unwrap();
    assert_eq!(all.rows, 1_001);
    assert_eq!(all.issues.len(), 1);
    assert_eq!(all.issues[0].row, 1_001);
    assert!(all.issues[0].message.contains("unknown key 'late'"));
    assert_eq!(all.column(0)[1_000].as_deref(), Some("1001"));
}

#[test]
fn malformed_files_are_errors() {
    let path = sample("truncated.json");
    let err = preview(&path, &json()).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert!(err.to_string().contains("element 2"), "{err}");
    let err = oxbase_io::open(
        &path,
        &json(),
        &[ColumnSpec::new("id", DataType::INT64)],
        10,
    )
    .err()
    .unwrap();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);

    let dir = tempfile::tempdir().unwrap();
    for (name, text, says) in [
        ("tail.json", "[{\"a\": 1}] x", "after the array"),
        ("comma.json", "[{\"a\": 1} {\"a\": 2}]", "expected ','"),
        ("bad.json", "[{\"a\": tru}]", "not valid JSON"),
        ("open.json", "[{\"a\": 1},", "ends inside the array"),
    ] {
        let path = write(dir.path(), name, text);
        let err = preview(&path, &json()).unwrap_err();
        assert_eq!(err.category(), ErrorCategory::InvalidInput, "{name}");
        assert!(err.to_string().contains(says), "{name}: {err}");
        assert!(
            err.to_string()
                .starts_with(&format!("invalid input: {name}")),
            "{err}"
        );
    }
    let path = write(dir.path(), "object.json", "{\"a\": 1}");
    let array = ReadOptions::Json(JsonReadOptions {
        layout: JsonLayout::Array,
        ..JsonReadOptions::default()
    });
    let err = preview(&path, &array).unwrap_err();
    assert!(err.to_string().contains("expected a JSON array"), "{err}");
    // The same file as JSON lines is one row.
    let (_, all) = preview_and_read(&path, &json()).unwrap();
    assert_eq!(all.rows, 1);
}

#[test]
fn json_columns_and_renamed_specs() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        dir.path(),
        "doc.ndjson",
        "{\"d\": \"x\", \"n\": 1}\n{\"d\": {\"a\": [1, 2]}, \"n\": 2}\n{\"d\": \"[3]\", \"n\": 3}\n",
    );
    let p = preview(&path, &json()).unwrap();
    assert_eq!(p.columns[0].data_type, DataType::Text);
    let mut specs = ColumnSpec::from_preview(&p);
    specs[0].data_type = DataType::Json { binary: false };
    // Renamed columns still map by position.
    specs[0].name = "document".to_owned();
    specs[1].name = "number".to_owned();
    let all = read_all(&path, &json(), &specs, 10).unwrap();
    assert_eq!(all.column(0), some(&["\"x\"", r#"{"a":[1,2]}"#, "[3]"]));
    assert_eq!(all.column(1), some(&["1", "2", "3"]));
    assert_eq!(all.batches[0].batch.schema().field(0).name(), "document");
}
