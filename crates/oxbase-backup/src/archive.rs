//! Reading and writing archives (`docs/m8-contracts.md` §7.1-§7.2). Synchronous (ZIP and
//! Arrow IPC are blocking I/O): the async backup and restore run them on blocking threads with
//! a bounded channel in between.
//!
//! # Robustness
//!
//! Archives come from disk and may be truncated, corrupted or written by a newer Oxbase.
//! Nothing here panics on such input: ZIP and JSON errors become `InvalidInput`, a newer format
//! version `Unsupported`, and Arrow IPC decoding of entry bytes runs under
//! `std::panic::catch_unwind` so that a malformed stream that passed its checksum (a forged
//! archive) still ends in an error. Entry sizes are capped before anything is read into memory
//! ([`MAX_JSON_ENTRY`]); table data is streamed batch by batch.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Seek, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use arrow_array::RecordBatch;
use arrow_ipc::CompressionType;
use arrow_ipc::reader::StreamReader;
use arrow_ipc::writer::{IpcWriteOptions, StreamWriter};
use arrow_schema::SchemaRef;
use oxbase_types::{Error, ObjectDef, Result};
use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::format::{
    Compression, EntryHasher, EntryInfo, FORMAT_NAME, FORMAT_VERSION, MANIFEST_ENTRY, Manifest,
    data_entry, ddl_entry, definition_entry, sha256_hex,
};

/// The largest manifest, definition or DDL entry a reader loads into memory (256 MiB). Table
/// data is streamed and has no such cap.
pub const MAX_JSON_ENTRY: u64 = 256 * 1024 * 1024;

/// Progress callbacks of [`HashingReader`] fire at most once per this many bytes.
const PROGRESS_STEP: u64 = 1024 * 1024;

fn zip_error(e: &zip::result::ZipError) -> Error {
    Error::invalid_input(format!("the backup archive is damaged: {e}"))
}

fn arrow_error(e: &arrow_schema::ArrowError) -> Error {
    Error::invalid_input(format!("the backup's table data is damaged: {e}"))
}

fn panic_error() -> Error {
    Error::invalid_input("the backup's table data is damaged (it cannot be decoded)")
}

fn poisoned() -> Error {
    Error::internal("the backup archive writer failed earlier")
}

/// A writer that hashes and counts what goes through it.
struct HashingWriter<W: Write> {
    inner: W,
    hasher: EntryHasher,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

enum WriterState<W: Write + Seek> {
    /// Between entries.
    Idle(ZipWriter<W>),
    /// A table's data entry is open.
    Table {
        writer: Box<StreamWriter<HashingWriter<ZipWriter<W>>>>,
        rows: u64,
    },
    /// An earlier call failed; the archive is unusable.
    Poisoned,
}

/// Writes one archive into `W` (an `oxbase_io::SinkFile`, renamed on success). Objects first,
/// then each table's rows, then [`ArchiveWriter::finish`] writes the manifest (the manifest
/// lists the checksums, so it is written last; readers find it through the ZIP directory).
pub struct ArchiveWriter<W: Write + Seek> {
    state: WriterState<W>,
    compression: Compression,
}

impl<W: Write + Seek> ArchiveWriter<W> {
    pub fn new(sink: W, compression: Compression) -> Self {
        Self {
            state: WriterState::Idle(ZipWriter::new(sink)),
            compression,
        }
    }

    fn deflated() -> SimpleFileOptions {
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .large_file(true)
    }

    fn idle(&mut self) -> Result<&mut ZipWriter<W>> {
        match &mut self.state {
            WriterState::Idle(zip) => Ok(zip),
            WriterState::Table { .. } => Err(Error::internal(
                "a table's data entry is still open in the backup archive",
            )),
            WriterState::Poisoned => Err(poisoned()),
        }
    }

    /// Writes one deflated entry; poisons the writer on failure.
    fn put(&mut self, name: &str, bytes: &[u8]) -> Result<EntryInfo> {
        let zip = self.idle()?;
        let written = zip
            .start_file(name, Self::deflated())
            .map_err(|e| zip_error(&e))
            .and_then(|()| zip.write_all(bytes).map_err(Error::from));
        if let Err(e) = written {
            self.state = WriterState::Poisoned;
            return Err(e);
        }
        Ok(EntryInfo {
            size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            sha256: sha256_hex(bytes),
        })
    }

