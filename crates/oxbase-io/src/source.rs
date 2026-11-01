//! Reading files: preview, column specs, [`SourceReader`] and the async adapter.

use std::path::Path;

use arrow_array::RecordBatch;
use arrow_schema::{Field, Schema, SchemaRef};
use futures::stream::{self, BoxStream};
use oxbase_driver::CancellationToken;
use oxbase_types::{DataType, Error, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::{ReadOptions, TextEncoding, arrow_type_for};

/// How much a preview reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewLimits {
    /// Rows read for type inference (and the most rows returned).
    pub sample_rows: usize,
    /// Rows returned as text in [`SourcePreview::rows`].
    pub display_rows: usize,
    /// Stop sampling after this many bytes even if fewer rows were read (very wide rows).
    pub sample_bytes: u64,
}

impl Default for PreviewLimits {
    fn default() -> Self {
        Self {
            sample_rows: 1_000,
            display_rows: 100,
            sample_bytes: 8 * 1024 * 1024,
        }
    }
}

/// What a reader detected (or was told) about the file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Detected {
    /// The encoding used (text formats; `Auto` resolved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<TextEncoding>,
    /// A byte order mark was present.
    #[serde(default)]
    pub bom: bool,
    /// CSV: the delimiter used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delimiter: Option<char>,
    /// JSON: `true` for one object per line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_lines: Option<bool>,
    /// XLSX: every sheet name in workbook order, and the sheet read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sheets: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    /// Parquet: rows in the file (from its metadata).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_rows: Option<u64>,
}

/// One source column as the preview sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewColumn {
    /// 0-based position in the source.
    pub index: usize,
    /// Header text (empty or duplicate headers are made unique: `column_3`, `name_2`).
    pub name: String,
    /// The inferred logical type (Parquet: from the file's schema).
    pub data_type: DataType,
    /// NULLs (or empty values read as NULL) were seen in the sample.
    pub nullable: bool,
    /// Why the inferred type may be wrong ("3 of 1000 values are not integers; read as
    /// text", "longest value has 1200 characters").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// What [`preview`] returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcePreview {
    pub columns: Vec<PreviewColumn>,
    /// The first [`PreviewLimits::display_rows`] rows as display text (`None` = NULL), one
    /// inner vector per row with one entry per column.
    pub rows: Vec<Vec<Option<String>>>,
    /// Rows read for inference.
    pub sampled_rows: usize,
    /// The whole sample was the whole file.
    pub complete: bool,
    /// File size in bytes.
    pub total_bytes: u64,
    pub detected: Detected,
    /// File-level warnings (malformed bytes replaced, ragged rows seen, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// One source column as it will be read: the user's choice after the preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnSpec {
    /// Column name (the preview's, or renamed).
    pub name: String,
    /// The logical type to read the column as (the preview's inferred type, or an override).
    pub data_type: DataType,
}

impl ColumnSpec {
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }

    /// The specs a preview suggests (its names and inferred types).
    pub fn from_preview(preview: &SourcePreview) -> Vec<Self> {
        preview
            .columns
            .iter()
            .map(|c| Self::new(c.name.clone(), c.data_type.clone()))
            .collect()
    }

    /// The schema a reader of `specs` produces: one nullable field per spec with
    /// [`arrow_type_for`] its type.
    pub fn schema(specs: &[Self]) -> SchemaRef {
        std::sync::Arc::new(Schema::new(
            specs
                .iter()
                .map(|s| Field::new(&s.name, arrow_type_for(&s.data_type), true))
                .collect::<Vec<_>>(),
        ))
    }
}

/// A cell or row that could not be read as specified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RowIssue {
    /// Source row number, 1-based, counting data rows only (the header and skipped lines are
    /// not rows). For CSV the issue message also names the line when it differs.
    pub row: u64,
    /// Row index in the batch it belongs to.
    pub batch_row: usize,
    /// The column, when the issue is one cell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// What went wrong ("'abc' is not an integer", "row has 7 fields, expected 5"). Never
    /// more than 200 characters of the source value.
    pub message: String,
}

/// One batch from a [`SourceReader`].
#[derive(Debug, Clone)]
pub struct SourceBatch {
    /// Rows in the reader's schema ([`ColumnSpec::schema`]). Cells listed in `issues` are NULL.
    pub batch: RecordBatch,
    /// Source row number (1-based, see [`RowIssue::row`]) of the batch's first row.
    pub first_row: u64,
    /// Problems in this batch's rows, in row order.
    pub issues: Vec<RowIssue>,
    /// Byte position in the source after this batch ([`SourceReader::bytes_read`]).
    pub bytes_read: u64,
}

/// Reads a file as Arrow batches in the schema of its column specs. Synchronous: run it on a
/// blocking thread ([`spawn_reader`]).
pub trait SourceReader: Send {
    /// The schema of every batch ([`ColumnSpec::schema`] of the specs it was opened with).
    fn schema(&self) -> SchemaRef;

    /// File size in bytes.
    fn total_bytes(&self) -> u64;

    /// Bytes of the file consumed so far: monotonic, `total_bytes` after the last batch.
    /// (Parquet and XLSX advance per row group / per compressed chunk.)
    fn bytes_read(&self) -> u64;

    /// The next batch of at most the `batch_rows` given to [`open`]; `None` at the end. A
    /// malformed *file* (not a malformed value) is an error (`InvalidInput` naming the line /
    /// row); after an error the reader is done.
    fn next_batch(&mut self) -> Result<Option<SourceBatch>>;
}

