//! CSV / TSV reading (rules: module docs of [`crate::csv_file`]).
//!
//! The file is decoded to UTF-8 ([`super::text`]), passed through a [`Guard`] (skipped lines,
//! the 64 MiB row cap, the last byte for unclosed-quote detection) and parsed with the `csv`
//! crate (`flexible`, so ragged rows reach the reader).
//!
//! Row ends ([`crate::LineEnding`]):
//! - `Auto`, `Lf`, `CrLf`: `\n` and `\r\n` end a row; a lone `\r` is data. The parser ends
//!   records at `\n`; when the byte before that `\n` is a `\r` (the [`Guard`] notes where
//!   each `\r\n` is), the `\r` is outside quotes, so it is dropped from the record's last
//!   field. A quoted `\r` (`"a\r"\n`) stays. Line numbers count `\n`.
//! - `Cr`: `\r\n`, `\n` and a lone `\r` end a row (the `csv` crate's CRLF terminator, which
//!   keeps a quoted `\r` intact). Line numbers are not known (the parser counts `\n` only), so
//!   messages name rows (parse errors: records), not lines.
//!
//! Blank lines are skipped. Skipped lines (`skipRows`) end the same way.
//!
//! `trim` strips ASCII white space around every field, quoted fields included (the parser
//! does not report whether a field was quoted).

use std::collections::VecDeque;
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
    ColumnSpec, CsvReadOptions, Detected, LineEnding, PreviewColumn, PreviewLimits, RowIssue,
    SourceBatch, SourcePreview, SourceReader, ValueFormats,
};

type Decoded = DecodingReader<CountingReader<std::fs::File>>;

/// The parser's input buffer.
const PARSER_BUFFER: u64 = 64 * 1024;

/// Between the decoder and the parser: drops the first `skip_lines` lines, fails once one row
/// pulls more than [`MAX_ROW_BYTES`], remembers the last byte that is not a line end and
/// (with `track_crlf`) where the `\n` of each `\r\n` is.
struct Guard {
    inner: Decoded,
    skip_lines: u32,
    cr_ends_lines: bool,
    prev_cr: bool,
    since_row: u64,
    limit: u64,
    tripped: bool,
    last_byte: Option<u8>,
    track_crlf: bool,
    /// Bytes passed to the parser so far.
    emitted: u64,
    /// The last two bytes passed (second to last, last).
    tail: [Option<u8>; 2],
    /// Each `\r\n` passed (those within two parser buffers of the end of what was passed):
    /// the parser offset of its `\n`, and whether the pair is a whole line (it follows a `\n`
    /// or starts the text).
    crlf: VecDeque<(u64, bool)>,
}

/// The byte `back` (1 or 2) positions before `out[k]`, looking into `tail` (the bytes passed
/// before `out`) when needed; `None` before the start of the text.
fn byte_before(out: &[u8], tail: [Option<u8>; 2], k: usize, back: usize) -> Option<u8> {
    match k.checked_sub(back) {
        Some(j) => out.get(j).copied(),
        None => tail.get(2 + k - back).copied().flatten(),
    }
}

impl Guard {
    fn new(inner: Decoded, skip_lines: u32, cr_ends_lines: bool, limit: u64) -> Self {
        Self {
            inner,
            skip_lines,
            cr_ends_lines,
            prev_cr: false,
            since_row: 0,
            limit,
            tripped: false,
            last_byte: None,
            track_crlf: !cr_ends_lines,
            emitted: 0,
            tail: [None, None],
            crlf: VecDeque::new(),
        }
    }

    /// Notes the `\r\n` pairs of `out`, the next bytes passed to the parser.
    fn note_crlf(&mut self, out: &[u8]) {
        let Some(&last) = out.last() else {
            return;
        };
        let mut from = 0;
        while let Some(i) = out[from..].iter().position(|&b| b == b'\n') {
            let at = from + i;
            if byte_before(out, self.tail, at, 1) == Some(b'\r') {
                let whole_line = matches!(byte_before(out, self.tail, at, 2), None | Some(b'\n'));
                let offset = self.emitted + u64::try_from(at).unwrap_or(u64::MAX);
                self.crlf.push_back((offset, whole_line));
            }
            from = at + 1;
        }
        self.tail = match out {
            [.., a, b] => [Some(*a), Some(*b)],
            _ => [self.tail[1], Some(last)],
        };
        self.emitted += u64::try_from(out.len()).unwrap_or(u64::MAX);
        // A record ends at most one parser buffer behind what was passed: older pairs cannot
        // be asked for (this bounds the queue even in a 64 MiB field of `\r\n`s).
        let horizon = self.emitted.saturating_sub(2 * PARSER_BUFFER);
        while self.crlf.front().is_some_and(|&(at, _)| at < horizon) {
            self.crlf.pop_front();
        }
    }

