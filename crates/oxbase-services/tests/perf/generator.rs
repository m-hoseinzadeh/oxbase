//! The deterministic CSV of the M5 exit check (docs/m5-contracts.md §9.1).
//!
//! Row `i` (1-based) of `rows`, header `id,code,qty,amount,price,active,born,at,note`, UTF-8,
//! LF line ends, fields quoted the way the `csv` crate's `QuoteStyle::Necessary` quotes them
//! (only a field holding `,`, `"`, CR or LF is quoted; quotes inside are doubled). The writer is
//! written out here rather than taken from the `csv` crate so the test needs no new
//! dependency; for these values its bytes are the same.
//!
//! | Column | Value |
//! |---|---|
//! | `id` | `i` |
//! | `code` | `C` + base-36 of `(i * 7919) % 36^8`, zero-padded to 8 |
//! | `qty` | empty (NULL) when `i % 17 == 0`, else `(i * 31) % 10007 - 5000` |
//! | `amount` | `((i * 7907) % 10 000 000) / 100` with 2 decimals, negative when `i % 11 == 0` |
//! | `price` | `(i % 1000) / 7.0` in Rust's shortest form |
//! | `active` | `true` when `i % 3 == 0`, else `false` |
//! | `born` | 1970-01-01 + `(i * 37) % 20 000` days |
//! | `at` | 2000-01-01 00:00:00 + `i * 7` s + `i % 1 000 000` µs, `YYYY-MM-DD HH:MM:SS.ffffff` |
//! | `note` | empty (NULL) when `i % 13 == 0`, else [`NOTE_FRAGMENTS`]`[i % 8]` + ` #` + `i` |
//!
//! 10 000 000 rows make 1 238 093 427 bytes (between the 1.0 and 1.3 GB the contract asks
//! for). An amount of zero cents is written without a sign.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use chrono::{Datelike, NaiveDate, NaiveDateTime, TimeDelta, Timelike};

/// The header line's column names, in order.
pub const COLUMNS: [&str; 9] = [
    "id", "code", "qty", "amount", "price", "active", "born", "at", "note",
];

/// `note` is one of these, then ` #<i>`. Plain ASCII; a comma; quotes; LF; CRLF; non-ASCII
/// with an astral character (two UTF-16 units); a leading `=` (formula-looking); 60 × `x`.
pub const NOTE_FRAGMENTS: [&str; 8] = [
    "plain ascii text",
    "with, a comma",
    "with \"quotes\" inside",
    "line one\nline two",
    "line one\r\nline two",
    "naïve café – 日本語 🚀",
    "=SUM(A1:A9)",
    "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
];

/// The six numbers the checksum query of each engine returns (`[m5.perf.checksum]`), plus the
/// note length in UTF-16 units that SQL Server's query reports instead of code points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Checksum {
    pub count: i128,
    pub sum_id: i128,
    pub sum_qty: i128,
    pub notes: i128,
    /// Σ note length in Unicode code points.
    pub note_chars: i128,
    /// Σ note length in UTF-16 code units (SQL Server `DATALENGTH(nvarchar) / 2`).
    pub note_utf16: i128,
    pub amount_cents: i128,
}

impl Checksum {
    /// The six numbers in the order of the checksum queries, for engine `key` (`mssql` counts
    /// note lengths in UTF-16 units).
    pub fn expected(&self, key: &str) -> [i128; 6] {
        let lengths = if key == "mssql" {
            self.note_utf16
        } else {
            self.note_chars
        };
        [
            self.count,
            self.sum_id,
            self.sum_qty,
            self.notes,
            lengths,
            self.amount_cents,
        ]
    }

    fn add_note(&mut self, note: &str) {
        self.notes += 1;
        self.note_chars += i128::try_from(note.chars().count()).expect("small");
        self.note_utf16 += i128::try_from(note.encode_utf16().count()).expect("small");
    }
}

/// One generated row's values (`None` = empty field = NULL).
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: u64,
    pub code: String,
    pub qty: Option<i64>,
    pub amount_cents: i64,
    pub price: f64,
    pub active: bool,
    pub born: NaiveDate,
    pub at: NaiveDateTime,
    pub note: Option<String>,
}

const CODE_MODULUS: u64 = 36u64.pow(8);

