//! Live tests of the import on every SQL engine (PostgreSQL, MySQL, MariaDB, SQLite, SQL
//! Server) against the Docker fixtures, on scratch tables `m5_t_<case>_<token>` that are
//! dropped at the end:
//!
//! - create (one transaction) with rows of every common type, read back equal; append in
//!   batches; truncate, delete_all and replace, each checked by its row count;
//! - failures per target mode and transaction policy (`docs/m5-contracts.md` §5.2): a server
//!   error rolls a single transaction back, keeps the committed batches, empties the table on
//!   engines whose TRUNCATE commits, rolls a `DELETE` back, rolls a `CREATE` back (or drops
//!   the table where DDL commits), keeps or loses the old table of a replace per engine, and
//!   drops a table the import created;
//! - text fidelity: NULL versus empty text, astral characters, CR / LF / tab, backslashes,
//!   quotes and a long text;
//! - the skip policy with conversion errors; plan rejections;
//! - cancel ends with `Cancelled` within 2 s and leaves exactly the committed rows;
//! - with the file readers in place (read stream): every sample format into a new table and
//!   appended to it again.
//!
//! The rows come from in-memory sources so the tests do not depend on the file readers; the
//! file tests are skipped (reported as pending) while `oxbase_io` has no readers, unless
//! `OXBASE_SERVICES_REQUIRE_M5_IO=1`.
//!
//! ```sh
//! scripts/fixtures-up.sh
//! cargo test -p oxbase-services --lib import::live_tests -- --ignored --test-threads 1
//! ```

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use arrow_array::RecordBatch;
use oxbase_io::{CsvReadOptions, JsonReadOptions, RowIssue};
use oxbase_types::Value;

use super::testing::{MemoryReader, sample_batch, sample_row, sample_specs};
use super::*;
use crate::edit::live_tests::{Live, report};
use crate::query::RunEvent;

/// `true` when the file tests must run (`OXBASE_SERVICES_REQUIRE_M5_IO=1`).
fn require_io() -> bool {
    std::env::var("OXBASE_SERVICES_REQUIRE_M5_IO").is_ok_and(|v| v == "1")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

/// The committed sample `name` of `tests/data/m5`.
pub(crate) fn sample_file(name: &str) -> PathBuf {
    repo_root().join("tests/data/m5").join(name)
}

/// `true` once `oxbase_io` reads files (the read stream filled its readers). Probed on a
/// thread, which a still unimplemented reader panics.
pub(crate) fn readers_ready() -> bool {
    let path = sample_file("basic.csv");
    let probe = std::thread::spawn(move || {
        oxbase_io::preview(
            &path,
            &ReadOptions::Csv(CsvReadOptions::default()),
            PreviewLimits::default(),
            &CancellationToken::new(),
        )
        .is_ok()
    });
    let ready = probe.join().unwrap_or(false);
    assert!(
        ready || !require_io(),
        "oxbase_io cannot read files yet (OXBASE_SERVICES_REQUIRE_M5_IO=1)"
    );
    ready
}

/// A value as comparable text: numbers without trailing fraction zeros, booleans `1` / `0`,
/// dates `YYYY-MM-DD`, timestamps `YYYY-MM-DD HH:MM:SS.ffffff` (instants in UTC), texts as
/// they are unless they spell one of those.
pub(crate) fn norm(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Bool(b) => if *b { "1" } else { "0" }.to_owned(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => norm_text(&f.to_string()),
        Value::Decimal(text) | Value::Text(text) => norm_text(text),
        Value::Bytes(bytes) => bytes.iter().fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        }),
        Value::Date(d) => d.format("%Y-%m-%d").to_string(),
        Value::Time(t) => t.format("%H:%M:%S%.6f").to_string(),
        Value::Timestamp(t) => t.format("%Y-%m-%d %H:%M:%S%.6f").to_string(),
        Value::TimestampTz(t) => t.naive_utc().format("%Y-%m-%d %H:%M:%S%.6f").to_string(),
        Value::Uuid(u) => u.to_string(),
        Value::Json(j) => j.to_string(),
    }
}

/// [`norm`] of a text.
pub(crate) fn norm_text(text: &str) -> String {
    let t = text.trim();
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(ts) = chrono::NaiveDateTime::parse_from_str(t, format) {
            return ts.format("%Y-%m-%d %H:%M:%S%.6f").to_string();
        }
    }
    let digits = t.strip_prefix('-').unwrap_or(t);
    let numeric = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && digits.bytes().filter(|b| *b == b'.').count() <= 1
        && digits.bytes().next().is_some_and(|b| b.is_ascii_digit());
    if !numeric {
        return text.to_owned();
    }
    let mut out = t.to_owned();
    if out.contains('.') {
        while out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            out.pop();
        }
    }
    if out == "-0" {
        out = "0".to_owned();
    }
    out
}

