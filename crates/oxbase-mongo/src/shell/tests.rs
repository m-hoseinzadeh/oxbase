//! Unit tests of the shell that need no server: the language rules (table-driven and fuzzed),
//! and the sandboxed QuickJS session (values, helpers, errors, limits, cancel). Live tests
//! against the fixture are in `tests/shell_live.rs`.

use std::time::{Duration, Instant};

use mongodb::options::{ClientOptions, ServerAddress};
use oxbase_driver::{CancellationToken, ConnectParams};
use oxbase_sql::StatementKind;
use oxbase_types::Engine;
use pretty_assertions::assert_eq;
use serde_json::{Value as Json, json};

use super::*;

#[test]
fn limits_follow_the_profile_options() {
    let mut params = ConnectParams::network(Engine::MongoDb, "127.0.0.1");
    params
        .options
        .insert(crate::options::SHELL_TIME_LIMIT_MS.into(), "0".into());
    let options = MongoOptions::from_params(&params).unwrap();
    let limits = ShellLimits::from_options(&options);
    assert_eq!(limits.memory_bytes, 64 * 1024 * 1024);
    assert_eq!(limits.stack_bytes, DEFAULT_STACK_BYTES);
    assert_eq!(limits.time_limit, None);
    let session = ShellSession::new(limits);
    assert_eq!(session.limits(), limits);
    assert_eq!(MongoShell.editor_language(), "javascript");
    assert_eq!(MongoShell.name(), "mongo-shell");
}

#[test]
fn the_language_splits_guards_and_redacts() {
    let script = "use oxbase_test\ndb.m7_docs.find({a: 1})\n  .limit(2)\ndb.m7_docs.drop()\n";
    let statements = MongoShell.split(script);
    let texts: Vec<&str> = statements.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "use oxbase_test",
            "db.m7_docs.find({a: 1})\n  .limit(2)",
            "db.m7_docs.drop()"
        ]
    );
    assert_eq!(MongoShell.guard(&statements[0]), StatementGuard::Allowed);
    assert_eq!(MongoShell.guard(&statements[1]), StatementGuard::Allowed);
    assert!(matches!(
        MongoShell.guard(&statements[2]),
        StatementGuard::Confirm(_)
    ));
    assert_eq!(statements[1].kind, StatementKind::Query);
    assert_eq!(
        MongoShell.redact_secrets("db.createUser({user: 'u', pwd: 'pw', roles: []})"),
        "db.createUser({user: 'u', pwd: '***', roles: []})"
    );
    assert!(matches!(
        MongoShell.redact_secrets("db.c.find()"),
        Cow::Borrowed(_)
    ));
}

// ---- fuzz -----------------------------------------------------------------------------------

/// Deterministic xorshift64* generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        let n = u64::try_from(n).unwrap_or(u64::MAX);
        usize::try_from(self.next() % n).unwrap_or(0)
    }
}

const PIECES: &[&str] = &[
    "db",
    ".",
    "c",
    "find",
    "(",
    ")",
    "{",
    "}",
    "[",
    "]",
    ";",
    "\n",
    " ",
    "\t",
    "'",
    "\"",
    "`",
    "${",
    "\\",
    "/",
    "//",
    "/*",
    "*/",
    "+",
    "++",
    "-",
    "=>",
    "=",
    ":",
    ",",
    "?",
    "!",
    "use",
    "show",
    "it",
    "x",
    "pwd",
    "password",
    "'s3'",
    "mongodb://u:p@h",
    "drop",
    "deleteMany",
    "updateMany",
    "renameCollection",
    "true",
    "runCommand",
    "dropDatabase",
    "if",
    "else",
    "function",
    "return",
    "ü",
    "€",
    "𝄞",
    "\r\n",
    "1",
    "0x1f",
    "auth",
    "changeUserPassword",
];

