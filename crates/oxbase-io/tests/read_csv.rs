//! CSV / TSV reader: values, options, overrides, preview limits, bounded batches, progress,
//! cancel and a debug-build throughput floor (docs/m5-contracts.md §3.3, §12 read).

mod read_support;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use arrow_array::cast::AsArray;
use arrow_array::types::{Int64Type, TimestampMicrosecondType};
use chrono::NaiveDate;
use futures::StreamExt;
use oxbase_driver::CancellationToken;
use oxbase_io::{
    ColumnSpec, CsvReadOptions, LineEnding, PreviewLimits, ReadOptions, TextEncoding, spawn_reader,
};
use oxbase_types::{DataType, ErrorCategory};
use read_support::{ReadAll, preview, preview_and_read, read_all, sample};

fn csv(o: CsvReadOptions) -> ReadOptions {
    ReadOptions::Csv(o)
}

fn defaults() -> ReadOptions {
    csv(CsvReadOptions::default())
}

fn write(dir: &Path, name: &str, data: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, data).unwrap();
    path
}

fn some(values: &[&str]) -> Vec<Option<String>> {
    values.iter().map(|v| Some((*v).to_owned())).collect()
}

fn micros(all: &ReadAll, column: usize) -> Vec<Option<i64>> {
    all.batches
        .iter()
        .flat_map(|b| {
            b.batch
                .column(column)
                .as_primitive::<TimestampMicrosecondType>()
                .iter()
                .collect::<Vec<_>>()
        })
        .collect()
}

/// `rows` generated rows: `id,code,amount,flag`.
fn generated(dir: &Path, name: &str, rows: u64) -> PathBuf {
    let mut text = String::from("id,code,amount,flag\n");
    for i in 1..=rows {
        writeln!(
            text,
            "{i},C{i:08},{}.{:02},{}",
            i / 3,
            i % 100,
            i.is_multiple_of(2)
        )
        .unwrap();
    }
    write(dir, name, text.as_bytes())
}

