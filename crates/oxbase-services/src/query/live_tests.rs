//! M2 exit-check live tests of `QueryService` against the Docker fixtures (PostgreSQL, MySQL,
//! MariaDB) and a SQLite file:
//!
//! - (a) a 1,000,000-row query (`[m2] large_result_rows`) streams completely into a run whose
//!   memory budget forces spilling; the first page is readable within 1 s of the start, 300
//!   random pages of 200 rows have a p95 latency under 50 ms, every row arrives exactly once
//!   and the run's batch memory stays within its budget;
//! - (b) cancelling after at least 200,000 rows ends the run within 2 s, the rows stay
//!   pageable and the editor's session answers `SELECT 1` afterwards;
//! - (c) a multi-statement script yields several result sets and rows-affected counts, all on
//!   one session;
//! - (d) history records each statement of a run, redacted, in the app database.
//!
//! They are unit tests (not `tests/`) so they can use the crate's fallback splitter while the
//! `sql` stream's splitter is not merged (see `test_split`). Timings go straight to stderr so
//! the CI log shows them even when the test passes.
//!
//! ```sh
//! scripts/fixtures-up.sh
//! cargo test -p oxbase-services --lib query::live_tests -- --ignored --test-threads 1
//! ```

use std::io::Write as _;
use std::path::{Path, PathBuf};

use arrow_array::{Array, Int64Array, RecordBatch, StringArray};
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_store::{
    AppDb, ConnectionProfile, HistoryQuery, MemoryHistoryRepository, MemorySecretStore,
    ProfileAddress, Secret, SecretRef, SecretSlot, SecretStore, SqliteHistoryRepository, TlsMode,
    TlsSettings,
};

use super::test_split::{real_split_available, redaction_available};
use super::*;
use crate::{ProvidedSecrets, default_dialects, default_drivers};

/// Writes a line to the process's stderr, bypassing the test harness's output capture.
fn report(line: &str) {
    let _ = writeln!(std::io::stderr().lock(), "[m2 query] {line}");
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn fixtures() -> toml::Table {
    let path = std::env::var_os("OXBASE_FIXTURES")
        .map(PathBuf::from)
        .map_or_else(|| root().join("tests/fixtures.toml"), |p| root().join(p));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.parse().expect("fixtures.toml parses")
}

fn table<'a>(t: &'a toml::Table, key: &str) -> &'a toml::Table {
    t.get(key)
        .and_then(toml::Value::as_table)
        .unwrap_or_else(|| panic!("fixtures.toml: missing table {key}"))
}

fn string(t: &toml::Table, key: &str) -> String {
    t.get(key)
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("fixtures.toml: missing string {key}"))
        .to_owned()
}

fn large_rows(fx: &toml::Table) -> u64 {
    table(fx, "m2")
        .get("large_result_rows")
        .and_then(toml::Value::as_integer)
        .and_then(|n| u64::try_from(n).ok())
        .expect("fixtures.toml: [m2] large_result_rows")
}

/// A connected service on one engine, with the memory budget that forces spilling.
struct Live {
    service: QueryService,
    session: SessionId,
    engine: Engine,
    _dirs: tempfile::TempDir,
}

const MEMORY_BUDGET: u64 = 8 * 1024 * 1024;

