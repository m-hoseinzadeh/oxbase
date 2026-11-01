//! CSV / TSV reading (rules: module docs of [`crate::csv_file`]).
//!
//! The file is decoded to UTF-8 ([`super::text`]), passed through a [`Guard`] (skipped lines,
//! the 64 MiB row cap, the last byte for unclosed-quote detection) and parsed with the `csv`
//! crate (`flexible`, so ragged rows reach the reader).
//!
//! Row ends: `\r\n`, `\n` and a lone `\r` all end a row whatever
//! [`crate::LineEnding`] says (the `csv` crate's CRLF terminator). Telling a `\r\n` row end
//! from a quoted field that ends with `\r` needs the parser's quoting state, which only the
//! CRLF terminator has; the cost is that a lone `\r` inside an *unquoted* field splits the row.
//! Blank lines are skipped. Skipped lines (`skipRows`) end the same way.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use oxbase_driver::CancellationToken;
use oxbase_types::{Error, Result};

use super::column::{Cell, ColumnBuilder};
use super::text::{consumed, encoding_name, open_text};
use super::{
    MAX_ROW_BYTES, Progress, check_cancel, display, generated_names, invalid, io_error,
    unique_names,
};
use crate::encoding::{CountingReader, DecodingReader};
use crate::infer::TypeInference;
use crate::{
    ColumnSpec, CsvReadOptions, Detected, PreviewColumn, PreviewLimits, RowIssue, SourceBatch,
    SourcePreview, SourceReader, ValueFormats,
};

type Decoded = DecodingReader<CountingReader<std::fs::File>>;

/// Between the decoder and the parser: drops the first `skip_lines` lines, fails once one row
/// pulls more than [`MAX_ROW_BYTES`], and remembers the last byte that is not a line end.
struct Guard {
    inner: Decoded,
    skip_lines: u32,
    cr_ends_lines: bool,
    prev_cr: bool,
    since_row: u64,
    limit: u64,
    tripped: bool,
    last_byte: Option<u8>,
}

impl Guard {
    /// How many bytes at the start of `data` belong to skipped lines.
    fn skip(&mut self, data: &[u8]) -> usize {
        let mut i = 0;
        while self.skip_lines > 0 && i < data.len() {
            let b = data[i];
            i += 1;
            let after_cr = self.prev_cr;
            self.prev_cr = b == b'\r';
            let ends = match b {
                b'\n' => !(self.cr_ends_lines && after_cr),
                b'\r' => self.cr_ends_lines,
                _ => false,
            };
            if ends {
                self.skip_lines -= 1;
            }
        }
        // A `\n` right after the `\r` that ended the last skipped line belongs to that line.
        if self.skip_lines == 0 && self.cr_ends_lines && self.prev_cr && data.get(i) == Some(&b'\n')
        {
            i += 1;
            self.prev_cr = false;
        }
        i
    }
}

impl Read for Guard {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = self.inner.read(buf)?;
            if n == 0 {
                return Ok(0);
            }
            let start = if self.skip_lines > 0 {
                self.skip(&buf[..n])
            } else {
                0
            };
            if start == n {
                continue;
            }
            if start > 0 {
                buf.copy_within(start..n, 0);
            }
            let len = n - start;
            if let Some(&b) = buf[..len].iter().rev().find(|&&b| b != b'\n' && b != b'\r') {
                self.last_byte = Some(b);
            }
            self.since_row = self
                .since_row
                .saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
            if self.since_row > self.limit {
                self.tripped = true;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a row is longer than the limit",
                ));
            }
            return Ok(len);
        }
    }
}

fn ascii(c: char) -> u8 {
    u8::try_from(c).unwrap_or(b'?')
}

/// The parser over a file, one record at a time.
struct CsvSource {
    reader: csv::Reader<Guard>,
    record: csv::ByteRecord,
    counter: Arc<AtomicU64>,
    line_offset: u64,
    path: PathBuf,
    detected: Detected,
    total_bytes: u64,
    eof: bool,
}

