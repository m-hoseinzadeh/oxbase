//! `ExportService` with the mock driver (no Docker): a query, table data with a typed filter and
//! a sort, every row of a spilled result, the source checks, overwrite rules, a statement
//! without rows, and cancel leaving no file. The writer is the test `MemoryWriter`, which goes
//! through `oxbase_io::SinkFile` like the format writers.

use std::path::Path;

use arrow_array::cast::AsArray;
use arrow_array::types::Int64Type;
use async_trait::async_trait;
use oxbase_dialect::{Fetcher, PgQuoting};
use oxbase_driver::mock::{MockConfig, MockDriver};
use oxbase_driver::{Connection, DriverRegistry};
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_store::{ConnectionProfile, MemoryHistoryRepository, MemorySecretStore, ProfileAddress};
use oxbase_types::{ColumnDef, FilterOp, ObjectRef, ObjectSummary, SortKey};
use pretty_assertions::assert_eq;

use super::*;
use crate::ProvidedSecrets;
use crate::history::HistoryService;
use crate::import::testing::{MemoryWriter, Written};
use crate::query::{EditorId, QueryConfig, RunEvent, RunOptions, RunTarget};

/// Table `t (id int64, name text)` of `db1.public`.
struct OneTable;

#[async_trait]
impl Fetcher for OneTable {
    async fn list(
        &self,
        _c: &dyn Connection,
        parent: &ObjectRef,
        _cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        Ok(vec![ObjectSummary::new(
            parent.child(ObjectKind::Table, "t"),
        )])
    }

    async fn load(
        &self,
        _c: &dyn Connection,
        obj: &ObjectRef,
        _cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        if obj.name != "t" {
            return Err(Error::not_found(format!("no table {}", obj.name)));
        }
        let mut def = TableDef::new(obj.clone());
        def.columns = vec![
            ColumnDef::new("id", DataType::INT64),
            ColumnDef::new("name", DataType::Text),
        ];
        Ok(ObjectDef::Table(def))
    }
}

struct Harness {
    export: ExportService,
    query: Arc<QueryService>,
    driver: MockDriver,
    session: SessionId,
    dir: tempfile::TempDir,
}

async fn harness(mock: MockConfig) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let driver = MockDriver::new(Engine::Postgres).with_config(mock);
    let mut drivers = DriverRegistry::new();
    drivers.register(Arc::new(driver.clone()));
    let manager = Arc::new(ConnectionManager::new(
        drivers,
        Arc::new(MemorySecretStore::new()),
        Arc::new(RusshTunnelProvider::new()),
    ));
    let mut profile = ConnectionProfile::new(
        "export",
        Engine::Postgres,
        ProfileAddress::Network {
            host: "export.invalid".into(),
            port: Some(5432),
        },
    );
    profile.database = Some("db1".into());
    let base = manager
        .open(&profile, &ProvidedSecrets::none(), CancellationToken::new())
        .await
        .unwrap();
    let mut dialects = DialectRegistry::new();
    dialects.register_fetcher(Engine::Postgres, ObjectKind::Table, Arc::new(OneTable));
    dialects.register_quoting(Engine::Postgres, Arc::new(PgQuoting));
    let dialects = Arc::new(dialects);
    let query = Arc::new(QueryService::new(
        Arc::clone(&manager),
        Arc::clone(&dialects),
        Arc::new(HistoryService::new(
            Arc::new(MemoryHistoryRepository::new()),
        )),
        QueryConfig {
            spill_dir: dir.path().join("results"),
            memory_budget_bytes: 16 * 1024,
            rows_event_interval: Duration::ZERO,
            ..QueryConfig::default()
        },
    ));
    let tree = Arc::new(TreeService::new(
        Arc::clone(&manager),
        Arc::clone(&dialects),
    ));
    let export = ExportService::new(
        manager,
        Arc::clone(&query),
        tree,
        dialects,
        ExportConfig {
            page_rows: 64,
            progress_interval: Duration::ZERO,
            ..ExportConfig::default()
        },
    );
    Harness {
        export,
        query,
        driver,
        session: base.id(),
        dir,
    }
}