#[test]
fn fuzzed_scripts_never_panic_and_keep_the_range_rules() {
    let mut rng = Rng(0x0DDB_1A5E_5BAD_5EED);
    for round in 0..20_000 {
        let n = rng.below(40);
        let mut script: String = (0..n).map(|_| PIECES[rng.below(PIECES.len())]).collect();
        if round % 10 == 0 {
            script.insert(0, '{');
        }
        let statements = MongoShell.split(&script);
        let mut last = 0;
        for s in &statements {
            assert!(s.range.start >= last, "{script:?}");
            assert!(s.range.end <= s.segment_end && s.segment_end <= script.len());
            assert_eq!(&script[s.range.clone()], s.text, "{script:?}");
            assert!(!s.text.trim().is_empty(), "{script:?}");
            last = s.segment_end;
            let _ = MongoShell.guard(s);
        }
        let redacted = MongoShell.redact_secrets(&script);
        assert_eq!(
            MongoShell.redact_secrets(&redacted),
            redacted,
            "redaction is stable: {script:?}"
        );
        let _ = lex::tokens(&script);
    }
}

// ---- the QuickJS session ------------------------------------------------------------------

/// A client for an address nothing listens on: statements that never reach the server work.
fn offline_client() -> mongodb::Client {
    let mut options = ClientOptions::default();
    options.hosts = vec![ServerAddress::Tcp {
        host: "127.0.0.1".into(),
        port: Some(1),
    }];
    options.server_selection_timeout = Some(Duration::from_secs(1));
    options.connect_timeout = Some(Duration::from_secs(1));
    mongodb::Client::with_options(options).unwrap()
}

fn limits() -> ShellLimits {
    ShellLimits {
        memory_bytes: 32 * 1024 * 1024,
        stack_bytes: DEFAULT_STACK_BYTES,
        time_limit: Some(Duration::from_secs(10)),
    }
}

struct Js {
    worker: Worker,
    client: mongodb::Client,
}

impl Js {
    fn new(limits: ShellLimits) -> Self {
        Self {
            worker: Worker::start(limits).unwrap(),
            client: offline_client(),
        }
    }

    async fn run_with(&self, source: &str, cancel: CancellationToken) -> Outcome {
        let (reply, outcome) = tokio::sync::oneshot::channel();
        let env = StatementEnv {
            client: self.client.clone(),
            comment: "oxbase:test:1".into(),
            handle: tokio::runtime::Handle::current(),
            cancel,
        };
        let job = Job {
            source: source.to_owned(),
            database: "test".into(),
            env,
            reply,
        };
        assert!(self.worker.send(job).is_ok());
        outcome.await.unwrap()
    }

    async fn run(&self, source: &str) -> Outcome {
        self.run_with(source, CancellationToken::new()).await
    }

    /// The prelude's description of `source`'s value.
    async fn value(&self, source: &str) -> Json {
        let outcome = self.run(source).await;
        let text = outcome.result.unwrap_or_else(|e| panic!("{source}: {e}"));
        serde_json::from_str(&text).unwrap()
    }

