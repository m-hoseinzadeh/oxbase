//! The import run: work session, target DDL, reading, conversion, error policy, loading,
//! commits, progress, cancel and clean-up (module docs of `import`, step 4).

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use arrow_array::{BooleanArray, RecordBatch};
use arrow_schema::SchemaRef;
use futures::StreamExt;
use oxbase_dialect::{
    BulkLimits, BulkLoader, DialectRegistry, LoadColumn, LoadSession, LoadTarget,
};
use oxbase_driver::{Connection, Tx};
use oxbase_io::{SourceBatch, SourceReader};
use oxbase_types::{BoundStatement, Error, Result, ScriptStep};
use tokio::sync::mpsc;

use super::convert::Converter;
use super::{
    ErrorPolicy, ImportConfig, ImportEvent, ImportId, ImportReport, ImportRequest, ImportRowError,
    Prepared, TargetMode, close_work_session, value_formats, work_session,
};
use crate::query::ErrorInfo;
use crate::{CancellationToken, ConnectionManager, SessionHandle};

/// Opens the source (`oxbase_io::open`, or a test reader). Runs on a blocking thread.
pub(super) type Opener = Box<dyn FnOnce() -> Result<Box<dyn SourceReader>> + Send>;

/// How long a cancelled load may take to stop on its own before it is abandoned.
const CANCEL_GRACE: Duration = Duration::from_millis(800);
/// How long a rollback may take before the transaction is dropped (the server then rolls it
/// back when the session closes).
const ROLLBACK_WAIT: Duration = Duration::from_millis(600);
/// How long dropping the table the import created may take.
const DROP_WAIT: Duration = Duration::from_secs(5);
/// How long `BulkLoader::finish` and closing the work session may take before the terminal
/// event (they go on in the background).
const FINISH_WAIT: Duration = Duration::from_millis(500);
/// Row errors sent as events.
const MAX_ERROR_EVENTS: usize = 100;

/// One started import.
pub(super) struct Job {
    pub id: ImportId,
    pub prepared: Prepared,
    pub request: ImportRequest,
    pub manager: Arc<ConnectionManager>,
    pub dialects: Arc<DialectRegistry>,
    pub config: ImportConfig,
    pub token: CancellationToken,
    pub events: mpsc::Sender<ImportEvent>,
    pub running: Arc<Mutex<HashMap<ImportId, CancellationToken>>>,
}

/// Counters and what happened to the table.
#[derive(Debug, Default)]
struct State {
    rows_read: u64,
    rows_inserted: u64,
    rows_committed: u64,
    rows_skipped: u64,
    bytes_read: u64,
    errors: Vec<ImportRowError>,
    errors_truncated: bool,
    error_events: usize,
    /// DDL ran inside the open transaction (undone by its rollback).
    ddl_in_open_tx: bool,
    /// The new table exists for good (create / replace).
    created_committed: bool,
    /// The old table is gone for good (replace).
    old_dropped: bool,
    /// The truncate / delete is committed.
    emptied_committed: bool,
    /// The import dropped the table it had created.
    table_dropped: bool,
    drop_failed: Option<String>,
    truncated_texts: u64,
    warnings: Vec<String>,
}

impl Job {
    /// Runs the import to its terminal event.
    pub(super) async fn run(self, open: Opener) {
        let started = Instant::now();
        let mut st = State::default();
        let _ = self
            .events
            .send(ImportEvent::Started {
                import_id: self.id,
                total_bytes: self.prepared.total_bytes,
                plan: self.prepared.plan.clone(),
            })
            .await;
        let table = &self.prepared.plan.table;
        let session = work_session(
            &self.manager,
            &self.dialects,
            self.request.target.session,
            table.database_name(),
            &self.token,
        )
        .await;
        let outcome = match session {
            Err(e) => Err(e),
            Ok(session) => {
                let outcome = self
                    .on_session(&session.handle, open, &mut st, started)
                    .await;
                close_work_session(&self.manager, session, FINISH_WAIT).await;
                outcome
            }
        };
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
        let ok = outcome.is_ok();
        let report = self.report(&st, ok, started);
        let event = match outcome {
            Ok(()) => ImportEvent::Finished { report },
            Err(e) if e.is_cancelled() || self.token.is_cancelled() => {
                ImportEvent::Cancelled { report }
            }
            Err(e) => ImportEvent::Failed {
                error: ErrorInfo::from(&e),
                report,
            },
        };
        let _ = self.events.send(event).await;
    }