/// The values of row `i` (1-based).
pub fn row(i: u64) -> Row {
    let qty = (i % 17 != 0).then(|| i64::try_from((i * 31) % 10_007).expect("below 10007") - 5_000);
    let cents = i64::try_from((i * 7_907) % 10_000_000).expect("below 10^7");
    let amount_cents = if i % 11 == 0 { -cents } else { cents };
    let price = f64::from(u32::try_from(i % 1_000).expect("below 1000")) / 7.0;
    let days = i64::try_from((i * 37) % 20_000).expect("below 20000");
    let born = epoch_date() + TimeDelta::days(days);
    let seconds = i64::try_from(i * 7).expect("fits");
    let micros = i64::try_from(i % 1_000_000).expect("below 10^6");
    let at = at_base() + TimeDelta::seconds(seconds) + TimeDelta::microseconds(micros);
    let note = (i % 13 != 0).then(|| format!("{} #{i}", NOTE_FRAGMENTS[usize_of(i % 8)]));
    Row {
        id: i,
        code: code(i),
        qty,
        amount_cents,
        price,
        active: i % 3 == 0,
        born,
        at,
        note,
    }
}

fn usize_of(v: u64) -> usize {
    usize::try_from(v).expect("small")
}

fn epoch_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid date")
}

fn at_base() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2000, 1, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .expect("valid timestamp")
}

fn code(i: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut v = (i * 7_919) % CODE_MODULUS;
    let mut out = [b'0'; 8];
    for slot in out.iter_mut().rev() {
        *slot = DIGITS[usize_of(v % 36)];
        v /= 36;
    }
    let mut s = String::with_capacity(9);
    s.push('C');
    s.push_str(std::str::from_utf8(&out).expect("ASCII"));
    s
}

/// `cents` as `[-]units.cc` (no sign for zero).
pub fn amount_text(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// Appends `field` to `line` the way `QuoteStyle::Necessary` writes it.
fn push_field(line: &mut String, field: &str) {
    if field.contains([',', '"', '\r', '\n']) {
        line.push('"');
        for c in field.chars() {
            if c == '"' {
                line.push('"');
            }
            line.push(c);
        }
        line.push('"');
    } else {
        line.push_str(field);
    }
}

/// Appends row `i` as one CSV record (with its LF) to `line` and adds it to `sum`.
fn push_row(line: &mut String, i: u64, sum: &mut Checksum) {
    let r = row(i);
    sum.count += 1;
    sum.sum_id += i128::from(r.id);
    sum.amount_cents += i128::from(r.amount_cents);
    let _ = write!(line, "{},{},", r.id, r.code);
    if let Some(q) = r.qty {
        sum.sum_qty += i128::from(q);
        let _ = write!(line, "{q}");
    }
    let _ = write!(
        line,
        ",{},{},{},{:04}-{:02}-{:02},{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:06},",
        amount_text(r.amount_cents),
        r.price,
        r.active,
        r.born.year(),
        r.born.month(),
        r.born.day(),
        r.at.year(),
        r.at.month(),
        r.at.day(),
        r.at.hour(),
        r.at.minute(),
        r.at.second(),
        r.at.nanosecond() / 1_000,
    );
    if let Some(note) = &r.note {
        sum.add_note(note);
        push_field(line, note);
    }
    line.push('\n');
}

/// Writes the header and rows `1..=rows` to `out`. Returns the bytes written and the checksum.
pub fn write_rows<W: Write>(mut out: W, rows: u64) -> io::Result<(u64, Checksum)> {
    let mut sum = Checksum::default();
    let mut line = String::with_capacity(256);
    line.push_str(&COLUMNS.join(","));
    line.push('\n');
    let mut bytes = 0u64;
    for i in 1..=rows {
        push_row(&mut line, i, &mut sum);
        if line.len() >= 64 * 1024 {
            out.write_all(line.as_bytes())?;
            bytes += u64::try_from(line.len()).expect("small");
            line.clear();
        }
    }
    out.write_all(line.as_bytes())?;
    bytes += u64::try_from(line.len()).expect("small");
    out.flush()?;
    Ok((bytes, sum))
}

/// A generated file.
#[derive(Debug, Clone, Copy)]
pub struct Generated {
    pub bytes: u64,
    pub checksum: Checksum,
}

/// Writes the CSV of `rows` rows to `path` (replacing it) and syncs it to disk.
pub fn write_csv(path: &Path, rows: u64) -> io::Result<Generated> {
    let file = File::create(path)?;
    let mut out = BufWriter::with_capacity(1 << 20, file);
    let (bytes, checksum) = write_rows(&mut out, rows)?;
    let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
    file.sync_all()?;
    Ok(Generated { bytes, checksum })
}