    async fn error(&self, source: &str) -> Error {
        match self.run(source).await.result {
            Ok(v) => panic!("{source}: expected an error, got {v}"),
            Err(e) => e,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn values_and_globals() {
    let js = Js::new(limits());
    assert_eq!(
        js.value("1 + 2").await,
        json!({"kind": "value", "value": {"$numberInt": "3"}})
    );
    assert_eq!(js.value("var x = 40").await, json!({"kind": "none"}));
    assert_eq!(
        js.value("x + 2.5").await,
        json!({"kind": "value", "value": {"$numberDouble": "42.5"}})
    );
    assert_eq!(js.value("let y = 'a'").await, json!({"kind": "none"}));
    assert_eq!(js.value("y").await, json!({"kind": "value", "value": "a"}));
    assert_eq!(
        js.value("({b: 1, a: [true, null]})").await,
        json!({"kind": "documents", "docs": [{"b": {"$numberInt": "1"}, "a": [true, null]}]})
    );
    assert_eq!(
        js.value("[{a: 1}, {a: 'x'}]").await["docs"][1],
        json!({"a": "x"})
    );
    assert_eq!(js.value("[]").await, json!({"kind": "value", "value": []}));
    assert_eq!(
        js.value("null").await,
        json!({"kind": "value", "value": null})
    );
    assert_eq!(
        js.value("db.getName()").await,
        json!({"kind": "value", "value": "test"})
    );
    assert_eq!(
        js.value("db.getSiblingDB('o').system.users.getFullName()")
            .await,
        json!({"kind": "value", "value": "o.system.users"})
    );
    assert_eq!(
        js.value("NumberLong(2) + 3").await,
        json!({"kind": "value", "value": {"$numberInt": "5"}})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helpers_become_typed_bson() {
    let js = Js::new(limits());
    let v = js
        .value(
            "({id: ObjectId('65A000000000000000000001'), l: NumberLong('9223372036854775807'), \
              i: NumberInt(7), d: NumberDecimal('1.50'), t: ISODate('2024-01-02T03:04:05Z'), \
              u: UUID('0123456789abcdef0123456789abcdef'), b: BinData(0, 'AP/+Cg=='), \
              ts: Timestamp(5, 6), lo: MinKey(), hi: MaxKey(), r: /a.b/i, big: 3000000000, \
              neg: -0, n: NaN})",
        )
        .await;
    let doc = to_document(&v["docs"][0], "doc").unwrap();
    assert_eq!(
        doc.get_object_id("id").unwrap().to_hex(),
        "65a000000000000000000001"
    );
    assert_eq!(doc.get_i64("l").unwrap(), i64::MAX);
    assert_eq!(doc.get_i32("i").unwrap(), 7);
    assert!(matches!(doc.get("d"), Some(bson::Bson::Decimal128(d)) if d.to_string() == "1.50"));
    assert_eq!(
        doc.get_datetime("t").unwrap().timestamp_millis(),
        1_704_164_645_000
    );
    let uuid = doc.get_binary_generic("u").unwrap();
    assert_eq!(uuid[0], 0x01);
    assert_eq!(uuid.len(), 16);
    assert_eq!(
        doc.get_binary_generic("b").unwrap(),
        &[0x00_u8, 0xff, 0xfe, 0x0a]
    );
    assert!(
        matches!(doc.get("ts"), Some(bson::Bson::Timestamp(t)) if t.time == 5 && t.increment == 6)
    );
    assert_eq!(doc.get("lo"), Some(&bson::Bson::MinKey));
    assert_eq!(doc.get("hi"), Some(&bson::Bson::MaxKey));
    assert!(
        matches!(doc.get("r"), Some(bson::Bson::RegularExpression(r)) if r.pattern.as_str() == "a.b")
    );
    assert!((doc.get_f64("big").unwrap() - 3.0e9).abs() < 0.5);
    assert!(doc.get_f64("neg").unwrap().is_sign_negative());
    assert!(doc.get_f64("n").unwrap().is_nan());

    // Server values come back as the same helpers.
    assert_eq!(
        js.value(
            "var e = EJSON.parse('{\"_id\": {\"$oid\": \"65a000000000000000000002\"}, \
             \"n\": {\"$numberLong\": \"5\"}, \"t\": {\"$date\": {\"$numberLong\": \"0\"}}}'); \
             [e._id instanceof ObjectId, e.n instanceof NumberLong, e.t instanceof Date, \
             e.n + 1, EJSON.stringify(e)]"
        )
        .await["value"],
        json!([true, true, true, {"$numberInt": "6"},
            "{\"_id\":{\"$oid\":\"65a000000000000000000002\"},\"n\":5,\"t\":{\"$date\":\"1970-01-01T00:00:00.000Z\"}}"])
    );
    // Bad helper input is a TypeError with the statement line.
    let e = js.error("1\nObjectId('xyz')").await;
    assert_eq!(e.sql_code(), Some("TypeError"), "{e}");
    assert!(
        e.to_string().contains("24 hexadecimal digits (line 2)"),
        "{e}"
    );
    assert!(
        js.error("NumberLong('9223372036854775808')")
            .await
            .to_string()
            .contains("64-bit integer")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn printing_makes_notices() {
    let js = Js::new(limits());
    let outcome = js
        .run("print('a', 1, {b: 2}); printjson({c: NumberLong(3)}); console.log('z')")
        .await;
    let texts: Vec<&str> = outcome.notices.iter().map(|n| n.message.as_str()).collect();
    assert_eq!(texts, ["a 1 {\"b\":2}", "{\n  \"c\": 3\n}", "z"]);
    assert_eq!(outcome.result.unwrap(), r#"{"kind":"none"}"#);
    // Output printed before an error is kept.
    let outcome = js.run("print('before'); undefinedThing()").await;
    assert_eq!(outcome.notices.len(), 1);
    let e = outcome.result.unwrap_err();
    assert_eq!(e.sql_code(), Some("ReferenceError"), "{e}");
    assert!(e.to_string().contains("(line 1)"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sandbox_has_no_host_access() {
    let js = Js::new(limits());
    assert_eq!(
        js.value(
            "[typeof require, typeof load, typeof process, typeof fetch, typeof std, \
             typeof os, typeof import_meta, typeof XMLHttpRequest, typeof setTimeout, \
             typeof __oxbase_host, typeof Mongo, typeof connect, typeof quit]"
        )
        .await["value"],
        Json::Array(vec![json!("undefined"); 13])
    );
    let e = js.error("require('fs')").await;
    assert_eq!(e.sql_code(), Some("ReferenceError"), "{e}");
    let e = js.error("import fs from 'fs'").await;
    assert_eq!(e.sql_code(), Some("SyntaxError"), "{e}");
    let e = js.error("load('/etc/passwd')").await;
    assert_eq!(e.sql_code(), Some("ReferenceError"), "{e}");
    // The hidden hooks cannot be replaced.
    assert_eq!(
        js.value("__oxbase_describe = 1; 2").await["value"],
        json!({"$numberInt": "2"})
    );
    assert_eq!(js.value("({a: 1})").await["kind"], json!("documents"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_endless_loop_is_interrupted_by_cancel() {
    let js = Js::new(limits());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let outcome = js.run_with("while (true) {}", cancel).await;
    assert!(
        matches!(outcome.result, Err(Error::Cancelled)),
        "{:?}",
        outcome.result
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    // `try` cannot swallow the interrupt, and the session keeps working.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let outcome = js
        .run_with("try { while (true) {} } catch (e) {} 5", cancel)
        .await;
    assert!(matches!(outcome.result, Err(Error::Cancelled)));
    assert_eq!(js.value("6").await["value"], json!({"$numberInt": "6"}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sleep_is_interrupted_by_cancel() {
    let js = Js::new(limits());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let outcome = js.run_with("sleep(9000); 1", cancel).await;
    assert!(
        matches!(outcome.result, Err(Error::Cancelled)),
        "{:?}",
        outcome.result
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        js.error("sleep(20000)")
            .await
            .to_string()
            .contains("10000 ms")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cpu_limit_stops_a_loop_but_not_waiting() {
    let js = Js::new(ShellLimits {
        time_limit: Some(Duration::from_millis(300)),
        ..limits()
    });
    let started = Instant::now();
    let e = js.error("for (;;) {}").await;
    assert!(matches!(e, Error::Timeout { .. }), "{e}");
    assert!(e.to_string().contains("300 ms"), "{e}");
    assert!(started.elapsed() < Duration::from_secs(3));
    // Waiting (sleep) does not count as CPU time.
    assert_eq!(
        js.value("sleep(500); 7").await["value"],
        json!({"$numberInt": "7"})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_memory_limit_stops_an_allocation_loop_and_resets_the_session() {
    let js = Js::new(ShellLimits {
        memory_bytes: 16 * 1024 * 1024,
        ..limits()
    });
    assert_eq!(js.value("var kept = 1").await, json!({"kind": "none"}));
    let e = js
        .error("var a = []; while (true) { a.push(new Array(100000).fill(1)); }")
        .await;
    assert_eq!(e.sql_code(), Some("OutOfMemory"), "{e}");
    assert!(e.to_string().contains("16 MiB"), "{e}");
    // A fresh runtime: works, without the old globals.
    assert_eq!(js.value("typeof kept").await["value"], json!("undefined"));
    // Deep recursion is an error, not a crash.
    let e = js
        .error("function f(n) { return f(n + 1) + 1; } f(0)")
        .await;
    assert!(matches!(e, Error::Sql { .. }), "{e}");
    assert_eq!(js.value("1").await["kind"], json!("value"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursors_become_requests_and_db_assignment_switches() {
    let js = Js::new(limits());
    let v = js
        .value("db.m7_big.find({g: 7}, {g: 1}).sort({_id: 1}).skip(2).limit(-5).maxTimeMS(900)")
        .await;
    let Output::Request(DocumentQuery::Find(f)) = parse_output(&v.to_string()).unwrap() else {
        panic!("a find request: {v}");
    };
    assert_eq!(f.collection.catalog.as_deref(), Some("test"));
    assert_eq!(f.collection.name, "m7_big");
    assert_eq!(f.filter, Some(json!({"g": {"$numberInt": "7"}})));
    assert_eq!(f.projection, Some(json!({"g": {"$numberInt": "1"}})));
    assert_eq!(f.sort, Some(json!({"_id": {"$numberInt": "1"}})));
    assert_eq!(
        (f.skip, f.limit, f.max_time_ms),
        (Some(2), Some(5), Some(900))
    );

    let v = js
        .value("db.getSiblingDB('o').c.aggregate([{$match: {a: {$regex: '^x'}}}], {allowDiskUse: true})")
        .await;
    let Output::Request(DocumentQuery::Aggregate(a)) = parse_output(&v.to_string()).unwrap() else {
        panic!("an aggregate request: {v}");
    };
    assert_eq!(a.collection.catalog.as_deref(), Some("o"));
    assert_eq!(a.pipeline, vec![json!({"$match": {"a": {"$regex": "^x"}}})]);
    assert!(a.allow_disk_use);

    let e = js.error("db.c.aggregate([]).limit(1)").await;
    assert!(e.to_string().contains("not available on aggregate"), "{e}");

    let outcome = js.run("db = db.getSiblingDB('other'); db.getName()").await;
    assert_eq!(outcome.switched.as_deref(), Some("other"));
    assert_eq!(
        outcome.result.unwrap(),
        r#"{"kind":"value","value":"other"}"#
    );
    // The next statement starts on the database the job names (the connection's current one).
    assert_eq!(js.value("db.getName()").await["value"], json!("test"));
    let e = js.error("db = 5").await;
    assert_eq!(e.sql_code(), Some("TypeError"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_errors_keep_their_category() {
    // Nothing listens: the server call fails with a connection error the script may catch.
    let js = Js::new(limits());
    let e = js.error("db.c.findOne()").await;
    assert!(matches!(e, Error::Connection { .. }), "{e}");
    assert_eq!(
        js.value("try { db.c.findOne() } catch (e) { e.name }")
            .await["value"],
        json!("ConnectionError")
    );
}

#[test]
fn outputs_parse() {
    assert!(matches!(
        parse_output(r#"{"kind":"none"}"#).unwrap(),
        Output::None
    ));
    let Output::Write { affected, row } = parse_output(
        r#"{"kind":"write","affected":2,"row":{"acknowledged":true,"modifiedCount":{"$numberInt":"2"}}}"#,
    )
    .unwrap() else {
        panic!("write");
    };
    assert_eq!(affected, 2);
    assert_eq!(row.get_i32("modifiedCount").unwrap(), 2);
    assert!(parse_output("not json").is_err());
    assert_eq!(count(Some(&json!(-1))), None);
    assert_eq!(count(Some(&json!(3.0))), Some(3));
}