    /// Everything on the work session: load, then clean up after a failure.
    async fn on_session(
        &self,
        session: &SessionHandle,
        open: Opener,
        st: &mut State,
        started: Instant,
    ) -> Result<()> {
        let _guard =
            crate::until_cancelled(&self.token, async { Ok(session.exclusive().await) }).await?;
        let connection = session.connection().as_ref();
        let p = &self.prepared;
        let loader = self.dialects.bulk_loader(p.engine)?;
        let limits = loader.limits(&p.version);
        let load_columns: Vec<LoadColumn> = p.columns.iter().map(|(_, _, c)| c.clone()).collect();
        let target = LoadTarget {
            table: &p.plan.table,
            columns: &load_columns,
            version: &p.version,
        };
        let mut pipeline = Pipeline::new(
            self,
            connection,
            loader.as_ref(),
            target,
            limits,
            st,
            started,
        );
        let result = pipeline.run(open).await;
        if result.is_err() {
            pipeline.abort().await;
        }
        let prepared_loader = pipeline.loader_prepared;
        let truncated_texts = pipeline.converter.truncated_texts;
        drop(pipeline);
        st.truncated_texts = truncated_texts;
        if prepared_loader {
            let finish = loader.finish(
                LoadSession::new(connection, None),
                &target,
                CancellationToken::new(),
            );
            match tokio::time::timeout(FINISH_WAIT, finish).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => st
                    .warnings
                    .push(format!("ending the bulk load session failed: {e}")),
                Err(_) => st
                    .warnings
                    .push("ending the bulk load session took too long".to_owned()),
            }
        }
        if result.is_err() {
            self.clean_up(connection, st).await;
        }
        result
    }

    /// After a failure: drops the table the import created when it exists for good.
    async fn clean_up(&self, connection: &dyn Connection, st: &mut State) {
        if !st.created_committed || self.prepared.drop_new.is_empty() {
            return;
        }
        let token = CancellationToken::new();
        let dropping = async {
            for step in &self.prepared.drop_new {
                run_step(LoadSession::new(connection, None), step, &token).await?;
            }
            Ok::<(), Error>(())
        };
        match tokio::time::timeout(DROP_WAIT, dropping).await {
            Ok(Ok(())) => st.table_dropped = true,
            Ok(Err(e)) => st.drop_failed = Some(e.to_string()),
            Err(_) => st.drop_failed = Some("it took too long".to_owned()),
        }
    }

    fn report(&self, st: &State, ok: bool, started: Instant) -> ImportReport {
        let p = &self.prepared;
        let mut warnings = st.warnings.clone();
        if st.truncated_texts > 0 {
            warnings.push(format!(
                "{} texts were cut to their column's length",
                st.truncated_texts
            ));
        }
        let creates = p.plan.creates_table;
        ImportReport {
            import_id: self.id,
            table: p.plan.table.clone(),
            method: p.plan.method,
            rows_read: st.rows_read,
            rows_inserted: st.rows_inserted,
            rows_committed: st.rows_committed,
            rows_skipped: st.rows_skipped,
            errors: st.errors.clone(),
            errors_truncated: st.errors_truncated,
            table_created: creates && (ok || st.created_committed),
            table_dropped: st.table_dropped,
            left_behind: self.left_behind(st, ok),
            bytes_read: if ok {
                st.bytes_read.max(p.total_bytes)
            } else {
                st.bytes_read
            },
            elapsed_ms: millis(started),
            warnings,
        }
    }

    /// What the table holds now, in words (module docs, *Transactions*).
    fn left_behind(&self, st: &State, ok: bool) -> String {
        let p = &self.prepared;
        let table = p.plan.table.display_name();
        let committed = st.rows_committed;
        if ok {
            return match p.plan.mode {
                TargetMode::Append => format!("{committed} rows were added to {table}"),
                _ => format!("{table} holds the {committed} imported rows"),
            };
        }
        let mut text = match p.plan.mode {
            TargetMode::Append if committed == 0 => {
                format!("nothing was inserted: {table} is as it was")
            }
            TargetMode::Append => {
                format!("{table} keeps the {committed} rows committed before the import stopped")
            }
            TargetMode::Truncate | TargetMode::DeleteAll => {
                if st.emptied_committed {
                    format!(
                        "{table} was emptied and holds the {committed} rows committed before \
                         the import stopped"
                    )
                } else {
                    format!("{table} is as it was (the transaction was rolled back)")
                }
            }
            TargetMode::Create | TargetMode::Replace => {
                let old = if p.plan.mode == TargetMode::Replace && p.existed {
                    if st.old_dropped {
                        "the old table is gone; "
                    } else {
                        "the old table is as it was; "
                    }
                } else {
                    ""
                };
                let new = if st.table_dropped {
                    "the table the import created was dropped".to_owned()
                } else if let Some(e) = &st.drop_failed {
                    format!(
                        "{table} was created and holds {committed} rows: dropping it failed \
                         ({e})"
                    )
                } else if st.created_committed {
                    format!("{table} was created and holds {committed} rows")
                } else {
                    "no table was created (the transaction was rolled back)".to_owned()
                };
                format!("{old}{new}")
            }
        };
        if !p.transactional_storage && st.rows_inserted > st.rows_committed {
            text.push_str(
                "; the table's storage does not support transactions, so rows inserted after \
                 the last commit may have stayed",
            );
        }
        text
    }

    async fn send(&self, event: ImportEvent) {
        let _ = self.events.send(event).await;
    }
}