    /// Writes object `index`'s definition JSON and, when given, its DDL text; returns their
    /// entry infos (definition, ddl).
    pub fn add_object(
        &mut self,
        index: u32,
        definition: &ObjectDef,
        ddl: Option<&str>,
    ) -> Result<(EntryInfo, Option<EntryInfo>)> {
        let json = serde_json::to_vec(definition)
            .map_err(|e| Error::internal(format!("cannot encode a definition: {e}")))?;
        let def_info = self.put(&definition_entry(index), &json)?;
        let ddl_info = match ddl {
            Some(text) => Some(self.put(&ddl_entry(index), text.as_bytes())?),
            None => None,
        };
        Ok((def_info, ddl_info))
    }

    /// Starts table `index`'s data entry with `schema` (the load schema); batches follow with
    /// [`ArchiveWriter::write_rows`], then [`ArchiveWriter::end_table`].
    pub fn begin_table(&mut self, index: u32, schema: &SchemaRef) -> Result<()> {
        self.idle()?;
        let WriterState::Idle(mut zip) = std::mem::replace(&mut self.state, WriterState::Poisoned)
        else {
            return Err(poisoned());
        };
        // Arrow buffers are compressed already (or the user chose none): stored.
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(true);
        zip.start_file(data_entry(index), options)
            .map_err(|e| zip_error(&e))?;
        let codec = match self.compression {
            Compression::None => None,
            Compression::Zstd => Some(CompressionType::ZSTD),
        };
        let ipc = IpcWriteOptions::default()
            .try_with_compression(codec)
            .map_err(|e| Error::internal(format!("arrow: {e}")))?;
        let writer = StreamWriter::try_new_with_options(
            HashingWriter {
                inner: zip,
                hasher: EntryHasher::new(),
            },
            schema,
            ipc,
        )
        .map_err(|e| Error::internal(format!("cannot start a table's data: {e}")))?;
        self.state = WriterState::Table {
            writer: Box::new(writer),
            rows: 0,
        };
        Ok(())
    }

    /// Appends rows to the open table entry.
    pub fn write_rows(&mut self, batch: &RecordBatch) -> Result<()> {
        let WriterState::Table { writer, rows } = &mut self.state else {
            return Err(Error::internal("no table data entry is open"));
        };
        if let Err(e) = writer.write(batch) {
            self.state = WriterState::Poisoned;
            return Err(Error::internal(format!("cannot write table data: {e}")));
        }
        *rows += u64::try_from(batch.num_rows()).unwrap_or(u64::MAX);
        Ok(())
    }

    /// Ends the open table entry; returns its info and row count.
    pub fn end_table(&mut self) -> Result<(EntryInfo, u64)> {
        if !matches!(self.state, WriterState::Table { .. }) {
            return Err(Error::internal("no table data entry is open"));
        }
        let WriterState::Table { writer, rows } =
            std::mem::replace(&mut self.state, WriterState::Poisoned)
        else {
            return Err(poisoned());
        };
        let hashing = (*writer)
            .into_inner()
            .map_err(|e| Error::internal(format!("cannot finish table data: {e}")))?;
        self.state = WriterState::Idle(hashing.inner);
        Ok((hashing.hasher.finish(), rows))
    }

    /// Writes the manifest and the ZIP directory; returns the sink (to commit).
    pub fn finish(mut self, manifest: &Manifest) -> Result<W> {
        let json = serde_json::to_vec_pretty(manifest)
            .map_err(|e| Error::internal(format!("cannot encode the manifest: {e}")))?;
        self.put(MANIFEST_ENTRY, &json)?;
        let WriterState::Idle(zip) = self.state else {
            return Err(poisoned());
        };
        zip.finish().map_err(|e| zip_error(&e))
    }
}

/// One entry's verification result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryCheck {
    pub entry: String,
    pub ok: bool,
    /// What is wrong (checksum mismatch, missing entry, row count differs, unreadable IPC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// The result of [`ArchiveReader::verify`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyReport {
    /// Every entry matched its checksum and every table its row count.
    pub ok: bool,
    pub entries_checked: u32,
    pub bytes_checked: u64,
    /// Problems only (empty when `ok`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<EntryCheck>,
}

impl VerifyReport {
    /// The problems as one line each (`entry: problem`), for error messages.
    pub fn problem_lines(&self) -> Vec<String> {
        self.problems
            .iter()
            .map(|p| {
                format!(
                    "{}: {}",
                    p.entry,
                    p.problem.as_deref().unwrap_or("not valid")
                )
            })
            .collect()
    }
}

/// A reader that hashes and counts what goes through it, polls a cancel flag and reports
/// progress.
struct HashingReader<'c, R: Read> {
    inner: R,
    hasher: EntryHasher,
    read: u64,
    cancelled: Option<&'c dyn Fn() -> bool>,
    progress: Option<&'c mut dyn FnMut(u64)>,
    /// Bytes checked before this entry (progress is cumulative).
    base: u64,
    reported: u64,
    was_cancelled: bool,
}

