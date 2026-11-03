//! The transfer run (module docs of `transfer`, *Run*): work sessions, tables in plan order
//! through `oxbase_transfer::pipeline::copy_table`, DDL that commits on its own, clean-up of
//! tables a failed load created, foreign keys and identity resyncs after the load, events.

use std::sync::Arc;
use std::time::{Duration, Instant};

use oxbase_dialect::{BulkLoader, DialectRegistry};
use oxbase_driver::Connection;
use oxbase_sql::DdlVerb;
use oxbase_transfer::pipeline::{
    CopyTable, PlanInputs, Prepared, PreparedTable, atomic_steps, copy_table, prepare, run_steps,
};
use oxbase_types::{BoundStatement, Error, ErrorCategory, Result, ScriptStep, StepPhase};
use tokio::sync::mpsc;

use super::{
    ErrorMode, IdentityMode, TableMode, TableProgress, TableReport, TableStatus, TransferEvent,
    TransferReport, TransferRequest, TransferSpec,
};
use crate::import::{WorkSession, close_work_session, work_session};
use crate::query::ErrorInfo;
use crate::runs::{OperationId, RunTicket};
use crate::schema_cache::{ChangeSource, SchemaEvents, object_event};
use crate::{CancellationToken, ConnectionManager, SessionGuard, SessionHandle};

/// How long closing a work session may take before the run goes on (it closes in the
/// background after that).
const CLOSE_WAIT: Duration = Duration::from_millis(500);
/// How long dropping a table the run created may take.
const DROP_WAIT: Duration = Duration::from_secs(5);
/// Row issues sent as events per table.
const MAX_ROW_ERROR_EVENTS: usize = 100;

/// The two dedicated sessions of a plan or a run.
pub(super) struct Sessions {
    source: WorkSession,
    target: WorkSession,
}

impl Sessions {
    /// Opens the source session on the first source object's database and the target session
    /// on the target container's database.
    pub(super) async fn open(
        manager: &Arc<ConnectionManager>,
        dialects: &DialectRegistry,
        request: &TransferRequest,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let spec = &request.spec;
        let source_db = spec
            .objects
            .iter()
            .find_map(|o| o.source_ref())
            .and_then(|r| r.database_name())
            .map(str::to_owned);
        let target_db = spec.target.database_name().map(str::to_owned);
        let source = work_session(
            manager,
            dialects,
            request.source_session,
            source_db.as_deref(),
            cancel,
        )
        .await?;
        let target = match work_session(
            manager,
            dialects,
            request.target_session,
            target_db.as_deref(),
            cancel,
        )
        .await
        {
            Ok(target) => target,
            Err(e) => {
                close_work_session(manager, source, CLOSE_WAIT).await;
                return Err(e);
            }
        };
        let sessions = Self { source, target };
        if sessions.source.handle.id() == sessions.target.handle.id() {
            sessions.close(manager).await;
            return Err(Error::invalid_input(
                "the source and the target are one private in-memory session: a transfer \
                 needs two sessions",
            ));
        }
        Ok(sessions)
    }

    fn source(&self) -> &SessionHandle {
        &self.source.handle
    }

    fn target(&self) -> &SessionHandle {
        &self.target.handle
    }

    /// Exclusive use of both sessions.
    async fn lock(&self, cancel: &CancellationToken) -> Result<(SessionGuard, SessionGuard)> {
        let source =
            crate::until_cancelled(cancel, async { Ok(self.source().exclusive().await) }).await?;
        let target =
            crate::until_cancelled(cancel, async { Ok(self.target().exclusive().await) }).await?;
        Ok((source, target))
    }

    /// The plan and what the run needs, read on both sessions.
    pub(super) async fn prepare(
        &self,
        dialects: &DialectRegistry,
        spec: &TransferSpec,
        cancel: &CancellationToken,
    ) -> Result<Prepared> {
        let _guards = self.lock(cancel).await?;
        prepare(
            PlanInputs {
                dialects,
                source: self.source().connection().as_ref(),
                target: self.target().connection().as_ref(),
                spec,
            },
            cancel.clone(),
        )
        .await
    }

