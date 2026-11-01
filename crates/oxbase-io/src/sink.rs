//! Writing files: [`SinkWriter`], the atomic [`SinkFile`] and the async [`WriterHandle`].

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use oxbase_dialect::DialectRegistry;
use oxbase_types::{Error, Result, TableDef};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::{WriteOptions, file_label};

/// What a finished writer reports.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SinkReport {
    pub rows: u64,
    /// Size of the finished file.
    pub bytes: u64,
    /// XLSX: the sheets written (more than one when rows overflowed a sheet).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sheets: Vec<String>,
    /// Values written in a lossy way ("12 timestamps with a time zone were written in UTC",
    /// "3 texts longer than 32767 characters were cut", ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// What a writer may need besides the batches.
#[derive(Clone, Default)]
pub struct WriteContext {
    /// The dialect cells (SQL dump: the target's `DdlGenerator` and `Quoting`).
    pub dialects: Option<Arc<DialectRegistry>>,
    /// The definition of the exported table, when the export is a table (SQL dump `CREATE
    /// TABLE` with its keys and exact types when the target engine is the source engine).
    pub table: Option<TableDef>,
    /// The engine the rows come from.
    pub source_engine: Option<oxbase_types::Engine>,
}

impl std::fmt::Debug for WriteContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteContext")
            .field("dialects", &self.dialects.is_some())
            .field("table", &self.table.as_ref().map(|t| t.obj.display_name()))
            .field("source_engine", &self.source_engine)
            .finish()
    }
}

/// Writes batches to one destination file. Synchronous: run it on a blocking thread
/// ([`spawn_writer`]).
///
/// Every implementation writes into a [`SinkFile`]; nothing appears at the destination until
/// [`SinkWriter::finish`] succeeds, and [`SinkWriter::abort`] (or dropping the writer) deletes
/// the temporary file.
pub trait SinkWriter: Send {
    /// Writes every row of `batch` (in the schema the writer was created with). After an error
    /// the writer is done: call [`SinkWriter::abort`].
    fn write(&mut self, batch: &RecordBatch) -> Result<()>;

    /// Rows written so far.
    fn rows_written(&self) -> u64;

    /// Bytes handed to the file so far (formats that buffer, like Parquet row groups and the
    /// XLSX archive, advance in steps).
    fn bytes_written(&self) -> u64;

    /// Completes the file (footer, archive), syncs it and renames it over the destination.
    fn finish(self: Box<Self>) -> Result<SinkReport>;

    /// Drops the temporary file. The destination is untouched.
    fn abort(self: Box<Self>);
}

/// Creates the writer for `options` at `destination`. Fails before creating anything when the
/// options are invalid, or when the destination exists and `overwrite` is false (the save
/// dialog asks the user, see `docs/m5-contracts.md` §6). Blocking.
pub fn create_writer(
    destination: &Path,
    overwrite: bool,
    schema: SchemaRef,
    options: &WriteOptions,
    context: &WriteContext,
) -> Result<Box<dyn SinkWriter>> {
    options.validate()?;
    let file = SinkFile::create(destination, overwrite)?;
    match options {
        WriteOptions::Csv(o) => crate::csv_file::writer(file, schema, o),
        WriteOptions::Json(o) => crate::json_file::writer(file, schema, o),
        WriteOptions::Xlsx(o) => crate::xlsx_file::writer(file, schema, o),
        WriteOptions::Parquet(o) => crate::parquet_file::writer(file, schema, o),
        WriteOptions::SqlDump(o) => crate::sql_dump::writer(file, schema, o, context),
    }
}

/// A temporary file next to the destination (same directory, so the final rename is atomic on
/// one file system), named `.oxbase-<random>.part`. Counts the bytes written through it.
/// [`SinkFile::commit`] renames it over the destination; dropping it without a commit deletes
/// it.
pub struct SinkFile {
    temp: NamedTempFile,
    destination: PathBuf,
    overwrite: bool,
    written: u64,
}

impl std::fmt::Debug for SinkFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SinkFile")
            .field("destination", &file_label(&self.destination))
            .field("written", &self.written)
            .finish_non_exhaustive()
    }
}

fn io_error(path: &Path, e: &io::Error) -> Error {
    Error::Io {
        kind: e.kind().to_string(),
        message: format!("{}: {e}", file_label(path)),
    }
}

