//! Export (M5): a table, a query or a result the grid holds, into a file, with progress and
//! cancel. Contract: `docs/m5-contracts.md` §5.4.
//!
//! # Sources
//!
//! - [`ExportSource::Table`]: the M3 table-data query (filter and sort with bound values,
//!   `oxbase_dialect::sql::select_table_data` with typed filter values, like
//!   `EditService::open_table_data`), streamed from the server on a **dedicated child
//!   session** (`ConnectionManager::open_child` on the table's database, closed at the end).
//!   The table's definition (loaded with the `Table` fetcher) goes to the writer
//!   ([`oxbase_io::WriteContext::table`]) for SQL dumps.
//! - [`ExportSource::Query`]: one statement (`oxbase_sql::split` must find exactly one, else
//!   `InvalidInput`), run on a dedicated child session; its first result set is exported. Not
//!   recorded in the query history.
//! - [`ExportSource::Result`]: a result set of a query run (`QueryService`): every row the
//!   run's `ResultStore` holds, **including spilled rows**, read page by page with
//!   `QueryService::page` ([`ExportConfig::page_rows`] per page). The run must not be running
//!   (`InvalidInput` "wait for the query to finish"); a truncated result exports what was
//!   kept and says so in the report.
//!
//! Rows stream from the source into an `oxbase_io::WriterHandle` (bounded queue), so memory
//! stays at a few batches whatever the size.
//!
//! # Files
//!
//! The writer writes to a temporary file in the destination's directory and renames it over
//! the destination on success ([`oxbase_io::SinkFile`]); a failure or cancel deletes the
//! temporary file and leaves an existing destination untouched. Overwriting is decided by the
//! caller ([`ExportDestination::overwrite`]: the app sets it from the native save dialog, which
//! asked the user).
//!
//! # Events, cancel
//!
//! [`ExportEvent::Started`], [`ExportEvent::Progress`] at most every
//! [`ExportConfig::progress_interval`] (rows and bytes; `total_rows` known for result
//! exports), then exactly one of `Finished`, `Failed`, `Cancelled`. [`ExportService::cancel`]
//! stops the source (server-side cancel for table and query sources) and the writer; the run
//! ends with `Cancelled` within 2 s and no file is left behind.
//!
//! Owner of the bodies: the **services** stream.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use futures::StreamExt;
use oxbase_dialect::DialectRegistry;
use oxbase_dialect::sql::select_table_data;
use oxbase_driver::ResultItem;
use oxbase_io::{SinkReport, SinkWriter, WriteContext, WriteOptions, WriterHandle};
use oxbase_sql::SqlDialect;
use oxbase_types::{
    BoundStatement, DataType, Engine, Error, ErrorCategory, Filter, ObjectDef, ObjectKind, Result,
    TableDataQuery, TableDef, Value,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::convert::parse_input;
use crate::edit::CellInput;
use crate::import::{close_work_session, work_session};
use crate::query::{ErrorInfo, QueryService, RunId, RunState};
use crate::tree::TreeService;
use crate::{CancellationToken, ConnectionManager, SessionHandle, SessionId};

#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod service_tests;

/// Identifies one export run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExportId(pub Uuid);

impl ExportId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for ExportId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ExportId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What to export.
///
/// JSON: `{"type": "table" | "query" | "result", ...camelCase fields}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ExportSource {
    /// A table's rows with the grid's filter and sort (a base session).
    Table {
        session: SessionId,
        query: TableDataQuery,
    },
    /// One statement's first result set (a base session; `database` like
    /// `RunOptions::database`).
    Query {
        session: SessionId,
        sql: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        database: Option<String>,
    },
    /// A result set of a finished query run.
    Result { run_id: RunId, result_index: usize },
}

/// What to export and how (everything but the destination path).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportRequest {
    pub source: ExportSource,
    pub options: WriteOptions,
}

/// Where the file goes. Never serialized or sent to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportDestination {
    pub path: PathBuf,
    /// Replace an existing file (the user confirmed it in the save dialog).
    pub overwrite: bool,
}

/// The outcome of an export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportReport {
    pub export_id: ExportId,
    /// The destination's file name (no directories).
    pub file_name: String,
    /// Rows, bytes, sheets and writer warnings.
    #[serde(flatten)]
    pub sink: SinkReport,
    /// The source result was truncated (`max_rows` or the spill cap): not every row of the
    /// query was exported.
    pub source_truncated: bool,
    pub elapsed_ms: u64,
}