/// The rows of `sql`, normalized with [`norm`].
pub(crate) async fn select(live: &Live, sql: &str) -> Vec<Vec<String>> {
    let (run, events) = live.run(sql).await;
    assert!(
        matches!(events.last(), Some(RunEvent::Finished { .. })),
        "{}: {sql}: {events:?}",
        live.key
    );
    let total = live.query.info(run).unwrap().results[0].rows;
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < total {
        let page = live.query.page(run, 0, offset, 10_000).await.unwrap();
        if page.num_rows() == 0 {
            break;
        }
        for row in 0..page.num_rows() {
            out.push(
                page.columns()
                    .iter()
                    .map(|c| norm(&oxbase_driver::cell_value(c.as_ref(), row).unwrap()))
                    .collect(),
            );
        }
        offset += u64::try_from(page.num_rows()).unwrap();
    }
    let _ = live.query.close(run).await;
    out
}

/// Rows in `table`.
pub(crate) async fn count(live: &Live, table: &ObjectRef) -> u64 {
    let q = live.qualified(table);
    let rows = select(live, &format!("SELECT COUNT(*) FROM {q}")).await;
    rows[0][0].parse().unwrap()
}

/// `SELECT <columns> FROM <table> ORDER BY <first column>`, normalized.
pub(crate) async fn table_rows(
    live: &Live,
    table: &ObjectRef,
    columns: &[&str],
) -> Vec<Vec<String>> {
    let quoting = live.dialects.quoting(live.engine).unwrap();
    let list: Vec<String> = columns.iter().map(|c| quoting.quote_ident(c)).collect();
    let sql = format!(
        "SELECT {} FROM {} ORDER BY {}",
        list.join(", "),
        live.qualified(table),
        list[0]
    );
    select(live, &sql).await
}

/// The sample rows `range`, normalized like [`norm`].
pub(crate) fn expected_rows(range: std::ops::RangeInclusive<i64>) -> Vec<Vec<String>> {
    range
        .map(|i| sample_row(i).iter().map(|v| norm_text(v)).collect())
        .collect()
}

pub(crate) const SAMPLE_COLUMNS: [&str; 6] = ["id", "name", "amount", "flag", "born", "at"];

/// An import service over the live harness.
pub(crate) fn import_service(live: &Live) -> ImportService {
    let tree = Arc::new(TreeService::new(
        Arc::clone(&live.manager),
        Arc::clone(&live.dialects),
    ));
    ImportService::new(
        Arc::clone(&live.manager),
        tree,
        Arc::clone(&live.dialects),
        ImportConfig {
            progress_interval: Duration::from_millis(50),
            ..ImportConfig::default()
        },
    )
}

/// An import of the sample columns into `table` (primary key `id`).
pub(crate) fn sample_request(
    live: &Live,
    table: &ObjectRef,
    mode: TargetMode,
    transaction: TransactionPolicy,
) -> ImportRequest {
    let columns = sample_specs();
    let mapping = columns
        .iter()
        .enumerate()
        .map(|(source, c)| ColumnMapping {
            source,
            target: Some(c.name.clone()),
            data_type: None,
        })
        .collect();
    ImportRequest {
        options: ReadOptions::Csv(CsvReadOptions::default()),
        columns,
        mapping,
        target: ImportTarget {
            session: live.session,
            table: table.clone(),
            mode,
        },
        primary_key: vec!["id".into()],
        conversion: ConversionOptions::default(),
        errors: ErrorPolicy::Stop,
        transaction,
    }
}

/// Sample rows `first..first + rows` in batches of `per_batch`.
pub(crate) fn sample_batches(
    first: i64,
    rows: usize,
    per_batch: usize,
) -> Vec<(RecordBatch, Vec<RowIssue>)> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < rows {
        let n = per_batch.min(rows - start);
        out.push((
            sample_batch(first + i64::try_from(start).unwrap(), n),
            Vec::new(),
        ));
        start += n;
    }
    out
}

/// Runs `request` reading `reader`; returns every event.
pub(crate) async fn run_import(
    import: &ImportService,
    file: &Path,
    request: ImportRequest,
    reader: MemoryReader,
) -> Vec<ImportEvent> {
    let source = ImportSource {
        path: file.to_path_buf(),
        options: request.options.clone(),
    };
    let handle = import
        .start_with_reader(&source, request, Box::new(reader))
        .await
        .unwrap_or_else(|e| panic!("import did not start: {e}"));
    drain(handle.events).await
}

