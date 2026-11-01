//! The XLSX writer (rules: [`crate::xlsx_file`] module docs, *Writing*).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arrow_array::cast::AsArray;
use arrow_array::types::{Float16Type, Float32Type};
use arrow_array::{Array, RecordBatch};
use arrow_schema::{DataType as ArrowType, Schema};
use chrono::{Datelike, NaiveDate, Timelike};
use oxbase_types::{Error, Result};
use rust_xlsxwriter::{Format, Workbook, Worksheet, XlsxError};

use super::cells::{
    EXACT_DOUBLE_INT, TextForms, bytes_at, date_at, decimal_at, float_at, int_at, is_text, is_utc,
    normalize, normalized_schema, null_at, push_hex, push_uuid, str_at, time_at, timestamp_at,
};
use super::{Count, Tally, already_failed, io_error, plural};
use crate::formula::needs_guard;
use crate::{SinkFile, SinkReport, SinkWriter, XLSX_MAX_ROWS, XlsxWriteOptions};

/// Excel's column limit.
const MAX_COLUMNS: usize = 16_384;
/// Excel's cell text limit, in UTF-16 code units.
const MAX_CELL_UTF16: usize = 32_767;
/// Significant digits a double keeps exactly.
const EXACT_DOUBLE_DIGITS: usize = 15;

fn xlsx_error(e: XlsxError) -> Error {
    match e {
        // The message would name the temporary directory.
        XlsxError::TempFileError(_, io) => Error::Io {
            kind: io.kind().to_string(),
            message: format!("cannot create a temporary file for the workbook: {io}"),
        },
        XlsxError::IoError(io) => io_error(&io),
        other => Error::internal(format!("XLSX: {other}")),
    }
}

/// Formats, options and counters the cell writes need.
struct Cells {
    header: Format,
    header_guarded: Format,
    date: Format,
    timestamp: Format,
    time: Format,
    guarded: Format,
    guard: bool,
    forms: TextForms,
    text: String,
    big_numbers: Count,
    long_decimals: Count,
    cut_texts: Count,
    non_finite: Count,
    out_of_range: Count,
    sub_millisecond: Count,
    zoned: BTreeSet<String>,
}

struct XlsxSink {
    workbook: Workbook,
    /// `None` after `finish` took it.
    file: Option<SinkFile>,
    bytes: Arc<AtomicU64>,
    base_name: String,
    header: bool,
    max_rows: u32,
    columns: Vec<String>,
    sheets: Vec<String>,
    /// Next row (0-based) of the current sheet.
    next_row: u32,
    rows: u64,
    failed: bool,
    cells: Cells,
}

/// The XLSX writer over `file`.
pub(crate) fn writer(
    file: SinkFile,
    schema: &Schema,
    options: &XlsxWriteOptions,
) -> Result<Box<dyn SinkWriter>> {
    let schema = normalized_schema(schema);
    if schema.fields().len() > MAX_COLUMNS {
        return Err(Error::invalid_input(format!(
            "a worksheet holds at most {MAX_COLUMNS} columns, the export has {}",
            schema.fields().len()
        )));
    }
    let header = Format::new().set_bold();
    let mut sink = XlsxSink {
        workbook: Workbook::new(),
        file: Some(file),
        bytes: Arc::new(AtomicU64::new(0)),
        base_name: options.sheet_name.trim().to_owned(),
        header: options.header,
        max_rows: options.max_rows_per_sheet.clamp(2, XLSX_MAX_ROWS),
        columns: schema.fields().iter().map(|f| f.name().clone()).collect(),
        sheets: Vec::new(),
        next_row: 0,
        rows: 0,
        failed: false,
        cells: Cells {
            header_guarded: header.clone().set_quote_prefix(),
            header,
            date: Format::new().set_num_format(&options.date_format),
            timestamp: Format::new().set_num_format(&options.timestamp_format),
            time: Format::new().set_num_format("hh:mm:ss"),
            guarded: Format::new().set_quote_prefix(),
            guard: options.formula_guard,
            forms: TextForms::default(),
            text: String::new(),
            big_numbers: Count::default(),
            long_decimals: Count::default(),
            cut_texts: Count::default(),
            non_finite: Count::default(),
            out_of_range: Count::default(),
            sub_millisecond: Count::default(),
            zoned: BTreeSet::new(),
        },
    };
    sink.add_sheet()?;
    Ok(Box::new(sink))
}

