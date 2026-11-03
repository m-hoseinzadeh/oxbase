//! Restore into the same engine (`docs/m8-contracts.md` §7.3). Owner of the body marked
//! `unimplemented!("m8: backup")`: the **backup** stream.
//!
//! # Steps ([`restore`])
//!
//! 1. Open the archive (`ArchiveReader::open`: manifest validated); refuse another engine
//!    (`InvalidInput`: "the backup is from PostgreSQL, the target is MySQL"); verify the
//!    checksums of the selected entries first unless [`RestoreOptions::verify_first`] is off.
//! 2. **Structure** (content `structure_and_data` / `structure_only`): the archive's
//!    definitions form a source `oxbase_diff::SchemaSnapshot` of the backed-up container; the
//!    target container is read with `oxbase_diff::load_snapshot`; `oxbase_diff::compare` and
//!    `oxbase_diff::sync_script` (selection = the selected objects' items, drops only of those
//!    with [`RestoreOptions::drop_existing`]) give the DDL, which the services run with
//!    `execute_script` (one transaction where the engine has transactional DDL). An object that
//!    already exists and is not dropped is left as it is when equal and is an error when it
//!    differs ("exists and differs; restore with drop existing"). The target container itself
//!    must exist (M8 does not create databases or schemas).
//! 3. **Data** (content `structure_and_data` / `data_only`): per selected table in manifest
//!    order (parents first): optionally empty it ([`RestoreOptions::truncate_before_load`],
//!    always for a table the structure step just created), then stream its batches from the
//!    archive into the target engine's `BulkLoader` (they are already in the load schema), one
//!    transaction per table, foreign-key checks off where the engine allows
//!    (`BulkLoader::foreign_key_checks`) when tables form a cycle, identity values kept
//!    (`BulkLoader::prepare` / `identity_resync`).
//! 4. Report per object (created, dropped, left, rows loaded) and what a failure or cancel
//!    left: structure steps follow the script rules (rolled back where atomic); data per table
//!    (a failed table rolls back; earlier tables stay).
//!
//! Destructive restores (drop existing, truncate) need the caller's confirmation
//! ([`RestoreOptions::confirm_destructive`]); the services refuse without it.

use oxbase_dialect::DialectRegistry;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{ObjectRef, Result};
use serde::{Deserialize, Serialize};

/// What a restore writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreContent {
    #[default]
    StructureAndData,
    StructureOnly,
    DataOnly,
}

/// How to restore. Every field but `container` has a default (camelCase JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreOptions {
    /// The existing database or schema to restore into (may differ from the backed-up one).
    pub container: ObjectRef,
    #[serde(default)]
    pub content: RestoreContent,
    /// Manifest object indexes to restore (empty: all).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<u32>,
    /// Drop selected objects that exist in the target before creating them. Destructive.
    #[serde(default)]
    pub drop_existing: bool,
    /// Empty existing tables before loading (data only). Destructive.
    #[serde(default)]
    pub truncate_before_load: bool,
    /// Check the selected entries' checksums before writing anything.
    #[serde(default = "yes")]
    pub verify_first: bool,
    /// The user confirmed a destructive restore.
    #[serde(default)]
    pub confirm_destructive: bool,
    /// Rows per load call (default: the loader's own `rows_per_call`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_rows: Option<usize>,
}

fn yes() -> bool {
    true
}

impl RestoreOptions {
    /// Restore everything into `container` with the defaults.
    pub fn into_container(container: ObjectRef) -> Self {
        Self {
            container,
            content: RestoreContent::StructureAndData,
            objects: Vec::new(),
            drop_existing: false,
            truncate_before_load: false,
            verify_first: true,
            confirm_destructive: false,
            batch_rows: None,
        }
    }

    /// `true` when the restore can destroy existing data.
    pub fn is_destructive(&self) -> bool {
        self.drop_existing || self.truncate_before_load
    }
}

/// Cumulative restore progress.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreProgress {
    /// `verify`, `structure` or `data`.
    pub phase: String,
    pub objects_done: u32,
    pub objects_total: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<ObjectRef>,
    pub rows: u64,
    pub rows_total: u64,
    pub elapsed_ms: u64,
}

/// What happened to one object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoredStatus {
    Created,
    /// Dropped and created again.
    Replaced,
    /// Existed and was equal: left as it is.
    Kept,
    /// Rows loaded into an existing table (data only).
    Loaded,
    Failed,
    NotRun,
}

/// One object's outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoredObject {
    pub index: u32,
    /// The object as created in the target container.
    pub object: ObjectRef,
    pub status: RestoredStatus,
    #[serde(default)]
    pub rows: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The outcome of a restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreReport {
    pub objects: Vec<RestoredObject>,
    pub rows: u64,
    /// What a failure or cancel left behind, in words.
    pub left_behind: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub elapsed_ms: u64,
}

/// Runs a structure script on the restore's target session with the `oxbase_types::script`
/// semantics; `Ok(true)` when it committed. The services pass `execute_script` on a dedicated
/// child session.
pub type ScriptRunner =
    dyn Fn(oxbase_types::Script) -> futures::future::BoxFuture<'static, Result<bool>> + Send + Sync;

/// The session and registry a restore writes through. The structure script runs through
/// `run_script` (the services pass `execute_script` on a dedicated child session), so this
/// crate needs no services dependency.
#[derive(Clone, Copy)]
pub struct RestoreJob<'a> {
    pub dialects: &'a DialectRegistry,
    /// A dedicated session on the target database (structure reads, loads).
    pub target: &'a dyn Connection,
    pub options: &'a RestoreOptions,
}

/// Restores the archive at `source` (module docs). `run_script` executes a structure script on
/// the target session with the `oxbase_types::script` semantics and returns whether it
/// committed.
pub async fn restore(
    job: RestoreJob<'_>,
    source: &std::path::Path,
    run_script: &ScriptRunner,
    on_progress: &(dyn Fn(&RestoreProgress) + Send + Sync),
    cancel: CancellationToken,
) -> Result<RestoreReport> {
    let _ = (job.options, source, run_script, on_progress, cancel);
    // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
    std::future::ready(()).await;
    unimplemented!("m8: backup")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn options_default_to_a_verified_non_destructive_restore() {
        let o: RestoreOptions =
            serde_json::from_str(r#"{"container":{"kind":"database","name":"m8_restore"}}"#)
                .unwrap();
        assert_eq!(
            o,
            RestoreOptions::into_container(ObjectRef::database("m8_restore"))
        );
        assert!(o.verify_first && !o.is_destructive());
        let json = serde_json::to_value(&o).unwrap();
        assert_eq!(json["content"], "structure_and_data");
        assert!(json.get("objects").is_none());
    }
}
