//! `QueryService` with a scripted in-memory driver (no Docker): event order, multiple result
//! sets, row limits, spilling while streaming, the disk cap, cancel, close/drop cleanup, one run
//! per editor, editor sessions and history.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};

use arrow_array::{Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, SchemaRef};
use async_trait::async_trait;
use futures::stream;
use oxbase_driver::{ConnectParams, Connection, Driver, DriverRegistry, ResultItem, Tx};
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_store::{
    ConnectionProfile, HistoryQuery, HistoryRepository, MemoryHistoryRepository, MemorySecretStore,
    ProfileAddress,
};
use oxbase_types::{ServerVersion, Value};
use pretty_assertions::assert_eq;

use super::*;
use crate::ProvidedSecrets;

type Log = Arc<Mutex<Vec<String>>>;

// ---------------------------------------------------------------------------------------------
// Scripted driver
// ---------------------------------------------------------------------------------------------

/// Answers statements by their first word:
///
/// - `rows N [B [D]]`: one result set `(id Int64, name Utf8)` of N rows in batches of B
///   (default 100), D ms before each batch;
/// - `multi K N`: K result sets of N rows;
/// - `update N`: `RowsAffected(N)`; `notice text`: a warning notice and `RowsAffected(0)`;
/// - `fail`: an SQL error from `execute`; `midfail`: one batch, then an SQL error;
/// - `lose`: a connection error; `hang`: nothing until cancelled;
/// - `stuck`: a result set, then nothing; a cancel closes the connection and the stream never
///   ends (a driver that gives up on a statement it cannot stop);
/// - `select <any of the above>`: the same (the splitter calls it a query);
/// - anything else: `RowsAffected(0)`.
///
/// A stream that sees its statement cancelled logs `cancelled`. Connecting to the database
/// `hang` never finishes.
struct ScriptDriver {
    log: Log,
    engine: Engine,
}

#[async_trait]
impl Driver for ScriptDriver {
    fn engine(&self) -> Engine {
        self.engine
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        let database = params.database.clone();
        self.log
            .lock()
            .unwrap()
            .push(format!("connect {}", database.as_deref().unwrap_or("-")));
        if database.as_deref() == Some("hang") {
            std::future::pending::<()>().await;
        }
        Ok(Box::new(ScriptConnection {
            database,
            version: ServerVersion::new(16, 0, 0),
            log: self.log.clone(),
            closed: Arc::new(AtomicBool::new(false)),
        }))
    }
}

struct ScriptConnection {
    database: Option<String>,
    version: ServerVersion,
    log: Log,
    closed: Arc<AtomicBool>,
}

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
    ]))
}

fn rows_batch(start: u64, n: u64) -> RecordBatch {
    let ids: Vec<i64> = (start..start + n)
        .map(|i| i64::try_from(i).unwrap())
        .collect();
    let names: Vec<String> = ids.iter().map(|i| format!("row {i}")).collect();
    RecordBatch::try_new(
        schema(),
        vec![
            Arc::new(Int64Array::from(ids)),
            Arc::new(StringArray::from(names)),
        ],
    )
    .unwrap()
}

enum Step {
    Item(ResultItem),
    Rows { start: u64, n: u64 },
    Fail(Error),
    Hang,
    Stuck,
}

fn arg(words: &[&str], i: usize, default: u64) -> u64 {
    words.get(i).and_then(|w| w.parse().ok()).unwrap_or(default)
}

#[async_trait]
impl Connection for ScriptConnection {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn server_version(&self) -> &ServerVersion {
        &self.version
    }

    fn current_database(&self) -> Option<String> {
        self.database.clone()
    }

