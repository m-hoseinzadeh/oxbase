//! The JSON writer (rules: [`crate::json_file`] module docs, *Writing*).
//!
//! Rows are written by `arrow-json`'s value encoders ([`make_encoder`]) under oxbase's own
//! layout: decimals and 64-bit integers come out as exact JSON numbers, nested values as JSON,
//! and [`JsonCells`] covers what `arrow-json` would write in a non-portable way (binary, UUIDs,
//! JSON text, durations).

use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arrow_array::{Array, RecordBatch};
use arrow_json::writer::{EncoderOptions, make_encoder};
use arrow_schema::{DataType as ArrowType, Schema};
use oxbase_types::{Error, Result};

use super::cells::{
    EXACT_DOUBLE_INT, JsonCells, arrow_error, float_at, int_at, null_at, push_json_string,
    push_pretty_json,
};
use super::{Count, Tally, already_failed, io_error, plural};
use crate::{JsonLayout, JsonWriteOptions, SinkFile, SinkReport, SinkWriter};

struct JsonSink {
    out: BufWriter<Tally>,
    bytes: Arc<AtomicU64>,
    options: JsonWriteOptions,
    encoder_options: EncoderOptions,
    /// `"name":` per column, JSON-escaped.
    keys: Vec<Vec<u8>>,
    row: Vec<u8>,
    pretty: Vec<u8>,
    rows: u64,
    failed: bool,
    big_ints: Count,
    non_finite: Count,
}

/// The JSON writer over `file`.
pub(crate) fn writer(
    file: SinkFile,
    schema: &Schema,
    options: &JsonWriteOptions,
) -> Result<Box<dyn SinkWriter>> {
    let tally = Tally::new(file);
    let bytes = tally.counter();
    let mut out = BufWriter::with_capacity(64 * 1024, tally);
    if options.layout == JsonLayout::Array {
        out.write_all(b"[").map_err(|e| io_error(&e))?;
    }
    let keys = schema
        .fields()
        .iter()
        .map(|f| {
            let mut key = Vec::with_capacity(f.name().len() + 3);
            push_json_string(f.name(), &mut key);
            key.push(b':');
            key
        })
        .collect();
    let encoder_options = EncoderOptions::default()
        .with_explicit_nulls(true)
        .with_encoder_factory(Arc::new(JsonCells {
            binary: options.binary,
        }));
    Ok(Box::new(JsonSink {
        out,
        bytes,
        options: options.clone(),
        encoder_options,
        keys,
        row: Vec::new(),
        pretty: Vec::new(),
        rows: 0,
        failed: false,
        big_ints: Count::default(),
        non_finite: Count::default(),
    }))
}

impl JsonSink {
    /// Counts the values a reader that uses doubles would not get back exactly.
    fn count_lossy(&mut self, column: &dyn Array) {
        match column.data_type() {
            ArrowType::Int64 | ArrowType::UInt64 => {
                for row in 0..column.len() {
                    if column.is_valid(row)
                        && int_at(column, row).is_some_and(|v| v.abs() > EXACT_DOUBLE_INT)
                    {
                        self.big_ints.add();
                    }
                }
            }
            ArrowType::Float16 | ArrowType::Float32 | ArrowType::Float64 => {
                for row in 0..column.len() {
                    if column.is_valid(row) && float_at(column, row).is_some_and(|v| !v.is_finite())
                    {
                        self.non_finite.add();
                    }
                }
            }
            _ => {}
        }
    }

    fn write_batch(&mut self, batch: &RecordBatch) -> Result<()> {
        if batch.num_columns() != self.keys.len() {
            return Err(Error::invalid_input(format!(
                "a batch has {} columns, the file {}",
                batch.num_columns(),
                self.keys.len()
            )));
        }
        for column in batch.columns() {
            self.count_lossy(column.as_ref());
        }
        let encoder_options = self.encoder_options.clone();
        let schema = batch.schema();
        let mut encoders = Vec::with_capacity(batch.num_columns());
        for (field, column) in schema.fields().iter().zip(batch.columns()) {
            encoders.push(
                make_encoder(field, column.as_ref(), &encoder_options)
                    .map_err(|e| arrow_error(&e))?,
            );
        }
        let explicit = self.options.explicit_nulls;
        for row in 0..batch.num_rows() {
            let buf = &mut self.row;
            buf.clear();
            buf.push(b'{');
            let mut first = true;
            for ((encoder, column), key) in encoders.iter_mut().zip(batch.columns()).zip(&self.keys)
            {
                let null = null_at(column.as_ref(), row);
                if null && !explicit {
                    continue;
                }
                if !first {
                    buf.push(b',');
                }
                first = false;
                buf.extend_from_slice(key);
                if null {
                    buf.extend_from_slice(b"null");
                } else {
                    encoder.encode(row, buf);
                }
            }
            buf.push(b'}');
            emit_row(
                &mut self.out,
                &mut self.pretty,
                &self.row,
                self.rows,
                &self.options,
            )
            .map_err(|e| io_error(&e))?;
            self.rows += 1;
        }
        Ok(())
    }
}

/// Writes one encoded row in the file's layout.
fn emit_row(
    out: &mut BufWriter<Tally>,
    pretty: &mut Vec<u8>,
    row: &[u8],
    rows_before: u64,
    options: &JsonWriteOptions,
) -> std::io::Result<()> {
    match options.layout {
        JsonLayout::Array => {
            let separator: &[u8] = if rows_before == 0 { b"\n" } else { b",\n" };
            out.write_all(separator)?;
            if options.pretty {
                pretty.clear();
                pretty.extend_from_slice(b"  ");
                push_pretty_json(row, 1, pretty);
                out.write_all(pretty)
            } else {
                out.write_all(row)
            }
        }
        JsonLayout::Lines | JsonLayout::Auto => {
            out.write_all(row)?;
            out.write_all(b"\n")
        }
    }
}

impl SinkWriter for JsonSink {
    fn write(&mut self, batch: &RecordBatch) -> Result<()> {
        if self.failed {
            return Err(already_failed());
        }
        let result = self.write_batch(batch);
        self.failed = result.is_err();
        result
    }

    fn rows_written(&self) -> u64 {
        self.rows
    }

    fn bytes_written(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    fn finish(self: Box<Self>) -> Result<SinkReport> {
        let mut this = *self;
        if this.failed {
            return Err(already_failed());
        }
        if this.options.layout == JsonLayout::Array {
            let end: &[u8] = if this.rows == 0 { b"]\n" } else { b"\n]\n" };
            this.out.write_all(end).map_err(|e| io_error(&e))?;
        }
        let tally = this.out.into_inner().map_err(|e| io_error(e.error()))?;
        let bytes = tally.into_file().commit()?;
        let mut warnings = Vec::new();
        this.big_ints.note(&mut warnings, |n| {
            format!(
                "{n} integer{} beyond ±2^53 were written exactly; programs that read JSON \
                 numbers as doubles round them",
                plural(n)
            )
        });
        this.non_finite.note(&mut warnings, |n| {
            format!(
                "{n} NaN or infinite float{} were written as null",
                plural(n)
            )
        });
        Ok(SinkReport {
            rows: this.rows,
            bytes,
            sheets: Vec::new(),
            warnings,
        })
    }

    fn abort(self: Box<Self>) {
        // Drop the buffered bytes instead of flushing them into a file about to be deleted.
        let this = *self;
        let (tally, _) = this.out.into_parts();
        tally.into_file().abort();
    }
}
