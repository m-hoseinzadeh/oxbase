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
use std::time::Duration;

use oxbase_dialect::DialectRegistry;
use oxbase_io::{SinkReport, WriteOptions};
use oxbase_types::{Result, TableDataQuery};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::query::{ErrorInfo, QueryService, RunId};
use crate::tree::TreeService;
use crate::{CancellationToken, ConnectionManager, SessionId};

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
    /// Cancel tokens of the running exports.
    running: Mutex<HashMap<ExportId, CancellationToken>>,
}

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
            running: Mutex::new(HashMap::new()),
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

    /// Starts exporting `request` into `destination` and returns once the source is open and
    /// the destination checked (options valid, the file does not exist unless `overwrite`);
    /// events follow on the handle. `NotFound` for an unknown session or run, `InvalidInput`
    /// for a running result, a query that is not one statement, or bad options.
    #[allow(
        clippy::unused_async,
        reason = "skeleton: the services stream fills it in"
    )]
    pub async fn start(
        &self,
        request: ExportRequest,
        destination: ExportDestination,
    ) -> Result<ExportHandle> {
        let _ = (self, request, destination);
        unimplemented!("m5: services")
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
