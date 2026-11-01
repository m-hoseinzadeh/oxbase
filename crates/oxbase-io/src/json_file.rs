//! JSON (array of objects, NDJSON): reader (read stream) and writer (write stream).
//!
//! # Reading
//!
//! - Encoding: UTF-8 or UTF-16 (BOM or [`JsonReadOptions::encoding`]), decoded with
//!   [`crate::encoding::DecodingReader`].
//! - Layout [`JsonLayout::Array`](crate::JsonLayout::Array): one top-level `[...]` of objects,
//!   read **element by element** with a streaming scanner (strings and escapes tracked, depth
//!   counted), each element parsed with `serde_json`; the array is never held whole.
//!   [`JsonLayout::Lines`](crate::JsonLayout::Lines): one object per non-blank line.
//! - Columns: the keys of the sampled objects in first-seen order. A key first seen after the
//!   sample is a [`crate::RowIssue`] for that row ("unknown key 'x'"), the value dropped. A
//!   missing key is NULL. A row that is not an object is an issue.
//! - Values: `null` → NULL; booleans, numbers (integers within `i64` as `int`, others
//!   `decimal` text exactly as written when the column is decimal, else `float`), strings
//!   (parsed per the column type with [`crate::ValueFormats`]); **nested objects and arrays are
//!   JSON text** (logical type `json`, compact form).
//! - Progress: file bytes consumed. Memory: one batch plus the largest single element.
//!
//! # Writing
//!
//! `arrow-json` (`ArrayWriter` / `LineDelimitedWriter`) after converting columns it would
//! write in a lossy or non-portable way: decimals as JSON numbers written from their exact text,
//! 64-bit integers as numbers (consumers that use doubles may round above 2^53: noted in the
//! report when such values occur), binary as base64 or hex strings
//! ([`JsonWriteOptions::binary`]), UUIDs as strings, JSON-text columns embedded as JSON values
//! (not strings), timestamps ISO 8601. `explicit_nulls` writes `"col": null`. `pretty` indents
//! the array layout.

use std::path::Path;

use arrow_schema::SchemaRef;
use oxbase_driver::CancellationToken;
use oxbase_types::Result;

use crate::{
    ColumnSpec, JsonReadOptions, JsonWriteOptions, PreviewLimits, SinkFile, SinkWriter,
    SourcePreview, SourceReader,
};

/// Samples a JSON file (see [`crate::preview`]).
pub fn preview(
    path: &Path,
    options: &JsonReadOptions,
    limits: PreviewLimits,
    cancel: &CancellationToken,
) -> Result<SourcePreview> {
    let _ = (path, options, limits, cancel);
    unimplemented!("m5: read")
}

/// Opens a JSON file for reading (see [`crate::open`]).
pub fn open(
    path: &Path,
    options: &JsonReadOptions,
    columns: &[ColumnSpec],
    batch_rows: usize,
) -> Result<Box<dyn SourceReader>> {
    let _ = (path, options, columns, batch_rows);
    unimplemented!("m5: read")
}

/// The JSON writer over `file` (see [`crate::create_writer`]).
pub fn writer(
    file: SinkFile,
    schema: SchemaRef,
    options: &JsonWriteOptions,
) -> Result<Box<dyn SinkWriter>> {
    crate::write::json::writer(file, &schema, options)
}