    async fn execute(
        &self,
        sql: &str,
        _params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::connection("closed"));
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        self.log.lock().unwrap().push(format!(
            "{}: {sql}",
            self.database.as_deref().unwrap_or("-")
        ));
        let mut words: Vec<&str> = sql.split_whitespace().collect();
        if words.len() > 1 && words[0] == "select" {
            words.remove(0);
        }
        let mut steps = VecDeque::new();
        let mut delay = Duration::ZERO;
        match words.first().copied().unwrap_or("") {
            "rows" => {
                let total = arg(&words, 1, 0);
                let per = arg(&words, 2, 100).max(1);
                delay = Duration::from_millis(arg(&words, 3, 0));
                steps.push_back(Step::Item(ResultItem::Schema(schema())));
                let mut start = 0;
                while start < total {
                    let n = per.min(total - start);
                    steps.push_back(Step::Rows { start, n });
                    start += n;
                }
            }
            "multi" => {
                for _ in 0..arg(&words, 1, 1) {
                    steps.push_back(Step::Item(ResultItem::Schema(schema())));
                    steps.push_back(Step::Rows {
                        start: 0,
                        n: arg(&words, 2, 1),
                    });
                }
            }
            "update" => steps.push_back(Step::Item(ResultItem::RowsAffected(arg(&words, 1, 0)))),
            "notice" => {
                steps.push_back(Step::Item(ResultItem::Notice(Notice::warning(
                    words[1..].join(" "),
                ))));
                steps.push_back(Step::Item(ResultItem::RowsAffected(0)));
            }
            "fail" => return Err(Error::sql(Some("XX000"), format!("failed: {sql}"))),
            "lose" => return Err(Error::connection("the server went away")),
            "midfail" => {
                steps.push_back(Step::Item(ResultItem::Schema(schema())));
                steps.push_back(Step::Rows { start: 0, n: 5 });
                steps.push_back(Step::Fail(Error::sql(Some("22012"), "division by zero")));
            }
            "hang" => steps.push_back(Step::Hang),
            "stuck" => {
                steps.push_back(Step::Item(ResultItem::Schema(schema())));
                steps.push_back(Step::Stuck);
            }
            _ => steps.push_back(Step::Item(ResultItem::RowsAffected(0))),
        }
        let log = self.log.clone();
        let closed = Arc::clone(&self.closed);
        Ok(
            stream::unfold((steps, cancel, false), move |(mut steps, cancel, done)| {
                let log = log.clone();
                let closed = Arc::clone(&closed);
                async move {
                    if done {
                        return None;
                    }
                    let step = steps.pop_front()?;
                    if !delay.is_zero() {
                        tokio::select! {
                            () = cancel.cancelled() => {}
                            () = tokio::time::sleep(delay) => {}
                        }
                    }
                    if cancel.is_cancelled() {
                        log.lock().unwrap().push("cancelled".into());
                        return Some((Err(Error::Cancelled), (steps, cancel, true)));
                    }
                    let item = match step {
                        Step::Item(item) => Ok(item),
                        Step::Rows { start, n } => Ok(ResultItem::Batch(rows_batch(start, n))),
                        Step::Fail(e) => Err(e),
                        Step::Hang => {
                            cancel.cancelled().await;
                            Err(Error::Cancelled)
                        }
                        Step::Stuck => {
                            cancel.cancelled().await;
                            closed.store(true, Ordering::SeqCst);
                            std::future::pending::<()>().await;
                            Err(Error::Cancelled)
                        }
                    };
                    let done = item.is_err();
                    Some((item, (steps, cancel, done)))
                }
            })
            .boxed(),
        )
    }

    async fn cancel_running(&self) -> Result<()> {
        Ok(())
    }

    async fn begin(&self) -> Result<Box<dyn Tx>> {
        Err(Error::unsupported("no transactions"))
    }

    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

struct Harness {
    service: QueryService,
    manager: Arc<ConnectionManager>,
    session: SessionId,
    log: Log,
    repo: Arc<MemoryHistoryRepository>,
    spill: tempfile::TempDir,
}

impl Harness {
    async fn new(configure: impl FnOnce(&mut QueryConfig)) -> Self {
        let mut profile = ConnectionProfile::new(
            "script",
            Engine::Postgres,
            ProfileAddress::Network {
                host: "script.invalid".into(),
                port: Some(5432),
            },
        );
        profile.database = Some("db1".into());
        Self::with_profile(profile, configure).await
    }

    /// A harness whose base session is opened from `profile` (on the scripted driver).
    async fn with_profile(
        profile: ConnectionProfile,
        configure: impl FnOnce(&mut QueryConfig),
    ) -> Self {
        let log = Log::default();
        let mut drivers = DriverRegistry::new();
        drivers.register(Arc::new(ScriptDriver {
            log: log.clone(),
            engine: profile.engine,
        }));
        let manager = Arc::new(ConnectionManager::new(
            drivers,
            Arc::new(MemorySecretStore::new()),
            Arc::new(RusshTunnelProvider::new()),
        ));
        let base = manager
            .open(&profile, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .unwrap();
        let spill = tempfile::tempdir().unwrap();
        let mut config = QueryConfig {
            spill_dir: spill.path().join("results"),
            rows_event_interval: Duration::ZERO,
            ..QueryConfig::default()
        };
        configure(&mut config);
        let repo = Arc::new(MemoryHistoryRepository::new());
        let service = QueryService::new(
            Arc::clone(&manager),
            Arc::new(DialectRegistry::new()),
            Arc::new(HistoryService::new(repo.clone())),
            config,
        );
        Self {
            service,
            manager,
            session: base.id(),
            log,
            repo,
            spill,
        }
    }

    fn target(&self, editor: &str) -> RunTarget {
        RunTarget {
            session: self.session,
            editor: EditorId::new(editor),
        }
    }

    async fn start(&self, editor: &str, script: &str, options: RunOptions) -> RunHandle {
        self.service
            .run(self.target(editor), script, options)
            .await
            .unwrap()
    }

    /// Runs `script` and collects every event up to the terminal one.
    async fn run(&self, editor: &str, script: &str, options: RunOptions) -> (RunId, Vec<RunEvent>) {
        let handle = self.start(editor, script, options).await;
        (handle.id, drain(handle.events).await)
    }

    fn run_dirs(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.spill.path().join("results")) else {
            return Vec::new();
        };
        entries.flatten().map(|e| e.path()).collect()
    }

