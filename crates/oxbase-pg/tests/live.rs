//! Live tests against the `postgres` fixture (`scripts/fixtures-up.sh postgres ssh`; the
//! tunnel test also needs the `ssh` bastion).
//!
//! All are `#[ignore]`d: run them with
//! `cargo test -p oxbase-pg --test live -- --include-ignored --test-threads 1`.
//! Connection details come from `tests/fixtures.toml` (`$OXBASE_FIXTURES`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use arrow_array::Array;
use arrow_array_check::{column_i32, column_str};
use futures::StreamExt;
use oxbase_driver::arrow::DataType;
use oxbase_driver::{
    CancellationToken, ConnectParams, Connection, Driver, NoticeSeverity, PemSource, ResultItem,
    TlsMode, TlsOptions, collect,
};
use oxbase_net::ssh::{
    KeySource, KnownHostsPolicy, RusshTunnelProvider, SshAuth, SshOptions, TunnelHandle,
    TunnelProvider,
};
use oxbase_pg::PgDriver;
use oxbase_types::{Endpoint, Engine, Error, Value};

struct Fixture {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
    tls_user: String,
    tls_password: String,
    ca_cert: PathBuf,
    ssh: SshFixture,
}

struct SshFixture {
    host: String,
    port: u16,
    user: String,
    client_key: PathBuf,
    known_hosts: PathBuf,
    /// Where the bastion reaches PostgreSQL (`[ssh.targets] postgres`).
    target: Endpoint,
}

fn fixture() -> Fixture {
    let path = PathBuf::from(std::env::var("OXBASE_FIXTURES").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures.toml").into()
    }));
    let text = std::fs::read_to_string(&path).expect("read tests/fixtures.toml");
    let doc: toml::Table = text.parse().expect("parse tests/fixtures.toml");
    let root = path
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf();
    let pg = doc["engines"]["postgres"]
        .as_table()
        .expect("[engines.postgres]");
    let s = |t: &toml::Table, k: &str| t[k].as_str().expect(k).to_owned();
    let tls_user = pg["tls_user"]
        .as_table()
        .expect("[engines.postgres.tls_user]");
    let tls = doc["tls"].as_table().expect("[tls]");
    let ssh = doc["ssh"].as_table().expect("[ssh]");
    let key_dir = root.join(s(ssh, "key_dir"));
    let target = ssh["targets"]["postgres"]
        .as_table()
        .expect("[ssh.targets] postgres");
    let port =
        |t: &toml::Table| u16::try_from(t["port"].as_integer().expect("port")).expect("port");
    let ssh = SshFixture {
        host: s(ssh, "host"),
        port: port(ssh),
        user: s(ssh, "user"),
        client_key: key_dir.join(s(ssh, "client_key")),
        known_hosts: key_dir.join(s(ssh, "known_hosts")),
        target: Endpoint::new(s(target, "host"), port(target)),
    };
    Fixture {
        host: s(pg, "host"),
        port: u16::try_from(pg["port"].as_integer().expect("port")).expect("port"),
        user: s(pg, "user"),
        password: s(pg, "password"),
        database: s(pg, "database"),
        tls_user: s(tls_user, "user"),
        tls_password: s(tls_user, "password"),
        ca_cert: root.join(s(tls, "dir")).join(s(tls, "ca_cert")),
        ssh,
    }
}

fn plain_params(f: &Fixture) -> ConnectParams {
    ConnectParams::network(Engine::Postgres, &f.host)
        .with_port(f.port)
        .with_user(&f.user)
        .with_password(f.password.as_str())
        .with_database(&f.database)
        .with_tls(TlsOptions::new(TlsMode::Disable))
}

fn tls_params(f: &Fixture, tls: TlsOptions) -> ConnectParams {
    ConnectParams::network(Engine::Postgres, &f.host)
        .with_port(f.port)
        .with_user(&f.tls_user)
        .with_password(f.tls_password.as_str())
        .with_database(&f.database)
        .with_tls(tls)
}

