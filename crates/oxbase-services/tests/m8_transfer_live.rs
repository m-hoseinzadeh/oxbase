//! M8 data transfer live tests (`docs/m8-contracts.md` §12, transfer) against the Docker
//! fixtures, through `oxbase_services::transfer::TransferService`:
//!
//! - **PostgreSQL → MySQL** of the `[m8.transfer]` table set (`m8_types` with the `[m8]` type
//!   set, `m8_parent`, `m8_child`, `m8_tree`), listed child first: the plan's fidelities equal
//!   `[m8.transfer.expected_mysql]`, `information_schema.COLUMNS.COLUMN_TYPE` equals the expected
//!   native types, every value reads back equal (per type rules below), the foreign keys exist
//!   after the load and `m8_parent` loads before `m8_child`;
//! - PostgreSQL → PostgreSQL (another schema), MySQL → PostgreSQL, SQL Server → PostgreSQL,
//!   SQL Server → MySQL, PostgreSQL → SQLite and SQLite → PostgreSQL of an `m8_data` table (row
//!   count and every value);
//! - `continue` mode skips a bad row and reports it; a cancel mid-table ends the run within
//!   2 s; closing the source session cancels a running transfer.
//!
//! The source objects are scratch copies built by each test (schema / database / file
//! `m8_t_<token>`, dropped at the end), so the tests do not depend on the conformance seeds;
//! their shapes follow `[m8.transfer]` in `tests/fixtures.toml`. MySQL scratch databases are
//! created as root (the fixture user may only use `oxbase_test`).
//!
//! ```sh
//! scripts/fixtures-up.sh
//! cargo test -p oxbase-services --test m8_transfer_live -- --ignored --test-threads 1
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{NaiveDateTime, NaiveTime};
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_services::transfer::{
    ColumnOverride, ErrorMode, SourceObject, TableStatus, TransactionScope, TransferEvent,
    TransferHandle, TransferOptions, TransferReport, TransferRequest, TransferService,
    TransferSpec,
};
use oxbase_services::tree::TreeService;
use oxbase_services::{
    CancellationToken, ConnectionManager, ConnectionProfile, ProvidedSecrets, SecretRef,
    SecretStore, default_dialects, default_drivers,
};
use oxbase_store::{MemorySecretStore, ProfileAddress, Secret, SecretSlot, TlsMode, TlsSettings};
use oxbase_types::{DataType, Engine, ObjectRef, Value};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn fixtures() -> toml::Table {
    let path = root().join("tests/fixtures.toml");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
        .parse()
        .expect("fixtures.toml parses")
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

/// 8 lowercase base36 characters from the time and the process id (unique per run).
fn token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut n = nanos ^ (u128::from(std::process::id()) << 64);
    let mut out = String::new();
    for _ in 0..8 {
        let digit = u32::try_from(n % 36).unwrap_or(0);
        out.push(char::from_digit(digit, 36).unwrap_or('x'));
        n /= 36;
    }
    out
}

/// The connection manager, the transfer service and the fixture table.
struct Live {
    fx: toml::Table,
    manager: Arc<ConnectionManager>,
    secrets: Arc<MemorySecretStore>,
    service: TransferService,
    _dir: tempfile::TempDir,
    dir: PathBuf,
}

impl Live {
    fn new() -> Self {
        let secrets = Arc::new(MemorySecretStore::new());
        let manager = Arc::new(ConnectionManager::new(
            default_drivers(),
            secrets.clone(),
            Arc::new(RusshTunnelProvider::new()),
        ));
        let dialects = Arc::new(default_dialects());
        let tree = Arc::new(TreeService::new(
            Arc::clone(&manager),
            Arc::clone(&dialects),
        ));
        let service = TransferService::new(Arc::clone(&manager), tree, dialects);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        Self {
            fx: fixtures(),
            manager,
            secrets,
            service,
            _dir: dir,
            dir: path,
        }
    }

    /// A session on engine `key` (`root`: MySQL / MariaDB as root, who may create databases).
    async fn open(&self, key: &str, engine: Engine, root_user: bool) -> Session {
        let e = table(table(&self.fx, "engines"), key);
        let mut p = ConnectionProfile::new(
            format!("m8 {key}"),
            engine,
            ProfileAddress::Network {
                host: string(e, "host"),
                port: e
                    .get("port")
                    .and_then(toml::Value::as_integer)
                    .and_then(|p| u16::try_from(p).ok()),
            },
        );
        let (user, password) = if root_user {
            ("root".to_owned(), string(e, "root_password"))
        } else {
            (string(e, "user"), string(e, "password"))
        };
        p.user = Some(user);
        p.database = Some(string(e, "database"));
        let secret = p.secret_key(SecretSlot::Password);
        self.secrets
            .set(&secret, &Secret::new(password))
            .await
            .unwrap();
        p.password = Some(SecretRef::Keychain { key: secret });
        p.tls = TlsSettings {
            mode: TlsMode::Disable,
            ..TlsSettings::default()
        };
        p.connect_timeout_secs = Some(15);
        self.connect(&p).await
    }

