//! Delimited text (CSV, TSV): reader (read stream) and writer (write stream).
//!
//! # Reading
//!
//! - The file is decoded with [`crate::encoding::DecodingReader`] (encoding from
//!   [`CsvReadOptions::encoding`], `Auto` resolved with [`crate::encoding::resolve`] on the first
//!   64 KiB; a BOM is removed) and parsed with the `csv` crate (`flexible(true)`, so ragged rows
//!   reach the reader, which applies [`CsvReadOptions::allow_ragged`]). `\r\n` and `\n` always
//!   end rows; with [`LineEnding::Cr`](crate::LineEnding::Cr) a lone `\r` does too.
//! - Quoted fields may hold the delimiter, quotes (doubled or escaped) and line ends. A
//!   multi-line field is one value; row numbers count records, not lines.
//! - Values matching [`CsvReadOptions::null_tokens`] are NULL. Other text is parsed as its
//!   column's type with [`crate::ValueFormats`]; failures are [`crate::RowIssue`]s (cell NULL).
//! - The header row names the columns (empty names become `column_<n>`, duplicates get `_2`,
//!   `_3`, ...). Without a header: `column_1` ... `column_<n>` from the widest sampled row.
//! - Empty file: a preview with no columns and a warning; header only: the columns, no rows,
//!   every type `text`.
//! - Progress: bytes of the *file* (before decoding) consumed, via
//!   [`crate::encoding::CountingReader`]; the `csv` reader buffers 64 KiB, so the position runs
//!   ahead of the rows by at most that.
//! - Memory: one batch of rows plus fixed buffers; a single field longer than 64 MiB is an
//!   error (a missing closing quote would otherwise read the rest of the file into one field).
//!
//! # Writing
//!
//! - Header with the field names (unless [`CsvWriteOptions::header`] is off), one record per
//!   row, [`CsvWriteOptions::quote_style`], `escape` or doubled quotes, line ends `\n` or
//!   `\r\n`, optional BOM, then [`crate::encoding::EncodingWriter`] (a character the encoding
//!   cannot hold fails the export, naming it).
//! - NULL is [`CsvWriteOptions::null_token`]. Values are written as text with
//!   [`crate::ValueFormats`]: decimals exactly as their text (decimal separator replaced), floats
//!   in shortest round-trip form, booleans `true` / `false`, dates and times ISO 8601 or the
//!   given formats, timestamps with a zone in UTC with `Z` unless a format is given, binary as
//!   `0x` + hex, UUIDs hyphenated, JSON as compact JSON text, lists / structs as JSON text.
//! - With [`CsvWriteOptions::formula_guard`], text values get [`crate::formula::guard_text`].

use std::path::Path;

use arrow_schema::SchemaRef;
use oxbase_driver::CancellationToken;
use oxbase_types::Result;

use crate::{
    ColumnSpec, CsvReadOptions, CsvWriteOptions, PreviewLimits, SinkFile, SinkWriter,
    SourcePreview, SourceReader,
};

/// Samples a delimited file (see [`crate::preview`]).
pub fn preview(
    path: &Path,
    options: &CsvReadOptions,
    limits: PreviewLimits,
    cancel: &CancellationToken,
) -> Result<SourcePreview> {
    let _ = (path, options, limits, cancel);
    unimplemented!("m5: read")
}

/// Opens a delimited file for reading (see [`crate::open`]).
pub fn open(
    path: &Path,
    options: &CsvReadOptions,
    columns: &[ColumnSpec],
    batch_rows: usize,
) -> Result<Box<dyn SourceReader>> {
    let _ = (path, options, columns, batch_rows);
    unimplemented!("m5: read")
}

/// The delimited-text writer over `file` (see [`crate::create_writer`]).
// The signature is the contract (docs/m5-contracts.md §3.5); the writer only reads the schema.
#[allow(clippy::needless_pass_by_value)]
pub fn writer(
    file: SinkFile,
    schema: SchemaRef,
    options: &CsvWriteOptions,
) -> Result<Box<dyn SinkWriter>> {
    crate::write::csv::writer(file, &schema, options)
}
