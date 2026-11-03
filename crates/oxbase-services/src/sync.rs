//! Structure sync and data sync services (M8, `docs/m8-contracts.md` §5.5, §6.4, §8).
//! Owners of the bodies marked `unimplemented!`: the **structure-sync** stream
//! ([`StructureSyncService`]) and the **data-sync** stream ([`DataSyncService`]).
//!
//! # Sessions
//!
//! Both sides are **base** sessions of the same engine (the same session twice is allowed:
//! two schemas of one server). Reads run on dedicated child sessions opened on each side's
//! database (PostgreSQL / SQL Server read one database per session); applies run on a
//! dedicated child session of the target, closed afterwards.
//!
//! # Compare results are kept by id
//!
//! A compare's result (snapshots and diff; for data, the [`oxbase_diff::DiffStore`] of differing
//! rows) stays in the service under its [`OperationId`], so `script` and `apply` work from
//! exactly what the user saw. At most [`KEPT_COMPARES`] results per service are kept (the oldest
//! is dropped); [`StructureSyncService::forget`] / [`DataSyncService::forget`] drop one when
//! its view closes. An unknown id is `NotFound` ("compare again").
//!
//! # Structure apply
//!
//! [`StructureSyncService::apply`]: refuse a destructive script without
//! [`StructureApplyRequest::confirm_destructive`]; re-read every selected object on the target
//! and refuse ("the target changed since the compare; compare again") when one differs from
//! the snapshot; run the script with [`crate::designer::execute_script`] (one transaction where
//! [`oxbase_diff::SyncScript::atomic`]); publish schema changes; then **re-compare** the same
//! containers with the same options and return the new result (`in_sync` must be true for the
//! applied items: the exit check).
//!
//! # Data compare and apply
//!
//! [`DataSyncService::start_compare`] runs `oxbase_diff::data::compare_table` per pair in
//! order (events: `Started`, `Progress`, `TableFinished`, one terminal). `script` renders the
//! preview. [`DataSyncService::start_apply`] refuses deletes without `confirm_deletes`, runs
//! the statements in transactions of `batch_statements` (deletes children-first, then updates,
//! then inserts parents-first; every statement must affect exactly one row, else the batch rolls
//! back and the apply stops), then re-compares the applied pairs and reports whether they are in
//! sync. Cancel ends either within 2 s (the open batch rolls back).

use std::sync::Arc;

use oxbase_dialect::DialectRegistry;
use oxbase_types::{ObjectRef, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub use oxbase_diff::{
    CompareProgress, DataApplyOptions, DataCompareOptions, DataScript, DataScriptOptions,
    DiffCounts, DiffItem, DiffStatus, KeyChoice, RowCounts, RowOp, RowSample, SkippedObject,
    StructureDiff, StructureOptions, SyncScript, TableCompareStatus, TableDataDiff, TablePair,
};

use crate::designer::ScriptOutcome;
use crate::query::ErrorInfo;
use crate::runs::{OperationId, RunRegistry};
use crate::schema_cache::SchemaEvents;
use crate::tree::TreeService;
use crate::{CancellationToken, ConnectionManager, SessionId};

/// Compare results kept per service (module docs).
pub const KEPT_COMPARES: usize = 8;

// --- structure -------------------------------------------------------------------------------

/// Compare two containers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructureCompareRequest {
    pub source_session: SessionId,
    /// The source database or schema.
    pub source: ObjectRef,
    pub target_session: SessionId,
    /// The target database or schema.
    pub target: ObjectRef,
    #[serde(default)]
    pub options: StructureOptions,
}

/// A structure compare result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructureCompareResult {
    /// The id `structure_script` / `structure_apply` / `compare_forget` take.
    pub compare_id: OperationId,
    pub diff: StructureDiff,
    pub counts: DiffCounts,
    /// Objects a side could not load (permissions, unsupported features).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedObject>,
    pub elapsed_ms: u64,
}

/// The script for some items of a compare.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructureScriptRequest {
    pub compare_id: OperationId,
    /// Item ids (empty: every differing item; drops only with the compare's `include_drops`).
    #[serde(default)]
    pub selection: Vec<String>,
}

/// Apply the script of some items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructureApplyRequest {
    pub compare_id: OperationId,
    #[serde(default)]
    pub selection: Vec<String>,
    /// The user confirmed a destructive script.
    #[serde(default)]
    pub confirm_destructive: bool,
}

