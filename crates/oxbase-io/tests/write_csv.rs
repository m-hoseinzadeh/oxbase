//! CSV / TSV writer: value text forms, quoting, encodings and BOMs, the formula guard, options
//! (docs/m5-contracts.md §3.5, §3.8, §12 write).

mod write_support;

use oxbase_io::{
    CsvWriteOptions, LineEnding, QuoteStyle, TextEncoding, ValueFormats, WriteContext, WriteOptions,
};
use oxbase_types::ErrorCategory;
use pretty_assertions::assert_eq;
use write_support::{COLUMNS, TRICKY, UNICODE, UUID_1_TEXT, parts, sample, write_all};

fn expected_rows() -> Vec<Vec<String>> {
    let rows: [[&str; 16]; 4] = [
        [
            "true",
            "1",
            "0",
            "-5",
            "0.1",
            "12.5000000000",
            "plain",
            "'=SUM(A1)",
            "{\"a\": [1, 2.50]}",
            "0x00ff",
            "2024-02-29",
            "03:04:05.123456",
            "2024-01-02 03:04:05.123456",
            "2023-11-14 22:13:20.123456Z",
            UUID_1_TEXT,
            "[1,2]",
        ],
        [
            "false",
            "9007199254740993",
            "18446744073709551615",
            "2147483647",
            "-1.5e300",
            "-12345678901234567890.0123456789",
            TRICKY,
            "'+1",
            "[true,null]",
            "0x",
            "1899-12-31",
            "23:59:59",
            "1970-01-01 00:00:00",
            "1969-12-31 23:59:59Z",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
            "[]",
        ],
        [""; 16],
        [
            "true",
            "-9223372036854775808",
            "42",
            "0",
            "1.0",
            "0.0000000000",
            UNICODE,
            "'-2",
            "not json",
            "0x616263",
            "9999-12-31",
            "00:00:00",
            "2038-01-19 03:14:08",
            "1970-01-01 00:00:00.001Z",
            "00000000-0000-0000-0000-000000000000",
            "[3]",
        ],
    ];
    rows.iter()
        .map(|r| r.iter().map(|s| (*s).to_owned()).collect())
        .collect()
}

/// Parses `bytes` with the `csv` crate (headers on).
fn parse(bytes: &[u8], builder: &mut csv::ReaderBuilder) -> (Vec<String>, Vec<Vec<String>>) {
    let mut reader = builder.from_reader(bytes);
    let headers = reader
        .headers()
        .unwrap()
        .iter()
        .map(str::to_owned)
        .collect();
    let rows = reader
        .records()
        .map(|r| r.unwrap().iter().map(str::to_owned).collect())
        .collect();
    (headers, rows)
}

fn csv(options: CsvWriteOptions) -> WriteOptions {
    WriteOptions::Csv(options)
}

#[test]
fn writes_every_type_as_text_and_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("all.csv");
    let report = write_all(
        &path,
        &csv(CsvWriteOptions::default()),
        &WriteContext::default(),
        &[sample(), sample()],
    )
    .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(report.rows, 8);
    assert_eq!(report.bytes, u64::try_from(bytes.len()).unwrap());
    assert_eq!(report.warnings, Vec::<String>::new());
    assert_eq!(parts(dir.path()), Vec::<String>::new());

    let (headers, rows) = parse(&bytes, &mut csv::ReaderBuilder::new());
    assert_eq!(headers, COLUMNS);
    let mut expected = expected_rows();
    expected.extend(expected_rows());
    assert_eq!(rows, expected);
    // LF line ends, no BOM, the CR LF inside the quoted text kept as is.
    assert!(bytes.starts_with(b"flag,n,big,"));
    assert!(!bytes.starts_with(b"\xEF\xBB\xBF"));
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("\"it's \"\"quoted\"\", a;b\tc\r\nd\\e\""));
    assert!(text.ends_with("[3]\n"));
}