    pub(super) async fn close(self, manager: &Arc<ConnectionManager>) {
        close_work_session(manager, self.source, CLOSE_WAIT).await;
        close_work_session(manager, self.target, CLOSE_WAIT).await;
    }
}

/// One started transfer.
pub(super) struct Job {
    pub id: OperationId,
    pub request: TransferRequest,
    pub prepared: Prepared,
    pub sessions: Sessions,
    pub manager: Arc<ConnectionManager>,
    pub dialects: Arc<DialectRegistry>,
    pub schema_events: Option<SchemaEvents>,
    pub events: mpsc::Sender<TransferEvent>,
    /// Unregisters the run when dropped (after the terminal event).
    pub ticket: RunTicket,
}

/// How the tables went.
#[derive(Default)]
struct Outcome {
    reports: Vec<TableReport>,
    foreign_keys_added: u32,
    warnings: Vec<String>,
    /// The error that ended the run (`ErrorMode::Stop`).
    error: Option<ErrorInfo>,
    cancelled: bool,
}

impl Job {
    /// Runs the transfer to its terminal event.
    pub(super) async fn run(self) {
        let started = Instant::now();
        let token = self.ticket.token().clone();
        self.send(TransferEvent::Started {
            transfer_id: self.id,
            plan: self.prepared.plan.clone(),
        })
        .await;
        let mut outcome = Outcome {
            reports: self
                .prepared
                .plan
                .tables
                .iter()
                .enumerate()
                .map(|(index, t)| not_run(index, t.target.clone()))
                .collect(),
            warnings: self.prepared.plan.warnings.clone(),
            ..Outcome::default()
        };
        match self.sessions.lock(&token).await {
            Err(e) => {
                if e.is_cancelled() {
                    outcome.cancelled = true;
                } else {
                    outcome.error = Some(ErrorInfo::from(&e));
                }
            }
            Ok(guards) => {
                self.tables(&token, &mut outcome).await;
                drop(guards);
            }
        }
        self.announce(&outcome);
        let report = TransferReport {
            tables: std::mem::take(&mut outcome.reports),
            foreign_keys_added: outcome.foreign_keys_added,
            warnings: std::mem::take(&mut outcome.warnings),
            elapsed_ms: millis(started),
        };
        let event = if outcome.cancelled || token.is_cancelled() {
            TransferEvent::Cancelled { report }
        } else if let Some(error) = outcome.error {
            TransferEvent::Failed { error, report }
        } else {
            TransferEvent::Finished { report }
        };
        let Self {
            sessions,
            manager,
            events,
            ticket,
            ..
        } = self;
        sessions.close(&manager).await;
        let _ = events.send(event).await;
        drop(ticket);
    }

    async fn send(&self, event: TransferEvent) {
        let _ = self.events.send(event).await;
    }

    fn source(&self) -> &dyn Connection {
        self.sessions.source().connection().as_ref()
    }

    fn target(&self) -> &dyn Connection {
        self.sessions.target().connection().as_ref()
    }

    /// Every table in plan order, then the foreign keys and identity resyncs.
    async fn tables(&self, token: &CancellationToken, outcome: &mut Outcome) {
        let plan = &self.prepared.plan;
        let options = &self.request.spec.options;
        let loader = match self.dialects.bulk_loader(plan.target_engine) {
            Ok(loader) => loader,
            Err(e) => {
                outcome.error = Some(ErrorInfo::from(&e));
                return;
            }
        };
        for &index in &plan.order {
            if token.is_cancelled() {
                outcome.cancelled = true;
                break;
            }
            self.send(TransferEvent::TableStarted { table: index })
                .await;
            let report = self.table(index, loader.as_ref(), token).await;
            for issue in report.issues.iter().take(MAX_ROW_ERROR_EVENTS) {
                self.send(TransferEvent::RowError {
                    table: index,
                    issue: issue.clone(),
                })
                .await;
            }
            let status = report.status;
            let error = report.error.clone();
            let label = report.target.display_name();
            let stopped_by_issue = !report.issues.is_empty();
            outcome.reports[index] = report.clone();
            self.send(TransferEvent::TableFinished { report }).await;
            match status {
                TableStatus::Cancelled => {
                    outcome.cancelled = true;
                    break;
                }
                TableStatus::Failed if options.errors == ErrorMode::Stop => {
                    let message = format!(
                        "{label}: {}",
                        error.unwrap_or_else(|| "the copy failed".to_owned())
                    );
                    outcome.error = Some(ErrorInfo {
                        category: if stopped_by_issue {
                            ErrorCategory::InvalidInput
                        } else {
                            ErrorCategory::Sql
                        },
                        message,
                    });
                    break;
                }
                _ => {}
            }
        }
        if outcome.cancelled || outcome.error.is_some() {
            return;
        }
        self.after_load(loader.as_ref(), token, outcome).await;
    }

