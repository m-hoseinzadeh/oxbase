//! Shared helpers of the redis stream's live tests (fixtures, connections, sources, cleanup).
//! Each test file uses part of them.
#![allow(
    dead_code,
    reason = "each live test file uses a different part of these helpers"
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int64Type};
use arrow_array::{Array, RecordBatch};
use oxbase_driver::{
    CancellationToken, Collected, ConnectParams, Connection, ResultSource, collect,
};
use oxbase_redis::RedisConnection;
use oxbase_types::{Engine, TlsMode, TlsOptions};

pub const ROWS: usize = 200_000;

/// The keys of `[m7.redis] keys` (and the other seed names) come from the fixture file.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

pub fn fixtures() -> toml::Table {
    let path = std::env::var("OXBASE_FIXTURES")
        .ok()
        .filter(|p| !p.is_empty())
        .map_or_else(
            || workspace_root().join("tests/fixtures.toml"),
            |p| {
                let p = PathBuf::from(p);
                if p.is_absolute() {
                    p
                } else {
                    workspace_root().join(p)
                }
            },
        );
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.parse().expect("fixtures.toml parses")
}

pub fn lookup<'a>(t: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Value> {
    let (last, parents) = path.split_last()?;
    let mut table = t;
    for key in parents {
        table = table.get(*key)?.as_table()?;
    }
    table.get(*last)
}

pub fn s(t: &toml::Table, path: &[&str]) -> String {
    lookup(t, path)
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("fixtures.toml has no string {}", path.join(".")))
        .to_owned()
}

pub fn int(t: &toml::Table, path: &[&str]) -> i64 {
    lookup(t, path)
        .and_then(toml::Value::as_integer)
        .unwrap_or_else(|| panic!("fixtures.toml has no integer {}", path.join(".")))
}

pub fn port(t: &toml::Table, path: &[&str]) -> u16 {
    u16::try_from(int(t, path)).expect("a port")
}

pub fn strings(t: &toml::Table, path: &[&str]) -> Vec<String> {
    lookup(t, path)
        .and_then(toml::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// 10 base36 characters from the time and the process id.
pub fn token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut n = (nanos ^ (u128::from(std::process::id()) << 64)) % 36u128.pow(10);
    let mut out = Vec::new();
    for _ in 0..10 {
        let d = u8::try_from(n % 36).unwrap();
        out.push(if d < 10 { b'0' + d } else { b'a' + d - 10 });
        n /= 36;
    }
    String::from_utf8(out).unwrap()
}

/// Whether `OXBASE_REDIS_REQUIRE_M7=1` (CI at integration): missing M7 fixtures fail.
pub fn required() -> bool {
    std::env::var("OXBASE_REDIS_REQUIRE_M7").as_deref() == Ok("1")
}

/// Whether a fixture port answers; otherwise the test is pending (or fails when required).
pub fn up_or_pending(what: &str, port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    if std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok() {
        return true;
    }
    assert!(
        !required(),
        "{what} (127.0.0.1:{port}) is not up and OXBASE_REDIS_REQUIRE_M7=1"
    );
    eprintln!("pending: {what} (127.0.0.1:{port}) is not up");
    false
}

/// The fixture's default user on `database`, TLS off.
pub fn params_on(database: &str) -> ConnectParams {
    let f = fixtures();
    ConnectParams::network(Engine::Redis, s(&f, &["engines", "redis", "host"]))
        .with_port(port(&f, &["engines", "redis", "port"]))
        .with_user(s(&f, &["engines", "redis", "user"]))
        .with_password(s(&f, &["engines", "redis", "password"]))
        .with_database(database)
        .with_tls(TlsOptions::new(TlsMode::Disable))
}

pub fn params() -> ConnectParams {
    params_on("0")
}

pub fn with_option(mut p: ConnectParams, key: &str, value: &str) -> ConnectParams {
    p.options.insert(key.to_owned(), value.to_owned());
    p
}

pub async fn connect(p: &ConnectParams) -> RedisConnection {
    RedisConnection::connect(p)
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"))
}

/// Runs one raw command (arguments as given) and returns its reply.
pub async fn raw(c: &RedisConnection, args: &[&[u8]]) -> redis::Value {
    let mut cmd = redis::Cmd::new();
    for a in args {
        cmd.arg(*a);
    }
    c.command(&cmd, &CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("{:?}: {e}", String::from_utf8_lossy(args[0])))
}

/// Whether the M7 seed (`seed-m7.redis`) is loaded; otherwise the test is pending (or fails
/// when required).
pub async fn seeded(c: &RedisConnection) -> bool {
    let f = fixtures();
    let first = strings(&f, &["m7", "redis", "keys"])
        .into_iter()
        .next()
        .expect("[m7.redis] keys");
    if raw(c, &[b"EXISTS", first.as_bytes()]).await == redis::Value::Int(1) {
        return true;
    }
    assert!(
        !required(),
        "the M7 Redis seed ({first}) is missing and OXBASE_REDIS_REQUIRE_M7=1"
    );
    eprintln!("pending: the M7 Redis seed ({first}) is not loaded");
    false
}

/// Deletes every key matching `pattern` (SCAN + UNLINK).
pub async fn cleanup(c: &RedisConnection, pattern: &str) {
    let mut cursor = b"0".to_vec();
    loop {
        let reply = raw(
            c,
            &[
                b"SCAN",
                &cursor,
                b"MATCH",
                pattern.as_bytes(),
                b"COUNT",
                b"1000",
            ],
        )
        .await;
        let redis::Value::Array(mut parts) = reply else {
            panic!("SCAN reply {reply:?}");
        };
        let keys = parts.pop().unwrap();
        let next = parts.pop().unwrap();
        let redis::Value::BulkString(next) = next else {
            panic!("SCAN cursor {next:?}");
        };
        if let redis::Value::Array(keys) = keys {
            for key in keys {
                if let redis::Value::BulkString(key) = key {
                    raw(c, &[b"UNLINK", &key]).await;
                }
            }
        }
        if next == b"0" {
            break;
        }
        cursor = next;
    }
}

/// Opens `source` on `c` and gathers everything.
pub async fn read(source: &dyn ResultSource, c: &dyn Connection) -> Collected {
    let stream = source
        .open(c, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("open: {e}"));
    collect(stream)
        .await
        .unwrap_or_else(|e| panic!("read: {e}"))
}

/// The batches of the first result set.
pub fn batches(out: &Collected) -> &[RecordBatch] {
    &out.result_sets[0].batches
}

/// Rows of the first result set.
pub fn row_count(out: &Collected) -> usize {
    out.result_sets[0].num_rows()
}

fn column<'a>(b: &'a RecordBatch, col: &str) -> &'a dyn Array {
    b.column_by_name(col)
        .unwrap_or_else(|| panic!("no column {col}"))
        .as_ref()
}