#[test]
fn formula_guard_can_be_turned_off() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raw.csv");
    let options = CsvWriteOptions {
        formula_guard: false,
        ..CsvWriteOptions::default()
    };
    write_all(&path, &csv(options), &WriteContext::default(), &[sample()]).unwrap();
    let (_, rows) = parse(
        &std::fs::read(&path).unwrap(),
        &mut csv::ReaderBuilder::new(),
    );
    let formulas: Vec<&str> = rows.iter().map(|r| r[7].as_str()).collect();
    assert_eq!(formulas, ["=SUM(A1)", "+1", "", "-2"]);
    // Numbers are never guarded, with or without the option.
    assert_eq!(rows[3][1], "-9223372036854775808");
}

#[test]
fn delimiter_quotes_line_ends_nulls_and_decimal_comma() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("eu.csv");
    let options = CsvWriteOptions {
        delimiter: ';',
        quote_style: QuoteStyle::Always,
        line_ending: LineEnding::CrLf,
        null_token: "\\N".to_owned(),
        header: false,
        formats: ValueFormats {
            decimal_separator: ',',
            date_format: Some("%d.%m.%Y".to_owned()),
            ..ValueFormats::default()
        },
        ..CsvWriteOptions::default()
    };
    write_all(&path, &csv(options), &WriteContext::default(), &[sample()]).unwrap();
    let text = String::from_utf8(std::fs::read(&path).unwrap()).unwrap();
    assert!(
        text.starts_with("\"true\";\"1\";\"0\";\"-5\";\"0,1\";\"12,5000000000\";\"plain\";"),
        "{text}"
    );
    assert!(text.contains("\"29.02.2024\""));
    assert!(text.ends_with("\"[3]\"\r\n"));
    let (_, rows) = parse(
        text.as_bytes(),
        csv::ReaderBuilder::new().has_headers(false).delimiter(b';'),
    );
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[2], vec!["\\N".to_owned(); 16]);
    assert_eq!(rows[1][6], TRICKY);
}

#[test]
fn tsv_and_backslash_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.tsv");
    let options = CsvWriteOptions {
        delimiter: '\t',
        escape: Some('\\'),
        ..CsvWriteOptions::default()
    };
    write_all(&path, &csv(options), &WriteContext::default(), &[sample()]).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.starts_with(b"flag\tn\tbig\t"));
    let (headers, rows) = parse(
        &bytes,
        csv::ReaderBuilder::new()
            .delimiter(b'\t')
            .double_quote(false)
            .escape(Some(b'\\')),
    );
    assert_eq!(headers, COLUMNS);
    assert_eq!(rows, expected_rows());
}

#[test]
fn never_quoting_refuses_values_that_need_quotes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.csv");
    let options = CsvWriteOptions {
        quote_style: QuoteStyle::Never,
        ..CsvWriteOptions::default()
    };
    let err = write_all(&path, &csv(options), &WriteContext::default(), &[sample()]).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert!(err.to_string().contains("quoting is off"), "{err}");
    assert!(!path.exists());
    assert_eq!(parts(dir.path()), Vec::<String>::new());
}

#[test]
fn utf16_with_bom_matches_the_utf8_text() {
    let dir = tempfile::tempdir().unwrap();
    let utf8 = dir.path().join("utf8.csv");
    write_all(
        &utf8,
        &csv(CsvWriteOptions::default()),
        &WriteContext::default(),
        &[sample()],
    )
    .unwrap();
    let expected = String::from_utf8(std::fs::read(&utf8).unwrap()).unwrap();

    for (encoding, bom, codec) in [
        (
            TextEncoding::Utf16Le,
            &b"\xFF\xFE"[..],
            encoding_rs::UTF_16LE,
        ),
        (
            TextEncoding::Utf16Be,
            &b"\xFE\xFF"[..],
            encoding_rs::UTF_16BE,
        ),
    ] {
        let path = dir.path().join(format!("{encoding:?}.csv"));
        let options = CsvWriteOptions {
            encoding,
            bom: true,
            ..CsvWriteOptions::default()
        };
        write_all(&path, &csv(options), &WriteContext::default(), &[sample()]).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(bom), "{encoding:?}");
        let (text, had_errors) = codec.decode_without_bom_handling(&bytes[2..]);
        assert!(!had_errors);
        assert_eq!(text, expected);
    }

    let bom8 = dir.path().join("bom8.csv");
    let options = CsvWriteOptions {
        bom: true,
        ..CsvWriteOptions::default()
    };
    write_all(&bom8, &csv(options), &WriteContext::default(), &[sample()]).unwrap();
    let bytes = std::fs::read(&bom8).unwrap();
    assert_eq!(&bytes[..3], b"\xEF\xBB\xBF");
    assert_eq!(&bytes[3..], expected.as_bytes());
}