impl Live {
    async fn connect(key: &str, engine: Engine, history: Arc<HistoryService>) -> Self {
        let fx = fixtures();
        let dirs = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecretStore::new());
        let profile = if engine == Engine::Sqlite {
            ConnectionProfile::new(
                format!("live {key}"),
                engine,
                ProfileAddress::File {
                    path: dirs.path().join("live.sqlite"),
                },
            )
        } else {
            let e = table(table(&fx, "engines"), key);
            let mut p = ConnectionProfile::new(
                format!("live {key}"),
                engine,
                ProfileAddress::Network {
                    host: string(e, "host"),
                    port: e
                        .get("port")
                        .and_then(toml::Value::as_integer)
                        .and_then(|p| u16::try_from(p).ok()),
                },
            );
            p.user = Some(string(e, "user"));
            p.database = Some(string(e, "database"));
            let secret = p.secret_key(SecretSlot::Password);
            secrets
                .set(&secret, &Secret::new(string(e, "password")))
                .await
                .unwrap();
            p.password = Some(SecretRef::Keychain { key: secret });
            p.tls = TlsSettings {
                mode: TlsMode::Disable,
                ..TlsSettings::default()
            };
            p.connect_timeout_secs = Some(15);
            p
        };
        let manager = Arc::new(ConnectionManager::new(
            default_drivers(),
            secrets,
            Arc::new(RusshTunnelProvider::new()),
        ));
        let base = manager
            .open(&profile, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{key}: connect failed: {e}"));
        let config = QueryConfig {
            spill_dir: dirs.path().join("results"),
            memory_budget_bytes: MEMORY_BUDGET,
            ..QueryConfig::default()
        };
        let service = QueryService::new(manager, Arc::new(default_dialects()), history, config);
        Self {
            service,
            session: base.id(),
            engine,
            _dirs: dirs,
        }
    }

    async fn plain(key: &str, engine: Engine) -> Self {
        let history = Arc::new(HistoryService::new(
            Arc::new(MemoryHistoryRepository::new()),
        ));
        Self::connect(key, engine, history).await
    }

    fn target(&self) -> RunTarget {
        RunTarget {
            session: self.session,
            editor: EditorId::new("live"),
        }
    }

    async fn start(&self, script: &str, options: RunOptions) -> RunHandle {
        self.service
            .run(self.target(), script, options)
            .await
            .unwrap_or_else(|e| panic!("{}: run failed to start: {e}", self.engine))
    }

    /// Batch bytes held in memory by every result of `run`.
    fn memory_of(&self, run: RunId) -> u64 {
        self.service.registry.run(run).map_or(0, |r| {
            lock(&r.results)
                .iter()
                .map(|x| x.store.memory_bytes())
                .sum()
        })
    }
}

/// `SELECT` of `rows` rows `(id, name = 'row <id>', twice = 2 * id)`, streamed by the server.
fn large_query(engine: Engine, rows: u64) -> String {
    match engine {
        Engine::Postgres => format!(
            "SELECT g::bigint AS id, 'row ' || g AS name, g::bigint * 2 AS twice \
             FROM (SELECT generate_series(1, {rows}) AS g) AS s"
        ),
        Engine::MySql | Engine::MariaDb => {
            // A cross join of digit tables: streams without materialising the rows.
            let mut digits = rows;
            let mut terms = Vec::new();
            let mut tables = Vec::new();
            let mut scale = 1_u64;
            while digits > 1 {
                let t = tables.len();
                tables.push(format!(
                    "(SELECT 0 AS d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 \
                     UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL \
                     SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) AS t{t}"
                ));
                terms.push(format!("{scale} * t{t}.d"));
                scale *= 10;
                digits /= 10;
            }
            assert_eq!(scale, rows, "MySQL queries use a power of ten rows");
            let n = format!("(1 + {})", terms.join(" + "));
            format!(
                "SELECT CAST({n} AS SIGNED) AS id, CONCAT('row ', CAST({n} AS CHAR)) AS name, \
                 CAST({n} * 2 AS SIGNED) AS twice FROM {}",
                tables.join(" CROSS JOIN ")
            )
        }
        Engine::Sqlite => format!(
            "WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < {rows}) \
             SELECT n AS id, 'row ' || n AS name, n * 2 AS twice FROM s"
        ),
        other => panic!("no large query for {other}"),
    }
}

