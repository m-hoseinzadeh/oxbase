//! Import/export: CSV, JSON, XLSX, Parquet and SQL dump, all over Arrow `RecordBatch`es
//! (PLAN §1, rule 2). Contracts: `docs/m5-contracts.md` §3.
//!
//! The crate reads and writes **files**; it knows nothing about sessions, tables or the UI.
//! `oxbase_services::import` / `export` connect it to databases, and the app hands them paths
//! that only Rust-side native dialogs produced (`docs/m5-contracts.md` §6).
//!
//! # Reading (read stream)
//!
//! 1. [`preview`] opens a file with [`ReadOptions`], detects what it can (encoding and BOM,
//!    delimiter, header, sheets), reads a sample of at most [`PreviewLimits::sample_rows`] rows,
//!    infers a logical type per column ([`infer::TypeInference`]) and returns a
//!    [`SourcePreview`] (columns with inferred types and warnings, the first rows as text).
//! 2. The user (or the CLI) confirms or overrides the column types: one [`ColumnSpec`] per
//!    source column.
//! 3. [`open`] returns a [`SourceReader`] that yields [`SourceBatch`]es of at most
//!    `batch_rows` rows **in the load schema of the specs**: each column has the Arrow type
//!    [`arrow_type_for`] gives for its logical type (the same canonical types the bulk loaders
//!    take, `oxbase_dialect::bulk::load_arrow_type`). Cells that cannot be read as their
//!    column's type are NULL in the batch and listed in [`SourceBatch::issues`]; the import
//!    service applies its error policy to them.
//! 4. [`spawn_reader`] runs a reader on a blocking thread and turns it into an async stream
//!    with backpressure (at most `capacity` batches buffered), stopping at the
//!    `CancellationToken`.
//!
//! # Writing (write stream)
//!
//! [`create_writer`] returns a [`SinkWriter`] for a destination path, the batches' schema and
//! [`WriteOptions`]. Writers stream: they never hold more than a batch (plus the format's own
//! buffers: a Parquet row group, one XLSX row in constant-memory mode). Every writer writes to
//! a [`SinkFile`]: a temporary file **in the destination's directory**, renamed over the
//! destination by [`SinkWriter::finish`] and deleted by [`SinkWriter::abort`] or on drop, so a
//! failed or cancelled export never leaves a partial file and never damages an existing one.
//! [`spawn_writer`] runs a writer on a blocking thread behind an async handle.
//!
//! # Invariants (every format)
//!
//! - **Bounded memory.** Readers and writers stream; nothing loads a whole file. Exceptions are
//!   documented per format (XLSX reading keeps the workbook's shared-strings table in memory;
//!   reading a Parquet row group is bounded by the file's row-group size).
//! - **Progress.** [`SourceReader::bytes_read`] is the byte position in the source file
//!   (monotonic, reaches [`SourceReader::total_bytes`] at the end);
//!   [`SinkWriter::bytes_written`] counts bytes handed to the file.
//! - **Cancel.** Readers and writers are synchronous and stop between batches; the async
//!   adapters check the token before each batch.
//! - **Formula injection.** Text cells that a spreadsheet would run as a formula (starting with
//!   `=`, `+`, `-`, `@`, tab or carriage return) are neutralized in CSV and XLSX output unless
//!   the options turn it off ([`formula`]).
//! - **Secrets.** Paths are not secrets, but errors name only the file's base name, never a
//!   full path (the app forwards errors to the UI).
//!
//! # Formats
//!
//! | Format | Read | Write | Crate |
//! |---|---|---|---|
//! | CSV / TSV | [`csv_file`] | [`csv_file`] | `csv` 1.4 + `encoding_rs` |
//! | JSON (array, NDJSON) | [`json_file`] | [`json_file`] | `serde_json` (read), `arrow-json` (write) |
//! | XLSX | [`xlsx_file`] (`calamine`, streaming cell reader) | [`xlsx_file`] (`rust_xlsxwriter`, constant memory) | |
//! | Parquet | [`parquet_file`] | [`parquet_file`] | `parquet` 60 (arrow major 60) |
//! | SQL dump | - (open it in the query editor) | [`sql_dump`] | `oxbase-dialect` cells |

pub mod csv_file;
pub mod encoding;
pub mod formula;
pub mod infer;
pub mod json_file;
mod options;
pub mod parquet_file;
mod read;
mod sink;
mod source;
pub mod sql_dump;
mod types;
pub mod xlsx_file;

pub use options::{
    BinaryEncoding, CsvReadOptions, CsvWriteOptions, FileFormat, JsonLayout, JsonReadOptions,
    JsonWriteOptions, LineEnding, ParquetCompression, ParquetReadOptions, ParquetWriteOptions,
    QuoteStyle, ReadOptions, SqlDumpOptions, TextEncoding, ValueFormats, WriteOptions,
    XLSX_MAX_ROWS, XlsxReadOptions, XlsxWriteOptions,
};
pub use sink::{
    SinkFile, SinkReport, SinkWriter, WriteContext, WriterHandle, create_writer, spawn_writer,
};
pub use source::{
    ColumnSpec, Detected, PreviewColumn, PreviewLimits, RowIssue, SourceBatch, SourcePreview,
    SourceReader, open, preview, spawn_reader,
};
pub use types::{arrow_type_for, file_label, logical_type};