    fn statements_run(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|l| !l.starts_with("connect ") && *l != "cancelled")
            .cloned()
            .collect()
    }
}

async fn drain(mut events: mpsc::Receiver<RunEvent>) -> Vec<RunEvent> {
    let mut out = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("the run must not stall")
            .expect("the run must end with a terminal event");
        let terminal = event.is_terminal();
        out.push(event);
        if terminal {
            return out;
        }
    }
}

/// A compact form of the event sequence, without rows events (lossy) and timings.
fn shape(events: &[RunEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Started { statements, .. } => Some(format!("started {}", statements.len())),
            RunEvent::StatementStarted { statement_index } => {
                Some(format!("stmt {statement_index}"))
            }
            RunEvent::ResultSet {
                result_index,
                statement_index,
                columns,
                ..
            } => Some(format!(
                "result {result_index} of {statement_index} ({} cols)",
                columns.len()
            )),
            RunEvent::Rows { .. } => None,
            RunEvent::ResultDone {
                result_index,
                total_rows,
                truncated,
            } => Some(format!(
                "done {result_index}: {total_rows}{}",
                if *truncated { " truncated" } else { "" }
            )),
            RunEvent::Notice { level, message, .. } => Some(format!("notice {level:?}: {message}")),
            RunEvent::StatementDone {
                statement_index,
                rows_affected,
                ..
            } => Some(format!("ok {statement_index} {rows_affected:?}")),
            RunEvent::StatementFailed {
                statement_index,
                error,
                ..
            } => Some(format!("failed {statement_index} {:?}", error.category)),
            RunEvent::Finished { .. } => Some("finished".into()),
            RunEvent::Failed { error, .. } => Some(format!("run failed {:?}", error.category)),
            RunEvent::Cancelled { .. } => Some("cancelled".into()),
        })
        .collect()
}

fn ids(batch: &RecordBatch) -> Vec<i64> {
    batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap()
        .values()
        .to_vec()
}

fn check_rows(batch: &RecordBatch, first: i64) {
    let ids = ids(batch);
    let names = batch
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(*id, first + i64::try_from(i).unwrap());
        assert_eq!(names.value(i), format!("row {id}"));
    }
}

fn no_history() -> RunOptions {
    RunOptions {
        record_history: false,
        ..RunOptions::default()
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn multi_statement_scripts_report_every_step_in_order() {
    let h = Harness::new(|_| {}).await;
    let script = "rows 250 100; update 7;\nnotice careful; multi 2 3; select";
    let (run, events) = h.run("tab", script, no_history()).await;
    assert_eq!(
        shape(&events),
        [
            "started 5",
            "stmt 0",
            "result 0 of 0 (2 cols)",
            "done 0: 250",
            "ok 0 None",
            "stmt 1",
            "ok 1 Some(7)",
            "stmt 2",
            "notice Warning: careful",
            "ok 2 Some(0)",
            "stmt 3",
            "result 1 of 3 (2 cols)",
            "done 1: 3",
            "result 2 of 3 (2 cols)",
            "done 2: 3",
            "ok 3 None",
            "stmt 4",
            "ok 4 Some(0)",
            "finished",
        ]
    );
    let RunEvent::Started { run_id, statements } = &events[0] else {
        panic!("first event is started");
    };
    assert_eq!(*run_id, run);
    assert_eq!(statements[1].start_byte, script.find("update").unwrap());
    let RunEvent::ResultSet {
        statement_range,
        columns,
        ..
    } = &events[2]
    else {
        panic!("result set expected");
    };
    assert_eq!(statement_range, &statements[0]);
    assert_eq!(columns[0].name, "id");
    // Rows events are cumulative and end at the total.
    let rows: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Rows {
                result_index: 0,
                total_so_far,
            } => Some(*total_so_far),
            _ => None,
        })
        .collect();
    assert!(rows.windows(2).all(|w| w[0] < w[1]), "{rows:?}");
    assert_eq!(rows.last(), Some(&250));

    // Every result pages, also across batch boundaries.
    check_rows(&h.service.page(run, 0, 95, 10).await.unwrap(), 95);
    assert_eq!(
        h.service.page(run, 0, 240, 100).await.unwrap().num_rows(),
        10
    );
    assert_eq!(h.service.page(run, 0, 400, 10).await.unwrap().num_rows(), 0);
    check_rows(&h.service.page(run, 2, 0, 10).await.unwrap(), 0);
    let err = h.service.page(run, 3, 0, 10).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);
    let ipc = h.service.page_ipc(run, 1, 0, 10).await.unwrap();
    assert!(ipc.len() > 8, "an IPC stream with a schema and a batch");

    let info = h.service.info(run).unwrap();
    assert_eq!(info.state, RunState::Finished);
    assert_eq!(info.statements.len(), 5);
    assert_eq!(
        info.results
            .iter()
            .map(|r| (r.statement_index, r.rows, r.complete, r.truncated))
            .collect::<Vec<_>>(),
        [
            (0, 250, true, false),
            (3, 3, true, false),
            (3, 3, true, false)
        ]
    );
    assert!(!h.service.cancel(run), "an ended run cannot be cancelled");

    // All statements ran on one child session, on the base's database, not on the base.
    let log = h.log.lock().unwrap().clone();
    assert_eq!(log.iter().filter(|l| l.starts_with("connect")).count(), 2);
    assert!(h.statements_run().iter().all(|l| l.starts_with("db1: ")));
}

