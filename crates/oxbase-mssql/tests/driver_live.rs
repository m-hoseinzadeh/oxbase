//! Live tests of the SQL Server driver against the `mssql` fixture (`scripts/fixtures-up.sh
//! mssql`), docs/m4-contracts.md §12 (driver).
//!
//! All are `#[ignore]`d: the conformance CI job runs them with
//! `cargo test -p oxbase-mssql -- --include-ignored`. Connection parameters come from
//! `tests/fixtures.toml` (`$OXBASE_FIXTURES`). The type checks build their own `#temp` table
//! with the literals of `[m4]` `m4_types` (§5.3), so they do not depend on the M4 seed scripts.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Decimal128Type, Float64Type, Int64Type, Time64MicrosecondType,
    TimestampMicrosecondType,
};
use arrow_array::{Array, RecordBatch};
use arrow_schema::DataType;
use chrono::{FixedOffset, NaiveDate, NaiveTime, TimeZone};
use futures::StreamExt;
use oxbase_driver::{
    CancellationToken, Collected, ConnectParams, Connection, Driver, ResultItem, collect,
};
use oxbase_mssql::{MsSqlConnection, MsSqlDriver, options};
use oxbase_types::{Engine, Error, PemSource, TlsMode, TlsOptions, Value};

const CANCEL_LIMIT: Duration = Duration::from_secs(2);

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
    let mut table = t;
    let mut out = None;
    for (i, key) in path.iter().enumerate() {
        let next = table
            .get(*key)
            .unwrap_or_else(|| panic!("fixtures.toml has no {}", path.join(".")));
        if i + 1 < path.len() {
            table = next.as_table().expect("table");
        }
        out = Some(next);
    }
    out.expect("non-empty path")
}

fn s(t: &toml::Table, path: &[&str]) -> String {
    get(t, path).as_str().expect("string").to_owned()
}

fn port(t: &toml::Table, path: &[&str]) -> u16 {
    u16::try_from(get(t, path).as_integer().expect("integer")).expect("port")
}

