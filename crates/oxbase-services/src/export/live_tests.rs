//! Live tests of the export on every SQL engine against the Docker fixtures, on scratch tables
//! `m5_t_<case>_<token>` filled by the import and dropped at the end:
//!
//! - a table with a typed filter and a sort, a query, and a 300 000-row result spilled to
//!   disk (8 MiB memory budget), each read back equal;
//! - cancel ends with `Cancelled` within 2 s and leaves no file; an existing file is not
//!   overwritten without `overwrite`;
//! - with the file readers and writers in place (read / write streams): the table exported to
//!   CSV, JSON, XLSX and Parquet and imported again into a new table with the same contents,
//!   and (PostgreSQL, SQLite) a SQL dump run against the database.
//!
//! The first group uses the test `MemoryWriter` (through `oxbase_io::SinkFile`), so it does not
//! depend on the format writers; the file group is reported as pending while `oxbase_io` has no
//! readers or writers, unless `OXBASE_SERVICES_REQUIRE_M5_IO=1`.
//!
//! ```sh
//! scripts/fixtures-up.sh
//! cargo test -p oxbase-services --lib export::live_tests -- --ignored --test-threads 1
//! ```

use std::path::Path;

use oxbase_io::{
    CsvReadOptions, CsvWriteOptions, JsonReadOptions, JsonWriteOptions, ParquetReadOptions,
    ParquetWriteOptions, ReadOptions, SqlDumpOptions, XlsxReadOptions, XlsxWriteOptions,
};
use oxbase_store::MemoryHistoryRepository;
use oxbase_types::{FilterOp, ObjectRef, SortKey};

use super::*;
use crate::edit::live_tests::{Live, report};
use crate::history::HistoryService;
use crate::import::live_tests::{
    SAMPLE_COLUMNS, assert_finished, count, drain as drain_import, import_service, norm,
    readers_ready, run_import, sample_batches, sample_request, table_rows,
};
use crate::import::testing::{MemoryReader, MemoryWriter, Written};
use crate::import::{
    ColumnMapping, ConversionOptions, ErrorPolicy, ImportRequest, ImportService, ImportSource,
    ImportTarget, TargetMode, TransactionPolicy,
};
use crate::query::{EditorId, QueryConfig, RunEvent, RunOptions, RunTarget};

/// `true` once `oxbase_io` writes files (the write stream filled its writers). Probed on a
/// thread, which a still unimplemented writer panics.
fn writers_ready(dir: &Path) -> bool {
    let path = dir.join("probe.csv");
    let probe = std::thread::spawn(move || {
        let schema = Arc::new(arrow_schema::Schema::new(vec![arrow_schema::Field::new(
            "a",
            arrow_schema::DataType::Int64,
            true,
        )]));
        match oxbase_io::create_writer(
            &path,
            false,
            schema,
            &WriteOptions::Csv(CsvWriteOptions::default()),
            &WriteContext::default(),
        ) {
            Ok(writer) => {
                writer.abort();
                true
            }
            Err(_) => false,
        }
    });
    probe.join().unwrap_or(false)
}

async fn drain(mut events: mpsc::Receiver<ExportEvent>) -> Vec<ExportEvent> {
    let mut out = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(300), events.recv())
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

#[track_caller]
fn finished(key: &str, events: &[ExportEvent]) -> ExportReport {
    match events.last() {
        Some(ExportEvent::Finished { report }) => report.clone(),
        other => panic!("{key}: the export did not finish: {other:?}"),
    }
}