/// Checks every row of a page of [`large_query`]; returns the ids.
fn check_large_page(engine: Engine, batch: &RecordBatch, offset: u64) -> Vec<i64> {
    let column = |name: &str| {
        batch
            .column_by_name(name)
            .unwrap_or_else(|| panic!("{engine}: no column {name} in {:?}", batch.schema()))
    };
    let ids = column("id")
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap_or_else(|| panic!("{engine}: id is {:?}", column("id").data_type()));
    let names = column("name")
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap_or_else(|| panic!("{engine}: name is {:?}", column("name").data_type()));
    let twice = column("twice")
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap_or_else(|| panic!("{engine}: twice is {:?}", column("twice").data_type()));
    let ordered = matches!(engine, Engine::Postgres | Engine::Sqlite);
    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        let id = ids.value(i);
        assert_eq!(names.value(i), format!("row {id}"), "{engine}");
        assert_eq!(twice.value(i), id * 2, "{engine}");
        if ordered {
            let expected = i64::try_from(offset).unwrap() + i64::try_from(i).unwrap() + 1;
            assert_eq!(id, expected, "{engine}: row order");
        }
        out.push(id);
    }
    out
}

fn rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

async fn next_event(events: &mut mpsc::Receiver<RunEvent>, engine: Engine) -> RunEvent {
    tokio::time::timeout(Duration::from_secs(120), events.recv())
        .await
        .unwrap_or_else(|_| panic!("{engine}: the run stalled"))
        .unwrap_or_else(|| panic!("{engine}: the event channel closed early"))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// (a) 1M rows: stream, first page, random pages, memory, spill.
async fn large_result(key: &str, engine: Engine) {
    let live = Live::plain(key, engine).await;
    let rows = large_rows(&fixtures());
    let rss_before = rss_kib();
    let clock = Instant::now();
    let handle = live
        .start(&large_query(engine, rows), RunOptions::default())
        .await;
    let run = handle.id;
    let mut events = handle.events;
    let mut first_page: Option<Duration> = None;
    let mut peak_memory = 0;
    let mut peak_rss = rss_before.unwrap_or(0);
    let mut result_index = None;
    let total = loop {
        match next_event(&mut events, engine).await {
            RunEvent::ResultSet {
                result_index: r, ..
            } => result_index = Some(r),
            RunEvent::Rows {
                result_index: r, ..
            } if first_page.is_none() => {
                let page = live.service.page(run, r, 0, 200).await.unwrap();
                if page.num_rows() > 0 {
                    check_large_page(engine, &page, 0);
                    first_page = Some(clock.elapsed());
                }
            }
            RunEvent::ResultDone {
                total_rows,
                truncated,
                ..
            } => {
                assert!(!truncated, "{engine}");
                break total_rows;
            }
            RunEvent::StatementFailed { error, .. } | RunEvent::Failed { error, .. } => {
                panic!("{engine}: {error:?}")
            }
            _ => {}
        }
        peak_memory = peak_memory.max(live.memory_of(run));
        peak_rss = peak_rss.max(rss_kib().unwrap_or(0));
    };
    let streamed = clock.elapsed();
    loop {
        let event = next_event(&mut events, engine).await;
        if event.is_terminal() {
            assert!(
                matches!(event, RunEvent::Finished { .. }),
                "{engine}: {event:?}"
            );
            break;
        }
    }
    let result = result_index.expect("a result set");
    assert_eq!(total, rows, "{engine}: every row arrives");
    let first_page = first_page.unwrap_or_else(|| {
        // The whole result arrived before the first rows event was handled.
        clock.elapsed()
    });
    let disk_used = live.service.disk_budget().used();

    // 300 random pages of 200 rows.
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut latencies = Vec::with_capacity(300);
    for _ in 0..300 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let offset = x % (rows - 200);
        let t = Instant::now();
        let page = live.service.page(run, result, offset, 200).await.unwrap();
        latencies.push(t.elapsed());
        assert_eq!(page.num_rows(), 200, "{engine}");
        check_large_page(engine, &page, offset);
    }
    latencies.sort();
    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[latencies.len() * 95 / 100];

    // Every row exactly once (a full scan in large pages).
    let mut seen = vec![false; usize::try_from(rows).unwrap() + 1];
    let mut offset = 0;
    while offset < rows {
        let page = live
            .service
            .page(run, result, offset, 50_000)
            .await
            .unwrap();
        assert!(page.num_rows() > 0);
        for id in check_large_page(engine, &page, offset) {
            let slot = &mut seen[usize::try_from(id).unwrap()];
            assert!(!*slot, "{engine}: row {id} twice");
            *slot = true;
        }
        offset += u64::try_from(page.num_rows()).unwrap();
    }
    assert!(seen[1..].iter().all(|s| *s), "{engine}: rows missing");

    report(&format!(
        "{engine} large result: {rows} rows streamed in {:.0} ms, first page after {:.0} ms, \
         random 200-row pages p50 {:.2} ms / p95 {:.2} ms, spill {} MiB, peak batch memory \
         {:.1} MiB (budget {} MiB), RSS {} -> {} MiB",
        ms(streamed),
        ms(first_page),
        ms(p50),
        ms(p95),
        disk_used / (1024 * 1024),
        f64::from(u32::try_from(peak_memory / 1024).unwrap_or(u32::MAX)) / 1024.0,
        MEMORY_BUDGET / (1024 * 1024),
        rss_before.unwrap_or(0) / 1024,
        peak_rss / 1024,
    ));
    assert!(
        first_page < Duration::from_secs(1),
        "{engine}: first page after {first_page:?}"
    );
    assert!(
        p95 < Duration::from_millis(50),
        "{engine}: page p95 {p95:?}"
    );
    assert!(disk_used > 0, "{engine}: the result spilled to disk");
    // Budget plus the batch in flight (drivers cap batches at 8 MiB).
    assert!(
        peak_memory <= MEMORY_BUDGET + 8 * 1024 * 1024,
        "{engine}: batch memory {peak_memory}"
    );
    if let Some(before) = rss_before {
        assert!(
            peak_rss.saturating_sub(before) < 384 * 1024,
            "{engine}: RSS grew from {before} KiB to {peak_rss} KiB"
        );
    }
    live.service.close(run).await.unwrap();
    assert_eq!(
        live.service.disk_budget().used(),
        0,
        "{engine}: spill freed"
    );
}

