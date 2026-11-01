//! Live tests of the SQLite bulk loader (M5, `docs/m5-contracts.md` §4, §12 *load*) on a fresh
//! temporary database file per test (`[m5.sqlite]`):
//!
//! - 100 000 rows of every canonical load type (NULLs, Unicode, text with tabs / CR / LF /
//!   backslashes, decimals kept exact in a `TEXT` column, 64-bit extremes, blobs with empty
//!   values, dates, times, timestamps with and without zone as ISO text, UUIDs, JSON) loaded
//!   in four calls inside one transaction, every call split under the 32 766-parameter limit;
//!   checked by counts and sums and an exact read-back of sample rows;
//! - a cancel during a large call: `Cancelled` within 2 s, and the rolled-back transaction
//!   leaves no row;
//! - a duplicate key fails the call.
//!
//! The scratch tables are `m5_t_bulk<case>` in the temporary file, which is deleted with its
//! directory. `#[ignore]`d like the other engines' live tests (the conformance job runs them):
//! `cargo test -p oxbase-sqlite --test bulk_live -- --include-ignored`.

#![allow(
    clippy::many_single_char_names,
    reason = "the connection, table, batch and row are c, t, b and i throughout"
)]

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_array::{
    ArrayRef, BinaryArray, BooleanArray, Date32Array, FixedSizeBinaryArray, Float64Array,
    Int64Array, RecordBatch, StringArray, Time64MicrosecondArray, TimestampMicrosecondArray,
};
use oxbase_dialect::DialectRegistry;
use oxbase_dialect::bulk::{BulkLoader, BulkMethod, LoadColumn, LoadSession, LoadTarget};
use oxbase_driver::{
    CancellationToken, Collected, ConnectParams, Connection, Driver, Tx, cell_value, collect,
};
use oxbase_sqlite::SqliteDriver;
use oxbase_types::{DataType, Engine, Error, ObjectRef, ServerVersion, Value};

const SCRATCH_PREFIX: &str = "m5_t_";

async fn connect(path: &Path) -> Box<dyn Connection> {
    match SqliteDriver.connect(&ConnectParams::sqlite(path)).await {
        Ok(c) => c,
        Err(e) => panic!("open {}: {e}", path.display()),
    }
}

async fn run(c: &dyn Connection, sql: &str) -> Collected {
    let result = match c.execute(sql, &[], CancellationToken::new()).await {
        Ok(stream) => collect(stream).await,
        Err(e) => Err(e),
    };
    result.unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Every row of the first result set as `Value`s.
fn rows(out: &Collected) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    if let Some(set) = out.result_sets.first() {
        for batch in &set.batches {
            for row in 0..batch.num_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .map(|c| cell_value(c.as_ref(), row).expect("cell"))
                        .collect(),
                );
            }
        }
    }
    rows
}

/// A fresh database file with the scratch table `columns_sql`; runs `body` with the connection
/// and the table name. The directory is deleted afterwards.
async fn with_table<F, Fut>(case: &str, columns_sql: &str, body: F)
where
    F: FnOnce(Box<dyn Connection>, String) -> Fut,
    Fut: Future<Output = ()>,
{
    let dir = tempfile::tempdir().expect("temp dir");
    let c = connect(&dir.path().join("oxbase_m5.sqlite")).await;
    let name = format!("{SCRATCH_PREFIX}bulk{case}");
    run(
        c.as_ref(),
        &format!("CREATE TABLE \"main\".\"{name}\" ({columns_sql})"),
    )
    .await;
    body(c, name).await;
}

fn qualified(name: &str) -> String {
    format!("\"main\".\"{name}\"")
}

fn table(name: &str) -> ObjectRef {
    ObjectRef::table(Some("main"), None, name)
}

fn loader() -> Arc<dyn BulkLoader> {
    let mut reg = DialectRegistry::new();
    oxbase_sqlite::dialect::register(&mut reg);
    reg.bulk_loader(Engine::Sqlite).expect("bulk loader")
}

// ---- data -----------------------------------------------------------------------------------

