//! Live tests of the SQL Server bulk loader (M5, `docs/m5-contracts.md` §4, §12 *load*) against
//! the `mssql` fixture:
//!
//! - 100 000 rows of every canonical load type (NULLs, `nvarchar` Unicode with astral
//!   characters, text with tabs / CR / LF / backslashes, exact `decimal(20,4)` with mixed
//!   scales in the source text, 64-bit extremes, `varbinary` with NULLs and empty values,
//!   `date`, `time(6)`, `datetime2(6)`, `datetimeoffset(6)` at `+00:00`, `uniqueidentifier`,
//!   JSON text) loaded in four calls inside one transaction into a table whose key is an
//!   `IDENTITY` column: `prepare` turns `IDENTITY_INSERT` on, `finish` off again. Every
//!   statement stays under the 2 100-parameter limit (14 columns: 149 rows). Checked by
//!   server-side counts and sums and an exact read-back of sample rows;
//! - a cancel while a statement waits for a key lock held by another session: `Cancelled`
//!   within 2 s, and the rolled-back transaction leaves no row;
//! - a duplicate key fails the call.
//!
//! Scratch tables are `m5_t_bulk<case>_<token>` in `[m5.mssql]`, dropped also when a test
//! fails; leftovers older than `[m5] stale_after_secs` are dropped first. The live tests are
//! `#[ignore]`d: `cargo test -p oxbase-mssql --test bulk_live -- --include-ignored`.

#![allow(
    clippy::many_single_char_names,
    reason = "the fixture and connections are f, c, a and b throughout"
)]

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arrow_array::{
    ArrayRef, BinaryArray, BooleanArray, Date32Array, FixedSizeBinaryArray, Float64Array,
    Int64Array, RecordBatch, StringArray, Time64MicrosecondArray, TimestampMicrosecondArray,
};
use futures::FutureExt;
use oxbase_dialect::DialectRegistry;
use oxbase_dialect::bulk::{BulkLoader, BulkMethod, LoadColumn, LoadSession, LoadTarget};
use oxbase_driver::{
    CancellationToken, Collected, ConnectParams, Connection, Driver, TlsMode, TlsOptions, Tx,
    cell_value, collect,
};
use oxbase_mssql::MsSqlDriver;
use oxbase_types::{DataType, Engine, Error, ObjectRef, ServerVersion, Value};

// ---- fixture ------------------------------------------------------------------------------------

struct Fixture {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
    schema: String,
    prefix: String,
    stale_after_secs: u64,
}

fn fixture() -> Fixture {
    let path = PathBuf::from(std::env::var("OXBASE_FIXTURES").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures.toml").into()
    }));
    let text = std::fs::read_to_string(&path).expect("read tests/fixtures.toml");
    let doc: toml::Table = text.parse().expect("parse tests/fixtures.toml");
    let server = doc["engines"]["mssql"].as_table().expect("[engines.mssql]");
    let m5 = doc["m5"].as_table().expect("[m5]");
    let m5e = m5["mssql"].as_table().expect("[m5.mssql]");
    let s = |t: &toml::Table, k: &str| t[k].as_str().expect(k).to_owned();
    Fixture {
        host: s(server, "host"),
        port: u16::try_from(server["port"].as_integer().expect("port")).expect("port"),
        user: s(server, "user"),
        password: s(server, "password"),
        database: s(m5e, "database"),
        schema: s(m5e, "schema"),
        prefix: s(m5, "scratch_prefix"),
        stale_after_secs: u64::try_from(m5["stale_after_secs"].as_integer().expect("stale"))
            .expect("stale"),
    }
}

impl Fixture {
    fn params(&self) -> ConnectParams {
        ConnectParams::network(Engine::MsSql, &self.host)
            .with_port(self.port)
            .with_user(&self.user)
            .with_password(self.password.as_str())
            .with_database(&self.database)
            .with_tls(TlsOptions::new(TlsMode::Disable))
    }

    fn table(&self, name: &str) -> ObjectRef {
        ObjectRef::table(Some(&self.database), Some(&self.schema), name)
    }

    fn scratch(&self, case: &str) -> String {
        format!("{}bulk{case}_{}", self.prefix, token())
    }

    fn qualified(&self, name: &str) -> String {
        format!("[{}].[{name}]", self.schema)
    }
}

const BASE36: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