/// (b) cancel after at least 200k rows.
async fn cancel_mid_stream(key: &str, engine: Engine) {
    let live = Live::plain(key, engine).await;
    let handle = live
        .start(&large_query(engine, 10_000_000), RunOptions::default())
        .await;
    let run = handle.id;
    let mut events = handle.events;
    let received = loop {
        match next_event(&mut events, engine).await {
            RunEvent::Rows { total_so_far, .. } if total_so_far >= 200_000 => break total_so_far,
            event if event.is_terminal() => panic!("{engine}: ended early: {event:?}"),
            _ => {}
        }
    };
    let clock = Instant::now();
    assert!(live.service.cancel(run), "{engine}");
    let terminal = loop {
        let event = next_event(&mut events, engine).await;
        if event.is_terminal() {
            break event;
        }
    };
    let stopped = clock.elapsed();
    assert!(
        matches!(terminal, RunEvent::Cancelled { .. }),
        "{engine}: {terminal:?}"
    );
    let info = live.service.info(run).unwrap();
    assert_eq!(info.state, RunState::Cancelled);
    let kept = info.results[0].rows;
    assert!(
        info.results[0].truncated && kept >= received,
        "{engine}: {info:?}"
    );
    let page = live.service.page(run, 0, 0, 200).await.unwrap();
    check_large_page(engine, &page, 0);
    assert_eq!(page.num_rows(), 200);
    let tail = live.service.page(run, 0, kept - 10, 200).await.unwrap();
    assert_eq!(tail.num_rows(), 10, "{engine}");

    // The editor's session still answers.
    let clock_next = Instant::now();
    let handle = live.start("SELECT 1 AS one", RunOptions::default()).await;
    let mut events = handle.events;
    let mut replaced = false;
    loop {
        let event = next_event(&mut events, engine).await;
        if let RunEvent::Notice { message, .. } = &event
            && message.contains("a new session was opened")
        {
            replaced = true;
        }
        if event.is_terminal() {
            assert!(
                matches!(event, RunEvent::Finished { .. }),
                "{engine}: {event:?}"
            );
            break;
        }
    }
    let one = live.service.page(handle.id, 0, 0, 10).await.unwrap();
    assert_eq!(one.num_rows(), 1, "{engine}");
    report(&format!(
        "{engine} cancel: cancelled after {received} rows ({kept} kept), run ended {:.0} ms \
         after cancel, next SELECT 1 took {:.0} ms{}",
        ms(stopped),
        ms(clock_next.elapsed()),
        if replaced {
            " (the driver had closed the editor's session after the cancel; it was replaced)"
        } else {
            " on the same session"
        }
    ));
    assert!(stopped < Duration::from_secs(2), "{engine}: {stopped:?}");
}

