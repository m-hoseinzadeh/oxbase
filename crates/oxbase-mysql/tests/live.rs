//! Live tests against the `mysql` and `mariadb` fixtures (`scripts/fixtures-up.sh mysql mariadb`).
//!
//! All are `#[ignore]`d: run them with
//! `cargo test -p oxbase-mysql --test live -- --include-ignored --test-threads 2`.
//! Connection parameters come from `tests/fixtures.toml` (`$OXBASE_FIXTURES`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::StreamExt;
use oxbase_driver::{
    CancellationToken, ConnectParams, Connection, NoticeSeverity, ResultItem, collect,
};
use oxbase_mysql::{LAST_INSERT_ID_CODE, MySqlConnection, MySqlDriver, OPT_UTC_SESSION};
use oxbase_net::ssh::{RusshTunnelProvider, TunnelHandle, TunnelProvider};
use oxbase_types::{
    Endpoint, Engine, Error, KeySource, KnownHostsPolicy, PemSource, SshAuth, SshOptions, TlsMode,
    TlsOptions, Value,
};

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

fn get<'a>(t: &'a toml::Table, path: &[&str]) -> &'a toml::Value {
    let mut v: Option<&toml::Value> = None;
    let mut table = t;
    for (i, key) in path.iter().enumerate() {
        let next = table
            .get(*key)
            .unwrap_or_else(|| panic!("fixtures.toml has no {}", path.join(".")));
        if i + 1 < path.len() {
            table = next.as_table().expect("table");
        }
        v = Some(next);
    }
    v.expect("non-empty path")
}

fn s(t: &toml::Table, path: &[&str]) -> String {
    get(t, path).as_str().expect("string").to_owned()
}

fn port(t: &toml::Table, path: &[&str]) -> u16 {
    u16::try_from(get(t, path).as_integer().expect("integer")).expect("port")
}

fn engine_of(key: &str) -> Engine {
    match key {
        "mysql" => Engine::MySql,
        "mariadb" => Engine::MariaDb,
        other => panic!("not a MySQL-family fixture: {other}"),
    }
}

/// Plain parameters for the fixture's main user (TLS disabled).
fn params(key: &str) -> ConnectParams {
    let f = fixtures();
    ConnectParams::network(engine_of(key), s(&f, &["engines", key, "host"]))
        .with_port(port(&f, &["engines", key, "port"]))
        .with_user(s(&f, &["engines", key, "user"]))
        .with_password(s(&f, &["engines", key, "password"]))
        .with_database(s(&f, &["engines", key, "database"]))
        .with_tls(TlsOptions::new(TlsMode::Disable))
}

fn ca_cert() -> PemSource {
    let f = fixtures();
    let dir = in_workspace(&s(&f, &["tls", "dir"]));
    let ca = dir.join(s(&f, &["tls", "ca_cert"]));
    assert!(
        ca.exists(),
        "fixture CA {} is missing: start the fixtures with the M1 TLS PKI",
        ca.display()
    );
    PemSource::File(ca)
}

/// Parameters for the fixture's TLS-only user.
fn tls_user_params(key: &str, tls: TlsOptions) -> ConnectParams {
    let f = fixtures();
    params(key)
        .with_user(s(&f, &["engines", key, "tls_user", "user"]))
        .with_password(s(&f, &["engines", key, "tls_user", "password"]))
        .with_database(s(&f, &["engines", key, "tls_user", "database"]))
        .with_tls(tls)
}

async fn connect(p: &ConnectParams) -> MySqlConnection {
    MySqlDriver::new()
        .connect_mysql(p)
        .await
        .unwrap_or_else(|e| panic!("connect {}: {e}", p.engine))
}