fn base36(mut n: u64, width: usize) -> String {
    let mut out = vec![b'0'; width];
    for slot in out.iter_mut().rev() {
        *slot = BASE36[usize::try_from(n % 36).expect("digit")];
        n /= 36;
    }
    String::from_utf8(out).expect("ascii")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

/// 10 lowercase base36 characters: 6 for the unix seconds, 4 for the process id.
fn token() -> String {
    format!(
        "{}{}",
        base36(now_secs(), 6),
        base36(u64::from(std::process::id()) % 36u64.pow(4), 4)
    )
}

/// The unix seconds of a scratch name's token (its last `_` segment), if it has one.
fn token_secs(name: &str) -> Option<u64> {
    let part = name.rsplit('_').next()?;
    if part.len() != 10 || !part.bytes().all(|b| BASE36.contains(&b)) {
        return None;
    }
    u64::from_str_radix(&part[..6], 36).ok()
}

async fn connect(f: &Fixture) -> Box<dyn Connection> {
    match MsSqlDriver::new().connect(&f.params()).await {
        Ok(c) => c,
        Err(e) => panic!("connect failed: {e}"),
    }
}

async fn run(c: &dyn Connection, sql: &str) -> Collected {
    let result = match c.execute(sql, &[], CancellationToken::new()).await {
        Ok(stream) => collect(stream).await,
        Err(e) => Err(e),
    };
    result.unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn run_tx(tx: &dyn Tx, sql: &str) {
    let result = match tx.execute(sql, &[], CancellationToken::new()).await {
        Ok(stream) => collect(stream).await,
        Err(e) => Err(e),
    };
    result.unwrap_or_else(|e| panic!("{sql}: {e}"));
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

async fn drop_stale(c: &dyn Connection, f: &Fixture) {
    let sql = format!(
        "SELECT CAST(t.name AS nvarchar(128)) FROM sys.tables AS t \
         JOIN sys.schemas AS s ON s.schema_id = t.schema_id \
         WHERE s.name = N'{}' AND t.name LIKE N'{}bulk%'",
        f.schema, f.prefix
    );
    for row in rows(&run(c, &sql).await) {
        let Some(Value::Text(name)) = row.into_iter().next() else {
            continue;
        };
        if token_secs(&name).is_some_and(|s| now_secs().saturating_sub(s) > f.stale_after_secs) {
            run(c, &format!("DROP TABLE IF EXISTS {}", f.qualified(&name))).await;
        }
    }
}

/// Creates the scratch table with `columns_sql`, runs `body`, and drops the table whatever
/// happened.
async fn with_table<F, Fut>(f: &Fixture, case: &str, columns_sql: &str, body: F)
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = ()>,
{
    let c = connect(f).await;
    drop_stale(c.as_ref(), f).await;
    let name = f.scratch(case);
    run(
        c.as_ref(),
        &format!("CREATE TABLE {} ({columns_sql})", f.qualified(&name)),
    )
    .await;
    let outcome = AssertUnwindSafe(body(name.clone())).catch_unwind().await;
    run(
        c.as_ref(),
        &format!("DROP TABLE IF EXISTS {}", f.qualified(&name)),
    )
    .await;
    c.close().await.expect("close");
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

fn loader() -> Arc<dyn BulkLoader> {
    let mut reg = DialectRegistry::new();
    oxbase_mssql::dialect::register(&mut reg);
    reg.bulk_loader(Engine::MsSql).expect("bulk loader")
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

fn clock_text(micros: i64) -> String {
    let secs = micros / 1_000_000;
    format!(
        "{:02}:{:02}:{:02}.{:06}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        micros % 1_000_000
    )
}

fn ts_text(micros: i64) -> String {
    let days = i32::try_from(micros.div_euclid(MICROS_PER_DAY)).expect("days");
    format!(
        "{} {}",
        date_text(days),
        clock_text(micros.rem_euclid(MICROS_PER_DAY))
    )
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

const MAIN_TABLE_SQL: &str = "id bigint IDENTITY(1,1) PRIMARY KEY, flag bit, qty int, \
     big bigint, amount decimal(20,4), ratio float, note nvarchar(max), raw varbinary(max), \
     born date, tod time(6), ts datetime2(6), tstz datetimeoffset(6), uid uniqueidentifier, \
     doc nvarchar(max)";

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

/// `text` (`-12.3400`) in units of 10^-`scale`; digits beyond `scale` must be zeros.
fn scaled(text: &str, scale: usize) -> i128 {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let mut frac = frac.to_owned();
    while frac.len() > scale {
        assert!(frac.ends_with('0'), "{text} has more than {scale} decimals");
        frac.pop();
    }
    while frac.len() < scale {
        frac.push('0');
    }
    let value: i128 = format!("{int}{frac}").parse().expect("number");
    if negative { -value } else { value }
}

fn number(v: &Value, scale: usize) -> i128 {
    match v {
        Value::Int(i) => i128::from(*i) * 10_i128.pow(u32::try_from(scale).expect("scale")),
        Value::UInt(u) => i128::from(*u) * 10_i128.pow(u32::try_from(scale).expect("scale")),
        Value::Decimal(s) | Value::Text(s) => scaled(s, scale),
        Value::Null => 0,
        other => panic!("not a number: {other:?}"),
    }
}

/// The server-side check expressions and what each must return (the second item is the
/// scale of a decimal result, 0 for integers).
fn checks() -> Vec<(&'static str, usize, i128)> {
    let all = || 1..=ROWS;
    let count =
        |f: &dyn Fn(i64) -> bool| i128::try_from(all().filter(|&i| f(i)).count()).expect("n");
    let sum = |f: &dyn Fn(i64) -> i128| all().map(f).sum::<i128>();
    vec![
        ("COUNT_BIG(*)", 0, i128::from(ROWS)),
        ("SUM(id)", 0, i128::from(ROWS) * i128::from(ROWS + 1) / 2),
        ("COUNT_BIG(flag)", 0, count(&|i| flag(i).is_some())),
        (
            "SUM(CASE WHEN flag = 1 THEN 1 ELSE 0 END)",
            0,
            count(&|i| flag(i) == Some(true)),
        ),
        (
            "SUM(CAST(qty AS bigint))",
            0,
            sum(&|i| i128::from(qty(i).unwrap_or(0))),
        ),
        ("COUNT_BIG(big)", 0, count(&|i| big(i).is_some())),
        ("COUNT_BIG(amount)", 0, count(&|i| amount(i).is_some())),
        (
            "CAST(SUM(amount) AS varchar(50))",
            4,
            sum(&|i| amount_units(i).unwrap_or(0)),
        ),
        ("COUNT_BIG(ratio)", 0, count(&|i| ratio(i).is_some())),
        ("COUNT_BIG(note)", 0, count(&|i| note(i).is_some())),
        (
            "SUM(CAST(DATALENGTH(note) AS bigint)) / 2",
            0,
            sum(&|i| i128::try_from(note(i).map_or(0, |n| n.encode_utf16().count())).expect("n")),
        ),
        ("COUNT_BIG(raw)", 0, count(&|i| raw(i).is_some())),
        (
            "SUM(CAST(DATALENGTH(raw) AS bigint))",
            0,
            sum(&|i| i128::try_from(raw(i).map_or(0, |r| r.len())).expect("n")),
        ),
        ("COUNT_BIG(born)", 0, count(&|i| born(i).is_some())),
        ("COUNT_BIG(tod)", 0, count(&|i| tod(i).is_some())),
        ("COUNT_BIG(ts)", 0, count(&|i| ts(i).is_some())),
        ("COUNT_BIG(tstz)", 0, count(&|i| tstz(i).is_some())),
        ("COUNT_BIG(uid)", 0, count(&|i| uid(i).is_some())),
        ("COUNT_BIG(doc)", 0, count(&|i| doc(i).is_some())),
    ]
}

/// One read-back cell, compared loosely where engines differ in form only.
#[derive(Debug, PartialEq)]
enum Cell {
    Null,
    Int(i64),
    Text(String),
    Float(f64),
    Bytes(Vec<u8>),
    Json(serde_json::Value),
}

fn cell(v: Value, like: &Cell) -> Cell {
    match (v, like) {
        (Value::Null, _) => Cell::Null,
        (Value::Text(s), Cell::Json(_)) => {
            Cell::Json(serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s)))
        }
        (Value::Json(j), _) => Cell::Json(j),
        (Value::Int(i), _) => Cell::Int(i),
        (Value::UInt(u), _) => Cell::Int(i64::try_from(u).expect("fits")),
        (Value::Bool(b), _) => Cell::Int(i64::from(b)),
        (Value::Float(x), _) => Cell::Float(x),
        (Value::Text(s) | Value::Decimal(s), _) => Cell::Text(s),
        (Value::Bytes(b), _) => Cell::Bytes(b),
        (other, _) => Cell::Text(format!("{other:?}")),
    }
}

fn opt<T>(v: Option<T>, f: impl FnOnce(T) -> Cell) -> Cell {
    v.map_or(Cell::Null, f)
}

/// Sample rows: every note fragment and NULL pattern, the extremes and the last rows.
fn sample_ids() -> Vec<i64> {
    let mut out: Vec<i64> = (1..=60).collect();
    out.extend([997, 1_994, 99_999, 100_000]);
    out
}

const SAMPLE_SQL: &str = "SELECT id, CAST(flag AS int), qty, big, \
     CAST(amount AS varchar(40)), ratio, note, raw, CONVERT(varchar(10), born, 23), \
     CAST(tod AS varchar(16)), CONVERT(varchar(26), ts, 121), \
     CONVERT(varchar(26), CAST(SWITCHOFFSET(tstz, '+00:00') AS datetime2(6)), 121), \
     LOWER(CAST(uid AS char(36))), doc";

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
        opt(tod(i), |t| Cell::Text(clock_text(t))),
        opt(ts(i), |t| Cell::Text(ts_text(t))),
        opt(tstz(i), |t| Cell::Text(ts_text(t))),
        opt(uid(i), |u| Cell::Text(uuid_text(&u))),
        opt(doc(i), |d| {
            Cell::Json(serde_json::from_str(&d).expect("generated JSON"))
        }),
    ]
}

// ---- tests ----------------------------------------------------------------------------------

#[test]
fn registers_the_insert_loader() {
    let mut reg = DialectRegistry::new();
    oxbase_mssql::dialect::register(&mut reg);
    assert!(reg.has_bulk_loader(Engine::MsSql));
    let loader = reg.bulk_loader(Engine::MsSql).expect("loader");
    let version = ServerVersion::new(17, 0, 1000);
    assert_eq!(loader.method(&version), BulkMethod::MultiRowInsert);
    let limits = loader.limits(&version);
    assert_eq!(limits.max_params, 2_098);
    assert_eq!(limits.rows_per_statement(14), 149);
    assert_eq!(limits.rows_per_statement(2), 1_000);
}

#[test]
fn generated_data_is_consistent() {
    let cols = columns();
    let table = ObjectRef::table(None, Some("dbo"), "t");
    let version = ServerVersion::new(17, 0, 1000);
    let target = LoadTarget {
        table: &table,
        columns: &cols,
        version: &version,
    };
    let b = batch(&target, &ids(1..=100));
    target.check_batch(&b).expect("load schema");
    assert_eq!(amount(3).as_deref(), Some("-9999999999999999.9999"));
    assert_eq!(checks().len(), 19);
}

#[tokio::test]
#[ignore = "needs the mssql fixture (scripts/fixtures-up.sh mssql)"]
async fn load_100k_rows_of_every_type() {
    let fx = fixture();
    let f = &fx;
    with_table(f, "load", MAIN_TABLE_SQL, |name| async move {
        let c = connect(f).await;
        let loader = loader();
        let cols = columns();
        let table = f.table(&name);
        let target = LoadTarget {
            table: &table,
            columns: &cols,
            version: c.server_version(),
        };
        let token = CancellationToken::new();
        // `id` is the identity column: prepare turns IDENTITY_INSERT on.
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
            "mssql multi-row INSERT: {ROWS} rows in {:?}",
            started.elapsed()
        );
        assert_eq!(loaded, u64::try_from(ROWS).expect("rows"));

        // IDENTITY_INSERT is off again: a row without an explicit id gets the next identity.
        run(
            c.as_ref(),
            &format!(
                "INSERT INTO {} (note) VALUES (N'after finish')",
                f.qualified(&name)
            ),
        )
        .await;
        let out = run(
            c.as_ref(),
            &format!(
                "SELECT id FROM {} WHERE note = N'after finish'",
                f.qualified(&name)
            ),
        )
        .await;
        assert_eq!(rows(&out), vec![vec![Value::Int(ROWS + 1)]]);
        run(
            c.as_ref(),
            &format!("DELETE FROM {} WHERE id > {ROWS}", f.qualified(&name)),
        )
        .await;

        let expected = checks();
        let exprs: Vec<&str> = expected.iter().map(|(e, _, _)| *e).collect();
        let out = run(
            c.as_ref(),
            &format!("SELECT {} FROM {}", exprs.join(", "), f.qualified(&name)),
        )
        .await;
        let got = rows(&out).into_iter().next().expect("one row");
        for ((expr, scale, want), value) in expected.iter().zip(&got) {
            assert_eq!(number(value, *scale), *want, "{expr} = {value:?}");
        }

        let wanted = sample_ids();
        let list: Vec<String> = wanted.iter().map(ToString::to_string).collect();
        let out = run(
            c.as_ref(),
            &format!(
                "{SAMPLE_SQL} FROM {} WHERE id IN ({}) ORDER BY id",
                f.qualified(&name),
                list.join(", ")
            ),
        )
        .await;
        let got = rows(&out);
        assert_eq!(got.len(), wanted.len());
        for (row, i) in got.into_iter().zip(wanted) {
            let want = expected_row(i);
            let have: Vec<Cell> = row
                .into_iter()
                .zip(&want)
                .map(|(v, like)| cell(v, like))
                .collect();
            assert_eq!(have, want, "row {i}");
        }
        c.close().await.expect("close");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the mssql fixture (scripts/fixtures-up.sh mssql)"]
async fn cancel_while_a_statement_waits_rolls_back() {
    let fx = fixture();
    let f = &fx;
    with_table(
        f,
        "cancel",
        "id bigint PRIMARY KEY, note nvarchar(max)",
        |name| async move {
            let a = connect(f).await;
            let b = connect(f).await;
            let cols = vec![
                LoadColumn::new("id", DataType::INT64),
                LoadColumn::new("note", DataType::Text),
            ];
            let table = f.table(&name);
            let target = LoadTarget {
                table: &table,
                columns: &cols,
                version: a.server_version(),
            };
            let loader = loader();
            // Session B holds an uncommitted row with a key A's second call also inserts.
            let blocker: Box<dyn Tx> = b.begin().await.expect("begin b");
            run_tx(
                blocker.as_ref(),
                &format!(
                    "INSERT INTO {} (id, note) VALUES (75000, N'blocker')",
                    f.qualified(&name)
                ),
            )
            .await;

            let tx: Box<dyn Tx> = a.begin().await.expect("begin a");
            let session = LoadSession::new(a.as_ref(), Some(tx.as_ref()));
            let first = batch(&target, &ids(1..=50_000));
            let n = loader
                .load(session, &target, &first, CancellationToken::new())
                .await
                .expect("first call");
            assert_eq!(n, 50_000);

            let second = batch(&target, &ids(50_001..=100_000));
            let token = CancellationToken::new();
            let load = async {
                let r = loader.load(session, &target, &second, token.clone()).await;
                (r, Instant::now())
            };
            let cancel = async {
                tokio::time::sleep(Duration::from_millis(1_500)).await;
                token.cancel();
                Instant::now()
            };
            let ((result, returned), cancelled) = tokio::join!(load, cancel);
            assert!(
                matches!(result, Err(Error::Cancelled)),
                "the blocked load must be cancelled, got {result:?}"
            );
            let latency = returned.saturating_duration_since(cancelled);
            println!("mssql INSERT cancel latency: {latency:?}");
            assert!(latency < Duration::from_secs(2), "cancel took {latency:?}");

            tx.rollback().await.expect("rollback a");
            blocker.rollback().await.expect("rollback b");
            let out = run(
                a.as_ref(),
                &format!("SELECT COUNT_BIG(*) FROM {}", f.qualified(&name)),
            )
            .await;
            assert_eq!(rows(&out), vec![vec![Value::Int(0)]]);
            a.close().await.expect("close a");
            b.close().await.expect("close b");
        },
    )
    .await;
}

#[tokio::test]
#[ignore = "needs the mssql fixture (scripts/fixtures-up.sh mssql)"]
async fn duplicate_key_fails_the_call() {
    let fx = fixture();
    let f = &fx;
    with_table(
        f,
        "dup",
        "id bigint PRIMARY KEY, note nvarchar(max)",
        |name| async move {
            let c = connect(f).await;
            let cols = vec![
                LoadColumn::new("id", DataType::INT64),
                LoadColumn::new("note", DataType::Text),
            ];
            let table = f.table(&name);
            let target = LoadTarget {
                table: &table,
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
            let out = run(
                c.as_ref(),
                &format!("SELECT COUNT_BIG(*) FROM {}", f.qualified(&name)),
            )
            .await;
            assert_eq!(rows(&out), vec![vec![Value::Int(0)]]);
            c.close().await.expect("close");
        },
    )
    .await;
}
