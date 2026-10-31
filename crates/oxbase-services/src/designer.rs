//! The table designer service (M3): load a table into a [`TableDesign`], preview the DDL of an
//! edited design, apply it, reload. The model, validation and preview are pure functions of
//! `oxbase-designer`; this module adds sessions, catalog reads and execution.
//!
//! # Sessions
//!
//! Reads (load, the conflict check, triggers, reload) go through `TreeService::load` /
//! `TreeService::session_for`, so a PostgreSQL table of another database is read on that
//! database's metadata session. The apply runs on a **dedicated child session** opened on the
//! table's catalog (`ConnectionManager::open_child`) and closed afterwards, so its session
//! settings (SQLite `PRAGMA foreign_keys`, an aborted transaction) never leak into the tree or
//! an editor. On a private in-memory SQLite database, which cannot have children, it runs on
//! the base session under `SessionHandle::exclusive`; the script's capture/only-if steps
//! restore what they changed.
//!
//! # Preview
//!
//! [`DesignerService::preview`] resolves the engine's `Table` `DdlGenerator`, `Comparator`,
//! `Quoting` and `Capabilities::designer` from the registry (a missing one is `Unsupported`:
//! the designer is read-only on that engine), loads the table's triggers (the `Trigger`
//! fetcher, when registered) and calls `oxbase_designer::preview`.
//!
//! # Apply
//!
//! [`DesignerService::apply`]:
//!
//! 1. builds the preview; refuses (`InvalidInput`) when it has error issues, when it is
//!    destructive and [`DesignApplyOptions::confirm_destructive`] is not set, or when the script
//!    is empty;
//! 2. reloads the table and compares it with `design.original` through the engine's
//!    comparator: any difference is `InvalidInput` "the table changed since it was loaded;
//!    reload it" (nothing runs). A new table must not exist yet;
//! 3. runs the script with [`execute_script`] on the apply session: prepare steps, the main
//!    steps in one transaction when the preview is atomic (else one by one, stopping at the
//!    first failure), finally steps always;
//! 4. reloads the table into a fresh [`TableDesign`] for the UI (also after a failure, when the
//!    script was not atomic, since some steps may have been applied).
//!
//! Owner of the bodies marked `unimplemented!("m3: services")`: the **services** stream.

use std::sync::Arc;

use oxbase_designer::{DesignPreview, TableDesign};
use oxbase_dialect::{DesignerCapabilities, DialectRegistry};
use oxbase_types::{Engine, Error, ObjectRef, Result, Script, ServerVersion};
use serde::{Deserialize, Serialize};

use crate::query::ErrorInfo;
use crate::tree::TreeService;
use crate::{CancellationToken, ConnectionManager, SessionHandle, SessionId};

/// What an open session's engine offers (the UI's `capabilities_get`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineCapabilities {
    pub engine: Engine,
    pub server_version: ServerVersion,
    /// What the table designer may offer.
    pub designer: DesignerCapabilities,
    /// The engine has a `DmlWriter`: table data with a key can be edited.
    pub grid_editing: bool,
    /// `ORDER BY ... NULLS FIRST / LAST` is native (else the filter builder's sort emulates it).
    pub nulls_ordering: bool,
}

/// Options of [`DesignerService::apply`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DesignApplyOptions {
    /// The user confirmed a destructive script (drops, rebuilds, narrowing changes).
    pub confirm_destructive: bool,
}

/// What happened to one script step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StepStatus {
    /// Ran (and, inside a transaction, was committed).
    Done,
    /// Failed (the error says why).
    Failed,
    /// Its `only_if` condition was false.
    Skipped,
    /// Ran inside the transaction, which was rolled back.
    RolledBack,
    /// Did not run (an earlier step failed).
    NotRun,
}

/// One step's outcome, in execution order (`Script::ordered`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepOutcome {
    /// Index in `Script::steps`.
    pub index: usize,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
    pub elapsed_ms: u64,
}

/// What [`execute_script`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptOutcome {
    /// Every main step ran and was committed.
    pub committed: bool,
    /// The main steps ran in one transaction.
    pub atomic: bool,
    pub steps: Vec<StepOutcome>,
    /// Failures of finally steps, and other things the user should know.
    pub warnings: Vec<String>,
}

/// The result of [`DesignerService::apply`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesignApplyReport {
    pub outcome: ScriptOutcome,
    /// The table as it is now (reloaded), when it exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design: Option<TableDesign>,
}

/// Loads, previews and applies table designs. See the module docs.
pub struct DesignerService {
    manager: Arc<ConnectionManager>,
    tree: Arc<TreeService>,
    dialects: Arc<DialectRegistry>,
}