    /// Foreign keys of the loaded tables, then identity resyncs.
    async fn after_load(
        &self,
        loader: &dyn BulkLoader,
        token: &CancellationToken,
        outcome: &mut Outcome,
    ) {
        let plan = &self.prepared.plan;
        let options = &self.request.spec.options;
        for &index in &plan.order {
            if outcome.reports[index].status != TableStatus::Done {
                continue;
            }
            for script in &self.prepared.tables[index].foreign_keys {
                if token.is_cancelled() {
                    outcome.cancelled = true;
                    return;
                }
                let atomic = plan.transactional_ddl && script.is_atomic();
                match run_steps(self.target(), &script.steps, atomic, token).await {
                    Ok(()) => outcome.foreign_keys_added += 1,
                    Err(e) if e.is_cancelled() => {
                        outcome.cancelled = true;
                        return;
                    }
                    Err(e) => {
                        let message = format!(
                            "adding a foreign key to {} failed: {e}",
                            plan.tables[index].target.display_name()
                        );
                        if options.errors == ErrorMode::Stop {
                            outcome.error = Some(ErrorInfo {
                                category: e.category(),
                                message,
                            });
                            return;
                        }
                        outcome.warnings.push(message);
                    }
                }
            }
        }
        if options.identity != IdentityMode::Keep {
            return;
        }
        for &index in &plan.order {
            let prepared = &self.prepared.tables[index];
            if outcome.reports[index].status != TableStatus::Done
                || prepared.identity_columns.is_empty()
            {
                continue;
            }
            let statements =
                loader.identity_resync(&plan.tables[index].target, &prepared.identity_columns);
            let steps: Vec<ScriptStep> = statements.into_iter().map(ScriptStep::new).collect();
            if steps.is_empty() {
                continue;
            }
            if let Err(e) = run_steps(self.target(), &steps, false, token).await {
                if e.is_cancelled() {
                    outcome.cancelled = true;
                    return;
                }
                outcome.warnings.push(format!(
                    "moving the identity counter of {} past the copied values failed: {e}",
                    plan.tables[index].target.display_name()
                ));
            }
        }
    }

