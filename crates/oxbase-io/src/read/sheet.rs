//! XLSX reading (rules: module docs of [`crate::xlsx_file`]).
//!
//! The workbook is first checked by [`super::xlsx_guard`] (size caps against zip bombs and
//! oversized cells). `calamine`'s streaming cell reader borrows the workbook, so it runs on a
//! worker thread that owns both and sends rows in chunks over a bounded channel (at most
//! [`CHANNEL_CHUNKS`] chunks of [`CHUNK_ROWS`] rows ahead of the consumer); dropping the
//! reader stops the thread. Rows come in sheet order with missing rows filled in as empty
//! rows. Progress is the position in the worksheet's compressed data, mapped onto the file
//! size.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use calamine::{DataRef, Reader, Xlsx};
use chrono::{NaiveDateTime, NaiveTime};
use oxbase_driver::CancellationToken;
use oxbase_types::{DataType, Error, Result};

use super::column::{Cell, ColumnBuilder, integral_f64, iso_datetime};
use super::xlsx_guard::{LIMITS, SheetLocation, check};
use super::{
    Progress, check_cancel, display, generated_names, invalid, io_error, quote, unique_names,
};
use crate::infer::TypeInference;
use crate::{
    ColumnSpec, Detected, PreviewColumn, PreviewLimits, RowIssue, SourceBatch, SourcePreview,
    SourceReader, ValueFormats, XlsxReadOptions,
};

/// Rows per message from the worker.
const CHUNK_ROWS: usize = 256;

/// Messages buffered between the worker and the reader.
const CHANNEL_CHUNKS: usize = 4;

/// The format's limits (rows and columns per sheet).
const MAX_ROWS: u32 = 1_048_576;
const MAX_COLUMNS: u32 = 16_384;

/// One cell, owned (it crosses threads).
#[derive(Debug, Clone, PartialEq)]
enum XCell {
    Empty,
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    DateTime(NaiveDateTime),
    Error(String),
}

impl XCell {
    fn from_data(value: &DataRef<'_>) -> Self {
        match value {
            DataRef::Int(v) => Self::Int(*v),
            DataRef::Float(v) => Self::Float(*v),
            DataRef::String(s) | DataRef::DateTimeIso(s) | DataRef::DurationIso(s) => {
                Self::Str(s.clone())
            }
            DataRef::SharedString(s) => Self::Str((*s).to_owned()),
            DataRef::Bool(v) => Self::Bool(*v),
            DataRef::DateTime(d) => match d.as_datetime() {
                Some(v) if d.is_datetime() => Self::DateTime(v),
                _ => Self::Float(d.as_f64()),
            },
            DataRef::Error(e) => Self::Error(e.to_string()),
            DataRef::Empty => Self::Empty,
        }
    }

    fn cell(&self) -> Cell<'_> {
        match self {
            Self::Empty => Cell::Null,
            Self::Int(v) => Cell::Int(*v),
            Self::Float(v) => Cell::Float(*v),
            Self::Str(s) => Cell::Text(s),
            Self::Bool(v) => Cell::Bool(*v),
            Self::DateTime(v) => Cell::DateTime(*v),
            Self::Error(e) => Cell::Error(e),
        }
    }

    fn display(&self) -> Option<String> {
        match self {
            Self::Empty => None,
            Self::Str(s) | Self::Error(s) => Some(display(s)),
            Self::DateTime(v) => Some(iso_datetime(*v)),
            other => Some(super::column::to_text(other.cell())),
        }
    }

    fn observe(&self, inference: &mut TypeInference, column: usize) {
        match self {
            Self::Empty | Self::Error(_) => inference.observe(column, None),
            Self::Int(v) => inference.observe_int(column, *v),
            Self::Float(f) => match integral_f64(*f) {
                Some(v) => inference.observe_int(column, v),
                None => inference.observe_typed(column, &DataType::FLOAT64),
            },
            Self::Str(s) => inference.observe(column, Some(s)),
            Self::Bool(_) => inference.observe_typed(column, &DataType::Bool),
            Self::DateTime(v) => inference.observe_typed(
                column,
                &if v.time() == NaiveTime::MIN {
                    DataType::Date
                } else {
                    DataType::Timestamp { precision: Some(6) }
                },
            ),
        }
    }
}

type XRow = Vec<XCell>;

enum Msg {
    Sheets { names: Vec<String>, sheet: String },
    Rows(Vec<(u32, XRow)>),
    Failed(Error),
}

/// The file under the workbook reader: publishes its position for progress.
struct PosFile {
    file: File,
    offset: u64,
    position: Arc<AtomicU64>,
}

impl Read for PosFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buf)?;
        self.offset = self
            .offset
            .saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        self.position.store(self.offset, Ordering::Relaxed);
        Ok(n)
    }
}

