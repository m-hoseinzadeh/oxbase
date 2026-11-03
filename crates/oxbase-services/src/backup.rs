//! Backup and restore service (M8, `docs/m8-contracts.md` §7.4, §8). Owner of the bodies marked
//! `unimplemented!("m8: backup")`: the **backup** stream.
//!
//! Files are paths here (the app turns its file handles into paths, as for M5 import / export).
//!
//! - [`BackupService::start_backup`]: a dedicated child session on the container's database
//!   reads definitions and rows (`oxbase_backup::backup::backup`); the archive is written next
//!   to `destination` and renamed on success. Events: `Started`, `Progress` (lossy),
//!   one terminal.
//! - [`BackupService::start_restore`]: refuses a destructive restore without
//!   `confirm_destructive`, then runs `oxbase_backup::restore::restore` on a dedicated child
//!   session of the target database; the structure script runs through
//!   [`crate::designer::execute_script`]; created objects are announced on [`SchemaEvents`].
//! - [`BackupService::verify`] and [`BackupService::list`] read the archive on a blocking
//!   thread (no session).
//!
//! Every run registers on its session ([`crate::runs::RunRegistry`]): `cancel` by id, and
//! closing the connection cancels it. A cancel ends a run within 2 s; a cancelled backup leaves
//! no file.

use std::path::Path;
use std::sync::Arc;

use oxbase_dialect::DialectRegistry;
use oxbase_types::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub use oxbase_backup::{
    BackupContent, BackupProgress, BackupReport, BackupSpec, Compression, Manifest, ManifestObject,
    RestoreContent, RestoreOptions, RestoreProgress, RestoreReport, RestoredObject, RestoredStatus,
    VerifyReport,
};

use crate::query::ErrorInfo;
use crate::runs::{OperationId, RunRegistry};
use crate::schema_cache::SchemaEvents;
use crate::tree::TreeService;
use crate::{CancellationToken, ConnectionManager, SessionId};

/// Back up a database or schema of an open connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupRequest {
    /// A base session.
    pub session: SessionId,
    pub spec: BackupSpec,
}

/// Restore an archive into an open connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreRequest {
    /// A base session.
    pub session: SessionId,
    pub options: RestoreOptions,
}

/// What a backup reports, in order.
///
/// JSON: `{"type": "<camelCase variant>", ...camelCase fields}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BackupEvent {
    Started {
        backup_id: OperationId,
        objects: u32,
    },
    Progress {
        progress: BackupProgress,
    },
    Finished {
        report: Box<BackupReport>,
    },
    Failed {
        error: ErrorInfo,
        elapsed_ms: u64,
    },
    Cancelled {
        elapsed_ms: u64,
    },
}

/// What a restore reports, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RestoreEvent {
    Started {
        restore_id: OperationId,
        manifest: Box<Manifest>,
    },
    Progress {
        progress: RestoreProgress,
    },
    Finished {
        report: RestoreReport,
    },
    Failed {
        error: ErrorInfo,
        report: RestoreReport,
    },
    Cancelled {
        report: RestoreReport,
    },
}

impl BackupEvent {
    /// `true` for `Finished`, `Failed` and `Cancelled`.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Finished { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }
}

impl RestoreEvent {
    /// `true` for `Finished`, `Failed` and `Cancelled`.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Finished { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }
}

/// A started backup or restore: its id and its event stream.
#[derive(Debug)]
pub struct BackupRunHandle<E> {
    pub id: OperationId,
    pub events: mpsc::Receiver<E>,
}

/// Backs up, restores, verifies and lists archives. See the module docs.
pub struct BackupService {
    manager: Arc<ConnectionManager>,
    tree: Arc<TreeService>,
    dialects: Arc<DialectRegistry>,
    runs: RunRegistry,
    schema_events: Option<SchemaEvents>,
}

impl BackupService {
    pub fn new(
        manager: Arc<ConnectionManager>,
        tree: Arc<TreeService>,
        dialects: Arc<DialectRegistry>,
    ) -> Self {
        Self {
            manager,
            tree,
            dialects,
            runs: RunRegistry::new(),
            schema_events: None,
        }
    }

    /// Announces restored objects on `events` (M6 schema cache).
    #[must_use]
    pub fn with_schema_events(mut self, events: SchemaEvents) -> Self {
        self.schema_events = Some(events);
        self
    }

    pub fn manager(&self) -> &Arc<ConnectionManager> {
        &self.manager
    }

    pub fn tree(&self) -> &Arc<TreeService> {
        &self.tree
    }

    pub fn dialects(&self) -> &Arc<DialectRegistry> {
        &self.dialects
    }

    pub fn runs(&self) -> &RunRegistry {
        &self.runs
    }

    /// Starts a backup into `destination` (`overwrite`: the user confirmed replacing it).
    pub async fn start_backup(
        &self,
        request: BackupRequest,
        destination: &Path,
        overwrite: bool,
    ) -> Result<BackupRunHandle<BackupEvent>> {
        let _ = (&self.runs, request, destination, overwrite);
        // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
        std::future::ready(()).await;
        unimplemented!("m8: backup")
    }

    /// Starts restoring the archive at `source`.
    pub async fn start_restore(
        &self,
        request: RestoreRequest,
        source: &Path,
    ) -> Result<BackupRunHandle<RestoreEvent>> {
        let _ = (&self.schema_events, request, source);
        // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
        std::future::ready(()).await;
        unimplemented!("m8: backup")
    }

    /// Recomputes every checksum of the archive at `path`.
    pub async fn verify(&self, path: &Path, cancel: CancellationToken) -> Result<VerifyReport> {
        let _ = (&self.runs, path, cancel);
        // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
        std::future::ready(()).await;
        unimplemented!("m8: backup")
    }

    /// The manifest of the archive at `path` (validated).
    pub async fn list(&self, path: &Path, cancel: CancellationToken) -> Result<Manifest> {
        let _ = (&self.runs, path, cancel);
        // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
        std::future::ready(()).await;
        unimplemented!("m8: backup")
    }

    /// Cancels a running backup or restore; `false` when it is unknown or already ended.
    pub fn cancel(&self, id: OperationId) -> bool {
        self.runs.cancel(id)
    }

    /// Cancels every backup and restore that uses `session` (closing it).
    pub fn cancel_session(&self, session: SessionId) -> usize {
        self.runs.cancel_session(session)
    }
}

impl std::fmt::Debug for BackupService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackupService")
            .field("runs", &self.runs)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::ObjectRef;
    use pretty_assertions::assert_eq;

    #[test]
    fn payloads_are_camel_case() {
        let request = BackupRequest {
            session: SessionId::new(),
            spec: BackupSpec {
                container: ObjectRef::database("m8_src"),
                objects: Vec::new(),
                content: BackupContent::StructureAndData,
                compression: Compression::Zstd,
            },
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["spec"]["content"], "structure_and_data");
        assert_eq!(json["spec"]["compression"]["codec"], "zstd");
        assert_eq!(
            serde_json::from_value::<BackupRequest>(json).unwrap(),
            request
        );

        let event = BackupEvent::Cancelled { elapsed_ms: 3 };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::json!({"type": "cancelled", "elapsedMs": 3})
        );
        assert!(event.is_terminal());
        let restore: RestoreRequest = serde_json::from_value(serde_json::json!({
            "session": SessionId::new(),
            "options": {"container": {"kind": "schema", "catalog": "oxbase_test", "name": "m8_restore"}}
        }))
        .unwrap();
        assert!(restore.options.verify_first);
    }
}