// The async bodies below are skeletons until the services stream fills them.
#[allow(clippy::unused_async)]
impl DesignerService {
    pub fn new(
        manager: Arc<ConnectionManager>,
        tree: Arc<TreeService>,
        dialects: Arc<DialectRegistry>,
    ) -> Self {
        Self {
            manager,
            tree,
            dialects,
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

    /// What the engine of open session `session` offers, at its server version. Engines that
    /// registered nothing get a read-only designer and no grid editing. `NotFound` for an
    /// unknown session.
    pub fn capabilities(&self, session: SessionId) -> Result<EngineCapabilities> {
        let handle = self
            .manager
            .get(session)
            .ok_or_else(|| Error::not_found(format!("no open session {session}")))?;
        let engine = handle.summary().engine;
        let server_version = handle.info().server_version.clone();
        let designer = self
            .dialects
            .capabilities(engine)
            .map(|caps| caps.designer(&server_version))
            .unwrap_or_default();
        Ok(EngineCapabilities {
            engine,
            designer,
            grid_editing: self.dialects.dml_writer(engine).is_ok(),
            nulls_ordering: self
                .dialects
                .quoting(engine)
                .is_ok_and(|quoting| quoting.nulls_ordering()),
            server_version,
        })
    }

    /// The design of table `table` (`TableDesign::from_def` of its current definition).
    pub async fn load(
        &self,
        session: SessionId,
        table: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<TableDesign> {
        let _ = (self, session, table, cancel);
        unimplemented!("m3: services")
    }

    /// The preview of `design` (module docs, *Preview*).
    pub async fn preview(
        &self,
        session: SessionId,
        design: &TableDesign,
        cancel: CancellationToken,
    ) -> Result<DesignPreview> {
        let _ = (self, session, design, cancel);
        unimplemented!("m3: services")
    }

    /// Applies `design` (module docs, *Apply*). `Err` when nothing ran (refused, conflict,
    /// session failure before the first step); a failing step is reported in the outcome.
    pub async fn apply(
        &self,
        session: SessionId,
        design: &TableDesign,
        options: DesignApplyOptions,
        cancel: CancellationToken,
    ) -> Result<DesignApplyReport> {
        let _ = (self, session, design, options, cancel);
        unimplemented!("m3: services")
    }
}

impl std::fmt::Debug for DesignerService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesignerService").finish_non_exhaustive()
    }
}

/// Runs `script` on `session` (holding `SessionHandle::exclusive`) with the semantics of the
/// `oxbase_types::script` module docs: prepare steps, then the main steps in one transaction
/// (`Connection::begin`) when `atomic` (else each on its own, stopping at the first failure),
/// then finally steps whatever happened; captures, `only_if` conditions and `expect_no_rows`
/// checks (the failing check's first rows go into its error). Cancellation stops before the
/// next step and rolls the transaction back; finally steps still run.
#[allow(clippy::unused_async)]
pub async fn execute_script(
    session: &SessionHandle,
    script: &Script,
    atomic: bool,
    cancel: CancellationToken,
) -> Result<ScriptOutcome> {
    let _ = (session, script, atomic, cancel);
    unimplemented!("m3: services")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn payloads_are_camel_case() {
        let caps = EngineCapabilities {
            engine: Engine::MariaDb,
            server_version: ServerVersion::new(11, 8, 9),
            designer: DesignerCapabilities::read_only(),
            grid_editing: true,
            nulls_ordering: false,
        };
        let json = serde_json::to_value(&caps).unwrap();
        assert_eq!(json["engine"], "mariadb");
        assert_eq!(json["serverVersion"]["major"], 11);
        assert_eq!(json["gridEditing"], true);
        assert_eq!(json["designer"]["addColumn"], "unsupported");
        let options: DesignApplyOptions =
            serde_json::from_value(serde_json::json!({"confirmDestructive": true})).unwrap();
        assert!(options.confirm_destructive);
        assert!(!DesignApplyOptions::default().confirm_destructive);
        let outcome = ScriptOutcome {
            committed: false,
            atomic: true,
            steps: vec![StepOutcome {
                index: 2,
                status: StepStatus::RolledBack,
                error: None,
                elapsed_ms: 4,
            }],
            warnings: vec![],
        };
        let json = serde_json::to_value(&outcome).unwrap();
        assert_eq!(json["steps"][0]["status"], "rolledBack");
        assert_eq!(json["steps"][0]["elapsedMs"], 4);
    }
}
