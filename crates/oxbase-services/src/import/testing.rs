//! Test sources and sinks shared by the import and export tests: an in-memory (optionally
//! slow) `SourceReader`, a `SinkWriter` that keeps the batches and writes one line per row
//! through `oxbase_io::SinkFile` (so the atomic-file rules are exercised), and generated rows
//! of every common type.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use arrow_array::{
    ArrayRef, BooleanArray, Date32Array, Int64Array, RecordBatch, StringArray,
    TimestampMicrosecondArray,
};
use arrow_schema::SchemaRef;
use oxbase_io::{
    ColumnSpec, RowIssue, SinkFile, SinkReport, SinkWriter, SourceBatch, SourceReader,
};
use oxbase_types::{DataType, Error, Result};

/// Bytes a [`MemoryReader`] pretends each row takes.
pub(crate) const ROW_BYTES: u64 = 10;

/// Yields prepared batches (in the reader schema of its specs), sleeping `delay` before each.
pub(crate) struct MemoryReader {
    schema: SchemaRef,
    batches: VecDeque<(RecordBatch, Vec<RowIssue>)>,
    total: u64,
    read: u64,
    next_row: u64,
    delay: Duration,
}

impl MemoryReader {
    pub(crate) fn new(specs: &[ColumnSpec], batches: Vec<(RecordBatch, Vec<RowIssue>)>) -> Self {
        let rows: usize = batches.iter().map(|(b, _)| b.num_rows()).sum();
        Self {
            schema: ColumnSpec::schema(specs),
            total: u64::try_from(rows).unwrap() * ROW_BYTES,
            batches: batches.into(),
            read: 0,
            next_row: 1,
            delay: Duration::ZERO,
        }
    }

    #[must_use]
    pub(crate) fn slow(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

impl SourceReader for MemoryReader {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    fn total_bytes(&self) -> u64 {
        self.total
    }

    fn bytes_read(&self) -> u64 {
        self.read
    }

    fn next_batch(&mut self) -> Result<Option<SourceBatch>> {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        let Some((batch, issues)) = self.batches.pop_front() else {
            return Ok(None);
        };
        let batch = RecordBatch::try_new(Arc::clone(&self.schema), batch.columns().to_vec())
            .map_err(|e| Error::internal(e.to_string()))?;
        let rows = u64::try_from(batch.num_rows()).unwrap();
        let first_row = self.next_row;
        self.next_row += rows;
        self.read += rows * ROW_BYTES;
        Ok(Some(SourceBatch {
            batch,
            first_row,
            issues,
            bytes_read: self.read,
        }))
    }
}

/// What [`MemoryWriter`]s wrote, shared with the test.
pub(crate) type Written = Arc<Mutex<Vec<RecordBatch>>>;

/// Keeps every batch in `written` and writes `<rows>\n` per batch into a [`SinkFile`].
pub(crate) struct MemoryWriter {
    file: SinkFile,
    written: Written,
    rows: u64,
    delay: Duration,
}

impl MemoryWriter {
    pub(crate) fn create(
        destination: &Path,
        overwrite: bool,
        written: Written,
        delay: Duration,
    ) -> Result<Box<dyn SinkWriter>> {
        Ok(Box::new(Self {
            file: SinkFile::create(destination, overwrite)?,
            written,
            rows: 0,
            delay,
        }))
    }
}

impl SinkWriter for MemoryWriter {
    fn write(&mut self, batch: &RecordBatch) -> Result<()> {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        writeln!(self.file, "{}", batch.num_rows()).map_err(|e| Error::internal(e.to_string()))?;
        self.rows += u64::try_from(batch.num_rows()).unwrap();
        self.written
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(batch.clone());
        Ok(())
    }

    fn rows_written(&self) -> u64 {
        self.rows
    }

    fn bytes_written(&self) -> u64 {
        self.file.bytes_written()
    }

    fn finish(self: Box<Self>) -> Result<SinkReport> {
        let rows = self.rows;
        let bytes = self.file.commit()?;
        Ok(SinkReport {
            rows,
            bytes,
            ..SinkReport::default()
        })
    }

    fn abort(self: Box<Self>) {
        self.file.abort();
    }
}

/// The specs of [`sample_batch`]: `id int`, `name text`, `amount decimal(12,2)`, `flag bool`,
/// `born date`, `at timestamp(6)`.
pub(crate) fn sample_specs() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("id", DataType::INT64),
        ColumnSpec::new("name", DataType::VarChar { length: Some(100) }),
        ColumnSpec::new(
            "amount",
            DataType::Decimal {
                precision: Some(12),
                scale: Some(2),
            },
        ),
        ColumnSpec::new("flag", DataType::Bool),
        ColumnSpec::new("born", DataType::Date),
        ColumnSpec::new("at", DataType::Timestamp { precision: Some(6) }),
    ]
}

/// Row `i` of the sample data, as normalized text per column (see [`sample_batch`]).
pub(crate) fn sample_row(i: i64) -> Vec<String> {
    let cents = (i * 7_919) % 1_000_000 - 500_000;
    let mut amount = String::new();
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.abs();
    let _ = write!(amount, "{sign}{}.{:02}", abs / 100, abs % 100);
    let born = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()
        + chrono::Duration::days((i * 37) % 20_000);
    let at = chrono::DateTime::from_timestamp_micros(946_684_800_000_000 + i * 7_000_123)
        .unwrap()
        .naive_utc();
    vec![
        i.to_string(),
        format!("name {i} 'q' \"d\" naïve"),
        amount,
        if i % 3 == 0 { "1" } else { "0" }.to_owned(),
        born.format("%Y-%m-%d").to_string(),
        at.format("%Y-%m-%d %H:%M:%S%.6f").to_string(),
    ]
}

/// Rows `first..first + rows` of the sample data in the reader schema of [`sample_specs`]
/// (decimals as text, the flag every third row, names with quotes and a non-ASCII letter).
pub(crate) fn sample_batch(first: i64, rows: usize) -> RecordBatch {
    let ids: Vec<i64> = (first..first + i64::try_from(rows).unwrap()).collect();
    let texts: Vec<Vec<String>> = ids.iter().map(|i| sample_row(*i)).collect();
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(ids.clone())),
        Arc::new(StringArray::from_iter_values(
            texts.iter().map(|t| t[1].clone()),
        )),
        Arc::new(StringArray::from_iter_values(
            texts.iter().map(|t| t[2].clone()),
        )),
        Arc::new(BooleanArray::from(
            ids.iter().map(|i| i % 3 == 0).collect::<Vec<_>>(),
        )),
        Arc::new(Date32Array::from(
            texts
                .iter()
                .map(|t| {
                    let d = chrono::NaiveDate::parse_from_str(&t[4], "%Y-%m-%d").unwrap();
                    i32::try_from(d.signed_duration_since(epoch).num_days()).unwrap()
                })
                .collect::<Vec<_>>(),
        )),
        Arc::new(TimestampMicrosecondArray::from(
            ids.iter()
                .map(|i| 946_684_800_000_000 + i * 7_000_123)
                .collect::<Vec<_>>(),
        )),
    ];
    RecordBatch::try_new(ColumnSpec::schema(&sample_specs()), columns).unwrap()
}

/// A one-column text batch (`text` specs) with `values`.
pub(crate) fn text_batch(values: &[Option<&str>]) -> RecordBatch {
    let specs = [ColumnSpec::new("value", DataType::Text)];
    RecordBatch::try_new(
        ColumnSpec::schema(&specs),
        vec![Arc::new(StringArray::from(values.to_vec()))],
    )
    .unwrap()
}