pub(crate) async fn drain(mut events: mpsc::Receiver<ImportEvent>) -> Vec<ImportEvent> {
    let mut out = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(300), events.recv())
            .await
            .expect("the import must not stall")
            .expect("the import must end with a terminal event");
        let terminal = event.is_terminal();
        out.push(event);
        if terminal {
            return out;
        }
    }
}

pub(crate) fn final_report(events: &[ImportEvent]) -> &ImportReport {
    match events.last() {
        Some(
            ImportEvent::Finished { report }
            | ImportEvent::Failed { report, .. }
            | ImportEvent::Cancelled { report },
        ) => report,
        other => panic!("no terminal event: {other:?}"),
    }
}

#[track_caller]
pub(crate) fn assert_finished(key: &str, events: &[ImportEvent]) -> ImportReport {
    match events.last() {
        Some(ImportEvent::Finished { report }) => report.clone(),
        other => panic!("{key}: the import did not finish: {other:?}"),
    }
}

#[track_caller]
fn assert_failed(key: &str, events: &[ImportEvent]) -> ImportReport {
    match events.last() {
        Some(ImportEvent::Failed { report, .. }) => report.clone(),
        other => panic!("{key}: the import did not fail: {other:?}"),
    }
}

/// A file the transaction policy sizes (a few bytes: `Auto` is one transaction).
fn small_file(dir: &Path) -> PathBuf {
    let path = dir.join("rows.bin");
    std::fs::write(&path, b"rows").unwrap();
    path
}

/// Engines whose DDL commits on its own (MySQL, MariaDB).
fn ddl_commits(engine: Engine) -> bool {
    matches!(engine, Engine::MySql | Engine::MariaDb)
}