/// Column `column` of every written batch, normalized.
fn written_column(written: &Written, column: usize) -> Vec<String> {
    written
        .lock()
        .unwrap()
        .iter()
        .flat_map(|batch| {
            (0..batch.num_rows())
                .map(|row| {
                    norm(&oxbase_driver::cell_value(batch.column(column).as_ref(), row).unwrap())
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn memory(destination: &Path, written: &Written, delay: Duration) -> WriterFactory {
    let destination = destination.to_path_buf();
    let written = Arc::clone(written);
    Box::new(move |_schema, _context| MemoryWriter::create(&destination, false, written, delay))
}

/// Files in `dir` other than the probe and spill directories.
fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "results" && n != "rows.bin")
        .collect();
    names.sort();
    names
}

async fn export_cycle(key: &'static str, engine: Engine) {
    let live = Live::connect(key, engine, false).await;
    let import = import_service(&live);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("rows.bin");
    std::fs::write(&file, b"rows").unwrap();
    let query = Arc::new(QueryService::new(
        Arc::clone(&live.manager),
        Arc::clone(&live.dialects),
        Arc::new(HistoryService::new(
            Arc::new(MemoryHistoryRepository::new()),
        )),
        QueryConfig {
            spill_dir: dir.path().join("results"),
            memory_budget_bytes: 8 * 1024 * 1024,
            ..QueryConfig::default()
        },
    ));
    let tree = Arc::new(TreeService::new(
        Arc::clone(&live.manager),
        Arc::clone(&live.dialects),
    ));
    let export = ExportService::new(
        Arc::clone(&live.manager),
        Arc::clone(&query),
        tree,
        Arc::clone(&live.dialects),
        ExportConfig {
            progress_interval: Duration::from_millis(50),
            ..ExportConfig::default()
        },
    );
    let main = live.table(&format!("m5_t_ex_{}", live.token));
    let big = live.table(&format!("m5_t_big_{}", live.token));
    live.drop_table(&main).await;
    live.drop_table(&big).await;
    let request = sample_request(&live, &main, TargetMode::Create, TransactionPolicy::Single);
    let reader = MemoryReader::new(&request.columns, sample_batches(1, 1_000, 500));
    assert_finished(key, &run_import(&import, &file, request, reader).await);
    let quoting = live.dialects.quoting(engine).unwrap();
    let id = quoting.quote_ident("id");
    let name = quoting.quote_ident("name");

    // A table with a typed filter and a sort.
    let mut data = TableDataQuery::new(main.clone());
    data.filter = Some(Filter::condition(
        "id",
        FilterOp::Gt,
        vec![Value::Text("500".into())],
    ));
    data.sort = vec![SortKey::desc("id")];
    let written = Written::default();
    let destination = ExportDestination {
        path: dir.path().join("table.out"),
        overwrite: false,
    };
    let handle = export
        .start_with_writer(
            ExportRequest {
                source: ExportSource::Table {
                    session: live.session,
                    query: data,
                },
                options: WriteOptions::Csv(CsvWriteOptions::default()),
            },
            &destination,
            memory(&destination.path, &written, Duration::ZERO),
        )
        .await
        .unwrap_or_else(|e| panic!("{key}: table export: {e}"));
    let summary = finished(key, &drain(handle.events).await);
    assert_eq!(summary.sink.rows, 500, "{key}");
    let ids: Vec<String> = (501..=1_000).rev().map(|i: i64| i.to_string()).collect();
    assert_eq!(written_column(&written, 0), ids, "{key}");
    assert!(destination.path.is_file(), "{key}");

    // A query.
    let written = Written::default();
    let destination = ExportDestination {
        path: dir.path().join("query.out"),
        overwrite: false,
    };
    let sql = format!(
        "SELECT {id}, {name} FROM {} WHERE {id} <= 10 ORDER BY {id}",
        live.qualified(&main)
    );
    let handle = export
        .start_with_writer(
            ExportRequest {
                source: ExportSource::Query {
                    session: live.session,
                    sql,
                    database: Some(live.catalog.clone()),
                },
                options: WriteOptions::Csv(CsvWriteOptions::default()),
            },
            &destination,
            memory(&destination.path, &written, Duration::ZERO),
        )
        .await
        .unwrap_or_else(|e| panic!("{key}: query export: {e}"));
    assert_eq!(
        finished(key, &drain(handle.events).await).sink.rows,
        10,
        "{key}"
    );
    let expected = table_rows(&live, &main, &["id", "name"]).await;
    let names: Vec<String> = expected.iter().take(10).map(|r| r[1].clone()).collect();
    assert_eq!(written_column(&written, 1), names, "{key}");

    // An existing file is not replaced without `overwrite`.
    let e = export
        .start_with_writer(
            ExportRequest {
                source: ExportSource::Query {
                    session: live.session,
                    sql: "SELECT 1".into(),
                    database: None,
                },
                options: WriteOptions::Csv(CsvWriteOptions::default()),
            },
            &destination,
            memory(&destination.path, &written, Duration::ZERO),
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("already exists"), "{key}: {e}");

    // A 300 000-row result spilled to disk, every row exported.
    let mut request = sample_request(
        &live,
        &big,
        TargetMode::Create,
        TransactionPolicy::Batches { rows: 100_000 },
    );
    request.mapping.truncate(2);
    let reader = MemoryReader::new(&request.columns, sample_batches(1, 300_000, 10_000));
    assert_finished(key, &run_import(&import, &file, request, reader).await);
    let target = RunTarget {
        session: live.session,
        editor: EditorId::new("m5-export"),
    };
    let select = format!(
        "SELECT {id}, {name} FROM {} ORDER BY {id}",
        live.qualified(&big)
    );
    let run = query
        .run(target, &select, RunOptions::default())
        .await
        .unwrap();
    let mut run_events = run.events;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(300), run_events.recv())
            .await
            .unwrap()
            .unwrap();
        if event.is_terminal() {
            assert!(
                matches!(event, RunEvent::Finished { .. }),
                "{key}: {event:?}"
            );
            break;
        }
    }
    assert!(
        query.disk_budget().used() > 0,
        "{key}: the result did not spill"
    );
    let written = Written::default();
    let destination = ExportDestination {
        path: dir.path().join("result.out"),
        overwrite: false,
    };
    let handle = export
        .start_with_writer(
            ExportRequest {
                source: ExportSource::Result {
                    run_id: run.id,
                    result_index: 0,
                },
                options: WriteOptions::Csv(CsvWriteOptions::default()),
            },
            &destination,
            memory(&destination.path, &written, Duration::ZERO),
        )
        .await
        .unwrap();
    let events = drain(handle.events).await;
    assert!(
        matches!(
            events[0],
            ExportEvent::Started {
                total_rows: Some(300_000),
                ..
            }
        ),
        "{key}: {:?}",
        events[0]
    );
    let summary = finished(key, &events);
    assert_eq!(summary.sink.rows, 300_000, "{key}");
    assert!(!summary.source_truncated, "{key}");
    let ids = written_column(&written, 0);
    assert!(
        ids.iter()
            .enumerate()
            .all(|(i, id)| *id == (i + 1).to_string()),
        "{key}: the exported ids are not 1..=300000"
    );

    // Cancel: `Cancelled` within 2 s, no file.
    let destination = ExportDestination {
        path: dir.path().join("cancelled.out"),
        overwrite: false,
    };
    let handle = export
        .start_with_writer(
            ExportRequest {
                source: ExportSource::Result {
                    run_id: run.id,
                    result_index: 0,
                },
                options: WriteOptions::Csv(CsvWriteOptions::default()),
            },
            &destination,
            memory(
                &destination.path,
                &Written::default(),
                Duration::from_millis(50),
            ),
        )
        .await
        .unwrap();
    let mut events = handle.events;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(60), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!event.is_terminal(), "{key}: {event:?}");
        if matches!(event, ExportEvent::Progress { rows, .. } if rows > 0) {
            break;
        }
    }
    let asked = Instant::now();
    assert!(export.cancel(handle.id), "{key}");
    let rest = drain(events).await;
    let latency = asked.elapsed();
    assert!(
        latency < Duration::from_secs(2),
        "{key}: cancel took {latency:?}"
    );
    assert!(
        matches!(rest.last(), Some(ExportEvent::Cancelled { .. })),
        "{key}: {rest:?}"
    );
    assert!(!destination.path.exists(), "{key}");
    assert_eq!(
        files(dir.path()),
        ["query.out", "result.out", "table.out"],
        "{key}: no partial file"
    );
    let _ = query.close(run.id).await;
    report(&format!("{key}: export sources ok, cancel {latency:?}"));

    if readers_ready() && writers_ready(dir.path()) {
        format_round_trips(&live, &import, &export, &main, dir.path()).await;
    } else {
        report(&format!(
            "{key}: file round trips PENDING (oxbase-io readers / writers missing)"
        ));
    }

    live.drop_table(&main).await;
    live.drop_table(&big).await;
}

