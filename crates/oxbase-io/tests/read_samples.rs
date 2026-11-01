//! Every committed sample in `tests/data/m5` against its `[m5.files."<name>"]` expectations in
//! `tests/fixtures.toml` (docs/m5-contracts.md §8, §12 read): rows, columns, names, types,
//! values, NULLs, row issues, encodings, BOM and errors.

mod read_support;

use std::path::Path;

use oxbase_io::{ColumnSpec, CsvReadOptions, JsonReadOptions, ReadOptions, TextEncoding};
use toml::Value;

/// The read options each sample is meant to be read with (the comments in fixtures.toml).
fn options_for(name: &str) -> ReadOptions {
    let json = Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json") || e.eq_ignore_ascii_case("ndjson"));
    if json {
        return ReadOptions::Json(JsonReadOptions::default());
    }
    let tsv = Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("tsv"));
    let base = if tsv {
        CsvReadOptions::tsv()
    } else {
        CsvReadOptions::default()
    };
    let options = match name {
        "latin1.csv" => CsvReadOptions {
            encoding: TextEncoding::Latin1,
            ..base
        },
        "no_header.csv" => CsvReadOptions {
            header: false,
            ..base
        },
        "semicolon_decimal_comma.csv" => {
            let mut formats = base.formats.clone();
            formats.decimal_separator = ',';
            formats.thousands_separator = Some('.');
            CsvReadOptions {
                delimiter: ';',
                formats,
                ..base
            }
        }
        "null_tokens.csv" => CsvReadOptions {
            null_tokens: ["", "\\N", "NULL"].map(String::from).to_vec(),
            ..base
        },
        _ => base,
    };
    ReadOptions::Csv(options)
}

fn count(expect: &toml::Table, key: &str) -> Option<usize> {
    expect
        .get(key)
        .and_then(Value::as_integer)
        .map(|n| usize::try_from(n).unwrap())
}

fn strings(expect: &toml::Table, key: &str) -> Option<Vec<String>> {
    expect.get(key).and_then(Value::as_array).map(|values| {
        values
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    })
}

#[test]
fn every_sample_reads_as_expected() {
    let fixtures = read_support::fixtures();
    let files = fixtures["m5"]["files"].as_table().expect("[m5.files]");
    assert!(files.len() >= 20, "only {} samples listed", files.len());
    for (name, expect) in files {
        let expect = expect.as_table().unwrap();
        let path = read_support::sample(name);
        assert!(path.is_file(), "{name} is listed but missing");
        let options = options_for(name);

        if let Some(error) = expect.get("error").and_then(Value::as_str) {
            let err = read_support::preview_and_read(&path, &options)
                .err()
                .unwrap_or_else(|| panic!("{name}: expected an error"));
            assert_eq!(err.category().to_string(), error, "{name}: {err}");
            assert!(
                !err.to_string().contains(&*path.to_string_lossy()),
                "{name}: {err}"
            );
            continue;
        }

        let preview = read_support::preview(&path, &options)
            .unwrap_or_else(|e| panic!("{name}: preview: {e}"));
        assert!(preview.complete, "{name}: small samples are read whole");
        assert_eq!(
            preview.total_bytes,
            std::fs::metadata(&path).unwrap().len(),
            "{name}"
        );
        if let Some(columns) = count(expect, "columns") {
            assert_eq!(preview.columns.len(), columns, "{name}: columns");
        }
        if let Some(names) = strings(expect, "names") {
            let found: Vec<&str> = preview.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(found, names, "{name}: names");
        }
        if let Some(types) = strings(expect, "types") {
            let found: Vec<&str> = preview
                .columns
                .iter()
                .map(|c| read_support::kind(&c.data_type))
                .collect();
            assert_eq!(found, types, "{name}: types");
        }
        if let Some(encoding) = expect.get("encoding").and_then(Value::as_str) {
            let found = serde_json::to_value(preview.detected.encoding).unwrap();
            assert_eq!(found.as_str(), Some(encoding), "{name}: encoding");
        }
        if let Some(bom) = expect.get("bom").and_then(Value::as_bool) {
            assert_eq!(preview.detected.bom, bom, "{name}: bom");
        }

        let specs = ColumnSpec::from_preview(&preview);
        if specs.is_empty() {
            assert_eq!(count(expect, "rows"), Some(0), "{name}");
            assert_eq!(preview.rows.len(), 0, "{name}");
            assert_ne!(preview.warnings.len(), 0, "{name}: an empty file is noted");
            let err = oxbase_io::open(&path, &options, &specs, 10).err().unwrap();
            assert_eq!(
                err.category(),
                oxbase_types::ErrorCategory::InvalidInput,
                "{name}"
            );
            continue;
        }
        let all = read_support::read_all(&path, &options, &specs, 2)
            .unwrap_or_else(|e| panic!("{name}: read: {e}"));
        all.assert_progress();
        if let Some(rows) = count(expect, "rows") {
            assert_eq!(all.rows, rows, "{name}: rows");
            assert_eq!(preview.sampled_rows, rows, "{name}: sampled rows");
        }
        assert_eq!(
            all.issues.len(),
            count(expect, "issues").unwrap_or(0),
            "{name}: issues {:?}",
            all.issues
        );
        if let Some(values) = strings(expect, "values") {
            let text = all.all_text();
            for value in values {
                assert!(text.contains(&value), "{name}: {value:?} not in {text:?}");
            }
        }
        if let Some(nulls) = count(expect, "nulls") {
            assert_eq!(all.nulls(), nulls, "{name}: NULLs");
        }
    }
}