    /// A session on a new SQLite file in the test's temp directory.
    async fn sqlite(&self, name: &str) -> Session {
        let p = ConnectionProfile::new(
            format!("m8 sqlite {name}"),
            Engine::Sqlite,
            ProfileAddress::File {
                path: self.dir.join(format!("{name}.sqlite")),
            },
        );
        self.connect(&p).await
    }

    async fn connect(&self, p: &ConnectionProfile) -> Session {
        let handle = self
            .manager
            .open(p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{}: open failed: {e}", p.name));
        Session { handle }
    }

    async fn run(&self, request: TransferRequest) -> (Vec<TransferEvent>, TransferReport) {
        let mut handle = self
            .service
            .start(request)
            .await
            .unwrap_or_else(|e| panic!("start failed: {e}"));
        let events = drain(&mut handle, Duration::from_secs(120)).await;
        let report = match events.last() {
            Some(TransferEvent::Finished { report }) => report.clone(),
            other => panic!("the transfer did not finish: {other:?}"),
        };
        (events, report)
    }
}

/// Every event of a run up to the terminal one.
async fn drain(handle: &mut TransferHandle, limit: Duration) -> Vec<TransferEvent> {
    let mut events = Vec::new();
    let deadline = Instant::now() + limit;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, handle.events.recv()).await {
            Ok(Some(event)) => {
                let terminal = event.is_terminal();
                events.push(event);
                if terminal {
                    return events;
                }
            }
            Ok(None) => panic!("the event stream ended without a terminal event: {events:?}"),
            Err(_) => panic!("no terminal event within {limit:?}"),
        }
    }
}

struct Session {
    handle: oxbase_services::SessionHandle,
}

impl Session {
    fn id(&self) -> oxbase_services::SessionId {
        self.handle.id()
    }

    /// Runs `sql` (one statement) and returns its rows as values.
    async fn query(&self, sql: &str) -> Vec<Vec<Value>> {
        let _guard = self.handle.exclusive().await;
        let stream = self
            .handle
            .connection()
            .execute(sql, &[], CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let collected = oxbase_driver::collect(stream)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let mut rows = Vec::new();
        for set in &collected.result_sets {
            for batch in &set.batches {
                for row in 0..batch.num_rows() {
                    rows.push(
                        batch
                            .columns()
                            .iter()
                            .map(|c| oxbase_driver::cell_value(c.as_ref(), row).unwrap())
                            .collect(),
                    );
                }
            }
        }
        rows
    }

    /// Runs each statement.
    async fn exec(&self, statements: &[String]) {
        for sql in statements {
            self.query(sql).await;
        }
    }

    /// Runs each statement, ignoring errors (clean-up).
    async fn try_exec(&self, statements: &[String]) {
        let _guard = self.handle.exclusive().await;
        for sql in statements {
            if let Ok(stream) = self
                .handle
                .connection()
                .execute(sql, &[], CancellationToken::new())
                .await
            {
                let _ = oxbase_driver::collect(stream).await;
            }
        }
    }

    async fn count(&self, sql: &str) -> i64 {
        match self.query(sql).await.first().and_then(|r| r.first()) {
            Some(Value::Int(n)) => *n,
            Some(Value::UInt(n)) => i64::try_from(*n).unwrap(),
            Some(Value::Decimal(t) | Value::Text(t)) => t.parse().unwrap(),
            other => panic!("{sql}: not a count: {other:?}"),
        }
    }
}

fn spec(objects: Vec<SourceObject>, target: ObjectRef) -> TransferSpec {
    TransferSpec {
        objects,
        target,
        options: TransferOptions::default(),
        overrides: Vec::new(),
    }
}

fn source_table(catalog: Option<&str>, schema: Option<&str>, name: &str) -> SourceObject {
    SourceObject::Table {
        table: ObjectRef::table(catalog, schema, name),
        target_name: None,
        filter: None,
    }
}

fn request(source: &Session, target: &Session, spec: TransferSpec) -> TransferRequest {
    TransferRequest {
        source_session: source.id(),
        target_session: target.id(),
        spec,
        confirm_destructive: false,
    }
}

// ------------------------------------------------------------------------------------------
// Value comparison

/// A decimal or integer text without trailing fraction zeros (`12.50` → `12.5`, `3.0` → `3`).
fn decimal(text: &str) -> String {
    let t = text.trim();
    if !t.contains('.') {
        return t.to_owned();
    }
    let t = t.trim_end_matches('0').trim_end_matches('.');
    if t == "-0" {
        "0".to_owned()
    } else {
        t.to_owned()
    }
}

fn json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Json(j) => j.clone(),
        Value::Text(t) => serde_json::from_str(t).unwrap_or_else(|e| panic!("{t}: {e}")),
        other => panic!("not JSON: {other:?}"),
    }
}