/// What an export reports, in order.
///
/// JSON: `{"type": "<camelCase variant>", ...camelCase fields}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ExportEvent {
    Started {
        export_id: ExportId,
        /// Known for result exports.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_rows: Option<u64>,
    },
    /// Cumulative counters (lossy).
    Progress {
        rows: u64,
        bytes_written: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_rows: Option<u64>,
        elapsed_ms: u64,
    },
    /// Terminal: the file is in place.
    Finished { report: ExportReport },
    /// Terminal: nothing was written to the destination.
    Failed { error: ErrorInfo, elapsed_ms: u64 },
    /// Terminal: nothing was written to the destination.
    Cancelled { elapsed_ms: u64 },
}

impl ExportEvent {
    /// `true` for `Finished`, `Failed` and `Cancelled`.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Finished { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }
}

/// A started export: its id and its event stream.
#[derive(Debug)]
pub struct ExportHandle {
    pub id: ExportId,
    pub events: mpsc::Receiver<ExportEvent>,
}

/// Tunables of an [`ExportService`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportConfig {
    /// Rows per page read from a result store.
    pub page_rows: usize,
    /// Batches queued for the writer.
    pub writer_capacity: usize,
    /// Minimum time between two `Progress` events.
    pub progress_interval: Duration,
    /// Capacity of each export's event channel.
    pub event_capacity: usize,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            page_rows: 8_192,
            writer_capacity: 4,
            progress_interval: Duration::from_millis(250),
            event_capacity: 64,
        }
    }
}

/// Exports tables, queries and results into files. See the module docs.
pub struct ExportService {
    manager: Arc<ConnectionManager>,
    query: Arc<QueryService>,
    tree: Arc<TreeService>,
    dialects: Arc<DialectRegistry>,
    config: ExportConfig,
    /// Cancel tokens of the running exports (shared with the run tasks, which remove theirs).
    running: Arc<Mutex<HashMap<ExportId, CancellationToken>>>,
}

/// Creates the writer once the schema of the rows is known (`oxbase_io::create_writer`, or a
/// test writer). Runs on a blocking thread.
pub(crate) type WriterFactory =
    Box<dyn FnOnce(SchemaRef, WriteContext) -> Result<Box<dyn SinkWriter>> + Send>;

impl ExportService {
    pub fn new(
        manager: Arc<ConnectionManager>,
        query: Arc<QueryService>,
        tree: Arc<TreeService>,
        dialects: Arc<DialectRegistry>,
        config: ExportConfig,
    ) -> Self {
        Self {
            manager,
            query,
            tree,
            dialects,
            config,
            running: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn manager(&self) -> &Arc<ConnectionManager> {
        &self.manager
    }

    pub fn query(&self) -> &Arc<QueryService> {
        &self.query
    }

    pub fn tree(&self) -> &Arc<TreeService> {
        &self.tree
    }

    pub fn dialects(&self) -> &Arc<DialectRegistry> {
        &self.dialects
    }

    pub fn config(&self) -> &ExportConfig {
        &self.config
    }

    /// Starts exporting `request` into `destination` and returns once the source is checked
    /// (the session or run exists, the statement is built) and the destination too (options
    /// valid, the file does not exist unless `overwrite`); events follow on the handle.
    /// `NotFound` for an unknown session or run, `InvalidInput` for a running result, a query
    /// that is not one statement, or bad options. The source's statement runs on the task, on
    /// a dedicated session, so a slow query never blocks this call.
    pub async fn start(
        &self,
        request: ExportRequest,
        destination: ExportDestination,
    ) -> Result<ExportHandle> {
        let path = destination.path.clone();
        let overwrite = destination.overwrite;
        let options = request.options.clone();
        let make: WriterFactory = Box::new(move |schema, context| {
            oxbase_io::create_writer(&path, overwrite, schema, &options, &context)
        });
        self.start_with(request, &destination, make).await
    }

    /// [`ExportService::start`] with a test writer (`make` gets the schema and the context).
    #[cfg(test)]
    pub(crate) async fn start_with_writer(
        &self,
        request: ExportRequest,
        destination: &ExportDestination,
        make: WriterFactory,
    ) -> Result<ExportHandle> {
        self.start_with(request, destination, make).await
    }

    async fn start_with(
        &self,
        request: ExportRequest,
        destination: &ExportDestination,
        make: WriterFactory,
    ) -> Result<ExportHandle> {
        request.options.validate()?;
        check_destination(destination)?;
        let token = CancellationToken::new();
        let source = self.resolve(&request.source, &token).await?;
        let id = ExportId::new();
        let (events, receiver) = mpsc::channel(self.config.event_capacity.max(1));
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, token.clone());
        let job = Job {
            id,
            source,
            file_name: oxbase_io::file_label(&destination.path),
            manager: Arc::clone(&self.manager),
            query: Arc::clone(&self.query),
            dialects: Arc::clone(&self.dialects),
            config: self.config.clone(),
            token,
            events,
            running: Arc::clone(&self.running),
        };
        tokio::spawn(job.run(make));
        Ok(ExportHandle {
            id,
            events: receiver,
        })
    }

    /// Cancels a running export. `false` when it is unknown or already ended.
    pub fn cancel(&self, id: ExportId) -> bool {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
            .inspect(CancellationToken::cancel)
            .is_some()
    }

    /// `true` while export `id` runs.
    pub fn is_running(&self, id: ExportId) -> bool {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&id)
    }