/// What a structure apply did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructureApplyReport {
    pub script: SyncScript,
    pub outcome: ScriptOutcome,
    /// The re-compare after the apply (a new compare id).
    pub recompare: StructureCompareResult,
    /// The applied items are equal now (module docs).
    pub in_sync: bool,
}

/// Structure compare, script and apply. See the module docs.
pub struct StructureSyncService {
    manager: Arc<ConnectionManager>,
    tree: Arc<TreeService>,
    dialects: Arc<DialectRegistry>,
    runs: RunRegistry,
    schema_events: Option<SchemaEvents>,
}

impl StructureSyncService {
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

    /// Announces applied changes on `events` (M6 schema cache).
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

    /// The running compares and applies (registered with their request tokens).
    pub fn runs(&self) -> &RunRegistry {
        &self.runs
    }

    /// Compares `request` (registered on both sessions while it runs; `cancel` stops it).
    pub async fn compare(
        &self,
        request: &StructureCompareRequest,
        cancel: CancellationToken,
    ) -> Result<StructureCompareResult> {
        let _ = (&self.schema_events, request, cancel);
        // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
        std::future::ready(()).await;
        unimplemented!("m8: structure-sync")
    }

    /// The script of the selected items of a kept compare. Changes nothing.
    pub fn script(&self, request: &StructureScriptRequest) -> Result<SyncScript> {
        let _ = (&self.runs, request);
        unimplemented!("m8: structure-sync")
    }

    /// Applies the selected items (module docs, *Structure apply*).
    pub async fn apply(
        &self,
        request: &StructureApplyRequest,
        cancel: CancellationToken,
    ) -> Result<StructureApplyReport> {
        let _ = (&self.runs, request, cancel);
        // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
        std::future::ready(()).await;
        unimplemented!("m8: structure-sync")
    }

    /// Drops a kept compare; `false` when it is unknown.
    pub fn forget(&self, compare_id: OperationId) -> bool {
        let _ = (&self.runs, compare_id);
        unimplemented!("m8: structure-sync")
    }

    /// Cancels every compare and apply that uses `session` (closing it).
    pub fn cancel_session(&self, session: SessionId) -> usize {
        self.runs.cancel_session(session)
    }
}

impl std::fmt::Debug for StructureSyncService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StructureSyncService")
            .field("runs", &self.runs)
            .finish_non_exhaustive()
    }
}

// --- data ------------------------------------------------------------------------------------

/// Compare table data between two connections of the same engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataCompareRequest {
    pub source_session: SessionId,
    pub target_session: SessionId,
    pub pairs: Vec<TablePair>,
    #[serde(default)]
    pub options: DataCompareOptions,
}

/// Every table's result of a data compare.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataCompareResult {
    /// The id `data_script` / `data_apply` / `compare_forget` take.
    pub compare_id: OperationId,
    pub tables: Vec<TableDataDiff>,
    pub elapsed_ms: u64,
}

/// What a data compare reports, in order.
///
/// JSON: `{"type": "<camelCase variant>", ...camelCase fields}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DataCompareEvent {
    Started {
        compare_id: OperationId,
        tables: usize,
    },
    /// Cumulative counters of the running table (lossy).
    Progress {
        progress: CompareProgress,
    },
    TableFinished {
        table: Box<TableDataDiff>,
    },
    Finished {
        result: DataCompareResult,
    },
    /// The tables compared before the error are in `result` (the compare id stays usable for
    /// them).
    Failed {
        error: ErrorInfo,
        result: DataCompareResult,
    },
    Cancelled {
        result: DataCompareResult,
    },
}

impl DataCompareEvent {
    /// `true` for `Finished`, `Failed` and `Cancelled`.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Finished { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }
}

/// The script preview of a kept data compare.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataScriptRequest {
    pub compare_id: OperationId,
    #[serde(default)]
    pub options: DataScriptOptions,
}

/// Apply a kept data compare.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataApplyRequest {
    pub compare_id: OperationId,
    #[serde(default)]
    pub options: DataApplyOptions,
}

/// What a data apply did to one table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableApplyReport {
    pub pair: usize,
    pub target: ObjectRef,
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What a data apply did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataApplyReport {
    pub tables: Vec<TableApplyReport>,
    pub statements: u64,
    pub committed_batches: u64,
    /// What a failure or cancel left (committed batches stay), in words.
    pub left_behind: String,
    /// The re-compare of the applied pairs (a new compare id); `None` after a failure or
    /// cancel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recompare: Option<DataCompareResult>,
    /// Every applied pair is in sync now (for the kinds that were applied).
    pub in_sync: bool,
    pub elapsed_ms: u64,
}