fn millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Runs one DDL step to its end (cancellable).
async fn run_step(
    session: LoadSession<'_>,
    step: &ScriptStep,
    cancel: &CancellationToken,
) -> Result<()> {
    let statement = BoundStatement::new(step.sql.clone(), Vec::new());
    cancellable(cancel, session.run(&statement, cancel.clone()))
        .await
        .map(|_| ())
}

/// Runs `fut` to its end, or, once `cancel` fires, gives it [`CANCEL_GRACE`] to stop and
/// returns `Cancelled`.
async fn cancellable<T>(
    cancel: &CancellationToken,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::pin!(fut);
    tokio::select! {
        biased;
        result = &mut fut => result,
        () = cancel.cancelled() => {
            let _ = tokio::time::timeout(CANCEL_GRACE, &mut fut).await;
            Err(Error::Cancelled)
        }
    }
}

/// `error` with the source rows of the failing call in front of its message.
fn in_rows(error: Error, first: u64, last: u64) -> Error {
    let range = if first == last {
        format!("row {first}")
    } else {
        format!("rows {first}-{last}")
    };
    match error {
        Error::Sql { code, message } => Error::Sql {
            code,
            message: format!("{range}: {message}"),
        },
        Error::InvalidInput { message } => Error::InvalidInput {
            message: format!("{range}: {message}"),
        },
        Error::Unsupported { message } => Error::Unsupported {
            message: format!("{range}: {message}"),
        },
        Error::Internal { message } => Error::Internal {
            message: format!("{range}: {message}"),
        },
        other => other,
    }
}

/// The loading half of a run, on the work session.
struct Pipeline<'a> {
    job: &'a Job,
    connection: &'a dyn Connection,
    loader: &'a dyn BulkLoader,
    target: LoadTarget<'a>,
    load_schema: SchemaRef,
    converter: Converter,
    /// Source column names that are imported (reader issues of other columns are ignored).
    mapped_sources: HashSet<String>,
    /// Rows per `BulkLoader::load` call.
    call_rows: usize,
    commit_every: Option<u64>,
    tx: Option<Box<dyn Tx>>,
    /// Converted rows waiting for a load call, and their source row numbers.
    pending: Vec<RecordBatch>,
    pending_rows: VecDeque<u64>,
    loader_prepared: bool,
    last_progress: Instant,
    started: Instant,
    st: &'a mut State,
}

