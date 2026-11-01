//! Property tests: random tables written as CSV by a minimal writer in this file (independent
//! of the crate's CSV writer) read back exactly, whatever the delimiter, row ends and field
//! contents (quotes, delimiters, CR / LF, Unicode, empty strings); and type inference rules
//! hold for random integers and decimals.

mod read_support;

use std::path::Path;

use oxbase_io::infer::TypeInference;
use oxbase_io::{ColumnSpec, CsvReadOptions, ReadOptions, ValueFormats};
use oxbase_types::DataType;
use read_support::{Rng, read_all};

const ALPHABET: [&str; 16] = [
    "a", "b", "Z", " ", ",", ";", "\t", "|", "\"", "\n", "\r", "é", "日", "🚀", "\\", "=",
];

fn random_field(rng: &mut Rng) -> String {
    let len = rng.below(9);
    (0..len)
        .map(|_| ALPHABET[rng.below(ALPHABET.len())])
        .collect()
}

/// RFC 4180 style: a field is quoted when it holds the delimiter, a quote, CR or LF, is
/// empty, or (at random) anyway.
fn encode(field: &str, delimiter: char, rng: &mut Rng) -> String {
    let needs = field.is_empty()
        || field.contains([delimiter, '"', '\r', '\n'])
        || field.starts_with(' ')
        || rng.below(4) == 0;
    if needs {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_owned()
    }
}

#[test]
fn random_tables_round_trip_through_the_parser() {
    let mut rng = Rng::new(0x5EED_CAFE);
    let dir = tempfile::tempdir().unwrap();
    for case in 0..150 {
        let delimiter = [',', ';', '\t', '|'][rng.below(4)];
        let row_end = ["\n", "\r\n"][rng.below(2)];
        let columns = 1 + rng.below(5);
        let rows = rng.below(30);
        let table: Vec<Vec<String>> = (0..rows)
            .map(|_| (0..columns).map(|_| random_field(&mut rng)).collect())
            .collect();
        let mut text = String::new();
        let header: Vec<String> = (1..=columns).map(|c| format!("c{c}")).collect();
        text.push_str(&header.join(&delimiter.to_string()));
        text.push_str(row_end);
        for (i, row) in table.iter().enumerate() {
            let fields: Vec<String> = row.iter().map(|f| encode(f, delimiter, &mut rng)).collect();
            text.push_str(&fields.join(&delimiter.to_string()));
            // The last row ends without a row end now and then.
            if i + 1 < table.len() || rng.below(10) > 0 {
                text.push_str(row_end);
            }
        }
        let path = dir.path().join(format!("case{case}.csv"));
        std::fs::write(&path, &text).unwrap();

        let options = ReadOptions::Csv(CsvReadOptions {
            delimiter,
            null_tokens: Vec::new(),
            ..CsvReadOptions::default()
        });
        let specs: Vec<ColumnSpec> = header
            .iter()
            .map(|name| ColumnSpec::new(name.clone(), DataType::Text))
            .collect();
        let all = read_all(&path, &options, &specs, 7)
            .unwrap_or_else(|e| panic!("case {case}: {e}\n{text:?}"));
        assert_eq!(all.rows, rows, "case {case}: {text:?}");
        assert_eq!(all.issues.len(), 0, "case {case}: {:?}", all.issues);
        for c in 0..columns {
            let expected: Vec<Option<String>> =
                table.iter().map(|row| Some(row[c].clone())).collect();
            assert_eq!(all.column(c), expected, "case {case}, column {c}: {text:?}");
        }
        all.assert_progress();
        check_preview(&path, &options, rows, columns, case);
    }
}

fn check_preview(path: &Path, options: &ReadOptions, rows: usize, columns: usize, case: usize) {
    let p = read_support::preview(path, options).unwrap();
    assert_eq!(p.sampled_rows, rows, "case {case}");
    assert_eq!(p.columns.len(), columns, "case {case}");
    assert!(p.complete, "case {case}");
}

#[test]
fn inference_of_random_numbers() {
    let mut rng = Rng::new(42);
    for _ in 0..200 {
        let mut inference = TypeInference::new(2, ValueFormats::default());
        let n = 1 + rng.below(20);
        let mut scale = 0;
        let mut int_digits = 1;
        for _ in 0..n {
            let value = i64::try_from(rng.next_u64() >> 1).unwrap() - i64::MAX / 2;
            inference.observe(0, Some(&value.to_string()));
            let digits = 1 + rng.below(6);
            let frac = 1 + rng.below(4);
            let whole = rng.below(10usize.pow(u32::try_from(digits).unwrap()));
            let fraction = rng.below(10usize.pow(u32::try_from(frac).unwrap()));
            let text = format!("{whole}.{fraction:0frac$}");
            scale = scale.max(frac);
            int_digits = int_digits.max(whole.to_string().len());
            inference.observe(1, Some(&text));
        }
        let columns = inference.finish(true);
        assert_eq!(columns[0].data_type, DataType::INT64);
        let precision = u32::try_from(int_digits + scale).unwrap();
        let found = &columns[1].data_type;
        let DataType::Decimal {
            precision: Some(p),
            scale: Some(s),
        } = found
        else {
            panic!("not a decimal: {found:?}");
        };
        assert_eq!(*s, u32::try_from(scale).unwrap());
        // `0.x` has no integer digits, so the precision may be one less.
        assert!(*p == precision || *p + 1 == precision, "{p} vs {precision}");
    }
}