impl Seek for PosFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.offset = self.file.seek(pos)?;
        self.position.store(self.offset, Ordering::Relaxed);
        Ok(self.offset)
    }
}

fn place(row: &mut XRow, column: u32, value: XCell) {
    let i = usize::try_from(column).unwrap_or(0);
    if row.len() <= i {
        row.resize(i + 1, XCell::Empty);
    }
    row[i] = value;
}

fn run_worker(
    path: &Path,
    sheet: Option<&str>,
    position: &Arc<AtomicU64>,
    tx: &SyncSender<Msg>,
) -> Result<()> {
    let file = File::open(path).map_err(|e| io_error(path, &e))?;
    let reader = BufReader::with_capacity(
        64 * 1024,
        PosFile {
            file,
            offset: 0,
            position: Arc::clone(position),
        },
    );
    let mut workbook: Xlsx<_> = Xlsx::new(reader)
        .map_err(|e| invalid(path, format!("is not a readable XLSX workbook: {e}")))?;
    let names = workbook.sheet_names();
    let chosen = match sheet {
        Some(wanted) => names
            .iter()
            .find(|n| n.as_str() == wanted)
            .cloned()
            .ok_or_else(|| {
                invalid(
                    path,
                    format!(
                        "has no sheet named {} (sheets: {})",
                        quote(wanted),
                        names.join(", ")
                    ),
                )
            })?,
        None => names
            .first()
            .cloned()
            .ok_or_else(|| invalid(path, "has no sheets"))?,
    };
    let sheet_error =
        |e: calamine::XlsxError| invalid(path, format!("sheet {}: {e}", quote(&chosen)));
    if tx
        .send(Msg::Sheets {
            names,
            sheet: chosen.clone(),
        })
        .is_err()
    {
        return Ok(());
    }
    let mut cells = workbook
        .worksheet_cells_reader(&chosen)
        .map_err(sheet_error)?;
    let mut chunk: Vec<(u32, XRow)> = Vec::new();
    let mut current: Option<(u32, XRow)> = None;
    while let Some(cell) = cells.next_cell().map_err(sheet_error)? {
        let (row, column) = cell.get_position();
        if row >= MAX_ROWS || column >= MAX_COLUMNS {
            return Err(invalid(
                path,
                format!(
                    "sheet {}: a cell lies outside the sheet's {MAX_ROWS} rows and {MAX_COLUMNS} columns",
                    quote(&chosen)
                ),
            ));
        }
        let value = XCell::from_data(cell.get_value());
        if value == XCell::Empty {
            continue;
        }
        match current.as_ref().map(|(r, _)| *r) {
            Some(r) if r == row => {
                if let Some((_, row_cells)) = current.as_mut() {
                    place(row_cells, column, value);
                }
            }
            Some(r) if r > row => {
                return Err(invalid(
                    path,
                    format!(
                        "sheet {}: row {} comes after row {}",
                        quote(&chosen),
                        row + 1,
                        r + 1
                    ),
                ));
            }
            _ => {
                if let Some(done) = current.take() {
                    chunk.push(done);
                    if chunk.len() >= CHUNK_ROWS
                        && tx.send(Msg::Rows(std::mem::take(&mut chunk))).is_err()
                    {
                        return Ok(());
                    }
                }
                let mut row_cells = Vec::new();
                place(&mut row_cells, column, value);
                current = Some((row, row_cells));
            }
        }
    }
    chunk.extend(current);
    if !chunk.is_empty() {
        // The reader may be gone; nothing to do then.
        let _ = tx.send(Msg::Rows(chunk));
    }
    Ok(())
}

/// The rows of one worksheet, read on a worker thread.
struct SheetRows {
    rx: Option<Receiver<Msg>>,
    worker: Option<JoinHandle<()>>,
    pending: VecDeque<(u32, XRow)>,
    next_index: u32,
    position: Arc<AtomicU64>,
    names: Vec<String>,
    sheet: String,
    finished: bool,
}

impl SheetRows {
    fn start(path: &Path, sheet: Option<&str>) -> Result<Self> {
        let (tx, rx) = sync_channel(CHANNEL_CHUNKS);
        let position = Arc::new(AtomicU64::new(0));
        let worker = {
            let worker_path = path.to_path_buf();
            let sheet = sheet.map(str::to_owned);
            let position = Arc::clone(&position);
            std::thread::Builder::new()
                .name("oxbase-xlsx-reader".to_owned())
                .spawn(move || {
                    if let Err(e) = run_worker(&worker_path, sheet.as_deref(), &position, &tx) {
                        let _ = tx.send(Msg::Failed(e));
                    }
                })
                .map_err(|e| io_error(path, &e))?
        };
        let mut rows = Self {
            rx: Some(rx),
            worker: Some(worker),
            pending: VecDeque::new(),
            next_index: 0,
            position,
            names: Vec::new(),
            sheet: String::new(),
            finished: false,
        };
        match rows.recv() {
            Some(Msg::Sheets { names, sheet }) => {
                rows.names = names;
                rows.sheet = sheet;
                Ok(rows)
            }
            Some(Msg::Failed(e)) => Err(e),
            _ => Err(Error::internal("the XLSX reader stopped before reading")),
        }
    }