#[tokio::test]
async fn errors_stop_the_script_unless_asked_to_continue() {
    let h = Harness::new(|_| {}).await;
    let (_, events) = h
        .run("a", "update 1; fail now; update 2", no_history())
        .await;
    assert_eq!(
        shape(&events),
        [
            "started 3",
            "stmt 0",
            "ok 0 Some(1)",
            "stmt 1",
            "failed 1 Sql",
            "run failed Sql"
        ]
    );
    let RunEvent::StatementFailed { error, .. } = &events[4] else {
        panic!("statement failed expected");
    };
    assert_eq!(error.message, "SQL error [XX000]: failed: fail now");

    let options = RunOptions {
        stop_on_error: false,
        ..no_history()
    };
    let (run, events) = h
        .run(
            "a",
            "midfail; update 2; fail again; update 3",
            options.clone(),
        )
        .await;
    assert_eq!(
        shape(&events),
        [
            "started 4",
            "stmt 0",
            "result 0 of 0 (2 cols)",
            "done 0: 5 truncated",
            "failed 0 Sql",
            "stmt 1",
            "ok 1 Some(2)",
            "stmt 2",
            "failed 2 Sql",
            "stmt 3",
            "ok 3 Some(3)",
            "finished"
        ]
    );
    // Rows received before the error stay pageable.
    check_rows(&h.service.page(run, 0, 0, 100).await.unwrap(), 0);

    // A lost session ends the run even when continuing on errors.
    let (_, events) = h.run("a", "lose; update 1", options).await;
    assert_eq!(
        shape(&events),
        [
            "started 2",
            "stmt 0",
            "failed 0 Connection",
            "run failed Connection"
        ]
    );

    // An empty script finishes right away.
    let (_, events) = h.run("a", "  ;\n ", no_history()).await;
    assert_eq!(shape(&events), ["started 0", "finished"]);
}

#[tokio::test]
async fn max_rows_truncates_and_the_run_goes_on() {
    let h = Harness::new(|_| {}).await;
    let options = RunOptions {
        max_rows: Some(150),
        ..no_history()
    };
    let (run, events) = h
        .run("a", "rows 1000 100; rows 120 100; update 4", options)
        .await;
    let seen = shape(&events);
    assert_eq!(
        seen,
        [
            "started 3",
            "stmt 0",
            "result 0 of 0 (2 cols)",
            "notice Warning: result 1 stopped after 150 rows: the row limit was reached",
            "done 0: 150 truncated",
            "ok 0 None",
            "stmt 1",
            "result 1 of 1 (2 cols)",
            "done 1: 120",
            "ok 1 None",
            "stmt 2",
            "ok 2 Some(4)",
            "finished"
        ]
    );
    let page = h.service.page(run, 0, 100, 100).await.unwrap();
    assert_eq!(page.num_rows(), 50);
    check_rows(&page, 100);
}