impl SinkFile {
    /// Creates the temporary file. `InvalidInput` when the destination exists and `overwrite`
    /// is false, or names a directory.
    pub fn create(destination: &Path, overwrite: bool) -> Result<Self> {
        if destination.is_dir() {
            return Err(Error::invalid_input(format!(
                "{} is a folder",
                file_label(destination)
            )));
        }
        if !overwrite && destination.exists() {
            return Err(Error::invalid_input(format!(
                "{} already exists",
                file_label(destination)
            )));
        }
        let dir = match destination.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let temp = tempfile::Builder::new()
            .prefix(".oxbase-")
            .suffix(".part")
            .tempfile_in(&dir)
            .map_err(|e| io_error(destination, &e))?;
        Ok(Self {
            temp,
            destination: destination.to_path_buf(),
            overwrite,
            written: 0,
        })
    }

    /// The destination (for messages use [`file_label`]).
    pub fn destination(&self) -> &Path {
        &self.destination
    }

    /// Bytes written so far.
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    /// Flushes, syncs and renames the file over the destination; returns its size. With
    /// `overwrite` false, fails (and deletes the temporary file) when the destination appeared
    /// meanwhile.
    pub fn commit(mut self) -> Result<u64> {
        let destination = self.destination.clone();
        self.temp
            .as_file_mut()
            .flush()
            .and_then(|()| self.temp.as_file().sync_all())
            .map_err(|e| io_error(&destination, &e))?;
        let size = self
            .temp
            .as_file()
            .metadata()
            .map_err(|e| io_error(&destination, &e))?
            .len();
        let persisted = if self.overwrite {
            self.temp.persist(&destination)
        } else {
            self.temp.persist_noclobber(&destination)
        };
        persisted.map_err(|e| io_error(&destination, &e.error))?;
        Ok(size)
    }

    /// Deletes the temporary file (same as dropping it).
    pub fn abort(self) {
        drop(self);
    }

    /// The underlying file, for writers that need `Seek` (rust_xlsxwriter's
    /// `save_to_writer`). Bytes written through it are not counted.
    pub fn file_mut(&mut self) -> &mut File {
        self.temp.as_file_mut()
    }
}

impl Write for SinkFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.temp.as_file_mut().write(buf)?;
        self.written += u64::try_from(n).unwrap_or(u64::MAX);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.temp.as_file_mut().flush()
    }
}

impl Seek for SinkFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.temp.as_file_mut().seek(pos)
    }
}

enum Command {
    Write(RecordBatch),
    Finish,
    Abort,
}

/// A [`SinkWriter`] running on a blocking thread ([`spawn_writer`]). Batches queue in a bounded
/// channel, so a slow disk slows the producer instead of growing memory.
pub struct WriterHandle {
    tx: mpsc::Sender<Command>,
    /// `None` once the writer task was awaited (after a failed `write`).
    join: Option<JoinHandle<Result<Option<SinkReport>>>>,
    /// The error the writer stopped with, once known.
    failed: Option<Error>,
    rows: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
}

/// Runs `writer` on a blocking thread with room for `capacity` (at least 1) queued batches.
/// Needs a tokio runtime.
pub fn spawn_writer(mut writer: Box<dyn SinkWriter>, capacity: usize) -> WriterHandle {
    let (tx, mut rx) = mpsc::channel(capacity.max(1));
    let rows = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let (rows_seen, bytes_seen) = (Arc::clone(&rows), Arc::clone(&bytes));
    let join = tokio::task::spawn_blocking(move || {
        while let Some(command) = rx.blocking_recv() {
            match command {
                Command::Write(batch) => {
                    if let Err(e) = writer.write(&batch) {
                        writer.abort();
                        return Err(e);
                    }
                    rows_seen.store(writer.rows_written(), Ordering::Relaxed);
                    bytes_seen.store(writer.bytes_written(), Ordering::Relaxed);
                }
                Command::Finish => return writer.finish().map(Some),
                Command::Abort => break,
            }
        }
        // Aborted, or the handle was dropped without finishing.
        writer.abort();
        Ok(None)
    });
    WriterHandle {
        tx,
        join: Some(join),
        failed: None,
        rows,
        bytes,
    }
}

fn stopped_early() -> Error {
    Error::internal("the file writer stopped early")
}

async fn outcome(join: JoinHandle<Result<Option<SinkReport>>>) -> Result<Option<SinkReport>> {
    join.await
        .unwrap_or_else(|_| Err(Error::internal("the file writer stopped unexpectedly")))
}

