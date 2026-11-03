//! Reading and writing archives (`docs/m8-contracts.md` §7.1-§7.2). Synchronous (ZIP and
//! Arrow IPC are blocking I/O): the async backup and restore run them on blocking threads with
//! a bounded channel in between. Owner of the bodies marked `unimplemented!("m8: backup")`:
//! the **backup** stream.

use std::io::{Read, Seek, Write};
use std::path::Path;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use oxbase_types::{ObjectDef, Result};
use serde::{Deserialize, Serialize};

use crate::format::{Compression, EntryInfo, Manifest};

/// Writes one archive into `W` (an `oxbase_io::SinkFile`, renamed on success). Objects first,
/// then each table's rows, then [`ArchiveWriter::finish`] writes the manifest (the manifest
/// lists the checksums, so it is written last; readers find it through the ZIP directory).
pub struct ArchiveWriter<W: Write + Seek> {
    zip: zip::ZipWriter<W>,
    compression: Compression,
}

impl<W: Write + Seek> ArchiveWriter<W> {
    pub fn new(sink: W, compression: Compression) -> Self {
        Self {
            zip: zip::ZipWriter::new(sink),
            compression,
        }
    }

    /// Writes object `index`'s definition JSON and, when given, its DDL text; returns their
    /// entry infos (definition, ddl).
    pub fn add_object(
        &mut self,
        index: u32,
        definition: &ObjectDef,
        ddl: Option<&str>,
    ) -> Result<(EntryInfo, Option<EntryInfo>)> {
        let _ = (&mut self.zip, self.compression, index, definition, ddl);
        unimplemented!("m8: backup")
    }

    /// Starts table `index`'s data entry with `schema` (the load schema); batches follow with
    /// [`ArchiveWriter::write_rows`], then [`ArchiveWriter::end_table`].
    pub fn begin_table(&mut self, index: u32, schema: &SchemaRef) -> Result<()> {
        let _ = (&mut self.zip, index, schema);
        unimplemented!("m8: backup")
    }

    /// Appends rows to the open table entry.
    pub fn write_rows(&mut self, batch: &RecordBatch) -> Result<()> {
        let _ = (&mut self.zip, batch);
        unimplemented!("m8: backup")
    }

    /// Ends the open table entry; returns its info and row count.
    pub fn end_table(&mut self) -> Result<(EntryInfo, u64)> {
        let _ = &mut self.zip;
        unimplemented!("m8: backup")
    }

    /// Writes the manifest and the ZIP directory; returns the sink (to commit).
    pub fn finish(self, manifest: &Manifest) -> Result<W> {
        let _ = (self.zip, manifest);
        unimplemented!("m8: backup")
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
        let _ = path;
        unimplemented!("m8: backup")
    }
}

impl<R: Read + Seek> ArchiveReader<R> {
    /// Opens an archive from any reader (tests: in-memory archives).
    pub fn from_reader(reader: R) -> Result<Self> {
        let _ = reader;
        unimplemented!("m8: backup")
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Object `index`'s definition (checksum verified while reading).
    pub fn definition(&mut self, index: u32) -> Result<ObjectDef> {
        let _ = (&mut self.zip, index);
        unimplemented!("m8: backup")
    }

    /// Table `index`'s rows, batch by batch (checksum verified at the end: the last item is an
    /// error when it does not match).
    pub fn table_rows(
        &mut self,
        index: u32,
    ) -> Result<Box<dyn Iterator<Item = Result<RecordBatch>> + '_>> {
        let _ = (&mut self.zip, index);
        unimplemented!("m8: backup")
    }

    /// Recomputes every entry's checksum and every table's row count. `on_bytes` reports
    /// progress (bytes checked so far); `cancelled` is polled between reads.
    pub fn verify(
        &mut self,
        on_bytes: &mut dyn FnMut(u64),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<VerifyReport> {
        let _ = (&mut self.zip, on_bytes, cancelled);
        unimplemented!("m8: backup")
    }
}
