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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use oxbase_designer::{
    DesignIssue, DesignPreview, IssueSeverity, PreviewInput, TableDesign, compatible_types,
    references_itself,
};
use oxbase_dialect::{DatabaseScope, DesignerCapabilities, DialectRegistry};
use oxbase_driver::{Collected, ResultSet, Tx, collect};
use oxbase_types::{
    Engine, Error, ErrorCategory, ObjectDef, ObjectKind, ObjectRef, Result, Script, ScriptStep,
    ServerVersion, StepCheck, StepPhase, TableDef, TriggerDef,
};
use serde::{Deserialize, Serialize};

use crate::convert::cell_text;
use crate::query::ErrorInfo;
use crate::tree::TreeService;
use crate::{
    CancellationToken, Connection, ConnectionManager, SessionHandle, SessionId, check_cancelled,
    until_cancelled,
};

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
        let designer = match self.dialects.capabilities(engine) {
            Ok(caps) => caps.designer(&server_version),
            Err(_) => DesignerCapabilities::read_only(),
        };
        let grid_editing = self.dialects.dml_writer(engine).is_ok();
        let nulls_ordering = match self.dialects.quoting(engine) {
            Ok(quoting) => quoting.nulls_ordering(),
            Err(_) => false,
        };
        Ok(EngineCapabilities {
            engine,
            server_version,
            designer,
            grid_editing,
            nulls_ordering,
        })
    }

    /// The design of table `table` (`TableDesign::from_def` of its current definition).
    pub async fn load(
        &self,
        session: SessionId,
        table: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<TableDesign> {
        if table.kind != ObjectKind::Table {
            return Err(Error::invalid_input(format!(
                "{} is not a table",
                table.display_name()
            )));
        }
        match self.tree.load(session, table, cancel).await? {
            ObjectDef::Table(def) => Ok(TableDesign::from_def(def)),
            other => Err(Error::invalid_input(format!(
                "{} is not a table",
                other.object_ref().display_name()
            ))),
        }
    }

    /// The preview of `design` (module docs, *Preview*).
    pub async fn preview(
        &self,
        session: SessionId,
        design: &TableDesign,
        cancel: CancellationToken,
    ) -> Result<DesignPreview> {
        check_cancelled(&cancel)?;
        let base = self
            .manager
            .get(session)
            .ok_or_else(|| Error::not_found(format!("no open session {session}")))?;
        let engine = base.summary().engine;
        let version = base.info().server_version.clone();
        let ddl = self.dialects.ddl(engine, ObjectKind::Table)?;
        let comparator = self.dialects.comparator(engine, ObjectKind::Table)?;
        let quoting = self.dialects.quoting(engine)?;
        let capabilities = self.dialects.capabilities(engine)?.designer(&version);
        let triggers = match &design.original {
            Some(original) => self.triggers(session, &original.obj, &cancel).await?,
            None => Vec::new(),
        };
        let reference_issues = self.reference_issues(session, design, &cancel).await?;
        let input = PreviewInput {
            ddl: ddl.as_ref(),
            comparator: comparator.as_ref(),
            quoting: quoting.as_ref(),
            capabilities: &capabilities,
            version: &version,
            triggers: &triggers,
            terminator: ";",
        };
        let mut preview = oxbase_designer::preview(design, &input)?;
        if !reference_issues.is_empty() {
            preview.issues.extend(reference_issues);
            if preview.has_errors() {
                // An error blocks the script.
                preview.script = Script::new();
                preview.sql = String::new();
                preview.atomic = false;
                preview.destructive = false;
            }
        }
        check_cancelled(&cancel)?;
        Ok(preview)
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
        let preview = self.preview(session, design, cancel.clone()).await?;
        self.apply_preview(session, design, &preview, &options, cancel)
            .await
    }

    /// Steps 1-4 of the apply with an already built preview.
    async fn apply_preview(
        &self,
        session: SessionId,
        design: &TableDesign,
        preview: &DesignPreview,
        options: &DesignApplyOptions,
        cancel: CancellationToken,
    ) -> Result<DesignApplyReport> {
        // 1. Refuse what must not run.
        if preview.has_errors() {
            let messages: Vec<&str> = preview
                .issues
                .iter()
                .filter(|issue| issue.severity == IssueSeverity::Error)
                .map(|issue| issue.message.as_str())
                .collect();
            return Err(Error::invalid_input(format!(
                "the design has errors: {}",
                messages.join("; ")
            )));
        }
        if preview.script.is_empty() {
            return Err(Error::invalid_input(
                "nothing to apply: the design has no changes",
            ));
        }
        if preview.destructive && !options.confirm_destructive {
            return Err(Error::invalid_input(
                "the change can lose data (it drops or rebuilds something); confirm it to apply",
            ));
        }
        let base = self
            .manager
            .get(session)
            .ok_or_else(|| Error::not_found(format!("no open session {session}")))?;
        let engine = base.summary().engine;

        // 2. The table must still be what the design was loaded from.
        let comparator = self.dialects.comparator(engine, ObjectKind::Table)?;
        match &design.original {
            Some(original) => {
                let current = match self.tree.load(session, &original.obj, cancel.clone()).await {
                    Ok(current) => current,
                    Err(e) if e.category() == ErrorCategory::NotFound => {
                        return Err(Error::invalid_input(format!(
                            "{} no longer exists; reload it",
                            original.obj.display_name()
                        )));
                    }
                    Err(e) => return Err(e),
                };
                let changed = !matches!(current, ObjectDef::Table(_))
                    || !comparator
                        .diff(&ObjectDef::Table(original.clone()), &current)
                        .is_empty();
                if changed {
                    return Err(Error::invalid_input(CHANGED_SINCE_LOADED));
                }
            }
            None => match self.tree.load(session, &design.table, cancel.clone()).await {
                Ok(_) => {
                    return Err(Error::invalid_input(format!(
                        "{} already exists",
                        design.table.display_name()
                    )));
                }
                Err(e) if e.category() == ErrorCategory::NotFound => {}
                Err(e) => return Err(e),
            },
        }

        // 3. Run on a dedicated session.
        let (apply_session, dedicated) = self.apply_session(&base, &design.table, &cancel).await?;
        let outcome = execute_script(
            &apply_session,
            &preview.script,
            preview.atomic,
            cancel.clone(),
        )
        .await;
        if dedicated {
            match self.manager.close(apply_session.id()).await {
                Ok(()) => {}
                Err(e) if e.category() == ErrorCategory::NotFound => {}
                Err(e) => tracing::debug!(error = %e, "closing the designer's session failed"),
            }
        }
        let mut outcome = outcome?;

        // 4. Reload what is there now.
        let reload = outcome.committed || !outcome.atomic;
        let design = if reload {
            // A cancelled apply still reloads: the token only stopped the script.
            match self
                .load(session, &design.table, CancellationToken::new())
                .await
            {
                Ok(reloaded) => Some(reloaded),
                Err(e) => {
                    outcome
                        .warnings
                        .push(format!("the table could not be reloaded: {e}"));
                    None
                }
            }
        } else {
            None
        };
        drop(cancel);
        Ok(DesignApplyReport { outcome, design })
    }

    /// The session the apply runs on: a new child of `base` on the table's database, or (for
    /// a private in-memory database, whose children would not see it) the base itself. The
    /// flag says whether it must be closed afterwards.
    async fn apply_session(
        &self,
        base: &SessionHandle,
        table: &ObjectRef,
        cancel: &CancellationToken,
    ) -> Result<(SessionHandle, bool)> {
        if !self.manager.children_share_data(base.id()) {
            return Ok((base.clone(), false));
        }
        let per_database = self
            .dialects
            .tree_layout(base.summary().engine)
            .is_ok_and(|layout| layout.scope == DatabaseScope::Session);
        let database = if per_database {
            table.database_name()
        } else {
            None
        };
        let child = self
            .manager
            .open_child(base.id(), database, cancel.clone())
            .await?;
        Ok((child, true))
    }

    /// The triggers on `table` (with their definitions), for engines whose rebuild re-creates
    /// them. Empty when the engine has no trigger fetcher.
    async fn triggers(
        &self,
        session: SessionId,
        table: &ObjectRef,
        cancel: &CancellationToken,
    ) -> Result<Vec<TriggerDef>> {
        let base = self
            .manager
            .get(session)
            .ok_or_else(|| Error::not_found(format!("no open session {session}")))?;
        let Ok(fetcher) = self
            .dialects
            .fetcher(base.summary().engine, ObjectKind::Trigger)
        else {
            return Ok(Vec::new());
        };
        let reader = self
            .tree
            .session_for(session, table.database_name(), cancel.clone())
            .await?;
        let listed = {
            let _guard = until_cancelled(cancel, async { Ok(reader.exclusive().await) }).await?;
            fetcher
                .list(reader.connection().as_ref(), table, cancel.clone())
                .await?
        };
        let mut out = Vec::with_capacity(listed.len());
        for summary in listed {
            let _guard = until_cancelled(cancel, async { Ok(reader.exclusive().await) }).await?;
            if let ObjectDef::Trigger(trigger) = fetcher
                .load(reader.connection().as_ref(), &summary.obj, cancel.clone())
                .await?
            {
                out.push(trigger);
            }
        }
        Ok(out)
    }

    /// Issues about what the design's foreign keys reference on the server: the table exists,
    /// has the referenced columns, and their types fit the referencing columns. New and
    /// changed foreign keys only (an unchanged one was accepted by the server already);
    /// references to the table itself are checked by `oxbase_designer::validate`.
    async fn reference_issues(
        &self,
        session: SessionId,
        design: &TableDesign,
        cancel: &CancellationToken,
    ) -> Result<Vec<DesignIssue>> {
        let mut issues = Vec::new();
        let mut loaded: Vec<(ObjectRef, Option<TableDef>)> = Vec::new();
        for (pos, item) in design.foreign_keys.iter().enumerate() {
            let fk = &item.def;
            let unchanged = item
                .origin
                .and_then(|o| design.original.as_ref().and_then(|t| t.foreign_keys.get(o)))
                .is_some_and(|old| {
                    old.ref_table == fk.ref_table
                        && old.ref_columns == fk.ref_columns
                        && old.columns == fk.columns
                });
            if unchanged || fk.ref_table.name.trim().is_empty() || references_itself(design, fk) {
                continue;
            }
            let path = format!("foreignKeys[{pos}]");
            let mut target = fk.ref_table.clone();
            target.kind = ObjectKind::Table;
            if target.catalog.is_none() {
                target.catalog.clone_from(&design.table.catalog);
            }
            if target.schema.is_none() {
                target.schema.clone_from(&design.table.schema);
            }
            let cached = loaded
                .iter()
                .find(|(r, _)| *r == target)
                .map(|(_, d)| d.clone());
            let def = match cached {
                Some(def) => def,
                None => {
                    let def = match self.tree.load(session, &target, cancel.clone()).await {
                        Ok(ObjectDef::Table(def)) => Some(def),
                        Ok(_) => None,
                        Err(e) if e.category() == ErrorCategory::NotFound => None,
                        Err(e) => return Err(e),
                    };
                    loaded.push((target.clone(), def.clone()));
                    def
                }
            };
            let Some(def) = def else {
                issues.push(DesignIssue::error(
                    format!("{path}.refTable"),
                    format!(
                        "the referenced table {} does not exist",
                        target.display_name()
                    ),
                ));
                continue;
            };
            for (k, (from, to)) in fk.columns.iter().zip(&fk.ref_columns).enumerate() {
                let Some(referenced) = def.column(to) else {
                    issues.push(DesignIssue::error(
                        format!("{path}.refColumns[{k}]"),
                        format!("{} has no column '{to}'", target.display_name()),
                    ));
                    continue;
                };
                if let Some(source) = design.columns.iter().find(|c| c.column.name == *from)
                    && !compatible_types(&source.column.data_type, &referenced.data_type)
                {
                    issues.push(DesignIssue::error(
                        format!("{path}.refColumns[{k}]"),
                        format!(
                            "'{from}' ({}) cannot reference {}.{to} ({})",
                            source.column.data_type,
                            target.display_name(),
                            referenced.data_type
                        ),
                    ));
                }
            }
        }
        Ok(issues)
    }
}

