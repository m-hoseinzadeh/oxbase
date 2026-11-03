//! Logical backup and restore (M8): the Oxbase backup archive (format, writer, reader, verify,
//! list), backup of a database or schema, restore into the same engine. Contract:
//! `docs/m8-contracts.md` §7.
//!
//! Module map:
//!
//! - [`format`]: the archive layout, [`Manifest`] (format version, engine, server version,
//!   objects, order, checksums; never connection details), entry names, SHA-256.
//! - [`archive`]: [`archive::ArchiveWriter`] / [`archive::ArchiveReader`] (ZIP + Arrow IPC,
//!   blocking I/O), [`archive::VerifyReport`].
//! - [`backup`]: [`backup::backup`] of a [`backup::BackupSpec`] with progress and cancel.
//! - [`restore`]: [`restore::restore`] with [`restore::RestoreOptions`] (new or existing
//!   container, structure / data / both, selected objects, drop existing), dependency order.
//!
//! **Relation to the M5 SQL dump:** separate. A SQL dump (`oxbase-io`, export) is a portable
//! text script for one table or result, readable by any SQL client and by other engines; a
//! backup is Oxbase's own archive of a whole database or schema, restorable into the same engine
//! with exact values (Arrow, not literals), checksums and selective restore. Restore never reads
//! SQL dumps; a backup never writes one (each object's `CREATE` text is inside for people).
//!
//! Sessions, file handles, events and cancellation live in `oxbase_services::backup`.

pub mod archive;
pub mod backup;
pub mod format;
mod load;
mod objects;
pub mod restore;

pub use archive::{EntryCheck, VerifyReport};
pub use backup::{BackupProgress, BackupReport, BackupSpec};
pub use format::{
    BackupContent, BackupSource, Compression, EntryInfo, FILE_EXTENSION, FORMAT_VERSION, Manifest,
    ManifestObject, TableData,
};
pub use restore::{
    RestoreContent, RestoreOptions, RestoreOutcome, RestoreProgress, RestoreReport, RestoredObject,
    RestoredStatus,
};
