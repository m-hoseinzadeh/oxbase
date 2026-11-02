//! Live tests of the Redis driver against the `redis` fixture (docs/m7-contracts.md §11,
//! redis-driver), the SSH bastion and, when they run, the M7 TLS port, cluster and sentinel
//! fixtures.
//!
//! All are `#[ignore]`d: the conformance CI job runs them with
//! `cargo test -p oxbase-redis -- --include-ignored`. Connection parameters come from
//! `tests/fixtures.toml` (`$OXBASE_FIXTURES`). The tests never change seed keys: they write their
//! own keys named `oxbase:driver:t<token>:*` and delete them at the end. Fixtures that are not up
//! (TLS port, cluster, sentinel) make their test print "pending" and pass, unless
//! `OXBASE_REDIS_REQUIRE_M7=1`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int64Type};
use arrow_array::{Array, RecordBatch};
use oxbase_driver::{
    CancellationToken, Collected, ConnectParams, Connection, Driver, NoticeSeverity, collect,
};
use oxbase_net::ssh::{RusshTunnelProvider, TunnelHandle, TunnelProvider};
use oxbase_redis::{RedisConnection, RedisDriver, options};
use oxbase_types::{
    Endpoint, Engine, Error, KeySource, KnownHostsPolicy, PemSource, SshAuth, SshOptions, TlsMode,
    TlsOptions, Value,
};

const CANCEL_LIMIT: Duration = Duration::from_secs(2);
const ROWS: usize = 200_000;

// ---- fixtures ---------------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn in_workspace(p: &str) -> PathBuf {
    let p = PathBuf::from(p);
    if p.is_absolute() {
        p
    } else {
        workspace_root().join(p)
    }
}

fn fixtures() -> toml::Table {
    let path = std::env::var("OXBASE_FIXTURES")
        .ok()
        .filter(|p| !p.is_empty())
        .map_or_else(
            || workspace_root().join("tests/fixtures.toml"),
            |p| in_workspace(&p),
        );
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.parse().expect("fixtures.toml parses")
}

fn lookup<'a>(t: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Value> {
    let (last, parents) = path.split_last()?;
    let mut table = t;
    for key in parents {
        table = table.get(*key)?.as_table()?;
    }
    table.get(*last)
}

fn s(t: &toml::Table, path: &[&str]) -> String {
    lookup(t, path)
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("fixtures.toml has no string {}", path.join(".")))
        .to_owned()
}

fn port(t: &toml::Table, path: &[&str]) -> u16 {
    lookup(t, path)
        .and_then(toml::Value::as_integer)
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or_else(|| panic!("fixtures.toml has no port {}", path.join(".")))
}