impl WriterHandle {
    /// Queues `batch`. When the writer already failed, returns its error (the writer has
    /// deleted its temporary file); later calls return the same error.
    pub async fn write(&mut self, batch: RecordBatch) -> Result<()> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        if self.tx.send(Command::Write(batch)).await.is_ok() {
            return Ok(());
        }
        let error = match self.join.take() {
            Some(join) => match outcome(join).await {
                Err(e) => e,
                Ok(_) => stopped_early(),
            },
            None => stopped_early(),
        };
        self.failed = Some(error.clone());
        Err(error)
    }

    /// Rows written so far (lags the queued batches).
    pub fn rows_written(&self) -> u64 {
        self.rows.load(Ordering::Relaxed)
    }

    /// Bytes written so far (lags the queued batches).
    pub fn bytes_written(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Writes the queued batches, finishes the file and renames it into place.
    pub async fn finish(self) -> Result<SinkReport> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let Some(join) = self.join else {
            return Err(stopped_early());
        };
        let _ = self.tx.send(Command::Finish).await;
        match outcome(join).await {
            Ok(Some(report)) => Ok(report),
            Ok(None) => Err(stopped_early()),
            Err(e) => Err(e),
        }
    }

    /// Drops the queued batches and deletes the temporary file; returns once it is gone.
    pub async fn abort(self) {
        let _ = self.tx.send(Command::Abort).await;
        if let Some(join) = self.join {
            let _ = join.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use arrow_array::{ArrayRef, Int64Array};
    use arrow_schema::{DataType, Field, Schema};

    /// Writes one line per row into its `SinkFile`.
    struct Lines {
        file: SinkFile,
        rows: u64,
        fail_at: Option<u64>,
    }

    impl SinkWriter for Lines {
        fn write(&mut self, batch: &RecordBatch) -> Result<()> {
            for _ in 0..batch.num_rows() {
                if Some(self.rows) == self.fail_at {
                    return Err(Error::invalid_input("boom"));
                }
                self.file.write_all(b"row\n")?;
                self.rows += 1;
            }
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

    fn batch(rows: i64) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, false)]));
        let column: ArrayRef = Arc::new(Int64Array::from_iter_values(0..rows));
        RecordBatch::try_new(schema, vec![column]).unwrap()
    }

    fn parts(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".part"))
            .collect()
    }

    #[test]
    fn commit_renames_and_protects_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.csv");
        let mut file = SinkFile::create(&dest, false).unwrap();
        file.write_all(b"a,b\n").unwrap();
        assert_eq!(file.bytes_written(), 4);
        assert!(!dest.exists());
        assert_eq!(parts(dir.path()).len(), 1);
        assert_eq!(file.commit().unwrap(), 4);
        assert_eq!(std::fs::read(&dest).unwrap(), b"a,b\n");
        assert!(parts(dir.path()).is_empty());

        // Exists and no overwrite: refused before anything is created.
        let err = SinkFile::create(&dest, false).unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);
        assert!(!err.to_string().contains(&*dir.path().to_string_lossy()));

        // Overwrite: replaced only on commit; an abort leaves the old file.
        let mut file = SinkFile::create(&dest, true).unwrap();
        file.write_all(b"new").unwrap();
        file.abort();
        assert_eq!(std::fs::read(&dest).unwrap(), b"a,b\n");
        assert!(parts(dir.path()).is_empty());
        let mut file = SinkFile::create(&dest, true).unwrap();
        file.write_all(b"new").unwrap();
        file.commit().unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");

        assert!(SinkFile::create(dir.path(), true).is_err());
    }

    #[tokio::test]
    async fn handle_finishes_aborts_and_reports_errors() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("rows.txt");

        let writer = Lines {
            file: SinkFile::create(&dest, false).unwrap(),
            rows: 0,
            fail_at: None,
        };
        let mut handle = spawn_writer(Box::new(writer), 1);
        handle.write(batch(2)).await.unwrap();
        handle.write(batch(3)).await.unwrap();
        let report = handle.finish().await.unwrap();
        assert_eq!((report.rows, report.bytes), (5, 20));
        assert_eq!(std::fs::read(&dest).unwrap().len(), 20);

        let other = dir.path().join("aborted.txt");
        let writer = Lines {
            file: SinkFile::create(&other, false).unwrap(),
            rows: 0,
            fail_at: None,
        };
        let mut handle = spawn_writer(Box::new(writer), 1);
        handle.write(batch(2)).await.unwrap();
        handle.abort().await;
        assert!(!other.exists());
        assert!(parts(dir.path()).is_empty());

        let writer = Lines {
            file: SinkFile::create(&other, false).unwrap(),
            rows: 0,
            fail_at: Some(1),
        };
        let mut handle = spawn_writer(Box::new(writer), 1);
        let mut failed = None;
        for _ in 0..4 {
            if let Err(e) = handle.write(batch(2)).await {
                failed = Some(e);
                break;
            }
        }
        let err = match failed {
            Some(e) => e,
            None => handle.finish().await.unwrap_err(),
        };
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);
        assert!(!other.exists());
        assert!(parts(dir.path()).is_empty());
    }
}
