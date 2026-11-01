//! XLSX reader over workbooks generated at runtime (`[m5.generated] xlsx`): typed cells, dates,
//! cached formula values, error cells, formula-looking text, empty rows, sheets, header rows
//! (docs/m5-contracts.md §3.3, §12 read).

mod read_support;

use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use oxbase_io::{ReadOptions, XlsxReadOptions};
use oxbase_types::{DataType, ErrorCategory};
use read_support::{preview, preview_and_read};
use rust_xlsxwriter::{Format, Formula, Workbook};

const HEADER: [&str; 8] = ["id", "price", "flag", "day", "at", "formula", "err", "text"];

fn col(c: usize) -> u16 {
    u16::try_from(c).unwrap()
}

/// Sheets "Data" and "Second"; Data row 3 is empty.
fn workbook(dir: &Path, name: &str, constant_memory: bool) -> PathBuf {
    let mut wb = Workbook::new();
    let date = Format::new().set_num_format("yyyy-mm-dd");
    let stamp = Format::new().set_num_format("yyyy-mm-dd hh:mm:ss");
    {
        let ws = if constant_memory {
            wb.add_worksheet_with_constant_memory().unwrap()
        } else {
            wb.add_worksheet()
        };
        ws.set_name("Data").unwrap();
        for (c, h) in HEADER.iter().enumerate() {
            ws.write_string(0, col(c), *h).unwrap();
        }
        let day = NaiveDate::from_ymd_opt(2001, 2, 3).unwrap();
        ws.write_number(1, 0, 1).unwrap();
        ws.write_number(1, 1, 1.5).unwrap();
        ws.write_boolean(1, 2, true).unwrap();
        ws.write_datetime_with_format(1, 3, day, &date).unwrap();
        ws.write_datetime_with_format(1, 4, day.and_hms_opt(4, 5, 6).unwrap(), &stamp)
            .unwrap();
        ws.write_formula(1, 5, Formula::new("=1+2").set_result("3"))
            .unwrap();
        ws.write_formula(1, 6, Formula::new("=1/0").set_result("#DIV/0!"))
            .unwrap();
        ws.write_string(1, 7, "=HYPERLINK(\"http://x\")").unwrap();
        // Row 3 (index 2) stays empty.
        let next = day.succ_opt().unwrap();
        ws.write_number(3, 0, 2).unwrap();
        ws.write_number(3, 1, 2.25).unwrap();
        ws.write_boolean(3, 2, false).unwrap();
        ws.write_datetime_with_format(3, 3, next, &date).unwrap();
        ws.write_datetime_with_format(3, 4, next.and_hms_opt(0, 0, 1).unwrap(), &stamp)
            .unwrap();
        ws.write_formula(3, 5, Formula::new("=2+2").set_result("4"))
            .unwrap();
        ws.write_string(3, 7, "plain").unwrap();
    }
    {
        let ws = wb.add_worksheet();
        ws.set_name("Second").unwrap();
        ws.write_string(0, 0, "only").unwrap();
        ws.write_number(1, 0, 7).unwrap();
    }
    let path = dir.join(name);
    wb.save(&path).unwrap();
    path
}

fn xlsx(o: XlsxReadOptions) -> ReadOptions {
    ReadOptions::Xlsx(o)
}

fn defaults() -> ReadOptions {
    xlsx(XlsxReadOptions::default())
}

fn text(values: &[Option<&str>]) -> Vec<Option<String>> {
    values.iter().map(|v| v.map(str::to_owned)).collect()
}

