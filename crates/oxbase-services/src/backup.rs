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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt;
use oxbase_backup::archive::ArchiveReader;
use oxbase_backup::backup::BackupJob;
use oxbase_backup::restore::{RestoreJob, ScriptRunner};
use oxbase_dialect::DialectRegistry;
use oxbase_sql::DdlVerb;
use oxbase_types::{Error, ErrorCategory, ObjectKind, ObjectRef, Result, Script};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub use oxbase_backup::{
    BackupContent, BackupProgress, BackupReport, BackupSpec, Compression, Manifest, ManifestObject,
    RestoreContent, RestoreOptions, RestoreProgress, RestoreReport, RestoredObject, RestoredStatus,
    VerifyReport,
};

use crate::designer::{StepStatus, execute_script};
use crate::import::{WorkSession, close_work_session, work_session};
use crate::query::ErrorInfo;
use crate::runs::{OperationId, RunRegistry, RunTicket};
use crate::schema_cache::{ChangeSource, SchemaEvents, object_event};
use crate::tree::TreeService;
use crate::{CancellationToken, ConnectionManager, SessionId};

/// Events buffered per run; progress beyond it is dropped (lossy), other events wait.
const EVENT_BUFFER: usize = 64;
/// How long closing a run's work session may hold up its terminal event.
const CLOSE_WAIT: Duration = Duration::from_millis(500);

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
        let base = self
            .manager
            .get(request.session)
            .ok_or_else(|| Error::not_found(format!("no open session {}", request.session)))?;
        if !matches!(
            request.spec.container.kind,
            ObjectKind::Database | ObjectKind::Schema
        ) {
            return Err(Error::invalid_input(
                "a backup is of a database or a schema",
            ));
        }
        let engine = base.summary().engine;
        if self.dialects.fetcher(engine, ObjectKind::Table).is_err() {
            return Err(Error::unsupported(format!(
                "Oxbase cannot back up {} databases",
                engine.display_name()
            )));
        }
        if !overwrite && tokio::fs::try_exists(destination).await.unwrap_or(false) {
            return Err(Error::invalid_input(format!(
                "{} already exists",
                oxbase_io::file_label(destination)
            )));
        }
        let ticket = self.runs.register(&[request.session]);
        let id = ticket.id();
        let (events, receiver) = mpsc::channel(EVENT_BUFFER);
        let run = BackupRun {
            manager: Arc::clone(&self.manager),
            dialects: Arc::clone(&self.dialects),
            request,
            destination: destination.to_path_buf(),
            overwrite,
            events,
        };
        tokio::spawn(run.run(ticket));
        Ok(BackupRunHandle {
            id,
            events: receiver,
        })
    }

    /// Starts restoring the archive at `source`.
    pub async fn start_restore(
        &self,
        request: RestoreRequest,
        source: &Path,
    ) -> Result<BackupRunHandle<RestoreEvent>> {
        let options = &request.options;
        if options.is_destructive() && !options.confirm_destructive {
            return Err(Error::invalid_input(
                "this restore drops or empties existing objects; confirm it first",
            ));
        }
        if !matches!(
            options.container.kind,
            ObjectKind::Database | ObjectKind::Schema
        ) {
            return Err(Error::invalid_input(
                "a restore goes into a database or a schema",
            ));
        }
        self.manager
            .get(request.session)
            .ok_or_else(|| Error::not_found(format!("no open session {}", request.session)))?;
        // The manifest first: a file that is not a backup fails here, before a run starts.
        let manifest = read_blocking(source.to_path_buf(), |r| Ok(r.manifest().clone())).await?;
        let ticket = self.runs.register(&[request.session]);
        let id = ticket.id();
        let (events, receiver) = mpsc::channel(EVENT_BUFFER);
        let run = RestoreRun {
            manager: Arc::clone(&self.manager),
            dialects: Arc::clone(&self.dialects),
            schema_events: self.schema_events.clone(),
            request,
            source: source.to_path_buf(),
            manifest,
            events,
        };
        tokio::spawn(run.run(ticket));
        Ok(BackupRunHandle {
            id,
            events: receiver,
        })
    }

    /// Recomputes every checksum of the archive at `path`.
    pub async fn verify(&self, path: &Path, cancel: CancellationToken) -> Result<VerifyReport> {
        // Request-style: registered with the request's token (no session).
        let _ticket = self.runs.register_with(&[], cancel.clone());
        let token = cancel.clone();
        let work = read_blocking(path.to_path_buf(), move |r| {
            r.verify(&mut |_| {}, &|| token.is_cancelled())
        });
        crate::until_cancelled(&cancel, work).await
    }

    /// The manifest of the archive at `path` (validated).
    pub async fn list(&self, path: &Path, cancel: CancellationToken) -> Result<Manifest> {
        let _ticket = self.runs.register_with(&[], cancel.clone());
        let work = read_blocking(path.to_path_buf(), |r| Ok(r.manifest().clone()));
        crate::until_cancelled(&cancel, work).await
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

/// Opens the archive at `path` on a blocking thread and runs `read` on it.
async fn read_blocking<T: Send + 'static>(
    path: PathBuf,
    read: impl FnOnce(&mut ArchiveReader<std::fs::File>) -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(move || {
        let mut reader = ArchiveReader::open(&path)?;
        read(&mut reader)
    })
    .await
    .map_err(|_| Error::internal("reading the backup stopped unexpectedly"))?
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The work session of a run on `container`'s database.
async fn open_session(
    manager: &ConnectionManager,
    dialects: &DialectRegistry,
    base: SessionId,
    container: &ObjectRef,
    cancel: &CancellationToken,
) -> Result<WorkSession> {
    crate::until_cancelled(
        cancel,
        work_session(manager, dialects, base, container.database_name(), cancel),
    )
    .await
}

/// One backup, from its session to its terminal event.
struct BackupRun {
    manager: Arc<ConnectionManager>,
    dialects: Arc<DialectRegistry>,
    request: BackupRequest,
    destination: PathBuf,
    overwrite: bool,
    events: mpsc::Sender<BackupEvent>,
}

impl BackupRun {
    async fn run(self, ticket: RunTicket) {
        let started = Instant::now();
        let token = ticket.token().clone();
        let id = ticket.id();
        let (progress_in, mut progress_out) = mpsc::unbounded_channel::<BackupProgress>();
        // Started (with the object count) once the objects are listed, then lossy progress.
        let forward = tokio::spawn({
            let events = self.events.clone();
            async move {
                let mut started = false;
                while let Some(progress) = progress_out.recv().await {
                    if !started {
                        started = true;
                        let _ = events
                            .send(BackupEvent::Started {
                                backup_id: id,
                                objects: progress.objects_total,
                            })
                            .await;
                    }
                    let _ = events.try_send(BackupEvent::Progress { progress });
                }
                started
            }
        });
        let result = self.backup(&token, progress_in).await;
        let announced = forward.await.unwrap_or(false);
        if !announced {
            let _ = self
                .events
                .send(BackupEvent::Started {
                    backup_id: id,
                    objects: 0,
                })
                .await;
        }
        let event = match result {
            Ok(report) => BackupEvent::Finished {
                report: Box::new(report),
            },
            Err(e) if e.is_cancelled() || token.is_cancelled() => BackupEvent::Cancelled {
                elapsed_ms: elapsed_ms(started),
            },
            Err(e) => BackupEvent::Failed {
                error: ErrorInfo::from(&e),
                elapsed_ms: elapsed_ms(started),
            },
        };
        let _ = self.events.send(event).await;
        drop(ticket);
    }

    async fn backup(
        &self,
        token: &CancellationToken,
        progress: mpsc::UnboundedSender<BackupProgress>,
    ) -> Result<BackupReport> {
        let session = open_session(
            &self.manager,
            &self.dialects,
            self.request.session,
            &self.request.spec.container,
            token,
        )
        .await?;
        let connection = Arc::clone(session.handle.connection());
        let result = oxbase_backup::backup::backup(
            BackupJob {
                dialects: &self.dialects,
                metadata: connection.as_ref(),
                rows: connection.as_ref(),
                spec: &self.request.spec,
            },
            &self.destination,
            self.overwrite,
            &move |p: &BackupProgress| {
                let _ = progress.send(p.clone());
            },
            token.clone(),
        )
        .await;
        drop(connection);
        close_work_session(&self.manager, session, CLOSE_WAIT).await;
        result
    }
}

/// One restore, from its session to its terminal event.
struct RestoreRun {
    manager: Arc<ConnectionManager>,
    dialects: Arc<DialectRegistry>,
    schema_events: Option<SchemaEvents>,
    request: RestoreRequest,
    source: PathBuf,
    manifest: Manifest,
    events: mpsc::Sender<RestoreEvent>,
}

impl RestoreRun {
    async fn run(self, ticket: RunTicket) {
        let started = Instant::now();
        let token = ticket.token().clone();
        let _ = self
            .events
            .send(RestoreEvent::Started {
                restore_id: ticket.id(),
                manifest: Box::new(self.manifest.clone()),
            })
            .await;
        let outcome = self.restore(&token).await;
        let event = match outcome {
            Ok(outcome) => {
                self.announce(&outcome.report);
                match outcome.error {
                    None => RestoreEvent::Finished {
                        report: outcome.report,
                    },
                    Some(e) if e.is_cancelled() || token.is_cancelled() => {
                        RestoreEvent::Cancelled {
                            report: outcome.report,
                        }
                    }
                    Some(e) => RestoreEvent::Failed {
                        error: ErrorInfo::from(&e),
                        report: outcome.report,
                    },
                }
            }
            Err(e) => {
                let report = RestoreReport {
                    objects: Vec::new(),
                    rows: 0,
                    left_behind: format!(
                        "nothing was written to {}",
                        self.request.options.container.display_name()
                    ),
                    warnings: Vec::new(),
                    elapsed_ms: elapsed_ms(started),
                };
                if e.is_cancelled() || token.is_cancelled() {
                    RestoreEvent::Cancelled { report }
                } else {
                    RestoreEvent::Failed {
                        error: ErrorInfo::from(&e),
                        report,
                    }
                }
            }
        };
        let _ = self.events.send(event).await;
        drop(ticket);
    }

    async fn restore(&self, token: &CancellationToken) -> Result<oxbase_backup::RestoreOutcome> {
        let session = open_session(
            &self.manager,
            &self.dialects,
            self.request.session,
            &self.request.options.container,
            token,
        )
        .await?;
        let connection = Arc::clone(session.handle.connection());
        let handle = session.handle.clone();
        let script_token = token.clone();
        let run_script: Box<ScriptRunner> = Box::new(move |script: Script| {
            let handle = handle.clone();
            let token = script_token.clone();
            async move { run_structure_script(&handle, &script, token).await }.boxed()
        });
        let events = self.events.clone();
        let outcome = oxbase_backup::restore::restore_run(
            RestoreJob {
                dialects: &self.dialects,
                target: connection.as_ref(),
                options: &self.request.options,
            },
            &self.source,
            run_script.as_ref(),
            &move |p: &RestoreProgress| {
                let _ = events.try_send(RestoreEvent::Progress {
                    progress: p.clone(),
                });
            },
            token.clone(),
        )
        .await;
        drop(run_script);
        drop(connection);
        close_work_session(&self.manager, session, CLOSE_WAIT).await;
        Ok(outcome)
    }

    /// M6: the schema cache reads restored tables and views again.
    fn announce(&self, report: &RestoreReport) {
        let Some(events) = &self.schema_events else {
            return;
        };
        let changed: Vec<&ObjectRef> = report
            .objects
            .iter()
            .filter(|o| {
                matches!(o.status, RestoredStatus::Created | RestoredStatus::Replaced)
                    && matches!(o.object.kind, ObjectKind::Table | ObjectKind::View)
            })
            .map(|o| &o.object)
            .collect();
        if changed.is_empty() {
            return;
        }
        let changes: Vec<(DdlVerb, &ObjectRef)> =
            changed.into_iter().map(|o| (DdlVerb::Create, o)).collect();
        events.publish(object_event(
            &self.dialects,
            self.manifest.engine,
            self.request.session,
            ChangeSource::Import,
            &changes,
        ));
    }
}

/// Runs a restore's structure script on its work session (`execute_script`, one transaction
/// where every step is transactional); the first failed step's error, naming the step.
async fn run_structure_script(
    session: &crate::SessionHandle,
    script: &Script,
    cancel: CancellationToken,
) -> Result<bool> {
    let outcome = execute_script(session, script, script.is_atomic(), cancel.clone()).await?;
    if outcome.committed {
        return Ok(true);
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let failed = outcome
        .steps
        .iter()
        .find(|s| s.status == StepStatus::Failed)
        .and_then(|s| s.error.as_ref().map(|e| (s.index, e)));
    match failed {
        Some((index, error)) => {
            let step = script.steps.get(index);
            let what = match step {
                Some(s) if !s.description.is_empty() => s.description.clone(),
                Some(s) => s.sql.clone(),
                None => String::new(),
            };
            Err(error_of(
                error.category,
                format!("{what}: {}", error.message),
            ))
        }
        None => Ok(false),
    }
}

/// An [`Error`] of `category` with `message` (the inverse of [`ErrorInfo::from`]).
fn error_of(category: ErrorCategory, message: String) -> Error {
    match category {
        ErrorCategory::Connection => Error::connection(message),
        ErrorCategory::Auth => Error::auth(message),
        ErrorCategory::Timeout => Error::timeout(message),
        ErrorCategory::Cancelled => Error::Cancelled,
        ErrorCategory::Unsupported => Error::unsupported(message),
        ErrorCategory::NotFound => Error::not_found(message),
        ErrorCategory::InvalidInput => Error::invalid_input(message),
        ErrorCategory::Io => Error::Io {
            kind: "other".to_owned(),
            message,
        },
        ErrorCategory::Internal => Error::internal(message),
        ErrorCategory::Sql => Error::sql(None::<String>, message),
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