const ROWS: i64 = 100_000;
const CALL_ROWS: i64 = 25_000;
const BIG_FACTOR: i64 = 1_000_000_007;
const EPOCH_2000_MICROS: i64 = 946_684_800_000_000;
const MICROS_PER_DAY: i64 = 86_400_000_000;

const FRAGMENTS: [&str; 9] = [
    "plain ascii",
    "comma, \"double\" and 'single' quotes",
    "tab\there",
    "line\nfeed",
    "crlf\r\nend",
    "back\\slash \\N \\x41 \\\\ \\.",
    "naïve café – 日本語 🚀",
    "=SUM(A1) +1 -1 @x",
    "",
];

fn flag(i: i64) -> Option<bool> {
    (i % 10 != 0).then_some(i % 3 == 0)
}

fn qty(i: i64) -> Option<i64> {
    (i % 17 != 0).then(|| (i * 31) % 10_007 - 5_000)
}

fn big(i: i64) -> Option<i64> {
    match i {
        1 => Some(i64::MAX),
        2 => Some(i64::MIN),
        _ if i % 19 == 0 => None,
        _ if i % 2 == 0 => Some(-(i * BIG_FACTOR)),
        _ => Some(i * BIG_FACTOR),
    }
}

/// The amount in units of 0.0001.
fn amount_units(i: i64) -> Option<i128> {
    if i % 13 == 0 {
        return None;
    }
    if i == 3 {
        return Some(-99_999_999_999_999_999_999);
    }
    let units = i128::from((i * 7_907_000_003) % 1_000_000_000_000);
    Some(if i % 11 == 0 { -units } else { units })
}

fn amount(i: i64) -> Option<String> {
    amount_units(i).map(|u| {
        let sign = if u < 0 { "-" } else { "" };
        format!("{sign}{}.{:04}", u.abs() / 10_000, u.abs() % 10_000)
    })
}

fn ratio(i: i64) -> Option<f64> {
    match i {
        _ if i % 23 == 0 => None,
        5 => Some(-2.5e-7),
        _ => Some(f64::from(i32::try_from(i % 1_000).expect("small")) / 7.0),
    }
}

fn note(i: i64) -> Option<String> {
    if i % 11 == 0 {
        return None;
    }
    if i % 997 == 0 {
        return Some(format!("{} #{i}", "L".repeat(4_000)));
    }
    let fragment = FRAGMENTS[usize::try_from(i % 9).expect("small")];
    Some(if fragment.is_empty() {
        String::new()
    } else {
        format!("{fragment} #{i}")
    })
}

fn raw(i: i64) -> Option<Vec<u8>> {
    (i % 7 != 0).then(|| {
        (0..i % 40)
            .map(|k| u8::try_from((i * 31 + k * 7) % 256).expect("byte"))
            .collect()
    })
}

fn born(i: i64) -> Option<i32> {
    (i % 29 != 0).then(|| i32::try_from((i * 37) % 40_000 - 20_000).expect("days"))
}

fn tod(i: i64) -> Option<i64> {
    (i % 31 != 0).then(|| (i * 1_234_567_891) % MICROS_PER_DAY)
}

fn ts(i: i64) -> Option<i64> {
    (i % 37 != 0).then(|| EPOCH_2000_MICROS + i * 7_000_000 + i % 1_000_000)
}

fn tstz(i: i64) -> Option<i64> {
    (i % 41 != 0).then(|| EPOCH_2000_MICROS + i * 11_000_000 + (i * 13) % 1_000_000)
}

/// RFC 4122 version 4 UUIDs (MariaDB's `UUID` type refuses other variants).
fn uid(i: i64) -> Option<[u8; 16]> {
    (i % 43 != 0).then(|| {
        let mut b = u128::try_from(i)
            .expect("positive")
            .wrapping_mul(0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C834)
            .to_be_bytes();
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        b
    })
}

fn doc(i: i64) -> Option<String> {
    (i % 47 != 0).then(|| {
        format!(
            "{{\"i\":{i},\"k\":\"v{}\",\"t\":[{},true,null]}}",
            i % 5,
            i % 3
        )
    })
}