#[test]
fn typed_cells_formulas_errors_and_empty_rows() {
    let dir = tempfile::tempdir().unwrap();
    for (name, constant_memory) in [("shared.xlsx", false), ("inline.xlsx", true)] {
        let path = workbook(dir.path(), name, constant_memory);
        let (p, all) = preview_and_read(&path, &defaults()).unwrap();
        assert_eq!(p.detected.sheets, ["Data", "Second"], "{name}");
        assert_eq!(p.detected.sheet.as_deref(), Some("Data"));
        let names: Vec<&str> = p.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, HEADER, "{name}");
        let kinds: Vec<&str> = p
            .columns
            .iter()
            .map(|c| read_support::kind(&c.data_type))
            .collect();
        assert_eq!(
            kinds,
            [
                "int",
                "float",
                "bool",
                "date",
                "timestamp",
                "int",
                "text",
                "text"
            ],
            "{name}"
        );
        assert!(p.complete);
        assert_eq!(p.sampled_rows, 3);
        assert!(p.columns[0].nullable, "the empty row is NULL");
        assert_eq!(p.rows[0][6].as_deref(), Some("#DIV/0!"));

        assert_eq!(all.rows, 3, "{name}");
        all.assert_progress();
        assert_eq!(all.column(0), text(&[Some("1"), None, Some("2")]));
        assert_eq!(all.column(1), text(&[Some("1.5"), None, Some("2.25")]));
        assert_eq!(all.column(2), text(&[Some("true"), None, Some("false")]));
        assert_eq!(
            all.column(3),
            text(&[Some("2001-02-03"), None, Some("2001-02-04")])
        );
        assert_eq!(
            all.column(4),
            text(&[
                Some("2001-02-03T04:05:06"),
                None,
                Some("2001-02-04T00:00:01")
            ])
        );
        // Formulas are their cached values, never evaluated.
        assert_eq!(all.column(5), text(&[Some("3"), None, Some("4")]));
        assert_eq!(
            all.column(7),
            text(&[Some("=HYPERLINK(\"http://x\")"), None, Some("plain")])
        );
        assert_eq!(all.issues.len(), 1, "{:?}", all.issues);
        let issue = &all.issues[0];
        assert_eq!(issue.row, 1);
        assert_eq!(issue.column.as_deref(), Some("err"));
        assert!(issue.message.contains("#DIV/0!"), "{issue:?}");
        assert!(issue.message.ends_with("(cell G2)"), "{issue:?}");
    }
}

#[test]
fn sheets_and_header_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = workbook(dir.path(), "book.xlsx", false);

    let second = xlsx(XlsxReadOptions {
        sheet: Some("Second".to_owned()),
        ..XlsxReadOptions::default()
    });
    let (p, all) = preview_and_read(&path, &second).unwrap();
    assert_eq!(p.detected.sheet.as_deref(), Some("Second"));
    assert_eq!(p.columns[0].name, "only");
    assert_eq!(p.columns[0].data_type, DataType::INT64);
    assert_eq!(all.column(0), text(&[Some("7")]));

    let missing = xlsx(XlsxReadOptions {
        sheet: Some("Nope".to_owned()),
        ..XlsxReadOptions::default()
    });
    let err = preview(&path, &missing).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert!(err.to_string().contains("'Nope'"), "{err}");
    assert!(err.to_string().contains("Data, Second"), "{err}");

    let no_header = xlsx(XlsxReadOptions {
        header_row: 0,
        ..XlsxReadOptions::default()
    });
    let (p, all) = preview_and_read(&path, &no_header).unwrap();
    assert_eq!(p.columns[0].name, "column_1");
    assert_eq!(p.columns[7].name, "column_8");
    assert_eq!(p.columns[0].data_type, DataType::Text);
    assert_eq!(all.rows, 4);
    assert_eq!(all.column(0)[0].as_deref(), Some("id"));
}

#[test]
fn files_that_are_not_workbooks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("table.xlsx");
    std::fs::copy(read_support::sample("basic.csv"), &path).unwrap();
    let err = preview(&path, &defaults()).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert!(err.to_string().contains("not an XLSX workbook"), "{err}");

    let empty = dir.path().join("empty.xlsx");
    std::fs::write(&empty, b"").unwrap();
    assert_eq!(
        preview(&empty, &defaults()).unwrap_err().category(),
        ErrorCategory::InvalidInput
    );
}
