//! Live tests through PgBouncer in front of the `postgres` fixture
//! (`scripts/fixtures-up.sh postgres pgbouncer pgbouncer-legacy`; tests/docker/pgbouncer):
//!
//! - `pgbouncer`, a current release in transaction pooling. It refuses an untracked parameter in
//!   the startup `options` ("unsupported startup parameter in options: bytea_output", which the
//!   driver sent before 0.8.2); the driver's defaults (`DateStyle`, `standard_conforming_strings`)
//!   are tracked, so they pass.
//! - `pgbouncer-legacy`, a release before 1.20, in session pooling. It refuses the startup
//!   `options` parameter altogether, so the driver connects again without its defaults and sets
//!   them with `SET`.
//!
//! On both: the session has the settings the decoder needs, values decode in the text protocol
//! (dates, `bytea` in both output formats) and statements with bound parameters (the extended
//! protocol) run through the pooler.
//!
//! All are `#[ignore]`d: run them with
//! `cargo test -p oxbase-pg --test pgbouncer_live -- --include-ignored --test-threads 1`.

use std::path::PathBuf;

use chrono::NaiveDate;
use oxbase_driver::{
    CancellationToken, Collected, ConnectParams, Connection, Driver, TlsMode, TlsOptions,
    cell_value, collect,
};
use oxbase_pg::PgDriver;
use oxbase_types::{Engine, Error, Value};

/// `[engines.postgres.<key>]` of tests/fixtures.toml, with the server's credentials.
fn params(key: &str) -> ConnectParams {
    let path = PathBuf::from(std::env::var("OXBASE_FIXTURES").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures.toml").into()
    }));
    let text = std::fs::read_to_string(&path).expect("read tests/fixtures.toml");
    let doc: toml::Table = text.parse().expect("parse tests/fixtures.toml");
    let pg = doc["engines"]["postgres"]
        .as_table()
        .expect("[engines.postgres]");
    let bouncer = pg[key]
        .as_table()
        .unwrap_or_else(|| panic!("[engines.postgres.{key}]"));
    let s = |t: &toml::Table, k: &str| t[k].as_str().expect(k).to_owned();
    let port = u16::try_from(bouncer["port"].as_integer().expect("port")).expect("port");
    ConnectParams::network(Engine::Postgres, s(bouncer, "host"))
        .with_port(port)
        .with_user(s(pg, "user"))
        .with_password(s(pg, "password"))
        .with_database(s(pg, "database"))
        .with_tls(TlsOptions::new(TlsMode::Disable))
}

async fn connect(key: &str, params: &ConnectParams) -> Box<dyn Connection> {
    PgDriver
        .connect(params)
        .await
        .unwrap_or_else(|e| panic!("{key}: connect failed: {e}"))
}

async fn connect_err(key: &str, params: &ConnectParams) -> Error {
    match PgDriver.connect(params).await {
        Ok(_) => panic!("{key}: connect should have failed"),
        Err(e) => e,
    }
}

async fn run(conn: &dyn Connection, sql: &str, params: &[Value]) -> Collected {
    let stream = conn
        .execute(sql, params, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("execute {sql}: {e}"));
    collect(stream)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// The values of the first row of the last result set.
fn row(out: &Collected) -> Vec<Value> {
    let set = out.result_sets.last().expect("a result set");
    let batch = set.batches.first().expect("a batch");
    (0..batch.num_columns())
        .map(|c| cell_value(batch.column(c).as_ref(), 0).expect("cell"))
        .collect()
}

fn text(v: &Value) -> &str {
    match v {
        Value::Text(s) => s,
        other => panic!("not text: {other:?}"),
    }
}

/// What every session through a pooler must do: the decoder's settings are in place, values
/// decode in the text protocol, and bound parameters run.
async fn check_session(key: &str, conn: &dyn Connection) {
    // Text protocol (no parameters): dates need DateStyle ISO.
    let out = run(
        conn,
        "SELECT DATE '2026-11-04' AS d, '\\x00ff10'::bytea AS b, \
         current_setting('DateStyle') AS ds, \
         current_setting('standard_conforming_strings') AS scs",
        &[],
    )
    .await;
    let v = row(&out);
    assert_eq!(
        v[0],
        Value::Date(NaiveDate::from_ymd_opt(2026, 11, 4).unwrap()),
        "{key}"
    );
    assert_eq!(v[1], Value::Bytes(vec![0, 255, 16]), "{key}");
    assert!(
        text(&v[2]).starts_with("ISO"),
        "{key}: DateStyle {:?}",
        v[2]
    );
    assert_eq!(text(&v[3]), "on", "{key}: standard_conforming_strings");

    // The driver no longer pins bytea_output: the escape format decodes too. One script, so the
    // SET and the SELECT share a server connection even in transaction pooling.
    let out = run(
        conn,
        "SET bytea_output = escape; SELECT '\\x00ff10'::bytea AS b; RESET bytea_output",
        &[],
    )
    .await;
    let set = out
        .result_sets
        .iter()
        .find(|s| !s.batches.is_empty())
        .expect("the SELECT's result set");
    let b = cell_value(set.batches[0].column(0).as_ref(), 0).expect("cell");
    assert_eq!(b, Value::Bytes(vec![0, 255, 16]), "{key}: escape format");

    // Extended protocol (bound parameters), several times: prepared statements through the
    // pooler.
    for i in 0..3_i64 {
        let out = run(
            conn,
            "SELECT $1::int8 + 1 AS n, $2::text AS t",
            &[Value::Int(40 + i), Value::Text(format!("run {i}"))],
        )
        .await;
        let v = row(&out);
        assert_eq!(v[0], Value::Int(41 + i), "{key}: run {i}");
        assert_eq!(v[1], Value::Text(format!("run {i}")), "{key}: run {i}");
    }
}

#[tokio::test]
#[ignore = "needs the postgres and pgbouncer fixtures"]
async fn a_current_pgbouncer_in_transaction_pooling_accepts_the_driver_defaults() {
    let key = "pgbouncer";
    let p = params(key);
    let conn = connect(key, &p).await;
    check_session(key, conn.as_ref()).await;
    conn.close().await.unwrap();

    // The fixture is the case that broke: an untracked parameter in the startup options is
    // refused. A profile's own `options` are still sent as they are, so that stays an error.
    let mut own = p.clone();
    own.options
        .insert("options".into(), "-c bytea_output=hex".into());
    let err = connect_err(key, &own).await;
    assert!(
        err.to_string()
            .contains("unsupported startup parameter in options: bytea_output"),
        "{key}: {err}"
    );
}

#[tokio::test]
#[ignore = "needs the postgres and pgbouncer-legacy fixtures"]
async fn a_pgbouncer_that_refuses_startup_options_gets_the_defaults_by_set() {
    let key = "pgbouncer_legacy";
    let p = params(key);
    // The fixture refuses the startup `options` parameter altogether: a profile's own options
    // are not dropped by the retry, so they fail with the pooler's message.
    let mut own = p.clone();
    own.options
        .insert("options".into(), "-c DateStyle=ISO".into());
    let err = connect_err(key, &own).await;
    assert!(
        err.to_string().contains("unsupported startup parameter"),
        "{key}: {err}"
    );

    // Without them, the driver's own defaults are retried without `options` and set with SET.
    let conn = connect(key, &p).await;
    check_session(key, conn.as_ref()).await;
    conn.close().await.unwrap();
}