#[tokio::test]
async fn results_spill_past_the_memory_budget_and_page_while_streaming() {
    let h = Harness::new(|c| c.memory_budget_bytes = 64 * 1024).await;
    let handle = h.start("a", "rows 20000 500 2", no_history()).await;
    let run = handle.id;
    let mut events = handle.events;
    let mut paged_while_streaming = 0;
    let mut saw_spill = false;
    let mut saw_incomplete = false;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        if let RunEvent::Rows { total_so_far, .. } = event {
            if total_so_far < 20_000 {
                saw_incomplete |= !h.service.info(run).unwrap().results[0].complete;
                // Pages from the start (spilled) and the end (in memory) are right.
                check_rows(&h.service.page(run, 0, 0, 10).await.unwrap(), 0);
                let tail = total_so_far.saturating_sub(5);
                let page = h.service.page(run, 0, tail, 100).await.unwrap();
                assert!(page.num_rows() >= 5);
                check_rows(&page, i64::try_from(tail).unwrap());
                paged_while_streaming += 1;
                saw_spill |= h.service.disk_budget().used() > 0;
            }
        } else if event.is_terminal() {
            assert!(matches!(event, RunEvent::Finished { .. }), "{event:?}");
            break;
        }
    }
    assert!(paged_while_streaming > 5, "{paged_while_streaming}");
    assert!(
        saw_incomplete,
        "pages were read before the result was complete"
    );
    assert!(saw_spill, "the result spilled while it was still streaming");
    let dir = &h.run_dirs()[0];
    let files = std::fs::read_dir(dir).unwrap().count();
    assert!(files > 1, "spill segments are files in the run directory");

    // Random pages across the whole range.
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..200 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let offset = x % 20_000;
        let page = h.service.page(run, 0, offset, 300).await.unwrap();
        assert_eq!(
            u64::try_from(page.num_rows()).unwrap(),
            300.min(20_000 - offset)
        );
        check_rows(&page, i64::try_from(offset).unwrap());
    }
    // The limit is clamped.
    let all = h.service.page(run, 0, 0, usize::MAX).await.unwrap();
    assert_eq!(all.num_rows(), 20_000);
    check_rows(&all, 0);

    // Closing deletes the run directory and gives the disk back.
    h.service.close(run).await.unwrap();
    assert!(!dir.exists());
    assert_eq!(h.service.disk_budget().used(), 0);
    let err = h.service.page(run, 0, 0, 1).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);
    assert!(h.service.info(run).is_err());
    h.service.close(run).await.unwrap();
}

#[tokio::test]
async fn the_disk_cap_truncates_and_the_run_goes_on() {
    let h = Harness::new(|c| {
        c.memory_budget_bytes = 16 * 1024;
        c.disk_cap_bytes = 1;
    })
    .await;
    let (run, events) = h.run("a", "rows 5000 100; update 1", no_history()).await;
    let seen = shape(&events);
    assert!(
        seen.iter()
            .any(|s| s.starts_with("notice Warning: result 1 stopped after")
                && s.contains("disk space")),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|s| s.starts_with("done 0:") && s.ends_with("truncated"))
    );
    assert_eq!(
        &seen[seen.len() - 3..],
        ["stmt 1", "ok 1 Some(1)", "finished"]
    );
    let info = h.service.info(run).unwrap();
    let kept = info.results[0].rows;
    assert!(kept > 0 && kept < 5000, "{kept}");
    assert!(info.results[0].truncated);
    check_rows(&h.service.page(run, 0, 0, 10_000).await.unwrap(), 0);
    assert_eq!(h.service.disk_budget().used(), 0);
}

#[tokio::test]
async fn cancel_ends_the_run_quickly_and_keeps_the_rows() {
    let h = Harness::new(|_| {}).await;
    let handle = h
        .start("a", "rows 1000000 100 5; update 1", RunOptions::default())
        .await;
    let run = handle.id;
    let mut events = handle.events;
    loop {
        if let RunEvent::Rows { total_so_far, .. } = events.recv().await.unwrap()
            && total_so_far >= 500
        {
            break;
        }
    }
    let clock = Instant::now();
    assert!(h.service.cancel(run));
    let rest = drain(events).await;
    assert!(
        clock.elapsed() < Duration::from_secs(2),
        "{:?}",
        clock.elapsed()
    );
    let seen = shape(&rest);
    assert_eq!(&seen[seen.len() - 2..], ["failed 0 Cancelled", "cancelled"]);
    assert!(
        seen.iter()
            .any(|s| s.starts_with("done 0:") && s.ends_with("truncated"))
    );
    let info = h.service.info(run).unwrap();
    assert_eq!(info.state, RunState::Cancelled);
    assert!(info.results[0].truncated && info.results[0].complete);
    let kept = info.results[0].rows;
    assert!(kept >= 500);
    check_rows(&h.service.page(run, 0, 0, 500).await.unwrap(), 0);
    assert!(!h.service.cancel(run));
    // The second statement never ran; the editor's session answers the next run.
    assert!(!h.statements_run().iter().any(|l| l.ends_with("update 1")));
    let (_, events) = h.run("a", "update 3", no_history()).await;
    assert_eq!(shape(&events).last().unwrap(), "finished");

    // A statement that waits for the server is cancelled too.
    let handle = h.start("a", "hang", no_history()).await;
    let mut events = handle.events;
    assert!(matches!(
        events.recv().await,
        Some(RunEvent::Started { .. })
    ));
    assert!(matches!(
        events.recv().await,
        Some(RunEvent::StatementStarted { .. })
    ));
    let clock = Instant::now();
    assert!(h.service.cancel(handle.id));
    let rest = drain(events).await;
    assert!(clock.elapsed() < Duration::from_secs(2));
    assert_eq!(shape(&rest), ["failed 0 Cancelled", "cancelled"]);
}