/// `sa` on oxbase_test, TLS disabled.
fn params() -> ConnectParams {
    let f = fixtures();
    ConnectParams::network(Engine::MsSql, s(&f, &["engines", "mssql", "host"]))
        .with_port(port(&f, &["engines", "mssql", "port"]))
        .with_user(s(&f, &["engines", "mssql", "user"]))
        .with_password(s(&f, &["engines", "mssql", "password"]))
        .with_database(s(&f, &["engines", "mssql", "database"]))
        .with_tls(TlsOptions::new(TlsMode::Disable))
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

/// The TLS-only login.
fn tls_user(tls: TlsOptions) -> ConnectParams {
    let f = fixtures();
    params()
        .with_user(s(&f, &["engines", "mssql", "tls_user", "user"]))
        .with_password(s(&f, &["engines", "mssql", "tls_user", "password"]))
        .with_database(s(&f, &["engines", "mssql", "tls_user", "database"]))
        .with_tls(tls)
}

async fn connect(p: &ConnectParams) -> MsSqlConnection {
    MsSqlDriver::new()
        .connect_mssql(p)
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"))
}

async fn run(c: &dyn Connection, sql: &str, params: &[Value]) -> Collected {
    collect(
        c.execute(sql, params, CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}")),
    )
    .await
    .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn first_batch(out: &Collected) -> &RecordBatch {
    &out.result_sets[0].batches[0]
}

async fn ping(c: &dyn Connection) {
    let out = run(c, "SELECT 1 AS one", &[]).await;
    let b = first_batch(&out);
    assert_eq!(b.column(0).as_primitive::<Int64Type>().value(0), 1);
}

#[tokio::test]
#[ignore = "needs the mssql fixture"]
async fn plain_connect_reports_session_facts() {
    let c = connect(&params()).await;
    assert!(!c.tls_in_use());
    assert_eq!(c.engine(), Engine::MsSql);
    assert_eq!(c.current_database().as_deref(), Some("oxbase_test"));
    let v = c.server_version();
    assert_eq!(v.major, 17, "{}", v.raw());
    assert!(
        v.raw().starts_with("Microsoft SQL Server 2025"),
        "{}",
        v.raw()
    );
    assert!(c.spid() > 0);
    let debug = format!("{c:?}");
    assert!(!debug.contains("Oxbase_Fixture_Pw"), "{debug}");
    ping(&c).await;
    c.close().await.unwrap();
    c.close().await.unwrap();
    let err = c
        .execute("SELECT 1", &[], CancellationToken::new())
        .await
        .err()
        .expect("closed");
    assert!(matches!(err, Error::Connection { .. }), "{err}");

    // Through the Driver trait too.
    let boxed = MsSqlDriver::new().connect(&params()).await.unwrap();
    ping(boxed.as_ref()).await;
}

#[tokio::test]
#[ignore = "needs the mssql fixture"]
async fn tls_modes() {
    let prefer = connect(&params().with_tls(TlsOptions::new(TlsMode::Prefer))).await;
    assert!(prefer.tls_in_use(), "Prefer negotiates TLS");
    ping(&prefer).await;

    let full = connect(&params().with_tls(verify(TlsMode::VerifyFull, "localhost"))).await;
    assert!(full.tls_in_use());
    ping(&full).await;

    let wrong = MsSqlDriver::new()
        .connect_mssql(&params().with_tls(verify(TlsMode::VerifyFull, "wrong-name.oxbase.invalid")))
        .await
        .expect_err("a wrong server name is refused");
    assert!(matches!(wrong, Error::Connection { .. }), "{wrong}");

    let ca =
        connect(&params().with_tls(verify(TlsMode::VerifyCa, "wrong-name.oxbase.invalid"))).await;
    assert!(ca.tls_in_use(), "VerifyCa does not check the name");
    ping(&ca).await;

    let require = connect(&params().with_tls(TlsOptions::new(TlsMode::Require))).await;
    assert!(require.tls_in_use());
}

#[tokio::test]
#[ignore = "needs the mssql fixture"]
async fn tds8_strict_encryption() {
    let mut p = params().with_tls(verify(TlsMode::VerifyFull, "localhost"));
    p.options
        .insert(options::ENCRYPT.into(), options::ENCRYPT_STRICT.into());
    let c = connect(&p).await;
    assert!(c.tls_in_use());
    ping(&c).await;
    let out = run(
        &c,
        "SELECT encrypt_option AS e FROM sys.dm_exec_connections WHERE session_id = @@SPID",
        &[],
    )
    .await;
    assert_eq!(
        first_batch(&out).column(0).as_string::<i32>().value(0),
        "TRUE"
    );
}

#[tokio::test]
#[ignore = "needs the mssql fixture"]
async fn tls_only_login() {
    let refused = MsSqlDriver::new()
        .connect_mssql(&tls_user(TlsOptions::new(TlsMode::Disable)))
        .await
        .expect_err("the TLS-only login is refused in plaintext");
    // The logon trigger's rollback: 17892 "Logon failed ... due to trigger execution".
    assert!(
        matches!(refused, Error::Sql { .. } | Error::Auth { .. }),
        "{refused}"
    );
    let c = connect(&tls_user(verify(TlsMode::VerifyFull, "localhost"))).await;
    assert!(c.tls_in_use());
    ping(&c).await;
}

#[tokio::test]
#[ignore = "needs the mssql fixture"]
async fn wrong_password_is_auth_and_never_echoed() {
    let marker = "MARKER-secret-pw-0001";
    let err = MsSqlDriver::new()
        .connect_mssql(&params().with_password(marker))
        .await
        .expect_err("wrong password");
    let text = format!("{err} {err:?}");
    assert!(matches!(err, Error::Auth { .. }), "{text}");
    assert!(!text.contains(marker), "{text}");
    assert!(
        text.contains("18456") || text.to_lowercase().contains("login failed"),
        "{text}"
    );
}

const TYPES_SQL: &str = "
CREATE TABLE #m4_types (
    id int PRIMARY KEY, c_tinyint tinyint, c_smallint smallint, c_int int, c_bigint bigint,
    c_bit bit, c_decimal decimal(18,4), c_numeric numeric(38,10), c_money money,
    c_smallmoney smallmoney, c_float float, c_real real, c_date date, c_time time(7),
    c_datetime datetime, c_datetime2 datetime2(7), c_smalldatetime smalldatetime,
    c_dto datetimeoffset(7), c_char char(10), c_varchar varchar(20), c_varchar_max varchar(max),
    c_nchar nchar(10), c_nvarchar nvarchar(40), c_nvarchar_max nvarchar(max), c_text text,
    c_ntext ntext, c_binary binary(16), c_varbinary varbinary(16), c_varbinary_max varbinary(max),
    c_image image, c_guid uniqueidentifier, c_xml xml, c_json json, c_vector vector(3),
    c_variant sql_variant, c_hierarchyid hierarchyid, c_geometry geometry,
    c_geography geography, c_rowversion rowversion);
INSERT INTO #m4_types (id, c_tinyint, c_smallint, c_int, c_bigint, c_bit, c_decimal, c_numeric,
    c_money, c_smallmoney, c_float, c_real, c_date, c_time, c_datetime, c_datetime2,
    c_smalldatetime, c_dto, c_char, c_varchar, c_varchar_max, c_nchar, c_nvarchar,
    c_nvarchar_max, c_text, c_ntext, c_binary, c_varbinary, c_varbinary_max, c_image, c_guid,
    c_xml, c_json, c_vector, c_variant, c_hierarchyid, c_geometry, c_geography)