fn uuid_text(b: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(36);
    for (pos, byte) in b.iter().enumerate() {
        if matches!(pos, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn date_text(days: i32) -> String {
    chrono::NaiveDate::from_num_days_from_ce_opt(days + 719_163)
        .expect("date")
        .format("%Y-%m-%d")
        .to_string()
}

/// The load columns of the main table, in table order.
fn columns() -> Vec<LoadColumn> {
    vec![
        LoadColumn::new("id", DataType::INT64),
        LoadColumn::new("flag", DataType::Bool),
        LoadColumn::new("qty", DataType::INT32),
        LoadColumn::new("big", DataType::INT64),
        LoadColumn::new(
            "amount",
            DataType::Decimal {
                precision: Some(20),
                scale: Some(4),
            },
        ),
        LoadColumn::new("ratio", DataType::Float { bits: 64 }),
        LoadColumn::new("note", DataType::Text),
        LoadColumn::new("raw", DataType::VarBinary { length: None }),
        LoadColumn::new("born", DataType::Date),
        LoadColumn::new("tod", DataType::Time { precision: Some(6) }),
        LoadColumn::new("ts", DataType::Timestamp { precision: Some(6) }),
        LoadColumn::new("tstz", DataType::TimestampTz { precision: Some(6) }),
        LoadColumn::new("uid", DataType::Uuid),
        LoadColumn::new("doc", DataType::Json { binary: false }),
    ]
}

/// `amount` is `TEXT`: SQLite's `NUMERIC` affinity would turn exact decimals into doubles.
const MAIN_TABLE_SQL: &str = "id INTEGER PRIMARY KEY, flag BOOLEAN, qty INTEGER, big INTEGER, \
     amount TEXT, ratio REAL, note TEXT, raw BLOB, born DATE, tod TIME, ts TIMESTAMP, \
     tstz TIMESTAMPTZ, uid UUID, doc JSON";

fn array(column: &LoadColumn, ids: &[i64]) -> ArrayRef {
    let it = ids.iter().copied();
    match column.name.as_str() {
        "id" => Arc::new(Int64Array::from_iter_values(it)),
        "flag" => Arc::new(it.map(flag).collect::<BooleanArray>()),
        "qty" => Arc::new(it.map(qty).collect::<Int64Array>()),
        "big" => Arc::new(it.map(big).collect::<Int64Array>()),
        "amount" => Arc::new(it.map(amount).collect::<StringArray>()),
        "ratio" => Arc::new(it.map(ratio).collect::<Float64Array>()),
        "note" => Arc::new(it.map(note).collect::<StringArray>()),
        "raw" => Arc::new(it.map(raw).collect::<BinaryArray>()),
        "born" => Arc::new(it.map(born).collect::<Date32Array>()),
        "tod" => Arc::new(it.map(tod).collect::<Time64MicrosecondArray>()),
        "ts" => Arc::new(it.map(ts).collect::<TimestampMicrosecondArray>()),
        "tstz" => Arc::new(
            it.map(tstz)
                .collect::<TimestampMicrosecondArray>()
                .with_timezone("+00:00"),
        ),
        "uid" if column.data_type == DataType::Uuid => Arc::new(
            FixedSizeBinaryArray::try_from_sparse_iter_with_size(it.map(uid), 16)
                .expect("uuid array"),
        ),
        "uid" => Arc::new(
            it.map(|i| uid(i).map(|b| uuid_text(&b)))
                .collect::<StringArray>(),
        ),
        "doc" => Arc::new(it.map(doc).collect::<StringArray>()),
        other => panic!("no generator for column {other}"),
    }
}

fn batch(target: &LoadTarget<'_>, ids: &[i64]) -> RecordBatch {
    let arrays = target.columns.iter().map(|c| array(c, ids)).collect();
    RecordBatch::try_new(target.schema(), arrays).expect("batch")
}

fn ids(range: std::ops::RangeInclusive<i64>) -> Vec<i64> {
    range.collect()
}

/// The check expressions and what each must return.
fn checks() -> Vec<(&'static str, i128)> {
    let all = || 1..=ROWS;
    let count =
        |f: &dyn Fn(i64) -> bool| i128::try_from(all().filter(|&i| f(i)).count()).expect("n");
    let sum = |f: &dyn Fn(i64) -> i128| all().map(f).sum::<i128>();
    vec![
        ("count(*)", i128::from(ROWS)),
        ("sum(id)", i128::from(ROWS) * i128::from(ROWS + 1) / 2),
        ("count(flag)", count(&|i| flag(i).is_some())),
        ("sum(flag = 1)", count(&|i| flag(i) == Some(true))),
        ("sum(qty)", sum(&|i| i128::from(qty(i).unwrap_or(0)))),
        ("count(big)", count(&|i| big(i).is_some())),
        ("count(amount)", count(&|i| amount(i).is_some())),
        ("count(ratio)", count(&|i| ratio(i).is_some())),
        ("count(note)", count(&|i| note(i).is_some())),
        (
            "sum(length(note))",
            sum(&|i| i128::try_from(note(i).map_or(0, |n| n.chars().count())).expect("n")),
        ),
        ("count(raw)", count(&|i| raw(i).is_some())),
        (
            "sum(length(raw))",
            sum(&|i| i128::try_from(raw(i).map_or(0, |r| r.len())).expect("n")),
        ),
        ("count(born)", count(&|i| born(i).is_some())),
        ("count(tod)", count(&|i| tod(i).is_some())),
        ("count(ts)", count(&|i| ts(i).is_some())),
        ("count(tstz)", count(&|i| tstz(i).is_some())),
        ("count(uid)", count(&|i| uid(i).is_some())),
        ("count(doc)", count(&|i| doc(i).is_some())),
    ]
}

fn number(v: &Value) -> i128 {
    match v {
        Value::Int(i) => i128::from(*i),
        Value::UInt(u) => i128::from(*u),
        Value::Null => 0,
        other => panic!("not an integer: {other:?}"),
    }
}

/// One read-back cell.
#[derive(Debug, PartialEq)]
enum Cell {
    Null,
    Int(i64),
    Text(String),
    Float(f64),
    Bytes(Vec<u8>),
}

fn cell(v: Value) -> Cell {
    match v {
        Value::Null => Cell::Null,
        Value::Int(i) => Cell::Int(i),
        Value::UInt(u) => Cell::Int(i64::try_from(u).expect("fits")),
        Value::Bool(b) => Cell::Int(i64::from(b)),
        Value::Float(x) => Cell::Float(x),
        Value::Text(s) | Value::Decimal(s) => Cell::Text(s),
        Value::Bytes(b) => Cell::Bytes(b),
        other => Cell::Text(format!("{other:?}")),
    }
}

fn opt<T>(v: Option<T>, f: impl FnOnce(T) -> Cell) -> Cell {
    v.map_or(Cell::Null, f)
}

/// The text the driver binds for a `Time64(µs)` value (chrono `%H:%M:%S%.f`).
fn sqlite_time(micros: i64) -> String {
    let secs = u32::try_from(micros / 1_000_000).expect("secs");
    let nanos = u32::try_from(micros % 1_000_000 * 1_000).expect("nanos");
    chrono::NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos)
        .expect("time")
        .format("%H:%M:%S%.f")
        .to_string()
}

fn sqlite_timestamp(micros: i64, zoned: bool) -> String {
    let instant = chrono::DateTime::from_timestamp_micros(micros).expect("timestamp");
    if zoned {
        instant
            .fixed_offset()
            .format("%Y-%m-%d %H:%M:%S%.f%:z")
            .to_string()
    } else {
        instant
            .naive_utc()
            .format("%Y-%m-%d %H:%M:%S%.f")
            .to_string()
    }
}

/// Sample rows: every note fragment and NULL pattern, the extremes and the last rows.
fn sample_ids() -> Vec<i64> {
    let mut out: Vec<i64> = (1..=60).collect();
    out.extend([997, 1_994, 99_999, 100_000]);
    out
}

const SAMPLE_SQL: &str = "SELECT id, CAST(flag AS INTEGER), qty, big, amount, ratio, note, raw, \
     CAST(born AS TEXT), CAST(tod AS TEXT), CAST(ts AS TEXT), CAST(tstz AS TEXT), \
     CAST(uid AS TEXT), CAST(doc AS TEXT)";

fn expected_row(i: i64) -> Vec<Cell> {
    vec![
        Cell::Int(i),
        opt(flag(i), |b| Cell::Int(i64::from(b))),
        opt(qty(i), Cell::Int),
        opt(big(i), Cell::Int),
        opt(amount(i), Cell::Text),
        opt(ratio(i), Cell::Float),
        opt(note(i), Cell::Text),
        opt(raw(i), Cell::Bytes),
        opt(born(i), |d| Cell::Text(date_text(d))),
        opt(tod(i), |t| Cell::Text(sqlite_time(t))),
        opt(ts(i), |t| Cell::Text(sqlite_timestamp(t, false))),
        opt(tstz(i), |t| Cell::Text(sqlite_timestamp(t, true))),
        opt(uid(i), |u| Cell::Text(uuid_text(&u))),
        // Canonical JSON already (compact, keys in order), so the bound text is the same.
        opt(doc(i), Cell::Text),
    ]
}

async fn count_rows(c: &dyn Connection, name: &str) -> Vec<Vec<Value>> {
    rows(&run(c, &format!("SELECT count(*) FROM {}", qualified(name))).await)
}

// ---- tests ----------------------------------------------------------------------------------

#[test]
fn registers_the_insert_loader() {
    let mut reg = DialectRegistry::new();
    oxbase_sqlite::dialect::register(&mut reg);
    assert!(reg.has_bulk_loader(Engine::Sqlite));
    let loader = reg.bulk_loader(Engine::Sqlite).expect("loader");
    let version = ServerVersion::new(3, 50, 4);
    assert_eq!(loader.method(&version), BulkMethod::MultiRowInsert);
    let limits = loader.limits(&version);
    assert_eq!(limits.max_params, 32_766);
    assert_eq!(limits.rows_per_statement(14), 2_340);
}

#[test]
fn generated_data_is_consistent() {
    let cols = columns();
    let t = table("t");
    let version = ServerVersion::new(3, 50, 4);
    let target = LoadTarget {
        table: &t,
        columns: &cols,
        version: &version,
    };
    let b = batch(&target, &ids(1..=100));
    target.check_batch(&b).expect("load schema");
    assert_eq!(sqlite_time(45_296_000_000), "12:34:56");
    assert_eq!(sqlite_time(45_296_123_456), "12:34:56.123456");
    assert_eq!(
        sqlite_timestamp(EPOCH_2000_MICROS + 1_000, true),
        "2000-01-01 00:00:00.001+00:00"
    );
    assert_eq!(checks().len(), 18);
}

#[tokio::test]
#[ignore = "live test (runs in the conformance job)"]
async fn load_100k_rows_of_every_type() {
    with_table("load", MAIN_TABLE_SQL, |c, name| async move {
        let loader = loader();
        let cols = columns();
        let t = table(&name);
        let target = LoadTarget {
            table: &t,
            columns: &cols,
            version: c.server_version(),
        };
        let token = CancellationToken::new();
        loader
            .prepare(LoadSession::new(c.as_ref(), None), &target, token.clone())
            .await
            .expect("prepare");
        let tx: Box<dyn Tx> = c.begin().await.expect("begin");
        let started = Instant::now();
        let mut loaded = 0_u64;
        let mut first = 1_i64;
        while first <= ROWS {
            let last = (first + CALL_ROWS - 1).min(ROWS);
            let b = batch(&target, &ids(first..=last));
            loaded += loader
                .load(
                    LoadSession::new(c.as_ref(), Some(tx.as_ref())),
                    &target,
                    &b,
                    token.clone(),
                )
                .await
                .unwrap_or_else(|e| panic!("load rows {first}..={last}: {e}"));
            first = last + 1;
        }
        tx.commit().await.expect("commit");
        loader
            .finish(LoadSession::new(c.as_ref(), None), &target, token.clone())
            .await
            .expect("finish");
        println!(
            "sqlite multi-row INSERT: {ROWS} rows in {:?}",
            started.elapsed()
        );
        assert_eq!(loaded, u64::try_from(ROWS).expect("rows"));

        let expected = checks();
        let exprs: Vec<&str> = expected.iter().map(|(e, _)| *e).collect();
        let out = run(
            c.as_ref(),
            &format!("SELECT {} FROM {}", exprs.join(", "), qualified(&name)),
        )
        .await;
        let got = rows(&out).into_iter().next().expect("one row");
        for ((expr, want), value) in expected.iter().zip(&got) {
            assert_eq!(number(value), *want, "{expr} = {value:?}");
        }

        let wanted = sample_ids();
        let list: Vec<String> = wanted.iter().map(ToString::to_string).collect();
        let out = run(
            c.as_ref(),
            &format!(
                "{SAMPLE_SQL} FROM {} WHERE id IN ({}) ORDER BY id",
                qualified(&name),
                list.join(", ")
            ),
        )
        .await;
        let got = rows(&out);
        assert_eq!(got.len(), wanted.len());
        for (row, i) in got.into_iter().zip(wanted) {
            let have: Vec<Cell> = row.into_iter().map(cell).collect();
            assert_eq!(have, expected_row(i), "row {i}");
        }
        c.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
#[ignore = "live test (runs in the conformance job)"]
async fn cancel_during_a_large_call_rolls_back() {
    with_table(
        "cancel",
        "id INTEGER PRIMARY KEY, note TEXT",
        |c, name| async move {
            let cols = vec![
                LoadColumn::new("id", DataType::INT64),
                LoadColumn::new("note", DataType::Text),
            ];
            let t = table(&name);
            let target = LoadTarget {
                table: &t,
                columns: &cols,
                version: c.server_version(),
            };
            let loader = loader();
            let tx: Box<dyn Tx> = c.begin().await.expect("begin");
            let session = LoadSession::new(c.as_ref(), Some(tx.as_ref()));
            let first = batch(&target, &ids(1..=50_000));
            let n = loader
                .load(session, &target, &first, CancellationToken::new())
                .await
                .expect("first call");
            assert_eq!(n, 50_000);

            // 300 000 rows in 30 statements: far more than the cancel delay allows.
            let second = batch(&target, &ids(50_001..=350_000));
            let token = CancellationToken::new();
            let load = async {
                let r = loader.load(session, &target, &second, token.clone()).await;
                (r, Instant::now())
            };
            let cancel = async {
                tokio::time::sleep(Duration::from_millis(30)).await;
                token.cancel();
                Instant::now()
            };
            let ((result, returned), cancelled) = tokio::join!(load, cancel);
            assert!(
                matches!(result, Err(Error::Cancelled)),
                "the load must be cancelled mid-call, got {result:?}"
            );
            let latency = returned.saturating_duration_since(cancelled);
            println!("sqlite INSERT cancel latency: {latency:?}");
            assert!(latency < Duration::from_secs(2), "cancel took {latency:?}");

            tx.rollback().await.expect("rollback");
            assert_eq!(
                count_rows(c.as_ref(), &name).await,
                vec![vec![Value::Int(0)]]
            );
            c.close().await.expect("close");
        },
    )
    .await;
}

#[tokio::test]
#[ignore = "live test (runs in the conformance job)"]
async fn duplicate_key_fails_the_call() {
    with_table(
        "dup",
        "id INTEGER PRIMARY KEY, note TEXT",
        |c, name| async move {
            let cols = vec![
                LoadColumn::new("id", DataType::INT64),
                LoadColumn::new("note", DataType::Text),
            ];
            let t = table(&name);
            let target = LoadTarget {
                table: &t,
                columns: &cols,
                version: c.server_version(),
            };
            let data = batch(&target, &[1, 2, 3, 2]);
            let err = loader()
                .load(
                    LoadSession::new(c.as_ref(), None),
                    &target,
                    &data,
                    CancellationToken::new(),
                )
                .await
                .expect_err("duplicate key");
            assert!(
                matches!(err, Error::Sql { .. }),
                "a server error, got {err:?}"
            );
            assert_eq!(
                count_rows(c.as_ref(), &name).await,
                vec![vec![Value::Int(0)]]
            );
            c.close().await.expect("close");
        },
    )
    .await;
}