impl<'c, R: Read> HashingReader<'c, R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: EntryHasher::new(),
            read: 0,
            cancelled: None,
            progress: None,
            base: 0,
            reported: 0,
            was_cancelled: false,
        }
    }

    fn watched(
        inner: R,
        base: u64,
        cancelled: &'c dyn Fn() -> bool,
        progress: &'c mut dyn FnMut(u64),
    ) -> Self {
        Self {
            cancelled: Some(cancelled),
            progress: Some(progress),
            base,
            ..Self::new(inner)
        }
    }

    /// Reads (and hashes) everything that is left.
    fn drain(&mut self) -> io::Result<()> {
        let mut buf = vec![0_u8; 64 * 1024];
        loop {
            if self.read(&mut buf)? == 0 {
                return Ok(());
            }
        }
    }
}

impl<R: Read> Read for HashingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancelled.is_some_and(|c| c()) {
            self.was_cancelled = true;
            return Err(io::Error::other("cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.read += u64::try_from(n).unwrap_or(u64::MAX);
        if let Some(progress) = &mut self.progress
            && self.read - self.reported >= PROGRESS_STEP
        {
            self.reported = self.read;
            progress(self.base + self.read);
        }
        Ok(n)
    }
}

/// Reads one archive. Opening reads and validates the manifest (`Manifest::validate`); nothing
/// else is read until asked.
pub struct ArchiveReader<R: Read + Seek> {
    zip: zip::ZipArchive<R>,
    manifest: Manifest,
}

impl ArchiveReader<std::fs::File> {
    /// Opens the archive at `path`. `InvalidInput` for a file that is not an Oxbase backup,
    /// `Unsupported` for a newer format version, `Io` for a file that cannot be read.
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).map_err(|e| Error::Io {
            kind: e.kind().to_string(),
            message: format!("{}: {e}", oxbase_io::file_label(path)),
        })?;
        Self::from_reader(file)
    }
}

/// The manifest JSON, checked in the order that gives the most useful error: not a backup,
/// newer format, malformed.
fn parse_manifest(bytes: &[u8]) -> Result<Manifest> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| Error::invalid_input("the file is not an Oxbase backup (bad manifest)"))?;
    if value.get("format").and_then(serde_json::Value::as_str) != Some(FORMAT_NAME) {
        return Err(Error::invalid_input("the file is not an Oxbase backup"));
    }
    let version = value
        .get("formatVersion")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if version > u64::from(FORMAT_VERSION) {
        let by = value
            .get("oxbaseVersion")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("a newer version");
        return Err(Error::unsupported(format!(
            "the backup was written in format version {version} by Oxbase {by}; this version \
             reads up to format version {FORMAT_VERSION}"
        )));
    }
    let manifest: Manifest = serde_json::from_value(value)
        .map_err(|e| Error::invalid_input(format!("the backup manifest is malformed: {e}")))?;
    manifest.validate()?;
    Ok(manifest)
}

/// Reads a whole entry of at most `cap` bytes.
fn read_capped<R: Read>(entry: &mut R, size: u64, cap: u64, name: &str) -> Result<Vec<u8>> {
    if size > cap {
        return Err(Error::invalid_input(format!(
            "the backup entry {name} is too large ({size} bytes)"
        )));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    entry
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::invalid_input(format!("the backup entry {name} is damaged: {e}")))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > cap {
        return Err(Error::invalid_input(format!(
            "the backup entry {name} is too large"
        )));
    }
    Ok(bytes)
}