fn csv() -> WriteOptions {
    WriteOptions::Csv(oxbase_io::CsvWriteOptions::default())
}

fn memory(
    destination: &Path,
    overwrite: bool,
    written: &Written,
    delay: Duration,
) -> WriterFactory {
    let destination = destination.to_path_buf();
    let written = Arc::clone(written);
    Box::new(move |_schema, _context| MemoryWriter::create(&destination, overwrite, written, delay))
}

async fn drain(mut events: mpsc::Receiver<ExportEvent>) -> Vec<ExportEvent> {
    let mut out = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("the export must not stall")
            .expect("the export must end with a terminal event");
        let terminal = event.is_terminal();
        out.push(event);
        if terminal {
            return out;
        }
    }
}

/// Every id the writer got, in order.
fn ids(written: &Written) -> Vec<i64> {
    written
        .lock()
        .unwrap()
        .iter()
        .flat_map(|b| b.column(0).as_primitive::<Int64Type>().values().to_vec())
        .collect()
}

/// Files left in `dir` (the destination and any temporary `.part` file).
fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "results")
        .collect();
    names.sort();
    names
}

impl Harness {
    fn destination(&self, name: &str, overwrite: bool) -> ExportDestination {
        ExportDestination {
            path: self.dir.path().join(name),
            overwrite,
        }
    }

    async fn export(
        &self,
        source: ExportSource,
        name: &str,
        written: &Written,
    ) -> Vec<ExportEvent> {
        let destination = self.destination(name, false);
        let request = ExportRequest {
            source,
            options: csv(),
        };
        let make = memory(&destination.path, false, written, Duration::ZERO);
        let handle = self
            .export
            .start_with_writer(request, &destination, make)
            .await
            .unwrap();
        drain(handle.events).await
    }
}

fn finished(events: &[ExportEvent]) -> &ExportReport {
    match events.last() {
        Some(ExportEvent::Finished { report }) => report,
        other => panic!("not finished: {other:?}"),
    }
}

#[tokio::test]
async fn query_export_writes_the_first_result_set() {
    let h = harness(MockConfig::default()).await;
    let written = Written::default();
    let source = ExportSource::Query {
        session: h.session,
        sql: "SELECT id, name FROM t;".into(),
        database: None,
    };
    let events = h.export(source, "q.csv", &written).await;
    let report = finished(&events);
    assert!(matches!(
        events[0],
        ExportEvent::Started {
            total_rows: None,
            ..
        }
    ));
    assert_eq!(report.sink.rows, 12);
    assert_eq!(report.file_name, "q.csv");
    assert!(!report.source_truncated);
    assert_eq!(ids(&written), (0..12).collect::<Vec<i64>>());
    assert_eq!(files(h.dir.path()), ["q.csv"]);
    assert!(
        h.driver
            .log()
            .contains(&"SELECT id, name FROM t".to_owned()),
        "{:?}",
        h.driver.log()
    );
    assert!(!h.export.is_running(report.export_id));
    let progress = events
        .iter()
        .filter(|e| matches!(e, ExportEvent::Progress { .. }))
        .count();
    assert!(progress >= 1, "{events:?}");
}

