//! The readers (read stream): CSV / TSV, JSON / NDJSON, XLSX and Parquet behind
//! [`crate::preview`] and [`crate::open`]. Contracts: `docs/m5-contracts.md` §3.3-3.4 and the
//! module docs of [`crate::csv_file`], [`crate::json_file`], [`crate::xlsx_file`] and
//! [`crate::parquet_file`] (their `preview` / `open` delegate here).
//!
//! Shared pieces: [`column::ColumnBuilder`] turns cells (text, or typed spreadsheet / JSON
//! values) into the canonical load types with row issues instead of failures, [`values`]
//! parses text with [`crate::ValueFormats`], and [`text`] opens a text file with its encoding
//! resolved and a byte counter for progress.
//!
//! Hostile input: every reader keeps memory bounded by limits it checks itself (a 64 MiB row
//! or element cap for text formats, decompressed-size and cell-size caps for XLSX checked
//! before `calamine` sees the workbook, column-chunk bounds for Parquet), and errors name the
//! file by its base name only.

pub(crate) mod column;
pub(crate) mod columnar;
pub(crate) mod delimited;
pub(crate) mod json_rows;
pub(crate) mod sheet;
pub(crate) mod text;
pub(crate) mod values;
mod xlsx_guard;

use std::collections::HashSet;
use std::fmt::Display;
use std::io;
use std::path::Path;

use oxbase_driver::CancellationToken;
use oxbase_types::{Error, Result};

use crate::file_label;

/// The longest row (CSV), element (JSON) or line (NDJSON) a text reader accepts. A missing
/// closing quote would otherwise read the rest of the file into one field.
pub(crate) const MAX_ROW_BYTES: u64 = 64 * 1024 * 1024;

/// Characters of a value quoted in a row issue.
const MAX_QUOTED_CHARS: usize = 200;

/// Characters of a value shown in a preview row.
const MAX_DISPLAY_CHARS: usize = 1_000;

/// Characters kept of a column name.
const MAX_NAME_CHARS: usize = 255;

/// An I/O error naming the file by its base name.
pub(crate) fn io_error(path: &Path, e: &io::Error) -> Error {
    Error::Io {
        kind: e.kind().to_string(),
        message: format!("{}: {e}", file_label(path)),
    }
}

/// `InvalidInput` naming the file by its base name.
pub(crate) fn invalid(path: &Path, message: impl Display) -> Error {
    Error::invalid_input(format!("{}: {message}", file_label(path)))
}

/// `Err(Cancelled)` once `cancel` fired.
pub(crate) fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// `value` in quotes for a message, cut after 200 characters.
pub(crate) fn quote(value: &str) -> String {
    match value.char_indices().nth(MAX_QUOTED_CHARS) {
        Some((cut, _)) => format!("'{}…'", &value[..cut]),
        None => format!("'{value}'"),
    }
}

/// `value` for a preview row, cut after 1000 characters.
pub(crate) fn display(value: &str) -> String {
    match value.char_indices().nth(MAX_DISPLAY_CHARS) {
        Some((cut, _)) => format!("{}…", &value[..cut]),
        None => value.to_owned(),
    }
}

/// Column names from header texts: trimmed, empty ones become `column_<n>` (1-based
/// position), repeated ones (compared without case, as most databases do) get `_2`, `_3`, ...
pub(crate) fn unique_names(raw: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(raw.len());
    for (i, name) in raw.into_iter().enumerate() {
        let trimmed = name.trim();
        let base = if trimmed.is_empty() {
            format!("column_{}", i + 1)
        } else {
            match trimmed.char_indices().nth(MAX_NAME_CHARS) {
                Some((cut, _)) => trimmed[..cut].to_owned(),
                None => trimmed.to_owned(),
            }
        };
        let mut candidate = base.clone();
        let mut n = 2_u64;
        while !seen.insert(candidate.to_lowercase()) {
            candidate = format!("{base}_{n}");
            n += 1;
        }
        out.push(candidate);
    }
    out
}

/// `column_1` ... `column_<n>`.
pub(crate) fn generated_names(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("column_{i}")).collect()
}

/// A monotonic progress position: never goes down, never passes `total` (reported only once
/// the reader is done).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Progress {
    position: u64,
    total: u64,
}

impl Progress {
    pub(crate) fn new(total: u64) -> Self {
        Self { position: 0, total }
    }

    /// Records `position` (clamped below the total until [`Progress::finish`]).
    pub(crate) fn advance(&mut self, position: u64) -> u64 {
        let capped = position.min(self.total.saturating_sub(1));
        self.position = self.position.max(capped);
        self.position
    }

    /// The reader is done: the position is the total.
    pub(crate) fn finish(&mut self) -> u64 {
        self.position = self.total;
        self.position
    }

    pub(crate) fn position(&self) -> u64 {
        self.position
    }

    pub(crate) fn total(&self) -> u64 {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_never_empty() {
        let names = unique_names(
            ["id", "", "name", "Name", " name ", "column_2"]
                .map(String::from)
                .to_vec(),
        );
        assert_eq!(
            names,
            ["id", "column_2", "name", "Name_2", "name_3", "column_2_2"]
        );
        assert_eq!(generated_names(2), ["column_1", "column_2"]);
    }

    #[test]
    fn quoting_cuts_long_values() {
        assert_eq!(quote("abc"), "'abc'");
        let long = "é".repeat(500);
        let quoted = quote(&long);
        assert_eq!(quoted.chars().count(), 200 + 3);
        assert!(quoted.ends_with("…'"));
        assert_eq!(display(&long).chars().count(), 500);
    }

    #[test]
    fn progress_is_monotonic_and_ends_at_the_total() {
        let mut p = Progress::new(100);
        assert_eq!(p.advance(10), 10);
        assert_eq!(p.advance(5), 10);
        assert_eq!(p.advance(500), 99);
        assert_eq!(p.finish(), 100);
        assert_eq!(p.position(), 100);
        assert_eq!(p.total(), 100);
    }
}