async fn run(c: &dyn Connection, sql: &str) -> oxbase_driver::Collected {
    collect(
        c.execute(sql, &[], CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}")),
    )
    .await
    .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn scalar_text(c: &dyn Connection, sql: &str) -> String {
    let out = run(c, sql).await;
    let batch = &out.result_sets[0].batches[0];
    arrow_cast_text(batch.column(0).as_ref(), 0)
}

/// An integer cell of any width. MariaDB types small integer expressions (`SELECT 1`, `n + 1`,
/// `CAST(.. AS SIGNED)`) as `INT` (`Int32`), MySQL as `BIGINT` (`Int64`).
fn int_at(a: &dyn arrow_array::Array, row: usize) -> Option<i128> {
    use arrow_array::cast::AsArray;
    use arrow_array::types::{
        Int8Type, Int16Type, Int32Type, Int64Type, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
    };
    use oxbase_driver::arrow::DataType;
    if a.is_null(row) {
        return None;
    }
    Some(match a.data_type() {
        DataType::Int8 => a.as_primitive::<Int8Type>().value(row).into(),
        DataType::Int16 => a.as_primitive::<Int16Type>().value(row).into(),
        DataType::Int32 => a.as_primitive::<Int32Type>().value(row).into(),
        DataType::Int64 => a.as_primitive::<Int64Type>().value(row).into(),
        DataType::UInt8 => a.as_primitive::<UInt8Type>().value(row).into(),
        DataType::UInt16 => a.as_primitive::<UInt16Type>().value(row).into(),
        DataType::UInt32 => a.as_primitive::<UInt32Type>().value(row).into(),
        DataType::UInt64 => a.as_primitive::<UInt64Type>().value(row).into(),
        other => panic!("not an integer column: {other}"),
    })
}

fn arrow_cast_text(a: &dyn arrow_array::Array, row: usize) -> String {
    use arrow_array::cast::AsArray;
    use oxbase_driver::arrow::DataType;
    match a.data_type() {
        DataType::Utf8 => a.as_string::<i32>().value(row).to_owned(),
        DataType::Binary => String::from_utf8_lossy(a.as_binary::<i32>().value(row)).into_owned(),
        DataType::Boolean => a.as_boolean().value(row).to_string(),
        t if t.is_integer() => int_at(a, row).map_or_else(String::new, |v| v.to_string()),
        other => panic!("unexpected type {other}"),
    }
}

/// Opens a tunnel through the fixture SSH bastion (key auth, host key pinned by the fixture
/// `known_hosts`) to engine `key` inside the compose network. Returns the tunnel, the SSH options
/// and the in-network server address (`mysql:3306`, `mariadb:3306`).
async fn bastion_tunnel(key: &str) -> (TunnelHandle, SshOptions, String, u16) {
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
    let host = s(&f, &["ssh", "targets", key, "host"]);
    let target_port = port(&f, &["ssh", "targets", key, "port"]);
    let tunnel = RusshTunnelProvider::new()
        .open(
            &ssh,
            &Endpoint::new(host.clone(), target_port),
            CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("SSH tunnel to {host}:{target_port}: {e}"));
    (tunnel, ssh, host, target_port)
}

// ---- scenarios -------------------------------------------------------------------------------

async fn plain_connect(key: &str) {
    let f = fixtures();
    let c = connect(&params(key)).await;
    assert_eq!(c.engine(), engine_of(key));
    assert!(!c.tls_in_use());
    let prefix = s(&f, &["engines", key, "version_prefix"]);
    assert!(
        c.server_version().raw().starts_with(&prefix),
        "{} does not start with {prefix}",
        c.server_version().raw()
    );
    assert!(c.connection_id() > 0);
    let out = run(&c, "SELECT 1 AS one").await;
    assert_eq!(out.result_sets.len(), 1);
    assert_eq!(out.result_sets[0].num_rows(), 1);
    c.close().await.unwrap();
    c.close().await.unwrap();
    assert!(
        c.execute("SELECT 1", &[], CancellationToken::new())
            .await
            .is_err()
    );
}

async fn tls_verify_full(key: &str) {
    let full = TlsOptions::new(TlsMode::VerifyFull).with_ca_cert(ca_cert());
    // Direct, verifying 127.0.0.1 (an IP SAN of the fixture certificate).
    let c = connect(&tls_user_params(key, full.clone())).await;
    assert!(c.tls_in_use());
    let cipher = run(&c, "SHOW SESSION STATUS LIKE 'Ssl_cipher'").await;
    assert_eq!(cipher.result_sets[0].num_rows(), 1);
    c.close().await.unwrap();

    // The tunnel shape: dial 127.0.0.1, verify the service name.
    let p = tls_user_params(key, full.clone());
    let dial = std::net::SocketAddr::new([127, 0, 0, 1].into(), p.port.unwrap());
    let mut tunnelled = p.with_dial_addr(dial);
    tunnelled.host = Some(key.to_owned());
    let c = connect(&tunnelled).await;
    assert!(c.tls_in_use());
    // Cancel goes through the same dial address and TLS settings.
    cancel_sleep_on(&c).await;
    c.close().await.unwrap();

    // A wrong server name fails.
    let wrong = tls_user_params(key, full.with_server_name("not-the-server.example"));
    let err = MySqlDriver::new().connect_mysql(&wrong).await.unwrap_err();
    assert!(matches!(err, Error::Connection { .. }), "{err}");

    // VerifyCa (chain only) is refused rather than silently checking the name like VerifyFull.
    let ca_only = tls_user_params(
        key,
        TlsOptions::new(TlsMode::VerifyCa)
            .with_ca_cert(ca_cert())
            .with_server_name("not-the-server.example"),
    );
    let err = MySqlDriver::new()
        .connect_mysql(&ca_only)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported { .. }), "{err}");

    // The TLS-only user cannot log in without TLS.
    let plain = tls_user_params(key, TlsOptions::new(TlsMode::Disable));
    assert!(MySqlDriver::new().connect_mysql(&plain).await.is_err());
}

