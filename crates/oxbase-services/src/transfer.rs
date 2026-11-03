//! Data transfer service (M8, `docs/m8-contracts.md` §4.5, §8): plans and runs
//! `oxbase_transfer` transfers between two open connections, with an event stream, cancel,
//! and "closing a connection cancels its transfers". Owner of the bodies marked
//! `unimplemented!("m8: transfer")`: the **transfer** stream.
//!
//! # Sessions
//!
//! A request names two **base** sessions (`ConnectionManager::open`; the same session twice is
//! allowed: a transfer between two schemas of one server). The service opens dedicated child
//! sessions (`ConnectionManager::open_child`) on the source object's database and on the target
//! container's database: one for catalog reads and the source rows, one for the target DDL and
//! loads; closed when the run ends. The tree and the editors stay usable meanwhile.
//!
//! # Run
//!
//! [`TransferService::start`] plans first (fails before starting like
//! [`TransferService::plan`], also when the plan is not `runnable`, or is `destructive`
//! without [`TransferRequest::confirm_destructive`]), registers the run on both base sessions
//! ([`crate::runs::RunRegistry`]) and spawns it: tables in plan order through
//! `oxbase_transfer::pipeline::copy_table`, then the planned foreign keys and identity
//! resyncs, then the terminal event. With `ErrorMode::Stop` the first failed table ends the
//! run (`Failed`); with `Continue` every table runs and the run ends `Finished` with failed
//! tables in the report. A cancel ends the run with `Cancelled` within 2 s.
//!
//! # Events
//!
//! `Started`, then per table `TableStarted`, `Progress` (lossy, `try_send`), `RowError` (the
//! first 100 per table), `TableFinished`; then exactly one of `Finished`, `Failed`,
//! `Cancelled`. Schema changes on the target (created / replaced tables) are announced on
//! [`SchemaEvents`] like the import's.

use std::sync::Arc;

use oxbase_dialect::DialectRegistry;
use oxbase_types::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub use oxbase_transfer::convert::RowIssue;
pub use oxbase_transfer::{
    ColumnOverride, ColumnPlan, ErrorMode, FkChecks, IdentityMode, PairReport, SourceObject,
    TableMode, TablePlan, TableProgress, TableReport, TableStatus, TransactionScope,
    TransferOptions, TransferPlan, TransferReport, TransferSpec,
};

use crate::query::ErrorInfo;
use crate::runs::{OperationId, RunRegistry};
use crate::schema_cache::SchemaEvents;
use crate::tree::TreeService;
use crate::{CancellationToken, ConnectionManager, SessionId};

/// A transfer between two open connections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferRequest {
    /// Base session the source objects are read from.
    pub source_session: SessionId,
    /// Base session the tables are written to.
    pub target_session: SessionId,
    pub spec: TransferSpec,
    /// The user confirmed a destructive plan (`replace`, `truncate`).
    #[serde(default)]
    pub confirm_destructive: bool,
}

/// What a transfer reports, in order (module docs, *Events*).
///
/// JSON: `{"type": "<camelCase variant>", ...camelCase fields}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TransferEvent {
    Started {
        transfer_id: OperationId,
        plan: TransferPlan,
    },
    TableStarted {
        table: usize,
    },
    /// Cumulative counters of the running table (lossy).
    Progress {
        progress: TableProgress,
    },
    RowError {
        table: usize,
        issue: RowIssue,
    },
    TableFinished {
        report: TableReport,
    },
    Finished {
        report: TransferReport,
    },
    Failed {
        error: ErrorInfo,
        report: TransferReport,
    },
    Cancelled {
        report: TransferReport,
    },
}

impl TransferEvent {
    /// `true` for `Finished`, `Failed` and `Cancelled`.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Finished { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }
}

/// A started transfer: its id and its event stream.
#[derive(Debug)]
pub struct TransferHandle {
    pub id: OperationId,
    pub events: mpsc::Receiver<TransferEvent>,
}

/// Plans and runs transfers. See the module docs.
pub struct TransferService {
    manager: Arc<ConnectionManager>,
    tree: Arc<TreeService>,
    dialects: Arc<DialectRegistry>,
    runs: RunRegistry,
    schema_events: Option<SchemaEvents>,
}

impl TransferService {
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

    /// Announces created or replaced target tables on `events` (M6 schema cache).
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

    /// The running transfers.
    pub fn runs(&self) -> &RunRegistry {
        &self.runs
    }

    /// The plan of `request` (also the dry run: target DDL and mapping report). Reads catalogs
    /// only.
    pub async fn plan(
        &self,
        request: &TransferRequest,
        cancel: CancellationToken,
    ) -> Result<TransferPlan> {
        let _ = (&self.schema_events, request, cancel);
        // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
        std::future::ready(()).await;
        unimplemented!("m8: transfer")
    }

    /// The type mapping report of an engine pair (the wizard's mapping help), for the engines
    /// of two open sessions.
    pub fn pair_report(
        &self,
        source_session: SessionId,
        target_session: SessionId,
    ) -> Result<PairReport> {
        let _ = (&self.manager, source_session, target_session);
        unimplemented!("m8: transfer")
    }

    /// Starts `request` (module docs, *Run*); events follow on the handle.
    pub async fn start(&self, request: TransferRequest) -> Result<TransferHandle> {
        let _ = (&self.runs, request);
        // Keeps the skeleton an `async fn` (clippy::unused_async) until the body lands.
        std::future::ready(()).await;
        unimplemented!("m8: transfer")
    }

    /// Cancels transfer `id`; `false` when it is unknown or already ended.
    pub fn cancel(&self, id: OperationId) -> bool {
        self.runs.cancel(id)
    }

    /// Cancels every transfer that reads or writes through `session` (closing it).
    pub fn cancel_session(&self, session: SessionId) -> usize {
        self.runs.cancel_session(session)
    }

    /// `true` while transfer `id` runs.
    pub fn is_running(&self, id: OperationId) -> bool {
        self.runs.is_running(id)
    }
}

impl std::fmt::Debug for TransferService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferService")
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
    fn requests_and_events_are_camel_case() {
        let request = TransferRequest {
            source_session: SessionId::new(),
            target_session: SessionId::new(),
            spec: TransferSpec {
                objects: vec![SourceObject::Query {
                    sql: "SELECT 1".into(),
                    target_name: "t".into(),
                }],
                target: ObjectRef::database("m8_xfer_target"),
                options: TransferOptions::default(),
                overrides: Vec::new(),
            },
            confirm_destructive: false,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert!(json.get("sourceSession").is_some() && json.get("targetSession").is_some());
        assert_eq!(json["spec"]["objects"][0]["type"], "query");
        assert_eq!(
            serde_json::from_value::<TransferRequest>(json).unwrap(),
            request
        );

        let event = TransferEvent::TableStarted { table: 2 };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::json!({"type": "tableStarted", "table": 2})
        );
        assert!(!event.is_terminal());
        let done = TransferEvent::Finished {
            report: TransferReport {
                tables: Vec::new(),
                foreign_keys_added: 0,
                warnings: Vec::new(),
                elapsed_ms: 5,
            },
        };
        assert!(done.is_terminal());
        assert_eq!(
            serde_json::to_value(&done).unwrap()["report"]["elapsedMs"],
            5
        );
    }
}
