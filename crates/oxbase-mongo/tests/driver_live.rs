//! Live tests of the MongoDB driver against the `mongodb` fixture (`scripts/fixtures-up.sh
//! mongodb ssh`), docs/m7-contracts.md §11 (mongo-driver).
//!
//! All are `#[ignore]`d: the conformance CI job runs them with
//! `cargo test -p oxbase-mongo -- --include-ignored`. Connection parameters come from
//! `tests/fixtures.toml` (`$OXBASE_FIXTURES`). The tests never touch seed collections: they
//! create their own in `oxbase_test` (`m7drv_t<epoch>_<pid>_<what>`), drop them at the end, and
//! drop leftovers older than an hour at start. The 200 000-document check reads `m7_big` when
//! the M7 seed has it, else a collection of the same shape it builds itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arrow_array::Array;
use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int32Type, Int64Type};
use arrow_schema::DataType;
use bson::{Document, doc};
use futures::StreamExt;
use oxbase_driver::{
    CancellationToken, Collected, ConnectParams, Connection, Driver, ResultItem, collect,
};
use oxbase_mongo::{MongoConnection, MongoDriver, options};
use oxbase_net::ssh::{RusshTunnelProvider, TunnelHandle, TunnelProvider};
use oxbase_types::{
    Endpoint, Engine, Error, KeySource, KnownHostsPolicy, PemSource, Secret, ServerVersion,
    SshAuth, SshOptions, TlsMode, TlsOptions,
};

const CANCEL_LIMIT: Duration = Duration::from_secs(2);
const DATABASE: &str = "oxbase_test";
const WRONG_NAME: &str = "wrong-name.oxbase.invalid";
const BIG_ROWS: usize = 200_000;

// ---- fixtures -----------------------------------------------------------------------------------

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

fn get<'a>(t: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Value> {
    let mut table = t;
    let mut out = None;
    for (i, key) in path.iter().enumerate() {
        let next = table.get(*key)?;
        if i + 1 < path.len() {
            table = next.as_table()?;
        }
        out = Some(next);
    }
    out
}

fn s(t: &toml::Table, path: &[&str]) -> String {
    get(t, path)
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("fixtures.toml has no string {}", path.join(".")))
        .to_owned()
}

fn port(t: &toml::Table, path: &[&str]) -> u16 {
    let n = get(t, path)
        .and_then(toml::Value::as_integer)
        .unwrap_or_else(|| panic!("fixtures.toml has no integer {}", path.join(".")));
    u16::try_from(n).expect("port")
}