/// (c) a multi-statement script.
async fn multi_statement(key: &str, engine: Engine) {
    let live = Live::plain(key, engine).await;
    let script = "CREATE TEMPORARY TABLE m2q_t (id INT, name VARCHAR(20));\n\
                  INSERT INTO m2q_t (id, name) VALUES (1, 'a'), (2, 'b'), (3, 'c');\n\
                  SELECT id, name FROM m2q_t ORDER BY id;\n\
                  UPDATE m2q_t SET name = 'z' WHERE id > 1;\n\
                  SELECT COUNT(*) AS n FROM m2q_t;\n\
                  DROP TABLE m2q_t;";
    let handle = live.start(script, RunOptions::default()).await;
    let run = handle.id;
    let mut events = handle.events;
    let mut all = Vec::new();
    loop {
        let event = next_event(&mut events, engine).await;
        let terminal = event.is_terminal();
        all.push(event);
        if terminal {
            break;
        }
    }
    assert!(
        matches!(all.last(), Some(RunEvent::Finished { .. })),
        "{engine}: {all:?}"
    );
    let results: Vec<(usize, usize)> = all
        .iter()
        .filter_map(|e| match e {
            RunEvent::ResultSet {
                result_index,
                statement_index,
                ..
            } => Some((*result_index, *statement_index)),
            _ => None,
        })
        .collect();
    assert_eq!(results, [(0, 2), (1, 4)], "{engine}: {all:?}");
    let affected: BTreeMap<usize, Option<u64>> = all
        .iter()
        .filter_map(|e| match e {
            RunEvent::StatementDone {
                statement_index,
                rows_affected,
                ..
            } => Some((*statement_index, *rows_affected)),
            _ => None,
        })
        .collect();
    assert_eq!(affected.len(), 6, "{engine}: {all:?}");
    assert_eq!(affected[&1], Some(3), "{engine}: INSERT");
    assert_eq!(affected[&3], Some(2), "{engine}: UPDATE");
    let first = live.service.page(run, 0, 0, 10).await.unwrap();
    assert_eq!(first.num_rows(), 3);
    let names = first
        .column_by_name("name")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .map(|a| {
            (0..a.len())
                .map(|i| a.value(i).to_owned())
                .collect::<Vec<_>>()
        });
    assert_eq!(
        names,
        Some(vec!["a".into(), "b".into(), "c".into()]),
        "{engine}"
    );
    assert_eq!(
        live.service.page(run, 1, 0, 10).await.unwrap().num_rows(),
        1
    );
    report(&format!(
        "{engine} script: {} statements, {} result sets, rows affected {affected:?}, split by {}",
        affected.len(),
        results.len(),
        if real_split_available() {
            "oxbase_sql::split"
        } else {
            "the test stand-in (oxbase_sql::split is not on this branch)"
        }
    ));
}