fn instant(value: &Value) -> Option<NaiveDateTime> {
    match value {
        Value::Timestamp(t) => Some(*t),
        Value::TimestampTz(t) => Some(t.naive_utc()),
        Value::Text(t) => ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"]
            .iter()
            .find_map(|f| NaiveDateTime::parse_from_str(t.trim(), f).ok()),
        _ => None,
    }
}

fn time(value: &Value) -> Option<NaiveTime> {
    match value {
        Value::Time(t) => Some(*t),
        Value::Text(t) => NaiveTime::parse_from_str(t.trim(), "%H:%M:%S%.f").ok(),
        _ => None,
    }
}

/// A value's comparable text form: booleans `1` / `0`, numbers without trailing zeros, bytes
/// as hex, temporal values in ISO form, texts as they are.
fn canon(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Bool(b) => if *b { "1" } else { "0" }.to_owned(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => decimal(&f.to_string()),
        Value::Decimal(t) => decimal(t),
        Value::Text(t) => instant(value).map_or_else(
            || {
                if t.parse::<f64>().is_ok() && !t.contains(['e', 'E']) {
                    decimal(t)
                } else {
                    t.clone()
                }
            },
            |ts| ts.format("%Y-%m-%d %H:%M:%S%.6f").to_string(),
        ),
        Value::Bytes(b) => b.iter().fold(String::new(), |mut out, x| {
            use std::fmt::Write as _;
            let _ = write!(out, "{x:02x}");
            out
        }),
        Value::Date(d) => d.to_string(),
        Value::Time(t) => t.format("%H:%M:%S%.6f").to_string(),
        Value::Timestamp(_) | Value::TimestampTz(_) => instant(value)
            .map_or_else(String::new, |t| {
                t.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
            }),
        Value::Uuid(u) => u.to_string(),
        Value::Json(j) => j.to_string(),
    }
}

/// The bits of `x` as a 32-bit float (`real` columns hold f32 values).
#[allow(
    clippy::cast_possible_truncation,
    reason = "the values come from 32-bit float columns"
)]
fn f32_bits(x: f64) -> u32 {
    (x as f32).to_bits()
}

/// `true` when `a` (source) and `b` (target) hold the same value of column `column` (rules of
/// `[m8.transfer]`: instants in UTC, JSON as equal values, `char(n)` padding ignored).
fn same(column: &str, a: &Value, b: &Value) -> bool {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return matches!(a, Value::Null) && matches!(b, Value::Null);
    }
    match column {
        "c_real" => match (a, b) {
            (Value::Float(x), Value::Float(y)) => f32_bits(*x) == f32_bits(*y),
            _ => false,
        },
        "c_double" => match (a, b) {
            (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
            _ => false,
        },
        "c_json" | "c_jsonb" | "c_int_array" | "c_text_array" => json(a) == json(b),
        "c_time" => time(a).is_some() && time(a) == time(b),
        "c_timestamp" | "c_timestamptz" | "created" => {
            instant(a).is_some() && instant(a) == instant(b)
        }
        "c_char" => canon(a).trim_end() == canon(b).trim_end(),
        _ => canon(a) == canon(b),
    }
}

/// Compares two row sets column by column (both ordered by their first column).
fn assert_same_rows(columns: &[&str], source: &[Vec<Value>], target: &[Vec<Value>], what: &str) {
    assert_eq!(source.len(), target.len(), "{what}: row counts");
    for (r, (a, b)) in source.iter().zip(target).enumerate() {
        for (i, column) in columns.iter().enumerate() {
            assert!(
                same(column, &a[i], &b[i]),
                "{what}: row {r}, column {column}: source {:?}, target {:?}",
                a[i],
                b[i]
            );
        }
    }
}

// ------------------------------------------------------------------------------------------
// PostgreSQL → MySQL: the [m8] type set

const TYPES_COLUMNS: [&str; 24] = [
    "id",
    "c_bool",
    "c_int2",
    "c_int4",
    "c_int8",
    "c_numeric",
    "c_numeric_free",
    "c_real",
    "c_double",
    "c_char",
    "c_varchar",
    "c_text",
    "c_bytea",
    "c_date",
    "c_time",
    "c_timestamp",
    "c_timestamptz",
    "c_interval",
    "c_uuid",
    "c_json",
    "c_jsonb",
    "c_int_array",
    "c_text_array",
    "c_enum",
];