/// `base` for the first sheet, then `base (2)`, `base (3)`, ... cut to Excel's 31 characters.
fn sheet_name(base: &str, n: usize) -> String {
    if n == 1 {
        return base.to_owned();
    }
    let suffix = format!(" ({n})");
    let room = 31_usize.saturating_sub(suffix.chars().count());
    let mut name: String = base.chars().take(room).collect();
    name.push_str(&suffix);
    name
}

/// `text` cut to Excel's cell limit (never inside a surrogate pair); `true` when it was cut.
fn fit_cell(text: &mut String) -> bool {
    if text.len() <= MAX_CELL_UTF16 {
        // At most one UTF-16 unit per byte.
        return false;
    }
    let mut units = 0;
    for (at, c) in text.char_indices() {
        units += c.len_utf16();
        if units > MAX_CELL_UTF16 {
            text.truncate(at);
            return true;
        }
    }
    false
}

/// Significant digits of a decimal text (`-0012.3400` → 6: leading zeros do not count).
fn significant_digits(text: &str) -> usize {
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    digits.trim_start_matches('0').len()
}

fn excel_date_range(date: NaiveDate) -> bool {
    (1900..=9999).contains(&date.year())
}

/// A typed cell write, with the worksheet borrow released.
fn done(
    result: std::result::Result<&mut Worksheet, XlsxError>,
) -> std::result::Result<(), XlsxError> {
    result.map(|_| ())
}

#[allow(clippy::cast_precision_loss)] // only called with |v| <= 2^53, which doubles hold exactly
fn exact_f64(v: i128) -> f64 {
    v as f64
}

/// A finite float cell as the double whose shortest text is the cell's own: a `Float32` /
/// `Float16` `0.1` is written as `0.1`, not as the widened `0.10000000149011612`.
fn float_cell(array: &dyn Array, index: usize) -> Option<f64> {
    let single = match array.data_type() {
        ArrowType::Float32 => array.as_primitive::<Float32Type>().value(index),
        ArrowType::Float16 => array.as_primitive::<Float16Type>().value(index).to_f32(),
        _ => return float_at(array, index),
    };
    format!("{single:?}").parse().ok()
}

/// `true` when `nanos` has digits below a millisecond (Excel keeps milliseconds).
fn below_millisecond(nanos: u32) -> bool {
    nanos % 1_000_000 != 0
}

impl Cells {
    /// Writes `self.text` as a string cell; `number` marks the text of a number (an integer
    /// beyond 2^53, a long decimal, `-Infinity`), which is never formula-guarded.
    fn write_text(&mut self, ws: &mut Worksheet, row: u32, col: u16, number: bool) -> Result<()> {
        if fit_cell(&mut self.text) {
            self.cut_texts.add();
        }
        let text = std::mem::take(&mut self.text);
        let result = if self.guard && !number && needs_guard(&text) {
            ws.write_string_with_format(row, col, text, &self.guarded)
        } else {
            ws.write_string(row, col, text)
        };
        result.map(|_| ()).map_err(xlsx_error)
    }