impl<R: Read + Seek> ArchiveReader<R> {
    /// Opens an archive from any reader (tests: in-memory archives).
    pub fn from_reader(reader: R) -> Result<Self> {
        let mut zip = zip::ZipArchive::new(reader).map_err(|_| {
            Error::invalid_input("the file is not an Oxbase backup (not a ZIP archive)")
        })?;
        let bytes = {
            let mut entry = zip.by_name(MANIFEST_ENTRY).map_err(|_| {
                Error::invalid_input("the file is not an Oxbase backup (no manifest)")
            })?;
            let size = entry.size();
            read_capped(&mut entry, size, MAX_JSON_ENTRY, MANIFEST_ENTRY)?
        };
        let manifest = parse_manifest(&bytes)?;
        Ok(Self { zip, manifest })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The bytes of a small entry listed in the manifest, checksum verified.
    fn small_entry(&mut self, name: &str) -> Result<Vec<u8>> {
        let info = self
            .manifest
            .entries
            .get(name)
            .cloned()
            .ok_or_else(|| Error::invalid_input(format!("the backup does not list {name}")))?;
        let mut entry = self
            .zip
            .by_name(name)
            .map_err(|_| Error::invalid_input(format!("the backup entry {name} is missing")))?;
        let bytes = read_capped(&mut entry, info.size, MAX_JSON_ENTRY, name)?;
        if sha256_hex(&bytes) != info.sha256
            || u64::try_from(bytes.len()).unwrap_or(u64::MAX) != info.size
        {
            return Err(Error::invalid_input(format!(
                "the backup entry {name} does not match its checksum"
            )));
        }
        Ok(bytes)
    }

    /// Object `index`'s definition (checksum verified while reading).
    pub fn definition(&mut self, index: u32) -> Result<ObjectDef> {
        let name = self
            .manifest
            .object(index)
            .map(|o| o.definition.clone())
            .ok_or_else(|| Error::invalid_input(format!("the backup has no object {index}")))?;
        let bytes = self.small_entry(&name)?;
        serde_json::from_slice(&bytes).map_err(|e| {
            Error::invalid_input(format!("the backup definition {name} is malformed: {e}"))
        })
    }

    /// Object `index`'s DDL text, when the backup has it (checksum verified).
    pub fn ddl(&mut self, index: u32) -> Result<Option<String>> {
        let Some(name) = self.manifest.object(index).and_then(|o| o.ddl.clone()) else {
            return Ok(None);
        };
        let bytes = self.small_entry(&name)?;
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| Error::invalid_input(format!("the backup entry {name} is not text")))
    }