#[test]
fn basic_values_and_line_ends() {
    let (p, all) = preview_and_read(&sample("basic.csv"), &defaults()).unwrap();
    assert_eq!(p.detected.delimiter, Some(','));
    assert_eq!(p.rows[2][1].as_deref(), Some("multi\nline"));
    assert_eq!(all.column(0), some(&["1", "2", "3", "4", "5"]));
    let names = all.column(1);
    assert_eq!(names[1].as_deref(), Some("comma, inside"));
    assert_eq!(names[2].as_deref(), Some("multi\nline"));
    assert_eq!(names[3].as_deref(), Some("naïve café – 日本語 🚀"));
    assert_eq!(names[4].as_deref(), Some("=1+1"));
    assert_eq!(
        all.column(2),
        some(&["12.50", "-0.75", "1000000.00", "0.00", "3.14"])
    );
    assert_eq!(all.column(3)[3], None);
    let day = NaiveDate::from_ymd_opt(1999, 12, 31).unwrap();
    let at = micros(&all, 4);
    assert_eq!(
        at[1],
        Some(
            day.and_hms_micro_opt(23, 59, 59, 123_456)
                .unwrap()
                .and_utc()
                .timestamp_micros()
        )
    );
    let leap = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
    assert_eq!(
        at[2],
        Some(
            leap.and_hms_opt(12, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp_micros()
        )
    );
    assert_eq!(at[3], None);
    let notes = all.column(6);
    assert_eq!(notes[1].as_deref(), Some("say \"hi\""));
    assert_eq!(notes[2].as_deref(), Some("crlf\r\ninside"));
    assert_eq!(notes[4].as_deref(), Some("@SUM(A1)"));
    assert_eq!(all.column(5)[3], None);

    // The same rows with CRLF row ends.
    let (_, crlf) = preview_and_read(&sample("crlf.csv"), &defaults()).unwrap();
    for c in 0..7 {
        assert_eq!(crlf.column(c), all.column(c), "column {c}");
    }

    // TSV with a quoted tab.
    let (p, tsv) = preview_and_read(&sample("basic.tsv"), &csv(CsvReadOptions::tsv())).unwrap();
    assert_eq!(p.detected.delimiter, Some('\t'));
    assert_eq!(tsv.column(2)[0].as_deref(), Some("tab\tinside"));
    assert_eq!(tsv.column(2)[1], None);
}

#[test]
fn ragged_rows() {
    let (p, all) = preview_and_read(&sample("ragged.csv"), &defaults()).unwrap();
    assert!(
        p.warnings
            .iter()
            .any(|w| w.contains("do not have 3 fields"))
    );
    assert_eq!(all.column(2)[1], None);
    // Row 2 lacks c, so c is nullable even though every c in the file has a value.
    assert!(p.complete);
    assert!(p.columns[2].nullable);
    // 4 rows in batches of 2: the last batch carries the file size.
    all.assert_progress();
    assert_eq!(all.issues.len(), 1);
    assert_eq!(all.issues[0].row, 3);
    assert_eq!(all.issues[0].column, None);
    assert!(all.issues[0].message.contains("extra fields were dropped"));

    let strict = csv(CsvReadOptions {
        allow_ragged: false,
        ..CsvReadOptions::default()
    });
    let (_, all) = preview_and_read(&sample("ragged.csv"), &strict).unwrap();
    let rows: Vec<u64> = all.issues.iter().map(|i| i.row).collect();
    assert_eq!(rows, [2, 3]);
    assert!(
        all.issues[0]
            .message
            .contains("row has 2 fields, expected 3")
    );
}

#[test]
fn overrides_change_types_and_bad_values_are_issues() {
    let path = sample("basic.csv");
    let p = preview(&path, &defaults()).unwrap();
    let mut specs = ColumnSpec::from_preview(&p);
    specs[0].data_type = DataType::Text;
    specs[1].data_type = DataType::INT64;
    let all = read_all(&path, &defaults(), &specs, 100).unwrap();
    assert_eq!(
        all.batches[0].batch.schema().field(0).data_type(),
        &arrow_schema::DataType::Utf8
    );
    assert_eq!(all.column(0), some(&["1", "2", "3", "4", "5"]));
    assert_eq!(all.column(1), vec![None; 5]);
    assert_eq!(all.issues.len(), 5);
    for (i, issue) in all.issues.iter().enumerate() {
        assert_eq!(issue.row, u64::try_from(i).unwrap() + 1);
        assert_eq!(issue.batch_row, i);
        assert_eq!(issue.column.as_deref(), Some("name"));
        assert!(issue.message.contains("is not an integer"), "{issue:?}");
    }
    assert!(all.issues[0].message.starts_with("'plain'"));
    // Row 4 starts on line 7 (row 3 spans three lines): the message says so.
    assert!(
        all.issues[3].message.ends_with("(line 7)"),
        "{:?}",
        all.issues[3]
    );

    // A spec count that does not match the header is refused.
    let err = oxbase_io::open(&path, &defaults(), &specs[..3], 10)
        .err()
        .unwrap();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
}

#[test]
fn encodings_and_malformed_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "bad.csv", b"id,name\n1,ab\xff\n");
    let utf8 = csv(CsvReadOptions {
        encoding: TextEncoding::Utf8,
        ..CsvReadOptions::default()
    });
    let (p, all) = preview_and_read(&path, &utf8).unwrap();
    assert!(
        p.warnings.iter().any(|w| w.contains("not valid UTF-8")),
        "{:?}",
        p.warnings
    );
    assert_eq!(all.column(1)[0].as_deref(), Some("ab\u{FFFD}"));

    // Auto: not UTF-8, so Windows-1252.
    let (p, all) = preview_and_read(&path, &defaults()).unwrap();
    assert_eq!(p.detected.encoding, Some(TextEncoding::Windows1252));
    assert_eq!(all.column(1)[0].as_deref(), Some("abÿ"));

    let (p, all) = preview_and_read(&sample("utf16be_bom.csv"), &defaults()).unwrap();
    assert_eq!(p.detected.encoding, Some(TextEncoding::Utf16Be));
    assert!(p.detected.bom);
    assert_eq!(p.columns[1].name, "name");
    assert_eq!(all.column(1), some(&["é", "日本"]));
}