    /// Writes the non-NULL cell `index` of `array` at (`row`, `col`).
    fn write(
        &mut self,
        ws: &mut Worksheet,
        row: u32,
        col: u16,
        array: &dyn Array,
        index: usize,
    ) -> Result<()> {
        self.text.clear();
        let t = array.data_type();
        let number = t.is_numeric();
        let written = match t {
            ArrowType::Null => return Ok(()),
            ArrowType::Boolean => Some(done(ws.write_boolean(
                row,
                col,
                array.as_boolean().value(index),
            ))),
            _ if int_at(array, index).is_some() => {
                let v = int_at(array, index).unwrap_or_default();
                if v.abs() <= EXACT_DOUBLE_INT {
                    Some(done(ws.write_number(row, col, exact_f64(v))))
                } else {
                    self.big_numbers.add();
                    self.text = v.to_string();
                    None
                }
            }
            _ if float_at(array, index).is_some() => {
                let v = float_cell(array, index).unwrap_or_default();
                if v.is_finite() {
                    Some(done(ws.write_number(row, col, v)))
                } else {
                    self.non_finite.add();
                    self.forms.push_text(array, index, &mut self.text)?;
                    None
                }
            }
            ArrowType::Decimal32(_, _)
            | ArrowType::Decimal64(_, _)
            | ArrowType::Decimal128(_, _)
            | ArrowType::Decimal256(_, _) => {
                let text = decimal_at(array, index).unwrap_or_default();
                match text.parse::<f64>() {
                    Ok(v) if significant_digits(&text) <= EXACT_DOUBLE_DIGITS => {
                        Some(done(ws.write_number(row, col, v)))
                    }
                    _ => {
                        self.long_decimals.add();
                        self.text = text;
                        None
                    }
                }
            }
            ArrowType::Date32 | ArrowType::Date64 => match date_at(array, index) {
                Some(d) if excel_date_range(d) => {
                    Some(done(ws.write_date_with_format(row, col, d, &self.date)))
                }
                _ => {
                    self.out_of_range.add();
                    self.forms.push_text(array, index, &mut self.text)?;
                    None
                }
            },
            ArrowType::Timestamp(_, _) => match timestamp_at(array, index) {
                Some(v) if excel_date_range(v.date()) => {
                    if below_millisecond(v.nanosecond()) {
                        self.sub_millisecond.add();
                    }
                    Some(done(ws.write_datetime_with_format(
                        row,
                        col,
                        v,
                        &self.timestamp,
                    )))
                }
                _ => {
                    self.out_of_range.add();
                    self.forms.push_text(array, index, &mut self.text)?;
                    None
                }
            },
            ArrowType::Time32(_) | ArrowType::Time64(_) => {
                if let Some(v) = time_at(array, index) {
                    if below_millisecond(v.nanosecond()) {
                        self.sub_millisecond.add();
                    }
                    Some(done(ws.write_time_with_format(row, col, v, &self.time)))
                } else {
                    self.forms.push_text(array, index, &mut self.text)?;
                    None
                }
            }
            ArrowType::FixedSizeBinary(16) => {
                push_uuid(bytes_at(array, index).unwrap_or_default(), &mut self.text);
                None
            }
            ArrowType::Binary
            | ArrowType::LargeBinary
            | ArrowType::BinaryView
            | ArrowType::FixedSizeBinary(_) => {
                self.text.push_str("0x");
                push_hex(bytes_at(array, index).unwrap_or_default(), &mut self.text);
                None
            }
            t if is_text(t) => {
                self.text.push_str(str_at(array, index).unwrap_or_default());
                None
            }
            _ => {
                self.forms.push_text(array, index, &mut self.text)?;
                None
            }
        };
        match written {
            Some(result) => result.map_err(xlsx_error),
            None => self.write_text(ws, row, col, number),
        }
    }
}

impl XlsxSink {
    /// Starts the next sheet (constant-memory mode) and writes its header row.
    fn add_sheet(&mut self) -> Result<()> {
        let name = sheet_name(&self.base_name, self.sheets.len() + 1);
        let ws = self
            .workbook
            .add_worksheet_with_constant_memory()
            .map_err(xlsx_error)?;
        ws.set_name(&name).map_err(xlsx_error)?;
        self.next_row = 0;
        if self.header {
            for (col, title) in self.columns.iter().enumerate() {
                let col = u16::try_from(col).map_err(|_| Error::internal("too many columns"))?;
                let format = if self.cells.guard && needs_guard(title) {
                    &self.cells.header_guarded
                } else {
                    &self.cells.header
                };
                ws.write_string_with_format(0, col, title, format)
                    .map_err(xlsx_error)?;
            }
            self.next_row = 1;
        }
        self.sheets.push(name);
        Ok(())
    }