#[tokio::test]
async fn one_run_per_editor_and_earlier_runs_close() {
    let h = Harness::new(|_| {}).await;
    let busy = h.start("a", "hang", no_history()).await;
    let err = h
        .service
        .run(h.target("a"), "update 1", no_history())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    // Another editor runs meanwhile, on its own session.
    let (other, events) = h.run("b", "rows 3", no_history()).await;
    assert_eq!(shape(&events).last().unwrap(), "finished");
    assert!(h.service.cancel(busy.id));
    drain(busy.events).await;

    // A new run on "b" closes its earlier run, unless asked to keep it.
    let (kept, _) = h.run("b", "rows 3", no_history()).await;
    assert!(h.service.info(other).is_err(), "closed by the next run");
    let keep = RunOptions {
        keep_previous: true,
        ..no_history()
    };
    h.run("b", "rows 3", keep).await;
    assert!(h.service.info(kept).is_ok());

    // Bad editor ids and unknown sessions fail before anything runs.
    let err = h
        .service
        .run(
            RunTarget {
                session: h.session,
                editor: EditorId::new(""),
            },
            "x",
            no_history(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    let err = h
        .service
        .run(
            RunTarget {
                session: SessionId::new(),
                editor: EditorId::new("c"),
            },
            "x",
            no_history(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);

    // Two editors, two children; neither is listed as an open session.
    assert_eq!(h.manager.list_open().len(), 1);
    let children = h
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("connect"))
        .count();
    assert_eq!(children, 3, "base + one child per editor");

    // Closing an editor closes its runs and its session.
    h.service.close_editor(&EditorId::new("b")).await.unwrap();
    assert!(h.service.info(kept).is_err());
    h.service
        .close_editor(&EditorId::new("nope"))
        .await
        .unwrap();
    let (_, events) = h.run("b", "update 1", no_history()).await;
    assert_eq!(shape(&events).last().unwrap(), "finished");
    let children = h
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("connect"))
        .count();
    assert_eq!(children, 4, "a closed editor gets a new session");
}

#[tokio::test]
async fn editors_follow_the_requested_database() {
    let h = Harness::new(|_| {}).await;
    let on = |db: &str| RunOptions {
        database: Some(db.to_owned()),
        ..no_history()
    };
    h.run("a", "update 1", no_history()).await;
    h.run("a", "update 2", on("db2")).await;
    h.run("a", "update 3", no_history()).await;
    h.run("a", "update 4", on("db2")).await;
    h.run("a", "update 5", on("db1")).await;
    assert_eq!(
        h.log.lock().unwrap().clone(),
        [
            "connect db1",
            "connect db1",
            "db1: update 1",
            "connect db2",
            "db2: update 2",
            "db2: update 3",
            "db2: update 4",
            "connect db1",
            "db1: update 5",
        ]
    );
    // The base session closing takes the editor's session along; the next run fails cleanly.
    h.manager.close(h.session).await.unwrap();
    let err = h
        .service
        .run(h.target("a"), "update 6", no_history())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);
}