async fn import_cycle(key: &'static str, engine: Engine) {
    let live = Live::connect(key, engine, false).await;
    let import = import_service(&live);
    let dir = tempfile::tempdir().unwrap();
    let file = small_file(dir.path());
    let name = |case: &str| live.table(&format!("m5_t_{case}_{}", live.token));
    let main = name("main");
    let fresh = name("fresh");
    let skip = name("skip");
    let req = name("req");
    let slow = name("slow");
    let fid = name("fid");
    for t in [&main, &fresh, &skip, &req, &slow, &fid] {
        live.drop_table(t).await;
    }

    // Create in one transaction, rows of every common type read back equal.
    let request = sample_request(&live, &main, TargetMode::Create, TransactionPolicy::Single);
    let plan = import
        .plan(
            &ImportSource {
                path: file.clone(),
                options: request.options.clone(),
            },
            &request,
            CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{key}: plan: {e}"));
    assert!(
        plan.creates_table && plan.ddl.iter().any(|d| d.contains("CREATE")),
        "{key}: {plan:?}"
    );
    let reader = MemoryReader::new(&request.columns, sample_batches(1, 2_500, 1_000));
    let events = run_import(&import, &file, request, reader).await;
    let created = assert_finished(key, &events);
    assert_eq!(created.rows_committed, 2_500, "{key}");
    assert!(created.table_created, "{key}");
    assert_eq!(
        table_rows(&live, &main, &SAMPLE_COLUMNS).await,
        expected_rows(1..=2_500),
        "{key}: the rows read back"
    );

    // Append in batches.
    let request = sample_request(
        &live,
        &main,
        TargetMode::Append,
        TransactionPolicy::Batches { rows: 1_000 },
    );
    let reader = MemoryReader::new(&request.columns, sample_batches(2_501, 2_500, 700));
    let events = run_import(&import, &file, request, reader).await;
    let appended = assert_finished(key, &events);
    let commits = events
        .iter()
        .filter(|e| matches!(e, ImportEvent::Committed { .. }))
        .count();
    assert!(commits >= 3, "{key}: {commits} commits");
    assert_eq!(count(&live, &main).await, 5_000, "{key}");
    assert_eq!(appended.rows_inserted, 2_500, "{key}");

    // A server error (duplicate keys) in one transaction: nothing stays.
    let request = sample_request(&live, &main, TargetMode::Append, TransactionPolicy::Single);
    let mut batches = sample_batches(5_001, 300, 100);
    batches.extend(sample_batches(1, 10, 10));
    let reader = MemoryReader::new(&request.columns, batches);
    let events = run_import(&import, &file, request, reader).await;
    let failed = assert_failed(key, &events);
    assert_eq!(failed.rows_committed, 0, "{key}");
    assert_eq!(count(&live, &main).await, 5_000, "{key}: rolled back");
    assert!(
        failed.left_behind.contains("as it was"),
        "{key}: {}",
        failed.left_behind
    );

    // The same in batches of 100: the committed batches stay.
    let request = sample_request(
        &live,
        &main,
        TargetMode::Append,
        TransactionPolicy::Batches { rows: 100 },
    );
    let mut batches = sample_batches(5_001, 300, 100);
    batches.extend(sample_batches(1, 10, 10));
    let reader = MemoryReader::new(&request.columns, batches);
    let events = run_import(&import, &file, request, reader).await;
    let failed = assert_failed(key, &events);
    assert_eq!(failed.rows_committed, 300, "{key}");
    assert_eq!(
        count(&live, &main).await,
        5_300,
        "{key}: committed batches stay"
    );

    // Truncate with a failure: rolled back, or (TRUNCATE commits) emptied.
    let request = sample_request(
        &live,
        &main,
        TargetMode::Truncate,
        TransactionPolicy::Single,
    );
    let mut batches = sample_batches(1, 10, 10);
    batches.extend(sample_batches(5, 1, 1));
    let reader = MemoryReader::new(&request.columns, batches);
    let events = run_import(&import, &file, request, reader).await;
    let failed = assert_failed(key, &events);
    let left = count(&live, &main).await;
    if ddl_commits(engine) {
        assert_eq!(left, 0, "{key}: TRUNCATE committed: {}", failed.left_behind);
        assert!(
            failed.left_behind.contains("emptied"),
            "{key}: {}",
            failed.left_behind
        );
    } else {
        assert_eq!(left, 5_300, "{key}: rolled back: {}", failed.left_behind);
    }

    // Truncate in batches with a failure: emptied, the committed batches stay.
    let request = sample_request(
        &live,
        &main,
        TargetMode::Truncate,
        TransactionPolicy::Batches { rows: 5 },
    );
    let mut batches = sample_batches(1, 10, 10);
    batches.extend(sample_batches(3, 1, 1));
    let reader = MemoryReader::new(&request.columns, batches);
    let failed = assert_failed(key, &run_import(&import, &file, request, reader).await);
    assert_eq!(failed.rows_committed, 10, "{key}");
    assert_eq!(
        table_rows(&live, &main, &SAMPLE_COLUMNS).await,
        expected_rows(1..=10),
        "{key}: {}",
        failed.left_behind
    );
    assert!(
        failed.left_behind.contains("emptied"),
        "{key}: {}",
        failed.left_behind
    );

    // Truncate, then delete_all, succeeding.
    let request = sample_request(
        &live,
        &main,
        TargetMode::Truncate,
        TransactionPolicy::Single,
    );
    let reader = MemoryReader::new(&request.columns, sample_batches(1, 40, 40));
    assert_finished(key, &run_import(&import, &file, request, reader).await);
    assert_eq!(count(&live, &main).await, 40, "{key}");
    let request = sample_request(
        &live,
        &main,
        TargetMode::DeleteAll,
        TransactionPolicy::Batches { rows: 10 },
    );
    let reader = MemoryReader::new(&request.columns, sample_batches(100, 25, 25));
    assert_finished(key, &run_import(&import, &file, request, reader).await);
    assert_eq!(
        table_rows(&live, &main, &SAMPLE_COLUMNS).await,
        expected_rows(100..=124),
        "{key}"
    );

    // delete_all in one transaction with a failure: the DELETE rolls back with the rows.
    let request = sample_request(
        &live,
        &main,
        TargetMode::DeleteAll,
        TransactionPolicy::Single,
    );
    let mut batches = sample_batches(1, 10, 10);
    batches.extend(sample_batches(5, 1, 1));
    let reader = MemoryReader::new(&request.columns, batches);
    let failed = assert_failed(key, &run_import(&import, &file, request, reader).await);
    assert_eq!(failed.rows_committed, 0, "{key}");
    assert_eq!(
        table_rows(&live, &main, &SAMPLE_COLUMNS).await,
        expected_rows(100..=124),
        "{key}: {}",
        failed.left_behind
    );

    // Replace with fewer columns.
    let mut request = sample_request(&live, &main, TargetMode::Replace, TransactionPolicy::Single);
    request.mapping.truncate(2);
    let reader = MemoryReader::new(&request.columns, sample_batches(1, 30, 30));
    assert_finished(key, &run_import(&import, &file, request, reader).await);
    let rows = table_rows(&live, &main, &["id", "name"]).await;
    assert_eq!(rows.len(), 30, "{key}");
    assert_eq!(rows[0].len(), 2, "{key}");

    // Replace in one transaction with a failure: the old table is intact where DDL is
    // transactional; where it commits, the old table is gone and the new one dropped.
    let request = sample_request(&live, &main, TargetMode::Replace, TransactionPolicy::Single);
    let mut batches = sample_batches(1, 10, 10);
    batches.extend(sample_batches(3, 1, 1));
    let reader = MemoryReader::new(&request.columns, batches);
    let failed = assert_failed(key, &run_import(&import, &file, request, reader).await);
    if ddl_commits(engine) {
        assert!(!live.exists(&main).await, "{key}: {}", failed.left_behind);
        assert!(failed.table_dropped, "{key}: {}", failed.left_behind);
        assert!(
            failed.left_behind.contains("old table is gone"),
            "{key}: {}",
            failed.left_behind
        );
    } else {
        let rows = table_rows(&live, &main, &["id", "name"]).await;
        assert_eq!(rows.len(), 30, "{key}: {}", failed.left_behind);
        assert!(!failed.table_created, "{key}: {}", failed.left_behind);
        assert!(!failed.table_dropped, "{key}: {}", failed.left_behind);
        assert!(
            failed.left_behind.contains("old table is as it was"),
            "{key}: {}",
            failed.left_behind
        );
    }

    // Replace in batches with a failure: the old table is gone, the new one dropped.
    let request = sample_request(
        &live,
        &main,
        TargetMode::Replace,
        TransactionPolicy::Batches { rows: 5 },
    );
    let mut batches = sample_batches(1, 10, 10);
    batches.extend(sample_batches(3, 1, 1));
    let reader = MemoryReader::new(&request.columns, batches);
    let failed = assert_failed(key, &run_import(&import, &file, request, reader).await);
    assert_eq!(failed.rows_committed, 10, "{key}");
    assert!(!live.exists(&main).await, "{key}: {}", failed.left_behind);
    assert!(failed.table_dropped, "{key}: {}", failed.left_behind);

    // A failed create leaves no table, whether the CREATE rolled back or was dropped.
    let request = sample_request(
        &live,
        &fresh,
        TargetMode::Create,
        TransactionPolicy::Batches { rows: 5 },
    );
    let mut batches = sample_batches(1, 10, 10);
    batches.extend(sample_batches(3, 1, 1));
    let reader = MemoryReader::new(&request.columns, batches);
    let events = run_import(&import, &file, request, reader).await;
    let failed = assert_failed(key, &events);
    assert!(!live.exists(&fresh).await, "{key}: {}", failed.left_behind);
    assert!(
        failed.table_dropped,
        "{key}: committed batches mean a committed CREATE"
    );

    // The same in one transaction: the CREATE rolls back, or (DDL commits) is dropped.
    let request = sample_request(&live, &fresh, TargetMode::Create, TransactionPolicy::Single);
    let mut batches = sample_batches(1, 10, 10);
    batches.extend(sample_batches(3, 1, 1));
    let reader = MemoryReader::new(&request.columns, batches);
    let failed = assert_failed(key, &run_import(&import, &file, request, reader).await);
    assert!(!live.exists(&fresh).await, "{key}: {}", failed.left_behind);
    if ddl_commits(engine) {
        assert!(failed.table_created, "{key}: {}", failed.left_behind);
        assert!(failed.table_dropped, "{key}: {}", failed.left_behind);
    } else {
        assert!(!failed.table_created, "{key}: {}", failed.left_behind);
        assert!(!failed.table_dropped, "{key}: {}", failed.left_behind);
        assert!(
            failed.left_behind.contains("no table was created"),
            "{key}: {}",
            failed.left_behind
        );
    }

    text_fidelity(&live, &import, &file, &fid).await;
    exact_decimals(&live, &import, &file, &fid).await;

    // Skip policy: conversion errors are skipped and listed.
    let columns = vec![
        ColumnSpec::new("id", DataType::Text),
        ColumnSpec::new("amount", DataType::Text),
    ];
    let request = ImportRequest {
        options: ReadOptions::Csv(CsvReadOptions::default()),
        columns: columns.clone(),
        mapping: vec![
            ColumnMapping {
                source: 0,
                target: Some("id".into()),
                data_type: Some(DataType::INT64),
            },
            ColumnMapping {
                source: 1,
                target: Some("amount".into()),
                data_type: Some(DataType::Decimal {
                    precision: Some(6),
                    scale: Some(2),
                }),
            },
        ],
        target: ImportTarget {
            session: live.session,
            table: skip.clone(),
            mode: TargetMode::Create,
        },
        primary_key: vec!["id".into()],
        conversion: ConversionOptions {
            trim_text: true,
            ..ConversionOptions::default()
        },
        errors: ErrorPolicy::Skip { max_errors: 5 },
        transaction: TransactionPolicy::Auto,
    };
    let ids = vec![Some("1"), Some("two"), Some(" 3 "), Some("4"), None];
    let amounts = vec![
        Some("1.25"),
        Some("2"),
        Some("99999.99"),
        Some("4.005"),
        Some("5"),
    ];
    let batch = RecordBatch::try_new(
        ColumnSpec::schema(&columns),
        vec![
            Arc::new(arrow_array::StringArray::from(ids)),
            Arc::new(arrow_array::StringArray::from(amounts)),
        ],
    )
    .unwrap();
    let reader = MemoryReader::new(&columns, vec![(batch, Vec::new())]);
    let skipped = assert_finished(key, &run_import(&import, &file, request, reader).await);
    assert_eq!(skipped.rows_skipped, 3, "{key}: {:?}", skipped.errors);
    assert_eq!(
        table_rows(&live, &skip, &["id", "amount"]).await,
        [["1", "1.25"], ["4", "4.01"]],
        "{key}"
    );

    // Plan rejections against the server's table.
    let q = live.qualified(&req);
    let quoting = live.dialects.quoting(engine).unwrap();
    live.sql(&format!(
        "CREATE TABLE {q} ({} INT NOT NULL, {} INT NOT NULL)",
        quoting.quote_ident("id"),
        quoting.quote_ident("needed")
    ))
    .await;
    let mut request = sample_request(&live, &req, TargetMode::Append, TransactionPolicy::Single);
    request.mapping.truncate(1);
    let source = ImportSource {
        path: file.clone(),
        options: request.options.clone(),
    };
    let e = import
        .plan(&source, &request, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("needed"), "{key}: {e}");
    request.mapping[0].target = Some("nope".into());
    let e = import
        .plan(&source, &request, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no column 'nope'"), "{key}: {e}");

    // Cancel: `Cancelled` within 2 s, exactly the committed rows stay.
    let request = sample_request(
        &live,
        &slow,
        TargetMode::Create,
        TransactionPolicy::Batches { rows: 50 },
    );
    let reader = MemoryReader::new(&request.columns, sample_batches(1, 5_000, 50))
        .slow(Duration::from_millis(100));
    let source = ImportSource {
        path: file.clone(),
        options: request.options.clone(),
    };
    let handle = import
        .start_with_reader(&source, request, Box::new(reader))
        .await
        .unwrap();
    let mut events = handle.events;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(60), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!event.is_terminal(), "{key}: {event:?}");
        if matches!(event, ImportEvent::Committed { rows_committed } if rows_committed >= 150) {
            break;
        }
    }
    let asked = Instant::now();
    assert!(import.cancel(handle.id), "{key}");
    let rest = drain(events).await;
    let latency = asked.elapsed();
    assert!(
        latency < Duration::from_secs(2),
        "{key}: cancel took {latency:?}"
    );
    let cancelled = match rest.last() {
        Some(ImportEvent::Cancelled { report }) => report.clone(),
        other => panic!("{key}: not cancelled: {other:?}"),
    };
    assert!(cancelled.rows_committed >= 150, "{key}");
    assert!(
        cancelled.table_dropped,
        "{key}: create mode drops the table"
    );
    assert!(!live.exists(&slow).await, "{key}");
    report(&format!("{key}: import modes ok, cancel {latency:?}"));

    // Append after a cancel keeps the committed rows.
    let mut request = sample_request(
        &live,
        &skip,
        TargetMode::Append,
        TransactionPolicy::Batches { rows: 1 },
    );
    request.mapping.truncate(1);
    request.columns.truncate(1);
    let reader = MemoryReader::new(
        &request.columns,
        (10..40).map(|i| (id_batch(i), Vec::new())).collect(),
    )
    .slow(Duration::from_millis(100));
    let handle = import
        .start_with_reader(&source, request, Box::new(reader))
        .await
        .unwrap();
    let mut events = handle.events;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(60), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!event.is_terminal(), "{key}: {event:?}");
        if matches!(event, ImportEvent::Committed { rows_committed } if rows_committed >= 3) {
            break;
        }
    }
    assert!(import.cancel(handle.id));
    let rest = drain(events).await;
    let cancelled = final_report(&rest).clone();
    assert!(
        matches!(rest.last(), Some(ImportEvent::Cancelled { .. })),
        "{key}: {rest:?}"
    );
    assert_eq!(
        count(&live, &skip).await,
        2 + cancelled.rows_committed,
        "{key}: {}",
        cancelled.left_behind
    );

    if readers_ready() {
        sample_files(&live, &import).await;
    } else {
        report(&format!(
            "{key}: sample files PENDING (oxbase-io readers missing)"
        ));
    }

    for t in [&main, &fresh, &skip, &req, &slow, &fid] {
        live.drop_table(t).await;
    }
}