/// The table exported to each format and imported again into a new table with the same
/// contents; (PostgreSQL, SQLite) a SQL dump run against the database.
async fn format_round_trips(
    live: &Live,
    import: &ImportService,
    export: &ExportService,
    main: &ObjectRef,
    dir: &Path,
) {
    let key = live.key;
    let original = table_rows(live, main, &SAMPLE_COLUMNS).await;
    // Every column of every format; XLSX keeps times to the millisecond (Excel serial dates,
    // digits below a millisecond dropped).
    let formats: [(&str, WriteOptions, ReadOptions, Precision); 4] = [
        (
            "csv",
            WriteOptions::Csv(CsvWriteOptions::default()),
            ReadOptions::Csv(CsvReadOptions::default()),
            Precision::Exact,
        ),
        (
            "json",
            WriteOptions::Json(JsonWriteOptions::default()),
            ReadOptions::Json(JsonReadOptions::default()),
            Precision::Exact,
        ),
        (
            "xlsx",
            WriteOptions::Xlsx(XlsxWriteOptions::default()),
            ReadOptions::Xlsx(XlsxReadOptions::default()),
            Precision::Millis,
        ),
        (
            "parquet",
            WriteOptions::Parquet(ParquetWriteOptions::default()),
            ReadOptions::Parquet(ParquetReadOptions::default()),
            Precision::Exact,
        ),
    ];
    for (ext, write, read, precision) in formats {
        let path = dir.join(format!("round.{ext}"));
        let handle = export
            .start(
                ExportRequest {
                    source: ExportSource::Table {
                        session: live.session,
                        query: TableDataQuery::new(main.clone()),
                    },
                    options: write,
                },
                ExportDestination {
                    path: path.clone(),
                    overwrite: false,
                },
            )
            .await
            .unwrap_or_else(|e| panic!("{key}: {ext}: export: {e}"));
        let written = finished(key, &drain(handle.events).await);
        assert_eq!(written.sink.rows, 1_000, "{key}: {ext}");
        let source = ImportSource {
            path: path.clone(),
            options: read.clone(),
        };
        let preview = import
            .preview(&source, CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{key}: {ext}: preview: {e}"));
        let columns = oxbase_io::ColumnSpec::from_preview(&preview);
        let mapping = columns
            .iter()
            .enumerate()
            .map(|(source, c)| ColumnMapping {
                source,
                target: Some(c.name.clone()),
                data_type: None,
            })
            .collect();
        let copy = live.table(&format!("m5_t_rt_{ext}_{}", live.token));
        live.drop_table(&copy).await;
        let request = ImportRequest {
            options: read,
            columns,
            mapping,
            target: ImportTarget {
                session: live.session,
                table: copy.clone(),
                mode: TargetMode::Create,
            },
            primary_key: Vec::new(),
            conversion: ConversionOptions::default(),
            errors: ErrorPolicy::Stop,
            transaction: TransactionPolicy::Auto,
        };
        let handle = import
            .start(source, request)
            .await
            .unwrap_or_else(|e| panic!("{key}: {ext}: import: {e}"));
        assert_finished(key, &drain_import(handle.events).await);
        let copied = table_rows(live, &copy, &SAMPLE_COLUMNS).await;
        assert_eq!(
            loose(&copied, precision),
            loose(&original, precision),
            "{key}: {ext} round trip"
        );
        live.drop_table(&copy).await;
    }

    if matches!(live.engine, Engine::Postgres | Engine::Sqlite) {
        let copy = live.table(&format!("m5_t_dump_{}", live.token));
        live.drop_table(&copy).await;
        let path = dir.join("round.sql");
        let mut options = SqlDumpOptions::new(live.engine);
        options.table = Some(copy.clone());
        let handle = export
            .start(
                ExportRequest {
                    source: ExportSource::Table {
                        session: live.session,
                        query: TableDataQuery::new(main.clone()),
                    },
                    options: WriteOptions::SqlDump(options),
                },
                ExportDestination {
                    path: path.clone(),
                    overwrite: false,
                },
            )
            .await
            .unwrap();
        finished(key, &drain(handle.events).await);
        let script = std::fs::read_to_string(&path).unwrap();
        live.sql(&script).await;
        assert_eq!(count(live, &copy).await, 1_000, "{key}: dump");
        assert_eq!(
            table_rows(live, &copy, &SAMPLE_COLUMNS).await,
            original,
            "{key}: dump round trip"
        );
        live.drop_table(&copy).await;
    }
    report(&format!("{key}: file round trips ok"));
}

/// How exactly a format keeps times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Precision {
    Exact,
    /// Times to the millisecond.
    Millis,
}