async fn tls_client_cert(key: &str) {
    let f = fixtures();
    let dir = in_workspace(&s(&f, &["tls", "dir"]));
    let tls = TlsOptions::new(TlsMode::VerifyFull)
        .with_ca_cert(ca_cert())
        .with_client_identity(
            PemSource::File(dir.join(s(&f, &["tls", "client_cert"]))),
            KeySource::File(dir.join(s(&f, &["tls", "client_key"]))),
        );
    let c = connect(&tls_user_params(key, tls)).await;
    assert!(c.tls_in_use());
    let out = run(&c, "SHOW SESSION STATUS LIKE 'Ssl_cipher'").await;
    let cipher = arrow_cast_text(out.result_sets[0].batches[0].column(1).as_ref(), 0);
    assert!(!cipher.is_empty(), "no TLS cipher reported");
    c.close().await.unwrap();
}

async fn ssh_tunnel(key: &str) {
    let (tunnel, ssh, host, target_port) = bastion_tunnel(key).await;
    let through = |p: ConnectParams| {
        let mut p = p.with_ssh(ssh.clone()).with_dial_addr(tunnel.local_addr());
        p.host = Some(host.clone());
        p.port = Some(target_port);
        p
    };

    // Plaintext through the tunnel.
    let c = connect(&through(params(key))).await;
    assert_eq!(c.engine(), engine_of(key));
    assert!(!c.tls_in_use());
    assert_eq!(scalar_text(&c, "SELECT 7").await, "7");
    c.close().await.unwrap();

    // TLS through the tunnel, verifying the in-network service name (a SAN of the fixture
    // certificate), not the 127.0.0.1 tunnel address.
    let full = TlsOptions::new(TlsMode::VerifyFull).with_ca_cert(ca_cert());
    let c = connect(&through(tls_user_params(key, full))).await;
    assert_eq!(c.engine(), engine_of(key));
    assert!(c.tls_in_use());
    // KILL QUERY side connections go through the same tunnel.
    cancel_sleep_on(&c).await;
    c.close().await.unwrap();
    drop(tunnel);
}