    /// Checks the source and builds what the run reads.
    async fn resolve(&self, source: &ExportSource, cancel: &CancellationToken) -> Result<Resolved> {
        match source {
            ExportSource::Table { session, query } => {
                self.resolve_table(*session, query.clone(), cancel).await
            }
            ExportSource::Query {
                session,
                sql,
                database,
            } => {
                let base = self.base(*session)?;
                let engine = base.summary().engine;
                let dialect = SqlDialect::for_engine(engine).ok_or_else(|| {
                    Error::unsupported(format!("{} has no SQL to export", engine.display_name()))
                })?;
                let statements = oxbase_sql::split(dialect, sql);
                let [statement] = statements.as_slice() else {
                    return Err(Error::invalid_input(format!(
                        "export one statement at a time; the text holds {}",
                        statements.len()
                    )));
                };
                let text = sql
                    .get(statement.range.clone())
                    .ok_or_else(|| Error::internal("the statement range is outside the text"))?;
                Ok(Resolved::Statement {
                    session: *session,
                    database: database.clone(),
                    statement: BoundStatement::new(text, Vec::new()),
                    engine,
                    table: None,
                })
            }
            ExportSource::Result {
                run_id,
                result_index,
            } => {
                let info = self.query.info(*run_id)?;
                if info.state == RunState::Running {
                    return Err(Error::invalid_input(
                        "the query is still running; wait for it to finish, then export",
                    ));
                }
                let result = info.results.get(*result_index).ok_or_else(|| {
                    Error::not_found(format!("query run {run_id} has no result {result_index}"))
                })?;
                let engine = self
                    .query
                    .run_session(*run_id)
                    .ok()
                    .map(|s| s.summary().engine);
                Ok(Resolved::Result {
                    run_id: *run_id,
                    result_index: *result_index,
                    total_rows: result.rows,
                    truncated: result.truncated,
                    engine,
                })
            }
        }
    }

    async fn resolve_table(
        &self,
        session: SessionId,
        mut query: TableDataQuery,
        cancel: &CancellationToken,
    ) -> Result<Resolved> {
        let base = self.base(session)?;
        let engine = base.summary().engine;
        if !matches!(
            query.table.kind,
            ObjectKind::Table | ObjectKind::View | ObjectKind::MatView
        ) {
            return Err(Error::invalid_input(format!(
                "{} is not a table or view",
                query.table.display_name()
            )));
        }
        let quoting = self.dialects.quoting(engine)?;
        let def = match self.tree.load(session, &query.table, cancel.clone()).await {
            Ok(ObjectDef::Table(table)) => Some(table),
            Err(e)
                if query.table.kind == ObjectKind::Table
                    || e.category() != ErrorCategory::Unsupported =>
            {
                return Err(e);
            }
            // A view (also one without a fetcher, which can still be read).
            Ok(ObjectDef::View(_)) | Err(_) => None,
            Ok(other) => {
                return Err(Error::invalid_input(format!(
                    "{} is not a table or view",
                    other.object_ref().display_name()
                )));
            }
        };
        if let (Some(filter), Some(table)) = (query.filter.as_mut(), def.as_ref()) {
            typed_filter_values(filter, table)?;
        }
        let statement = select_table_data(quoting.as_ref(), &query, def.as_ref())?;
        Ok(Resolved::Statement {
            session,
            database: query.table.catalog.clone(),
            statement,
            engine,
            table: def,
        })
    }