    fn recv(&self) -> Option<Msg> {
        self.rx.as_ref()?.recv().ok()
    }

    /// The next row (0-based sheet row index, cells); missing rows come as empty rows.
    fn next_row(&mut self) -> Result<Option<(u32, XRow)>> {
        loop {
            if let Some(index) = self.pending.front().map(|(i, _)| *i) {
                if index > self.next_index {
                    let at = self.next_index;
                    self.next_index += 1;
                    return Ok(Some((at, Vec::new())));
                }
                self.next_index = index.saturating_add(1);
                return Ok(self.pending.pop_front());
            }
            if self.finished {
                return Ok(None);
            }
            match self.recv() {
                Some(Msg::Rows(rows)) => self.pending.extend(rows),
                Some(Msg::Failed(e)) => {
                    self.finished = true;
                    return Err(e);
                }
                Some(Msg::Sheets { .. }) => {}
                None => self.finished = true,
            }
        }
    }

    /// File position of the worker's reads.
    fn raw_position(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }
}

impl Drop for SheetRows {
    fn drop(&mut self) {
        // Closing the channel makes the worker's next send fail, so it returns.
        self.rx = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Compressed bytes of the sheet consumed, mapped onto the file size.
fn position(raw: u64, location: Option<SheetLocation>, total: u64) -> u64 {
    match location {
        Some(l) if l.compressed_size > 0 => {
            let consumed = raw.saturating_sub(l.data_start).min(l.compressed_size);
            let scaled = u128::from(consumed) * u128::from(total) / u128::from(l.compressed_size);
            u64::try_from(scaled).unwrap_or(u64::MAX)
        }
        _ => raw,
    }
}

/// `A1`-style reference of a cell.
fn cell_ref(column: usize, row_index: u32) -> String {
    let mut letters = Vec::new();
    let mut n = column + 1;
    while n > 0 {
        let rem = (n - 1) % 26;
        letters.push(char::from(b'A' + u8::try_from(rem).unwrap_or(0)));
        n = (n - 1) / 26;
    }
    letters.reverse();
    format!(
        "{}{}",
        letters.into_iter().collect::<String>(),
        u64::from(row_index) + 1
    )
}

fn file_size(path: &Path) -> Result<u64> {
    std::fs::metadata(path)
        .as_ref()
        .map(std::fs::Metadata::len)
        .map_err(|e| io_error(path, e))
}

/// Samples a worksheet (see [`crate::preview`]).
pub(crate) fn preview(
    path: &Path,
    o: &XlsxReadOptions,
    limits: PreviewLimits,
    cancel: &CancellationToken,
) -> Result<SourcePreview> {
    let total_bytes = file_size(path)?;
    let location = check(path, o.sheet.as_deref(), &LIMITS)?;
    check_cancel(cancel)?;
    let mut sheet = SheetRows::start(path, o.sheet.as_deref())?;
    let first_data = o.header_row;
    let mut header: Vec<String> = Vec::new();
    let mut inference = TypeInference::new(0, o.formats.clone());
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    let mut lengths: Vec<usize> = Vec::new();
    let mut complete = false;
    loop {
        let consumed = position(sheet.raw_position(), location, total_bytes);
        if lengths.len() >= limits.sample_rows
            || (!lengths.is_empty() && location.is_some() && consumed > limits.sample_bytes)
        {
            break;
        }
        check_cancel(cancel)?;
        let Some((index, cells)) = sheet.next_row()? else {
            complete = true;
            break;
        };
        if index < first_data {
            if index + 1 == first_data {
                header = cells
                    .iter()
                    .map(|c| c.display().unwrap_or_default())
                    .collect();
            }
            continue;
        }
        for (i, cell) in cells.iter().enumerate() {
            cell.observe(&mut inference, i);
        }
        if rows.len() < limits.display_rows {
            rows.push(cells.iter().map(XCell::display).collect());
        }
        lengths.push(cells.len());
    }
    let widest = lengths.iter().copied().max().unwrap_or(0);
    let n = widest.max(header.len());
    // Rows shorter than a column lacked it: NULL.
    for column in 0..n {
        let missing = lengths.iter().filter(|&&len| len <= column).count();
        inference.observe_nulls(column, u64::try_from(missing).unwrap_or(0));
    }
    let names = if o.header_row == 0 {
        generated_names(n)
    } else {
        header.resize(n, String::new());
        unique_names(header)
    };
    for row in &mut rows {
        row.resize(n, None);
    }
    let inferred = inference.finish(complete);
    let columns: Vec<PreviewColumn> = names
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let column = inferred.get(index);
            PreviewColumn {
                index,
                name,
                data_type: column.map_or(DataType::Text, |c| c.data_type.clone()),
                nullable: column.is_none_or(|c| c.nullable),
                warnings: column.map_or_else(Vec::new, |c| c.warnings.clone()),
            }
        })
        .collect();
    let mut warnings = Vec::new();
    if columns.is_empty() {
        warnings.push(format!("the sheet {} is empty", quote(&sheet.sheet)));
    }
    Ok(SourcePreview {
        columns,
        rows,
        sampled_rows: lengths.len(),
        complete,
        total_bytes,
        detected: Detected {
            sheets: sheet.names.clone(),
            sheet: Some(sheet.sheet.clone()),
            ..Detected::default()
        },
        warnings,
    })
}