impl CsvSource {
    fn open(path: &Path, o: &CsvReadOptions) -> Result<Self> {
        let input = open_text(path, o.encoding)?;
        let guard = Guard {
            inner: input.reader,
            skip_lines: o.skip_rows,
            cr_ends_lines: true,
            prev_cr: false,
            since_row: 0,
            limit: MAX_ROW_BYTES,
            tripped: false,
            last_byte: None,
        };
        let reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .delimiter(ascii(o.delimiter))
            .quoting(o.quote.is_some())
            .quote(o.quote.map_or(b'"', ascii))
            .double_quote(true)
            .escape(o.escape.map(ascii))
            .comment(o.comment.map(ascii))
            .trim(if o.trim {
                csv::Trim::Fields
            } else {
                csv::Trim::None
            })
            .terminator(csv::Terminator::CRLF)
            .buffer_capacity(64 * 1024)
            .from_reader(guard);
        Ok(Self {
            reader,
            record: csv::ByteRecord::new(),
            counter: input.counter,
            line_offset: u64::from(o.skip_rows),
            path: path.to_path_buf(),
            detected: Detected {
                encoding: Some(input.encoding),
                bom: input.bom,
                delimiter: Some(o.delimiter),
                ..Detected::default()
            },
            total_bytes: input.total_bytes,
            eof: false,
        })
    }

    /// Reads the next record; `false` at the end.
    fn next_record(&mut self) -> Result<bool> {
        if self.eof {
            return Ok(false);
        }
        self.reader.get_mut().since_row = 0;
        match self.reader.read_byte_record(&mut self.record) {
            Ok(true) => Ok(true),
            Ok(false) => {
                self.eof = true;
                Ok(false)
            }
            Err(e) => {
                self.eof = true;
                Err(self.error(&e))
            }
        }
    }

    fn error(&self, e: &csv::Error) -> Error {
        let line = self.reader.position().line() + self.line_offset;
        if self.reader.get_ref().tripped {
            return invalid(
                &self.path,
                format!(
                    "line {line}: a row is longer than {} MiB (is a closing quote missing?)",
                    MAX_ROW_BYTES / (1024 * 1024)
                ),
            );
        }
        match e.kind() {
            csv::ErrorKind::Io(io) => io_error(&self.path, io),
            _ => invalid(&self.path, format!("line {line}: {e}")),
        }
    }

    fn fields(&self) -> usize {
        self.record.len()
    }

    /// Field `i` of the current record.
    fn field(&self, i: usize) -> &str {
        std::str::from_utf8(self.record.get(i).unwrap_or_default()).unwrap_or("\u{FFFD}")
    }

    /// Line of the current record's start (1-based, skipped lines counted).
    fn line(&self) -> u64 {
        self.record.position().map_or(0, csv::Position::line) + self.line_offset
    }

    fn consumed(&self) -> u64 {
        consumed(&self.counter)
    }

    fn had_replacements(&self) -> bool {
        self.reader.get_ref().inner.had_replacements()
    }

    fn last_byte(&self) -> Option<u8> {
        self.reader.get_ref().last_byte
    }
}

fn is_null(text: &str, tokens: &[String]) -> bool {
    tokens.iter().any(|t| t == text)
}