    fn base(&self, session: SessionId) -> Result<SessionHandle> {
        self.manager
            .get(session)
            .ok_or_else(|| Error::not_found(format!("no open session {session}")))
    }
}

impl fmt::Debug for ExportService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExportService")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// `InvalidInput` when the destination is a folder, or exists and may not be replaced.
fn check_destination(destination: &ExportDestination) -> Result<()> {
    let label = oxbase_io::file_label(&destination.path);
    if destination.path.is_dir() {
        return Err(Error::invalid_input(format!("{label} is a folder")));
    }
    if !destination.overwrite && destination.path.exists() {
        return Err(Error::invalid_input(format!("{label} already exists")));
    }
    Ok(())
}

/// Converts the text values the UI typed into a comparison filter to the column's type, so
/// they bind with the right type (as the grid's table data does). Pattern operators keep text.
fn typed_filter_values(filter: &mut Filter, table: &TableDef) -> Result<()> {
    match filter {
        Filter::And { items } | Filter::Or { items } => items
            .iter_mut()
            .try_for_each(|item| typed_filter_values(item, table)),
        Filter::Not { item } => typed_filter_values(item, table),
        Filter::Condition { column, op, values } => {
            let Some(def) = table.column(column) else {
                // Unknown columns are reported by the SELECT builder.
                return Ok(());
            };
            let converts = matches!(
                def.data_type,
                DataType::Bool
                    | DataType::Int { .. }
                    | DataType::Float { .. }
                    | DataType::Decimal { .. }
                    | DataType::Date
                    | DataType::Time { .. }
                    | DataType::Timestamp { .. }
                    | DataType::TimestampTz { .. }
                    | DataType::Uuid
                    | DataType::Binary { .. }
                    | DataType::VarBinary { .. }
            );
            if op.is_pattern() || !converts {
                return Ok(());
            }
            let mut as_value = def.clone();
            as_value.nullable = true;
            for value in values.iter_mut() {
                if let Value::Text(text) = value {
                    let input = CellInput::text(text.clone());
                    if let Some(typed) = parse_input(&input, &as_value)? {
                        *value = typed;
                    }
                }
            }
            Ok(())
        }
        Filter::RawSql { .. } => Ok(()),
    }
}

/// A checked source.
#[derive(Debug, Clone)]
enum Resolved {
    /// A statement run on a work session of `session` (table data or a query).
    Statement {
        session: SessionId,
        database: Option<String>,
        statement: BoundStatement,
        engine: Engine,
        table: Option<TableDef>,
    },
    /// The rows a finished run keeps.
    Result {
        run_id: RunId,
        result_index: usize,
        total_rows: u64,
        truncated: bool,
        engine: Option<Engine>,
    },
}

/// How long the writer may take to delete its temporary file after a cancel or failure.
const ABORT_WAIT: Duration = Duration::from_millis(1_500);
/// How long a server-side cancel and closing the work session may take before the terminal
/// event.
const CLOSE_WAIT: Duration = Duration::from_millis(500);

/// One started export.
struct Job {
    id: ExportId,
    source: Resolved,
    file_name: String,
    manager: Arc<ConnectionManager>,
    query: Arc<QueryService>,
    dialects: Arc<DialectRegistry>,
    config: ExportConfig,
    token: CancellationToken,
    events: mpsc::Sender<ExportEvent>,
    running: Arc<Mutex<HashMap<ExportId, CancellationToken>>>,
}

/// The writer of a run and its counters.
struct Output {
    writer: Option<WriterHandle>,
    rows: u64,
    total_rows: Option<u64>,
    last_progress: Instant,
}