/// Why an apply was refused when the table on the server differs from the loaded one.
pub const CHANGED_SINCE_LOADED: &str = "the table changed since it was loaded; reload it";

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
pub async fn execute_script(
    session: &SessionHandle,
    script: &Script,
    atomic: bool,
    cancel: CancellationToken,
) -> Result<ScriptOutcome> {
    let _guard = until_cancelled(&cancel, async { Ok(session.exclusive().await) }).await?;
    let connection = session.connection().as_ref();
    let mut run = ScriptRun {
        captures: HashMap::new(),
        outcomes: Vec::with_capacity(script.steps.len()),
        warnings: script.warnings.clone(),
        failed: false,
    };
    let by_phase = |phase: StepPhase| -> Vec<usize> {
        script
            .steps
            .iter()
            .enumerate()
            .filter(|(_, step)| step.phase == phase)
            .map(|(index, _)| index)
            .collect()
    };

    // 1. Prepare, outside any transaction.
    for index in by_phase(StepPhase::Prepare) {
        run.step(Runner::Connection(connection), script, index, &cancel)
            .await;
    }

    // 2. Main.
    let main = by_phase(StepPhase::Main);
    let mut committed = false;
    if run.failed {
        run.not_run(&main);
    } else if atomic && !main.is_empty() {
        match connection.begin().await {
            Err(e) => {
                run.fail_all(&main, &e);
            }
            Ok(tx) => {
                let first = run.outcomes.len();
                for &index in &main {
                    if run.failed {
                        run.not_run(&[index]);
                    } else {
                        run.step(Runner::Tx(tx.as_ref()), script, index, &cancel)
                            .await;
                    }
                }
                if run.failed {
                    if let Err(e) = tx.rollback().await {
                        run.warnings
                            .push(format!("rolling the transaction back failed: {e}"));
                    }
                    run.rolled_back(first);
                } else {
                    match tx.commit().await {
                        Ok(()) => committed = true,
                        Err(e) => {
                            run.failed = true;
                            run.rolled_back(first);
                            if let Some(last) = run.outcomes.last_mut() {
                                last.status = StepStatus::Failed;
                                last.error = Some(ErrorInfo::from(&Error::sql(
                                    None::<String>,
                                    format!("committing the changes failed: {e}"),
                                )));
                            }
                        }
                    }
                }
            }
        }
    } else {
        for &index in &main {
            if run.failed {
                run.not_run(&[index]);
            } else {
                run.step(Runner::Connection(connection), script, index, &cancel)
                    .await;
            }
        }
        committed = !run.failed;
    }

    // 3. Finally steps always run (also after a cancel), and only warn.
    let main_failed = run.failed;
    let always = CancellationToken::new();
    for index in by_phase(StepPhase::Finally) {
        run.failed = false;
        run.step(Runner::Connection(connection), script, index, &always)
            .await;
        if run.failed
            && let Some(error) = run.outcomes.last().and_then(|o| o.error.as_ref())
        {
            run.warnings
                .push(format!("a clean-up step failed: {}", error.message));
        }
    }
    drop(cancel);
    Ok(ScriptOutcome {
        committed: committed && !main_failed,
        atomic: atomic && script.is_atomic(),
        steps: run.outcomes,
        warnings: run.warnings,
    })
}