/// The fixture user on `oxbase_test`, TLS disabled.
fn params() -> ConnectParams {
    let f = fixtures();
    let mut p = ConnectParams::network(Engine::MongoDb, s(&f, &["engines", "mongodb", "host"]))
        .with_port(port(&f, &["engines", "mongodb", "port"]))
        .with_user(s(&f, &["engines", "mongodb", "user"]))
        .with_password(s(&f, &["engines", "mongodb", "password"]))
        .with_database(s(&f, &["engines", "mongodb", "database"]))
        .with_tls(TlsOptions::new(TlsMode::Disable));
    p.options.insert(
        options::AUTH_SOURCE.into(),
        s(&f, &["engines", "mongodb", "auth_source"]),
    );
    p.connect_timeout = Some(Duration::from_secs(15));
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

async fn connect(p: &ConnectParams) -> MongoConnection {
    MongoConnection::connect(p)
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"))
}

async fn run(c: &dyn Connection, text: &str) -> Collected {
    collect(
        c.execute(text, &[], CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{text}: {e}")),
    )
    .await
    .unwrap_or_else(|e| panic!("{text}: {e}"))
}

async fn ping(c: &dyn Connection) {
    let out = run(
        c,
        r#"{"type": "command", "database": "admin", "command": {"ping": 1}}"#,
    )
    .await;
    let set = &out.result_sets[0];
    assert_eq!(set.num_rows(), 1);
    let ok = set.batches[0].column(set.schema.index_of("ok").unwrap());
    assert!((ok.as_primitive::<Float64Type>().value(0) - 1.0).abs() < f64::EPSILON);
}

fn find(collection: &str, extra: &str) -> String {
    format!(
        r#"{{"type": "find", "collection": {{"kind": "table", "catalog": "{DATABASE}", "name": "{collection}"}}{extra}}}"#
    )
}

fn aggregate(collection: &str, pipeline: &str) -> String {
    format!(
        r#"{{"type": "aggregate", "collection": {{"kind": "table", "catalog": "{DATABASE}", "name": "{collection}"}}, "pipeline": {pipeline}}}"#
    )
}

// ---- test-owned collections ---------------------------------------------------------------------

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Drops `m7drv_t<epoch>_...` collections older than an hour (leftovers of killed runs).
async fn drop_stale(c: &MongoConnection) {
    let db = c.client().database(DATABASE);
    let names = db.list_collection_names().await.expect("listCollections");
    let now = epoch_secs();
    for name in names {
        let Some(rest) = name.strip_prefix("m7drv_t") else {
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

/// A collection of this test run, dropped by [`Owned::drop_now`].
struct Owned {
    name: String,
}

impl Owned {
    async fn create(c: &MongoConnection, what: &str, docs: Vec<Document>) -> Self {
        drop_stale(c).await;
        let name = format!("m7drv_t{}_{}_{what}", epoch_secs(), std::process::id());
        let coll = c.client().database(DATABASE).collection::<Document>(&name);
        let _ = coll.drop().await;
        for chunk in docs.chunks(10_000) {
            coll.insert_many(chunk.to_vec())
                .await
                .unwrap_or_else(|e| panic!("insert into {name}: {e}"));
        }
        Self { name }
    }

    async fn drop_now(self, c: &MongoConnection) {
        let _ = c
            .client()
            .database(DATABASE)
            .collection::<Document>(&self.name)
            .drop()
            .await;
    }
}

/// `{_id: int32 n, g: n % 100, s: "row-<n>"}` for n in 1..=count (the shape of `m7_big`).
fn big_docs(count: i32) -> Vec<Document> {
    (1..=count)
        .map(|n| doc! {"_id": n, "g": n % 100, "s": format!("row-{n}")})
        .collect()
}

/// `m7_big` when the seed has all of it, else a test-owned copy of its shape.
async fn big_collection(c: &MongoConnection) -> (String, Option<Owned>) {
    let count = c
        .client()
        .database(DATABASE)
        .collection::<Document>("m7_big")
        .estimated_document_count()
        .await
        .unwrap_or(0);
    if count == u64::try_from(BIG_ROWS).unwrap() {
        return ("m7_big".to_owned(), None);
    }
    let owned = Owned::create(c, "big", big_docs(i32::try_from(BIG_ROWS).unwrap())).await;
    (owned.name.clone(), Some(owned))
}

// ---- SSH ------------------------------------------------------------------------------------------

/// Opens a tunnel through the fixture bastion to `mongodb:27017` (password or key auth).
async fn bastion(password: bool) -> (TunnelHandle, SshOptions, String, u16) {
    let f = fixtures();
    let key_dir = in_workspace(&s(&f, &["ssh", "key_dir"]));
    let known_hosts = key_dir.join(s(&f, &["ssh", "known_hosts"]));
    assert!(
        known_hosts.exists(),
        "fixture SSH files missing under {}: start the fixtures (scripts/fixtures-up.sh)",
        key_dir.display()
    );
    let auth = if password {
        SshAuth::Password(Secret::new(s(&f, &["ssh", "password"])))
    } else {
        SshAuth::PrivateKey {
            key: KeySource::File(key_dir.join(s(&f, &["ssh", "client_key"]))),
            passphrase: None,
        }
    };
    let ssh = SshOptions::new(s(&f, &["ssh", "host"]), s(&f, &["ssh", "user"]), auth)
        .with_port(port(&f, &["ssh", "port"]))
        .with_known_hosts(KnownHostsPolicy::Strict {
            path: Some(known_hosts),
        });
    let host = s(&f, &["ssh", "targets", "mongodb", "host"]);
    let target_port = port(&f, &["ssh", "targets", "mongodb", "port"]);
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

/// Whether the fixture certificate names the compose service (`[tls] server_sans`). Until the
/// conformance stream adds it, TLS through the tunnel verifies `localhost` instead: still a
/// name that is not the tunnel's 127.0.0.1 address.
fn certificate_names(name: &str) -> bool {
    let f = fixtures();
    get(&f, &["tls", "server_sans"])
        .and_then(toml::Value::as_array)
        .is_some_and(|sans| sans.iter().any(|v| v.as_str() == Some(name)))
}

// ---- cancel ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum CancelBy {
    Token,
    CancelRunning,
}

/// A find that runs for minutes: a `$where` busy loop of 50 ms per document.
fn slow_find(collection: &str) -> String {
    find(
        collection,
        r#", "filter": {"$where": "function() { var t = Date.now(); while (Date.now() - t < 50) {} return true; }"}"#,
    )
}

/// Runs the slow find, cancels it after 300 ms and checks that the stream ends with
/// `Cancelled`, the session answers, and the server operation is gone, within 2 s.
async fn cancel_once(c: &Arc<MongoConnection>, collection: &str, by: CancelBy) {
    let token = CancellationToken::new();
    let stream = c
        .execute(&slow_find(collection), &[], token.clone())
        .await
        .expect("execute");
    let task = tokio::spawn(collect(stream));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!task.is_finished(), "{by:?}: the slow find finished early");
    let requested = Instant::now();
    match by {
        CancelBy::Token => token.cancel(),
        CancelBy::CancelRunning => c.cancel_running().await.expect("cancel_running"),
    }
    let outcome = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the stream ends")
        .expect("task");
    let took = requested.elapsed();
    let err = outcome.expect_err("cancelled, not completed");
    assert!(err.is_cancelled(), "{by:?}: {err}");
    assert!(took < CANCEL_LIMIT, "{by:?}: cancelled after {took:?}");
    ping(c.as_ref()).await;
    let free = requested.elapsed();
    assert!(
        free < CANCEL_LIMIT,
        "{by:?}: session answered after {free:?}"
    );

    // The server stopped the operation (killOp), not only the client.
    let deadline = Instant::now() + CANCEL_LIMIT;
    loop {
        let out = run(
            c.as_ref(),
            &format!(
                r#"{{"type": "command", "database": "admin", "command": {{"currentOp": true, "$ownOps": true, "ns": "{DATABASE}.{collection}"}}}}"#
            ),
        )
        .await;
        let set = &out.result_sets[0];
        let inprog = set.batches[0].column(set.schema.index_of("inprog").unwrap());
        let text = inprog.as_string::<i32>().value(0).to_owned();
        if text == "[]" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{by:?}: the $where operation still runs: {text}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---- tests ----------------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs the mongodb fixture"]
async fn plain_connect_reports_session_facts() {
    let f = fixtures();
    let p = params();
    let c = connect(&p).await;
    assert_eq!(c.engine(), Engine::MongoDb);
    assert!(!c.tls_in_use());
    let want = ServerVersion::parse(&s(&f, &["engines", "mongodb", "version_prefix"])).unwrap();
    assert_eq!(c.server_version(), &want, "{}", c.server_version().raw());
    assert_eq!(c.current_database().as_deref(), Some(DATABASE));
    let password = s(&f, &["engines", "mongodb", "password"]);
    let debug = format!("{c:?}");
    assert!(!debug.contains(&password), "{debug}");
    let comment = c.next_comment();
    assert!(comment.starts_with("oxbase:"), "{comment}");
    assert_ne!(comment, c.next_comment());
    ping(&c).await;
    assert!(matches!(
        c.begin().await.err(),
        Some(Error::Unsupported { .. })
    ));
    c.close().await.unwrap();
    c.close().await.unwrap();
    let err = c
        .execute(
            r#"{"type": "command", "database": "admin", "command": {"ping": 1}}"#,
            &[],
            CancellationToken::new(),
        )
        .await
        .err()
        .expect("closed");
    assert!(matches!(err, Error::Connection { .. }), "{err}");

    // Through the Driver trait, without a database: the shell's default `test`.
    let mut p = params();
    p.database = None;
    let boxed = MongoDriver::new().connect(&p).await.unwrap();
    assert_eq!(boxed.current_database().as_deref(), Some("test"));
    ping(boxed.as_ref()).await;
    boxed.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the mongodb fixture"]
async fn tls_modes_verify_against_the_fixture_ca() {
    let full = connect(&params().with_tls(verify(TlsMode::VerifyFull, "localhost"))).await;
    assert!(full.tls_in_use());
    ping(&full).await;
    full.close().await.unwrap();

    // Verified against 127.0.0.1 (an IP SAN) when no name is given.
    let ip =
        connect(&params().with_tls(TlsOptions::new(TlsMode::VerifyFull).with_ca_cert(ca_cert())))
            .await;
    assert!(ip.tls_in_use());
    ip.close().await.unwrap();

    let started = Instant::now();
    let wrong =
        MongoConnection::connect(&params().with_tls(verify(TlsMode::VerifyFull, WRONG_NAME)))
            .await
            .expect_err("a wrong server name is refused");
    assert!(matches!(wrong, Error::Connection { .. }), "{wrong}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "refused at once, not after server selection: {:?}",
        started.elapsed()
    );

    let ca = connect(&params().with_tls(verify(TlsMode::VerifyCa, WRONG_NAME))).await;
    assert!(ca.tls_in_use(), "VerifyCa does not check the name");
    ping(&ca).await;
    ca.close().await.unwrap();

    for mode in [TlsMode::Prefer, TlsMode::Require] {
        let c = connect(&params().with_tls(TlsOptions::new(mode))).await;
        assert!(c.tls_in_use(), "{mode:?}: the fixture offers TLS");
        ping(&c).await;
        c.close().await.unwrap();
    }

    // Mutual TLS: the fixture accepts (does not require) the client certificate.
    let f = fixtures();
    let dir = in_workspace(&s(&f, &["tls", "dir"]));
    let mtls = verify(TlsMode::VerifyFull, "localhost").with_client_identity(
        PemSource::File(dir.join(s(&f, &["tls", "client_cert"]))),
        KeySource::File(dir.join(s(&f, &["tls", "client_key"]))),
    );
    let c = connect(&params().with_tls(mtls)).await;
    assert!(c.tls_in_use());
    ping(&c).await;
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the mongodb fixture"]
async fn scram_mechanisms_and_a_wrong_password() {
    for mechanism in ["SCRAM-SHA-256", "SCRAM-SHA-1"] {
        let mut p = params();
        p.options
            .insert(options::AUTH_MECHANISM.into(), mechanism.into());
        let c = connect(&p).await;
        ping(&c).await;
        c.close().await.unwrap();
    }

    let marker = "Wrong_Marker_Pw_4711";
    let p = params().with_password(marker);
    let err = MongoConnection::connect(&p)
        .await
        .expect_err("a wrong password is refused");
    assert!(matches!(err, Error::Auth { .. }), "{err}");
    let shown = format!("{err} {err:?}");
    assert!(!shown.contains(marker), "{shown}");

    // A user that does not exist in the auth source fails the same way.
    let mut p = params();
    p.options
        .insert(options::AUTH_SOURCE.into(), DATABASE.into());
    let err = MongoConnection::connect(&p).await.expect_err("refused");
    assert!(matches!(err, Error::Auth { .. }), "{err}");
}

#[tokio::test]
#[ignore = "needs the mongodb fixture"]
async fn json_requests_run_through_execute() {
    let c = connect(&params()).await;
    let owned = Owned::create(
        &c,
        "requests",
        vec![
            doc! {"_id": 1, "name": "a", "n": 1},
            doc! {"_id": 2, "name": "b", "n": 2_i64, "extra": true},
            doc! {"_id": 3, "name": "c", "n": 3},
        ],
    )
    .await;
    let name = owned.name.clone();

    let out = run(
        &c,
        &find(
            &name,
            r#", "filter": {"n": {"$gte": {"$numberLong": "2"}}}, "projection": {"n": 1, "name": 1}, "sort": {"_id": -1}"#,
        ),
    )
    .await;
    let set = &out.result_sets[0];
    let names: Vec<&str> = set
        .schema
        .fields()
        .iter()
        .map(|f| f.name().as_str())
        .collect();
    assert_eq!(names, ["_id", "n", "name", "$document"]);
    assert_eq!(set.num_rows(), 2);
    let batch = &set.batches[0];
    assert_eq!(batch.column(0).as_primitive::<Int32Type>().value(0), 3);
    assert_eq!(batch.column(1).data_type(), &DataType::Int64, "int + long");
    assert_eq!(batch.column(1).as_primitive::<Int64Type>().value(1), 2);
    let document = batch.column(3).as_string::<i32>().value(0);
    assert!(document.contains(r#""$numberInt":"3""#), "{document}");

    // The $regex operator, skip and limit.
    let out = run(
        &c,
        &find(
            &name,
            r#", "filter": {"name": {"$regex": "^[ab]$"}}, "sort": {"_id": 1}, "skip": 1, "limit": 5"#,
        ),
    )
    .await;
    assert_eq!(out.result_sets[0].num_rows(), 1);

    let out = run(
        &c,
        &aggregate(
            &name,
            r#"[{"$group": {"_id": null, "total": {"$sum": "$n"}}}]"#,
        ),
    )
    .await;
    let set = &out.result_sets[0];
    let total = set.batches[0].column(set.schema.index_of("total").unwrap());
    assert_eq!(total.as_primitive::<Int64Type>().value(0), 6);
    assert!(set.batches[0].column(0).is_null(0), "_id: null");

    let out = run(
        &c,
        &format!(
            r#"{{"type": "command", "database": "{DATABASE}", "command": {{"count": "{name}"}}}}"#
        ),
    )
    .await;
    let set = &out.result_sets[0];
    let n = set.batches[0].column(set.schema.index_of("n").unwrap());
    assert_eq!(n.as_primitive::<Int32Type>().value(0), 3);

    // Server errors keep their code name; requests are checked before they are sent.
    let err = collect(
        c.execute(
            &find(&name, r#", "filter": {"$bogus": 1}"#),
            &[],
            CancellationToken::new(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.sql_code(), Some("BadValue"), "{err}");
    let bad = c
        .execute(r#"{"type": "find"}"#, &[], CancellationToken::new())
        .await
        .err()
        .expect("invalid");
    assert!(matches!(bad, Error::InvalidInput { .. }), "{bad}");
    let params_err = c
        .execute(
            &find(&name, ""),
            &[oxbase_types::Value::Int(1)],
            CancellationToken::new(),
        )
        .await
        .err()
        .expect("no bound parameters");
    assert!(matches!(params_err, Error::InvalidInput { .. }));
    let token = CancellationToken::new();
    token.cancel();
    assert!(
        c.execute(&find(&name, ""), &[], token)
            .await
            .err()
            .expect("already cancelled")
            .is_cancelled()
    );

    owned.drop_now(&c).await;
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the mongodb fixture"]
async fn a_200k_document_find_streams_every_row() {
    let c = connect(&params()).await;
    let (name, owned) = big_collection(&c).await;
    let started = Instant::now();
    let mut stream = c
        .execute(&find(&name, ""), &[], CancellationToken::new())
        .await
        .unwrap();
    let (mut rows, mut batches, mut schemas) = (0usize, 0usize, 0usize);
    while let Some(item) = stream.next().await {
        match item.unwrap() {
            ResultItem::Schema(schema) => {
                schemas += 1;
                let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
                assert_eq!(names, ["_id", "g", "s", "$document"]);
            }
            ResultItem::Batch(b) => {
                rows += b.num_rows();
                batches += 1;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(schemas, 1);
    assert_eq!(rows, BIG_ROWS);
    assert!(batches >= BIG_ROWS / 1000, "{batches} batches");
    eprintln!("{rows} rows in {batches} batches, {:?}", started.elapsed());
    if let Some(owned) = owned {
        owned.drop_now(&c).await;
    }
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the mongodb fixture"]
async fn cancel_stops_the_server_operation_within_two_seconds() {
    let c = Arc::new(connect(&params()).await);
    let owned = Owned::create(&c, "cancel", big_docs(5000)).await;
    cancel_once(&c, &owned.name, CancelBy::Token).await;
    cancel_once(&c, &owned.name, CancelBy::CancelRunning).await;
    // Nothing running: cancel_running is a no-op.
    c.cancel_running().await.unwrap();
    ping(c.as_ref()).await;
    owned.drop_now(&c).await;
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the mongodb fixture and the SSH bastion"]
async fn ssh_tunnels_with_password_and_key_and_tls_by_service_name() {
    // Password auth, plain MongoDB through the tunnel.
    let (tunnel, ssh, host, target_port) = bastion(true).await;
    let mut p = params().with_ssh(ssh).with_dial_addr(tunnel.local_addr());
    p.host = Some(host.clone());
    p.port = Some(target_port);
    let c = connect(&p).await;
    assert!(!c.tls_in_use());
    ping(&c).await;
    c.close().await.unwrap();
    drop(tunnel);

    // Key auth, VerifyFull for the service name while dialling the tunnel's 127.0.0.1.
    let (tunnel, ssh, host, target_port) = bastion(false).await;
    let mut tls = TlsOptions::new(TlsMode::VerifyFull).with_ca_cert(ca_cert());
    if !certificate_names(&host) {
        tls = tls.with_server_name("localhost");
    }
    let mut p = params()
        .with_ssh(ssh)
        .with_dial_addr(tunnel.local_addr())
        .with_tls(tls);
    p.host = Some(host.clone());
    p.port = Some(target_port);
    let direct = connect(&params()).await;
    let owned = Owned::create(&direct, "ssh_cancel", big_docs(5000)).await;
    let c = Arc::new(connect(&p).await);
    assert!(c.tls_in_use());
    ping(c.as_ref()).await;
    // Cancel's side connection goes through the same tunnel and TLS.
    cancel_once(&c, &owned.name, CancelBy::Token).await;
    cancel_once(&c, &owned.name, CancelBy::CancelRunning).await;
    c.close().await.unwrap();

    // A wrong name through the tunnel is refused.
    let mut wrong = p.clone();
    wrong.tls = verify(TlsMode::VerifyFull, WRONG_NAME);
    let err = MongoConnection::connect(&wrong)
        .await
        .expect_err("wrong name refused");
    assert!(matches!(err, Error::Connection { .. }), "{err}");

    owned.drop_now(&direct).await;
    direct.close().await.unwrap();
    drop(tunnel);
}