#[test]
fn reading_options() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();

    let path = write(
        d,
        "skip.csv",
        b"junk line 1\njunk, \"line\" 2\r\nid,v\n1,a\n",
    );
    let o = csv(CsvReadOptions {
        skip_rows: 2,
        ..CsvReadOptions::default()
    });
    let (p, all) = preview_and_read(&path, &o).unwrap();
    assert_eq!(p.columns[1].name, "v");
    assert_eq!(all.column(1), some(&["a"]));

    let path = write(d, "cr.csv", b"id,v\r1,a\r2,b\r");
    let o = csv(CsvReadOptions {
        line_ending: LineEnding::Cr,
        ..CsvReadOptions::default()
    });
    let (_, all) = preview_and_read(&path, &o).unwrap();
    assert_eq!(all.column(1), some(&["a", "b"]));

    let path = write(d, "esc.csv", b"id,v\n1,\"a\\\"b\"\n");
    let o = csv(CsvReadOptions {
        escape: Some('\\'),
        ..CsvReadOptions::default()
    });
    let (_, all) = preview_and_read(&path, &o).unwrap();
    assert_eq!(all.column(1), some(&["a\"b"]));

    let path = write(d, "comment.csv", b"#c\nid,v\n#x\n1,a\n");
    let o = csv(CsvReadOptions {
        comment: Some('#'),
        ..CsvReadOptions::default()
    });
    let (_, all) = preview_and_read(&path, &o).unwrap();
    assert_eq!(all.rows, 1);

    let path = write(d, "trim.csv", b"id, v \n 1 , a \n");
    let o = csv(CsvReadOptions {
        trim: true,
        ..CsvReadOptions::default()
    });
    let (p, all) = preview_and_read(&path, &o).unwrap();
    assert_eq!(p.columns[1].name, "v");
    assert_eq!(p.columns[0].data_type, DataType::INT64);
    assert_eq!(all.column(1), some(&["a"]));

    let path = write(d, "noquote.csv", b"id,v\n1,\"a\"\n");
    let o = csv(CsvReadOptions {
        quote: None,
        ..CsvReadOptions::default()
    });
    let (_, all) = preview_and_read(&path, &o).unwrap();
    assert_eq!(all.column(1), some(&["\"a\""]));
}

#[test]
fn line_ends_and_trimming() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();

    // A lone CR in an unquoted field is data unless the line ending is `cr`.
    let path = write(d, "lone_cr.csv", b"id,v,w\n1,ab\rcd,x\n2,e,f\n");
    let (p, all) = preview_and_read(&path, &defaults()).unwrap();
    assert_eq!(p.warnings.len(), 0, "{:?}", p.warnings);
    assert_eq!(all.rows, 2);
    assert_eq!(all.issues.len(), 0, "{:?}", all.issues);
    assert_eq!(all.column(1), some(&["ab\rcd", "e"]));
    assert_eq!(all.column(2), some(&["x", "f"]));

    // CRLF row ends: the CR is not part of the last field; blank lines are skipped; a quoted
    // CR (and CRLF) inside a field is kept.
    let path = write(
        d,
        "crlf_blank.csv",
        b"id,v,w\r\n1,a,\"x\r\"\r\n\r\n2,\"b\r\nc\",y\r\n",
    );
    let (p, all) = preview_and_read(&path, &defaults()).unwrap();
    assert_eq!(p.columns[2].name, "w");
    assert_eq!(all.rows, 2);
    assert_eq!(all.column(1), some(&["a", "b\r\nc"]));
    assert_eq!(all.column(2), some(&["x\r", "y"]));

    // `cr`: a lone CR ends a row. Line numbers are unknown, so messages name rows only.
    let cr = csv(CsvReadOptions {
        line_ending: LineEnding::Cr,
        ..CsvReadOptions::default()
    });
    let path = write(d, "cr_issue.csv", b"id,v\r1,a\rx,b\n3,c\r\n");
    let specs = [
        ColumnSpec::new("id", DataType::INT64),
        ColumnSpec::new("v", DataType::Text),
    ];
    let all = read_all(&path, &cr, &specs, 10).unwrap();
    assert_eq!(all.rows, 3);
    assert_eq!(all.column(1), some(&["a", "b", "c"]));
    assert_eq!(all.issues.len(), 1);
    assert_eq!(all.issues[0].row, 2);
    assert!(
        !all.issues[0].message.contains("line"),
        "{:?}",
        all.issues[0]
    );

    // `trim` trims quoted fields too (documented: the parser does not report quoting).
    let path = write(d, "trim_quoted.csv", b"a,b,c\n\"  x  \", y ,\tz\t\n");
    let o = csv(CsvReadOptions {
        trim: true,
        ..CsvReadOptions::default()
    });
    let specs = ["a", "b", "c"].map(|n| ColumnSpec::new(n, DataType::Text));
    let all = read_all(&path, &o, &specs, 10).unwrap();
    assert_eq!(all.column(0), some(&["x"]));
    assert_eq!(all.column(1), some(&["y"]));
    assert_eq!(all.column(2), some(&["z"]));
}

