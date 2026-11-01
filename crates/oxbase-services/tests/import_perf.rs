//! M5 exit check (docs/m5-contracts.md §9, PLAN.md §6): a 1 GB CSV import with progress.
//!
//! The ignored test [`import_1gb_csv`] generates the deterministic CSV of `perf::generator`
//! (`[m5.perf] rows`, about 1.24 GB) under `$OXBASE_PERF_DIR`, then for every engine of
//! `[m5.perf] engines` (PostgreSQL and MySQL; `OXBASE_PERF_ENGINES=postgres,mysql,sqlite`
//! overrides the list):
//!
//! 1. a small probe import (create mode, one transaction) whose checksum must match, so a
//!    wrong value conversion fails in seconds rather than after the big run;
//! 2. the full import into `m5_perf` (create mode, `Batches { commit_rows }`, `Stop` on the
//!    first row issue) must finish with every row read and committed, the checksum query of
//!    `[m5.perf.checksum]` must return the generator's numbers, the progress events must be
//!    monotonic and frequent (at most `progress_max_gap_ms` or `progress_max_gap_fraction` of
//!    the file apart, the last one reporting every byte) and the resident memory may grow at
//!    most `rss_growth_limit_mb`;
//! 3. a second import (append into a pre-created `m5_perf_cancel`) is cancelled once a quarter
//!    of the file is read: `Cancelled` must arrive within `cancel_deadline_ms` and the table
//!    must hold exactly `report.rowsCommitted` (> 0) rows, the documented leftover of an
//!    append in batches (§5.2).
//!
//! It prints rows/s, MB/s (10^6 bytes), the progress cadence, the RSS growth and the cancel
//! latency per engine, and appends the same table to `$GITHUB_STEP_SUMMARY` when set.
//!
//! **Pending.** Until the read, services and load streams are merged, `oxbase_io`'s CSV reader
//! or `ImportService::start` are `unimplemented!("m5: ...")`. The test detects that (a probe
//! before any big work), prints `PENDING` with the reason, still generates the full file and
//! checks its size, and passes. With `OXBASE_REQUIRE_M5_PERF=1` (set by the integrator once
//! the streams are merged) a pending part fails the test.
//!
//! Without `OXBASE_PERF_DIR` the test does nothing (the conformance job runs every ignored
//! test of this crate; the 1 GB run belongs to the `import-perf` job only), unless
//! `OXBASE_REQUIRE_M5_PERF=1`.
//!
//! ```sh
//! scripts/fixtures-certs.sh && scripts/ci-fixtures.sh up postgres mysql
//! OXBASE_PERF_DIR=/tmp/m5-perf cargo test -p oxbase-services --profile perf \
//!     --test import_perf -- --include-ignored --test-threads 1 --nocapture
//! scripts/ci-fixtures.sh down
//! ```
//!
//! `OXBASE_PERF_ROWS=<n>` makes a smaller file for a quick local run (the size bounds are then
//! not checked); `OXBASE_PERF_KEEP_CSV=1` keeps the generated files.

#![allow(
    clippy::cast_precision_loss,
    reason = "throughput figures and fractions for the log"
)]

mod perf;

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once, PoisonError};
use std::time::{Duration, Instant};

use oxbase_dialect::BulkMethod;
use oxbase_io::{ColumnSpec, CsvReadOptions, PreviewLimits, ReadOptions};
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_services::import::{
    ColumnMapping, ConversionOptions, ErrorPolicy, ImportConfig, ImportEvent, ImportHandle,
    ImportReport, ImportRequest, ImportService, ImportSource, ImportTarget, TargetMode,
    TransactionPolicy,
};
use oxbase_services::tree::TreeService;
use oxbase_services::{
    CancellationToken, ConnectionManager, ConnectionProfile, DialectRegistry, ProvidedSecrets,
    SecretRef, SecretStore, SessionHandle, SessionId, default_dialects, default_drivers,
};
use oxbase_store::{MemorySecretStore, ProfileAddress, Secret, SecretSlot, TlsMode, TlsSettings};
use oxbase_types::{DataType, Engine, ObjectRef, Value};

use perf::generator::{self, COLUMNS, Checksum, Generated};
use perf::rss;

/// The contract's upper bound of the generated file (docs/m5-contracts.md §9.1).
const MAX_BYTES: u64 = 1_300_000_000;
/// Rows of the probe import.
const PROBE_ROWS: u64 = 20_000;
/// The longest wait for the next import event before the run counts as stalled.
const STALL: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------------------------
// Fixtures and environment

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

fn integer(t: &toml::Table, key: &str) -> u64 {
    t.get(key)
        .and_then(toml::Value::as_integer)
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or_else(|| panic!("fixtures.toml: missing integer {key}"))
}

fn float(t: &toml::Table, key: &str) -> f64 {
    t.get(key)
        .and_then(toml::Value::as_float)
        .unwrap_or_else(|| panic!("fixtures.toml: missing float {key}"))
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// `[m5.perf]` of tests/fixtures.toml.
#[derive(Debug, Clone)]
struct PerfConfig {
    rows: u64,
    rows_overridden: bool,
    min_bytes: u64,
    engines: Vec<String>,
    table: String,
    cancel_table: String,
    commit_rows: u64,
    progress_max_gap_ms: u64,
    progress_max_gap_fraction: f64,
    rss_growth_limit_mb: u64,
    cancel_after_fraction: f64,
    cancel_deadline_ms: u64,
    checksum: toml::Table,
}

impl PerfConfig {
    fn load(fx: &toml::Table) -> Self {
        let perf = table(table(fx, "m5"), "perf");
        let configured_rows = integer(perf, "rows");
        let rows = env_nonempty("OXBASE_PERF_ROWS").map(|v| {
            v.trim()
                .replace('_', "")
                .parse::<u64>()
                .expect("OXBASE_PERF_ROWS is a row count")
        });
        let engines = env_nonempty("OXBASE_PERF_ENGINES").map_or_else(
            || {
                perf.get("engines")
                    .and_then(toml::Value::as_array)
                    .expect("fixtures.toml: [m5.perf] engines")
                    .iter()
                    .map(|v| v.as_str().expect("engine keys are strings").to_owned())
                    .collect()
            },
            |v| {
                v.split(',')
                    .map(|k| k.trim().to_owned())
                    .filter(|k| !k.is_empty())
                    .collect()
            },
        );
        Self {
            rows: rows.unwrap_or(configured_rows),
            rows_overridden: rows.is_some_and(|r| r != configured_rows),
            min_bytes: integer(perf, "min_bytes"),
            engines,
            table: string(perf, "table"),
            cancel_table: string(perf, "cancel_table"),
            commit_rows: integer(perf, "commit_rows"),
            progress_max_gap_ms: integer(perf, "progress_max_gap_ms"),
            progress_max_gap_fraction: float(perf, "progress_max_gap_fraction"),
            rss_growth_limit_mb: integer(perf, "rss_growth_limit_mb"),
            cancel_after_fraction: float(perf, "cancel_after_fraction"),
            cancel_deadline_ms: integer(perf, "cancel_deadline_ms"),
            checksum: table(perf, "checksum").clone(),
        }
    }
}

fn engine_of(key: &str) -> Engine {
    match key {
        "postgres" => Engine::Postgres,
        "mysql" => Engine::MySql,
        "mariadb" => Engine::MariaDb,
        "mssql" => Engine::MsSql,
        "sqlite" => Engine::Sqlite,
        other => panic!("[m5.perf] engines: unknown engine key {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Pending detection: a panic of an `unimplemented!("m5: <stream>")` skeleton body.

static PANICS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Records every panic message (then runs the default hook), so a skeleton panic inside a
/// task or a blocking thread of the service can be told apart from a real failure.
fn install_panic_log() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let payload = info.payload();
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "(non-text panic payload)".to_owned());
            PANICS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(message);
            default(info);
        }));
    });
}

