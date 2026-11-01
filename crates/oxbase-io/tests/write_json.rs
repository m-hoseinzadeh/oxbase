//! JSON writer: exact numbers (decimals, 2^53 + 1, u64::MAX), binary / UUID / JSON-text values,
//! layouts, pretty printing, explicit NULLs; read back with `arrow-json` (docs/m5-contracts.md
//! §3.5, §10.7, §12 write).

mod write_support;

use std::sync::Arc;

use arrow_array::RecordBatch;
use arrow_schema::Schema;
use oxbase_io::{BinaryEncoding, JsonLayout, JsonWriteOptions, WriteContext, WriteOptions};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use write_support::{TRICKY, UNICODE, UUID_1_TEXT, parts, sample, write_all};

fn json(options: JsonWriteOptions) -> WriteOptions {
    WriteOptions::Json(options)
}

fn lines() -> JsonWriteOptions {
    JsonWriteOptions {
        layout: JsonLayout::Lines,
        ..JsonWriteOptions::default()
    }
}

#[test]
fn ndjson_keeps_numbers_exact_and_values_typed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rows.ndjson");
    let report = write_all(&path, &json(lines()), &WriteContext::default(), &[sample()]).unwrap();
    let text = String::from_utf8(std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(report.rows, 4);
    assert_eq!(report.bytes, u64::try_from(text.len()).unwrap());
    assert_eq!(parts(dir.path()), Vec::<String>::new());
    // 2^53 + 1, i64::MIN and u64::MAX are exact (3 values beyond 2^53).
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(
        report.warnings[0].starts_with("3 integers"),
        "{:?}",
        report.warnings
    );

    let raw: Vec<&str> = text.lines().collect();
    assert_eq!(raw.len(), 4);
    assert!(text.ends_with("}\n"));
    // Exact number texts (no f64 on the way).
    assert!(raw[0].contains(r#""dec":12.5000000000,"#), "{}", raw[0]);
    assert!(raw[0].contains(r#""doc":{"a":[1,2.50]},"#), "{}", raw[0]);
    assert!(raw[1].contains(r#""n":9007199254740993,"#), "{}", raw[1]);
    assert!(
        raw[1].contains(r#""big":18446744073709551615,"#),
        "{}",
        raw[1]
    );
    assert!(
        raw[1].contains(r#""dec":-12345678901234567890.0123456789,"#),
        "{}",
        raw[1]
    );
    assert!(
        raw[3].contains(r#""n":-9223372036854775808,"#),
        "{}",
        raw[3]
    );

    let rows: Vec<Value> = raw
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let first = &rows[0];
    assert_eq!(first["flag"], json!(true));
    assert_eq!(first["small"], json!(-5));
    assert_eq!(first["f"], json!(0.1));
    assert_eq!(first["txt"], json!("plain"));
    // No formula guard in JSON: the text is data.
    assert_eq!(first["formula"], json!("=SUM(A1)"));
    assert_eq!(first["doc"], json!({"a": [1, 2.5]}));
    assert_eq!(first["bin"], json!("AP8="));
    assert_eq!(first["day"], json!("2024-02-29"));
    assert_eq!(first["clock"], json!("03:04:05.123456"));
    assert_eq!(first["at"], json!("2024-01-02T03:04:05.123456"));
    assert!(
        first["at_tz"]
            .as_str()
            .unwrap()
            .starts_with("2023-11-14T22:13:20.123456"),
        "{}",
        first["at_tz"]
    );
    assert_eq!(first["id"], json!(UUID_1_TEXT));
    assert_eq!(first["tags"], json!([1, 2]));
    assert_eq!(rows[1]["txt"], json!(TRICKY));
    assert_eq!(rows[1]["bin"], json!(""));
    assert_eq!(rows[1]["doc"], json!([true, null]));
    assert_eq!(rows[1]["tags"], json!([]));
    assert_eq!(rows[3]["txt"], json!(UNICODE));
    // Text marked JSON that does not parse stays a string.
    assert_eq!(rows[3]["doc"], json!("not json"));
    // Explicit NULLs: every key, value null.
    let nulls = rows[2].as_object().unwrap();
    assert_eq!(nulls.len(), 16);
    assert!(nulls.values().all(Value::is_null));
}

#[test]
fn round_trips_through_arrow_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("back.ndjson");
    let options = JsonWriteOptions {
        binary: BinaryEncoding::Hex,
        ..lines()
    };
    write_all(
        &path,
        &json(options),
        &WriteContext::default(),
        &[sample(), sample()],
    )
    .unwrap();
    let hex = std::fs::read_to_string(&path).unwrap();
    assert!(hex.contains(r#""bin":"00ff""#), "{hex}");

    // Every column arrow-json can read back from its JSON form (not the UUID strings and the
    // embedded JSON documents).
    let original = sample();
    let keep: Vec<usize> = original
        .schema()
        .fields()
        .iter()
        .enumerate()
        .filter(|(_, f)| !matches!(f.name().as_str(), "doc" | "id"))
        .map(|(i, _)| i)
        .collect();
    let projected = original.project(&keep).unwrap();
    let schema: Arc<Schema> = projected.schema();
    let reader = arrow_json::ReaderBuilder::new(Arc::clone(&schema))
        .build(std::io::BufReader::new(std::fs::File::open(&path).unwrap()))
        .unwrap();
    let batches: Vec<RecordBatch> = reader.collect::<Result<_, _>>().unwrap();
    // The reader's default batch size (1024) holds all 8 rows.
    assert_eq!(batches.len(), 1);
    let back = &batches[0];
    assert_eq!(back.num_rows(), 8);
    for (i, field) in schema.fields().iter().enumerate() {
        let expected = projected.column(i);
        let got = back.column(i).slice(0, 4);
        assert_eq!(&got, expected, "column {}", field.name());
        let again = back.column(i).slice(4, 4);
        assert_eq!(&again, expected, "column {}", field.name());
    }
}

#[test]
fn array_layout_pretty_and_implicit_nulls() {
    let dir = tempfile::tempdir().unwrap();

    let compact = dir.path().join("compact.json");
    write_all(
        &compact,
        &json(JsonWriteOptions::default()),
        &WriteContext::default(),
        &[sample(), sample()],
    )
    .unwrap();
    let compact_text = std::fs::read_to_string(&compact).unwrap();
    assert!(
        compact_text.starts_with("[\n{\"flag\":true,"),
        "{compact_text}"
    );
    assert!(compact_text.ends_with("}\n]\n"));
    let compact_rows: Vec<Value> = serde_json::from_str(&compact_text).unwrap();
    assert_eq!(compact_rows.len(), 8);

    let pretty = dir.path().join("pretty.json");
    let options = JsonWriteOptions {
        pretty: true,
        ..JsonWriteOptions::default()
    };
    write_all(
        &pretty,
        &json(options),
        &WriteContext::default(),
        &[sample()],
    )
    .unwrap();
    let pretty_text = std::fs::read_to_string(&pretty).unwrap();
    assert!(
        pretty_text.starts_with("[\n  {\n    \"flag\": true,\n    \"n\": 1,"),
        "{pretty_text}"
    );
    assert!(pretty_text.contains("\"tags\": [\n      1,\n      2\n    ]\n  },\n  {"));
    assert!(pretty_text.contains("\"tags\": []\n"));
    assert!(pretty_text.ends_with("\n  }\n]\n"));
    let pretty_rows: Vec<Value> = serde_json::from_str(&pretty_text).unwrap();
    assert_eq!(pretty_rows, compact_rows[..4].to_vec());

    let sparse = dir.path().join("sparse.json");
    let options = JsonWriteOptions {
        explicit_nulls: false,
        binary: BinaryEncoding::Hex,
        ..JsonWriteOptions::default()
    };
    write_all(
        &sparse,
        &json(options),
        &WriteContext::default(),
        &[sample()],
    )
    .unwrap();
    let sparse_rows: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(&sparse).unwrap()).unwrap();
    assert_eq!(sparse_rows[2], json!({}));
    assert_eq!(sparse_rows[0]["bin"], json!("00ff"));
}

#[test]
fn empty_exports_are_valid_files() {
    let dir = tempfile::tempdir().unwrap();
    let array = dir.path().join("empty.json");
    let report = write_all(
        &array,
        &json(JsonWriteOptions::default()),
        &WriteContext::default(),
        &[],
    )
    .unwrap();
    assert_eq!(report.rows, 0);
    assert_eq!(std::fs::read_to_string(&array).unwrap(), "[]\n");

    let ndjson = dir.path().join("empty.ndjson");
    write_all(&ndjson, &json(lines()), &WriteContext::default(), &[]).unwrap();
    assert_eq!(std::fs::read_to_string(&ndjson).unwrap(), "");
}

#[test]
fn null_typed_columns_follow_the_null_option() {
    use arrow_array::{Int64Array, NullArray};
    use arrow_schema::{DataType, Field};

    let schema = Arc::new(Schema::new(vec![
        Field::new("n", DataType::Int64, true),
        Field::new("nothing", DataType::Null, true),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![1])),
            Arc::new(NullArray::new(1)),
        ],
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    for (explicit, expected) in [
        (true, "{\"n\":1,\"nothing\":null}\n"),
        (false, "{\"n\":1}\n"),
    ] {
        let path = dir.path().join(format!("{explicit}.ndjson"));
        let options = JsonWriteOptions {
            explicit_nulls: explicit,
            ..lines()
        };
        write_all(
            &path,
            &json(options),
            &WriteContext::default(),
            std::slice::from_ref(&batch),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    }
}