    /// Whether the record the parser just ended at `end` (its offset after the record)
    /// ended with `\r\n`: `Some(true)` when that `\r\n` is a whole (blank) line. Forgets the
    /// pairs before it.
    fn ended_by_crlf(&mut self, end: u64) -> Option<bool> {
        while self.crlf.front().is_some_and(|&(at, _)| at + 1 < end) {
            self.crlf.pop_front();
        }
        self.crlf
            .front()
            .filter(|&&(at, _)| at + 1 == end)
            .map(|&(_, whole_line)| whole_line)
    }

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
            if self.track_crlf {
                self.note_crlf(&buf[..len]);
            }
            return Ok(len);
        }
    }
}

fn ascii(c: char) -> u8 {
    u8::try_from(c).unwrap_or(b'?')
}

/// What [`CsvSource::at_end`] read ahead (the record is in `CsvSource::spare`).
enum Ahead {
    Record,
    End,
    Failed(Error),
}

/// The parser over a file, one record at a time.
struct CsvSource {
    reader: csv::Reader<Guard>,
    record: csv::ByteRecord,
    /// The record read ahead.
    spare: csv::ByteRecord,
    ahead: Option<Ahead>,
    /// Records end at `\n` (not [`LineEnding::Cr`]).
    strip_cr: bool,
    /// The current record ended with `\r\n`: its last field ends with that `\r`.
    cr_end: bool,
    /// The same for `spare`.
    spare_cr_end: bool,
    trim: bool,
    /// Records read (the header included).
    records: u64,
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
        let guard = Guard::new(
            input.reader,
            o.skip_rows,
            o.line_ending == LineEnding::Cr,
            MAX_ROW_BYTES,
        );
        let reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .delimiter(ascii(o.delimiter))
            .quoting(o.quote.is_some())
            .quote(o.quote.map_or(b'"', ascii))
            .double_quote(true)
            .escape(o.escape.map(ascii))
            .comment(o.comment.map(ascii))
            .terminator(if o.line_ending == LineEnding::Cr {
                csv::Terminator::CRLF
            } else {
                csv::Terminator::Any(b'\n')
            })
            .buffer_capacity(usize::try_from(PARSER_BUFFER).unwrap_or(64 * 1024))
            .from_reader(guard);
        Ok(Self {
            reader,
            record: csv::ByteRecord::new(),
            spare: csv::ByteRecord::new(),
            ahead: None,
            strip_cr: o.line_ending != LineEnding::Cr,
            cr_end: false,
            spare_cr_end: false,
            trim: o.trim,
            records: 0,
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
        match self.ahead.take() {
            Some(Ahead::Record) => {
                std::mem::swap(&mut self.record, &mut self.spare);
                std::mem::swap(&mut self.cr_end, &mut self.spare_cr_end);
                Ok(true)
            }
            Some(Ahead::End) => Ok(false),
            Some(Ahead::Failed(e)) => Err(e),
            None => self.read(false),
        }
    }

    /// `true` when no record is left. Reads the next one ahead (the current record stays);
    /// a failure is returned by the next [`CsvSource::next_record`].
    fn at_end(&mut self) -> bool {
        if self.ahead.is_none() {
            self.ahead = Some(match self.read(true) {
                Ok(true) => Ahead::Record,
                Ok(false) => Ahead::End,
                Err(e) => Ahead::Failed(e),
            });
        }
        matches!(self.ahead, Some(Ahead::End))
    }

    /// Parses the next record into `record` (or `spare` when reading ahead).
    fn read(&mut self, ahead: bool) -> Result<bool> {
        loop {
            if self.eof {
                return Ok(false);
            }
            self.reader.get_mut().since_row = 0;
            let record = if ahead {
                &mut self.spare
            } else {
                &mut self.record
            };
            match self.reader.read_byte_record(record) {
                Ok(true) => {}
                Ok(false) => {
                    self.eof = true;
                    return Ok(false);
                }
                Err(e) => {
                    self.eof = true;
                    return Err(self.error(&e));
                }
            }
            let end = self.reader.position().byte();
            let crlf = if self.strip_cr {
                self.reader.get_mut().ended_by_crlf(end)
            } else {
                None
            };
            let cr_end = crlf.is_some();
            let record = if ahead { &self.spare } else { &self.record };
            // A blank `\r\n` line is a record of one field holding `\r` (`""\r\n` is not
            // blank: its `\r\n` follows a quote).
            if crlf == Some(true) && record.len() == 1 {
                continue;
            }
            if ahead {
                self.spare_cr_end = cr_end;
            } else {
                self.cr_end = cr_end;
            }
            self.records += 1;
            return Ok(true);
        }
    }

    fn error(&self, e: &csv::Error) -> Error {
        let at = if self.strip_cr {
            format!("line {}", self.reader.position().line() + self.line_offset)
        } else {
            format!("record {}", self.records + 1)
        };
        if self.reader.get_ref().tripped {
            return invalid(
                &self.path,
                format!(
                    "{at}: a row is longer than {} MiB (is a closing quote missing?)",
                    MAX_ROW_BYTES / (1024 * 1024)
                ),
            );
        }
        match e.kind() {
            csv::ErrorKind::Io(io) => io_error(&self.path, io),
            _ => invalid(&self.path, format!("{at}: {e}")),
        }
    }

    fn fields(&self) -> usize {
        self.record.len()
    }

    /// Field `i` of the current record: the `\r` of a `\r\n` row end removed, trimmed when
    /// asked.
    fn field(&self, i: usize) -> &str {
        let mut bytes = self.record.get(i).unwrap_or_default();
        if self.cr_end && i + 1 == self.record.len() {
            bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        }
        if self.trim {
            bytes = bytes.trim_ascii();
        }
        std::str::from_utf8(bytes).unwrap_or("\u{FFFD}")
    }

    /// Line of the current record's start (1-based, skipped lines counted); `None` when lines
    /// are not counted (`LineEnding::Cr`).
    fn line(&self) -> Option<u64> {
        self.strip_cr
            .then(|| self.record.position().map_or(0, csv::Position::line) + self.line_offset)
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
        // The fields a short row lacks are read as NULL.
        if header.is_some() {
            for i in len..width {
                inference.observe(i, None);
            }
        }
        last_break = has_break.then(|| {
            src.line()
                .map_or_else(|| format!("row {sampled}"), |line| format!("line {line}"))
        });
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
        && let (Some(at), Some(quote)) = (last_break, o.quote)
        && src.last_byte() != Some(ascii(quote))
    {
        warnings.push(format!(
            "{at}: a quoted field is never closed; it holds the rest of the file"
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
            let at = match self.src.line() {
                Some(line) if line != self.row + self.header_lines => format!(" (line {line})"),
                _ => String::new(),
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
        // A full batch looks ahead, so the last batch carries the file size (§12).
        if !self.done && self.src.at_end() {
            self.done = true;
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
        Guard::new(input.reader, skip_lines, cr, limit)
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
        // The `\n` of `\r\n` is at offset 6; a record ending there (end 7) ended with it.
        assert_eq!(g.crlf, [(6, false)]);
        assert_eq!(g.ended_by_crlf(5), None);
        assert_eq!(g.ended_by_crlf(7), Some(false));
        assert_eq!(g.ended_by_crlf(9), None);
        assert_eq!(g.crlf.len(), 0);

        // Pairs split over reads, a lone `\r`, and blank `\r\n` lines (at the start, after
        // a `\n` in an earlier read).
        let mut g = guard(&dir, b"x", 0, false, 100);
        g.note_crlf(b"\r");
        g.note_crlf(b"\na\r");
        g.note_crlf(b"\nb\rc\n");
        g.note_crlf(b"\r");
        g.note_crlf(b"\n");
        assert_eq!(g.crlf, [(1, true), (4, false), (10, true)]);
    }
}