async fn connect(params: &ConnectParams) -> Box<dyn Connection> {
    match PgDriver.connect(params).await {
        Ok(c) => c,
        Err(e) => panic!("connect failed: {e}"),
    }
}

async fn connect_err(params: &ConnectParams) -> Error {
    match PgDriver.connect(params).await {
        Ok(_) => panic!("connect should have failed"),
        Err(e) => e,
    }
}

async fn run(conn: &dyn Connection, sql: &str, params: &[Value]) -> oxbase_driver::Collected {
    let stream = conn
        .execute(sql, params, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("execute {sql}: {e}"));
    collect(stream)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Small typed accessors for assertions.
mod arrow_array_check {
    use arrow_array::Array;
    use oxbase_driver::arrow::RecordBatch;

    pub fn column_i32(b: &RecordBatch, col: usize) -> Vec<Option<i32>> {
        let a = b
            .column(col)
            .as_any()
            .downcast_ref::<arrow_array::Int32Array>()
            .expect("Int32 column");
        a.iter().collect()
    }

    pub fn column_str(b: &RecordBatch, col: usize) -> Vec<Option<String>> {
        let a = b
            .column(col)
            .as_any()
            .downcast_ref::<arrow_array::StringArray>()
            .expect("Utf8 column");
        a.iter().map(|v| v.map(str::to_owned)).collect()
    }
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn connects_plain_and_reports_version() {
    let f = fixture();
    let conn = connect(&plain_params(&f)).await;
    assert_eq!(conn.engine(), Engine::Postgres);
    assert!(!conn.tls_in_use());
    assert_eq!(conn.server_version().major, 18, "{}", conn.server_version());
    let out = run(&*conn, "SELECT 1::int4 AS one, 'x'::text AS s", &[]).await;
    let batch = &out.result_sets[0].batches[0];
    assert_eq!(column_i32(batch, 0), [Some(1)]);
    assert_eq!(column_str(batch, 1), [Some("x".to_owned())]);
    conn.close().await.unwrap();
    conn.close().await.unwrap();
    let Err(err) = conn
        .execute("SELECT 1", &[], CancellationToken::new())
        .await
    else {
        panic!("closed connection must fail")
    };
    assert!(matches!(err, Error::Connection { .. }), "{err}");
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn wrong_password_is_an_auth_error_without_the_password() {
    let f = fixture();
    let params = plain_params(&f).with_password("Marker_Wrong_Pw_771");
    let err = connect_err(&params).await;
    assert!(matches!(err, Error::Auth { .. }), "{err}");
    assert!(!format!("{err} {err:?}").contains("Marker_Wrong_Pw_771"));
}

#[tokio::test]
#[ignore = "needs the postgres fixture with TLS (target/fixtures/tls)"]
async fn connects_with_verify_full_as_the_tls_only_user() {
    let f = fixture();
    assert!(
        f.ca_cert.exists(),
        "fixture CA missing at {}: start the fixtures with TLS",
        f.ca_cert.display()
    );
    let ca = PemSource::File(f.ca_cert.clone());
    let verify_full = TlsOptions::new(TlsMode::VerifyFull).with_ca_cert(ca.clone());
    let conn = connect(&tls_params(&f, verify_full.clone())).await;
    assert!(conn.tls_in_use());
    let out = run(
        &*conn,
        "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
        &[],
    )
    .await;
    let batch = &out.result_sets[0].batches[0];
    let ssl = batch
        .column(0)
        .as_any()
        .downcast_ref::<arrow_array::BooleanArray>()
        .expect("bool");
    assert!(ssl.value(0));

    // The cancel request uses the session's TLS settings.
    let stream = conn
        .execute("SELECT pg_sleep(30)", &[], CancellationToken::new())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t0 = Instant::now();
    conn.cancel_running().await.unwrap();
    let err = collect(stream).await.unwrap_err();
    assert!(err.is_cancelled(), "{err}");
    assert!(t0.elapsed() < Duration::from_secs(2));
    run(&*conn, "SELECT 1", &[]).await;

    // "localhost" is also in the certificate.
    let mut by_name = tls_params(&f, verify_full.clone());
    by_name.host = Some("localhost".into());
    assert!(connect(&by_name).await.tls_in_use());

    // A name the certificate does not carry fails VerifyFull but passes VerifyCa.
    let wrong = verify_full.clone().with_server_name("wrong.example");
    let err = connect_err(&tls_params(&f, wrong)).await;
    assert!(err.to_string().contains("TLS"), "{err}");
    let ca_only = TlsOptions::new(TlsMode::VerifyCa)
        .with_ca_cert(ca)
        .with_server_name("wrong.example");
    assert!(connect(&tls_params(&f, ca_only)).await.tls_in_use());

    // The TLS-only user cannot connect in plaintext.
    let err = connect_err(&tls_params(&f, TlsOptions::new(TlsMode::Disable))).await;
    assert!(matches!(err, Error::Auth { .. }), "{err}");
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn streams_large_results_in_batches() {
    let f = fixture();
    let conn = connect(&plain_params(&f)).await;
    let mut stream = conn
        .execute(
            "SELECT g, g::text AS s FROM generate_series(1, 200000) AS g",
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut schemas = 0;
    let mut batches = 0;
    let mut rows = 0;
    let mut max_batch = 0;
    let mut last = None;
    while let Some(item) = stream.next().await {
        match item.unwrap() {
            ResultItem::Schema(s) => {
                schemas += 1;
                assert_eq!(s.field(0).data_type(), &DataType::Int32);
                assert_eq!(s.field(1).data_type(), &DataType::Utf8);
                assert_eq!(
                    s.field(0)
                        .metadata()
                        .get("oxbase.pg.type")
                        .map(String::as_str),
                    Some("int4")
                );
            }
            ResultItem::Batch(b) => {
                batches += 1;
                rows += b.num_rows();
                max_batch = max_batch.max(b.num_rows());
                last = column_i32(&b, 0).last().copied().flatten();
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(schemas, 1);
    assert_eq!(rows, 200_000);
    assert!(batches > 1, "streamed in {batches} batch(es)");
    assert!(max_batch <= oxbase_pg::BATCH_ROWS);
    assert_eq!(last, Some(200_000));
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn cancels_a_sleeping_statement_quickly() {
    let f = fixture();
    let conn = connect(&plain_params(&f)).await;

    // cancel_running
    let stream = conn
        .execute("SELECT pg_sleep(30)", &[], CancellationToken::new())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t0 = Instant::now();
    conn.cancel_running().await.unwrap();
    let err = collect(stream).await.unwrap_err();
    assert!(err.is_cancelled(), "{err}");
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    run(&*conn, "SELECT 1", &[]).await;

    // the execute token
    let token = CancellationToken::new();
    let stream = conn
        .execute("SELECT pg_sleep(30)", &[], token.clone())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t0 = Instant::now();
    token.cancel();
    let err = collect(stream).await.unwrap_err();
    assert!(err.is_cancelled(), "{err}");
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    // The session survives and the server really stopped (no 30 s wait for the next statement).
    let t0 = Instant::now();
    let out = run(&*conn, "SELECT 2::int4", &[]).await;
    assert_eq!(column_i32(&out.result_sets[0].batches[0], 0), [Some(2)]);
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());

    // An already-cancelled token sends nothing.
    let done = CancellationToken::new();
    done.cancel();
    let Err(err) = conn.execute("SELECT 1", &[], done).await else {
        panic!("must not run")
    };
    assert!(err.is_cancelled());

    // Dropping a stream mid-way abandons (and cancels) the statement.
    let mut stream = conn
        .execute(
            "SELECT g FROM generate_series(1, 50000000) g",
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let _first = stream.next().await;
    drop(stream);
    let t0 = Instant::now();
    run(&*conn, "SELECT 1", &[]).await;
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn captures_notices() {
    let f = fixture();
    let conn = connect(&plain_params(&f)).await;
    let out = run(
        &*conn,
        "DO $$ BEGIN RAISE NOTICE 'hello %', 42; RAISE WARNING 'careful'; END $$",
        &[],
    )
    .await;
    let messages: Vec<_> = out.notices.iter().map(|n| n.message.as_str()).collect();
    assert!(messages.contains(&"hello 42"), "{messages:?}");
    let warning = out
        .notices
        .iter()
        .find(|n| n.message == "careful")
        .expect("warning");
    assert_eq!(warning.severity, NoticeSeverity::Warning);
    assert_eq!(warning.code.as_deref(), Some("01000"));

    // Notices come before the rows of the statement that raised them.
    run(
        &*conn,
        "CREATE OR REPLACE FUNCTION pg_temp.noisy() RETURNS int LANGUAGE plpgsql AS \
         $$ BEGIN RAISE NOTICE 'from noisy'; RETURN 7; END $$",
        &[],
    )
    .await;
    let stream = conn
        .execute("SELECT pg_temp.noisy()", &[], CancellationToken::new())
        .await
        .unwrap();
    let items: Vec<_> = stream.collect().await;
    let notice_at = items
        .iter()
        .position(|i| matches!(i, Ok(ResultItem::Notice(n)) if n.message == "from noisy"))
        .expect("notice");
    let batch_at = items
        .iter()
        .position(|i| matches!(i, Ok(ResultItem::Batch(_))))
        .expect("batch");
    assert!(notice_at < batch_at, "{items:?}");
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn transactions_commit_and_roll_back() {
    let f = fixture();
    let conn = connect(&plain_params(&f)).await;
    run(&*conn, "CREATE TEMP TABLE tx_probe (a int)", &[]).await;

    let tx = conn.begin().await.unwrap();
    assert!(conn.begin().await.is_err(), "one transaction at a time");
    let out = collect(
        tx.execute(
            "INSERT INTO tx_probe VALUES (1), (2)",
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(out.rows_affected, [2]);
    tx.rollback().await.unwrap();
    let count = run(&*conn, "SELECT count(*)::int4 FROM tx_probe", &[]).await;
    assert_eq!(column_i32(&count.result_sets[0].batches[0], 0), [Some(0)]);

    let tx = conn.begin().await.unwrap();
    collect(
        tx.execute(
            "INSERT INTO tx_probe VALUES ($1)",
            &[Value::Int(5)],
            CancellationToken::new(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // Dropping a transaction rolls it back before the next statement.
    let tx = conn.begin().await.unwrap();
    collect(
        tx.execute(
            "INSERT INTO tx_probe VALUES (9)",
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    drop(tx);
    let rows = run(&*conn, "SELECT a FROM tx_probe ORDER BY a", &[]).await;
    assert_eq!(column_i32(&rows.result_sets[0].batches[0], 0), [Some(5)]);
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn maps_types_on_both_protocols() {
    let f = fixture();
    let conn = connect(&plain_params(&f)).await;
    let sql = "SELECT true AS b, 2::int2 AS i2, 3::int8 AS i8, 1.5::float4 AS f4, \
               123.45::numeric(10,2) AS dec, 1.5::numeric AS num, 'x'::varchar AS v, \
               '\\x0102'::bytea AS by, '2024-01-15'::date AS d, '12:00'::time AS t, \
               '2024-01-15 10:00'::timestamp AS ts, '2024-01-15 10:00+02'::timestamptz AS tz, \
               '1 day'::interval AS iv, 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid AS u, \
               '{\"a\": 1}'::jsonb AS j, ARRAY[1,2] AS arr, '10.0.0.1'::inet AS ip, \
               'infinity'::timestamp AS inf";
    let expected = [
        DataType::Boolean,
        DataType::Int16,
        DataType::Int64,
        DataType::Float32,
        DataType::Decimal128(10, 2),
        DataType::Utf8,
        DataType::Utf8,
        DataType::Binary,
        DataType::Date32,
        DataType::Time64(arrow_schema::TimeUnit::Microsecond),
        DataType::Timestamp(arrow_schema::TimeUnit::Microsecond, None),
        DataType::Timestamp(arrow_schema::TimeUnit::Microsecond, Some("UTC".into())),
        DataType::Utf8,
        DataType::FixedSizeBinary(16),
        DataType::Utf8,
        DataType::Utf8,
        DataType::Utf8,
        DataType::Timestamp(arrow_schema::TimeUnit::Microsecond, None),
    ];
    // Text (no parameters) and binary (with a parameter) paths give the same result.
    for (query, params) in [
        (sql.to_owned(), vec![]),
        (format!("{sql}, $1::int4 AS p"), vec![Value::Int(7)]),
    ] {
        let out = run(&*conn, &query, &params).await;
        let set = &out.result_sets[0];
        for (i, want) in expected.iter().enumerate() {
            assert_eq!(
                set.schema.field(i).data_type(),
                want,
                "column {i} of {query}"
            );
        }
        let b = &set.batches[0];
        assert_eq!(column_str(b, 5), [Some("1.5".to_owned())]);
        assert_eq!(column_str(b, 12), [Some("1 day".to_owned())]);
        assert_eq!(column_str(b, 14), [Some("{\"a\": 1}".to_owned())]);
        assert_eq!(column_str(b, 15), [Some("{1,2}".to_owned())]);
        assert_eq!(column_str(b, 16), [Some("10.0.0.1".to_owned())]);
        assert!(b.column(17).is_null(0));
        assert_eq!(out.notices.len(), 1, "{:?}", out.notices);
        let tz = b
            .column(11)
            .as_any()
            .downcast_ref::<arrow_array::TimestampMicrosecondArray>()
            .unwrap();
        assert_eq!(tz.value(0), 1_705_305_600_000_000);
    }
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn scripts_dml_and_errors() {
    let f = fixture();
    let conn = connect(&plain_params(&f)).await;
    let out = run(
        &*conn,
        "CREATE TEMP TABLE s (a int); INSERT INTO s VALUES (1),(2),(3); SELECT a FROM s; \
         UPDATE s SET a = a + 1 WHERE a > 1",
        &[],
    )
    .await;
    assert_eq!(out.result_sets.len(), 1);
    assert_eq!(
        out.result_sets[0].schema.field(0).data_type(),
        &DataType::Utf8
    );
    assert_eq!(out.rows_affected, [0, 3, 2]);

    let out = run(&*conn, "DELETE FROM s WHERE a = $1", &[Value::Int(1)]).await;
    assert_eq!(out.rows_affected, [1]);

    let err = collect(
        conn.execute("SELECT * FROM no_such_table", &[], CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.sql_code(), Some("42P01"), "{err}");

    let err = collect(
        conn.execute("SELECT $1::int", &[], CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Sql { .. }), "{err}");
    let err = collect(
        conn.execute(
            "SELECT $1::int",
            &[Value::Int(1), Value::Int(2)],
            CancellationToken::new(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::InvalidInput { .. }), "{err}");
    // The session is still usable after errors.
    run(&*conn, "SELECT 1", &[]).await;
    assert!(
        run(&*conn, "  -- nothing\n", &[])
            .await
            .result_sets
            .is_empty()
    );
}

/// The next item, failing the test when it takes longer than `limit`.
async fn next_within(
    stream: &mut oxbase_driver::ResultStream,
    limit: Duration,
) -> Option<oxbase_types::Result<ResultItem>> {
    tokio::time::timeout(limit, stream.next())
        .await
        .expect("the stream did not move on in time")
}

/// Asserts the stream yields `Err(Cancelled)` and then ends, both promptly.
async fn assert_cancelled_then_ends(mut stream: oxbase_driver::ResultStream) {
    let limit = Duration::from_secs(2);
    match next_within(&mut stream, limit).await {
        Some(Err(e)) => assert!(e.is_cancelled(), "{e}"),
        other => panic!("expected Err(Cancelled), got {other:?}"),
    }
    assert!(
        next_within(&mut stream, limit).await.is_none(),
        "the stream must end right after Err(Cancelled)"
    );
}

/// `SELECT 1` must answer quickly: the server is no longer busy with a cancelled statement.
async fn assert_idle(conn: &dyn Connection) {
    let t0 = Instant::now();
    let out = run(conn, "SELECT 1::int4", &[]).await;
    assert_eq!(column_i32(&out.result_sets[0].batches[0], 0), [Some(1)]);
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
}

/// Cancel with no pause after `execute`, for queued statements, and checks that cancel landing
/// before the statement reaches the server still stops all server-side work.
async fn check_immediate_cancels(conn: &dyn Connection) {
    // cancel_running right after execute (the job may not have taken the lock yet).
    for params in [vec![], vec![Value::Int(30)]] {
        let sql = if params.is_empty() {
            "SELECT pg_sleep(30)"
        } else {
            "SELECT pg_sleep($1::int4)"
        };
        let stream = conn
            .execute(sql, &params, CancellationToken::new())
            .await
            .unwrap();
        conn.cancel_running().await.unwrap();
        assert_cancelled_then_ends(stream).await;
        assert_idle(conn).await;
    }

    // The execute token, right after execute.
    let token = CancellationToken::new();
    let stream = conn
        .execute("SELECT pg_sleep(30)", &[], token.clone())
        .await
        .unwrap();
    token.cancel();
    assert_cancelled_then_ends(stream).await;
    assert_idle(conn).await;

    // A short pause, so the cancel lands while the statement is in flight.
    for pause_ms in [1, 5, 20, 50] {
        let stream = conn
            .execute("SELECT pg_sleep(30)", &[], CancellationToken::new())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(pause_ms)).await;
        conn.cancel_running().await.unwrap();
        assert_cancelled_then_ends(stream).await;
        assert_idle(conn).await;
    }

    // A queued statement is cancelled too, without ever running.
    let first = conn
        .execute("SELECT pg_sleep(30)", &[], CancellationToken::new())
        .await
        .unwrap();
    let queued = conn
        .execute("SELECT pg_sleep(30)", &[], CancellationToken::new())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let t0 = Instant::now();
    conn.cancel_running().await.unwrap();
    assert_cancelled_then_ends(first).await;
    assert_cancelled_then_ends(queued).await;
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    // A statement submitted after cancel_running is not affected.
    assert_idle(conn).await;
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn cancels_right_after_execute_and_queued_statements() {
    let f = fixture();
    let conn = connect(&plain_params(&f)).await;
    check_immediate_cancels(&*conn).await;
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn statement_timeout_is_an_sql_error_not_a_cancel() {
    let f = fixture();
    let conn = connect(&plain_params(&f)).await;
    run(&*conn, "SET statement_timeout = '100ms'", &[]).await;
    let err = collect(
        conn.execute("SELECT pg_sleep(5)", &[], CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap_err();
    assert!(!err.is_cancelled(), "{err}");
    assert_eq!(err.sql_code(), Some("57014"), "{err}");
    assert!(err.to_string().contains("statement timeout"), "{err}");
    run(&*conn, "RESET statement_timeout", &[]).await;
    assert_idle(&*conn).await;
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn syntax_errors_are_reported_once() {
    let f = fixture();
    let conn = connect(&plain_params(&f)).await;
    // The prepare fails with 42601, the statement is retried as a script (in case the server
    // saw several statements) and the server reports the syntax error once, from the retry.
    let items: Vec<_> = conn
        .execute("SELEC 1", &[], CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(items.len(), 1, "{items:?}");
    match &items[0] {
        Err(e) => assert_eq!(e.sql_code(), Some("42601"), "{e}"),
        Ok(other) => panic!("expected a syntax error, got {other:?}"),
    }
    assert_idle(&*conn).await;
}

async fn open_tunnel(f: &Fixture) -> TunnelHandle {
    assert!(
        f.ssh.known_hosts.exists(),
        "fixture SSH keys missing at {}: start the fixtures with `scripts/fixtures-up.sh postgres ssh`",
        f.ssh.known_hosts.display()
    );
    let ssh = SshOptions::new(
        f.ssh.host.clone(),
        f.ssh.user.clone(),
        SshAuth::PrivateKey {
            key: KeySource::File(f.ssh.client_key.clone()),
            passphrase: None,
        },
    )
    .with_port(f.ssh.port)
    .with_known_hosts(KnownHostsPolicy::Strict {
        path: Some(f.ssh.known_hosts.clone()),
    });
    RusshTunnelProvider::new()
        .open(&ssh, &f.ssh.target, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("SSH tunnel: {e}"))
}

#[tokio::test]
#[ignore = "needs the postgres and ssh fixtures (scripts/fixtures-up.sh postgres ssh)"]
async fn connects_and_cancels_through_an_ssh_tunnel() {
    let f = fixture();
    let tunnel = open_tunnel(&f).await;
    let local = tunnel.local_addr();
    assert!(local.ip().is_loopback());
    // What the connection manager does: the server endpoint stays the one the user configured
    // (as the bastion resolves it), the driver dials the tunnel.
    let through = |p: ConnectParams| {
        let mut p = p.with_dial_addr(local);
        p.host = Some(f.ssh.target.host.clone());
        p.port = Some(f.ssh.target.port);
        p
    };

    // TLS VerifyFull through the tunnel: verified against the server name, not 127.0.0.1.
    let ca = PemSource::File(f.ca_cert.clone());
    let verify_full = TlsOptions::new(TlsMode::VerifyFull).with_ca_cert(ca);
    let conn = connect(&through(tls_params(&f, verify_full.clone()))).await;
    assert!(conn.tls_in_use());
    let out = run(
        &*conn,
        "SELECT ssl::text, inet_server_port()::int4 FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
        &[],
    )
    .await;
    let batch = &out.result_sets[0].batches[0];
    assert_eq!(column_str(batch, 0), [Some("true".to_owned())]);
    assert_eq!(column_i32(batch, 1), [Some(i32::from(f.ssh.target.port))]);

    // Cancel requests travel through the tunnel too (a second channel on the same tunnel).
    let stream = conn
        .execute("SELECT pg_sleep(30)", &[], CancellationToken::new())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t0 = Instant::now();
    conn.cancel_running().await.unwrap();
    assert_cancelled_then_ends(stream).await;
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    assert_idle(&*conn).await;
    check_immediate_cancels(&*conn).await;
    conn.close().await.unwrap();

    // A name the certificate does not carry still fails through the tunnel.
    let wrong = verify_full.with_server_name("wrong.example");
    let err = connect_err(&through(tls_params(&f, wrong))).await;
    assert!(err.to_string().contains("TLS"), "{err}");
    assert!(err.to_string().contains("via 127.0.0.1"), "{err}");

    // Plaintext through the tunnel.
    let conn = connect(&through(plain_params(&f))).await;
    assert!(!conn.tls_in_use());
    assert_idle(&*conn).await;
    let stream = conn
        .execute("SELECT pg_sleep(30)", &[], CancellationToken::new())
        .await
        .unwrap();
    conn.cancel_running().await.unwrap();
    assert_cancelled_then_ends(stream).await;
    assert_idle(&*conn).await;
    conn.close().await.unwrap();
    drop(tunnel);
}