/// Samples a delimited file (see [`crate::preview`]).
pub(crate) fn preview(
    path: &Path,
    o: &CsvReadOptions,
    limits: PreviewLimits,
    cancel: &CancellationToken,
) -> Result<SourcePreview> {
    let mut src = CsvSource::open(path, o)?;
    check_cancel(cancel)?;
    let header: Option<Vec<String>> = if o.header && src.next_record()? {
        Some((0..src.fields()).map(|i| src.field(i).to_owned()).collect())
    } else {
        None
    };
    let width = header.as_ref().map_or(usize::MAX, Vec::len);
    let mut inference = TypeInference::new(header.as_ref().map_or(0, Vec::len), o.formats.clone());
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    let mut sampled = 0_usize;
    let mut widest = 0_usize;
    let mut ragged = 0_usize;
    // Line of the last record when one of its fields holds a line break.
    let mut last_break = None;
    let mut complete = o.header && header.is_none();
    while !complete {
        if sampled >= limits.sample_rows || (sampled > 0 && src.consumed() > limits.sample_bytes) {
            break;
        }
        check_cancel(cancel)?;
        if !src.next_record()? {
            complete = true;
            break;
        }
        sampled += 1;
        let len = src.fields();
        widest = widest.max(len);
        if header.is_some() && len != width {
            ragged += 1;
        }
        let mut shown = (rows.len() < limits.display_rows).then(|| Vec::with_capacity(len));
        // An unclosed quote runs to the end of the file: always the last field.
        let has_break = len > 0 && src.field(len - 1).contains(['\n', '\r']);
        for i in 0..len.min(width) {
            let text = src.field(i);
            let value = (!is_null(text, &o.null_tokens)).then_some(text);
            inference.observe(i, value);
            if let Some(row) = shown.as_mut() {
                row.push(value.map(display));
            }
        }
        last_break = if has_break { Some(src.line()) } else { None };
        if let Some(row) = shown {
            rows.push(row);
        }
    }

    let n = header.as_ref().map_or(widest, Vec::len);
    let names = match header {
        Some(h) => unique_names(h),
        None => generated_names(n),
    };
    for row in &mut rows {
        row.resize(n, None);
    }
    let inferred = inference.finish(complete);
    let columns = names
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let column = inferred.get(index);
            PreviewColumn {
                index,
                name,
                data_type: column.map_or(oxbase_types::DataType::Text, |c| c.data_type.clone()),
                nullable: column.is_none_or(|c| c.nullable),
                warnings: column.map_or_else(Vec::new, |c| c.warnings.clone()),
            }
        })
        .collect::<Vec<_>>();

    let mut warnings = Vec::new();
    if columns.is_empty() {
        warnings.push("the file is empty".to_owned());
    }
    if src.had_replacements() {
        warnings.push(format!(
            "some bytes are not valid {}; they were replaced with U+FFFD",
            encoding_name(src.detected.encoding.unwrap_or_default())
        ));
    }
    if ragged > 0 {
        let what = if o.allow_ragged {
            "missing fields are read as NULL and extra fields are reported"
        } else {
            "they will be reported as row issues"
        };
        warnings.push(format!(
            "{ragged} of {sampled} rows do not have {n} fields; {what}"
        ));
    }
    if complete
        && let (Some(line), Some(quote)) = (last_break, o.quote)
        && src.last_byte() != Some(ascii(quote))
    {
        warnings.push(format!(
            "line {line}: a quoted field is never closed; it holds the rest of the file"
        ));
    }
    Ok(SourcePreview {
        columns,
        rows,
        sampled_rows: sampled,
        complete,
        total_bytes: src.total_bytes,
        detected: src.detected.clone(),
        warnings,
    })
}

/// Opens a delimited file for reading (see [`crate::open`]).
pub(crate) fn open(
    path: &Path,
    o: &CsvReadOptions,
    columns: &[ColumnSpec],
    batch_rows: usize,
) -> Result<Box<dyn SourceReader>> {
    let mut src = CsvSource::open(path, o)?;
    if o.header && src.next_record()? && src.fields() != columns.len() {
        return Err(invalid(
            path,
            format!(
                "the header has {} columns but {} were given",
                src.fields(),
                columns.len()
            ),
        ));
    }
    let total = src.total_bytes;
    Ok(Box::new(CsvReader {
        builders: columns
            .iter()
            .map(|c| ColumnBuilder::new(&c.data_type, batch_rows))
            .collect(),
        schema: ColumnSpec::schema(columns),
        names: columns.iter().map(|c| c.name.clone()).collect(),
        src,
        batch_rows,
        formats: o.formats.clone(),
        null_tokens: o.null_tokens.clone(),
        allow_ragged: o.allow_ragged,
        header_lines: u64::from(o.header) + u64::from(o.skip_rows),
        row: 0,
        progress: Progress::new(total),
        done: false,
    }))
}