VALUES (1, 255, -32768, 2147483647, -9223372036854775808, 1, 12345678901234.5678,
    1234567890123456789012345678.0123456789, 922337203685477.5807, -214748.3648, 1.5E300, 3.25,
    '0001-01-01', '23:59:59.1234567', '2026-01-15T10:20:30.997', '9999-12-31T23:59:59.9999999',
    '2026-01-15T10:21:00', '2026-01-15T10:20:30.1234567+03:30', 'abc', 'café',
    REPLICATE(CAST('x' AS varchar(max)), 9000), N'☃', N'Grüße 漢字 😀',
    REPLICATE(CAST(N'ab' AS nvarchar(max)), 5000), 'old text', N'old ntext', 0x0102,
    0xCAFEBABE, CAST(REPLICATE(CAST(0xAB AS varbinary(max)), 10000) AS varbinary(max)), 0xFFD8FF,
    '6F9619FF-8B86-D011-B42D-00C04FC964FF', N'<a b=\"1\"/>', N'{\"a\":1,\"b\":[true,null]}',
    '[1,2,3]', CAST(42 AS int), hierarchyid::Parse('/1/2/'),
    geometry::STGeomFromText('POINT(1 2)', 0), geography::Point(47.65, -122.34, 4326));
INSERT INTO #m4_types (id) VALUES (2);
SELECT *, CAST(c_hierarchyid AS nvarchar(100)) AS h_text FROM #m4_types ORDER BY id;
";

fn col<'a>(b: &'a RecordBatch, name: &str) -> &'a dyn Array {
    b.column_by_name(name)
        .unwrap_or_else(|| panic!("no column {name}"))
        .as_ref()
}

fn text(b: &RecordBatch, name: &str) -> String {
    col(b, name).as_string::<i32>().value(0).to_owned()
}

fn int(b: &RecordBatch, name: &str) -> i64 {
    col(b, name).as_primitive::<Int64Type>().value(0)
}

fn bytes(b: &RecordBatch, name: &str) -> Vec<u8> {
    col(b, name).as_binary::<i32>().value(0).to_vec()
}

fn micros(text: &str) -> i64 {
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
        .unwrap()
        .and_utc()
        .timestamp_micros()
}