/// Opens a worksheet for reading (see [`crate::open`]).
pub(crate) fn open(
    path: &Path,
    o: &XlsxReadOptions,
    columns: &[ColumnSpec],
    batch_rows: usize,
) -> Result<Box<dyn SourceReader>> {
    let total_bytes = file_size(path)?;
    let location = check(path, o.sheet.as_deref(), &LIMITS)?;
    let sheet = SheetRows::start(path, o.sheet.as_deref())?;
    Ok(Box::new(XlsxReader {
        sheet,
        location,
        first_data: o.header_row,
        names: columns.iter().map(|c| c.name.clone()).collect(),
        schema: ColumnSpec::schema(columns),
        builders: columns
            .iter()
            .map(|c| ColumnBuilder::new(&c.data_type, batch_rows))
            .collect(),
        batch_rows,
        formats: o.formats.clone(),
        row: 0,
        progress: Progress::new(total_bytes),
        done: false,
        path: path.to_path_buf(),
    }))
}

struct XlsxReader {
    sheet: SheetRows,
    location: Option<SheetLocation>,
    /// 0-based index of the first data row (the row after the header).
    first_data: u32,
    names: Vec<String>,
    schema: SchemaRef,
    builders: Vec<ColumnBuilder>,
    batch_rows: usize,
    formats: ValueFormats,
    row: u64,
    progress: Progress,
    done: bool,
    path: PathBuf,
}

impl SourceReader for XlsxReader {
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
            let next = match self.sheet.next_row() {
                Ok(next) => next,
                Err(e) => {
                    self.done = true;
                    return Err(e);
                }
            };
            let Some((index, cells)) = next else {
                self.done = true;
                break;
            };
            if index < self.first_data {
                continue;
            }
            self.row += 1;
            for (i, builder) in self.builders.iter_mut().enumerate() {
                let Some(cell) = cells.get(i) else {
                    builder.append_null();
                    continue;
                };
                if let Err(message) = builder.append(cell.cell(), &self.formats) {
                    builder.append_null();
                    issues.push(RowIssue {
                        row: self.row,
                        batch_row: rows,
                        column: Some(self.names[i].clone()),
                        message: format!("{message} (cell {})", cell_ref(i, index)),
                    });
                }
            }
            if cells.len() > n {
                issues.push(RowIssue {
                    row: self.row,
                    batch_row: rows,
                    column: None,
                    message: format!(
                        "row {} has values in {} columns, expected {n}; the extra cells were dropped",
                        u64::from(index) + 1,
                        cells.len()
                    ),
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
            .map_err(|e| invalid(&self.path, format!("XLSX batch: {e}")))?;
        let bytes_read = if self.done {
            self.progress.finish()
        } else {
            let at = position(
                self.sheet.raw_position(),
                self.location,
                self.progress.total(),
            );
            self.progress.advance(at)
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

    #[test]
    fn cell_references() {
        assert_eq!(cell_ref(0, 0), "A1");
        assert_eq!(cell_ref(25, 9), "Z10");
        assert_eq!(cell_ref(26, 0), "AA1");
        assert_eq!(cell_ref(16_383, 1_048_575), "XFD1048576");
    }

    #[test]
    fn progress_maps_the_sheet_onto_the_file() {
        let location = Some(SheetLocation {
            data_start: 100,
            compressed_size: 50,
        });
        assert_eq!(position(0, location, 1_000), 0);
        assert_eq!(position(125, location, 1_000), 500);
        assert_eq!(position(10_000, location, 1_000), 1_000);
        assert_eq!(position(42, None, 1_000), 42);
    }
}