/// Where a step runs: on the connection (auto-commit) or inside the script's transaction.
#[derive(Clone, Copy)]
enum Runner<'a> {
    Connection(&'a dyn Connection),
    Tx(&'a dyn Tx),
}

impl Runner<'_> {
    async fn execute(self, sql: &str, cancel: &CancellationToken) -> Result<Collected> {
        let stream = match self {
            Self::Connection(c) => c.execute(sql, &[], cancel.clone()).await?,
            Self::Tx(tx) => tx.execute(sql, &[], cancel.clone()).await?,
        };
        collect(stream).await
    }
}

/// The state of one `execute_script`.
struct ScriptRun {
    captures: HashMap<String, String>,
    outcomes: Vec<StepOutcome>,
    warnings: Vec<String>,
    /// A step failed (in the current phase).
    failed: bool,
}

impl ScriptRun {
    /// Runs step `index` (or skips it), recording its outcome.
    async fn step(
        &mut self,
        runner: Runner<'_>,
        script: &Script,
        index: usize,
        cancel: &CancellationToken,
    ) {
        let step = &script.steps[index];
        if let Some(condition) = &step.only_if
            && self.captures.get(&condition.capture).map(String::as_str)
                != Some(condition.equals.as_str())
        {
            self.outcomes.push(StepOutcome {
                index,
                status: StepStatus::Skipped,
                error: None,
                elapsed_ms: 0,
            });
            return;
        }
        let started = Instant::now();
        let result = if cancel.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            match runner.execute(&step.sql, cancel).await {
                Ok(collected) => self.check(step, &collected),
                Err(e) => Err(e),
            }
        };
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (status, error) = match result {
            Ok(()) => (StepStatus::Done, None),
            Err(e) => {
                self.failed = true;
                (StepStatus::Failed, Some(ErrorInfo::from(&e)))
            }
        };
        self.outcomes.push(StepOutcome {
            index,
            status,
            error,
            elapsed_ms,
        });
    }

    /// Captures the step's value and checks `expect_no_rows`.
    fn check(&mut self, step: &ScriptStep, collected: &Collected) -> Result<()> {
        if let Some(name) = &step.capture {
            let value = collected
                .result_sets
                .first()
                .and_then(|set| set.batches.iter().find(|b| b.num_rows() > 0))
                .and_then(|batch| {
                    let column = batch.columns().first()?;
                    cell_text(column.as_ref(), 0).ok().flatten()
                })
                .unwrap_or_default();
            self.captures.insert(name.clone(), value);
        }
        if step.check == StepCheck::ExpectNoRows {
            let rows: usize = collected.result_sets.iter().map(ResultSet::num_rows).sum();
            if rows > 0 {
                let what = if step.description.is_empty() {
                    step.sql.as_str()
                } else {
                    step.description.as_str()
                };
                return Err(Error::invalid_input(format!(
                    "the check \"{what}\" found {rows} problem row(s): {}",
                    first_rows(collected, 5)
                )));
            }
        }
        Ok(())
    }

    fn not_run(&mut self, indices: &[usize]) {
        for &index in indices {
            self.outcomes.push(StepOutcome {
                index,
                status: StepStatus::NotRun,
                error: None,
                elapsed_ms: 0,
            });
        }
    }

    fn fail_all(&mut self, indices: &[usize], error: &Error) {
        self.failed = true;
        let mut first = true;
        for &index in indices {
            let (status, error) = if first {
                (StepStatus::Failed, Some(ErrorInfo::from(error)))
            } else {
                (StepStatus::NotRun, None)
            };
            first = false;
            self.outcomes.push(StepOutcome {
                index,
                status,
                error,
                elapsed_ms: 0,
            });
        }
    }

    /// The main steps recorded from `first` on ran inside a transaction that was rolled back.
    fn rolled_back(&mut self, first: usize) {
        for outcome in &mut self.outcomes[first..] {
            if outcome.status == StepStatus::Done {
                outcome.status = StepStatus::RolledBack;
            }
        }
    }
}

/// Up to `limit` rows of the first result set as `col=value, ...; ...`.
fn first_rows(collected: &Collected, limit: usize) -> String {
    let mut rows = Vec::new();
    for set in &collected.result_sets {
        for batch in &set.batches {
            let schema = batch.schema();
            for row in 0..batch.num_rows() {
                if rows.len() == limit {
                    return rows.join("; ");
                }
                let cells: Vec<String> = schema
                    .fields()
                    .iter()
                    .zip(batch.columns())
                    .map(|(field, array)| {
                        let text = cell_text(array.as_ref(), row)
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| "NULL".to_owned());
                        format!("{}={text}", field.name())
                    })
                    .collect();
                rows.push(cells.join(", "));
            }
        }
    }
    rows.join("; ")
}

#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod service_tests;

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
