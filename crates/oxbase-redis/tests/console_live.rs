//! Live tests of the Redis console language against the `redis` fixture (docs/m7-contracts.md
//! §11, shell): console scripts split by `RedisConsole` and run line by line through
//! `Connection::execute`, as the query service runs them.
//!
//! All are `#[ignore]`d: the conformance CI job runs them with
//! `cargo test -p oxbase-redis -- --include-ignored`. Connection parameters come from
//! `tests/fixtures.toml` (`$OXBASE_FIXTURES`). The tests write only keys of their own
//! (`oxbase:console:t<token>:*`) and delete them at the end. The checks on the M7 seed keys
//! (`m7:hash`, `m7:zset`, `m7:binary`) run when the seed is loaded and print "pending"
//! otherwise, unless `OXBASE_SHELL_REQUIRE_M7_SEED=1`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_array::cast::AsArray;
use arrow_array::types::Float64Type;
use arrow_array::{Array, RecordBatch};
use oxbase_dialect::{ScriptLanguage, StatementGuard};
use oxbase_driver::{CancellationToken, Collected, ConnectParams, Connection, collect};
use oxbase_redis::RedisConnection;
use oxbase_redis::console::RedisConsole;
use oxbase_types::{Engine, Error, TlsMode, TlsOptions};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn fixtures() -> toml::Table {
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

fn engine(key: &str) -> String {
    let f = fixtures();
    let value = f
        .get("engines")
        .and_then(|e| e.get("redis"))
        .and_then(|r| r.get(key))
        .unwrap_or_else(|| panic!("fixtures.toml has no engines.redis.{key}"));
    match value {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn params() -> ConnectParams {
    let port: u16 = engine("port").parse().expect("port");
    ConnectParams::network(Engine::Redis, engine("host"))
        .with_port(port)
        .with_user(engine("user"))
        .with_password(engine("password"))
        .with_database(engine("database"))
        .with_tls(TlsOptions::new(TlsMode::Disable))
}

/// 10 base36 characters from the time and the process id.
fn token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut n = (nanos ^ (u128::from(std::process::id()) << 64)) % 36u128.pow(10);
    let mut out = String::new();
    for _ in 0..10 {
        let d = u32::try_from(n % 36).unwrap_or(0);
        out.push(char::from_digit(d, 36).unwrap_or('0'));
        n /= 36;
    }
    out
}

async fn try_run(c: &dyn Connection, line: &str) -> Result<Collected, Error> {
    collect(c.execute(line, &[], CancellationToken::new()).await?).await
}

async fn run(c: &dyn Connection, line: &str) -> Collected {
    try_run(c, line)
        .await
        .unwrap_or_else(|e| panic!("{line}: {e}"))
}

fn batch(out: &Collected) -> RecordBatch {
    let set = out.result_sets.first().expect("a result set");
    match set.batches.as_slice() {
        [] => RecordBatch::new_empty(set.schema.clone()),
        [only] => only.clone(),
        more => panic!("{} batches for a small reply", more.len()),
    }
}

fn binaries(b: &RecordBatch, col: &str) -> Vec<Vec<u8>> {
    let a = b
        .column_by_name(col)
        .unwrap_or_else(|| panic!("no column {col}"))
        .as_binary::<i32>();
    (0..a.len()).map(|i| a.value(i).to_vec()).collect()
}

fn floats(b: &RecordBatch, col: &str) -> Vec<f64> {
    let a = b
        .column_by_name(col)
        .unwrap_or_else(|| panic!("no column {col}"))
        .as_primitive::<Float64Type>();
    (0..a.len()).map(|i| a.value(i)).collect()
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

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn console_scripts_run_line_by_line() {
    let c = RedisConnection::connect(&params())
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"));
    let t = token();
    let hash = format!("oxbase:console:t{t}:hash");
    let zset = format!("oxbase:console:t{t}:zset");
    let bin = format!("oxbase:console:t{t}:bin");
    let script = format!(
        "# a console script\n\
         HSET {hash} a 1 b \"two words\"\n\
         \n\
         HGETALL {hash}\n\
         // another comment\n\
         ZADD {zset} 1 one 2.5 two\n\
         ZRANGE {zset} 0 -1 WITHSCORES\n\
         SET {bin} \"\\x00\\xff\\xfe\\n\"\n\
         GET {bin}\n\
         AUTH wrong-user not-the-password\n\
         DEL {hash} {zset} {bin}\n"
    );
    let statements = RedisConsole.split(&script);
    let texts_of: Vec<&str> = statements.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts_of.len(), 8, "{texts_of:?}");
    assert!(
        statements
            .iter()
            .all(|s| RedisConsole.guard(s) == StatementGuard::Allowed)
    );
    let mut outs = Vec::new();
    for s in &statements {
        outs.push(try_run(&c, &s.text).await);
    }
    let ok = |i: usize| -> &Collected {
        outs[i]
            .as_ref()
            .unwrap_or_else(|e| panic!("{}: {e}", statements[i].text))
    };
    assert_eq!(ok(0).rows_affected, [2], "HSET");
    let b = batch(ok(1));
    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = binaries(&b, "field")
        .into_iter()
        .zip(binaries(&b, "value"))
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        [
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"two words".to_vec())
        ]
    );
    assert_eq!(ok(2).rows_affected, [2], "ZADD");
    let b = batch(ok(3));
    assert_eq!(binaries(&b, "member"), [b"one".to_vec(), b"two".to_vec()]);
    assert_eq!(floats(&b, "score"), [1.0, 2.5]);
    assert_eq!(texts(&batch(ok(4)), "value"), [Some("OK".to_owned())]);
    assert_eq!(
        binaries(&batch(ok(5)), "raw"),
        [vec![0x00, 0xff, 0xfe, 0x0a]]
    );
    // A failing AUTH is a server error; its password never reaches the redacted history.
    let err = outs[6].as_ref().expect_err("wrong credentials");
    assert!(
        matches!(err, Error::Sql { .. } | Error::Auth { .. }),
        "{err}"
    );
    assert_eq!(
        RedisConsole.redact_secrets(&statements[6].text),
        "AUTH wrong-user ***"
    );
    assert_eq!(ok(7).rows_affected, [3], "DEL");
    // The session still works after the failed AUTH.
    assert_eq!(
        texts(&batch(&run(&c, "PING").await), "value"),
        [Some("PONG".to_owned())]
    );
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn destructive_lines_are_guarded_and_blocked_lines_refused() {
    let script = "FLUSHDB\nKEYS m7:*\nSUBSCRIBE m7:news\nMONITOR\nGET m7:string\n";
    let statements = RedisConsole.split(script);
    let guards: Vec<StatementGuard> = statements.iter().map(|s| RedisConsole.guard(s)).collect();
    assert!(matches!(guards[0], StatementGuard::Confirm(_)));
    assert!(matches!(guards[1], StatementGuard::Confirm(_)));
    assert!(matches!(guards[2], StatementGuard::Blocked(_)));
    assert!(matches!(guards[3], StatementGuard::Blocked(_)));
    assert_eq!(guards[4], StatementGuard::Allowed);
    // The driver refuses the blocked ones too (defence in depth), without touching the session.
    let c = RedisConnection::connect(&params())
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"));
    for line in ["SUBSCRIBE m7:news", "MONITOR"] {
        let err = try_run(&c, line).await.expect_err(line);
        assert!(matches!(err, Error::InvalidInput { .. }), "{line}: {err}");
    }
    assert_eq!(
        texts(&batch(&run(&c, "PING").await), "value"),
        [Some("PONG".to_owned())]
    );
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn seeded_keys_answer_the_exit_check() {
    let c = RedisConnection::connect(&params())
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"));
    let exists = texts(
        &batch(&run(&c, "EXISTS m7:hash m7:zset m7:binary").await),
        "value",
    );
    if exists != [Some("3".to_owned())] {
        assert!(
            std::env::var("OXBASE_SHELL_REQUIRE_M7_SEED").as_deref() != Ok("1"),
            "the M7 redis seed is not loaded"
        );
        eprintln!("pending: the M7 redis seed is not loaded");
        c.close().await.unwrap();
        return;
    }
    let script = "HGETALL m7:hash\nZRANGE m7:zset 0 -1 WITHSCORES\nGET m7:binary\n";
    let statements = RedisConsole.split(script);
    assert_eq!(statements.len(), 3);
    let hash = batch(&run(&c, &statements[0].text).await);
    assert!(hash.num_rows() > 0);
    assert_eq!(hash.schema().field(0).name(), "field");
    let zset = batch(&run(&c, &statements[1].text).await);
    let scores = floats(&zset, "score");
    assert_ne!(scores.len(), 0);
    assert!(scores.windows(2).all(|w| w[0] <= w[1]));
    let binary = batch(&run(&c, &statements[2].text).await);
    assert_eq!(binaries(&binary, "raw"), [vec![0x00, 0xff, 0xfe, 0x0a]]);
    c.close().await.unwrap();
}
