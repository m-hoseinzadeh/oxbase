//! XLSX writer: typed cells, exactness fallbacks to text, formula injection, sheet splitting,
//! the cell text limit; read back with `calamine` (docs/m5-contracts.md §3.5, §3.8, §12 write).

mod write_support;

use std::sync::Arc;

use arrow_array::{RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use calamine::{Data, Reader, Xlsx, open_workbook};
use oxbase_io::{WriteContext, WriteOptions, XlsxWriteOptions};
use pretty_assertions::assert_eq;
use write_support::{
    COLUMNS, TRICKY, UNICODE, UUID_1_TEXT, date, numbers, parts, sample, write_all,
};

fn xlsx(options: XlsxWriteOptions) -> WriteOptions {
    WriteOptions::Xlsx(options)
}

fn open(path: &std::path::Path) -> Xlsx<std::io::BufReader<std::fs::File>> {
    open_workbook(path).unwrap()
}

fn cell(range: &calamine::Range<Data>, row: u32, col: u32) -> Data {
    range.get_value((row, col)).cloned().unwrap_or(Data::Empty)
}

fn text(s: &str) -> Data {
    Data::String(s.to_owned())
}

fn number(range: &calamine::Range<Data>, row: u32, col: u32) -> f64 {
    match cell(range, row, col) {
        Data::Float(v) => v,
        Data::Int(v) => i32::try_from(v).map(f64::from).unwrap(),
        other => panic!("({row}, {col}) is not a number: {other:?}"),
    }
}

#[test]
fn typed_cells_and_exact_fallbacks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("all.xlsx");
    let report = write_all(
        &path,
        &xlsx(XlsxWriteOptions::default()),
        &WriteContext::default(),
        &[sample()],
    )
    .unwrap();
    assert_eq!(report.rows, 4);
    assert_eq!(report.sheets, ["Sheet1"]);
    assert_eq!(
        report.bytes,
        std::fs::metadata(&path).unwrap().len(),
        "report size"
    );
    assert_eq!(parts(dir.path()), Vec::<String>::new());
    let warnings = report.warnings.join("\n");
    assert!(warnings.contains("3 integers"), "{warnings}");
    assert!(warnings.contains("1 decimal"), "{warnings}");
    assert!(warnings.contains("1 date"), "{warnings}");

    let mut book = open(&path);
    assert_eq!(book.sheet_names(), ["Sheet1"]);
    let range = book.worksheet_range("Sheet1").unwrap();
    assert_eq!(range.height(), 5);
    let header: Vec<Data> = (0..16).map(|c| cell(&range, 0, c)).collect();
    let names: Vec<Data> = COLUMNS.iter().map(|n| text(n)).collect();
    assert_eq!(header, names);

    // Row 1: ordinary values, typed.
    assert_eq!(cell(&range, 1, 0), Data::Bool(true));
    assert!((number(&range, 1, 1) - 1.0).abs() < f64::EPSILON);
    assert!((number(&range, 1, 3) + 5.0).abs() < f64::EPSILON);
    assert!((number(&range, 1, 4) - 0.1).abs() < f64::EPSILON);
    assert!((number(&range, 1, 5) - 12.5).abs() < f64::EPSILON);
    assert_eq!(cell(&range, 1, 6), text("plain"));
    // Formula-looking text is a string cell, never a formula.
    assert_eq!(cell(&range, 1, 7), text("=SUM(A1)"));
    assert_eq!(cell(&range, 1, 8), text("{\"a\": [1, 2.50]}"));
    assert_eq!(cell(&range, 1, 9), text("0x00ff"));
    let Data::DateTime(day) = cell(&range, 1, 10) else {
        panic!("not a date: {:?}", cell(&range, 1, 10));
    };
    assert_eq!(day.as_datetime().unwrap().date(), date(2024, 2, 29));
    let Data::DateTime(at) = cell(&range, 1, 12) else {
        panic!("not a timestamp: {:?}", cell(&range, 1, 12));
    };
    assert_eq!(
        at.as_datetime()
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        "2024-01-02 03:04:05"
    );
    assert_eq!(cell(&range, 1, 14), text(UUID_1_TEXT));
    assert_eq!(cell(&range, 1, 15), text("[1,2]"));

    // Row 2: values a double cannot hold exactly are text, digit for digit.
    assert_eq!(cell(&range, 2, 1), text("9007199254740993"));
    assert_eq!(cell(&range, 2, 2), text("18446744073709551615"));
    assert_eq!(cell(&range, 2, 5), text("-12345678901234567890.0123456789"));
    assert_eq!(cell(&range, 2, 6), text(TRICKY));
    assert_eq!(cell(&range, 2, 7), text("+1"));
    // Before 1900: no Excel date.
    assert_eq!(cell(&range, 2, 10), text("1899-12-31"));

    // Row 3: NULLs are empty cells.
    for c in 0..16 {
        assert_eq!(cell(&range, 3, c), Data::Empty, "column {c}");
    }

    // Row 4.
    assert_eq!(cell(&range, 4, 1), text("-9223372036854775808"));
    assert_eq!(cell(&range, 4, 6), text(UNICODE));
    assert_eq!(cell(&range, 4, 7), text("-2"));

    // No cell holds a formula.
    let formulas = book.worksheet_formula("Sheet1").unwrap();
    assert_eq!(
        formulas
            .used_cells()
            .filter(|(_, _, f)| !f.is_empty())
            .count(),
        0
    );
}