#[tokio::test]
async fn sources_are_checked_before_starting() {
    let h = harness(MockConfig::default()).await;
    let written = Written::default();
    let start = |source: ExportSource, name: &str| {
        let destination = h.destination(name, false);
        let make = memory(&destination.path, false, &written, Duration::ZERO);
        let request = ExportRequest {
            source,
            options: csv(),
        };
        let export = &h.export;
        async move { export.start_with_writer(request, &destination, make).await }
    };
    let two = ExportSource::Query {
        session: h.session,
        sql: "SELECT 1; SELECT 2".into(),
        database: None,
    };
    let e = start(two, "a.csv").await.unwrap_err();
    assert_eq!(e.category(), ErrorCategory::InvalidInput);
    assert!(e.to_string().contains("one statement"), "{e}");

    let unknown = ExportSource::Query {
        session: SessionId::new(),
        sql: "SELECT 1".into(),
        database: None,
    };
    assert_eq!(
        start(unknown, "b.csv").await.unwrap_err().category(),
        ErrorCategory::NotFound
    );

    let run = ExportSource::Result {
        run_id: RunId::new(),
        result_index: 0,
    };
    assert_eq!(
        start(run, "c.csv").await.unwrap_err().category(),
        ErrorCategory::NotFound
    );

    let missing_table = ExportSource::Table {
        session: h.session,
        query: TableDataQuery::new(ObjectRef::table(Some("db1"), Some("public"), "nope")),
    };
    assert_eq!(
        start(missing_table, "d.csv").await.unwrap_err().category(),
        ErrorCategory::NotFound
    );

    // An existing file is kept unless the request may overwrite it.
    std::fs::write(h.dir.path().join("e.csv"), b"keep").unwrap();
    let query = ExportSource::Query {
        session: h.session,
        sql: "SELECT 1".into(),
        database: None,
    };
    let e = start(query.clone(), "e.csv").await.unwrap_err();
    assert!(e.to_string().contains("already exists"), "{e}");
    assert_eq!(std::fs::read(h.dir.path().join("e.csv")).unwrap(), b"keep");
    let destination = h.destination("e.csv", true);
    let make = memory(&destination.path, true, &written, Duration::ZERO);
    let handle = h
        .export
        .start_with_writer(
            ExportRequest {
                source: query,
                options: csv(),
            },
            &destination,
            make,
        )
        .await
        .unwrap();
    let events = drain(handle.events).await;
    assert_eq!(finished(&events).sink.rows, 12);
    assert_ne!(std::fs::read(h.dir.path().join("e.csv")).unwrap(), b"keep");
    assert_eq!(files(h.dir.path()), ["e.csv"], "no other file was created");
}

#[tokio::test]
async fn table_export_binds_the_typed_filter_and_sorts() {
    let h = harness(MockConfig::default()).await;
    let written = Written::default();
    let mut query = TableDataQuery::new(ObjectRef::table(Some("db1"), Some("public"), "t"));
    query.filter = Some(Filter::condition(
        "id",
        FilterOp::Gt,
        vec![Value::Text("5".into())],
    ));
    query.sort = vec![SortKey::desc("name")];
    let table_seen = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&table_seen);
    let destination = h.destination("t.csv", false);
    let path = destination.path.clone();
    let sink = Arc::clone(&written);
    let make: WriterFactory = Box::new(move |_schema, context| {
        *seen.lock().unwrap() = context
            .table
            .as_ref()
            .map(|t: &TableDef| t.obj.name.clone());
        MemoryWriter::create(&path, false, sink, Duration::ZERO)
    });
    let handle = h
        .export
        .start_with_writer(
            ExportRequest {
                source: ExportSource::Table {
                    session: h.session,
                    query,
                },
                options: csv(),
            },
            &destination,
            make,
        )
        .await
        .unwrap();
    let events = drain(handle.events).await;
    assert_eq!(finished(&events).sink.rows, 12);
    assert_eq!(table_seen.lock().unwrap().as_deref(), Some("t"));
    let log = h.driver.log();
    assert!(
        log.contains(
            &"SELECT * FROM \"public\".\"t\" WHERE \"id\" > $1 ORDER BY \"name\" DESC".to_owned()
        ),
        "{log:?}"
    );
}