/// (d) history in the app database, redacted.
async fn history_records(key: &str, engine: Engine) {
    let data = tempfile::tempdir().unwrap();
    let db = AppDb::open(data.path().join(AppDb::FILE_NAME))
        .await
        .unwrap();
    let repo = Arc::new(SqliteHistoryRepository::new(db));
    let history = Arc::new(HistoryService::new(repo));
    let live = Live::connect(key, engine, Arc::clone(&history)).await;
    let secret_statement = match engine {
        Engine::Postgres => "ALTER ROLE m2q_nobody PASSWORD 'marker-secret-77'",
        Engine::MySql | Engine::MariaDb => {
            "ALTER USER 'm2q_nobody'@'%' IDENTIFIED BY 'marker-secret-77'"
        }
        _ => "PRAGMA key = 'marker-secret-77'",
    };
    let script = format!("SELECT 'm2q-history' AS marker;\n{secret_statement};\nSELECT 2 AS two");
    let options = RunOptions {
        stop_on_error: false,
        ..RunOptions::default()
    };
    let handle = live.start(&script, options).await;
    let mut events = handle.events;
    loop {
        if next_event(&mut events, engine).await.is_terminal() {
            break;
        }
    }
    let entries = history.list(&HistoryQuery::default()).await.unwrap();
    let redactor = if redaction_available() {
        "oxbase_sql::redact_secrets"
    } else {
        // Until the `sql` stream's redaction is on this branch, `HistoryService` uses the test
        // stand-in in `test_split`; the recording path to the app database is the real one.
        "the test stand-in (oxbase_sql::redact_secrets is not on this branch)"
    };
    assert_eq!(entries.len(), 3, "{engine}: {entries:?}");
    assert_eq!(entries[2].sql, "SELECT 'm2q-history' AS marker");
    assert_eq!(entries[2].status, HistoryStatus::Ok);
    assert_eq!(entries[2].rows, Some(1));
    assert_eq!(entries[0].sql, "SELECT 2 AS two");
    for e in &entries {
        let json = serde_json::to_string(e).unwrap();
        assert!(!json.contains("marker-secret-77"), "{engine}: {json}");
    }
    assert!(
        entries[1].sql.contains("'***'"),
        "{engine}: {:?}",
        entries[1]
    );
    let found = history
        .list(&HistoryQuery {
            search: Some("M2Q-HISTORY".into()),
            ..HistoryQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(found.len(), 1, "{engine}");
    report(&format!(
        "{engine} history: {} entries, statuses {:?}, redacted by {redactor}",
        entries.len(),
        entries.iter().map(|e| e.status).collect::<Vec<_>>()
    ));
}

macro_rules! live_tests {
    ($($name:ident: $check:ident($key:literal, $engine:expr);)*) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
            async fn $name() {
                $check($key, $engine).await;
            }
        )*
    };
}

live_tests! {
    postgres_large_result: large_result("postgres", Engine::Postgres);
    postgres_cancel_mid_stream: cancel_mid_stream("postgres", Engine::Postgres);
    postgres_multi_statement: multi_statement("postgres", Engine::Postgres);
    postgres_history: history_records("postgres", Engine::Postgres);
    mysql_large_result: large_result("mysql", Engine::MySql);
    mysql_cancel_mid_stream: cancel_mid_stream("mysql", Engine::MySql);
    mysql_multi_statement: multi_statement("mysql", Engine::MySql);
    mysql_history: history_records("mysql", Engine::MySql);
    mariadb_large_result: large_result("mariadb", Engine::MariaDb);
    mariadb_cancel_mid_stream: cancel_mid_stream("mariadb", Engine::MariaDb);
    mariadb_multi_statement: multi_statement("mariadb", Engine::MariaDb);
    mariadb_history: history_records("mariadb", Engine::MariaDb);
    sqlite_large_result: large_result("sqlite", Engine::Sqlite);
    sqlite_cancel_mid_stream: cancel_mid_stream("sqlite", Engine::Sqlite);
    sqlite_multi_statement: multi_statement("sqlite", Engine::Sqlite);
    sqlite_history: history_records("sqlite", Engine::Sqlite);
}