/// A `decimal(38,10)` column the import creates keeps every digit (SQLite: `DECIMAL_TEXT`,
/// `TEXT` affinity; a `DECIMAL` column would keep about 15 significant digits, without an
/// error). Uses (and drops) `table`.
async fn exact_decimals(live: &Live, import: &ImportService, file: &Path, table: &ObjectRef) {
    let key = live.key;
    live.drop_table(table).await;
    let decimal = DataType::Decimal {
        precision: Some(38),
        scale: Some(10),
    };
    let columns = vec![
        ColumnSpec::new("id", DataType::INT64),
        ColumnSpec::new("amount", decimal.clone()),
    ];
    let request = ImportRequest {
        options: ReadOptions::Csv(CsvReadOptions::default()),
        columns: columns.clone(),
        mapping: vec![
            ColumnMapping {
                source: 0,
                target: Some("id".into()),
                data_type: None,
            },
            ColumnMapping {
                source: 1,
                target: Some("amount".into()),
                data_type: None,
            },
        ],
        target: ImportTarget {
            session: live.session,
            table: table.clone(),
            mode: TargetMode::Create,
        },
        primary_key: vec!["id".into()],
        conversion: ConversionOptions::default(),
        errors: ErrorPolicy::Stop,
        transaction: TransactionPolicy::Single,
    };
    let source = ImportSource {
        path: file.to_path_buf(),
        options: request.options.clone(),
    };
    let plan = import
        .plan(&source, &request, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("{key}: exact decimals: plan: {e}"));
    if live.engine == Engine::Sqlite {
        assert!(
            plan.ddl.iter().any(|d| d.contains("DECIMAL_TEXT(38,10)")),
            "{key}: {:?}",
            plan.ddl
        );
        assert!(
            plan.warnings.iter().any(|w| w.contains("DECIMAL_TEXT")),
            "{key}: {:?}",
            plan.warnings
        );
    }
    let values = [
        Some("1234567890123456789012345678.0123456789"),
        Some("-9999999999999999999999999999.9999999999"),
        Some("0.0000000001"),
        None,
    ];
    let batch = RecordBatch::try_new(
        ColumnSpec::schema(&columns),
        vec![
            Arc::new(arrow_array::Int64Array::from(vec![1, 2, 3, 4])),
            Arc::new(arrow_array::StringArray::from(values.to_vec())),
        ],
    )
    .unwrap();
    let reader = MemoryReader::new(&columns, vec![(batch, Vec::new())]);
    assert_finished(key, &run_import(import, file, request, reader).await);
    assert_eq!(
        table_rows(live, table, &["id", "amount"]).await,
        [
            ["1", "1234567890123456789012345678.0123456789"],
            ["2", "-9999999999999999999999999999.9999999999"],
            ["3", "0.0000000001"],
            ["4", "NULL"],
        ],
        "{key}: decimal(38,10) read back"
    );
    live.drop_table(table).await;
}