    fn write_batch(&mut self, batch: &RecordBatch) -> Result<()> {
        let batch = normalize(batch)?;
        if batch.num_columns() != self.columns.len() {
            return Err(Error::invalid_input(format!(
                "a batch has {} columns, the file {}",
                batch.num_columns(),
                self.columns.len()
            )));
        }
        for field in batch.schema().fields() {
            if let ArrowType::Timestamp(_, Some(zone)) = field.data_type()
                && !is_utc(zone)
            {
                self.cells.zoned.insert(field.name().clone());
            }
        }
        for index in 0..batch.num_rows() {
            if self.next_row >= self.max_rows {
                self.add_sheet()?;
            }
            let sheet = self.sheets.len() - 1;
            let ws = self
                .workbook
                .worksheet_from_index(sheet)
                .map_err(xlsx_error)?;
            for (col, array) in batch.columns().iter().enumerate() {
                if null_at(array.as_ref(), index) {
                    continue;
                }
                let col = u16::try_from(col).map_err(|_| Error::internal("too many columns"))?;
                self.cells
                    .write(ws, self.next_row, col, array.as_ref(), index)?;
            }
            self.next_row += 1;
            self.rows += 1;
        }
        Ok(())
    }
}

impl SinkWriter for XlsxSink {
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
        let file = this.file.take().ok_or_else(already_failed)?;
        let mut tally = Tally::new(file);
        this.workbook
            .save_to_writer(&mut tally)
            .map_err(xlsx_error)?;
        let bytes = tally.into_file().commit()?;
        this.bytes.store(bytes, Ordering::Relaxed);

        let c = &this.cells;
        let mut warnings = Vec::new();
        c.big_numbers.note(&mut warnings, |n| {
            format!(
                "{n} integer{} beyond ±2^53 were written as text so no digit is lost",
                plural(n)
            )
        });
        c.long_decimals.note(&mut warnings, |n| {
            format!(
                "{n} decimal{} with more than {EXACT_DOUBLE_DIGITS} significant digits were \
                 written as text so no digit is lost",
                plural(n)
            )
        });
        c.non_finite.note(&mut warnings, |n| {
            format!(
                "{n} NaN or infinite float{} were written as text",
                plural(n)
            )
        });
        c.out_of_range.note(&mut warnings, |n| {
            format!(
                "{n} date{} outside the years 1900-9999 were written as text",
                plural(n)
            )
        });
        c.sub_millisecond.note(&mut warnings, |n| {
            format!(
                "{n} time or timestamp value{} had digits below a millisecond, which Excel \
                 does not keep; they were dropped",
                plural(n)
            )
        });
        c.cut_texts.note(&mut warnings, |n| {
            format!(
                "{n} text{} longer than {MAX_CELL_UTF16} characters were cut",
                plural(n)
            )
        });
        for column in &c.zoned {
            warnings.push(format!(
                "column {column}: timestamps with a time zone were written in UTC"
            ));
        }
        Ok(SinkReport {
            rows: this.rows,
            bytes,
            sheets: this.sheets,
            warnings,
        })
    }

    fn abort(self: Box<Self>) {
        // Dropping the workbook deletes its constant-memory temporary files; dropping the
        // `SinkFile` deletes the partial export.
        drop(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(sheet_name("Data", 1), "Data");
        assert_eq!(sheet_name("Data", 2), "Data (2)");
        let long = "x".repeat(31);
        assert_eq!(sheet_name(&long, 12).chars().count(), 31);
        assert!(sheet_name(&long, 12).ends_with(" (12)"));

        assert_eq!(significant_digits("-0012.3400"), 6);
        assert_eq!(significant_digits("0.000123"), 3);
        assert_eq!(significant_digits("123456789012345.6"), 16);

        let mut text = "a".repeat(MAX_CELL_UTF16 - 1) + "😀";
        assert!(fit_cell(&mut text));
        assert_eq!(text.len(), MAX_CELL_UTF16 - 1);
        let mut short = "😀".repeat(10);
        assert!(!fit_cell(&mut short));
    }
}