impl<'a> Pipeline<'a> {
    fn new(
        job: &'a Job,
        connection: &'a dyn Connection,
        loader: &'a dyn BulkLoader,
        target: LoadTarget<'a>,
        limits: BulkLimits,
        st: &'a mut State,
        started: Instant,
    ) -> Self {
        let commit_every = job.prepared.plan.transaction.commit_every;
        let mut call_rows = limits.rows_per_call.max(1);
        if let Some(every) = commit_every {
            call_rows = call_rows.min(usize::try_from(every).unwrap_or(usize::MAX));
        }
        let converter = Converter::new(
            &job.prepared.columns,
            job.request.conversion,
            value_formats(&job.request.options),
        );
        let mapped_sources = job
            .prepared
            .columns
            .iter()
            .map(|(source, _, _)| job.request.columns[*source].name.clone())
            .collect();
        Self {
            job,
            connection,
            loader,
            load_schema: target.schema(),
            target,
            converter,
            mapped_sources,
            call_rows,
            commit_every,
            tx: None,
            pending: Vec::new(),
            pending_rows: VecDeque::new(),
            loader_prepared: false,
            last_progress: Instant::now(),
            started,
            st,
        }
    }

    fn token(&self) -> &CancellationToken {
        &self.job.token
    }

    async fn run(&mut self, open: Opener) -> Result<()> {
        let job = self.job;
        let p = &job.prepared;
        let token = job.token.clone();

        // 1. DDL that commits on its own runs first, outside the transaction.
        if !p.ddl_in_tx {
            for (index, step) in p.ddl.iter().enumerate() {
                run_step(LoadSession::new(self.connection, None), step, &token).await?;
                if index + 1 == p.drops {
                    self.st.old_dropped = true;
                }
            }
            self.ddl_committed();
        }

        // 2. Session settings of the loader.
        cancellable(
            &token,
            self.loader.prepare(
                LoadSession::new(self.connection, None),
                &self.target,
                token.clone(),
            ),
        )
        .await?;
        self.loader_prepared = true;

        // 3. The reader.
        let reader = tokio::task::spawn_blocking(open)
            .await
            .map_err(|_| Error::internal("opening the file stopped unexpectedly"))??;
        if reader.schema().fields().len() != job.request.columns.len() {
            return Err(Error::invalid_input(format!(
                "the file has {} columns, the request describes {}",
                reader.schema().fields().len(),
                job.request.columns.len()
            )));
        }
        let mut stream = oxbase_io::spawn_reader(reader, token.clone(), job.config.reader_capacity);

        // 4. The transaction, with the transactional DDL.
        self.begin().await?;
        if p.ddl_in_tx && !p.ddl.is_empty() {
            for step in &p.ddl {
                let tx = self.tx.as_deref();
                run_step(LoadSession::new(self.connection, tx), step, &token).await?;
            }
            self.st.ddl_in_open_tx = true;
        }

        // 5. Batches.
        loop {
            let next = tokio::select! {
                biased;
                () = token.cancelled() => Some(Err(Error::Cancelled)),
                item = stream.next() => item,
            };
            match next {
                None => break,
                Some(Err(e)) => return Err(e),
                Some(Ok(batch)) => self.batch(batch).await?,
            }
        }
        self.flush(true).await?;
        self.st.bytes_read = self.st.bytes_read.max(p.total_bytes);
        self.commit().await?;
        let event = self.progress_event();
        job.send(event).await;
        Ok(())
    }

    async fn begin(&mut self) -> Result<()> {
        let begin = self.connection.begin();
        self.tx = Some(cancellable(&self.job.token, begin).await?);
        Ok(())
    }

