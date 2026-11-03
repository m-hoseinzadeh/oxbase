//! Logical backup of a database or schema (`docs/m8-contracts.md` §7.2). Owner of the body
//! marked `unimplemented!("m8: backup")`: the **backup** stream.
//!
//! # Steps ([`backup`])
//!
//! 1. Snapshot the container with `oxbase_diff::load_snapshot` (tables, views, materialized
//!    views, sequences, routines, triggers; [`BackupSpec::objects`] narrows it, a selected
//!    table brings its triggers).
//! 2. Order: tables by foreign keys, then sequences before the tables whose defaults use them,
//!    then routines, views (dependency order) and triggers: the same ordering the structure sync
//!    script uses, recorded as `Manifest::order`.
//! 3. Per object: its definition JSON and its `CREATE` text (the source engine's
//!    `DdlGenerator::create`, skipped with a warning when the engine has no generator for the
//!    kind).
//! 4. Per table (content `structure_and_data`): `SELECT` of every stored column (generated
//!    columns left out) in primary-key order when there is one, streamed from a dedicated
//!    session, converted to the load schema (`oxbase_transfer::convert::BatchConverter`, same
//!    engine) and written as Arrow IPC batches. Memory: one batch at a time. Rows of one table
//!    are read in one read-only transaction where the engine has one (PostgreSQL `REPEATABLE
//!    READ`, MySQL / MariaDB `START TRANSACTION WITH CONSISTENT SNAPSHOT`, SQL Server snapshot
//!    isolation when the database allows it, else read committed with a warning), so each table
//!    is consistent; tables are not consistent with each other unless the engine allows one
//!    transaction for all (PostgreSQL, MySQL / MariaDB: the stream uses one when it can and the
//!    report says which).
//! 5. The manifest last; the file is a temporary file next to the destination, renamed on
//!    success (`oxbase_io::SinkFile`). A failed or cancelled backup leaves no file and never
//!    damages an existing one.
//!
//! Progress: [`BackupProgress`] at most every 250 ms and after each object. Cancel: within 2 s
//! (the running `SELECT` is cancelled through the driver, the temporary file deleted).

use oxbase_dialect::DialectRegistry;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{ObjectRef, Result};
use serde::{Deserialize, Serialize};

use crate::format::{BackupContent, Compression, Manifest};

/// What to back up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSpec {
    /// The database (MySQL, MariaDB, SQLite) or schema (PostgreSQL, SQL Server).
    pub container: ObjectRef,
    /// Objects to include (empty: every object of the container).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<ObjectRef>,
    #[serde(default)]
    pub content: BackupContent,
    #[serde(default)]
    pub compression: Compression,
}

/// Cumulative backup progress.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupProgress {
    pub objects_done: u32,
    pub objects_total: u32,
    /// The object being written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<ObjectRef>,
    pub rows: u64,
    /// Bytes written to the archive so far.
    pub bytes: u64,
    pub elapsed_ms: u64,
}

/// What a finished backup wrote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupReport {
    pub manifest: Manifest,
    /// The archive's size in bytes.
    pub bytes: u64,
    pub elapsed_ms: u64,
}

/// The sessions and registry a backup reads through.
#[derive(Clone, Copy)]
pub struct BackupJob<'a> {
    pub dialects: &'a DialectRegistry,
    /// Reads the catalog (definitions).
    pub metadata: &'a dyn Connection,
    /// Reads table rows (a dedicated session: it holds the snapshot transaction).
    pub rows: &'a dyn Connection,
    pub spec: &'a BackupSpec,
}

/// Writes the backup of `job.spec` into `destination` (module docs). `overwrite` replaces an
/// existing file (the app asked the user in its save dialog).
pub async fn backup(
    job: BackupJob<'_>,
    destination: &std::path::Path,
    overwrite: bool,
    on_progress: &(dyn Fn(&BackupProgress) + Send + Sync),
    cancel: CancellationToken,
) -> Result<BackupReport> {
    let _ = (job.spec, destination, overwrite, on_progress, cancel);
    // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
    std::future::ready(()).await;
    unimplemented!("m8: backup")
}