/// NULL versus empty text, astral characters, CR / LF / tab, backslashes (`\N` is NULL in
/// PostgreSQL's COPY text), quotes and a long text survive the engine's loader.
async fn text_fidelity(live: &Live, import: &ImportService, file: &Path, table: &ObjectRef) {
    let key = live.key;
    let long = format!("{}é", "x".repeat(5_000));
    let notes: Vec<Option<&str>> = vec![
        Some(""),
        None,
        Some("a\u{1F600}b \u{10348}"),
        Some("line1\r\nline2\ttab\rend"),
        Some(long.as_str()),
        Some("  spaced  "),
        Some("back\\slash \\N \\t 'q' \"d\" ;--"),
    ];
    let columns = vec![
        ColumnSpec::new("id", DataType::INT64),
        ColumnSpec::new("note", DataType::Text),
    ];
    let ids: Vec<i64> = (1..=i64::try_from(notes.len()).unwrap()).collect();
    let batch = RecordBatch::try_new(
        ColumnSpec::schema(&columns),
        vec![
            Arc::new(arrow_array::Int64Array::from(ids)),
            Arc::new(arrow_array::StringArray::from(notes.clone())),
        ],
    )
    .unwrap();
    let request = ImportRequest {
        options: ReadOptions::Csv(CsvReadOptions::default()),
        columns: columns.clone(),
        mapping: (0..columns.len())
            .map(|source| ColumnMapping {
                source,
                target: Some(columns[source].name.clone()),
                data_type: None,
            })
            .collect(),
        target: ImportTarget {
            session: live.session,
            table: table.clone(),
            mode: TargetMode::Create,
        },
        primary_key: vec!["id".into()],
        conversion: ConversionOptions::default(),
        errors: ErrorPolicy::Stop,
        transaction: TransactionPolicy::Single,
    };
    let reader = MemoryReader::new(&columns, vec![(batch, Vec::new())]);
    let done = assert_finished(key, &run_import(import, file, request, reader).await);
    assert_eq!(done.rows_committed, 7, "{key}");
    let expected: Vec<Vec<String>> = notes
        .iter()
        .enumerate()
        .map(|(index, note)| {
            vec![
                (index + 1).to_string(),
                note.map_or_else(|| "NULL".to_owned(), norm_text),
            ]
        })
        .collect();
    assert_eq!(
        table_rows(live, table, &["id", "note"]).await,
        expected,
        "{key}: text fidelity"
    );
    report(&format!("{key}: text fidelity ok"));
}