#[tokio::test]
async fn history_records_each_statement_that_started() {
    let h = Harness::new(|_| {}).await;
    let options = RunOptions {
        stop_on_error: false,
        ..RunOptions::default()
    };
    let script = "rows 7; update 2; fail ALTER USER x PASSWORD 'marker-secret-9'; hang";
    let handle = h.start("a", script, options).await;
    let mut events = handle.events;
    loop {
        let event = events.recv().await.unwrap();
        if event == (RunEvent::StatementStarted { statement_index: 3 }) {
            break;
        }
    }
    h.service.cancel(handle.id);
    drain(events).await;
    let mut entries = h.repo.list(&HistoryQuery::default()).await.unwrap();
    entries.reverse();
    assert_eq!(
        entries
            .iter()
            .map(|e| (e.status, e.rows, e.database.as_deref()))
            .collect::<Vec<_>>(),
        [
            (HistoryStatus::Ok, Some(7), Some("db1")),
            (HistoryStatus::Ok, Some(2), Some("db1")),
            (HistoryStatus::Error, None, Some("db1")),
            (HistoryStatus::Cancelled, None, Some("db1")),
        ]
    );
    assert_eq!(entries[0].sql, "rows 7");
    for e in &entries {
        let text = serde_json::to_string(e).unwrap();
        assert!(!text.contains("marker-secret-9"), "{text}");
    }
    assert!(entries[2].sql.contains("'***'"));
    assert!(entries[2].error.as_deref().unwrap().contains("'***'"));

    // Turning history off, or one run asking not to record, writes nothing.
    h.run("a", "update 1", no_history()).await;
    h.service
        .history()
        .set_settings(&oxbase_store::HistorySettings {
            enabled: false,
            max_entries: 0,
        })
        .await
        .unwrap();
    h.run("a", "update 1", RunOptions::default()).await;
    assert_eq!(
        h.repo.list(&HistoryQuery::default()).await.unwrap().len(),
        4
    );
}

#[tokio::test]
async fn dropping_the_service_removes_run_directories_and_stale_ones_go_on_start() {
    let h = Harness::new(|c| c.memory_budget_bytes = 1).await;
    let (run, _) = h.run("a", "rows 300", no_history()).await;
    assert_eq!(h.service.info(run).unwrap().results[0].rows, 300);
    let dirs = h.run_dirs();
    assert_eq!(dirs.len(), 1);
    assert!(std::fs::read_dir(&dirs[0]).unwrap().count() > 0);
    let Harness {
        service,
        manager,
        spill,
        ..
    } = h;
    let config = service.config().clone();
    drop(service);
    assert!(!dirs[0].exists());

    // A stale directory (older than a day) goes when a service starts; a fresh one stays.
    let stale = config.spill_dir.join("run-stale");
    let fresh = config.spill_dir.join("run-fresh");
    let other = config.spill_dir.join("keep-me");
    for d in [&stale, &fresh, &other] {
        std::fs::create_dir_all(d).unwrap();
    }
    let old = std::time::SystemTime::now() - Duration::from_secs(25 * 60 * 60);
    std::fs::File::open(&stale)
        .unwrap()
        .set_modified(old)
        .unwrap();
    std::fs::File::open(&other)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let _service = QueryService::new(
        manager,
        Arc::new(DialectRegistry::new()),
        Arc::new(HistoryService::new(
            Arc::new(MemoryHistoryRepository::new()),
        )),
        config,
    );
    assert!(!stale.exists());
    assert!(fresh.exists() && other.exists());
    drop(spill);
}

#[tokio::test]
async fn events_never_carry_values() {
    let h = Harness::new(|_| {}).await;
    let (_, events) = h.run("a", "rows 50", no_history()).await;
    let json = serde_json::to_string(&events).unwrap();
    assert!(!json.contains("row 1"), "{json}");
    let page = h.service.page(events_run(&events), 0, 0, 5).await.unwrap();
    assert_eq!(page.column(1).len(), 5);
}

fn events_run(events: &[RunEvent]) -> RunId {
    match &events[0] {
        RunEvent::Started { run_id, .. } => *run_id,
        other => panic!("unexpected first event {other:?}"),
    }
}

fn connects(h: &Harness) -> usize {
    h.log
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("connect"))
        .count()
}

#[tokio::test]
async fn a_full_query_is_cancelled_but_other_statements_run_to_their_end() {
    let h = Harness::new(|_| {}).await;
    let options = RunOptions {
        max_rows: Some(150),
        ..no_history()
    };
    // A query (the splitter's hint) is stopped on the server once its result is full.
    let (_, events) = h.run("a", "select rows 1000 100", options.clone()).await;
    assert_eq!(
        shape(&events),
        [
            "started 1",
            "stmt 0",
            "result 0 of 0 (2 cols)",
            "notice Warning: result 1 stopped after 150 rows: the row limit was reached",
            "done 0: 150 truncated",
            "ok 0 None",
            "finished"
        ]
    );
    assert_eq!(h.log.lock().unwrap().last().unwrap(), "cancelled");

    // Any other statement (it may change data) is not cancelled: it runs to its end, the rest
    // of the full result is discarded and its later results are stored.
    h.log.lock().unwrap().clear();
    let (run, events) = h.run("a", "multi 3 200", options).await;
    assert_eq!(
        shape(&events),
        [
            "started 1",
            "stmt 0",
            "result 0 of 0 (2 cols)",
            "notice Warning: result 1 stopped after 150 rows: the row limit was reached",
            "done 0: 150 truncated",
            "result 1 of 0 (2 cols)",
            "notice Warning: result 2 stopped after 150 rows: the row limit was reached",
            "done 1: 150 truncated",
            "result 2 of 0 (2 cols)",
            "notice Warning: result 3 stopped after 150 rows: the row limit was reached",
            "done 2: 150 truncated",
            "ok 0 None",
            "finished"
        ]
    );
    assert!(!h.log.lock().unwrap().iter().any(|l| l == "cancelled"));
    check_rows(&h.service.page(run, 2, 0, 1000).await.unwrap(), 0);
}