    /// One table: DDL that commits on its own, the copy, clean-up after a failure.
    async fn table(
        &self,
        index: usize,
        loader: &dyn BulkLoader,
        token: &CancellationToken,
    ) -> TableReport {
        let plan = &self.prepared.plan;
        let table = &plan.tables[index];
        let prepared = &self.prepared.tables[index];
        let started = Instant::now();
        let mut outside = false;
        if !prepared.ddl_in_transaction && !prepared.ddl.is_empty() {
            let atomic = plan.transactional_ddl && atomic_steps(&prepared.ddl);
            if let Err(e) = run_steps(self.target(), &prepared.ddl, atomic, token).await {
                let mut report = not_run(index, table.target.clone());
                report.status = if e.is_cancelled() {
                    TableStatus::Cancelled
                } else {
                    TableStatus::Failed
                };
                report.error = (!e.is_cancelled()).then(|| e.to_string());
                report.progress.elapsed_ms = millis(started);
                report.left_behind = format!(
                    "preparing {} failed: what its DDL changed before the failure stays (it \
                     commits at once on {})",
                    table.target.display_name(),
                    plan.target_engine.display_name()
                );
                return report;
            }
            outside = true;
        }
        let ddl: Vec<BoundStatement> = if prepared.ddl_in_transaction {
            prepared
                .ddl
                .iter()
                .filter(|s| s.phase == StepPhase::Main)
                .map(|s| BoundStatement::new(s.sql.clone(), Vec::new()))
                .collect()
        } else {
            Vec::new()
        };
        let events = self.events.clone();
        let on_progress = move |progress: &TableProgress| {
            let _ = events.try_send(TransferEvent::Progress {
                progress: progress.clone(),
            });
        };
        let job = CopyTable {
            plan: table,
            ddl: &ddl,
            source: self.source(),
            select: &prepared.select,
            target: self.target(),
            target_version: &self.sessions.target().info().server_version,
            loader,
            columns: &prepared.columns,
            options: &self.request.spec.options,
        };
        let mut report = match copy_table(job, &on_progress, token.clone()).await {
            Ok(report) => report,
            Err(e) => {
                let mut report = not_run(index, table.target.clone());
                report.status = TableStatus::Failed;
                report.error = Some(e.to_string());
                report.left_behind = "the copy could not start".to_owned();
                report
            }
        };
        if report.status != TableStatus::Done && outside {
            self.after_failure(table.mode, table.exists, prepared, &mut report)
                .await;
        }
        report
    }

    /// Where DDL committed on its own: drops the table the run created, and says what the
    /// target holds now.
    async fn after_failure(
        &self,
        mode: TableMode,
        existed: bool,
        prepared: &PreparedTable,
        report: &mut TableReport,
    ) {
        let table = report.target.display_name();
        let committed = report.progress.rows_committed;
        if mode == TableMode::Truncate {
            report.left_behind = format!(
                "{table} was emptied (its TRUNCATE commits at once) and holds {committed} \
                 copied rows"
            );
            return;
        }
        if !prepared.creates || prepared.drop_created.is_empty() {
            return;
        }
        let old = if mode == TableMode::Replace && existed {
            "the old table was dropped; "
        } else {
            ""
        };
        let never = CancellationToken::new();
        let dropping = run_steps(self.target(), &prepared.drop_created, false, &never);
        report.left_behind = match tokio::time::timeout(DROP_WAIT, dropping).await {
            Ok(Ok(())) => format!("{old}the table the transfer created was dropped"),
            Ok(Err(e)) => format!(
                "{old}{table} was created and holds {committed} rows: dropping it failed ({e})"
            ),
            Err(_) => format!(
                "{old}{table} was created and holds {committed} rows: dropping it took too long"
            ),
        };
    }

    /// Tells the schema cache about the tables the run created or replaced.
    fn announce(&self, outcome: &Outcome) {
        let Some(events) = &self.schema_events else {
            return;
        };
        let plan = &self.prepared.plan;
        let created: Vec<(DdlVerb, &oxbase_types::ObjectRef)> = plan
            .tables
            .iter()
            .zip(&outcome.reports)
            .filter(|(t, r)| {
                matches!(t.mode, TableMode::Create | TableMode::Replace)
                    && r.status == TableStatus::Done
            })
            .map(|(t, _)| (DdlVerb::Create, &t.target))
            .collect();
        if created.is_empty() {
            return;
        }
        events.publish(object_event(
            &self.dialects,
            plan.target_engine,
            self.request.target_session,
            ChangeSource::Import,
            &created,
        ));
    }
}

/// The report of a table that did not run.
fn not_run(index: usize, target: oxbase_types::ObjectRef) -> TableReport {
    TableReport {
        table: index,
        target,
        status: TableStatus::NotRun,
        progress: TableProgress {
            table: index,
            ..TableProgress::default()
        },
        issues: Vec::new(),
        issues_truncated: false,
        error: None,
        left_behind: "not started: the table is as it was".to_owned(),
    }
}

fn millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
