//! Parquet: reader (read stream) and writer (write stream), `parquet` 60 with the arrow
//! feature (same arrow major as the workspace).
//!
//! # Reading
//!
//! - `ParquetRecordBatchReaderBuilder` over the file; batches of `batch_rows`. Memory: one
//!   decoded row group column chunk at a time (bounded by how the file was written).
//! - The preview's types come from the file's Arrow schema ([`crate::logical_type`]); no
//!   inference. `total_rows` from the footer.
//! - Batches are cast to the load schema of the specs ([`crate::arrow_type_for`]): integers to
//!   `Int64` / `UInt64`, `Decimal128/256` to exact decimal text, timestamps to microseconds
//!   (nanosecond precision is truncated: a row issue only when the value had sub-microsecond
//!   digits), lists / structs / maps to JSON text, dictionary-encoded columns to their values.
//!   A user override the cast cannot do (text → int) parses per value like CSV, with row
//!   issues.
//! - Progress: compressed bytes of the row groups read so far.
//!
//! # Writing
//!
//! - `ArrowWriter` with [`ParquetWriteOptions::compression`]
//!   ([`crate::ParquetCompression::to_parquet`]) and `max_row_group_size =`
//!   [`ParquetWriteOptions::row_group_rows`]. The batches' schema is written as is (types
//!   preserved: decimals, timestamps with their zone, lists), except driver text columns marked
//!   JSON keep the `arrow.json` extension metadata.

use std::path::Path;

use arrow_schema::SchemaRef;
use oxbase_driver::CancellationToken;
use oxbase_types::Result;

use crate::{
    ColumnSpec, ParquetReadOptions, ParquetWriteOptions, PreviewLimits, SinkFile, SinkWriter,
    SourcePreview, SourceReader,
};

/// Reads a Parquet file's schema and first rows (see [`crate::preview`]).
pub fn preview(
    path: &Path,
    options: &ParquetReadOptions,
    limits: PreviewLimits,
    cancel: &CancellationToken,
) -> Result<SourcePreview> {
    crate::read::columnar::preview(path, options, limits, cancel)
}

/// Opens a Parquet file for reading (see [`crate::open`]).
pub fn open(
    path: &Path,
    options: &ParquetReadOptions,
    columns: &[ColumnSpec],
    batch_rows: usize,
) -> Result<Box<dyn SourceReader>> {
    crate::read::columnar::open(path, options, columns, batch_rows)
}

/// The Parquet writer over `file` (see [`crate::create_writer`]).
pub fn writer(
    file: SinkFile,
    schema: SchemaRef,
    options: &ParquetWriteOptions,
) -> Result<Box<dyn SinkWriter>> {
    let _ = (file, schema, options);
    unimplemented!("m5: write")
}