struct CsvReader {
    src: CsvSource,
    names: Vec<String>,
    schema: SchemaRef,
    builders: Vec<ColumnBuilder>,
    batch_rows: usize,
    formats: ValueFormats,
    null_tokens: Vec<String>,
    allow_ragged: bool,
    /// Lines before the first data row (header and skipped lines).
    header_lines: u64,
    /// Data rows read so far.
    row: u64,
    progress: Progress,
    done: bool,
}

impl SourceReader for CsvReader {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn total_bytes(&self) -> u64 {
        self.progress.total()
    }

    fn bytes_read(&self) -> u64 {
        self.progress.position()
    }

    fn next_batch(&mut self) -> Result<Option<SourceBatch>> {
        if self.done {
            return Ok(None);
        }
        let n = self.builders.len();
        let first_row = self.row + 1;
        let mut issues = Vec::new();
        let mut rows = 0_usize;
        while rows < self.batch_rows {
            if !self.src.next_record()? {
                self.done = true;
                break;
            }
            self.row += 1;
            let len = self.src.fields();
            let line = self.src.line();
            let at = if line == self.row + self.header_lines {
                String::new()
            } else {
                format!(" (line {line})")
            };
            for (i, builder) in self.builders.iter_mut().enumerate() {
                if i >= len {
                    builder.append_null();
                    continue;
                }
                let text = self.src.field(i);
                if is_null(text, &self.null_tokens) {
                    builder.append_null();
                } else if let Err(message) = builder.append(Cell::Text(text), &self.formats) {
                    builder.append_null();
                    issues.push(RowIssue {
                        row: self.row,
                        batch_row: rows,
                        column: Some(self.names[i].clone()),
                        message: format!("{message}{at}"),
                    });
                }
            }
            if len > n {
                issues.push(RowIssue {
                    row: self.row,
                    batch_row: rows,
                    column: None,
                    message: format!(
                        "row has {len} fields, expected {n}; the extra fields were dropped{at}"
                    ),
                });
            } else if len < n && !self.allow_ragged {
                issues.push(RowIssue {
                    row: self.row,
                    batch_row: rows,
                    column: None,
                    message: format!("row has {len} fields, expected {n}{at}"),
                });
            }
            rows += 1;
        }
        if rows == 0 {
            self.progress.finish();
            return Ok(None);
        }
        let columns = self
            .builders
            .iter_mut()
            .map(ColumnBuilder::finish)
            .collect();
        let batch = RecordBatch::try_new(self.schema.clone(), columns)
            .map_err(|e| Error::internal(format!("CSV batch: {e}")))?;
        let bytes_read = if self.done {
            self.progress.finish()
        } else {
            self.progress.advance(self.src.consumed())
        };
        Ok(Some(SourceBatch {
            batch,
            first_row,
            issues,
            bytes_read,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(
        dir: &tempfile::TempDir,
        data: &'static [u8],
        skip_lines: u32,
        cr: bool,
        limit: u64,
    ) -> Guard {
        let path = dir.path().join(format!("g{}.csv", data.len()));
        std::fs::write(&path, data).unwrap();
        let input = open_text(&path, crate::TextEncoding::Utf8).unwrap();
        Guard {
            inner: input.reader,
            skip_lines,
            cr_ends_lines: cr,
            prev_cr: false,
            since_row: 0,
            limit,
            tripped: false,
            last_byte: None,
        }
    }

    #[test]
    fn guard_skips_lines_and_caps_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = String::new();
        guard(&dir, b"one\ntwo\r\nthree\nrest", 2, false, 100)
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "three\nrest");

        let mut out = String::new();
        guard(&dir, b"one\rtwo\r\nthree", 2, true, 100)
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "three");

        let mut g = guard(&dir, b"0123456789", 0, false, 5);
        let mut out = Vec::new();
        assert!(g.read_to_end(&mut out).is_err());
        assert!(g.tripped);

        let mut g = guard(&dir, b"a,\"b\"\r\n", 0, false, 100);
        let mut out = Vec::new();
        g.read_to_end(&mut out).unwrap();
        assert_eq!(g.last_byte, Some(b'"'));
    }
}