#[test]
fn sub_microsecond_text_is_a_row_issue() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        dir.path(),
        "nanos.csv",
        b"t,ts\n10:00:00.123456,2001-02-03 04:05:06.000001\n10:00:00.1234567,2001-02-03 04:05:06.0000001\n",
    );
    let specs = [
        ColumnSpec::new("t", DataType::Time { precision: None }),
        ColumnSpec::new("ts", DataType::Timestamp { precision: None }),
    ];
    let all = read_all(&path, &defaults(), &specs, 10).unwrap();
    assert_eq!(all.issues.len(), 2, "{:?}", all.issues);
    assert!(all.issues.iter().all(|i| i.row == 2));
    assert!(
        all.issues[0].message.contains("sub-microsecond"),
        "{:?}",
        all.issues[0]
    );
    assert_eq!(all.column(0)[0].as_deref(), Some("10:00:00.123456"));
    assert_eq!(all.column(0)[1], None);
    assert_eq!(all.column(1)[1], None);
}

#[test]
fn an_unclosed_quote_is_one_field_with_a_warning() {
    let (p, all) = preview_and_read(&sample("unterminated_quote.csv"), &defaults()).unwrap();
    assert!(
        p.warnings.iter().any(|w| w.contains("never closed")),
        "{:?}",
        p.warnings
    );
    assert_eq!(all.column(1)[0].as_deref(), Some("open\n2,never closed\n"));
    let basic = preview(&sample("basic.csv"), &defaults()).unwrap();
    assert_eq!(basic.warnings.len(), 0, "{:?}", basic.warnings);
}

#[test]
fn preview_limits() {
    let dir = tempfile::tempdir().unwrap();
    let path = generated(dir.path(), "limits.csv", 5_000);
    let cancel = CancellationToken::new();
    let limited = PreviewLimits {
        sample_rows: 100,
        display_rows: 10,
        ..PreviewLimits::default()
    };
    let p = oxbase_io::preview(&path, &defaults(), limited, &cancel).unwrap();
    assert_eq!(p.sampled_rows, 100);
    assert_eq!(p.rows.len(), 10);
    assert!(!p.complete);
    assert!(p.columns.iter().all(|c| c.nullable));
    assert_eq!(p.columns[0].data_type, DataType::INT64);

    let whole = PreviewLimits {
        sample_rows: 10_000,
        ..PreviewLimits::default()
    };
    let p = oxbase_io::preview(&path, &defaults(), whole, &cancel).unwrap();
    assert_eq!(p.sampled_rows, 5_000);
    assert!(p.complete);
    assert!(!p.columns[0].nullable);
    assert_eq!(p.rows.len(), 100);

    // The byte limit stops sampling early (after at least one row).
    let big = generated(dir.path(), "bytes.csv", 30_000);
    let bytes = PreviewLimits {
        sample_bytes: 1,
        ..PreviewLimits::default()
    };
    let p = oxbase_io::preview(&big, &defaults(), bytes, &cancel).unwrap();
    assert!(
        p.sampled_rows >= 1 && p.sampled_rows < 1_000,
        "{}",
        p.sampled_rows
    );
    assert!(!p.complete);

    cancel.cancel();
    let err =
        oxbase_io::preview(&path, &defaults(), PreviewLimits::default(), &cancel).unwrap_err();
    assert!(err.is_cancelled());
}

#[test]
fn fifty_thousand_rows_in_bounded_batches() {
    let dir = tempfile::tempdir().unwrap();
    let path = generated(dir.path(), "fifty.csv", 50_000);
    let p = preview(&path, &defaults()).unwrap();
    let kinds: Vec<&str> = p
        .columns
        .iter()
        .map(|c| read_support::kind(&c.data_type))
        .collect();
    assert_eq!(kinds, ["int", "text", "decimal", "bool"]);
    let specs = ColumnSpec::from_preview(&p);
    // read_all checks every batch holds at most `batch_rows` rows and starts where the last
    // one ended.
    let all = read_all(&path, &defaults(), &specs, 1_000).unwrap();
    assert_eq!(all.rows, 50_000);
    assert_eq!(all.batches.len(), 50);
    assert_eq!(all.issues.len(), 0);
    all.assert_progress();
    assert_eq!(all.total_bytes, std::fs::metadata(&path).unwrap().len());
    // Positions advance during the read, not only at the end.
    assert!(all.positions[0] < all.total_bytes);
    let sum: i64 = all
        .batches
        .iter()
        .map(|b| {
            b.batch
                .column(0)
                .as_primitive::<Int64Type>()
                .iter()
                .flatten()
                .sum::<i64>()
        })
        .sum();
    assert_eq!(sum, 50_000 * 50_001 / 2);
}