async fn timestamps_utc(key: &str) {
    use arrow_array::cast::AsArray;
    use arrow_array::types::TimestampMicrosecondType;
    use arrow_schema::{DataType, TimeUnit};

    let c = connect(&params(key)).await;
    let table = format!("oxbase_mysql_ts_{}", std::process::id());
    run(&c, &format!("DROP TABLE IF EXISTS {table}")).await;
    run(
        &c,
        &format!("CREATE TABLE {table} (ts TIMESTAMP(6) NULL, dt DATETIME(6) NULL)"),
    )
    .await;
    let instant = chrono::DateTime::parse_from_rfc3339("2026-10-31T12:00:00.123456+02:00").unwrap();
    collect(
        c.execute(
            &format!("INSERT INTO {table} (ts, dt) VALUES (?, ?)"),
            &[Value::TimestampTz(instant), Value::TimestampTz(instant)],
            CancellationToken::new(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();

    // The default (UTC) session reads TIMESTAMP back as the same instant.
    let out = run(&c, &format!("SELECT ts, dt FROM {table}")).await;
    let set = &out.result_sets[0];
    assert_eq!(
        set.schema.field(0).data_type(),
        &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
    );
    assert_eq!(
        set.schema.field(1).data_type(),
        &DataType::Timestamp(TimeUnit::Microsecond, None)
    );
    let ts = set.batches[0]
        .column(0)
        .as_primitive::<TimestampMicrosecondType>();
    assert_eq!(ts.value(0), instant.timestamp_micros());

    // A session in another zone sees the same stored instant as its local wall-clock time.
    let mut keep_zone = params(key);
    keep_zone
        .options
        .insert(OPT_UTC_SESSION.to_owned(), "false".to_owned());
    let other = connect(&keep_zone).await;
    run(&other, "SET time_zone = '+05:00'").await;
    let out = run(&other, &format!("SELECT ts FROM {table}")).await;
    let set = &out.result_sets[0];
    assert_eq!(
        set.schema.field(0).data_type(),
        &DataType::Timestamp(TimeUnit::Microsecond, None)
    );
    let wall = set.batches[0]
        .column(0)
        .as_primitive::<TimestampMicrosecondType>()
        .value(0);
    let five_hours = 5 * 3600 * 1_000_000;
    assert_eq!(wall, instant.timestamp_micros() + five_hours);
    other.close().await.unwrap();

    run(&c, &format!("DROP TABLE {table}")).await;
}

async fn tls_require(key: &str) {
    // The servers ship (auto-generated or fixture) certificates; Require accepts any of them.
    let c = connect(&params(key).with_tls(TlsOptions::new(TlsMode::Require))).await;
    assert!(c.tls_in_use());
    let out = run(&c, "SHOW SESSION STATUS LIKE 'Ssl_version'").await;
    assert_eq!(out.result_sets[0].num_rows(), 1);
    let prefer = connect(&params(key).with_tls(TlsOptions::new(TlsMode::Prefer))).await;
    assert!(prefer.tls_in_use());
}

const ROWS_200K: &str = "WITH RECURSIVE a(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM a WHERE n < 999), \
     b(m) AS (SELECT 0 UNION ALL SELECT m + 1 FROM b WHERE m < 199) \
     SELECT a.n + 1000 * b.m AS id, CONCAT('row ', a.n + 1000 * b.m) AS label \
     FROM a CROSS JOIN b";

async fn stream_200k(key: &str) {
    let c = connect(&params(key)).await;
    let mut stream = c
        .execute(ROWS_200K, &[], CancellationToken::new())
        .await
        .unwrap();
    let (mut schemas, mut batches, mut rows, mut sum) = (0, 0, 0usize, 0i128);
    while let Some(item) = stream.next().await {
        match item.unwrap() {
            ResultItem::Schema(_) => schemas += 1,
            ResultItem::Batch(b) => {
                batches += 1;
                rows += b.num_rows();
                let ids = b.column(0);
                sum += (0..b.num_rows())
                    .map(|r| int_at(ids.as_ref(), r).expect("id"))
                    .sum::<i128>();
            }
            ResultItem::RowsAffected(_) | ResultItem::Notice(_) => {}
        }
    }
    assert_eq!(schemas, 1);
    assert_eq!(rows, 200_000);
    assert_eq!(sum, (0..200_000i128).sum::<i128>());
    assert!(batches > 1, "streamed in {batches} batch(es)");
}

async fn cancel_sleep_on(c: &MySqlConnection) {
    // Through cancel_running.
    let stream = c
        .execute("SELECT SLEEP(30)", &[], CancellationToken::new())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    c.cancel_running().await.unwrap();
    let err = collect(stream).await.unwrap_err();
    assert!(err.is_cancelled(), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(scalar_text(c, "SELECT 1").await, "1");

    // Through the token.
    let token = CancellationToken::new();
    let stream = c
        .execute("SELECT SLEEP(30)", &[], token.clone())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    token.cancel();
    let err = collect(stream).await.unwrap_err();
    assert!(err.is_cancelled(), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(scalar_text(c, "SELECT 2").await, "2");

    // A cancelled script does not run its later statements.
    let token = CancellationToken::new();
    let stream = c
        .execute(
            "SELECT SLEEP(30); SELECT SLEEP(30); SELECT 3",
            &[],
            token.clone(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    token.cancel();
    assert!(collect(stream).await.unwrap_err().is_cancelled());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(scalar_text(c, "SELECT 4").await, "4");

    // An already-cancelled token sends nothing.
    let token = CancellationToken::new();
    token.cancel();
    assert!(matches!(
        c.execute("SELECT 1", &[], token).await,
        Err(Error::Cancelled)
    ));
}

async fn cancel_sleep(key: &str) {
    let c = connect(&params(key)).await;
    cancel_sleep_on(&c).await;
    // Dropping the stream mid-statement also stops it and frees the session.
    let stream = c
        .execute("SELECT SLEEP(30)", &[], CancellationToken::new())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(stream);
    let started = Instant::now();
    assert_eq!(scalar_text(&c, "SELECT 5").await, "5");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

async fn multi_result_sets(key: &str) {
    let c = connect(&params(key)).await;
    let out = run(
        &c,
        "SELECT 1 AS a; \
         SELECT 'x' AS b, 2 AS c UNION ALL SELECT 'y', 3; \
         CREATE TEMPORARY TABLE t_multi (id INT AUTO_INCREMENT PRIMARY KEY, v INT); \
         INSERT INTO t_multi (v) VALUES (10), (20), (30); \
         SELECT COUNT(*) AS n FROM t_multi; \
         SELECT CAST('12abc' AS SIGNED) AS w",
    )
    .await;
    assert_eq!(out.result_sets.len(), 4);
    assert_eq!(out.result_sets[1].num_rows(), 2);
    assert_eq!(out.result_sets[1].schema.fields().len(), 2);
    assert_eq!(out.rows_affected, [0, 3]);
    assert!(
        out.notices
            .iter()
            .any(|n| n.code.as_deref() == Some(LAST_INSERT_ID_CODE)),
        "{:?}",
        out.notices
    );
    // The truncation warning of the last statement comes from SHOW WARNINGS.
    assert!(
        out.notices
            .iter()
            .any(|n| n.severity == NoticeSeverity::Warning && n.code.as_deref() == Some("1292")),
        "{:?}",
        out.notices
    );
    assert!(c.last_insert_id().is_some());

    // A failing statement in a script reports its server error, and the session survives.
    let err = collect(
        c.execute(
            "SELECT 1; SELECT * FROM no_such_table_xyz",
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.sql_code(), Some("1146"), "{err}");
    assert_eq!(scalar_text(&c, "SELECT 6").await, "6");
}

async fn types_and_params(key: &str) {
    use oxbase_driver::arrow::DataType;
    let c = connect(&params(key)).await;
    let sql = "SELECT CAST(1 AS UNSIGNED) AS u, CAST(-1 AS SIGNED) AS i, 1.50 AS d, \
               CAST('2026-10-31' AS DATE) AS dt, CAST('12:34:56.5' AS TIME(1)) AS tm, \
               CAST('2026-10-31 01:02:03' AS DATETIME) AS ts, 'héllo' AS s, \
               CAST(x'00ff' AS BINARY(2)) AS bin, b'101' AS bits, NULL AS nothing";
    for params in [vec![], vec![Value::Int(1)]] {
        let sql = if params.is_empty() {
            sql.to_owned()
        } else {
            format!("{sql} FROM DUAL WHERE ? = 1")
        };
        let out = collect(
            c.execute(&sql, &params, CancellationToken::new())
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        let schema = &out.result_sets[0].schema;
        let ty = |name: &str| schema.field_with_name(name).unwrap().data_type().clone();
        // MySQL types these casts as BIGINT, MariaDB as INT when the value is small.
        assert!(ty("u").is_unsigned_integer(), "{:?}", ty("u"));
        assert!(ty("i").is_signed_integer(), "{:?}", ty("i"));
        assert!(
            matches!(ty("d"), DataType::Decimal128(_, 2)),
            "{:?}",
            ty("d")
        );
        assert_eq!(ty("dt"), DataType::Date32);
        assert!(matches!(ty("tm"), DataType::Duration(_)));
        assert!(matches!(ty("ts"), DataType::Timestamp(_, None)));
        assert_eq!(ty("s"), DataType::Utf8);
        assert_eq!(ty("bin"), DataType::Binary);
        // `b'101'` is a bit-string literal, which the server types as VARBINARY.
        assert_eq!(ty("bits"), DataType::Binary);
        assert_eq!(out.result_sets[0].num_rows(), 1);
    }
    // Typed parameters round-trip.
    let out = collect(
        c.execute(
            "SELECT ? AS a, ? AS b, ? AS c",
            &[Value::Text("x".into()), Value::Int(-7), Value::Null],
            CancellationToken::new(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(out.result_sets[0].num_rows(), 1);
}

async fn transaction_rollback(key: &str) {
    let c = connect(&params(key)).await;
    let table = format!("oxbase_mysql_tx_{}", std::process::id());
    run(&c, &format!("DROP TABLE IF EXISTS {table}")).await;
    run(
        &c,
        &format!("CREATE TABLE {table} (id INT PRIMARY KEY) ENGINE=InnoDB"),
    )
    .await;
    let count = format!("SELECT COUNT(*) FROM {table}");

    let tx = c.begin().await.unwrap();
    collect(
        tx.execute(
            &format!("INSERT INTO {table} (id) VALUES (?), (?)"),
            &[Value::Int(1), Value::Int(2)],
            CancellationToken::new(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        scalar_text(&c, &count).await,
        "2",
        "visible inside the transaction"
    );
    tx.rollback().await.unwrap();
    assert_eq!(scalar_text(&c, &count).await, "0");

    // Dropping a transaction rolls it back too.
    let tx = c.begin().await.unwrap();
    collect(
        tx.execute(
            &format!("INSERT INTO {table} (id) VALUES (3)"),
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    drop(tx);
    assert_eq!(scalar_text(&c, &count).await, "0");

    // Commit keeps the row.
    let tx = c.begin().await.unwrap();
    collect(
        tx.execute(
            &format!("INSERT INTO {table} (id) VALUES (4)"),
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(scalar_text(&c, &count).await, "1");
    run(&c, &format!("DROP TABLE {table}")).await;
}

macro_rules! live {
    ($($name:ident),* $(,)?) => {
        mod mysql {
            $(
                #[tokio::test]
                #[ignore = "needs the mysql fixture (scripts/fixtures-up.sh mysql)"]
                async fn $name() {
                    super::$name("mysql").await;
                }
            )*
        }
        mod mariadb {
            $(
                #[tokio::test]
                #[ignore = "needs the mariadb fixture (scripts/fixtures-up.sh mariadb)"]
                async fn $name() {
                    super::$name("mariadb").await;
                }
            )*
        }
    };
}

live!(
    plain_connect,
    tls_verify_full,
    tls_client_cert,
    tls_require,
    ssh_tunnel,
    timestamps_utc,
    stream_200k,
    cancel_sleep,
    multi_result_sets,
    types_and_params,
    transaction_rollback,
);
