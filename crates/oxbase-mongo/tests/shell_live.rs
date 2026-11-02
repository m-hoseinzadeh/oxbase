//! Live tests of the mongo-shell subset against the `mongodb` fixture (docs/m7-contracts.md
//! §11, shell): statements split by `MongoShell` and run through `Connection::execute` on a
//! `MongoConnection`, as the query service runs them.
//!
//! All are `#[ignore]`d: the conformance CI job runs them with
//! `cargo test -p oxbase-mongo -- --include-ignored`. Connection parameters come from
//! `tests/fixtures.toml` (`$OXBASE_FIXTURES`). The tests write only collections of their own
//! (`m7sh_t<epoch>_<pid>_<what>` in `oxbase_test`), drop them at the end and drop leftovers
//! older than an hour. The exit-check queries on the M7 seed (`m7_docs`, `m7_big`) run when
//! the seed is loaded and print "pending" otherwise, unless `OXBASE_SHELL_REQUIRE_M7_SEED=1`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arrow_array::cast::AsArray;
use arrow_array::types::{Int32Type, Int64Type};
use arrow_array::{Array, RecordBatch};
use arrow_schema::DataType;
use bson::Document;
use oxbase_dialect::ScriptLanguage;
use oxbase_driver::{CancellationToken, Collected, ConnectParams, Connection, collect};
use oxbase_mongo::shell::MongoShell;
use oxbase_mongo::{MongoConnection, options};
use oxbase_types::{Engine, Error, TlsMode, TlsOptions};

const CANCEL_LIMIT: Duration = Duration::from_secs(2);
const DATABASE: &str = "oxbase_test";
const PREFIX: &str = "m7sh_t";