    /// The DDL is in the table for good.
    fn ddl_committed(&mut self) {
        let p = &self.job.prepared;
        match p.plan.mode {
            TargetMode::Create | TargetMode::Replace => {
                self.st.created_committed = true;
                if p.drops > 0 {
                    self.st.old_dropped = true;
                }
            }
            TargetMode::Truncate | TargetMode::DeleteAll => self.st.emptied_committed = true,
            TargetMode::Append => {}
        }
    }

    async fn commit(&mut self) -> Result<()> {
        if let Some(tx) = self.tx.take() {
            tx.commit().await?;
        }
        self.st.rows_committed = self.st.rows_inserted;
        if self.st.ddl_in_open_tx {
            self.st.ddl_in_open_tx = false;
            self.ddl_committed();
        }
        self.job
            .send(ImportEvent::Committed {
                rows_committed: self.st.rows_committed,
            })
            .await;
        self.progress(true);
        Ok(())
    }

    /// Rolls the open transaction back (bounded); the DDL in it goes with it.
    async fn abort(&mut self) {
        if let Some(tx) = self.tx.take() {
            match tokio::time::timeout(ROLLBACK_WAIT, tx.rollback()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => self
                    .st
                    .warnings
                    .push(format!("rolling the transaction back failed: {e}")),
                Err(_) => self.st.warnings.push(
                    "rolling the transaction back took too long; the server rolls it back when \
                     the session closes"
                        .to_owned(),
                ),
            }
        }
        self.st.ddl_in_open_tx = false;
    }

    /// One source batch: convert, apply the error policy, queue, load full calls.
    async fn batch(&mut self, source: SourceBatch) -> Result<()> {
        let rows = source.batch.num_rows();
        self.st.rows_read += u64::try_from(rows).unwrap_or(u64::MAX);
        self.st.bytes_read = self.st.bytes_read.max(source.bytes_read);
        let (converted, cell_issues) = self.converter.convert(&source.batch)?;
        let mut issues: Vec<(usize, Option<String>, String)> = source
            .issues
            .into_iter()
            .filter(|i| i.batch_row < rows)
            .filter(|i| {
                i.column
                    .as_ref()
                    .is_none_or(|c| self.mapped_sources.contains(c))
            })
            .map(|i| (i.batch_row, i.column, i.message))
            .collect();
        issues.extend(
            cell_issues
                .into_iter()
                .map(|i| (i.batch_row, Some(i.column), i.message)),
        );
        let mut keep = vec![true; rows];
        if !issues.is_empty() {
            issues.sort_by_key(|i| i.0);
            for (row, column, message) in issues {
                let row_number = source.first_row + u64::try_from(row).unwrap_or(u64::MAX);
                let error = ImportRowError {
                    row: row_number,
                    column,
                    message,
                };
                let stop = match self.job.request.errors {
                    ErrorPolicy::Stop => Some(stop_error(&error)),
                    ErrorPolicy::Skip { .. } => None,
                };
                self.record_error(error).await;
                if let Some(stop) = stop {
                    return Err(stop);
                }
                keep[row] = false;
            }
            let skipped = keep.iter().filter(|k| !**k).count();
            self.st.rows_skipped += u64::try_from(skipped).unwrap_or(u64::MAX);
            if let ErrorPolicy::Skip { max_errors } = self.job.request.errors
                && self.st.rows_skipped > max_errors
            {
                return Err(Error::invalid_input(format!(
                    "more than {max_errors} rows had errors; the import stopped"
                )));
            }
        }
        let kept = if keep.iter().all(|k| *k) {
            converted
        } else {
            arrow_select::filter::filter_record_batch(&converted, &BooleanArray::from(keep.clone()))
                .map_err(|e| Error::internal(format!("dropping skipped rows failed: {e}")))?
        };
        let batch = RecordBatch::try_new(Arc::clone(&self.load_schema), kept.columns().to_vec())
            .map_err(|e| Error::internal(format!("building a load batch failed: {e}")))?;
        for (row, kept) in keep.iter().enumerate() {
            if *kept {
                self.pending_rows
                    .push_back(source.first_row + u64::try_from(row).unwrap_or(u64::MAX));
            }
        }
        if batch.num_rows() > 0 {
            self.pending.push(batch);
        }
        self.flush(false).await?;
        self.progress(false);
        Ok(())
    }