#[tokio::test]
#[ignore = "needs the mssql fixture"]
async fn m4_types_decode_exactly() {
    let c = connect(&params()).await;
    let out = run(&c, TYPES_SQL, &[]).await;
    assert_eq!(out.result_sets.len(), 1, "only the SELECT returns rows");
    let set = &out.result_sets[0];
    assert_eq!(set.num_rows(), 2);
    let b = &set.batches[0];
    let schema = b.schema();
    let field = |n: &str| schema.field_with_name(n).unwrap().clone();
    assert_eq!(field("c_tinyint").data_type(), &DataType::Int64);
    assert_eq!(field("c_money").data_type(), &DataType::Decimal128(19, 4));
    assert_eq!(field("c_guid").data_type(), &DataType::FixedSizeBinary(16));
    assert_eq!(
        field("c_guid").metadata()["ARROW:extension:name"],
        "arrow.uuid"
    );
    assert_eq!(
        field("c_decimal").metadata()["oxbase.native_type"],
        "decimal"
    );

    assert_eq!(int(b, "c_tinyint"), 255);
    assert_eq!(int(b, "c_smallint"), -32768);
    assert_eq!(int(b, "c_int"), 2_147_483_647);
    assert_eq!(int(b, "c_bigint"), i64::MIN);
    assert!(col(b, "c_bit").as_boolean().value(0));
    assert_eq!(text(b, "c_decimal"), "12345678901234.5678");
    assert_eq!(
        text(b, "c_numeric"),
        "1234567890123456789012345678.0123456789"
    );
    // tiberius 0.13 decodes money through f64 (known gap): the largest money value comes back
    // within the f64 spacing at that magnitude.
    let money = col(b, "c_money").as_primitive::<Decimal128Type>().value(0);
    assert!(
        (money - 9_223_372_036_854_775_807).abs() <= 2048,
        "money mantissa {money}"
    );
    assert_eq!(
        col(b, "c_smallmoney")
            .as_primitive::<Decimal128Type>()
            .value_as_string(0),
        "-214748.3648"
    );
    let float = col(b, "c_float").as_primitive::<Float64Type>().value(0);
    assert_eq!(float.to_bits(), 1.5e300f64.to_bits());
    let real = col(b, "c_real").as_primitive::<Float64Type>().value(0);
    assert_eq!(real.to_bits(), 3.25f64.to_bits());
    assert_eq!(
        col(b, "c_date").as_primitive::<Date32Type>().value(0),
        -719_162
    );
    assert_eq!(
        col(b, "c_time")
            .as_primitive::<Time64MicrosecondType>()
            .value(0),
        86_399_123_456
    );
    let ts = |n: &str| {
        col(b, n)
            .as_primitive::<TimestampMicrosecondType>()
            .value(0)
    };
    assert_eq!(ts("c_datetime"), micros("2026-01-15 10:20:30.996667"));
    assert_eq!(ts("c_datetime2"), micros("9999-12-31 23:59:59.999999"));
    assert_eq!(ts("c_smalldatetime"), micros("2026-01-15 10:21:00.0"));
    assert_eq!(text(b, "c_dto"), "2026-01-15T10:20:30.1234567+03:30");
    assert_eq!(text(b, "c_char"), "abc       ");
    assert_eq!(text(b, "c_varchar"), "café");
    assert_eq!(text(b, "c_varchar_max"), "x".repeat(9000));
    assert_eq!(text(b, "c_nchar"), format!("☃{}", " ".repeat(9)));
    assert_eq!(text(b, "c_nvarchar"), "Grüße 漢字 😀");
    assert_eq!(text(b, "c_nvarchar_max").chars().count(), 10_000);
    assert_eq!(text(b, "c_text"), "old text");
    assert_eq!(text(b, "c_ntext"), "old ntext");
    let mut binary = vec![1u8, 2];
    binary.resize(16, 0);
    assert_eq!(bytes(b, "c_binary"), binary);
    assert_eq!(bytes(b, "c_varbinary"), [0xCA, 0xFE, 0xBA, 0xBE]);
    assert_eq!(bytes(b, "c_varbinary_max"), vec![0xAB; 10_000]);
    assert_eq!(bytes(b, "c_image"), [0xFF, 0xD8, 0xFF]);
    assert_eq!(
        col(b, "c_guid").as_fixed_size_binary().value(0),
        uuid::Uuid::parse_str("6f9619ff-8b86-d011-b42d-00c04fc964ff")
            .unwrap()
            .as_bytes()
    );
    assert_eq!(text(b, "c_xml"), "<a b=\"1\" />");
    let json: serde_json::Value = serde_json::from_str(&text(b, "c_json")).unwrap();
    assert_eq!(json, serde_json::json!({"a": 1, "b": [true, null]}));
    let vector: Vec<f64> = serde_json::from_str(&text(b, "c_vector")).unwrap();
    assert_eq!(vector, [1.0, 2.0, 3.0]);
    assert_eq!(text(b, "c_variant"), "42");
    assert!(!bytes(b, "c_hierarchyid").is_empty());
    assert_eq!(text(b, "h_text"), "/1/2/");
    assert!(!bytes(b, "c_geometry").is_empty());
    assert!(!bytes(b, "c_geography").is_empty());
    assert_eq!(bytes(b, "c_rowversion").len(), 8);

    // Row 2: everything NULL but the key and the row version.
    for (i, f) in schema.fields().iter().enumerate() {
        if matches!(f.name().as_str(), "id" | "c_rowversion") {
            continue;
        }
        assert!(b.column(i).is_null(1), "row 2 {} is NULL", f.name());
    }
}