impl Job {
    async fn run(self, make: WriterFactory) {
        let started = Instant::now();
        let total_rows = match &self.source {
            Resolved::Result { total_rows, .. } => Some(*total_rows),
            Resolved::Statement { .. } => None,
        };
        let _ = self
            .events
            .send(ExportEvent::Started {
                export_id: self.id,
                total_rows,
            })
            .await;
        let mut output = Output {
            writer: None,
            rows: 0,
            total_rows,
            last_progress: Instant::now(),
        };
        let outcome = match &self.source {
            Resolved::Statement {
                session,
                database,
                statement,
                engine,
                table,
            } => {
                let context = WriteContext {
                    dialects: Some(Arc::clone(&self.dialects)),
                    table: table.clone(),
                    source_engine: Some(*engine),
                };
                self.from_statement(
                    *session,
                    database.as_deref(),
                    statement,
                    context,
                    make,
                    &mut output,
                    started,
                )
                .await
            }
            Resolved::Result {
                run_id,
                result_index,
                engine,
                ..
            } => {
                let context = WriteContext {
                    dialects: Some(Arc::clone(&self.dialects)),
                    table: None,
                    source_engine: *engine,
                };
                self.from_result(*run_id, *result_index, context, make, &mut output, started)
                    .await
            }
        };
        let outcome = match outcome {
            Ok(()) => self.finish(&mut output).await,
            Err(e) => Err(e),
        };
        if outcome.is_err()
            && let Some(writer) = output.writer.take()
        {
            let _ = tokio::time::timeout(ABORT_WAIT, writer.abort()).await;
        }
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
        let elapsed_ms = millis(started);
        let event = match outcome {
            Ok(sink) => ExportEvent::Finished {
                report: ExportReport {
                    export_id: self.id,
                    file_name: self.file_name.clone(),
                    sink,
                    source_truncated: matches!(
                        self.source,
                        Resolved::Result {
                            truncated: true,
                            ..
                        }
                    ),
                    elapsed_ms,
                },
            },
            Err(e) if e.is_cancelled() || self.token.is_cancelled() => {
                ExportEvent::Cancelled { elapsed_ms }
            }
            Err(e) => ExportEvent::Failed {
                error: ErrorInfo::from(&e),
                elapsed_ms,
            },
        };
        let _ = self.events.send(event).await;
    }

    /// Completes the file (not cancellable: it is renamed into place as one step).
    async fn finish(&self, output: &mut Output) -> Result<SinkReport> {
        crate::check_cancelled(&self.token)?;
        let writer = output
            .writer
            .take()
            .ok_or_else(|| Error::internal("the export has no writer"))?;
        writer.finish().await
    }

    async fn open_writer(
        &self,
        make: WriterFactory,
        schema: SchemaRef,
        context: WriteContext,
    ) -> Result<WriterHandle> {
        let writer = tokio::task::spawn_blocking(move || make(schema, context))
            .await
            .map_err(|_| Error::internal("creating the file stopped unexpectedly"))??;
        Ok(oxbase_io::spawn_writer(writer, self.config.writer_capacity))
    }

    /// Queues `batch` for the writer (cancellable) and reports progress.
    async fn write(&self, output: &mut Output, batch: RecordBatch, started: Instant) -> Result<()> {
        let rows = u64::try_from(batch.num_rows()).unwrap_or(u64::MAX);
        let writer = output
            .writer
            .as_mut()
            .ok_or_else(|| Error::internal("the export has no writer"))?;
        tokio::select! {
            biased;
            () = self.token.cancelled() => return Err(Error::Cancelled),
            result = writer.write(batch) => result?,
        }
        output.rows += rows;
        if output.last_progress.elapsed() >= self.config.progress_interval {
            output.last_progress = Instant::now();
            let _ = self.events.try_send(ExportEvent::Progress {
                rows: output.rows,
                bytes_written: writer.bytes_written(),
                total_rows: output.total_rows,
                elapsed_ms: millis(started),
            });
        }
        Ok(())
    }