    /// Table `index`'s rows, batch by batch (checksum verified at the end: the last item is an
    /// error when it does not match).
    pub fn table_rows(
        &mut self,
        index: u32,
    ) -> Result<Box<dyn Iterator<Item = Result<RecordBatch>> + '_>> {
        let object = self
            .manifest
            .object(index)
            .ok_or_else(|| Error::invalid_input(format!("the backup has no object {index}")))?;
        let data = object.data.clone().ok_or_else(|| {
            Error::invalid_input(format!(
                "the backup has no rows for {}",
                object.object.display_name()
            ))
        })?;
        let info = self
            .manifest
            .entries
            .get(&data.entry)
            .cloned()
            .ok_or_else(|| {
                Error::invalid_input(format!("the backup does not list {}", data.entry))
            })?;
        let entry = self.zip.by_name(&data.entry).map_err(|_| {
            Error::invalid_input(format!("the backup entry {} is missing", data.entry))
        })?;
        let reader = HashingReader::new(entry);
        let stream = catch_unwind(AssertUnwindSafe(move || {
            StreamReader::try_new(reader, None)
        }))
        .map_err(|_| panic_error())?
        .map_err(|e| arrow_error(&e))?;
        Ok(Box::new(TableRows {
            stream: Some(stream),
            expected: info,
            expected_rows: data.rows,
            rows: 0,
            name: data.entry,
        }))
    }

    /// Recomputes every entry's checksum and every table's row count. `on_bytes` reports
    /// progress (bytes checked so far); `cancelled` is polled between reads.
    pub fn verify(
        &mut self,
        on_bytes: &mut dyn FnMut(u64),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<VerifyReport> {
        self.verify_entries(None, on_bytes, cancelled)
    }

    /// [`ArchiveReader::verify`] of the entries of the objects `indexes` only (`None`: every
    /// entry the manifest lists). Restore checks what it is about to use.
    pub fn verify_objects(
        &mut self,
        indexes: &BTreeSet<u32>,
        with_data: bool,
        on_bytes: &mut dyn FnMut(u64),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<VerifyReport> {
        let mut names = BTreeSet::new();
        for o in &self.manifest.objects {
            if !indexes.contains(&o.index) {
                continue;
            }
            names.insert(o.definition.clone());
            if let Some(ddl) = &o.ddl {
                names.insert(ddl.clone());
            }
            if with_data && let Some(data) = &o.data {
                names.insert(data.entry.clone());
            }
        }
        self.verify_entries(Some(&names), on_bytes, cancelled)
    }

    fn verify_entries(
        &mut self,
        only: Option<&BTreeSet<String>>,
        on_bytes: &mut dyn FnMut(u64),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<VerifyReport> {
        let rows_of: BTreeMap<String, u64> = self
            .manifest
            .objects
            .iter()
            .filter_map(|o| o.data.as_ref().map(|d| (d.entry.clone(), d.rows)))
            .collect();
        let entries: Vec<(String, EntryInfo)> = self
            .manifest
            .entries
            .iter()
            .filter(|(name, _)| only.is_none_or(|only| only.contains(*name)))
            .map(|(name, info)| (name.clone(), info.clone()))
            .collect();
        let mut report = VerifyReport {
            ok: true,
            entries_checked: 0,
            bytes_checked: 0,
            problems: Vec::new(),
        };
        for (name, info) in entries {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            report.entries_checked += 1;
            let base = report.bytes_checked;
            let (problem, read) = match self.zip.by_name(&name) {
                Err(_) => (Some("the entry is missing from the archive".to_owned()), 0),
                Ok(entry) => {
                    let mut reader = HashingReader::watched(entry, base, cancelled, &mut *on_bytes);
                    let drained = reader.drain();
                    if reader.was_cancelled {
                        return Err(Error::Cancelled);
                    }
                    let read = reader.read;
                    let problem = match drained {
                        Err(e) => Some(format!("the entry cannot be read: {e}")),
                        Ok(()) => (std::mem::take(&mut reader.hasher).finish() != info)
                            .then(|| "the entry does not match its checksum".to_owned()),
                    };
                    (problem, read)
                }
            };
            report.bytes_checked = base + read;
            // Table data is decoded only once its bytes matched the checksum: corrupted IPC
            // can name absurd buffer sizes.
            let problem = match (problem, rows_of.get(&name)) {
                (None, Some(&expected)) => self.count_rows(&name, expected),
                (problem, _) => problem,
            };
            on_bytes(report.bytes_checked);
            if let Some(problem) = problem {
                report.ok = false;
                report.problems.push(EntryCheck {
                    entry: name,
                    ok: false,
                    problem: Some(problem),
                });
            }
        }
        Ok(report)
    }

    /// Decodes data entry `name` and compares its row count with `expected`; the problem, if
    /// any.
    fn count_rows(&mut self, name: &str, expected: u64) -> Option<String> {
        let entry = match self.zip.by_name(name) {
            Ok(entry) => entry,
            Err(e) => return Some(format!("the entry cannot be read: {e}")),
        };
        let decoded = catch_unwind(AssertUnwindSafe(
            move || -> std::result::Result<u64, String> {
                let stream = StreamReader::try_new(entry, None).map_err(|e| e.to_string())?;
                let mut n = 0_u64;
                for batch in stream {
                    let batch = batch.map_err(|e| e.to_string())?;
                    n += u64::try_from(batch.num_rows()).unwrap_or(u64::MAX);
                }
                Ok(n)
            },
        ));
        match decoded {
            Ok(Ok(n)) if n == expected => None,
            Ok(Ok(n)) => Some(format!(
                "the table data has {n} rows, the manifest says {expected}"
            )),
            Ok(Err(e)) => Some(format!("the table data cannot be read: {e}")),
            Err(_) => Some("the table data cannot be decoded".to_owned()),
        }
    }
}

/// The iterator of [`ArchiveReader::table_rows`].
struct TableRows<R: Read> {
    stream: Option<StreamReader<HashingReader<'static, R>>>,
    expected: EntryInfo,
    expected_rows: u64,
    rows: u64,
    name: String,
}

impl<R: Read> TableRows<R> {
    /// After the last batch: the rest of the entry, its checksum and the row count.
    fn finish(&mut self, stream: &mut StreamReader<HashingReader<'static, R>>) -> Result<()> {
        let reader = stream.get_mut();
        reader.drain().map_err(|e| {
            Error::invalid_input(format!("the backup entry {} is damaged: {e}", self.name))
        })?;
        let actual = std::mem::take(&mut reader.hasher).finish();
        if actual != self.expected {
            return Err(Error::invalid_input(format!(
                "the backup entry {} does not match its checksum",
                self.name
            )));
        }
        if self.rows != self.expected_rows {
            return Err(Error::invalid_input(format!(
                "the backup entry {} has {} rows, the manifest says {}",
                self.name, self.rows, self.expected_rows
            )));
        }
        Ok(())
    }
}

impl<R: Read> Iterator for TableRows<R> {
    type Item = Result<RecordBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut stream = self.stream.take()?;
        let next = catch_unwind(AssertUnwindSafe(|| stream.next()));
        match next {
            Err(_) => Some(Err(panic_error())),
            Ok(Some(Err(e))) => Some(Err(arrow_error(&e))),
            Ok(Some(Ok(batch))) => {
                self.rows += u64::try_from(batch.num_rows()).unwrap_or(u64::MAX);
                self.stream = Some(stream);
                Some(Ok(batch))
            }
            Ok(None) => self.finish(&mut stream).err().map(Err),
        }
    }
}

#[cfg(test)]
mod tests;