// ---- fixtures -------------------------------------------------------------------------------

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
        .and_then(|e| e.get("mongodb"))
        .and_then(|m| m.get(key))
        .unwrap_or_else(|| panic!("fixtures.toml has no engines.mongodb.{key}"));
    match value {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn params() -> ConnectParams {
    let port: u16 = engine("port").parse().expect("port");
    let mut p = ConnectParams::network(Engine::MongoDb, engine("host"))
        .with_port(port)
        .with_user(engine("user"))
        .with_password(engine("password"))
        .with_database(DATABASE)
        .with_tls(TlsOptions::new(TlsMode::Disable));
    p.options
        .insert(options::AUTH_SOURCE.into(), engine("auth_source"));
    p.connect_timeout = Some(Duration::from_secs(15));
    p
}

async fn connect() -> MongoConnection {
    MongoConnection::connect(&params())
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"))
}

// ---- running statements ---------------------------------------------------------------------

async fn try_run(c: &dyn Connection, text: &str) -> Result<Collected, Error> {
    collect(c.execute(text, &[], CancellationToken::new()).await?).await
}

async fn run(c: &dyn Connection, text: &str) -> Collected {
    try_run(c, text)
        .await
        .unwrap_or_else(|e| panic!("{text}: {e}"))
}

/// Every statement of `script` (split like the editor does), in order.
async fn run_script(c: &dyn Connection, script: &str) -> Vec<Collected> {
    let mut out = Vec::new();
    for statement in MongoShell.split(script) {
        out.push(run(c, &statement.text).await);
    }
    out
}

/// The first result set of a small result (one batch).
fn one_batch(out: &Collected) -> RecordBatch {
    let set = out.result_sets.first().expect("a result set");
    match set.batches.as_slice() {
        [] => RecordBatch::new_empty(set.schema.clone()),
        [only] => only.clone(),
        more => panic!("{} batches for a small result", more.len()),
    }
}

/// The JSON text of a one-row `value` result.
fn value_text(out: &Collected) -> String {
    let b = one_batch(out);
    assert_eq!(b.num_rows(), 1);
    b.column(b.schema().index_of("value").expect("a value column"))
        .as_string::<i32>()
        .value(0)
        .to_owned()
}

/// A whole-number column as i64 (Int32 or Int64).
fn ints(b: &RecordBatch, name: &str) -> Vec<Option<i64>> {
    let column = b.column(
        b.schema()
            .index_of(name)
            .unwrap_or_else(|_| panic!("column {name}")),
    );
    match column.data_type() {
        DataType::Int32 => column
            .as_primitive::<Int32Type>()
            .iter()
            .map(|v| v.map(i64::from))
            .collect(),
        DataType::Int64 => column.as_primitive::<Int64Type>().iter().collect(),
        other => panic!("{name} is {other}"),
    }
}

fn texts(b: &RecordBatch, name: &str) -> Vec<Option<String>> {
    b.column(
        b.schema()
            .index_of(name)
            .unwrap_or_else(|_| panic!("column {name}")),
    )
    .as_string::<i32>()
    .iter()
    .map(|v| v.map(str::to_owned))
    .collect()
}

// ---- test-owned collections -----------------------------------------------------------------

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn owned_name(what: &str) -> String {
    format!("{PREFIX}{}_{}_{what}", epoch_secs(), std::process::id())
}

async fn drop_stale(c: &MongoConnection) {
    let db = c.client().database(DATABASE);
    let names = db.list_collection_names().await.expect("listCollections");
    let now = epoch_secs();
    for name in names {
        let Some(rest) = name.strip_prefix(PREFIX) else {
            continue;
        };
        let born: u64 = rest
            .split('_')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        if now.saturating_sub(born) > 3600 {
            let _ = db.collection::<Document>(&name).drop().await;
        }
    }
}

async fn drop_collection(c: &MongoConnection, name: &str) {
    let _ = c
        .client()
        .database(DATABASE)
        .collection::<Document>(name)
        .drop()
        .await;
}

/// Inserts `{_id: n, g: n % 100, s: "row-<n>"}` for n in 1..=count through the shell.
async fn fill(c: &MongoConnection, name: &str, count: u32) {
    let out = run(
        c,
        &format!(
            "db.{name}.insertMany(Array.from({{length: {count}}}, (_, i) => \
             ({{_id: i + 1, g: (i + 1) % 100, s: 'row-' + (i + 1)}})))"
        ),
    )
    .await;
    assert_eq!(out.rows_affected, [u64::from(count)]);
}

fn require_seed() -> bool {
    std::env::var("OXBASE_SHELL_REQUIRE_M7_SEED").as_deref() == Ok("1")
}

// ---- tests ----------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs the mongodb fixture"]
async fn shell_statements_read_write_and_stream() {
    let c = connect().await;
    drop_stale(&c).await;
    let name = owned_name("rw");
    drop_collection(&c, &name).await;
    fill(&c, &name, 1000).await;

    // A count is one `value` row.
    let out = run(&c, &format!("db.{name}.countDocuments({{}})")).await;
    assert_eq!(value_text(&out), "1000");
    assert_eq!(
        value_text(&run(&c, &format!("db.{name}.estimatedDocumentCount()")).await),
        "1000"
    );

    // A cursor streams as a document result set (driver request path).
    let script = format!("db.{name}.find({{g: 7}})\n  .sort({{_id: 1}})\n  .limit(5)\n");
    let statements = MongoShell.split(&script);
    assert_eq!(statements.len(), 1);
    let out = run(&c, &statements[0].text).await;
    let b = one_batch(&out);
    assert_eq!(ints(&b, "_id"), [7, 107, 207, 307, 407].map(Some));
    let schema = b.schema();
    assert_eq!(schema.fields().last().unwrap().name(), "$document");
    assert_eq!(texts(&b, "s")[0].as_deref(), Some("row-7"));

    // An aggregate with $group.
    let out = run(
        &c,
        &format!("db.{name}.aggregate([{{$group: {{_id: null, total: {{$sum: '$g'}}}}}}])"),
    )
    .await;
    assert_eq!(ints(&one_batch(&out), "total"), [Some(49_500)]);

    // Writes: RowsAffected and a one-row result with the acknowledged fields.
    let out = run(
        &c,
        &format!("db.{name}.updateMany({{g: 7}}, {{$set: {{seen: true}}}})"),
    )
    .await;
    assert_eq!(out.rows_affected, [10]);
    let b = one_batch(&out);
    assert_eq!(ints(&b, "matchedCount"), [Some(10)]);
    assert_eq!(ints(&b, "modifiedCount"), [Some(10)]);
    let out = run(&c, &format!("db.{name}.deleteOne({{_id: 7}})")).await;
    assert_eq!(out.rows_affected, [1]);
    assert_eq!(ints(&one_batch(&out), "deletedCount"), [Some(1)]);
    let out = run(&c, &format!("db.{name}.insertOne({{s: 'new'}})")).await;
    assert_eq!(out.rows_affected, [1]);
    let id = texts(&one_batch(&out), "insertedId")[0].clone().unwrap();
    assert_eq!(id.len(), 24, "an ObjectId: {id}");
    assert_eq!(
        value_text(&run(&c, &format!("db.{name}.findOne({{_id: 7}})")).await),
        "null"
    );
    let b = one_batch(&run(&c, &format!("db.{name}.findOne({{_id: 107}}, {{s: 1}})")).await);
    assert_eq!(ints(&b, "_id"), [Some(107)]);
    assert_eq!(texts(&b, "s"), [Some("row-107".to_owned())]);

    // Values read into JavaScript.
    assert_eq!(
        value_text(&run(&c, &format!("db.{name}.find({{g: 1}}).toArray().length")).await),
        "10"
    );
    let out = run(
        &c,
        &format!("db.{name}.find({{_id: {{$lte: 3}}}}).sort({{_id: 1}}).forEach(d => print(d.s))"),
    )
    .await;
    let printed: Vec<&str> = out.notices.iter().map(|n| n.message.as_str()).collect();
    assert_eq!(printed, ["row-1", "row-2", "row-3"]);
    assert!(out.result_sets.is_empty());
    assert_eq!(
        value_text(&run(&c, &format!("db.{name}.distinct('g', {{_id: {{$lt: 3}}}})")).await),
        "[1,2]"
    );

    // Indexes.
    assert_eq!(
        value_text(&run(&c, &format!("db.{name}.createIndex({{g: 1}})")).await),
        "\"g_1\""
    );
    let b = one_batch(&run(&c, &format!("db.{name}.getIndexes()")).await);
    let names = texts(&b, "name");
    assert!(names.contains(&Some("_id_".to_owned())) && names.contains(&Some("g_1".to_owned())));

    // Globals persist between statements of the session.
    let outs = run_script(
        &c,
        &format!("var n = db.{name}.countDocuments({{g: 2}})\nprint(n)\nn * 2"),
    )
    .await;
    assert_eq!(outs.len(), 3);
    assert_eq!(outs[1].notices[0].message, "10");
    assert_eq!(value_text(&outs[2]), "20");

    // show / use.
    let b = one_batch(&run(&c, "show collections").await);
    assert!(texts(&b, "name").contains(&Some(name.clone())));
    let b = one_batch(&run(&c, "show dbs").await);
    assert!(texts(&b, "name").contains(&Some(DATABASE.to_owned())));
    let out = run(&c, "use admin").await;
    assert_eq!(out.notices[0].message, "switched to db admin");
    assert_eq!(c.current_database().as_deref(), Some("admin"));
    assert_eq!(value_text(&run(&c, "db.getName()").await), "\"admin\"");
    run(&c, &format!("use {DATABASE}")).await;
    assert_eq!(c.current_database().as_deref(), Some(DATABASE));
    run(&c, "db = db.getSiblingDB('admin')").await;
    assert_eq!(c.current_database().as_deref(), Some("admin"));
    run(&c, &format!("use {DATABASE}")).await;

    // Server errors keep their code and name the statement line.
    let err = try_run(&c, &format!("1\ndb.{name}.insertOne({{_id: 1}})"))
        .await
        .expect_err("duplicate key");
    assert!(
        matches!(err.sql_code(), Some("DuplicateKey" | "11000")),
        "{err}"
    );
    assert!(err.to_string().contains("(line 2)"), "{err}");
    let err = try_run(&c, "it").await.expect_err("it");
    assert!(matches!(err, Error::Unsupported { .. }), "{err}");

    assert_eq!(
        value_text(&run(&c, &format!("db.{name}.drop()")).await),
        "true"
    );
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the mongodb fixture"]
async fn seeded_collections_answer_the_exit_check() {
    let c = connect().await;
    let db = c.client().database(DATABASE);
    let docs = db
        .collection::<Document>("m7_docs")
        .estimated_document_count()
        .await
        .unwrap_or(0);
    let big = db
        .collection::<Document>("m7_big")
        .estimated_document_count()
        .await
        .unwrap_or(0);
    if docs != 12 || big != 200_000 {
        assert!(
            !require_seed(),
            "the M7 seed is not loaded (m7_docs {docs}, m7_big {big})"
        );
        eprintln!("pending: the M7 mongodb seed is not loaded (m7_docs {docs}, m7_big {big})");
        return;
    }
    assert_eq!(
        value_text(&run(&c, "db.m7_docs.countDocuments({})").await),
        "12"
    );
    let b = one_batch(&run(&c, "db.m7_big.find({g: 7}).sort({_id: 1}).limit(5)").await);
    assert_eq!(ints(&b, "_id"), [7, 107, 207, 307, 407].map(Some));
    let b = one_batch(
        &run(
            &c,
            "db.m7_big.aggregate([{$group: {_id: null, total: {$sum: \"$g\"}}}])",
        )
        .await,
    );
    assert_eq!(ints(&b, "total"), [Some(9_900_000)]);
    // The whole collection streams from a cursor result (no JavaScript iteration limit).
    let out = run(&c, "db.m7_big.find({}, {g: 1})").await;
    assert_eq!(out.result_sets[0].num_rows(), 200_000);
    // Reading it into JavaScript hits the iteration limit (or the memory limit) instead.
    let err = try_run(&c, "db.m7_big.find().toArray().length")
        .await
        .expect_err("over the limit");
    assert!(
        err.to_string().contains("100000 documents") || err.sql_code() == Some("OutOfMemory"),
        "{err}"
    );
    run(&c, "use oxbase_test").await;
    assert_eq!(c.current_database().as_deref(), Some(DATABASE));
}

/// A `$where` busy loop of 50 ms per document: a find that runs for seconds.
const SLOW_FILTER: &str =
    "{$where: 'function() { var t = Date.now(); while (Date.now() - t < 50) {} return true; }'}";

/// Runs `text`, cancels it after 300 ms and checks the stream ends with `Cancelled` within
/// 2 s and the session answers afterwards.
async fn cancel_after_300ms(c: &Arc<MongoConnection>, text: &str) {
    let token = CancellationToken::new();
    let task = {
        let c = Arc::clone(c);
        let token = token.clone();
        let text = text.to_owned();
        tokio::spawn(async move { collect(c.execute(&text, &[], token).await?).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!task.is_finished(), "{text}: finished before the cancel");
    let requested = Instant::now();
    token.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the statement ends")
        .expect("task");
    let took = requested.elapsed();
    let err = outcome.expect_err("cancelled");
    assert!(err.is_cancelled(), "{text}: {err}");
    assert!(took < CANCEL_LIMIT, "{text}: cancelled after {took:?}");
    assert_eq!(value_text(&run(c.as_ref(), "1 + 1").await), "2");
    assert!(requested.elapsed() < CANCEL_LIMIT + CANCEL_LIMIT);
}

/// Waits until no operation of this user runs on `collection` (the cancel killed it).
async fn server_operation_gone(c: &MongoConnection, collection: &str) {
    let deadline = Instant::now() + CANCEL_LIMIT;
    loop {
        let out = run(
            c,
            &format!(
                r#"{{"type": "command", "database": "admin", "command": {{"currentOp": true, "$ownOps": true, "ns": "{DATABASE}.{collection}"}}}}"#
            ),
        )
        .await;
        let text = texts(&one_batch(&out), "inprog")[0]
            .clone()
            .unwrap_or_default();
        if text == "[]" {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the operation on {collection} still runs: {text}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "needs the mongodb fixture"]
async fn cancel_stops_loops_and_server_work_within_2s() {
    let c = Arc::new(connect().await);
    drop_stale(&c).await;
    let name = owned_name("cancel");
    drop_collection(&c, &name).await;
    fill(&c, &name, 200).await;

    // A JavaScript loop: the interrupt handler.
    cancel_after_300ms(&c, "while (true) {}").await;
    // A cursor read into JavaScript: the host wait and killOp by the statement's comment.
    cancel_after_300ms(&c, &format!("db.{name}.find({SLOW_FILTER}).toArray()")).await;
    server_operation_gone(&c, &name).await;
    // A command the script runs.
    cancel_after_300ms(
        &c,
        &format!("db.runCommand({{count: '{name}', query: {SLOW_FILTER}}})"),
    )
    .await;
    server_operation_gone(&c, &name).await;
    // A cursor returned as the result: the driver's request path.
    cancel_after_300ms(&c, &format!("db.{name}.find({SLOW_FILTER})")).await;
    server_operation_gone(&c, &name).await;

    drop_collection(&c, &name).await;
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the mongodb fixture"]
async fn the_time_limit_and_sandbox_hold_on_a_live_session() {
    let mut p = params();
    p.options
        .insert(options::SHELL_TIME_LIMIT_MS.into(), "500".into());
    let c = MongoConnection::connect(&p).await.expect("connect");
    let started = Instant::now();
    let err = try_run(&c, "for (;;) {}").await.expect_err("time limit");
    assert!(matches!(err, Error::Timeout { .. }), "{err}");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(
        value_text(&run(&c, "[typeof require, typeof load, typeof process].join()").await),
        "\"undefined,undefined,undefined\""
    );
    assert_eq!(
        value_text(&run(&c, "db.getName()").await),
        "\"oxbase_test\""
    );
    c.close().await.unwrap();
}