#[tokio::test]
#[ignore = "needs the mssql fixture"]
async fn every_value_kind_round_trips_as_a_parameter() {
    let c = connect(&params()).await;
    let one = |out: &Collected| first_batch(out).clone();

    let b = one(&run(&c, "SELECT @P1 AS v", &[Value::Null]).await);
    assert!(b.column(0).is_null(0));
    let b = one(&run(&c, "SELECT @P1 AS v", &[Value::Bool(true)]).await);
    assert!(b.column(0).as_boolean().value(0));
    let b = one(&run(&c, "SELECT @P1 AS v", &[Value::Int(i64::MIN)]).await);
    assert_eq!(b.column(0).as_primitive::<Int64Type>().value(0), i64::MIN);
    let b = one(&run(&c, "SELECT @P1 AS v", &[Value::UInt(u64::MAX)]).await);
    assert_eq!(
        b.column(0).as_string::<i32>().value(0),
        "18446744073709551615"
    );
    let b = one(&run(&c, "SELECT @P1 AS v", &[Value::Float(1.5)]).await);
    assert_eq!(
        b.column(0).as_primitive::<Float64Type>().value(0).to_bits(),
        1.5f64.to_bits()
    );
    let b = one(&run(&c, "SELECT @P1 AS v", &[Value::Decimal("-123.4500".into())]).await);
    assert_eq!(b.column(0).as_string::<i32>().value(0), "-123.4500");
    let b = one(&run(&c, "SELECT @P1 AS v", &[Value::Text("Grüße 😀".into())]).await);
    assert_eq!(b.column(0).as_string::<i32>().value(0), "Grüße 😀");
    let b = one(&run(
        &c,
        "SELECT @P1 AS v",
        &[Value::Json(serde_json::json!({"k": [1]}))],
    )
    .await);
    assert_eq!(b.column(0).as_string::<i32>().value(0), r#"{"k":[1]}"#);
    let b = one(&run(&c, "SELECT @P1 AS v", &[Value::Bytes(vec![0, 1, 255])]).await);
    assert_eq!(b.column(0).as_binary::<i32>().value(0), &[0, 1, 255]);
    let date = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
    let b = one(&run(&c, "SELECT @P1 AS v", &[Value::Date(date)]).await);
    assert_eq!(
        b.column(0).as_primitive::<Date32Type>().value(0),
        i32::try_from((date - NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()).num_days()).unwrap()
    );
    let time = NaiveTime::from_hms_micro_opt(13, 14, 15, 123_456).unwrap();
    let b = one(&run(&c, "SELECT @P1 AS v", &[Value::Time(time)]).await);
    assert_eq!(
        b.column(0).as_primitive::<Time64MicrosecondType>().value(0),
        (13 * 3600 + 14 * 60 + 15) * 1_000_000 + 123_456
    );
    let stamp = date.and_time(time);
    let b = one(&run(&c, "SELECT @P1 AS v", &[Value::Timestamp(stamp)]).await);
    assert_eq!(
        b.column(0)
            .as_primitive::<TimestampMicrosecondType>()
            .value(0),
        stamp.and_utc().timestamp_micros()
    );
    let tz = FixedOffset::east_opt(3 * 3600 + 30 * 60)
        .unwrap()
        .with_ymd_and_hms(2026, 1, 15, 10, 20, 30)
        .unwrap();
    let b = one(&run(&c, "SELECT @P1 AS v", &[Value::TimestampTz(tz)]).await);
    assert_eq!(
        b.column(0).as_string::<i32>().value(0),
        "2026-01-15T10:20:30.0000000+03:30"
    );
    let id = uuid::Uuid::parse_str("6f9619ff-8b86-d011-b42d-00c04fc964ff").unwrap();
    let b = one(&run(&c, "SELECT @P1 AS v", &[Value::Uuid(id)]).await);
    assert_eq!(b.column(0).as_fixed_size_binary().value(0), id.as_bytes());

    // Several parameters, and a parameter that is NULL in a comparison.
    let b = one(&run(
        &c,
        "SELECT @P1 + @P2 AS s, CASE WHEN @P3 IS NULL THEN 1 ELSE 0 END AS n",
        &[Value::Int(40), Value::Int(2), Value::Null],
    )
    .await);
    assert_eq!(b.column(0).as_primitive::<Int64Type>().value(0), 42);
    assert_eq!(b.column(1).as_primitive::<Int64Type>().value(0), 1);

    // NaN never reaches the server.
    let err = c
        .execute(
            "SELECT @P1",
            &[Value::Float(f64::NAN)],
            CancellationToken::new(),
        )
        .await
        .err()
        .expect("NaN refused");
    assert!(matches!(err, Error::InvalidInput { .. }), "{err}");
}

#[tokio::test]
#[ignore = "needs the mssql fixture"]
async fn batches_results_counts_and_session_scope() {
    let c = connect(&params()).await;

    // Several result sets in one batch, in order.
    let out = run(&c, "SELECT 1 AS a; SELECT N'x' AS b, 2 AS c", &[]).await;
    assert_eq!(out.result_sets.len(), 2);
    assert_eq!(out.result_sets[0].schema.field(0).name(), "a");
    assert_eq!(out.result_sets[1].schema.fields().len(), 2);

    // A #temp table created by one batch is visible to the next, and to sp_executesql.
    run(
        &c,
        "CREATE TABLE #t (x int); INSERT INTO #t VALUES (1), (2)",
        &[],
    )
    .await;
    let out = run(
        &c,
        "INSERT INTO #t (x) VALUES (@P1); UPDATE #t SET x = x + 1; SELECT COUNT(*) AS n FROM #t",
        &[Value::Int(3)],
    )
    .await;
    assert_eq!(out.rows_affected, [1, 3], "RowsAffected on the RPC path");
    assert_eq!(
        first_batch(&out)
            .column(0)
            .as_primitive::<Int64Type>()
            .value(0),
        3
    );

    // USE persists in the session.
    run(&c, "USE master", &[]).await;
    let out = run(&c, "SELECT DB_NAME() AS d", &[]).await;
    assert_eq!(
        first_batch(&out).column(0).as_string::<i32>().value(0),
        "master"
    );
    run(&c, "USE oxbase_test", &[]).await;

    // Server errors carry the number and the line; the session stays usable.
    let stream = c
        .execute(
            "SELECT 1 AS ok;\nSELECT * FROM dbo.no_such_table",
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let err = collect(stream).await.expect_err("missing table");
    assert_eq!(err.sql_code(), Some("208"), "{err}");
    assert!(err.to_string().contains("no_such_table"), "{err}");
    let err = collect(
        c.execute(
            "SELECT * FROM dbo.no_such_table WHERE 1 = @P1",
            &[Value::Int(1)],
            CancellationToken::new(),
        )
        .await
        .unwrap(),
    )
    .await
    .expect_err("missing table over RPC");
    assert_eq!(err.sql_code(), Some("208"), "{err}");
    ping(&c).await;

    // A pre-cancelled token sends nothing.
    let token = CancellationToken::new();
    token.cancel();
    assert!(
        c.execute("SELECT 1", &[], token)
            .await
            .err()
            .unwrap()
            .is_cancelled()
    );
}

#[tokio::test]
#[ignore = "needs the mssql fixture"]
async fn streams_200k_rows_in_batches() {
    let c = connect(&params()).await;
    let mut stream = c
        .execute(
            "SELECT value AS n, CONVERT(char(32), HASHBYTES('MD5', CAST(value AS varchar(10))), 2) \
             AS h FROM GENERATE_SERIES(1, 200000)",
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let (mut rows, mut batches, mut last) = (0usize, 0usize, 0i64);
    while let Some(item) = stream.next().await {
        if let ResultItem::Batch(b) = item.unwrap() {
            rows += b.num_rows();
            batches += 1;
            last = b
                .column(0)
                .as_primitive::<Int64Type>()
                .value(b.num_rows() - 1);
        }
    }
    assert_eq!(rows, 200_000);
    assert!(batches > 1, "{batches} batch(es)");
    assert_eq!(last, 200_000);
}

#[derive(Clone, Copy, Debug)]
enum CancelBy {
    Token,
    CancelRunning,
}

/// Cancels `sql` after 300 ms; the stream must end with `Cancelled` and the session answer
/// `SELECT 1` within 2 s of the request.
async fn cancel_once(
    c: &std::sync::Arc<MsSqlConnection>,
    sql: &'static str,
    params: Vec<Value>,
    by: CancelBy,
) {
    let token = CancellationToken::new();
    let run = {
        let c = c.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let stream = c.execute(sql, &params, token).await?;
            collect(stream).await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!run.is_finished(), "{by:?}: finished before the cancel");
    let requested = Instant::now();
    match by {
        CancelBy::Token => token.cancel(),
        CancelBy::CancelRunning => c.cancel_running().await.unwrap(),
    }
    let result = tokio::time::timeout(CANCEL_LIMIT * 5, run)
        .await
        .expect("ends")
        .expect("task");
    let took = requested.elapsed();
    assert!(
        result.as_ref().err().is_some_and(Error::is_cancelled),
        "{by:?}: {:?}",
        result.map(|_| ())
    );
    assert!(took <= CANCEL_LIMIT, "{by:?}: took {took:?}");
    ping(c.as_ref()).await;
    assert!(
        requested.elapsed() <= CANCEL_LIMIT,
        "{by:?}: session free after {:?}",
        requested.elapsed()
    );
}

#[tokio::test]
#[ignore = "needs the mssql fixture"]
async fn cancel_by_token_and_cancel_running() {
    let c = std::sync::Arc::new(connect(&params()).await);
    let sql = "WAITFOR DELAY '00:00:30'";
    cancel_once(&c, sql, Vec::new(), CancelBy::Token).await;
    cancel_once(&c, sql, Vec::new(), CancelBy::CancelRunning).await;
    // The RPC path.
    cancel_once(
        &c,
        "WAITFOR DELAY '00:00:30'; SELECT @P1 AS v",
        vec![Value::Int(1)],
        CancelBy::Token,
    )
    .await;

    // Dropping a stream in the middle of a large result cancels it; the session stays usable.
    let mut stream = c
        .execute(
            "SELECT a.value AS a, b.value AS b FROM GENERATE_SERIES(1, 100000) AS a \
             CROSS JOIN GENERATE_SERIES(1, 100) AS b",
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut seen = 0;
    while let Some(item) = stream.next().await {
        if let ResultItem::Batch(b) = item.unwrap() {
            seen += b.num_rows();
            if seen > 10_000 {
                break;
            }
        }
    }
    drop(stream);
    let started = Instant::now();
    ping(c.as_ref()).await;
    assert!(started.elapsed() <= CANCEL_LIMIT, "{:?}", started.elapsed());
    for _ in 0..3 {
        ping(c.as_ref()).await;
    }
}

async fn count_tx(c: &dyn Connection) -> i64 {
    let out = run(c, "SELECT COUNT(*) AS n FROM #tx", &[]).await;
    first_batch(&out)
        .column(0)
        .as_primitive::<Int64Type>()
        .value(0)
}

#[tokio::test]
#[ignore = "needs the mssql fixture"]
async fn transactions_commit_roll_back_and_roll_back_when_dropped() {
    let c = connect(&params()).await;
    run(&c, "CREATE TABLE #tx (x int)", &[]).await;

    let tx = c.begin().await.unwrap();
    collect(
        tx.execute("INSERT INTO #tx VALUES (1)", &[], CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(count_tx(&c).await, 0);

    let tx = c.begin().await.unwrap();
    collect(
        tx.execute(
            "INSERT INTO #tx VALUES (@P1)",
            &[Value::Int(2)],
            CancellationToken::new(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(count_tx(&c).await, 1);

    let tx = c.begin().await.unwrap();
    collect(
        tx.execute("INSERT INTO #tx VALUES (3)", &[], CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    drop(tx);
    assert_eq!(
        count_tx(&c).await,
        1,
        "a dropped transaction is rolled back"
    );
    let out = run(&c, "SELECT @@TRANCOUNT AS t", &[]).await;
    assert_eq!(
        first_batch(&out)
            .column(0)
            .as_primitive::<Int64Type>()
            .value(0),
        0
    );
}