/// A `Utf8` column of the first result set (all batches).
pub fn texts(out: &Collected, col: &str) -> Vec<Option<String>> {
    batches(out)
        .iter()
        .flat_map(|b| {
            let a = column(b, col).as_string::<i32>();
            (0..a.len())
                .map(|i| (!a.is_null(i)).then(|| a.value(i).to_owned()))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// A `Binary` column of the first result set (all batches).
pub fn bytes(out: &Collected, col: &str) -> Vec<Vec<u8>> {
    batches(out)
        .iter()
        .flat_map(|b| {
            let a = column(b, col).as_binary::<i32>();
            (0..a.len())
                .map(|i| a.value(i).to_vec())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// An `Int64` column of the first result set (all batches).
pub fn ints(out: &Collected, col: &str) -> Vec<Option<i64>> {
    batches(out)
        .iter()
        .flat_map(|b| {
            let a = column(b, col).as_primitive::<Int64Type>();
            (0..a.len())
                .map(|i| (!a.is_null(i)).then(|| a.value(i)))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// A `Float64` column of the first result set (all batches).
pub fn floats(out: &Collected, col: &str) -> Vec<f64> {
    batches(out)
        .iter()
        .flat_map(|b| {
            let a = column(b, col).as_primitive::<Float64Type>();
            (0..a.len()).map(|i| a.value(i)).collect::<Vec<_>>()
        })
        .collect()
}

/// The column names of the first result set.
pub fn columns(out: &Collected) -> Vec<String> {
    out.result_sets[0]
        .schema
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect()
}

/// `key_text` → (`type`, `ttl_ms`) of a scan result.
pub fn scanned(out: &Collected) -> HashMap<String, (String, Option<i64>)> {
    let keys = texts(out, "key_text");
    let types = texts(out, "type");
    let ttls = ints(out, "ttl_ms");
    let mut map = HashMap::new();
    for ((k, t), ttl) in keys.into_iter().zip(types).zip(ttls) {
        let k = k.unwrap();
        assert!(
            map.insert(k.clone(), (t.unwrap(), ttl)).is_none(),
            "{k} listed twice"
        );
    }
    map
}
