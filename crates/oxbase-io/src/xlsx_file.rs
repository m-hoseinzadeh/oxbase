//! XLSX: reader (read stream, `calamine`) and writer (write stream, `rust_xlsxwriter`).
//!
//! # Reading
//!
//! - `calamine::Xlsx` with [`XlsxReadOptions::sheet`] (default: the first sheet); the preview
//!   lists every sheet in [`crate::Detected::sheets`].
//! - Rows stream through `Xlsx::worksheet_cells_reader` (cells in row order), so a sheet is
//!   never loaded as a whole `Range`. The workbook's shared-strings table is held in memory
//!   (calamine loads it on open): that is the format's floor, bounded by the file.
//! - [`XlsxReadOptions::header_row`] (1-based; 0 = no header) names the columns; rows above it
//!   are skipped; the column count is the widest of header and sample.
//! - Cell types: numbers (`int` when integral within `i64` across the sample, else `float`),
//!   booleans, dates / date-times by the cell's number format (`date` when the time part is
//!   always midnight, else `timestamp`; 1904 workbooks handled by calamine), strings (parsed
//!   per column type with [`crate::ValueFormats`]), errors (`#DIV/0!`: a row issue, cell NULL),
//!   empty cells NULL. **Formulas are read as their cached value**, never evaluated.
//! - Progress: compressed bytes of the sheet's XML entry consumed (calamine reads through the
//!   zip entry), mapped onto the file size; coarse but monotonic.
//! - Hostile workbooks: before `calamine` opens the file, every XML part it loads whole is
//!   decompressed and capped (256 MiB each, 512 MiB together; the zip's declared sizes are not
//!   trusted), the shared-strings `uniqueCount` must fit the part, and the worksheet is scanned
//!   once for oversized texts, tags and cells (16 MiB). A workbook over a limit is
//!   `InvalidInput` naming the part.
//!
//! # Writing
//!
//! - `rust_xlsxwriter` in constant-memory mode (`add_worksheet_with_constant_memory`: rows are
//!   flushed to a temp file as they are written), saved with `save_to_writer` into the
//!   [`SinkFile`].
//! - Header row (bold) unless [`XlsxWriteOptions::header`] is off. Typed cells: numbers
//!   (integers above 2^53 and decimals with more than 15 significant digits are written as
//!   text so no digit is lost, noted in the report), booleans, dates and timestamps as Excel
//!   date numbers with [`XlsxWriteOptions::date_format`] / `timestamp_format` (timestamps with a
//!   zone in UTC), text (cut to 32 767 characters, the cell limit, noted in the report), binary
//!   as `0x` hex text, JSON / lists as JSON text.
//! - **Sheet splitting**: after [`XlsxWriteOptions::max_rows_per_sheet`] rows (header
//!   included, at most 1 048 576) the writer starts `<sheet_name> (2)`, `(3)`, ... each with
//!   the header again; [`crate::SinkReport::sheets`] lists them.
//! - **Formula injection**: data is only ever written with `write_string` / typed writers,
//!   never `write_formula`; with [`XlsxWriteOptions::formula_guard`] text that starts with
//!   `=`, `+`, `-`, `@`, tab or CR additionally gets the quote-prefix cell format
//!   ([`crate::formula::needs_guard`]).

use std::path::Path;

use arrow_schema::SchemaRef;
use oxbase_driver::CancellationToken;
use oxbase_types::Result;

use crate::{
    ColumnSpec, PreviewLimits, SinkFile, SinkWriter, SourcePreview, SourceReader, XlsxReadOptions,
    XlsxWriteOptions,
};

/// Samples a worksheet (see [`crate::preview`]).
pub fn preview(
    path: &Path,
    options: &XlsxReadOptions,
    limits: PreviewLimits,
    cancel: &CancellationToken,
) -> Result<SourcePreview> {
    crate::read::sheet::preview(path, options, limits, cancel)
}

/// Opens a worksheet for reading (see [`crate::open`]).
pub fn open(
    path: &Path,
    options: &XlsxReadOptions,
    columns: &[ColumnSpec],
    batch_rows: usize,
) -> Result<Box<dyn SourceReader>> {
    crate::read::sheet::open(path, options, columns, batch_rows)
}

/// The XLSX writer over `file` (see [`crate::create_writer`]).
// The signature is the contract (docs/m5-contracts.md §3.5); the writer only reads the schema.
#[allow(clippy::needless_pass_by_value)]
pub fn writer(
    file: SinkFile,
    schema: SchemaRef,
    options: &XlsxWriteOptions,
) -> Result<Box<dyn SinkWriter>> {
    crate::write::xlsx::writer(file, &schema, options)
}