#[tokio::test]
async fn result_export_pages_every_spilled_row() {
    let h = harness(MockConfig {
        batches: 40,
        rows_per_batch: 100,
        ..MockConfig::default()
    })
    .await;
    let target = RunTarget {
        session: h.session,
        editor: EditorId::new("e1"),
    };
    let handle = h
        .query
        .run(target, "SELECT id, name FROM big", RunOptions::default())
        .await
        .unwrap();
    let mut run_events = handle.events;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), run_events.recv())
            .await
            .unwrap()
            .unwrap();
        if event.is_terminal() {
            assert!(matches!(event, RunEvent::Finished { .. }), "{event:?}");
            break;
        }
    }
    let written = Written::default();
    let source = ExportSource::Result {
        run_id: handle.id,
        result_index: 0,
    };
    let events = h.export(source, "r.csv", &written).await;
    assert!(
        matches!(
            events[0],
            ExportEvent::Started {
                total_rows: Some(4_000),
                ..
            }
        ),
        "{events:?}"
    );
    let report = finished(&events);
    assert_eq!(report.sink.rows, 4_000);
    assert_eq!(ids(&written), (0..4_000).collect::<Vec<i64>>());

    // A running result is refused.
    let slow = harness(MockConfig {
        batches: 50,
        rows_per_batch: 10,
        batch_delay: Duration::from_millis(100),
    })
    .await;
    let running = slow
        .query
        .run(
            RunTarget {
                session: slow.session,
                editor: EditorId::new("e2"),
            },
            "SELECT id FROM slow",
            RunOptions::default(),
        )
        .await
        .unwrap();
    let destination = slow.destination("s.csv", false);
    let make = memory(&destination.path, false, &written, Duration::ZERO);
    let e = slow
        .export
        .start_with_writer(
            ExportRequest {
                source: ExportSource::Result {
                    run_id: running.id,
                    result_index: 0,
                },
                options: csv(),
            },
            &destination,
            make,
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("still running"), "{e}");
    assert!(slow.query.cancel(running.id));
}

#[tokio::test]
async fn cancel_and_failures_leave_no_file() {
    let h = harness(MockConfig {
        batches: 100,
        rows_per_batch: 10,
        batch_delay: Duration::from_millis(50),
    })
    .await;
    let written = Written::default();
    let destination = h.destination("c.csv", false);
    let make = memory(&destination.path, false, &written, Duration::ZERO);
    let handle = h
        .export
        .start_with_writer(
            ExportRequest {
                source: ExportSource::Query {
                    session: h.session,
                    sql: "SELECT id FROM slow".into(),
                    database: None,
                },
                options: csv(),
            },
            &destination,
            make,
        )
        .await
        .unwrap();
    let mut events = handle.events;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(event, ExportEvent::Progress { rows, .. } if rows >= 20) {
            break;
        }
        assert!(!event.is_terminal(), "{event:?}");
    }
    let asked = Instant::now();
    assert!(h.export.cancel(handle.id));
    let rest = drain(events).await;
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "{:?}",
        asked.elapsed()
    );
    assert!(
        matches!(rest.last(), Some(ExportEvent::Cancelled { .. })),
        "{rest:?}"
    );
    assert_eq!(files(h.dir.path()).len(), 0, "{:?}", files(h.dir.path()));
    assert!(!h.export.cancel(handle.id));

    // A statement without a result set fails, also without a file.
    let h = harness(MockConfig::default()).await;
    let written = Written::default();
    let source = ExportSource::Query {
        session: h.session,
        sql: "UPDATE t SET name = 'x'".into(),
        database: None,
    };
    let events = h.export(source, "u.csv", &written).await;
    match events.last() {
        Some(ExportEvent::Failed { error, .. }) => {
            assert!(error.message.contains("no result set"), "{error:?}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(files(h.dir.path()).len(), 0);

    // A failing statement too.
    let source = ExportSource::Query {
        session: h.session,
        sql: "FAIL now".into(),
        database: None,
    };
    let events = h.export(source, "f.csv", &written).await;
    assert!(
        matches!(events.last(), Some(ExportEvent::Failed { .. })),
        "{events:?}"
    );
    assert_eq!(files(h.dir.path()).len(), 0);
}