    /// Streams the first result set of `statement` on a work session into the writer.
    #[allow(
        clippy::too_many_arguments,
        reason = "the parts of a resolved statement source plus the run's state"
    )]
    async fn from_statement(
        &self,
        session: SessionId,
        database: Option<&str>,
        statement: &BoundStatement,
        context: WriteContext,
        make: WriterFactory,
        output: &mut Output,
        started: Instant,
    ) -> Result<()> {
        let work = work_session(
            &self.manager,
            &self.dialects,
            session,
            database,
            &self.token,
        )
        .await?;
        let result = self
            .stream_statement(&work.handle, statement, context, make, output, started)
            .await;
        if result.is_err() && self.token.is_cancelled() {
            let cancel = work.handle.connection().cancel_running();
            let _ = tokio::time::timeout(CLOSE_WAIT, cancel).await;
        }
        close_work_session(&self.manager, work, CLOSE_WAIT).await;
        result
    }

    async fn stream_statement(
        &self,
        session: &SessionHandle,
        statement: &BoundStatement,
        context: WriteContext,
        make: WriterFactory,
        output: &mut Output,
        started: Instant,
    ) -> Result<()> {
        let token = &self.token;
        let _guard = crate::until_cancelled(token, async { Ok(session.exclusive().await) }).await?;
        let connection = session.connection();
        let mut stream = crate::until_cancelled(
            token,
            connection.execute(&statement.sql, &statement.params, token.clone()),
        )
        .await?;
        let mut make = Some(make);
        let mut context = Some(context);
        loop {
            let next = tokio::select! {
                biased;
                () = token.cancelled() => Some(Err(Error::Cancelled)),
                item = stream.next() => item,
            };
            match next {
                None => break,
                Some(Err(e)) => return Err(e),
                Some(Ok(ResultItem::Schema(schema))) => {
                    let (Some(make), Some(context)) = (make.take(), context.take()) else {
                        // The next result set: only the first is exported.
                        break;
                    };
                    output.writer = Some(self.open_writer(make, schema, context).await?);
                }
                Some(Ok(ResultItem::Batch(batch))) => {
                    if output.writer.is_some() && batch.num_rows() > 0 {
                        self.write(output, batch, started).await?;
                    }
                }
                Some(Ok(ResultItem::RowsAffected(_) | ResultItem::Notice(_))) => {}
            }
        }
        drop(stream);
        if output.writer.is_none() {
            return Err(Error::invalid_input(
                "the statement returned no result set to export",
            ));
        }
        Ok(())
    }

    /// Pages every row the run's result store keeps into the writer.
    async fn from_result(
        &self,
        run_id: RunId,
        result_index: usize,
        context: WriteContext,
        make: WriterFactory,
        output: &mut Output,
        started: Instant,
    ) -> Result<()> {
        let page_rows = self.config.page_rows.max(1);
        let total = output.total_rows.unwrap_or(0);
        let first = crate::until_cancelled(
            &self.token,
            self.query.page(run_id, result_index, 0, page_rows),
        )
        .await?;
        output.writer = Some(self.open_writer(make, first.schema(), context).await?);
        let mut offset = u64::try_from(first.num_rows()).unwrap_or(u64::MAX);
        if first.num_rows() > 0 {
            self.write(output, first, started).await?;
        }
        while offset < total {
            crate::check_cancelled(&self.token)?;
            let page = crate::until_cancelled(
                &self.token,
                self.query.page(run_id, result_index, offset, page_rows),
            )
            .await?;
            if page.num_rows() == 0 {
                break;
            }
            offset += u64::try_from(page.num_rows()).unwrap_or(u64::MAX);
            self.write(output, page, started).await?;
        }
        Ok(())
    }
}

fn millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_io::CsvWriteOptions;
    use oxbase_types::ObjectRef;
    use pretty_assertions::assert_eq;

    #[test]
    fn request_and_report_json_shape() {
        let request = ExportRequest {
            source: ExportSource::Table {
                session: SessionId::new(),
                query: TableDataQuery::new(ObjectRef::table(None, Some("public"), "t")),
            },
            options: WriteOptions::Csv(CsvWriteOptions::default()),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["source"]["type"], "table");
        assert_eq!(json["options"]["format"], "csv");
        let back: ExportRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, request);

        let run = RunId::new();
        let source: ExportSource = serde_json::from_value(serde_json::json!({
            "type": "result", "runId": run, "resultIndex": 2
        }))
        .unwrap();
        assert_eq!(
            source,
            ExportSource::Result {
                run_id: run,
                result_index: 2
            }
        );

        let report = ExportReport {
            export_id: ExportId::new(),
            file_name: "t.csv".into(),
            sink: SinkReport {
                rows: 3,
                bytes: 12,
                ..SinkReport::default()
            },
            source_truncated: false,
            elapsed_ms: 5,
        };
        let json = serde_json::to_value(ExportEvent::Finished { report }).unwrap();
        assert_eq!(json["type"], "finished");
        assert_eq!(json["report"]["rows"], 3);
        assert_eq!(json["report"]["fileName"], "t.csv");
    }
}