#[test]
fn single_byte_encodings_write_or_refuse() {
    use std::sync::Arc;

    use arrow_array::{RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema};

    let dir = tempfile::tempdir().unwrap();
    let schema = Arc::new(Schema::new(vec![Field::new("s", DataType::Utf8, true)]));
    let euro = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![Arc::new(StringArray::from(vec!["€5", "é"]))],
    )
    .unwrap();

    let cp1252 = dir.path().join("cp1252.csv");
    let options = CsvWriteOptions {
        encoding: TextEncoding::Windows1252,
        ..CsvWriteOptions::default()
    };
    write_all(
        &cp1252,
        &csv(options),
        &WriteContext::default(),
        std::slice::from_ref(&euro),
    )
    .unwrap();
    assert_eq!(std::fs::read(&cp1252).unwrap(), b"s\n\x805\n\xe9\n");

    // Latin-1 has no euro sign: the export fails, naming the character, and leaves nothing.
    let latin1 = dir.path().join("latin1.csv");
    let options = CsvWriteOptions {
        encoding: TextEncoding::Latin1,
        ..CsvWriteOptions::default()
    };
    let err = write_all(&latin1, &csv(options), &WriteContext::default(), &[euro]).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert!(err.to_string().contains("U+20AC"), "{err}");
    assert!(!latin1.exists());
    assert_eq!(parts(dir.path()), Vec::<String>::new());
}

#[test]
fn bad_formats_are_refused_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fmt.csv");
    let options = CsvWriteOptions {
        formats: ValueFormats {
            timestamp_format: Some("%Q".to_owned()),
            ..ValueFormats::default()
        },
        ..CsvWriteOptions::default()
    };
    let err = write_all(&path, &csv(options), &WriteContext::default(), &[sample()]).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert!(!path.exists());
    assert_eq!(parts(dir.path()), Vec::<String>::new());
}

#[test]
fn null_typed_columns_and_values_that_look_like_null() {
    use std::sync::Arc;

    use arrow_array::{NullArray, RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema};

    // `SELECT NULL` on MySQL and SQL Server gives a `Null`-typed column: every cell is NULL.
    let schema = Arc::new(Schema::new(vec![
        Field::new("s", DataType::Utf8, true),
        Field::new("nothing", DataType::Null, true),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(vec![Some(""), Some("\\N"), None])),
            Arc::new(NullArray::new(3)),
        ],
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();

    let path = dir.path().join("token.csv");
    let options = CsvWriteOptions {
        null_token: "\\N".to_owned(),
        header: false,
        ..CsvWriteOptions::default()
    };
    let report = write_all(
        &path,
        &csv(options),
        &WriteContext::default(),
        std::slice::from_ref(&batch),
    )
    .unwrap();
    let text = String::from_utf8(std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(text, ",\\N\n\\N,\\N\n\\N,\\N\n");
    assert_eq!(
        report.warnings,
        [
            "1 value equal to the NULL token ('\\N') cannot be told apart from NULL when the \
          file is read back"
        ]
    );

    // With the default empty token, the empty string is the ambiguous one.
    let path = dir.path().join("empty.csv");
    let options = CsvWriteOptions {
        header: false,
        ..CsvWriteOptions::default()
    };
    let report = write_all(&path, &csv(options), &WriteContext::default(), &[batch]).unwrap();
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(
        report.warnings[0].starts_with("1 value equal to the NULL token (empty)"),
        "{:?}",
        report.warnings
    );
}