#[tokio::test]
async fn a_lost_session_is_replaced_on_the_next_run() {
    let h = Harness::new(|_| {}).await;
    h.run("a", "update 1", no_history()).await;
    assert_eq!(connects(&h), 2, "base + the editor's session");
    let (_, events) = h.run("a", "lose", no_history()).await;
    assert_eq!(shape(&events).last().unwrap(), "run failed Connection");
    let (_, events) = h.run("a", "update 2", no_history()).await;
    assert_eq!(shape(&events).last().unwrap(), "finished");
    assert_eq!(connects(&h), 3, "the lost session was replaced");
    // On the same database, and the base is untouched.
    assert_eq!(h.statements_run().last().unwrap(), "db1: update 2");
    assert!(h.manager.get(h.session).is_some());
}

#[tokio::test]
async fn closing_the_editor_stops_a_session_that_does_not_open() {
    let h = Harness::new(|_| {}).await;
    let options = RunOptions {
        database: Some("hang".into()),
        ..no_history()
    };
    let clock = Instant::now();
    let (started, closed) =
        tokio::join!(h.service.run(h.target("a"), "update 1", options), async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            h.service.close_editor(&EditorId::new("a")).await
        });
    closed.unwrap();
    assert_eq!(started.unwrap_err().category(), ErrorCategory::Cancelled);
    assert!(
        clock.elapsed() < Duration::from_secs(2),
        "{:?}",
        clock.elapsed()
    );
    assert!(h.run_dirs().is_empty(), "the run's directory is gone");
    // The editor is free again.
    let (_, events) = h.run("a", "update 1", no_history()).await;
    assert_eq!(shape(&events).last().unwrap(), "finished");
}

#[tokio::test]
async fn editors_of_a_private_in_memory_database_run_on_the_base_session() {
    let profile = ConnectionProfile::new(
        "memory",
        Engine::Sqlite,
        ProfileAddress::File {
            path: ":memory:".into(),
        },
    );
    let h = Harness::with_profile(profile, |_| {}).await;
    h.run("a", "update 1", no_history()).await;
    h.run("b", "update 2", no_history()).await;
    assert_eq!(
        connects(&h),
        1,
        "no child: it would see another, empty database"
    );
    assert_eq!(h.statements_run(), ["-: update 1", "-: update 2"]);
    // Closing an editor or losing the connection never closes the base.
    h.service.close_editor(&EditorId::new("a")).await.unwrap();
    h.run("b", "lose", no_history()).await;
    tokio::task::yield_now().await;
    assert!(h.manager.get(h.session).is_some());
    let (_, events) = h.run("a", "update 3", no_history()).await;
    assert_eq!(shape(&events).last().unwrap(), "finished");
}

#[tokio::test]
async fn a_session_closed_by_an_unfinished_cancel_is_replaced_on_the_next_statement() {
    let h = Harness::new(|_| {}).await;
    let handle = h.start("a", "stuck", no_history()).await;
    let mut events = handle.events;
    loop {
        if matches!(events.recv().await.unwrap(), RunEvent::ResultSet { .. }) {
            break;
        }
    }
    let clock = Instant::now();
    assert!(h.service.cancel(handle.id));
    let rest = drain(events).await;
    assert!(
        clock.elapsed() < Duration::from_secs(2),
        "{:?}",
        clock.elapsed()
    );
    assert_eq!(shape(&rest).last().unwrap(), "cancelled");

    // The driver closed the session over the statement it could not stop: the next statement
    // finds it closed before starting, gets a new session and runs.
    let (_, events) = h.run("a", "update 1", no_history()).await;
    let seen = shape(&events);
    assert!(
        seen.iter()
            .any(|s| s.starts_with("notice Warning: the editor's session was closed")),
        "{seen:?}"
    );
    assert_eq!(seen.last().unwrap(), "finished");
    assert_eq!(
        connects(&h),
        3,
        "base + the first session + its replacement"
    );
    assert_eq!(h.statements_run().last().unwrap(), "db1: update 1");

    // Without an unfinished stop, a closed session is not replaced mid-run.
    let (_, events) = h.run("a", "lose", no_history()).await;
    assert_eq!(shape(&events).last().unwrap(), "run failed Connection");
    assert_eq!(connects(&h), 3);
}