fn clear_panics() {
    PANICS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

/// The latest recorded skeleton panic (`not implemented: m5: <stream>`), if any.
fn skeleton_panic() -> Option<String> {
    PANICS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .rev()
        .find(|m| is_skeleton(m))
        .cloned()
}

fn is_skeleton(message: &str) -> bool {
    message.contains("not implemented") && message.contains("m5:")
}

/// A part of M5 the check needs is not implemented yet.
#[derive(Debug, Clone)]
struct Pending(String);

// ---------------------------------------------------------------------------------------------
// The import request of the check

/// Column types of the generated file (docs/m5-contracts.md §9.1, "Target type").
fn perf_columns() -> Vec<(&'static str, DataType)> {
    let types = [
        DataType::INT64,
        DataType::VarChar { length: Some(16) },
        DataType::INT32,
        DataType::Decimal {
            precision: Some(12),
            scale: Some(2),
        },
        DataType::FLOAT64,
        DataType::Bool,
        DataType::Date,
        DataType::Timestamp { precision: Some(6) },
        DataType::Text,
    ];
    COLUMNS.iter().copied().zip(types).collect()
}

fn csv_options() -> ReadOptions {
    ReadOptions::Csv(CsvReadOptions::default())
}

fn column_specs() -> Vec<ColumnSpec> {
    perf_columns()
        .into_iter()
        .map(|(name, data_type)| ColumnSpec::new(name, data_type))
        .collect()
}

fn request(
    session: SessionId,
    table: ObjectRef,
    mode: TargetMode,
    transaction: TransactionPolicy,
) -> ImportRequest {
    ImportRequest {
        options: csv_options(),
        columns: column_specs(),
        mapping: perf_columns()
            .into_iter()
            .enumerate()
            .map(|(source, (name, data_type))| ColumnMapping {
                source,
                target: Some(name.to_owned()),
                data_type: Some(data_type),
            })
            .collect(),
        target: ImportTarget {
            session,
            table,
            mode,
        },
        primary_key: vec!["id".to_owned()],
        conversion: ConversionOptions::default(),
        errors: ErrorPolicy::Stop,
        transaction,
    }
}

// ---------------------------------------------------------------------------------------------
// Reader probe (oxbase-io, no database)

/// Previews and reads the probe file with `oxbase_io` directly. `Err(Pending)` while the CSV
/// reader is a skeleton; a reader that exists must read every row without issues.
fn probe_reader(path: &Path, rows: u64, bytes: u64) -> Result<String, Pending> {
    clear_panics();
    let outcome = std::panic::catch_unwind(|| {
        let options = csv_options();
        let preview = oxbase_io::preview(
            path,
            &options,
            PreviewLimits::default(),
            &CancellationToken::new(),
        )
        .expect("oxbase_io::preview of the probe file");
        let names: Vec<&str> = preview.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, COLUMNS, "preview column names");
        let inferred: Vec<String> = preview
            .columns
            .iter()
            .map(|c| format!("{}: {:?}", c.name, c.data_type))
            .collect();
        let mut reader = oxbase_io::open(path, &options, &column_specs(), 8_192)
            .expect("oxbase_io::open of the probe file");
        let (mut read, mut issues, mut last_bytes) = (0u64, 0usize, 0u64);
        while let Some(batch) = reader.next_batch().expect("reading the probe file") {
            assert!(
                batch.bytes_read >= last_bytes,
                "bytes_read went down: {last_bytes} -> {}",
                batch.bytes_read
            );
            last_bytes = batch.bytes_read;
            read += u64::try_from(batch.batch.num_rows()).expect("row count");
            issues += batch.issues.len();
            if let Some(issue) = batch.issues.first() {
                eprintln!("probe file: first row issue: {issue:?}");
            }
        }
        assert_eq!(read, rows, "rows read from the probe file");
        assert_eq!(issues, 0, "row issues in the probe file");
        assert_eq!(
            reader.bytes_read(),
            bytes,
            "bytes_read at the end of the file"
        );
        format!("inferred types: {}", inferred.join(", "))
    });
    match outcome {
        Ok(summary) => Ok(summary),
        Err(payload) => match skeleton_panic() {
            Some(message) => Err(Pending(format!("oxbase_io CSV reader: {message}"))),
            None => std::panic::resume_unwind(payload),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// One engine's session and service

struct Live {
    key: String,
    engine: Engine,
    manager: Arc<ConnectionManager>,
    dialects: Arc<DialectRegistry>,
    service: Arc<ImportService>,
    session: SessionHandle,
    fx: toml::Table,
}

impl Live {
    async fn connect(fx: &toml::Table, key: &str, perf_dir: &Path) -> Self {
        let engine = engine_of(key);
        let secrets = Arc::new(MemorySecretStore::new());
        let profile = if engine == Engine::Sqlite {
            let path = perf_dir.join("m5_perf.sqlite");
            let _ = std::fs::remove_file(&path);
            ConnectionProfile::new(format!("perf {key}"), engine, ProfileAddress::File { path })
        } else {
            let e = table(table(fx, "engines"), key);
            let mut p = ConnectionProfile::new(
                format!("perf {key}"),
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
            p.connect_timeout_secs = Some(30);
            p
        };
        let manager = Arc::new(ConnectionManager::new(
            default_drivers(),
            secrets,
            Arc::new(RusshTunnelProvider::new()),
        ));
        let dialects = Arc::new(default_dialects());
        let tree = Arc::new(TreeService::new(manager.clone(), dialects.clone()));
        let service = Arc::new(ImportService::new(
            manager.clone(),
            tree,
            dialects.clone(),
            ImportConfig::default(),
        ));
        let session = manager
            .open(&profile, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{key}: connect failed: {e}"));
        Self {
            key: key.to_owned(),
            engine,
            manager,
            dialects,
            service,
            session,
            fx: fx.clone(),
        }
    }

    /// A scratch table `name` where `[m5.<key>]` says (database and schema).
    fn table_ref(&self, name: &str) -> ObjectRef {
        let m5 = table(table(&self.fx, "m5"), &self.key);
        let database = m5.get("database").and_then(toml::Value::as_str);
        let schema = m5.get("schema").and_then(toml::Value::as_str);
        ObjectRef::table(database, schema, name)
    }

    fn qualified(&self, obj: &ObjectRef) -> String {
        self.dialects
            .quoting(self.engine)
            .unwrap_or_else(|e| panic!("{}: no Quoting: {e}", self.key))
            .qualify(obj)
    }

    /// Runs `sql` on the base session; the first row of the first result set.
    async fn query_row(&self, sql: &str) -> Vec<Value> {
        let _exclusive = self.session.exclusive().await;
        let stream = self
            .session
            .connection()
            .execute(sql, &[], CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{}: {sql}: {e}", self.key));
        let out = oxbase_driver::collect(stream)
            .await
            .unwrap_or_else(|e| panic!("{}: {sql}: {e}", self.key));
        let set = out
            .result_sets
            .first()
            .unwrap_or_else(|| panic!("{}: {sql}: no result set", self.key));
        let batch = set
            .batches
            .iter()
            .find(|b| b.num_rows() > 0)
            .unwrap_or_else(|| panic!("{}: {sql}: no row", self.key));
        (0..batch.num_columns())
            .map(|c| {
                oxbase_driver::cell_value(batch.column(c).as_ref(), 0)
                    .unwrap_or_else(|e| panic!("{}: {sql}: column {c}: {e}", self.key))
            })
            .collect()
    }

    async fn execute(&self, sql: &str) {
        let _exclusive = self.session.exclusive().await;
        let stream = self
            .session
            .connection()
            .execute(sql, &[], CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{}: {sql}: {e}", self.key));
        oxbase_driver::collect(stream)
            .await
            .unwrap_or_else(|e| panic!("{}: {sql}: {e}", self.key));
    }

    async fn drop_table(&self, obj: &ObjectRef) {
        self.execute(&format!("DROP TABLE IF EXISTS {}", self.qualified(obj)))
            .await;
    }

    async fn count(&self, obj: &ObjectRef) -> i128 {
        let row = self
            .query_row(&format!("SELECT COUNT(*) FROM {}", self.qualified(obj)))
            .await;
        as_i128(&row[0])
    }

    /// The `[m5.perf.checksum]` query of this engine on `obj`.
    async fn checksum(&self, cfg: &PerfConfig, obj: &ObjectRef) -> [i128; 6] {
        let template = cfg
            .checksum
            .get(&self.key)
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| panic!("[m5.perf.checksum] has no query for {}", self.key));
        let sql = template.replace("{table}", &self.qualified(obj));
        let row = self.query_row(&sql).await;
        assert_eq!(
            row.len(),
            6,
            "{}: checksum query returns six columns",
            self.key
        );
        let mut out = [0i128; 6];
        for (slot, value) in out.iter_mut().zip(&row) {
            *slot = as_i128(value);
        }
        out
    }

    /// Runs this engine's checksum query over a two-row derived table (no DDL), so the query
    /// text, the result types and the note-length rule are checked before any import: ids 1
    /// and 2, qty -3 and NULL, amounts 1.25 and -0.75, notes `a🚀` (2 code points, 3 UTF-16
    /// units) and NULL.
    async fn checksum_self_test(&self, cfg: &PerfConfig) {
        let template = cfg
            .checksum
            .get(&self.key)
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| panic!("[m5.perf.checksum] has no query for {}", self.key));
        let note = self
            .dialects
            .quoting(self.engine)
            .unwrap_or_else(|e| panic!("{}: no Quoting: {e}", self.key))
            .literal(&Value::Text("a\u{1F680}".to_owned()));
        let derived = format!(
            "(SELECT 1 AS id, -3 AS qty, 1.25 AS amount, {note} AS note \
             UNION ALL SELECT 2, NULL, -0.75, NULL) t"
        );
        let row = self.query_row(&template.replace("{table}", &derived)).await;
        let got: Vec<i128> = row.iter().map(as_i128).collect();
        let lengths: i128 = if self.key == "mssql" { 3 } else { 2 };
        let want: [i128; 6] = [2, 3, -3, 1, lengths, 50];
        assert_eq!(
            got, want,
            "{}: checksum query over a derived table ({row:?})",
            self.key
        );
    }

    async fn close(self) {
        let _ = self.manager.close(self.session.id()).await;
    }
}

/// An integer cell (`bigint`, `numeric` / `DECIMAL` sums as exact text, NULL for an empty sum).
fn as_i128(value: &Value) -> i128 {
    match value {
        Value::Null => 0,
        Value::Int(v) => i128::from(*v),
        Value::UInt(v) => i128::from(*v),
        Value::Decimal(text) | Value::Text(text) => {
            let text = text.trim();
            let whole = match text.split_once('.') {
                Some((whole, fraction)) => {
                    assert!(fraction.chars().all(|c| c == '0'), "not an integer: {text}");
                    whole
                }
                None => text,
            };
            whole
                .parse()
                .unwrap_or_else(|e| panic!("not an integer: {text}: {e}"))
        }
        other => panic!("checksum cell is not an integer: {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Running one import and recording its events

#[derive(Debug, Clone, Copy)]
struct Snapshot {
    at: Instant,
    bytes_read: u64,
    total_bytes: u64,
    rows_read: u64,
    rows_inserted: u64,
    rows_committed: u64,
    rows_skipped: u64,
}

struct RunOutcome {
    terminal: ImportEvent,
    started_total: Option<u64>,
    method: Option<BulkMethod>,
    progress: Vec<Snapshot>,
    commits: Vec<u64>,
    row_errors: Vec<String>,
    /// From `start` to the terminal event.
    elapsed: Duration,
    /// When `cancel` was called and at which fraction of the file.
    cancelled_at: Option<(Instant, f64)>,
    /// From `cancel` to the terminal event.
    cancel_latency: Option<Duration>,
    terminal_at: Instant,
}

impl RunOutcome {
    fn report(&self) -> &ImportReport {
        match &self.terminal {
            ImportEvent::Finished { report }
            | ImportEvent::Failed { report, .. }
            | ImportEvent::Cancelled { report } => report,
            other => unreachable!("not terminal: {other:?}"),
        }
    }

    fn describe(&self) -> String {
        match &self.terminal {
            ImportEvent::Finished { .. } => "Finished".to_owned(),
            ImportEvent::Failed { error, report } => format!(
                "Failed ({:?}: {}); left behind: {}",
                error.category, error.message, report.left_behind
            ),
            ImportEvent::Cancelled { report } => {
                format!("Cancelled; left behind: {}", report.left_behind)
            }
            other => format!("{other:?}"),
        }
    }
}

/// Starts the import and follows its events to the end. `cancel_after`: call
/// `ImportService::cancel` once `bytesRead` reaches that fraction of the file.
async fn run_import(
    live: &Live,
    what: &str,
    source: ImportSource,
    request: ImportRequest,
    cancel_after: Option<f64>,
) -> Result<RunOutcome, Pending> {
    clear_panics();
    let begun = Instant::now();
    let service = Arc::clone(&live.service);
    let started = tokio::spawn(async move { service.start(source, request).await }).await;
    let ImportHandle { id, mut events } = match started {
        Ok(Ok(handle)) => handle,
        Ok(Err(e)) => panic!("{what}: ImportService::start failed: {e}"),
        Err(join) if join.is_panic() => match skeleton_panic() {
            Some(message) => return Err(Pending(format!("ImportService::start: {message}"))),
            None => std::panic::resume_unwind(join.into_panic()),
        },
        Err(join) => panic!("{what}: the start task ended: {join}"),
    };
    let mut started_total = None;
    let mut method = None;
    let mut progress: Vec<Snapshot> = Vec::new();
    let mut commits = Vec::new();
    let mut row_errors = Vec::new();
    let mut cancelled_at: Option<(Instant, f64)> = None;
    loop {
        let event = match tokio::time::timeout(STALL, events.recv()).await {
            Err(elapsed) => panic!(
                "{what}: no import event for {} s ({elapsed}; last progress: {:?})",
                STALL.as_secs(),
                progress.last()
            ),
            Ok(None) => {
                if let Some(message) = skeleton_panic() {
                    return Err(Pending(format!("import run: {message}")));
                }
                panic!("{what}: the event stream ended without Finished / Failed / Cancelled");
            }
            Ok(Some(event)) => event,
        };
        let now = Instant::now();
        match &event {
            ImportEvent::Started {
                total_bytes, plan, ..
            } => {
                started_total = Some(*total_bytes);
                method = Some(plan.method);
            }
            ImportEvent::Progress {
                bytes_read,
                total_bytes,
                rows_read,
                rows_inserted,
                rows_committed,
                rows_skipped,
                ..
            } => {
                progress.push(Snapshot {
                    at: now,
                    bytes_read: *bytes_read,
                    total_bytes: *total_bytes,
                    rows_read: *rows_read,
                    rows_inserted: *rows_inserted,
                    rows_committed: *rows_committed,
                    rows_skipped: *rows_skipped,
                });
                if let Some(fraction) = cancel_after
                    && cancelled_at.is_none()
                    && *total_bytes > 0
                {
                    let reached = *bytes_read as f64 / *total_bytes as f64;
                    if reached >= fraction {
                        assert!(
                            live.service.cancel(id),
                            "{what}: ImportService::cancel did not find the running import"
                        );
                        cancelled_at = Some((Instant::now(), reached));
                    }
                }
            }
            ImportEvent::RowError { error } => row_errors.push(format!(
                "row {} {}: {}",
                error.row,
                error.column.as_deref().unwrap_or(""),
                error.message
            )),
            ImportEvent::Committed { rows_committed } => commits.push(*rows_committed),
            ImportEvent::Finished { .. }
            | ImportEvent::Failed { .. }
            | ImportEvent::Cancelled { .. } => {
                if let ImportEvent::Failed { error, .. } = &event
                    && let Some(message) = skeleton_panic()
                {
                    return Err(Pending(format!(
                        "import run failed on a skeleton ({}): {message}",
                        error.message
                    )));
                }
                let cancel_latency = cancelled_at.map(|(at, _)| now.duration_since(at));
                assert!(
                    !live.service.is_running(id),
                    "{what}: the import is still registered as running after its terminal event"
                );
                return Ok(RunOutcome {
                    terminal: event,
                    started_total,
                    method,
                    progress,
                    commits,
                    row_errors,
                    elapsed: now.duration_since(begun),
                    cancelled_at,
                    cancel_latency,
                    terminal_at: now,
                });
            }
        }
    }
}

/// Asserts a finished import of every row of a file.
fn assert_finished(what: &str, outcome: &RunOutcome, rows: u64, file_bytes: u64) {
    assert!(
        matches!(outcome.terminal, ImportEvent::Finished { .. }),
        "{what}: expected Finished, got {}",
        outcome.describe()
    );
    let report = outcome.report();
    assert_eq!(
        outcome.row_errors,
        Vec::<String>::new(),
        "{what}: row errors"
    );
    assert_eq!(report.errors, Vec::new(), "{what}: report errors");
    assert_eq!(report.rows_read, rows, "{what}: rowsRead");
    assert_eq!(report.rows_inserted, rows, "{what}: rowsInserted");
    assert_eq!(report.rows_committed, rows, "{what}: rowsCommitted");
    assert_eq!(report.rows_skipped, 0, "{what}: rowsSkipped");
    assert_eq!(report.bytes_read, file_bytes, "{what}: report bytesRead");
    assert_eq!(
        outcome.started_total,
        Some(file_bytes),
        "{what}: Started totalBytes"
    );
}

/// Progress cadence of one run.
#[derive(Debug, Clone, Copy, Default)]
struct Cadence {
    events: usize,
    /// Longest time between two consecutive progress events.
    max_gap: Duration,
    /// Largest share of the file between two consecutive progress events.
    max_gap_fraction: f64,
    /// From Started's arrival (approximated by the run start) to the first progress event.
    first_after: Duration,
    commits: usize,
}

/// Checks the progress rules of docs/m5-contracts.md §9.2 (3).
fn check_progress(
    what: &str,
    cfg: &PerfConfig,
    outcome: &RunOutcome,
    file_bytes: u64,
    begun: Instant,
) -> Cadence {
    let progress = &outcome.progress;
    assert!(
        progress.len() >= 10,
        "{what}: only {} progress events for {file_bytes} bytes",
        progress.len()
    );
    let max_gap_ms = Duration::from_millis(cfg.progress_max_gap_ms);
    let mut cadence = Cadence {
        events: progress.len(),
        first_after: progress[0].at.duration_since(begun),
        commits: outcome.commits.len(),
        ..Cadence::default()
    };
    let mut violations = Vec::new();
    for pair in progress.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        for (name, before, after) in [
            ("bytesRead", a.bytes_read, b.bytes_read),
            ("rowsRead", a.rows_read, b.rows_read),
            ("rowsInserted", a.rows_inserted, b.rows_inserted),
            ("rowsCommitted", a.rows_committed, b.rows_committed),
            ("rowsSkipped", a.rows_skipped, b.rows_skipped),
        ] {
            assert!(
                after >= before,
                "{what}: progress {name} went down: {before} -> {after}"
            );
        }
        assert_eq!(a.total_bytes, b.total_bytes, "{what}: totalBytes changed");
        let gap = b.at.duration_since(a.at);
        let fraction = (b.bytes_read - a.bytes_read) as f64 / file_bytes as f64;
        cadence.max_gap = cadence.max_gap.max(gap);
        cadence.max_gap_fraction = cadence.max_gap_fraction.max(fraction);
        if gap > max_gap_ms && fraction > cfg.progress_max_gap_fraction {
            violations.push(format!(
                "{} ms and {:.2}% of the file between progress at {} and {} bytes",
                gap.as_millis(),
                fraction * 100.0,
                a.bytes_read,
                b.bytes_read
            ));
        }
    }
    for pair in outcome.commits.windows(2) {
        assert!(
            pair[1] >= pair[0],
            "{what}: committed rowsCommitted went down: {} -> {}",
            pair[0],
            pair[1]
        );
    }
    assert!(
        violations.is_empty(),
        "{what}: progress gaps over {} ms and {}% of the file: {}",
        cfg.progress_max_gap_ms,
        cfg.progress_max_gap_fraction * 100.0,
        violations.join("; ")
    );
    let last = progress.last().expect("checked above");
    assert_eq!(
        last.bytes_read, file_bytes,
        "{what}: the last progress event before Finished reports {} of {file_bytes} bytes",
        last.bytes_read
    );
    assert_eq!(last.total_bytes, file_bytes, "{what}: progress totalBytes");
    cadence
}

// ---------------------------------------------------------------------------------------------
// Per-engine run

#[derive(Debug, Clone, Default)]
struct EngineResult {
    key: String,
    server: String,
    pending: Option<String>,
    native_loader: bool,
    method: Option<BulkMethod>,
    rows: u64,
    bytes: u64,
    import_secs: f64,
    cadence: Cadence,
    rss_growth: Option<u64>,
    cancel_fraction: f64,
    cancel_latency_ms: u128,
    cancel_rows_committed: u64,
}

impl EngineResult {
    fn rows_per_sec(&self) -> f64 {
        self.rows as f64 / self.import_secs.max(f64::EPSILON)
    }

    fn mb_per_sec(&self) -> f64 {
        self.bytes as f64 / 1e6 / self.import_secs.max(f64::EPSILON)
    }
}

struct Files {
    full: PathBuf,
    full_gen: Generated,
    probe: PathBuf,
    probe_gen: Generated,
    header: PathBuf,
    header_gen: Generated,
}

fn source(path: &Path) -> ImportSource {
    ImportSource {
        path: path.to_owned(),
        options: csv_options(),
    }
}

async fn run_engine(
    cfg: &PerfConfig,
    fx: &toml::Table,
    key: &str,
    dir: &Path,
    files: &Files,
    reader_pending: Option<&Pending>,
) -> EngineResult {
    let live = Live::connect(fx, key, dir).await;
    let mut result = EngineResult {
        key: key.to_owned(),
        server: live.session.info().server_version.to_string(),
        native_loader: live.dialects.has_bulk_loader(live.engine),
        ..EngineResult::default()
    };
    println!(
        "M5 perf [{key}]: connected ({}), bulk loader: {}",
        result.server,
        if result.native_loader {
            "registered (native)"
        } else {
            "not registered (InsertLoader fallback)"
        }
    );
    live.checksum_self_test(cfg).await;
    println!("M5 perf [{key}]: checksum query self-test ok");
    if let Some(Pending(reason)) = reader_pending {
        result.pending = Some(reason.clone());
        live.close().await;
        return result;
    }
    match Box::pin(import_checks(cfg, &live, files, &mut result)).await {
        Ok(()) => {}
        Err(Pending(reason)) => result.pending = Some(reason),
    }
    live.close().await;
    result
}

async fn import_checks(
    cfg: &PerfConfig,
    live: &Live,
    files: &Files,
    result: &mut EngineResult,
) -> Result<(), Pending> {
    let key = live.key.clone();
    let session = live.session.id();
    let probe_table = live.table_ref(&format!("{}_probe", cfg.table));
    let main_table = live.table_ref(&cfg.table);
    let cancel_table = live.table_ref(&cfg.cancel_table);
    for t in [&probe_table, &main_table, &cancel_table] {
        live.drop_table(t).await;
    }

    // 1. Probe: a small file, one transaction, checksum.
    let what = format!("{key} probe import ({PROBE_ROWS} rows)");
    let outcome = run_import(
        live,
        &what,
        source(&files.probe),
        request(
            session,
            probe_table.clone(),
            TargetMode::Create,
            TransactionPolicy::Single,
        ),
        None,
    )
    .await?;
    assert_finished(&what, &outcome, PROBE_ROWS, files.probe_gen.bytes);
    assert_eq!(
        live.checksum(cfg, &probe_table).await,
        files.probe_gen.checksum.expected(&key),
        "{what}: checksum (count, sum id, sum qty, notes, note length, amount cents)"
    );
    live.drop_table(&probe_table).await;
    println!(
        "M5 perf [{key}]: probe import ok ({PROBE_ROWS} rows in {} ms, method {:?})",
        outcome.elapsed.as_millis(),
        outcome.method
    );

    // 2. The full import.
    let what = format!("{key} 1 GB import");
    let baseline = rss::vm_rss();
    let sampler = rss::Sampler::start(Duration::from_millis(20));
    let begun = Instant::now();
    let outcome = run_import(
        live,
        &what,
        source(&files.full),
        request(
            session,
            main_table.clone(),
            TargetMode::Create,
            TransactionPolicy::Batches {
                rows: cfg.commit_rows,
            },
        ),
        None,
    )
    .await?;
    let peak = sampler.finish();
    assert_finished(&what, &outcome, cfg.rows, files.full_gen.bytes);
    let cadence = check_progress(&what, cfg, &outcome, files.full_gen.bytes, begun);
    result.method = outcome.method;
    result.rows = cfg.rows;
    result.bytes = files.full_gen.bytes;
    result.import_secs = outcome.terminal_at.duration_since(begun).as_secs_f64();
    result.cadence = cadence;
    result.rss_growth = baseline.zip(peak).map(|(b, p)| p.saturating_sub(b));
    println!(
        "M5 perf [{key}]: imported {} rows, {:.1} MB in {:.1} s: {:.0} rows/s, {:.1} MB/s; \
         {} progress events (first after {} ms, max gap {} ms / {:.2}% of the file), {} commits",
        result.rows,
        result.bytes as f64 / 1e6,
        result.import_secs,
        result.rows_per_sec(),
        result.mb_per_sec(),
        cadence.events,
        cadence.first_after.as_millis(),
        cadence.max_gap.as_millis(),
        cadence.max_gap_fraction * 100.0,
        cadence.commits
    );
    if let Some(growth) = result.rss_growth {
        println!(
            "M5 perf [{key}]: peak RSS growth {} MiB (limit {} MiB)",
            growth / (1024 * 1024),
            cfg.rss_growth_limit_mb
        );
        assert!(
            growth <= cfg.rss_growth_limit_mb * 1024 * 1024,
            "{what}: RSS grew {} MiB, limit {} MiB",
            growth / (1024 * 1024),
            cfg.rss_growth_limit_mb
        );
    } else {
        println!("M5 perf [{key}]: no /proc/self/status here; RSS not checked");
    }
    let started = Instant::now();
    let sums = live.checksum(cfg, &main_table).await;
    assert_eq!(
        sums,
        files.full_gen.checksum.expected(&key),
        "{what}: checksum (count, sum id, sum qty, notes, note length, amount cents)"
    );
    println!(
        "M5 perf [{key}]: checksum matches ({} ms): {sums:?}",
        started.elapsed().as_millis()
    );
    live.drop_table(&main_table).await;

    // 3. Cancel: append into a pre-created table, cancel at a quarter of the file.
    let what = format!("{key} cancel table");
    let created = run_import(
        live,
        &what,
        source(&files.header),
        request(
            session,
            cancel_table.clone(),
            TargetMode::Create,
            TransactionPolicy::Single,
        ),
        None,
    )
    .await?;
    assert_finished(&what, &created, 0, files.header_gen.bytes);
    assert_eq!(
        live.count(&cancel_table).await,
        0,
        "{what}: new table is empty"
    );

    let what = format!("{key} cancelled import");
    let outcome = run_import(
        live,
        &what,
        source(&files.full),
        request(
            session,
            cancel_table.clone(),
            TargetMode::Append,
            TransactionPolicy::Batches {
                rows: cfg.commit_rows,
            },
        ),
        Some(cfg.cancel_after_fraction),
    )
    .await?;
    let (_, fraction) = outcome.cancelled_at.unwrap_or_else(|| {
        panic!(
            "{what}: never reached {}% of the file before {}",
            cfg.cancel_after_fraction * 100.0,
            outcome.describe()
        )
    });
    assert!(
        matches!(outcome.terminal, ImportEvent::Cancelled { .. }),
        "{what}: expected Cancelled, got {}",
        outcome.describe()
    );
    let latency = outcome.cancel_latency.expect("cancel time recorded");
    let report = outcome.report();
    let in_table = live.count(&cancel_table).await;
    result.cancel_fraction = fraction;
    result.cancel_latency_ms = latency.as_millis();
    result.cancel_rows_committed = report.rows_committed;
    println!(
        "M5 perf [{key}]: cancelled at {:.1}% of the file; Cancelled after {} ms (limit {} ms); \
         {} rows committed, {} in the table; left behind: {}",
        fraction * 100.0,
        latency.as_millis(),
        cfg.cancel_deadline_ms,
        report.rows_committed,
        in_table,
        report.left_behind
    );
    assert!(
        latency <= Duration::from_millis(cfg.cancel_deadline_ms),
        "{what}: Cancelled arrived {} ms after cancel (limit {} ms)",
        latency.as_millis(),
        cfg.cancel_deadline_ms
    );
    assert!(
        report.rows_committed > 0,
        "{what}: no batch was committed before the cancel"
    );
    assert_eq!(
        in_table,
        i128::from(report.rows_committed),
        "{what}: the table holds exactly the committed rows"
    );
    assert!(
        !report.table_created && !report.table_dropped,
        "{what}: an append neither creates nor drops the table"
    );
    live.drop_table(&cancel_table).await;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Report

fn summary_table(
    cfg: &PerfConfig,
    generated: &Generated,
    gen_secs: f64,
    results: &[EngineResult],
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "## M5 exit check: 1 GB CSV import\n");
    let _ = writeln!(
        out,
        "Generated {} rows, {} bytes ({:.1} MB) in {:.1} s.\n",
        cfg.rows,
        generated.bytes,
        generated.bytes as f64 / 1e6,
        gen_secs
    );
    let _ = writeln!(
        out,
        "| Engine | Server | Loader | Result | Rows/s | MB/s | Import s | Progress events | Max gap ms | Max gap % | RSS growth MiB | Cancel at % | Cancel ms | Rows kept |"
    );
    let _ = writeln!(
        out,
        "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
    );
    for r in results {
        let loader = match (r.method, r.native_loader) {
            (Some(m), _) => format!("{m:?}"),
            (None, true) => "native".to_owned(),
            (None, false) => "fallback".to_owned(),
        };
        if let Some(reason) = &r.pending {
            let _ = writeln!(
                out,
                "| {} | {} | {loader} | pending: {} | | | | | | | | | | |",
                r.key,
                r.server,
                reason.replace('|', "\\|")
            );
            continue;
        }
        let _ = writeln!(
            out,
            "| {} | {} | {loader} | passed | {:.0} | {:.1} | {:.1} | {} | {} | {:.2} | {} | {:.1} | {} | {} |",
            r.key,
            r.server,
            r.rows_per_sec(),
            r.mb_per_sec(),
            r.import_secs,
            r.cadence.events,
            r.cadence.max_gap.as_millis(),
            r.cadence.max_gap_fraction * 100.0,
            r.rss_growth
                .map_or_else(|| "n/a".to_owned(), |g| (g / (1024 * 1024)).to_string()),
            r.cancel_fraction * 100.0,
            r.cancel_latency_ms,
            r.cancel_rows_committed
        );
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The exit check

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "M5 exit check: needs OXBASE_PERF_DIR, the postgres and mysql fixtures and a --profile perf build (CI job import-perf)"]
async fn import_1gb_csv() {
    install_panic_log();
    let required = env_flag("OXBASE_REQUIRE_M5_PERF");
    let Some(dir) = env_nonempty("OXBASE_PERF_DIR").map(PathBuf::from) else {
        assert!(
            !required,
            "OXBASE_REQUIRE_M5_PERF=1 but OXBASE_PERF_DIR is not set"
        );
        println!(
            "M5 perf: skipped (OXBASE_PERF_DIR is not set; the CI job import-perf runs this check)"
        );
        return;
    };
    std::fs::create_dir_all(&dir).expect("create OXBASE_PERF_DIR");
    let fx = fixtures();
    let cfg = PerfConfig::load(&fx);
    println!(
        "M5 perf: {} rows, engines {:?}, commit every {} rows, required: {required}",
        cfg.rows, cfg.engines, cfg.commit_rows
    );

    // The files: the big one, the probe and a header-only one (creates the cancel table).
    let full = dir.join("m5_perf.csv");
    let started = Instant::now();
    let full_gen = generator::write_csv(&full, cfg.rows).expect("generate the CSV");
    let gen_secs = started.elapsed().as_secs_f64();
    println!(
        "M5 perf: generated {} ({} bytes, {:.1} MB) in {:.1} s ({:.0} MB/s); checksum {:?}",
        full.file_name().unwrap_or_default().to_string_lossy(),
        full_gen.bytes,
        full_gen.bytes as f64 / 1e6,
        gen_secs,
        full_gen.bytes as f64 / 1e6 / gen_secs.max(f64::EPSILON),
        full_gen.checksum
    );
    assert_eq!(
        std::fs::metadata(&full).expect("generated file").len(),
        full_gen.bytes,
        "bytes on disk"
    );
    if cfg.rows_overridden {
        println!("M5 perf: OXBASE_PERF_ROWS is set; the 1.0-1.3 GB size bounds are not checked");
    } else {
        assert!(
            full_gen.bytes >= cfg.min_bytes,
            "the generated file has {} bytes, under min_bytes {}",
            full_gen.bytes,
            cfg.min_bytes
        );
        assert!(
            full_gen.bytes <= MAX_BYTES,
            "the generated file has {} bytes, over {MAX_BYTES}",
            full_gen.bytes
        );
    }
    let probe = dir.join("m5_perf_probe.csv");
    let probe_gen = generator::write_csv(&probe, PROBE_ROWS).expect("generate the probe CSV");
    let header = dir.join("m5_perf_header.csv");
    let header_gen = generator::write_csv(&header, 0).expect("generate the header-only CSV");
    let files = Files {
        full,
        full_gen,
        probe,
        probe_gen,
        header,
        header_gen,
    };

    let reader_pending = match probe_reader(&files.probe, PROBE_ROWS, files.probe_gen.bytes) {
        Ok(summary) => {
            println!("M5 perf: oxbase_io reads the probe file ({summary})");
            None
        }
        Err(pending) => Some(pending),
    };

    let mut results = Vec::new();
    for key in &cfg.engines {
        let run = run_engine(&cfg, &fx, key, &dir, &files, reader_pending.as_ref());
        results.push(Box::pin(run).await);
    }

    let summary = summary_table(&cfg, &files.full_gen, gen_secs, &results);
    println!("\n{summary}");
    if let Some(path) = env_nonempty("GITHUB_STEP_SUMMARY")
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
    {
        let _ = writeln!(f, "{summary}");
    }
    if !env_flag("OXBASE_PERF_KEEP_CSV") {
        for path in [&files.full, &files.probe, &files.header] {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_file(dir.join("m5_perf.sqlite"));
    }

    let pending: Vec<String> = results
        .iter()
        .filter_map(|r| r.pending.as_ref().map(|p| format!("{}: {p}", r.key)))
        .collect();
    if !pending.is_empty() {
        let message = format!(
            "M5 perf exit check PENDING (a part of M5 is not merged yet): {}",
            pending.join("; ")
        );
        assert!(
            !required,
            "{message} (OXBASE_REQUIRE_M5_PERF=1 makes a pending part a failure)"
        );
        println!("::warning::{message}");
        println!("{message}");
    }
}

// ---------------------------------------------------------------------------------------------
// The generator itself (no database; runs in every test job)

/// A strict RFC 4180 reader for the generator's output: records of fields, quoted fields with
/// doubled quotes, LF record ends outside quotes.
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut in_quotes = false;
    let mut was_quoted = false;
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' => {
                assert!(field.is_empty() && !was_quoted, "quote inside a field");
                in_quotes = true;
                was_quoted = true;
            }
            ',' => {
                record.push(std::mem::take(&mut field));
                was_quoted = false;
            }
            '\n' => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
                was_quoted = false;
            }
            '\r' => panic!("bare CR outside quotes"),
            c => {
                assert!(!was_quoted, "text after a closing quote");
                field.push(c);
            }
        }
    }
    assert!(!in_quotes, "unterminated quote");
    assert!(
        field.is_empty() && record.is_empty(),
        "no LF after the last record"
    );
    records
}

fn amount_cents_of(text: &str) -> i64 {
    let (negative, digits) = text
        .strip_prefix('-')
        .map_or((false, text), |rest| (true, rest));
    let (units, cents) = digits.split_once('.').expect("two decimals");
    assert_eq!(cents.len(), 2, "two decimals in {text}");
    let value: i64 =
        units.parse::<i64>().expect("units") * 100 + cents.parse::<i64>().expect("cents");
    if negative { -value } else { value }
}

#[test]
fn generator_is_deterministic() {
    let mut a = Vec::new();
    let mut b = Vec::new();
    let (bytes_a, sum_a) = generator::write_rows(&mut a, 3_000).unwrap();
    let (bytes_b, sum_b) = generator::write_rows(&mut b, 3_000).unwrap();
    assert_eq!(a, b);
    assert_eq!(sum_a, sum_b);
    assert_eq!(bytes_a, bytes_b);
    assert_eq!(u64::try_from(a.len()).unwrap(), bytes_a);
    let (header_bytes, header_sum) = generator::write_rows(&mut Vec::new(), 0).unwrap();
    assert_eq!(header_bytes, 45, "header line only");
    assert_eq!(header_sum, Checksum::default());
}

#[test]
fn generator_output_parses_back_to_its_checksum() {
    let rows = 5_000u64;
    let mut out = Vec::new();
    let (_, sum) = generator::write_rows(&mut out, rows).unwrap();
    let text = String::from_utf8(out).expect("UTF-8");
    let records = parse_csv(&text);
    assert_eq!(records[0], COLUMNS);
    assert_eq!(u64::try_from(records.len()).unwrap(), rows + 1);
    let mut again = Checksum::default();
    for (index, record) in records[1..].iter().enumerate() {
        let i = u64::try_from(index).unwrap() + 1;
        let expected = generator::row(i);
        assert_eq!(record.len(), 9, "row {i}: {record:?}");
        let id: u64 = record[0].parse().unwrap();
        assert_eq!(id, i);
        assert_eq!(record[1], expected.code);
        assert_eq!(record[1].len(), 9);
        assert!(record[1].starts_with('C'));
        let qty = (!record[2].is_empty()).then(|| record[2].parse::<i64>().unwrap());
        assert_eq!(qty, expected.qty, "row {i} qty");
        let cents = amount_cents_of(&record[3]);
        assert_eq!(cents, expected.amount_cents, "row {i} amount");
        let price: f64 = record[4].parse().unwrap();
        assert!(
            price.to_bits() == expected.price.to_bits(),
            "row {i} price {}",
            record[4]
        );
        assert_eq!(
            record[5],
            if i.is_multiple_of(3) { "true" } else { "false" }
        );
        let born = chrono::NaiveDate::parse_from_str(&record[6], "%Y-%m-%d").unwrap();
        assert_eq!(born, expected.born, "row {i} born");
        let at =
            chrono::NaiveDateTime::parse_from_str(&record[7], "%Y-%m-%d %H:%M:%S%.6f").unwrap();
        assert_eq!(at, expected.at, "row {i} at");
        assert_eq!(record[7].len(), 26, "row {i} at has six fraction digits");
        let note = (!record[8].is_empty()).then(|| record[8].clone());
        assert_eq!(note, expected.note, "row {i} note");

        again.count += 1;
        again.sum_id += i128::from(id);
        again.sum_qty += qty.map_or(0, i128::from);
        again.amount_cents += i128::from(cents);
        if let Some(note) = &note {
            again.notes += 1;
            again.note_chars += i128::try_from(note.chars().count()).unwrap();
            again.note_utf16 += i128::try_from(note.encode_utf16().count()).unwrap();
        }
    }
    assert_eq!(again, sum);
    assert_eq!(sum.expected("postgres")[4], sum.note_chars);
    assert_eq!(sum.expected("mssql")[4], sum.note_utf16);
    assert!(
        sum.note_utf16 > sum.note_chars,
        "the emoji is two UTF-16 units"
    );
}

#[test]
fn generator_rows_follow_the_contract() {
    let text = {
        let mut out = Vec::new();
        generator::write_rows(&mut out, 26).unwrap();
        String::from_utf8(out).unwrap()
    };
    let records = parse_csv(&text);
    // Row 13: NULL note; row 17: NULL qty; row 11 / 22: negative amount.
    assert_eq!(records[13][8], "");
    assert_eq!(records[17][2], "");
    assert!(records[11][3].starts_with('-'));
    assert!(records[22][3].starts_with('-'));
    assert!(!records[12][3].starts_with('-'));
    // Fragments: 3 has a quoted LF, 4 a quoted CRLF, 2 doubled quotes, 6 starts with "=".
    assert!(text.contains("\"line one\nline two #3\""));
    assert!(text.contains("\"line one\r\nline two #4\""));
    assert!(text.contains("\"with \"\"quotes\"\" inside #2\""));
    assert!(text.contains("\"with, a comma #1\""));
    assert_eq!(records[6][8], "=SUM(A1:A9) #6");
    assert_eq!(records[5][8], "naïve café – 日本語 🚀 #5");
    assert_eq!(records[7][8], format!("{} #7", "x".repeat(60)));
    // Row 1 in full.
    assert_eq!(
        records[1],
        [
            "1",
            "C0000063z",
            "-4969",
            "79.07",
            "0.14285714285714285",
            "false",
            "1970-02-07",
            "2000-01-01 00:00:07.000001",
            "with, a comma #1",
        ]
    );
    assert_eq!(generator::amount_text(0), "0.00");
    assert_eq!(generator::amount_text(-5), "-0.05");
}

/// 1 000 000 rows make 121 886 231 bytes; with the same rules 10 000 000 make 1 238 093 427
/// (inside the contract's 1.0-1.3 GB). The CI job checks the full size on the real file.
#[test]
fn generator_size_per_million_rows() {
    let (bytes, sum) = generator::write_rows(std::io::sink(), 1_000_000).unwrap();
    assert_eq!(bytes, 121_886_231);
    assert_eq!(sum.count, 1_000_000);
    assert_eq!(sum.sum_id, 500_000_500_000);
}