/// The `[m8.transfer]` source set in PostgreSQL schema `s`.
fn pg_transfer_set(s: &str) -> Vec<String> {
    vec![
        format!("CREATE SCHEMA {s}"),
        format!("CREATE TYPE {s}.m8_mood AS ENUM ('sad', 'ok', 'happy')"),
        format!(
            "CREATE TABLE {s}.m8_types (id serial PRIMARY KEY, c_bool boolean, c_int2 smallint, \
             c_int4 integer, c_int8 bigint, c_numeric numeric(38,10), c_numeric_free numeric, \
             c_real real, c_double double precision, c_char char(10), c_varchar varchar(255), \
             c_text text, c_bytea bytea, c_date date, c_time time(6), c_timestamp timestamp(6), \
             c_timestamptz timestamptz(6), c_interval interval, c_uuid uuid, c_json json, \
             c_jsonb jsonb, c_int_array integer[], c_text_array text[], c_enum {s}.m8_mood)"
        ),
        format!(
            "INSERT INTO {s}.m8_types (c_bool, c_int2, c_int4, c_int8, c_numeric, \
             c_numeric_free, c_real, c_double, c_char, c_varchar, c_text, c_bytea, c_date, \
             c_time, c_timestamp, c_timestamptz, c_interval, c_uuid, c_json, c_jsonb, \
             c_int_array, c_text_array, c_enum) VALUES \
             (true, 12, 1234, 1234567890123, 12345678901234567890.0123456789, \
              12345678901234567890.0123456789, 1.5, 2.25, 'abc', 'héllo wörld', \
              'unicode ☃ text', '\\x00ff0a', '2026-03-01', '12:34:56.789012', \
              '2026-03-01 12:34:56.789012', '2026-03-01 10:00:00+05:30', '1 day 02:03:04.5', \
              '6f1c2a3e-0000-4000-8000-00000000abcd', '{{\"b\": [1, 2], \"a\": null}}', \
              '{{\"b\": [1, 2], \"a\": null}}', '{{1,2,3}}', '{{\"x\",\"y,z\"}}', 'happy'), \
             (false, -32768, -2147483648, -9223372036854775808, \
              -9999999999999999999999999999.9999999999, -0.000000000000000000000000000001, \
              -1.25, 1.7976931348623157e308, '', '', '😀', '\\x', '1970-01-01', \
              '23:59:59.999999', '1970-01-01 00:00:00', '1970-01-01 00:00:00+00', \
              '-1 days -00:00:01', '00000000-0000-0000-0000-000000000000', '[]', '{{}}', \
              '{{}}', '{{}}', 'sad')"
        ),
        format!("INSERT INTO {s}.m8_types DEFAULT VALUES"),
        format!("CREATE TABLE {s}.m8_parent (id integer PRIMARY KEY, name varchar(40) NOT NULL)"),
        format!(
            "CREATE TABLE {s}.m8_child (id integer PRIMARY KEY, parent_id integer NOT NULL \
             REFERENCES {s}.m8_parent (id), note text)"
        ),
        format!(
            "CREATE TABLE {s}.m8_tree (id integer PRIMARY KEY, parent_id integer REFERENCES \
             {s}.m8_tree (id), label varchar(40))"
        ),
        format!("INSERT INTO {s}.m8_parent VALUES (1, 'one'), (2, 'two')"),
        format!("INSERT INTO {s}.m8_child VALUES (10, 1, 'a'), (11, 2, NULL), (12, 2, 'c')"),
        format!("INSERT INTO {s}.m8_tree VALUES (1, NULL, 'root'), (2, 1, 'leaf')"),
    ]
}