    /// Keeps `error` in the report (up to the cap) and sends it (the first 100).
    async fn record_error(&mut self, error: ImportRowError) {
        if self.st.error_events < MAX_ERROR_EVENTS {
            self.st.error_events += 1;
            self.job
                .send(ImportEvent::RowError {
                    error: error.clone(),
                })
                .await;
        }
        if self.st.errors.len() < self.job.config.max_reported_errors {
            self.st.errors.push(error);
        } else {
            self.st.errors_truncated = true;
        }
    }

    /// Loads full calls of queued rows (every queued row with `all`).
    async fn flush(&mut self, all: bool) -> Result<()> {
        loop {
            let queued: usize = self.pending.iter().map(RecordBatch::num_rows).sum();
            if queued == 0 || (!all && queued < self.call_rows) {
                return Ok(());
            }
            let combined = if self.pending.len() == 1 {
                self.pending.remove(0)
            } else {
                let combined =
                    arrow_select::concat::concat_batches(&self.load_schema, &self.pending)
                        .map_err(|e| {
                            Error::internal(format!("combining load batches failed: {e}"))
                        })?;
                self.pending.clear();
                combined
            };
            let take = combined.num_rows().min(self.call_rows);
            if take < combined.num_rows() {
                self.pending
                    .push(combined.slice(take, combined.num_rows() - take));
            }
            let chunk = combined.slice(0, take);
            let numbers: Vec<u64> = self
                .pending_rows
                .drain(..take.min(self.pending_rows.len()))
                .collect();
            let first = numbers.first().copied().unwrap_or_default();
            let last = numbers.last().copied().unwrap_or(first);
            self.load(&chunk, first, last).await?;
        }
    }

    /// One `BulkLoader::load` call, then a commit when the policy says so.
    async fn load(&mut self, chunk: &RecordBatch, first: u64, last: u64) -> Result<()> {
        let token = self.token().clone();
        let session = LoadSession::new(self.connection, self.tx.as_deref());
        let load = self
            .loader
            .load(session, &self.target, chunk, token.clone());
        cancellable(&token, load).await.map_err(|e| {
            if e.is_cancelled() {
                e
            } else {
                in_rows(e, first, last)
            }
        })?;
        self.st.rows_inserted += u64::try_from(chunk.num_rows()).unwrap_or(u64::MAX);
        if let Some(every) = self.commit_every
            && self.st.rows_inserted / every > self.st.rows_committed / every
        {
            self.commit().await?;
            self.begin().await?;
        }
        Ok(())
    }

    fn progress_event(&self) -> ImportEvent {
        ImportEvent::Progress {
            bytes_read: self.st.bytes_read,
            total_bytes: self.job.prepared.total_bytes,
            rows_read: self.st.rows_read,
            rows_inserted: self.st.rows_inserted,
            rows_committed: self.st.rows_committed,
            rows_skipped: self.st.rows_skipped,
            elapsed_ms: millis(self.started),
        }
    }

    /// Sends a progress event (dropped when the consumer lags) at most every
    /// `progress_interval`, or now with `force`.
    fn progress(&mut self, force: bool) {
        if !force && self.last_progress.elapsed() < self.job.config.progress_interval {
            return;
        }
        self.last_progress = Instant::now();
        let _ = self.job.events.try_send(self.progress_event());
    }
}

/// The error a row issue stops the import with ([`ErrorPolicy::Stop`]).
fn stop_error(error: &ImportRowError) -> Error {
    match &error.column {
        Some(column) => Error::invalid_input(format!(
            "row {}, column '{column}': {}",
            error.row, error.message
        )),
        None => Error::invalid_input(format!("row {}: {}", error.row, error.message)),
    }
}