/// Normalized rows compared across formats: a timestamp at midnight is the date it names (a
/// format without a date type may bring a date back as a timestamp), and with
/// [`Precision::Millis`] times are cut to the millisecond (the XLSX writer drops the digits
/// below a millisecond, with a report warning; it does not round).
fn loose(rows: &[Vec<String>], precision: Precision) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| row.iter().map(|v| loose_value(v, precision)).collect())
        .collect()
}

fn loose_value(value: &str, precision: Precision) -> String {
    let Ok(ts) = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f") else {
        return value.to_owned();
    };
    let ts = match precision {
        Precision::Exact => ts,
        Precision::Millis => {
            let micros = ts.and_utc().timestamp_micros();
            let rounded = micros.div_euclid(1_000) * 1_000;
            match chrono::DateTime::from_timestamp_micros(rounded) {
                Some(t) => t.naive_utc(),
                None => ts,
            }
        }
    };
    if ts.time() == chrono::NaiveTime::MIN {
        ts.format("%Y-%m-%d").to_string()
    } else {
        ts.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
    }
}

#[test]
fn loose_values_compare_across_formats() {
    assert_eq!(
        loose_value("1970-02-07 00:00:00.000000", Precision::Exact),
        "1970-02-07"
    );
    assert_eq!(loose_value("1970-02-07", Precision::Exact), "1970-02-07");
    assert_eq!(
        loose_value("2000-01-01 00:00:07.000123", Precision::Exact),
        "2000-01-01 00:00:07.000123"
    );
    assert_eq!(
        loose_value("2000-01-01 00:00:07.000623", Precision::Millis),
        "2000-01-01 00:00:07.000000"
    );
    assert_eq!(
        loose_value("2000-01-01 00:00:07.001999", Precision::Millis),
        "2000-01-01 00:00:07.001000"
    );
    assert_eq!(
        loose_value("2000-01-01 00:00:07.000499", Precision::Millis),
        "2000-01-01 00:00:07.000000"
    );
    assert_eq!(loose_value("name 1", Precision::Millis), "name 1");
}

macro_rules! per_engine {
    ($($name:ident => $body:ident($key:literal, $engine:expr);)*) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            #[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
            async fn $name() {
                $body($key, $engine).await;
            }
        )*
    };
}

per_engine! {
    postgres_export_sources => export_cycle("postgres", Engine::Postgres);
    mysql_export_sources => export_cycle("mysql", Engine::MySql);
    mariadb_export_sources => export_cycle("mariadb", Engine::MariaDb);
    sqlite_export_sources => export_cycle("sqlite", Engine::Sqlite);
    mssql_export_sources => export_cycle("mssql", Engine::MsSql);
}