/// One `id` (int) row.
fn id_batch(id: i64) -> RecordBatch {
    let specs = [ColumnSpec::new("id", DataType::INT64)];
    RecordBatch::try_new(
        ColumnSpec::schema(&specs),
        vec![Arc::new(arrow_array::Int64Array::from(vec![id]))],
    )
    .unwrap()
}

/// Every sample format into a new table, then appended to it again (needs the readers).
async fn sample_files(live: &Live, import: &ImportService) {
    let key = live.key;
    let samples = [
        ("basic.csv", ReadOptions::Csv(CsvReadOptions::default()), 5),
        ("basic.tsv", ReadOptions::Csv(CsvReadOptions::tsv()), 2),
        (
            "array.json",
            ReadOptions::Json(JsonReadOptions::default()),
            3,
        ),
        (
            "lines.ndjson",
            ReadOptions::Json(JsonReadOptions::default()),
            0,
        ),
    ];
    for (index, (file, options, rows)) in samples.into_iter().enumerate() {
        let table = live.table(&format!("m5_t_file{index}_{}", live.token));
        live.drop_table(&table).await;
        let source = ImportSource {
            path: sample_file(file),
            options: options.clone(),
        };
        let preview = import
            .preview(&source, CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{key}: {file}: preview: {e}"));
        let columns = ColumnSpec::from_preview(&preview);
        let mapping = columns
            .iter()
            .enumerate()
            .map(|(source, c)| ColumnMapping {
                source,
                target: Some(c.name.clone()),
                data_type: None,
            })
            .collect();
        let mut request = ImportRequest {
            options,
            columns,
            mapping,
            target: ImportTarget {
                session: live.session,
                table: table.clone(),
                mode: TargetMode::Create,
            },
            primary_key: Vec::new(),
            conversion: ConversionOptions::default(),
            errors: ErrorPolicy::Skip { max_errors: 10 },
            transaction: TransactionPolicy::Auto,
        };
        let handle = import.start(source.clone(), request.clone()).await.unwrap();
        let first = drain(handle.events).await;
        let created = assert_finished(key, &first);
        if rows > 0 {
            assert_eq!(created.rows_committed, rows, "{key}: {file}");
        }
        request.target.mode = TargetMode::Append;
        let handle = import.start(source, request).await.unwrap();
        let again = assert_finished(key, &drain(handle.events).await);
        assert_eq!(
            count(live, &table).await,
            created.rows_committed + again.rows_committed,
            "{key}: {file}"
        );
        live.drop_table(&table).await;
    }
    report(&format!("{key}: sample files ok"));
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
    postgres_import_modes => import_cycle("postgres", Engine::Postgres);
    mysql_import_modes => import_cycle("mysql", Engine::MySql);
    mariadb_import_modes => import_cycle("mariadb", Engine::MariaDb);
    sqlite_import_modes => import_cycle("sqlite", Engine::Sqlite);
    mssql_import_modes => import_cycle("mssql", Engine::MsSql);
}

#[test]
fn values_normalize() {
    assert_eq!(norm_text("12.50"), "12.5");
    assert_eq!(norm_text("-0.00"), "0");
    assert_eq!(norm_text("100"), "100");
    assert_eq!(
        norm_text("2000-01-01T00:00:07.5"),
        "2000-01-01 00:00:07.500000"
    );
    assert_eq!(norm_text("1970-02-07"), "1970-02-07");
    assert_eq!(norm_text("name 1"), "name 1");
    assert_eq!(norm(&Value::Bool(true)), "1");
    assert_eq!(norm(&Value::Float(4.5)), "4.5");
    let expected = expected_rows(3..=3);
    assert_eq!(expected[0][0], "3");
    assert_eq!(expected[0][3], "1");
}