/// Opens `path` with `options`, samples it and infers column types (module docs of the crate,
/// *Reading*). Blocking. `InvalidInput` for bad options or an unreadable file (naming the
/// file's base name only), `Cancelled` when `cancel` fires between rows.
pub fn preview(
    path: &Path,
    options: &ReadOptions,
    limits: PreviewLimits,
    cancel: &CancellationToken,
) -> Result<SourcePreview> {
    options.validate()?;
    match options {
        ReadOptions::Csv(o) => crate::csv_file::preview(path, o, limits, cancel),
        ReadOptions::Json(o) => crate::json_file::preview(path, o, limits, cancel),
        ReadOptions::Xlsx(o) => crate::xlsx_file::preview(path, o, limits, cancel),
        ReadOptions::Parquet(o) => crate::parquet_file::preview(path, o, limits, cancel),
    }
}

/// Opens `path` for reading every row as `columns` (one spec per source column, in source
/// order; the count must match the preview's) in batches of at most `batch_rows` rows.
/// Blocking.
pub fn open(
    path: &Path,
    options: &ReadOptions,
    columns: &[ColumnSpec],
    batch_rows: usize,
) -> Result<Box<dyn SourceReader>> {
    options.validate()?;
    if columns.is_empty() {
        return Err(Error::invalid_input("no columns to read"));
    }
    let batch_rows = batch_rows.max(1);
    match options {
        ReadOptions::Csv(o) => crate::csv_file::open(path, o, columns, batch_rows),
        ReadOptions::Json(o) => crate::json_file::open(path, o, columns, batch_rows),
        ReadOptions::Xlsx(o) => crate::xlsx_file::open(path, o, columns, batch_rows),
        ReadOptions::Parquet(o) => crate::parquet_file::open(path, o, columns, batch_rows),
    }
}

/// Runs `reader` on a blocking thread and yields its batches as a stream, with at most
/// `capacity` (at least 1) batches buffered ahead of the consumer. The stream ends after the
/// last batch, after the first error (which it yields), or with one `Err(Cancelled)` once
/// `cancel` fires (checked before each batch). Dropping the stream stops the thread after its
/// current batch.
pub fn spawn_reader(
    mut reader: Box<dyn SourceReader>,
    cancel: CancellationToken,
    capacity: usize,
) -> BoxStream<'static, Result<SourceBatch>> {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    tokio::task::spawn_blocking(move || {
        loop {
            if cancel.is_cancelled() {
                let _ = tx.blocking_send(Err(Error::Cancelled));
                break;
            }
            match reader.next_batch() {
                Ok(Some(batch)) => {
                    if tx.blocking_send(Ok(batch)).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                    break;
                }
            }
        }
    });
    Box::pin(stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{ArrayRef, Int64Array};
    use futures::StreamExt;

    use super::*;

    /// Yields `batches` one-row batches, then ends.
    struct Counter {
        schema: SchemaRef,
        left: u64,
        read: u64,
    }

    impl SourceReader for Counter {
        fn schema(&self) -> SchemaRef {
            self.schema.clone()
        }
        fn total_bytes(&self) -> u64 {
            100
        }
        fn bytes_read(&self) -> u64 {
            self.read
        }
        fn next_batch(&mut self) -> Result<Option<SourceBatch>> {
            if self.left == 0 {
                return Ok(None);
            }
            self.left -= 1;
            self.read += 10;
            let column: ArrayRef =
                Arc::new(Int64Array::from(vec![i64::try_from(self.read).unwrap()]));
            Ok(Some(SourceBatch {
                batch: RecordBatch::try_new(self.schema.clone(), vec![column]).unwrap(),
                first_row: self.read / 10,
                issues: Vec::new(),
                bytes_read: self.read,
            }))
        }
    }

    fn counter(left: u64) -> Box<dyn SourceReader> {
        Box::new(Counter {
            schema: ColumnSpec::schema(&[ColumnSpec::new("n", DataType::INT64)]),
            left,
            read: 0,
        })
    }

    #[test]
    fn specs_make_the_load_schema() {
        let schema = ColumnSpec::schema(&[
            ColumnSpec::new("n", DataType::INT32),
            ColumnSpec::new(
                "d",
                DataType::Decimal {
                    precision: Some(10),
                    scale: Some(2),
                },
            ),
        ]);
        assert_eq!(schema.field(0).data_type(), &arrow_schema::DataType::Int64);
        assert_eq!(schema.field(1).data_type(), &arrow_schema::DataType::Utf8);
        assert!(schema.field(0).is_nullable());
    }

    #[tokio::test]
    async fn spawned_reader_streams_and_cancels() {
        let batches: Vec<_> = spawn_reader(counter(3), CancellationToken::new(), 1)
            .collect()
            .await;
        assert_eq!(batches.len(), 3);
        let positions: Vec<u64> = batches
            .iter()
            .map(|b| b.as_ref().unwrap().bytes_read)
            .collect();
        assert_eq!(positions, [10, 20, 30]);

        let cancel = CancellationToken::new();
        cancel.cancel();
        let items: Vec<_> = spawn_reader(counter(3), cancel, 1).collect().await;
        assert_eq!(items.len(), 1);
        assert!(items[0].as_ref().unwrap_err().is_cancelled());
    }

    #[test]
    fn open_rejects_bad_options_before_touching_the_file() {
        let csv = crate::CsvReadOptions {
            delimiter: '"',
            ..crate::CsvReadOptions::default()
        };
        let err = open(
            Path::new("missing.csv"),
            &ReadOptions::Csv(csv),
            &[ColumnSpec::new("a", DataType::Text)],
            10,
        )
        .err()
        .unwrap();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);
        let err = open(
            Path::new("missing.csv"),
            &ReadOptions::Csv(crate::CsvReadOptions::default()),
            &[],
            10,
        )
        .err()
        .unwrap();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);
    }
}
