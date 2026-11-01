//! The delimited-text writer (rules: [`crate::csv_file`] module docs, *Writing*).

use std::collections::BTreeSet;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arrow_array::RecordBatch;
use arrow_schema::{DataType as ArrowType, Schema};
use oxbase_types::{Error, Result};

use super::cells::{TextForms, is_text, is_utc, normalize, normalized_schema};
use super::{Tally, already_failed, io_error};
use crate::encoding::{EncodingWriter, bom_bytes};
use crate::formula::needs_guard;
use crate::{CsvWriteOptions, LineEnding, QuoteStyle, SinkFile, SinkReport, SinkWriter};

type Out = ::csv::Writer<EncodingWriter<Tally>>;

struct CsvSink {
    out: Out,
    bytes: Arc<AtomicU64>,
    forms: TextForms,
    options: CsvWriteOptions,
    /// Reused field texts of one record.
    fields: Vec<String>,
    rows: u64,
    failed: bool,
    /// Columns whose timestamps had a non-UTC zone (written as UTC).
    zoned: BTreeSet<String>,
}

fn ascii(what: &str, c: char) -> Result<u8> {
    u8::try_from(c)
        .ok()
        .filter(u8::is_ascii)
        .ok_or_else(|| Error::invalid_input(format!("the {what} must be an ASCII character")))
}

fn csv_error(e: ::csv::Error) -> Error {
    match e.into_kind() {
        ::csv::ErrorKind::Io(io) => io_error(&io),
        other => Error::internal(format!("CSV: {other:?}")),
    }
}

/// The delimited-text writer over `file`.
pub(crate) fn writer(
    file: SinkFile,
    schema: &Schema,
    options: &CsvWriteOptions,
) -> Result<Box<dyn SinkWriter>> {
    let forms = TextForms::new(&options.formats)?;
    let mut tally = Tally::new(file);
    let bytes = tally.counter();
    if options.bom {
        tally
            .write_all(bom_bytes(options.encoding))
            .map_err(|e| io_error(&e))?;
    }
    let encoded = EncodingWriter::new(tally, options.encoding)?;
    let mut builder = ::csv::WriterBuilder::new();
    builder
        .delimiter(ascii("delimiter", options.delimiter)?)
        .quote(ascii("quote", options.quote)?)
        .quote_style(match options.quote_style {
            QuoteStyle::Necessary => ::csv::QuoteStyle::Necessary,
            QuoteStyle::Always => ::csv::QuoteStyle::Always,
            QuoteStyle::NonNumeric => ::csv::QuoteStyle::NonNumeric,
            QuoteStyle::Never => ::csv::QuoteStyle::Never,
        })
        .terminator(match options.line_ending {
            LineEnding::CrLf => ::csv::Terminator::CRLF,
            _ => ::csv::Terminator::Any(b'\n'),
        })
        .has_headers(false)
        .flexible(false)
        .buffer_capacity(64 * 1024);
    if let Some(escape) = options.escape {
        builder.double_quote(false).escape(ascii("escape", escape)?);
    }
    let schema = normalized_schema(schema);
    let mut sink = CsvSink {
        out: builder.from_writer(encoded),
        bytes,
        forms,
        options: options.clone(),
        fields: vec![String::new(); schema.fields().len()],
        rows: 0,
        failed: false,
        zoned: BTreeSet::new(),
    };
    if options.header {
        for (field, slot) in schema.fields().iter().zip(sink.fields.iter_mut()) {
            slot.clear();
            slot.push_str(field.name());
            if options.formula_guard && needs_guard(slot) {
                slot.insert(0, '\'');
            }
        }
        sink.check_unquoted(0)?;
        sink.out.write_record(&sink.fields).map_err(csv_error)?;
    }
    Ok(Box::new(sink))
}

impl CsvSink {
    /// With [`QuoteStyle::Never`], a field that would need quotes is an error (writing it
    /// unquoted would break the file). `row` is 1-based (0: the header).
    fn check_unquoted(&self, row: u64) -> Result<()> {
        if self.options.quote_style != QuoteStyle::Never {
            return Ok(());
        }
        let delimiter = self.options.delimiter;
        let quote = self.options.quote;
        for (column, field) in self.fields.iter().enumerate() {
            if field.contains([delimiter, quote, '\n', '\r']) {
                let at = if row == 0 {
                    "the header".to_owned()
                } else {
                    format!("row {row}")
                };
                return Err(Error::invalid_input(format!(
                    "{at}, column {}: the value needs quotes, but quoting is off",
                    column + 1
                )));
            }
        }
        Ok(())
    }

    fn write_batch(&mut self, batch: &RecordBatch) -> Result<()> {
        let batch = normalize(batch)?;
        if batch.num_columns() != self.fields.len() {
            return Err(Error::invalid_input(format!(
                "a batch has {} columns, the file {}",
                batch.num_columns(),
                self.fields.len()
            )));
        }
        let schema = batch.schema();
        let guard: Vec<bool> = schema
            .fields()
            .iter()
            .map(|f| self.options.formula_guard && is_text(f.data_type()))
            .collect();
        for field in schema.fields() {
            if let ArrowType::Timestamp(_, Some(zone)) = field.data_type()
                && !is_utc(zone)
            {
                self.zoned.insert(field.name().clone());
            }
        }
        for row in 0..batch.num_rows() {
            for (c, column) in batch.columns().iter().enumerate() {
                let field = &mut self.fields[c];
                field.clear();
                if column.is_null(row) {
                    field.push_str(&self.options.null_token);
                    continue;
                }
                self.forms.push_text(column.as_ref(), row, field)?;
                if guard[c] && needs_guard(field) {
                    field.insert(0, '\'');
                }
            }
            self.check_unquoted(self.rows + 1)?;
            self.out.write_record(&self.fields).map_err(csv_error)?;
            self.rows += 1;
        }
        Ok(())
    }
}

impl SinkWriter for CsvSink {
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
        let this = *self;
        if this.failed {
            return Err(already_failed());
        }
        let encoded = this.out.into_inner().map_err(|e| io_error(e.error()))?;
        let tally = encoded.finish().map_err(|e| io_error(&e))?;
        let bytes = tally.into_file().commit()?;
        let warnings = this
            .zoned
            .iter()
            .map(|c| format!("column {c}: timestamps with a time zone were written in UTC"))
            .collect();
        Ok(SinkReport {
            rows: this.rows,
            bytes,
            sheets: Vec::new(),
            warnings,
        })
    }

    fn abort(self: Box<Self>) {
        drop(self);
    }
}