#[test]
fn formula_guard_marks_cells_with_a_quote_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let guarded = dir.path().join("guarded.xlsx");
    write_all(
        &guarded,
        &xlsx(XlsxWriteOptions::default()),
        &WriteContext::default(),
        &[sample()],
    )
    .unwrap();
    let plain = dir.path().join("plain.xlsx");
    let options = XlsxWriteOptions {
        formula_guard: false,
        ..XlsxWriteOptions::default()
    };
    write_all(
        &plain,
        &xlsx(options),
        &WriteContext::default(),
        &[sample()],
    )
    .unwrap();
    // Same values either way (the guard is a cell style, not a changed text).
    for path in [&guarded, &plain] {
        let range = open(path).worksheet_range("Sheet1").unwrap();
        assert_eq!(cell(&range, 1, 7), text("=SUM(A1)"));
        assert_eq!(cell(&range, 4, 7), text("-2"));
    }
}

#[test]
fn rows_overflow_into_numbered_sheets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("split.xlsx");
    let options = XlsxWriteOptions {
        sheet_name: "Data".to_owned(),
        max_rows_per_sheet: 4,
        ..XlsxWriteOptions::default()
    };
    let report = write_all(
        &path,
        &xlsx(options),
        &WriteContext::default(),
        &[numbers(5), numbers(5)],
    )
    .unwrap();
    // 10 rows, 3 per sheet under a header.
    assert_eq!(report.rows, 10);
    assert_eq!(report.sheets, ["Data", "Data (2)", "Data (3)", "Data (4)"]);
    let mut book = open(&path);
    assert_eq!(book.sheet_names(), report.sheets);
    let mut seen = Vec::new();
    for name in &report.sheets {
        let range = book.worksheet_range(name).unwrap();
        assert_eq!(cell(&range, 0, 0), text("n"), "{name}");
        for row in 1..range.height() {
            let row = u32::try_from(row).unwrap();
            seen.push(number(&range, row, 0));
        }
    }
    let expected: Vec<f64> = [0, 1, 2, 3, 4, 0, 1, 2, 3, 4]
        .into_iter()
        .map(f64::from)
        .collect();
    assert_eq!(seen, expected);

    // Without a header every row is data.
    let bare = dir.path().join("bare.xlsx");
    let options = XlsxWriteOptions {
        header: false,
        max_rows_per_sheet: 5,
        ..XlsxWriteOptions::default()
    };
    let report = write_all(
        &bare,
        &xlsx(options),
        &WriteContext::default(),
        &[numbers(10)],
    )
    .unwrap();
    assert_eq!(report.sheets, ["Sheet1", "Sheet1 (2)"]);
}

#[test]
fn long_text_is_cut_to_the_cell_limit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long.xlsx");
    let schema = Arc::new(Schema::new(vec![Field::new("s", DataType::Utf8, true)]));
    let long = "é".repeat(40_000);
    let batch = RecordBatch::try_new(
        schema,
        vec![Arc::new(StringArray::from(vec![long.as_str(), "short"]))],
    )
    .unwrap();
    let report = write_all(
        &path,
        &xlsx(XlsxWriteOptions::default()),
        &WriteContext::default(),
        &[batch],
    )
    .unwrap();
    assert!(
        report.warnings.iter().any(|w| w.starts_with("1 text")),
        "{:?}",
        report.warnings
    );
    let range = open(&path).worksheet_range("Sheet1").unwrap();
    let Data::String(cut) = cell(&range, 1, 0) else {
        panic!("not text");
    };
    assert_eq!(cut.chars().count(), 32_767);
    assert_eq!(cell(&range, 2, 0), text("short"));
}

#[test]
fn single_floats_and_sub_millisecond_times() {
    use arrow_array::{Float32Array, Time64MicrosecondArray};

    let schema = Arc::new(Schema::new(vec![
        Field::new("f", DataType::Float32, true),
        Field::new(
            "clock",
            DataType::Time64(arrow_schema::TimeUnit::Microsecond),
            true,
        ),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Float32Array::from(vec![0.1_f32, 3.4e38])),
            // 03:04:05.123456 and 03:04:05.123 (no digit below a millisecond).
            Arc::new(Time64MicrosecondArray::from(vec![
                11_045_123_456,
                11_045_123_000,
            ])),
        ],
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("floats.xlsx");
    let report = write_all(
        &path,
        &xlsx(XlsxWriteOptions::default()),
        &WriteContext::default(),
        &[batch],
    )
    .unwrap();
    let range = open(&path).worksheet_range("Sheet1").unwrap();
    // The f32 0.1 is the double 0.1, not 0.10000000149011612.
    assert_eq!(cell(&range, 1, 0), Data::Float(0.1));
    assert_eq!(cell(&range, 2, 0), Data::Float(3.4e38));
    let warnings = report.warnings.join("\n");
    assert!(
        warnings.contains("1 time or timestamp value had digits below a millisecond"),
        "{warnings}"
    );
}