/// What PostgreSQL reads back for comparison (arrays as JSON).
fn pg_types_select(s: &str) -> String {
    let columns: Vec<String> = TYPES_COLUMNS
        .iter()
        .map(|c| {
            if c.ends_with("_array") {
                format!("to_json({c})::text")
            } else {
                (*c).to_owned()
            }
        })
        .collect();
    format!(
        "SELECT {} FROM {s}.m8_types ORDER BY id",
        columns.join(", ")
    )
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn postgres_to_mysql_type_set() {
    let live = Live::new();
    let pg = live.open("postgres", Engine::Postgres, false).await;
    let my = live.open("mysql", Engine::MySql, true).await;
    let s = format!("m8_t_x{}", token());
    pg.try_exec(&[format!("DROP SCHEMA IF EXISTS {s} CASCADE")])
        .await;
    pg.exec(&pg_transfer_set(&s)).await;
    my.exec(&[format!("CREATE DATABASE {s}")]).await;

    let db = string(table(table(&live.fx, "engines"), "postgres"), "database");
    // Child first: the plan must reorder.
    let objects = ["m8_child", "m8_types", "m8_tree", "m8_parent"]
        .iter()
        .map(|t| source_table(Some(&db), Some(&s), t))
        .collect();
    let req = request(&pg, &my, spec(objects, ObjectRef::database(s.clone())));

    let plan = live
        .service
        .plan(&req, CancellationToken::new())
        .await
        .unwrap();
    assert!(plan.runnable, "{:?}", plan.blocked);
    assert!(!plan.transactional_ddl);
    let position = |name: &str| {
        plan.order
            .iter()
            .position(|&i| plan.tables[i].target.name == name)
            .unwrap()
    };
    assert!(position("m8_parent") < position("m8_child"));
    let expected = table(table(table(&live.fx, "m8"), "transfer"), "expected_mysql");
    let types = &plan.tables[1];
    assert_eq!(types.target.name, "m8_types");
    for column in &types.columns {
        let want = expected
            .get(&column.source)
            .and_then(toml::Value::as_array)
            .unwrap_or_else(|| panic!("no expectation for {}", column.source));
        assert_eq!(
            column.mapping.fidelity.as_str(),
            want[1].as_str().unwrap(),
            "{}: {:?}",
            column.source,
            column.mapping
        );
    }
    assert_eq!(types.columns.len(), expected.len());
    let enum_plan = types.columns.iter().find(|c| c.source == "c_enum").unwrap();
    assert_eq!(
        enum_plan.source_type,
        DataType::Enum {
            labels: vec!["sad".into(), "ok".into(), "happy".into()]
        }
    );

    let (events, report) = live.run(req).await;
    for t in &report.tables {
        assert_eq!(t.status, TableStatus::Done, "{t:?}");
    }
    assert_eq!(report.foreign_keys_added, 2, "{report:?}");
    let started: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            TransferEvent::TableStarted { table } => Some(*table),
            _ => None,
        })
        .collect();
    let parent = started.iter().position(|&t| t == 3).unwrap();
    let child = started.iter().position(|&t| t == 0).unwrap();
    assert!(parent < child, "{started:?}");

    // Column types.
    let rows = my
        .query(&format!(
            "SELECT COLUMN_NAME, COLUMN_TYPE FROM information_schema.COLUMNS WHERE \
             TABLE_SCHEMA = '{s}' AND TABLE_NAME = 'm8_types'"
        ))
        .await;
    assert_eq!(rows.len(), TYPES_COLUMNS.len());
    for row in rows {
        let name = canon(&row[0]);
        let column_type = canon(&row[1]);
        let want = expected.get(&name).and_then(toml::Value::as_array).unwrap();
        assert_eq!(column_type, want[0].as_str().unwrap(), "{name}");
    }

    // Values.
    let source = pg.query(&pg_types_select(&s)).await;
    let target = my
        .query(&format!(
            "SELECT {} FROM {s}.m8_types ORDER BY id",
            TYPES_COLUMNS.join(", ")
        ))
        .await;
    assert_eq!(source.len(), 3);
    assert_same_rows(&TYPES_COLUMNS, &source, &target, "m8_types");
    for (t, n) in [("m8_parent", 2), ("m8_child", 3), ("m8_tree", 2)] {
        assert_eq!(
            my.count(&format!("SELECT COUNT(*) FROM {s}.{t}")).await,
            n,
            "{t}"
        );
    }
    assert_eq!(
        my.count(&format!(
            "SELECT COUNT(*) FROM information_schema.REFERENTIAL_CONSTRAINTS WHERE \
             CONSTRAINT_SCHEMA = '{s}'"
        ))
        .await,
        2
    );
    // The identity counter continues after the copied ids.
    my.exec(&[format!("INSERT INTO {s}.m8_types (c_bool) VALUES (true)")])
        .await;
    assert_eq!(
        my.count(&format!("SELECT MAX(id) FROM {s}.m8_types")).await,
        4
    );

    my.try_exec(&[format!("DROP DATABASE {s}")]).await;
    pg.try_exec(&[format!("DROP SCHEMA {s} CASCADE")]).await;
}

// ------------------------------------------------------------------------------------------
// m8_data between engines

const DATA_COLUMNS: [&str; 6] = ["id", "name", "amount", "created", "flag", "note"];

fn data_select(qualified: &str) -> String {
    format!(
        "SELECT {} FROM {qualified} ORDER BY id",
        DATA_COLUMNS.join(", ")
    )
}

fn pg_data(s: &str) -> Vec<String> {
    vec![
        format!("CREATE SCHEMA {s}"),
        format!(
            "CREATE TABLE {s}.m8_data (id serial PRIMARY KEY, name varchar(40) NOT NULL, \
             amount numeric(12,2), created timestamp(6), flag boolean, note text)"
        ),
        format!("CREATE INDEX m8_data_name_{s} ON {s}.m8_data (name)"),
        format!(
            "INSERT INTO {s}.m8_data (name, amount, created, flag, note) VALUES \
             ('alpha', 12.50, '2026-01-02 03:04:05.123456', true, NULL), \
             ('béta', -0.01, '1999-12-31 23:59:59', false, 'line 1\nline 2'), \
             ('gamma', NULL, NULL, NULL, 'it''s'), \
             ('delta', 9999999999.99, '2026-06-30 00:00:00', true, '😀')"
        ),
    ]
}