/// What a data apply reports, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DataApplyEvent {
    Started {
        apply_id: OperationId,
        total_statements: u64,
    },
    /// Lossy.
    Progress {
        statements_done: u64,
        total_statements: u64,
        pair: usize,
        elapsed_ms: u64,
    },
    /// The re-compare started.
    Recomparing,
    Finished {
        report: DataApplyReport,
    },
    Failed {
        error: ErrorInfo,
        report: DataApplyReport,
    },
    Cancelled {
        report: DataApplyReport,
    },
}

impl DataApplyEvent {
    /// `true` for `Finished`, `Failed` and `Cancelled`.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Finished { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }
}

/// A started data compare or apply: its id and its event stream.
#[derive(Debug)]
pub struct DataRunHandle<E> {
    pub id: OperationId,
    pub events: mpsc::Receiver<E>,
}

/// Data compare, script and apply. See the module docs.
pub struct DataSyncService {
    manager: Arc<ConnectionManager>,
    tree: Arc<TreeService>,
    dialects: Arc<DialectRegistry>,
    runs: RunRegistry,
}

impl DataSyncService {
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
        }
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

    /// Starts a data compare; fails before starting when a pair names no table or the
    /// engines differ.
    pub async fn start_compare(
        &self,
        request: DataCompareRequest,
    ) -> Result<DataRunHandle<DataCompareEvent>> {
        let _ = (&self.runs, request);
        // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
        std::future::ready(()).await;
        unimplemented!("m8: data-sync")
    }

    /// The script preview of a kept compare. Changes nothing.
    pub async fn script(
        &self,
        request: &DataScriptRequest,
        cancel: CancellationToken,
    ) -> Result<DataScript> {
        let _ = (&self.runs, request, cancel);
        // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
        std::future::ready(()).await;
        unimplemented!("m8: data-sync")
    }

    /// Starts applying a kept compare (module docs, *Data compare and apply*).
    pub async fn start_apply(
        &self,
        request: DataApplyRequest,
    ) -> Result<DataRunHandle<DataApplyEvent>> {
        let _ = (&self.runs, request);
        // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
        std::future::ready(()).await;
        unimplemented!("m8: data-sync")
    }

    /// Drops a kept compare (and its spill files); `false` when it is unknown.
    pub fn forget(&self, compare_id: OperationId) -> bool {
        let _ = (&self.runs, compare_id);
        unimplemented!("m8: data-sync")
    }

    /// Cancels a running compare or apply; `false` when it is unknown or already ended.
    pub fn cancel(&self, id: OperationId) -> bool {
        self.runs.cancel(id)
    }

    /// Cancels every compare and apply that uses `session` (closing it).
    pub fn cancel_session(&self, session: SessionId) -> usize {
        self.runs.cancel_session(session)
    }
}

impl std::fmt::Debug for DataSyncService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataSyncService")
            .field("runs", &self.runs)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn payloads_are_camel_case() {
        let request = StructureCompareRequest {
            source_session: SessionId::new(),
            source: ObjectRef::schema(Some("oxbase_test"), "m8_src"),
            target_session: SessionId::new(),
            target: ObjectRef::schema(Some("oxbase_test"), "m8_tgt"),
            options: StructureOptions::default(),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["source"]["name"], "m8_src");
        assert_eq!(json["options"]["includeDrops"], false);
        assert_eq!(
            serde_json::from_value::<StructureCompareRequest>(json).unwrap(),
            request
        );

        let apply: StructureApplyRequest =
            serde_json::from_value(serde_json::json!({"compareId": OperationId::new()})).unwrap();
        assert_eq!(apply.selection, Vec::<String>::new());
        assert!(!apply.confirm_destructive);

        let started = DataCompareEvent::Started {
            compare_id: OperationId::new(),
            tables: 3,
        };
        let json = serde_json::to_value(&started).unwrap();
        assert_eq!(json["type"], "started");
        assert!(json.get("compareId").is_some());
        assert!(!started.is_terminal());
        assert_eq!(
            serde_json::to_value(DataApplyEvent::Recomparing).unwrap(),
            serde_json::json!({"type": "recomparing"})
        );
        let data: DataApplyRequest = serde_json::from_value(
            serde_json::json!({"compareId": OperationId::new(), "options": {"confirmDeletes": true}}),
        )
        .unwrap();
        assert!(data.options.confirm_deletes && !data.options.script.deletes);
    }
}