fn strings(t: &toml::Table, path: &[&str]) -> Vec<String> {
    let Some(items) = lookup(t, path).and_then(toml::Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

/// 10 base36 characters from the time and the process id.
fn token() -> String {
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

fn key(token: &str, name: &str) -> String {
    format!("oxbase:driver:t{token}:{name}")
}

/// Whether a fixture port answers; otherwise the test is pending (or fails when M7 fixtures are
/// required).
fn up_or_pending(what: &str, port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    if std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok() {
        return true;
    }
    assert!(
        std::env::var("OXBASE_REDIS_REQUIRE_M7").as_deref() != Ok("1"),
        "{what} (127.0.0.1:{port}) is not up and OXBASE_REDIS_REQUIRE_M7=1"
    );
    eprintln!("pending: {what} (127.0.0.1:{port}) is not up");
    false
}

/// The fixture's default user with its password on database 0, TLS off.
fn params() -> ConnectParams {
    let f = fixtures();
    ConnectParams::network(Engine::Redis, s(&f, &["engines", "redis", "host"]))
        .with_port(port(&f, &["engines", "redis", "port"]))
        .with_user(s(&f, &["engines", "redis", "user"]))
        .with_password(s(&f, &["engines", "redis", "password"]))
        .with_database(s(&f, &["engines", "redis", "database"]))
        .with_tls(TlsOptions::new(TlsMode::Disable))
}

fn with_option(mut p: ConnectParams, key: &str, value: &str) -> ConnectParams {
    p.options.insert(key.to_owned(), value.to_owned());
    p
}

fn ca_cert() -> PemSource {
    let f = fixtures();
    let ca = in_workspace(&s(&f, &["tls", "dir"])).join(s(&f, &["tls", "ca_cert"]));
    assert!(ca.exists(), "fixture CA {} is missing", ca.display());
    PemSource::File(ca)
}

fn verify(mode: TlsMode, name: &str) -> TlsOptions {
    TlsOptions::new(mode)
        .with_ca_cert(ca_cert())
        .with_server_name(name)
}

async fn connect(p: &ConnectParams) -> RedisConnection {
    RedisConnection::connect(p)
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"))
}

async fn try_run(
    c: &dyn Connection,
    line: &str,
    params: &[Value],
) -> oxbase_types::Result<Collected> {
    collect(c.execute(line, params, CancellationToken::new()).await?).await
}

async fn run(c: &dyn Connection, line: &str) -> Collected {
    try_run(c, line, &[])
        .await
        .unwrap_or_else(|e| panic!("{line}: {e}"))
}

async fn run_with(c: &dyn Connection, words: &str, params: &[Value]) -> Collected {
    try_run(c, words, params)
        .await
        .unwrap_or_else(|e| panic!("{words}: {e}"))
}

fn batch(out: &Collected) -> RecordBatch {
    let set = &out.result_sets[0];
    arrow_select_concat(set)
}

/// All batches of the first result set as one (small results only).
fn arrow_select_concat(set: &oxbase_driver::ResultSet) -> RecordBatch {
    let schema = set.schema.clone();
    if set.batches.is_empty() {
        return RecordBatch::new_empty(schema);
    }
    assert_eq!(set.batches.len(), 1, "small results come in one batch");
    set.batches[0].clone()
}

fn names(b: &RecordBatch) -> Vec<String> {
    b.schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect()
}

fn texts(b: &RecordBatch, col: &str) -> Vec<Option<String>> {
    let a = b
        .column_by_name(col)
        .unwrap_or_else(|| panic!("no column {col}"))
        .as_string::<i32>();
    (0..a.len())
        .map(|i| a.is_valid(i).then(|| a.value(i).to_owned()))
        .collect()
}

fn binaries(b: &RecordBatch, col: &str) -> Vec<Option<Vec<u8>>> {
    let a = b
        .column_by_name(col)
        .unwrap_or_else(|| panic!("no column {col}"))
        .as_binary::<i32>();
    (0..a.len())
        .map(|i| a.is_valid(i).then(|| a.value(i).to_vec()))
        .collect()
}

/// The `value` text of a scalar reply.
async fn scalar(c: &dyn Connection, line: &str) -> Option<String> {
    let out = run(c, line).await;
    texts(&batch(&out), "value").remove(0)
}

async fn ping(c: &dyn Connection) {
    assert_eq!(scalar(c, "PING").await.as_deref(), Some("PONG"));
}

/// The value of `key` in the reply of `HELLO` (no arguments).
async fn hello(c: &dyn Connection, key: &str) -> Option<String> {
    let b = batch(&run(c, "HELLO").await);
    let keys = texts(&b, "key");
    let values = texts(&b, "value");
    keys.iter()
        .position(|k| k.as_deref() == Some(key))
        .and_then(|i| values[i].clone())
}

async fn cleanup(c: &dyn Connection, token: &str) {
    let pattern = format!("oxbase:driver:t{token}:*");
    let mut cursor = "0".to_owned();
    loop {
        let out = run_with(
            c,
            "SCAN",
            &[
                Value::Text(cursor.clone()),
                Value::Text("MATCH".into()),
                Value::Text(pattern.clone()),
                Value::Text("COUNT".into()),
                Value::Int(1000),
            ],
        )
        .await;
        let b = batch(&out);
        let values = texts(&b, "value");
        cursor = values[0].clone().unwrap_or_default();
        let keys: Vec<String> = serde_json::from_str(values[1].as_deref().unwrap_or("[]")).unwrap();
        if !keys.is_empty() {
            let params: Vec<Value> = keys.into_iter().map(Value::Text).collect();
            let _ = try_run(c, "UNLINK", &params).await;
        }
        if cursor == "0" {
            break;
        }
    }
}

// ---- SSH ----------------------------------------------------------------------------------------

/// A tunnel through the fixture bastion (key auth, host key pinned) to `[ssh.targets] target`.
async fn bastion_tunnel(target: &str) -> (TunnelHandle, SshOptions, Endpoint) {
    let f = fixtures();
    let key_dir = in_workspace(&s(&f, &["ssh", "key_dir"]));
    let known_hosts = key_dir.join(s(&f, &["ssh", "known_hosts"]));
    assert!(
        known_hosts.exists(),
        "fixture SSH files missing under {}: start the fixtures (scripts/fixtures-up.sh)",
        key_dir.display()
    );
    let ssh = SshOptions::new(
        s(&f, &["ssh", "host"]),
        s(&f, &["ssh", "user"]),
        SshAuth::PrivateKey {
            key: KeySource::File(key_dir.join(s(&f, &["ssh", "client_key"]))),
            passphrase: None,
        },
    )
    .with_port(port(&f, &["ssh", "port"]))
    .with_known_hosts(KnownHostsPolicy::Strict {
        path: Some(known_hosts),
    });
    let server = Endpoint::new(
        s(&f, &["ssh", "targets", target, "host"]),
        port(&f, &["ssh", "targets", target, "port"]),
    );
    let tunnel = RusshTunnelProvider::new()
        .open(&ssh, &server, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("SSH tunnel to {server}: {e}"));
    (tunnel, ssh, server)
}

fn through(
    p: ConnectParams,
    tunnel: &TunnelHandle,
    ssh: &SshOptions,
    server: &Endpoint,
) -> ConnectParams {
    let mut p = p.with_ssh(ssh.clone()).with_dial_addr(tunnel.local_addr());
    p.host = Some(server.host.clone());
    p.port = Some(server.port);
    p
}

// ---- cancel -------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum CancelBy {
    Token,
    CancelRunning,
}

/// Runs `line` (a command that blocks for a long time), cancels it after 300 ms and checks that
/// it ends with `Cancelled` and the session answers `PING` within [`CANCEL_LIMIT`].
async fn cancel_once(c: &Arc<RedisConnection>, line: &str, by: CancelBy) {
    let token = CancellationToken::new();
    let task = {
        let c = c.clone();
        let token = token.clone();
        let line = line.to_owned();
        tokio::spawn(async move {
            let stream = c.execute(&line, &[], token).await?;
            collect(stream).await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!task.is_finished(), "{line} finished before the cancel");
    let requested = Instant::now();
    match by {
        CancelBy::Token => token.cancel(),
        CancelBy::CancelRunning => c.cancel_running().await.unwrap(),
    }
    let outcome = tokio::time::timeout(CANCEL_LIMIT * 5, task)
        .await
        .unwrap_or_else(|_| panic!("{by:?}: {line} still running 10 s after the cancel"))
        .unwrap();
    let took = requested.elapsed();
    let err = outcome.expect_err("the command was cancelled");
    assert!(err.is_cancelled(), "{by:?}: expected Cancelled, got {err}");
    assert!(took <= CANCEL_LIMIT, "{by:?}: cancelled after {took:?}");
    ping(c.as_ref()).await;
    let free = requested.elapsed();
    assert!(
        free <= CANCEL_LIMIT,
        "{by:?}: the session answered after {free:?}"
    );
}

// ---- tests --------------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn plain_connect_reports_session_facts() {
    let f = fixtures();
    let c = connect(&params()).await;
    assert_eq!(c.engine(), Engine::Redis);
    assert!(!c.tls_in_use());
    assert_eq!(c.current_database().as_deref(), Some("0"));
    let prefix = s(&f, &["engines", "redis", "version_prefix"]);
    assert!(
        c.server_version().raw().starts_with(&prefix),
        "{} does not start with {prefix}",
        c.server_version().raw()
    );
    assert_eq!(c.protocol(), redis::ProtocolVersion::RESP3);
    let debug = format!("{c:?}");
    assert!(
        !debug.contains(&s(&f, &["engines", "redis", "password"])),
        "{debug}"
    );
    ping(&c).await;
    c.close().await.unwrap();
    c.close().await.unwrap();
    let err = c
        .execute("PING", &[], CancellationToken::new())
        .await
        .err()
        .expect("closed");
    assert!(matches!(err, Error::Connection { .. }), "{err}");

    // Through the Driver trait and the registry the app uses.
    let boxed = RedisDriver::new().connect(&params()).await.unwrap();
    ping(boxed.as_ref()).await;
    let any = boxed.as_any().unwrap();
    assert!(any.downcast_ref::<RedisConnection>().is_some());
    boxed.close().await.unwrap();

    // Prefer on the plaintext port stays plaintext; a wrong password is Auth and leaks nothing.
    let prefer = connect(&params().with_tls(TlsOptions::new(TlsMode::Prefer))).await;
    assert!(!prefer.tls_in_use());
    ping(&prefer).await;
    let err = RedisConnection::connect(&params().with_password("Wrong_Marker_Pw_77"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Auth { .. }), "{err}");
    assert!(!err.to_string().contains("Wrong_Marker_Pw_77"), "{err}");
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn resp2_resp3_and_auto() {
    for (protocol, want, proto) in [
        ("resp2", redis::ProtocolVersion::RESP2, "2"),
        ("resp3", redis::ProtocolVersion::RESP3, "3"),
        ("auto", redis::ProtocolVersion::RESP3, "3"),
    ] {
        let c = connect(&with_option(params(), options::PROTOCOL, protocol)).await;
        assert_eq!(c.protocol(), want, "{protocol}");
        assert_eq!(
            hello(&c, "proto").await.as_deref(),
            Some(proto),
            "{protocol}"
        );
        assert!(hello(&c, "id").await.is_some());
        ping(&c).await;
        c.close().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn database_index_and_console_select() {
    let t = token();
    let k = key(&t, "db");
    let one = connect(&params().with_database("1")).await;
    assert_eq!(one.current_database().as_deref(), Some("1"));
    run_with(
        &one,
        "SET",
        &[Value::Text(k.clone()), Value::Text("in-db-1".into())],
    )
    .await;
    let zero = connect(&params()).await;
    assert_eq!(
        scalar(&zero, &format!("EXISTS {k}")).await.as_deref(),
        Some("0")
    );
    assert_eq!(
        scalar(&one, &format!("GET {k}")).await.as_deref(),
        Some("in-db-1")
    );

    // A console SELECT moves the session; the connection tracks it.
    assert_eq!(scalar(&zero, "SELECT 1").await.as_deref(), Some("OK"));
    assert_eq!(zero.current_database().as_deref(), Some("1"));
    assert_eq!(
        scalar(&zero, &format!("GET {k}")).await.as_deref(),
        Some("in-db-1")
    );
    let err = try_run(&zero, "SELECT 9999", &[]).await.unwrap_err();
    assert!(matches!(err, Error::Sql { .. }), "{err}");
    assert_eq!(zero.current_database().as_deref(), Some("1"));
    cleanup(&one, &t).await;
    one.close().await.unwrap();
    zero.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn acl_users_connect_and_wrong_passwords_are_auth_errors() {
    let f = fixtures();
    let password = s(&f, &["engines", "redis", "password"]);
    let admin = connect(&params()).await;
    let t = token();
    let user = format!("oxbase_t{t}");
    // Same rules as the M7 seed user m7_reader (`on ~m7:* +@read`), plus this test's keys.
    run_with(
        &admin,
        "ACL SETUSER",
        &[
            Value::Text(user.clone()),
            Value::Text("on".into()),
            Value::Text(format!(">{password}")),
            Value::Text("~m7:*".into()),
            Value::Text(format!("~oxbase:driver:t{t}:*")),
            Value::Text("+@read".into()),
        ],
    )
    .await;
    let k = key(&t, "acl");
    run_with(
        &admin,
        "SET",
        &[Value::Text(k.clone()), Value::Text("v".into())],
    )
    .await;
    for protocol in ["resp2", "resp3"] {
        let p = with_option(
            params().with_user(user.clone()),
            options::PROTOCOL,
            protocol,
        );
        let reader = connect(&p).await;
        // HELLO is allowed for every user, so the version is known.
        assert!(
            reader.server_version().major >= 6,
            "{}",
            reader.server_version().raw()
        );
        assert_eq!(
            scalar(&reader, &format!("GET {k}")).await.as_deref(),
            Some("v")
        );
        let err = try_run(&reader, &format!("SET {k} w"), &[])
            .await
            .unwrap_err();
        assert_eq!(err.sql_code(), Some("NOPERM"), "{err}");
        reader.close().await.unwrap();

        let wrong = with_option(
            params()
                .with_user(user.clone())
                .with_password("Wrong_Marker_Pw_88"),
            options::PROTOCOL,
            protocol,
        );
        let err = RedisConnection::connect(&wrong).await.unwrap_err();
        assert!(matches!(err, Error::Auth { .. }), "{protocol}: {err}");
        assert!(!err.to_string().contains("Wrong_Marker_Pw_88"), "{err}");
    }
    let _ = try_run(&admin, "ACL DELUSER", &[Value::Text(user)]).await;
    cleanup(&admin, &t).await;
    admin.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn replies_render_per_resp_type_and_command() {
    for protocol in ["resp2", "resp3"] {
        let c = connect(&with_option(params(), options::PROTOCOL, protocol)).await;
        let t = token();
        let (string, binary, hash, list, set, zset, stream) = (
            key(&t, "string"),
            key(&t, "binary"),
            key(&t, "hash"),
            key(&t, "list"),
            key(&t, "set"),
            key(&t, "zset"),
            key(&t, "stream"),
        );
        assert_eq!(
            scalar(&c, &format!("SET {string} hello")).await.as_deref(),
            Some("OK")
        );
        // Binary-safe values through bound parameters and through \xHH in a console line.
        let bytes = vec![0u8, 0xff, 0xfe, b'\n'];
        run_with(
            &c,
            "SET",
            &[Value::Text(binary.clone()), Value::Bytes(bytes.clone())],
        )
        .await;
        let out = run(&c, &format!("GET {binary}")).await;
        let b = batch(&out);
        assert_eq!(names(&b), ["type", "value", "raw"]);
        assert_eq!(binaries(&b, "raw"), [Some(bytes.clone())]);
        assert_eq!(texts(&b, "value"), [Some("\\x00\\xff\\xfe\\n".to_owned())]);
        run(&c, &format!(r#"SET {binary} "\x00\xff\xfe\n""#)).await;
        assert_eq!(
            binaries(&batch(&run(&c, &format!("GET {binary}")).await), "raw"),
            [Some(bytes)]
        );

        // Writes report rows affected.
        let out = run(&c, &format!("HSET {hash} f1 v1 f2 v2 f3 grüße")).await;
        assert_eq!(out.rows_affected, [3]);
        assert!(out.result_sets.is_empty());
        assert_eq!(
            run(&c, &format!("RPUSH {list} a b c")).await.rows_affected,
            [3]
        );
        assert_eq!(
            run(&c, &format!("SADD {set} a b c")).await.rows_affected,
            [3]
        );
        assert_eq!(
            run(&c, &format!("ZADD {zset} 1 a 2 b 3.5 c"))
                .await
                .rows_affected,
            [3]
        );
        run(&c, &format!("XADD {stream} 1-0 k v1")).await;
        run(&c, &format!("XADD {stream} 1-1 k v2")).await;

        let b = batch(&run(&c, &format!("HGETALL {hash}")).await);
        assert_eq!(names(&b), ["field", "value"], "{protocol}");
        let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = binaries(&b, "field")
            .into_iter()
            .zip(binaries(&b, "value"))
            .map(|(f, v)| (f.unwrap(), v.unwrap()))
            .collect();
        pairs.sort();
        assert_eq!(pairs[2], (b"f3".to_vec(), "grüße".as_bytes().to_vec()));

        let b = batch(&run(&c, &format!("ZRANGE {zset} 0 -1 WITHSCORES")).await);
        assert_eq!(names(&b), ["member", "score"], "{protocol}");
        assert_eq!(
            b.column(1).as_primitive::<Float64Type>().values().to_vec(),
            [1.0, 2.0, 3.5]
        );

        let b = batch(&run(&c, &format!("LRANGE {list} 1 -1")).await);
        assert_eq!(names(&b), ["index", "value"]);
        assert_eq!(
            b.column(0).as_primitive::<Int64Type>().values().to_vec(),
            [1, 2]
        );
        assert_eq!(
            binaries(&b, "value"),
            [Some(b"b".to_vec()), Some(b"c".to_vec())]
        );

        let b = batch(&run(&c, &format!("SMEMBERS {set}")).await);
        assert_eq!(names(&b), ["member"], "{protocol}");
        assert_eq!(b.num_rows(), 3);

        let b = batch(&run(&c, &format!("XRANGE {stream} - +")).await);
        assert_eq!(names(&b), ["id", "fields"]);
        assert_eq!(texts(&b, "fields")[1].as_deref(), Some(r#"{"k":"v2"}"#));

        let out = run(&c, &format!("HSCAN {hash} 0 COUNT 100")).await;
        assert_eq!(names(&batch(&out)), ["field", "value"]);
        assert!(out.notices.iter().any(|n| n.message.contains("cursor")));

        let b = batch(&run(&c, "INFO server").await);
        assert_eq!(names(&b), ["section", "key", "value"]);
        assert!(texts(&b, "key").contains(&Some("redis_version".to_owned())));

        // Generic shapes: a map (CONFIG GET), an array (SCAN with nested JSON), a double, nil.
        let b = batch(&run(&c, "CONFIG GET maxmemory-policy").await);
        assert_eq!(
            names(&b),
            ["key", "key_raw", "type", "value", "raw"],
            "{protocol}"
        );
        assert_eq!(texts(&b, "value"), [Some("noeviction".to_owned())]);
        let b = batch(&run(&c, &format!("SCAN 0 MATCH oxbase:driver:t{t}:* COUNT 1000")).await);
        assert_eq!(names(&b), ["#", "type", "value", "raw"]);
        assert_eq!(texts(&b, "type")[1].as_deref(), Some("array"));
        let b = batch(&run(&c, &format!("ZSCORE {zset} c")).await);
        let expect = if protocol == "resp3" {
            "double"
        } else {
            "string"
        };
        assert_eq!(texts(&b, "type"), [Some(expect.to_owned())]);
        assert_eq!(texts(&b, "value"), [Some("3.5".to_owned())]);
        let b = batch(&run(&c, &format!("GET {}", key(&t, "missing"))).await);
        assert_eq!(texts(&b, "type"), [Some("null".to_owned())]);
        assert_eq!(texts(&b, "value"), [None]);

        // MULTI / EXEC line by line; errors inside EXEC are rows.
        assert_eq!(scalar(&c, "MULTI").await.as_deref(), Some("OK"));
        assert_eq!(
            scalar(&c, &format!("INCR {string}")).await.as_deref(),
            Some("QUEUED")
        );
        assert_eq!(scalar(&c, "PING").await.as_deref(), Some("QUEUED"));
        let b = batch(&run(&c, "EXEC").await);
        assert_eq!(texts(&b, "type")[0].as_deref(), Some("error"));
        assert!(texts(&b, "value")[0].as_deref().unwrap().starts_with("ERR"));

        // Error replies are Sql errors with the first word as the code.
        let err = try_run(&c, &format!("LPUSH {string} x"), &[])
            .await
            .unwrap_err();
        assert_eq!(err.sql_code(), Some("WRONGTYPE"), "{err}");
        let err = try_run(&c, "NOSUCHCOMMAND x", &[]).await.unwrap_err();
        assert_eq!(err.sql_code(), Some("ERR"), "{err}");
        // Commands that would take the session over are refused before they run.
        for line in ["SUBSCRIBE c", "MONITOR", "HELLO 2", "QUIT"] {
            let err = try_run(&c, line, &[]).await.unwrap_err();
            assert!(matches!(err, Error::InvalidInput { .. }), "{line}: {err}");
        }
        ping(&c).await;

        // The native API the dialect cells use.
        let mut get = redis::cmd("GET");
        get.arg(&string);
        let v = c.command(&get, &CancellationToken::new()).await.unwrap();
        assert_eq!(v, redis::Value::BulkString(b"hello".to_vec()));
        let mut lpush = redis::cmd("LPUSH");
        lpush.arg(&string).arg("x");
        let err = c
            .command(&lpush, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(err.sql_code(), Some("WRONGTYPE"));
        let mut pipe = redis::pipe();
        pipe.cmd("TYPE").arg(&hash).cmd("PTTL").arg(&hash);
        let replies = c.pipeline(&pipe, &CancellationToken::new()).await.unwrap();
        assert_eq!(replies.len(), 2);
        assert_eq!(c.masters(&CancellationToken::new()).await.unwrap().len(), 1);

        cleanup(&c, &t).await;
        c.close().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn blocking_commands_cancel_within_2s_and_the_session_stays() {
    let t = token();
    let c = Arc::new(connect(&params()).await);
    let line = format!("BLPOP {} 0", key(&t, "never"));
    let id_before = hello(c.as_ref(), "id").await;
    cancel_once(&c, &line, CancelBy::Token).await;
    cancel_once(&c, &line, CancelBy::CancelRunning).await;
    // CLIENT UNBLOCK kept the connection: same client id.
    assert_eq!(hello(c.as_ref(), "id").await, id_before);

    // XREAD ... BLOCK too.
    let xread = format!("XREAD BLOCK 0 STREAMS {} $", key(&t, "nostream"));
    cancel_once(&c, &xread, CancelBy::Token).await;
    c.close().await.unwrap();

    // A user without CLIENT UNBLOCK: the connection is replaced, still within 2 s.
    let f = fixtures();
    let password = s(&f, &["engines", "redis", "password"]);
    let admin = connect(&params()).await;
    let user = format!("oxbase_t{t}");
    run_with(
        &admin,
        "ACL SETUSER",
        &[
            Value::Text(user.clone()),
            Value::Text("on".into()),
            Value::Text(format!(">{password}")),
            Value::Text("~*".into()),
            Value::Text("+@all".into()),
            Value::Text("-client".into()),
        ],
    )
    .await;
    let limited = Arc::new(connect(&params().with_user(user.clone()).with_database("2")).await);
    let id_before = hello(limited.as_ref(), "id").await;
    cancel_once(&limited, &line, CancelBy::Token).await;
    // A new connection, still on database 2.
    assert_ne!(hello(limited.as_ref(), "id").await, id_before);
    assert_eq!(limited.current_database().as_deref(), Some("2"));
    let out = run(limited.as_ref(), "PING").await;
    assert!(out.notices.is_empty());
    limited.close().await.unwrap();
    let _ = try_run(&admin, "ACL DELUSER", &[Value::Text(user)]).await;
    admin.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn a_cancelled_run_reports_the_reconnect() {
    // A run whose stream is dropped before the reply leaves the connection to be replaced; the
    // next command reconnects and says so.
    let t = token();
    let c = connect(&params()).await;
    let line = format!("BLPOP {} 0", key(&t, "dropped"));
    let stream = c
        .execute(&line, &[], CancellationToken::new())
        .await
        .unwrap();
    let mut stream = Box::pin(collect(stream));
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut stream)
            .await
            .is_err()
    );
    drop(stream);
    let out = run(&c, "PING").await;
    assert!(
        out.notices
            .iter()
            .any(|n| n.severity == NoticeSeverity::Warning && n.message.contains("reconnected")),
        "{:?}",
        out.notices
    );
    ping(&c).await;
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn two_hundred_thousand_elements_stream_in_batches() {
    let t = token();
    let c = connect(&params()).await;
    let list = key(&t, "big");
    for chunk in 0..ROWS / 10_000 {
        let mut params = vec![Value::Text(list.clone())];
        params.extend((0..10_000).map(|i| Value::Text(format!("e{}", chunk * 10_000 + i + 1))));
        let out = run_with(&c, "RPUSH", &params).await;
        assert_eq!(out.rows_affected.len(), 1);
    }
    let started = Instant::now();
    let stream = c
        .execute(
            &format!("LRANGE {list} 0 -1"),
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let out = collect(stream).await.unwrap();
    let set = &out.result_sets[0];
    assert_eq!(set.num_rows(), ROWS);
    assert!(set.batches.len() > 1, "{} batch(es)", set.batches.len());
    let last = set.batches.last().unwrap();
    let values = last.column(1).as_binary::<i32>();
    assert_eq!(
        values.value(values.len() - 1),
        format!("e{ROWS}").as_bytes()
    );
    eprintln!(
        "{ROWS} rows in {} batches, {:?}",
        set.batches.len(),
        started.elapsed()
    );
    cleanup(&c, &t).await;
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn dedicated_and_pubsub_connections() {
    use futures::StreamExt as _;
    use redis::aio::ConnectionLike as _;

    let t = token();
    let c = connect(&params().with_database("3")).await;
    let cancel = CancellationToken::new();
    let mut dedicated = c
        .dedicated(
            redis::AsyncConnectionConfig::new().set_response_timeout(None),
            &cancel,
        )
        .await
        .unwrap();
    let k = key(&t, "dedicated");
    let mut set = redis::cmd("SET");
    set.arg(&k).arg("on-db-3");
    dedicated
        .connection_mut()
        .req_packed_command(&set)
        .await
        .unwrap();
    assert_eq!(
        scalar(&c, &format!("GET {k}")).await.as_deref(),
        Some("on-db-3")
    );

    let mut pubsub = c.pubsub(&cancel).await.unwrap();
    let channel = key(&t, "channel");
    pubsub.subscribe(&channel).await.unwrap();
    let out = run(&c, &format!("PUBLISH {channel} hi")).await;
    assert_eq!(texts(&batch(&out), "value"), [Some("1".to_owned())]);
    let message = tokio::time::timeout(Duration::from_secs(5), pubsub.on_message().next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.get_payload_bytes(), b"hi");
    drop(pubsub);
    drop(dedicated);
    cleanup(&c, &t).await;
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture and the SSH bastion"]
async fn ssh_tunnel_plaintext_and_cancel() {
    let (tunnel, ssh, server) = bastion_tunnel("redis").await;
    let p = through(params(), &tunnel, &ssh, &server);
    let c = Arc::new(connect(&p).await);
    assert!(!c.tls_in_use());
    ping(c.as_ref()).await;
    // The CLIENT UNBLOCK side connection goes through the same tunnel.
    let t = token();
    let line = format!("BLPOP {} 0", key(&t, "ssh"));
    cancel_once(&c, &line, CancelBy::Token).await;
    cancel_once(&c, &line, CancelBy::CancelRunning).await;
    c.close().await.unwrap();

    // Cluster and sentinel profiles cannot use a tunnel.
    let err = RedisConnection::connect(&with_option(
        through(params(), &tunnel, &ssh, &server),
        options::MODE,
        "cluster",
    ))
    .await
    .unwrap_err();
    assert!(matches!(err, Error::InvalidInput { .. }), "{err}");
    drop(tunnel);
}

#[tokio::test]
#[ignore = "needs the redis TLS port (M7 fixtures)"]
async fn tls_modes_on_the_tls_port() {
    let f = fixtures();
    let tls_port = port(&f, &["engines", "redis", "tls_port"]);
    if !up_or_pending("the Redis TLS port", tls_port) {
        return;
    }
    let tls = |opts: TlsOptions| params().with_port(tls_port).with_tls(opts);

    let full = connect(&tls(verify(TlsMode::VerifyFull, "localhost"))).await;
    assert!(full.tls_in_use());
    ping(&full).await;
    let c = Arc::new(full);
    let t = token();
    let line = format!("BLPOP {} 0", key(&t, "tls"));
    cancel_once(&c, &line, CancelBy::Token).await;
    c.close().await.unwrap();

    // The certificate does not name the server: refused.
    let err = RedisConnection::connect(&tls(verify(
        TlsMode::VerifyFull,
        "wrong-name.oxbase.invalid",
    )))
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Connection { .. }), "{err}");
    // VerifyCa checks only the chain.
    let ca = connect(&tls(verify(TlsMode::VerifyCa, "wrong-name.oxbase.invalid"))).await;
    assert!(ca.tls_in_use());
    ping(&ca).await;
    // Require and Prefer encrypt without verifying; Prefer finds TLS on this port.
    for mode in [TlsMode::Require, TlsMode::Prefer] {
        let c = connect(&tls(TlsOptions::new(mode))).await;
        assert!(c.tls_in_use(), "{mode:?}");
        ping(&c).await;
    }
    // Plaintext to the TLS port fails.
    let err = RedisConnection::connect(&tls(TlsOptions::new(TlsMode::Disable)))
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Connection { .. } | Error::Timeout { .. }),
        "{err}"
    );
    // A client certificate is accepted (client certificates are optional on the fixture).
    let dir = in_workspace(&s(&f, &["tls", "dir"]));
    let mutual = verify(TlsMode::VerifyFull, "localhost").with_client_identity(
        PemSource::File(dir.join(s(&f, &["tls", "client_cert"]))),
        KeySource::File(dir.join(s(&f, &["tls", "client_key"]))),
    );
    let c = connect(&tls(mutual)).await;
    assert!(c.tls_in_use());
    ping(&c).await;
}

#[tokio::test]
#[ignore = "needs the redis TLS port (M7 fixtures) and the SSH bastion"]
async fn tls_through_the_ssh_tunnel_verifies_the_server_name() {
    let f = fixtures();
    if !up_or_pending(
        "the Redis TLS port",
        port(&f, &["engines", "redis", "tls_port"]),
    ) {
        return;
    }
    let (tunnel, ssh, server) = bastion_tunnel("redis_tls").await;
    // The service name once the fixture certificate carries it (M7 SANs), else a name it has;
    // either way the socket goes to 127.0.0.1:<tunnel port>.
    let sans = strings(&f, &["tls", "server_sans"]);
    let name = if sans.iter().any(|n| n == &server.host) {
        server.host.clone()
    } else {
        "localhost".to_owned()
    };
    let mut full = TlsOptions::new(TlsMode::VerifyFull).with_ca_cert(ca_cert());
    if name != server.host {
        full = full.with_server_name(&name);
    }
    let c = Arc::new(connect(&through(params().with_tls(full), &tunnel, &ssh, &server)).await);
    assert!(c.tls_in_use());
    ping(c.as_ref()).await;
    let t = token();
    let line = format!("BLPOP {} 0", key(&t, "tls-ssh"));
    cancel_once(&c, &line, CancelBy::CancelRunning).await;
    c.close().await.unwrap();

    let wrong = TlsOptions::new(TlsMode::VerifyFull)
        .with_ca_cert(ca_cert())
        .with_server_name("wrong-name.oxbase.invalid");
    let err = RedisConnection::connect(&through(params().with_tls(wrong), &tunnel, &ssh, &server))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Connection { .. }), "{err}");
    drop(tunnel);
}

#[tokio::test]
#[ignore = "needs the redis cluster fixture (M7)"]
async fn cluster_connects_and_routes_by_slot() {
    let f = fixtures();
    let ports: Vec<u16> = lookup(&f, &["m7", "redis_cluster", "ports"])
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_integer().and_then(|p| u16::try_from(p).ok()))
        .collect();
    let Some(&first) = ports.first() else {
        panic!("fixtures.toml has no [m7.redis_cluster] ports");
    };
    if !up_or_pending("the Redis cluster", first) {
        return;
    }
    let host = s(&f, &["m7", "redis_cluster", "host"]);
    let nodes = ports[1..]
        .iter()
        .map(|p| format!("{host}:{p}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut p = ConnectParams::network(Engine::Redis, host.clone())
        .with_port(first)
        .with_user(s(&f, &["m7", "redis_cluster", "user"]))
        .with_password(s(&f, &["m7", "redis_cluster", "password"]))
        .with_tls(TlsOptions::new(TlsMode::Disable));
    p = with_option(p, options::MODE, "cluster");
    p = with_option(p, options::NODES, &nodes);
    let c = connect(&p).await;
    assert!(c.is_cluster());
    assert_eq!(c.current_database().as_deref(), Some("0"));
    let masters = c.masters(&CancellationToken::new()).await.unwrap();
    assert_eq!(masters.len(), ports.len(), "{masters:?}");

    // Keys in different slots: every SET and GET is routed to its master.
    let t = token();
    for i in 0..20 {
        let k = format!("oxbase:driver:t{t}:{i}");
        run_with(&c, "SET", &[Value::Text(k.clone()), Value::Int(i)]).await;
        assert_eq!(scalar(&c, &format!("GET {k}")).await, Some(i.to_string()));
    }
    // A command on each master.
    let mut dbsize = 0;
    for node in &masters {
        let v = c
            .command_on(node, &redis::cmd("DBSIZE"), &CancellationToken::new())
            .await
            .unwrap();
        let redis::Value::Int(n) = v else {
            panic!("DBSIZE on {node}: {v:?}")
        };
        dbsize += n;
    }
    assert!(dbsize >= 20);
    for i in 0..20 {
        run(&c, &format!("DEL oxbase:driver:t{t}:{i}")).await;
    }
    assert!(try_run(&c, "SELECT 1", &[]).await.is_err());
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis sentinel fixture (M7)"]
async fn sentinel_finds_the_master() {
    let f = fixtures();
    let sentinel_port = port(&f, &["m7", "redis_sentinel", "sentinel_port"]);
    if !up_or_pending("the Redis sentinel", sentinel_port) {
        return;
    }
    let host = s(&f, &["m7", "redis_sentinel", "host"]);
    let mut p = ConnectParams::network(Engine::Redis, host)
        .with_port(sentinel_port)
        .with_user(s(&f, &["m7", "redis_sentinel", "user"]))
        .with_password(s(&f, &["m7", "redis_sentinel", "password"]))
        .with_tls(TlsOptions::new(TlsMode::Disable));
    p = with_option(p, options::MODE, "sentinel");
    p = with_option(
        p,
        options::MASTER_NAME,
        &s(&f, &["m7", "redis_sentinel", "master_name"]),
    );
    let c = connect(&p).await;
    let b = batch(&run(&c, "ROLE").await);
    assert_eq!(texts(&b, "value")[0].as_deref(), Some("master"));
    let masters = c.masters(&CancellationToken::new()).await.unwrap();
    assert_eq!(
        masters[0].port,
        port(&f, &["m7", "redis_sentinel", "master_port"])
    );
    let seed = s(&f, &["m7", "redis_sentinel", "seed_key"]);
    if let Some(value) = scalar(&c, &format!("GET {seed}")).await {
        assert_eq!(value, "master");
    }
    // An unknown master name is refused.
    let unknown = with_option(p.clone(), options::MASTER_NAME, "no-such-master");
    let err = RedisConnection::connect(&unknown).await.unwrap_err();
    assert!(err.to_string().contains("no-such-master"), "{err}");
    c.close().await.unwrap();
}