/// Copies `m8_data` from `source` (`from`) to `target` (`to`) and compares every value.
async fn data_case(
    live: &Live,
    source: &Session,
    from: (Option<&str>, Option<&str>),
    target: &Session,
    to: ObjectRef,
    target_qualified: &str,
    source_qualified: &str,
    what: &str,
) {
    let objects = vec![source_table(from.0, from.1, "m8_data")];
    let (_, report) = live.run(request(source, target, spec(objects, to))).await;
    assert_eq!(
        report.tables[0].status,
        TableStatus::Done,
        "{what}: {report:?}"
    );
    assert_eq!(report.tables[0].progress.rows_written, 4, "{what}");
    let a = source.query(&data_select(source_qualified)).await;
    let b = target.query(&data_select(target_qualified)).await;
    assert_eq!(a.len(), 4, "{what}");
    assert_same_rows(&DATA_COLUMNS, &a, &b, what);
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn postgres_to_postgres_another_schema() {
    let live = Live::new();
    let pg = live.open("postgres", Engine::Postgres, false).await;
    let (s, t) = (format!("m8_t_a{}", token()), format!("m8_t_b{}", token()));
    pg.exec(&pg_data(&s)).await;
    pg.exec(&[format!("CREATE SCHEMA {t}")]).await;
    data_case(
        &live,
        &pg,
        (Some("oxbase_test"), Some(&s)),
        &pg,
        ObjectRef::schema(Some("oxbase_test"), t.clone()),
        &format!("{t}.m8_data"),
        &format!("{s}.m8_data"),
        "postgres -> postgres",
    )
    .await;
    // Same engine: the same text for every row.
    let checksum = |schema: &str| {
        format!("SELECT md5(string_agg(d::text, '|' ORDER BY id)) FROM {schema}.m8_data d")
    };
    assert_eq!(
        canon(&pg.query(&checksum(&s)).await[0][0]),
        canon(&pg.query(&checksum(&t)).await[0][0])
    );
    // The serial continues after the copied ids.
    pg.exec(&[format!("INSERT INTO {t}.m8_data (name) VALUES ('next')")])
        .await;
    assert_eq!(
        pg.count(&format!("SELECT max(id) FROM {t}.m8_data")).await,
        5
    );
    pg.try_exec(&[
        format!("DROP SCHEMA {s} CASCADE"),
        format!("DROP SCHEMA {t} CASCADE"),
    ])
    .await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn mysql_to_postgres() {
    let live = Live::new();
    let my = live.open("mysql", Engine::MySql, true).await;
    let pg = live.open("postgres", Engine::Postgres, false).await;
    let s = format!("m8_t_m{}", token());
    my.exec(&[
        format!("CREATE DATABASE {s}"),
        format!(
            "CREATE TABLE {s}.m8_data (id int AUTO_INCREMENT PRIMARY KEY, name varchar(40) NOT \
             NULL, amount decimal(12,2), created datetime(6), flag tinyint(1), note text, \
             KEY m8_data_name (name)) ENGINE=InnoDB"
        ),
        format!(
            "INSERT INTO {s}.m8_data (name, amount, created, flag, note) VALUES \
             ('alpha', 12.50, '2026-01-02 03:04:05.123456', 1, NULL), \
             ('béta', -0.01, '1999-12-31 23:59:59', 0, 'line 1\\nline 2'), \
             ('gamma', NULL, NULL, NULL, 'it''s'), \
             ('delta', 9999999999.99, '2026-06-30 00:00:00', 1, '😀')"
        ),
    ])
    .await;
    pg.exec(&[format!("CREATE SCHEMA {s}")]).await;
    data_case(
        &live,
        &my,
        (Some(&s), None),
        &pg,
        ObjectRef::schema(Some("oxbase_test"), s.clone()),
        &format!("{s}.m8_data"),
        &format!("{s}.m8_data"),
        "mysql -> postgres",
    )
    .await;
    // The serial sequence was moved past the copied ids.
    pg.exec(&[format!("INSERT INTO {s}.m8_data (name) VALUES ('next')")])
        .await;
    assert_eq!(
        pg.count(&format!("SELECT max(id) FROM {s}.m8_data")).await,
        5
    );
    my.try_exec(&[format!("DROP DATABASE {s}")]).await;
    pg.try_exec(&[format!("DROP SCHEMA {s} CASCADE")]).await;
}

async fn mssql_source(ms: &Session, s: &str) {
    ms.exec(&[
        format!("CREATE SCHEMA {s}"),
        format!(
            "CREATE TABLE {s}.m8_data (id int IDENTITY(1,1) PRIMARY KEY, name nvarchar(40) NOT \
             NULL, amount decimal(12,2), created datetime2(6), flag bit, note nvarchar(max))"
        ),
        format!(
            "INSERT INTO {s}.m8_data (name, amount, created, flag, note) VALUES \
             (N'alpha', 12.50, '2026-01-02T03:04:05.123456', 1, NULL), \
             (N'béta', -0.01, '1999-12-31T23:59:59', 0, N'line 1' + CHAR(10) + N'line 2'), \
             (N'gamma', NULL, NULL, NULL, N'it''s'), \
             (N'delta', 9999999999.99, '2026-06-30T00:00:00', 1, N'😀')"
        ),
    ])
    .await;
}

async fn mssql_drop(ms: &Session, s: &str) {
    ms.try_exec(&[
        format!("DROP TABLE IF EXISTS {s}.m8_data"),
        format!("DROP SCHEMA IF EXISTS {s}"),
    ])
    .await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn mssql_to_postgres_and_mysql() {
    let live = Live::new();
    let ms = live.open("mssql", Engine::MsSql, false).await;
    let pg = live.open("postgres", Engine::Postgres, false).await;
    let my = live.open("mysql", Engine::MySql, true).await;
    let s = format!("m8_t_s{}", token());
    mssql_source(&ms, &s).await;
    pg.exec(&[format!("CREATE SCHEMA {s}")]).await;
    my.exec(&[format!("CREATE DATABASE {s}")]).await;
    let from = (Some("oxbase_test"), Some(s.as_str()));
    let qualified = format!("{s}.m8_data");
    data_case(
        &live,
        &ms,
        from,
        &pg,
        ObjectRef::schema(Some("oxbase_test"), s.clone()),
        &qualified,
        &qualified,
        "mssql -> postgres",
    )
    .await;
    data_case(
        &live,
        &ms,
        from,
        &my,
        ObjectRef::database(s.clone()),
        &qualified,
        &qualified,
        "mssql -> mysql",
    )
    .await;
    mssql_drop(&ms, &s).await;
    pg.try_exec(&[format!("DROP SCHEMA {s} CASCADE")]).await;
    my.try_exec(&[format!("DROP DATABASE {s}")]).await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn postgres_to_sqlite_and_back() {
    let live = Live::new();
    let pg = live.open("postgres", Engine::Postgres, false).await;
    let lite = live.sqlite("m8_t_copy").await;
    let (s, back) = (format!("m8_t_l{}", token()), format!("m8_t_r{}", token()));
    pg.exec(&pg_data(&s)).await;
    data_case(
        &live,
        &pg,
        (Some("oxbase_test"), Some(&s)),
        &lite,
        ObjectRef::database("main"),
        "main.m8_data",
        &format!("{s}.m8_data"),
        "postgres -> sqlite",
    )
    .await;
    pg.exec(&[format!("CREATE SCHEMA {back}")]).await;
    data_case(
        &live,
        &lite,
        (Some("main"), None),
        &pg,
        ObjectRef::schema(Some("oxbase_test"), back.clone()),
        &format!("{back}.m8_data"),
        "main.m8_data",
        "sqlite -> postgres",
    )
    .await;
    pg.try_exec(&[
        format!("DROP SCHEMA {s} CASCADE"),
        format!("DROP SCHEMA {back} CASCADE"),
    ])
    .await;
}

// ------------------------------------------------------------------------------------------
// Error mode, cancel, closing the source session

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn continue_mode_skips_and_reports_a_bad_row() {
    let live = Live::new();
    let pg = live.open("postgres", Engine::Postgres, false).await;
    let (s, t) = (format!("m8_t_c{}", token()), format!("m8_t_d{}", token()));
    pg.exec(&pg_data(&s)).await;
    pg.exec(&[format!("CREATE SCHEMA {t}")]).await;
    let mut transfer = spec(
        vec![source_table(Some("oxbase_test"), Some(&s), "m8_data")],
        ObjectRef::schema(Some("oxbase_test"), t.clone()),
    );
    // Every name fits 5 characters.
    transfer.overrides.push(ColumnOverride {
        object: 0,
        column: "name".into(),
        skip: false,
        target_name: None,
        data_type: Some(DataType::VarChar { length: Some(5) }),
        native_type: None,
    });
    transfer.options.errors = ErrorMode::Continue;
    let (events, report) = live.run(request(&pg, &pg, transfer.clone())).await;
    let table = &report.tables[0];
    assert_eq!(table.status, TableStatus::Done, "{table:?}");
    assert_eq!(table.progress.rows_read, 4);
    assert_eq!(table.progress.rows_skipped, 0);
    assert_eq!(
        pg.count(&format!("SELECT count(*) FROM {t}.m8_data")).await,
        4
    );

    // 4 characters: only `béta` fits; three rows are skipped and reported.
    pg.exec(&[format!("DROP TABLE {t}.m8_data")]).await;
    transfer.overrides[0].data_type = Some(DataType::VarChar { length: Some(4) });
    let (events2, report) = live.run(request(&pg, &pg, transfer.clone())).await;
    let table = &report.tables[0];
    assert_eq!(table.status, TableStatus::Done, "{table:?}");
    assert_eq!(table.progress.rows_skipped, 4 - 1, "{table:?}");
    assert_eq!(table.issues.len(), 3);
    assert_eq!(table.issues[0].column.as_deref(), Some("name"));
    assert!(
        events2
            .iter()
            .any(|e| matches!(e, TransferEvent::RowError { .. })),
        "{events2:?}"
    );
    assert_ne!(events.len(), 0);
    assert_eq!(
        pg.count(&format!("SELECT count(*) FROM {t}.m8_data")).await,
        1
    );

    // `stop` (the default) fails the table at the first issue; the created table rolls back.
    pg.exec(&[format!("DROP TABLE {t}.m8_data")]).await;
    transfer.options.errors = ErrorMode::Stop;
    let mut handle = live
        .service
        .start(request(&pg, &pg, transfer))
        .await
        .unwrap();
    let events = drain(&mut handle, Duration::from_secs(60)).await;
    match events.last() {
        Some(TransferEvent::Failed { error, report }) => {
            assert!(error.message.contains("row 1"), "{error:?}");
            assert_eq!(report.tables[0].status, TableStatus::Failed);
            assert_eq!(
                report.tables[0].left_behind,
                "rolled back: no table was created"
            );
        }
        other => panic!("expected a failed transfer: {other:?}"),
    }
    assert_eq!(
        pg.count(&format!(
            "SELECT count(*) FROM information_schema.tables WHERE table_schema = '{t}'"
        ))
        .await,
        0
    );
    pg.try_exec(&[
        format!("DROP SCHEMA {s} CASCADE"),
        format!("DROP SCHEMA {t} CASCADE"),
    ])
    .await;
}

/// A PostgreSQL schema with a 400 000-row table.
async fn big_source(pg: &Session, s: &str) {
    pg.exec(&[
        format!("CREATE SCHEMA {s}"),
        format!("CREATE TABLE {s}.m8_big (id integer PRIMARY KEY, payload text NOT NULL)"),
        format!(
            "INSERT INTO {s}.m8_big SELECT g, repeat(md5(g::text), 4) FROM \
             generate_series(1, 400000) g"
        ),
    ])
    .await;
}

/// Starts a copy of `m8_big`, waits for its first progress event, calls `stop`, and checks
/// that the run ends `Cancelled` within 2 s.
async fn cancel_case(live: &Live, pg: &Session, stop: impl FnOnce(&TransferHandle)) -> String {
    let (s, t) = (format!("m8_t_g{}", token()), format!("m8_t_h{}", token()));
    big_source(pg, &s).await;
    pg.exec(&[format!("CREATE SCHEMA {t}")]).await;
    let mut transfer = spec(
        vec![source_table(Some("oxbase_test"), Some(&s), "m8_big")],
        ObjectRef::schema(Some("oxbase_test"), t.clone()),
    );
    transfer.options.batch_rows = 1_000;
    transfer.options.transaction = TransactionScope::PerTable;
    let mut handle = live.service.start(request(pg, pg, transfer)).await.unwrap();
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(60), handle.events.recv())
            .await
            .expect("an event within 60 s")
            .expect("an open event stream");
        let progress = matches!(event, TransferEvent::Progress { .. });
        assert!(
            !event.is_terminal(),
            "finished before the cancel: {event:?}"
        );
        events.push(event);
        if progress {
            break;
        }
    }
    assert!(live.service.is_running(handle.id));
    let stopped = Instant::now();
    stop(&handle);
    let rest = drain(&mut handle, Duration::from_secs(10)).await;
    assert!(
        stopped.elapsed() < Duration::from_secs(2),
        "took {:?}",
        stopped.elapsed()
    );
    let left = match rest.last() {
        Some(TransferEvent::Cancelled { report }) => {
            assert_eq!(report.tables[0].status, TableStatus::Cancelled);
            report.tables[0].left_behind.clone()
        }
        other => panic!("expected a cancelled transfer: {other:?}"),
    };
    // PostgreSQL DDL is transactional: the created table is gone with the rollback.
    assert_eq!(
        pg.count(&format!(
            "SELECT count(*) FROM information_schema.tables WHERE table_schema = '{t}'"
        ))
        .await,
        0
    );
    pg.try_exec(&[
        format!("DROP SCHEMA {s} CASCADE"),
        format!("DROP SCHEMA {t} CASCADE"),
    ])
    .await;
    left
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn a_cancel_mid_table_ends_within_two_seconds() {
    let live = Live::new();
    let pg = live.open("postgres", Engine::Postgres, false).await;
    let service = &live.service;
    let left = cancel_case(&live, &pg, |handle| {
        assert!(service.cancel(handle.id));
    })
    .await;
    assert_eq!(left, "rolled back: no table was created");
}

#[tokio::test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn closing_the_source_session_cancels_the_transfer() {
    let live = Live::new();
    let pg = live.open("postgres", Engine::Postgres, false).await;
    let source = live.open("postgres", Engine::Postgres, false).await;
    let service = &live.service;
    let id = source.id();
    let left = cancel_case(&live, &pg, |_| {
        // What `close_connection` does first (`SyncState::cancel_session`).
        assert_eq!(service.cancel_session(id), 0);
        assert_eq!(service.cancel_session(pg.id()), 1);
    })
    .await;
    assert_ne!(left, "");
    live.manager.close(pg.id()).await.unwrap();
    live.manager.close(source.id()).await.unwrap();
}