#[test]
#[allow(clippy::cast_precision_loss)]
fn throughput_floor_in_a_debug_build() {
    let dir = tempfile::tempdir().unwrap();
    let rows = 100_000;
    let path = generated(dir.path(), "speed.csv", rows);
    let p = preview(&path, &defaults()).unwrap();
    let specs = ColumnSpec::from_preview(&p);
    let start = Instant::now();
    let all = read_all(&path, &defaults(), &specs, 8_192).unwrap();
    let secs = start.elapsed().as_secs_f64().max(1e-3);
    assert_eq!(all.rows, 100_000);
    let per_second = rows as f64 / secs;
    let mb = all.total_bytes as f64 / 1e6 / secs;
    println!("CSV read: {per_second:.0} rows/s, {mb:.1} MB/s (unoptimized build)");
    // Generous: an unoptimized build on a slow runner still parses far faster than this.
    assert!(per_second > 5_000.0, "{per_second:.0} rows/s");
}

#[tokio::test]
async fn the_async_adapter_streams_and_cancels() {
    let path = sample("basic.csv");
    let p = preview(&path, &defaults()).unwrap();
    let specs = ColumnSpec::from_preview(&p);
    let reader = oxbase_io::open(&path, &defaults(), &specs, 1).unwrap();
    let items: Vec<_> = spawn_reader(reader, CancellationToken::new(), 1)
        .collect()
        .await;
    assert_eq!(items.len(), 5);
    assert!(items.iter().all(Result::is_ok));

    let cancel = CancellationToken::new();
    cancel.cancel();
    let reader = oxbase_io::open(&path, &defaults(), &specs, 1).unwrap();
    let items: Vec<_> = spawn_reader(reader, cancel, 1).collect().await;
    assert_eq!(items.len(), 1);
    assert!(items[0].as_ref().unwrap_err().is_cancelled());
}

#[test]
fn errors_name_only_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("secret-folder").join("missing.csv");
    let err = preview(&missing, &defaults()).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Io);
    let message = err.to_string();
    assert!(message.contains("missing.csv"), "{message}");
    assert!(!message.contains("secret-folder"), "{message}");
}

#[test]
fn null_tokens_and_formats() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        dir.path(),
        "formats.csv",
        b"day;amount;flag\n03.02.2001;1.234,5;ja\n04.02.2001;-;nein\n",
    );
    let mut o = CsvReadOptions {
        delimiter: ';',
        null_tokens: vec!["-".to_owned()],
        ..CsvReadOptions::default()
    };
    o.formats.date_format = Some("%d.%m.%Y".to_owned());
    o.formats.decimal_separator = ',';
    o.formats.thousands_separator = Some('.');
    o.formats.true_values = vec!["ja".to_owned()];
    o.formats.false_values = vec!["nein".to_owned()];
    let o = csv(o);
    let p = preview(&path, &o).unwrap();
    assert_eq!(p.columns[0].data_type, DataType::Date);
    assert_eq!(
        p.columns[1].data_type,
        DataType::Decimal {
            precision: Some(5),
            scale: Some(1)
        }
    );
    // `ja` / `nein` are only booleans in a column typed bool.
    assert_eq!(p.columns[2].data_type, DataType::Text);
    let mut specs = ColumnSpec::from_preview(&p);
    specs[2].data_type = DataType::Bool;
    let all = read_all(&path, &o, &specs, 10).unwrap();
    assert_eq!(all.column(0), some(&["2001-02-03", "2001-02-04"]));
    assert_eq!(all.column(1), vec![Some("1234.5".to_owned()), None]);
    assert_eq!(all.column(2), some(&["true", "false"]));
    let flags = all.batches[0].batch.column(2).as_boolean();
    assert!(flags.value(0));
    assert!(!flags.value(1));
}
