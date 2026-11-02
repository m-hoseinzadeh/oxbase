//! M7 exit check for Redis through the services (docs/m7-contracts.md §11, redis): *browse* the
//! tree (databases 0..15, database 1's key, the namespace tree under `m7:` with key types and
//! TTLs), *query* (`KeyService::open_scan` of `m7:*` lists every seed key once,
//! `KeyService::open_value` of each type and the 200 000-element list and sorted set, paged at
//! random offsets), *edit* (`KeyService::apply`, a destructive edit refused without
//! confirmation) and pub/sub (`PubSubService` subscribe, publish, unsubscribe), against the
//! `redis` fixture and its M7 seed.
//!
//! `#[ignore]`d: the conformance CI job runs it with `--include-ignored`. Until M7 integration
//! registers the Redis cells in `default_dialects`, the test registers them itself
//! (`oxbase_redis::dialect::register`); until the shell stream fills the console language, it
//! stands in a one-line-per-statement language so `run_source` can name its statement.
//! Without the M7 seed the test is pending unless `OXBASE_SERVICES_REQUIRE_M7=1`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int64Type};
use arrow_array::{Array, RecordBatch};
use oxbase_dialect::script::{statement, trim_range};
use oxbase_dialect::{DialectRegistry, PubSubEvent, ScriptLanguage, SubscribeRequest};
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_services::history::HistoryService;
use oxbase_services::keys::KeyService;
use oxbase_services::pubsub::PubSubService;
use oxbase_services::query::{
    EditorId, QueryConfig, QueryService, RunEvent, RunId, RunOptions, RunTarget,
};
use oxbase_services::tree::{TreeNode, TreeNodeRef, TreeOptions, TreeService};
use oxbase_services::{
    CancellationToken, ConnectionManager, ConnectionProfile, ProvidedSecrets, SecretRef,
    SecretStore, SessionId, default_dialects, default_drivers,
};
use oxbase_sql::{Statement, StatementKind};
use oxbase_store::{
    MemoryHistoryRepository, MemorySecretStore, ProfileAddress, Secret, SecretSlot, TlsMode,
    TlsSettings,
};
use oxbase_types::{
    BinaryText, Engine, ErrorCategory, KeyAddress, KeyApplyRequest, KeyEdit, KeyScanRequest,
    KeyType, ObjectKind, ObjectRef, Value, props,
};
use tokio::sync::mpsc;

const ROWS: u64 = 200_000;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn fixtures() -> toml::Table {
    let path = std::env::var_os("OXBASE_FIXTURES")
        .map(PathBuf::from)
        .map_or_else(|| root().join("tests/fixtures.toml"), |p| root().join(p));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.parse().expect("fixtures.toml parses")
}

fn get<'a>(t: &'a toml::Table, path: &[&str]) -> &'a toml::Value {
    let (last, parents) = path.split_last().unwrap();
    let mut table = t;
    for key in parents {
        table = table
            .get(*key)
            .and_then(toml::Value::as_table)
            .unwrap_or_else(|| panic!("fixtures.toml: no table {key}"));
    }
    table
        .get(*last)
        .unwrap_or_else(|| panic!("fixtures.toml: no {}", path.join(".")))
}

fn string(t: &toml::Table, path: &[&str]) -> String {
    get(t, path).as_str().expect("a string").to_owned()
}

fn integer(t: &toml::Table, path: &[&str]) -> i64 {
    get(t, path).as_integer().expect("an integer")
}

fn token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:x}", nanos ^ u128::from(std::process::id()))
        .chars()
        .rev()
        .take(10)
        .collect()
}

/// One statement per non-blank line: stands in for the console language until the shell
/// stream's `RedisConsole` lands (the runs here have one-line descriptions only).
struct Lines;

impl ScriptLanguage for Lines {
    fn name(&self) -> &'static str {
        "redis-console"
    }
    fn editor_language(&self) -> &'static str {
        "plaintext"
    }
    fn split(&self, text: &str) -> Vec<Statement> {
        let mut out = Vec::new();
        let mut offset = 0;
        for line in text.split_inclusive('\n') {
            let range = offset..offset + line.len();
            offset = range.end;
            if let Some(trimmed) = trim_range(text, range.clone()) {
                out.push(statement(text, trimmed, range.end, StatementKind::Other));
            }
        }
        out
    }
    fn redact_secrets<'a>(&self, text: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(text)
    }
}

/// The registry with the Redis cells (see the module docs).
fn dialects() -> DialectRegistry {
    let mut reg = default_dialects();
    if reg.key_value_store(Engine::Redis).is_err() {
        oxbase_redis::dialect::register(&mut reg);
    }
    let console = reg.script_language(Engine::Redis).unwrap();
    let ready = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        console.split("PING");
    }))
    .is_ok();
    if !ready {
        eprintln!("the Redis console language is not implemented yet: a stand-in splits lines");
        reg.register_script_language(Engine::Redis, Arc::new(Lines));
    }
    reg
}

struct Live {
    fx: toml::Table,
    _spill: tempfile::TempDir,
    manager: Arc<ConnectionManager>,
    query: Arc<QueryService>,
    tree: Arc<TreeService>,
    keys: KeyService,
    pubsub: PubSubService,
    session: SessionId,
}

impl Live {
    async fn open() -> Self {
        let fx = fixtures();
        let secrets = Arc::new(MemorySecretStore::new());
        let manager = Arc::new(ConnectionManager::new(
            default_drivers(),
            secrets.clone(),
            Arc::new(RusshTunnelProvider::new()),
        ));
        let dialects = Arc::new(dialects());
        let spill = tempfile::tempdir().unwrap();
        let query = Arc::new(QueryService::new(
            Arc::clone(&manager),
            Arc::clone(&dialects),
            Arc::new(HistoryService::new(
                Arc::new(MemoryHistoryRepository::new()),
            )),
            QueryConfig {
                spill_dir: spill.path().join("results"),
                memory_budget_bytes: 4 * 1024 * 1024,
                ..QueryConfig::default()
            },
        ));
        let tree = Arc::new(TreeService::new(
            Arc::clone(&manager),
            Arc::clone(&dialects),
        ));
        let keys = KeyService::new(Arc::clone(&query), Arc::clone(&tree));
        let pubsub = PubSubService::new(Arc::clone(&manager), Arc::clone(&dialects));

        let mut p = ConnectionProfile::new(
            "m7 redis live",
            Engine::Redis,
            ProfileAddress::Network {
                host: string(&fx, &["engines", "redis", "host"]),
                port: Some(u16::try_from(integer(&fx, &["engines", "redis", "port"])).unwrap()),
            },
        );
        p.user = Some(string(&fx, &["engines", "redis", "user"]));
        p.database = Some("0".to_owned());
        let key = p.secret_key(SecretSlot::Password);
        secrets
            .set(
                &key,
                &Secret::new(string(&fx, &["engines", "redis", "password"])),
            )
            .await
            .unwrap();
        p.password = Some(SecretRef::Keychain { key });
        p.tls = TlsSettings {
            mode: TlsMode::Disable,
            ..TlsSettings::default()
        };
        p.connect_timeout_secs = Some(15);
        let session = manager
            .open(&p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("open the redis profile: {e}"))
            .id();
        Self {
            fx,
            _spill: spill,
            manager,
            query,
            tree,
            keys,
            pubsub,
            session,
        }
    }

    fn target(&self, editor: &str) -> RunTarget {
        RunTarget {
            session: self.session,
            editor: EditorId::new(editor),
        }
    }

    async fn children(&self, node: TreeNodeRef) -> Vec<TreeNode> {
        self.tree
            .children(
                self.session,
                &node,
                &TreeOptions::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|e| panic!("children of {}: {e}", node.key()))
    }

    async fn object_children(&self, object: &ObjectRef) -> HashMap<String, TreeNode> {
        self.children(TreeNodeRef::Object {
            object: object.clone(),
        })
        .await
        .into_iter()
        .map(|n| (n.label.clone(), n))
        .collect()
    }

    /// Whether the M7 seed is there; else pending (or a failure when required).
    async fn seeded(&self) -> bool {
        let info = self
            .keys
            .info(
                self.session,
                &KeyAddress::new("0", BinaryText::text("m7:string")),
                CancellationToken::new(),
            )
            .await;
        match info {
            Ok(_) => true,
            Err(e) if e.category() == ErrorCategory::NotFound => {
                assert!(
                    std::env::var("OXBASE_SERVICES_REQUIRE_M7").as_deref() != Ok("1"),
                    "the M7 Redis seed is missing and OXBASE_SERVICES_REQUIRE_M7=1"
                );
                eprintln!("pending: the M7 Redis seed is not loaded");
                false
            }
            Err(e) => panic!("key info: {e}"),
        }
    }

    async fn close(self) {
        self.manager.close(self.session).await.unwrap();
    }
}

/// Drains a run's events: the total rows of each result and the terminal event.
async fn finish(mut events: mpsc::Receiver<RunEvent>) -> (Vec<u64>, RunEvent) {
    let mut totals = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(120), events.recv())
        .await
        .expect("run events within 2 minutes")
    {
        match event {
            RunEvent::ResultDone { total_rows, .. } => totals.push(total_rows),
            RunEvent::StatementFailed { ref error, .. } => panic!("statement failed: {error:?}"),
            e if e.is_terminal() => return (totals, e),
            _ => {}
        }
    }
    panic!("the run ended without a terminal event");
}

async fn open(
    live: &Live,
    key: &str,
) -> oxbase_types::Result<(oxbase_services::query::RunHandle, oxbase_types::KeyInfo)> {
    live.keys
        .open_value(
            live.target("value"),
            &KeyAddress::new("0", BinaryText::text(key)),
            RunOptions::default(),
            CancellationToken::new(),
        )
        .await
}

async fn next(events: &mut mpsc::Receiver<PubSubEvent>) -> PubSubEvent {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("an event within 5 s")
        .expect("the event channel is open")
}

async fn page(live: &Live, run: RunId, offset: u64, limit: usize) -> RecordBatch {
    live.query
        .page(run, 0, offset, limit)
        .await
        .unwrap_or_else(|e| panic!("page {offset}+{limit}: {e}"))
}

fn texts(b: &RecordBatch, col: &str) -> Vec<String> {
    let a = b.column_by_name(col).unwrap().as_string::<i32>();
    (0..a.len()).map(|i| a.value(i).to_owned()).collect()
}

fn bytes(b: &RecordBatch, col: &str) -> Vec<Vec<u8>> {
    let a = b.column_by_name(col).unwrap().as_binary::<i32>();
    (0..a.len()).map(|i| a.value(i).to_vec()).collect()
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn redis_browse_query_edit_and_pubsub_through_the_services() {
    let live = Live::open().await;
    if !live.seeded().await {
        live.close().await;
        return;
    }
    let fx = live.fx.clone();

    // ---- browse ----
    let root = live.children(TreeNodeRef::Connection).await;
    let databases: Vec<&TreeNode> = root
        .iter()
        .filter(|n| matches!(&n.node, TreeNodeRef::Object { object } if object.kind == ObjectKind::Database))
        .collect();
    let expected = usize::try_from(integer(&fx, &["m7", "redis", "databases"])).unwrap();
    assert_eq!(databases.len(), expected);
    assert!(
        root.iter().any(
            |n| matches!(&n.node, TreeNodeRef::Folder { kind, .. } if *kind == ObjectKind::User)
        ),
        "a Users folder"
    );
    let users = live
        .children(TreeNodeRef::Folder {
            parent: ObjectRef::root(),
            kind: ObjectKind::User,
        })
        .await;
    let acl_user = string(&fx, &["m7", "redis", "acl_user"]);
    assert!(
        users.iter().any(|n| n.label == acl_user),
        "{acl_user} listed"
    );
    // Database 1 is read on its own child session.
    let db1 = live.object_children(&ObjectRef::database("1")).await;
    let m7_db1 = &db1["m7:"];
    let TreeNodeRef::Object { object } = &m7_db1.node else {
        panic!("an object node");
    };
    let under = live.object_children(object).await;
    let TreeNodeRef::Object { object } = &under["m7:db1:"].node else {
        panic!("an object node");
    };
    let db1_keys = live.object_children(object).await;
    assert!(db1_keys.contains_key(&string(&fx, &["m7", "redis", "db1_key"])));
    // The namespace tree of database 0.
    let db0 = live.object_children(&ObjectRef::database("0")).await;
    let m7 = &db0["m7:"];
    assert!(m7.expandable);
    let TreeNodeRef::Object { object: m7_ref } = &m7.node else {
        panic!("an object node");
    };
    let m7_children = live.object_children(m7_ref).await;
    let ns = &m7_children["m7:ns:"];
    assert_eq!(
        ns.summary.as_ref().unwrap().extras.get(&props::KEY_COUNT),
        Some(&Value::Int(4))
    );
    let ttl_key = string(&fx, &["m7", "redis", "ttl_key"]);
    let ttl_row = m7_children[&ttl_key].summary.as_ref().unwrap();
    assert!(matches!(ttl_row.extras.get(&props::TTL_MS), Some(Value::Int(ms)) if *ms > 0));
    assert_eq!(
        m7_children["m7:hash"]
            .summary
            .as_ref()
            .unwrap()
            .extras
            .get(&props::KEY_TYPE),
        Some(&Value::Text("hash".into()))
    );
    assert!(!m7_children["m7:hash"].expandable);

    // ---- query: the key browser ----
    let mut scan = KeyScanRequest::all("0");
    scan.pattern = Some("m7:*".to_owned());
    let run = live
        .keys
        .open_scan(
            live.target("keys"),
            &scan,
            RunOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let (totals, end) = finish(run.events).await;
    assert!(matches!(end, RunEvent::Finished { .. }), "{end:?}");
    let all = page(&live, run.id, 0, 10_000).await;
    assert_eq!(u64::try_from(all.num_rows()).unwrap(), totals[0]);
    let names = texts(&all, "key_text");
    let unique: HashSet<&String> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "every key once");
    for key in fx["m7"]["redis"]["keys"].as_array().unwrap() {
        let key = key.as_str().unwrap().to_owned();
        assert!(unique.contains(&key), "{key} missing");
    }

    // ---- query: typed values ----
    let (run, info) = open(&live, "m7:hash").await.unwrap();
    assert_eq!(info.key_type, KeyType::Hash);
    let (totals, _) = finish(run.events).await;
    assert_eq!(totals, [3]);
    let b = page(&live, run.id, 0, 10).await;
    let mut fields = bytes(&b, "field");
    fields.sort();
    assert_eq!(fields, [b"f1".to_vec(), b"f2".to_vec(), b"f3".to_vec()]);
    let (run, _) = open(&live, "m7:binary").await.unwrap();
    finish(run.events).await;
    assert_eq!(
        bytes(&page(&live, run.id, 0, 1).await, "value"),
        [vec![0, 0xff, 0xfe, 0x0a]]
    );
    let (run, _) = open(&live, "m7:stream").await.unwrap();
    let (totals, _) = finish(run.events).await;
    assert_eq!(totals, [2]);

    for (key, column) in [
        (string(&fx, &["m7", "redis", "big_list"]), "index"),
        (string(&fx, &["m7", "redis", "big_zset"]), "score"),
    ] {
        let (run, info) = open(&live, &key).await.unwrap();
        assert_eq!(info.length, Some(ROWS));
        let (totals, end) = finish(run.events).await;
        assert!(matches!(end, RunEvent::Finished { .. }), "{key}: {end:?}");
        assert_eq!(totals, [ROWS], "{key}");
        for offset in [0_u64, 1, 999, 1000, 54_321, 123_456, ROWS - 1] {
            let b = page(&live, run.id, offset, 3).await;
            assert!(b.num_rows() >= 1, "{key} at {offset}");
            let expected = offset + 1;
            if column == "index" {
                let index = b
                    .column_by_name("index")
                    .unwrap()
                    .as_primitive::<Int64Type>();
                assert_eq!(index.value(0), i64::try_from(offset).unwrap());
                assert_eq!(bytes(&b, "value")[0], format!("e{expected}").into_bytes());
            } else {
                let score = b
                    .column_by_name("score")
                    .unwrap()
                    .as_primitive::<Float64Type>();
                let want = f64::from(u32::try_from(expected).unwrap());
                assert_eq!(score.value(0).to_bits(), want.to_bits());
                assert_eq!(bytes(&b, "member")[0], format!("m{expected}").into_bytes());
            }
        }
    }
    let missing = open(&live, &format!("m7:missing:{}", token()))
        .await
        .unwrap_err();
    assert_eq!(missing.category(), ErrorCategory::NotFound);

    // ---- edit ----
    let tok = token();
    let key = format!("m7:edit:t{tok}:svc");
    let create = KeyApplyRequest {
        address: KeyAddress::new("0", BinaryText::text(&key)),
        key_type: None,
        create: true,
        edits: vec![
            KeyEdit::ZsetAdd {
                member: BinaryText::text("a"),
                score: 1.0,
            },
            KeyEdit::Expire {
                ttl_ms: Some(600_000),
            },
        ],
        confirm_destructive: false,
    };
    let report = live
        .keys
        .apply(live.session, &create, CancellationToken::new())
        .await
        .unwrap();
    assert!(report.committed, "{report:?}");
    assert_eq!(report.info.as_ref().unwrap().key_type, KeyType::Zset);
    let conflict = live
        .keys
        .apply(live.session, &create, CancellationToken::new())
        .await
        .unwrap();
    assert!(!conflict.committed && conflict.conflict, "{conflict:?}");
    let mut delete = KeyApplyRequest {
        address: create.address.clone(),
        key_type: Some(KeyType::Zset),
        create: false,
        edits: vec![KeyEdit::Delete],
        confirm_destructive: false,
    };
    let err = live
        .keys
        .apply(live.session, &delete, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    delete.confirm_destructive = true;
    let report = live
        .keys
        .apply(live.session, &delete, CancellationToken::new())
        .await
        .unwrap();
    assert!(report.committed && report.info.is_none(), "{report:?}");
    // The same edits on database 1 go to its own session.
    let on_db1 = KeyApplyRequest {
        address: KeyAddress::new("1", BinaryText::text(&key)),
        key_type: None,
        create: true,
        edits: vec![KeyEdit::SetString {
            value: BinaryText::text("one"),
            keep_ttl: false,
        }],
        confirm_destructive: true,
    };
    assert!(
        live.keys
            .apply(live.session, &on_db1, CancellationToken::new())
            .await
            .unwrap()
            .committed
    );
    let db1_info = live
        .keys
        .info(live.session, &on_db1.address, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(db1_info.key_type, KeyType::String);
    let gone = live
        .keys
        .info(
            live.session,
            &KeyAddress::new("0", BinaryText::text(&key)),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(gone.category(), ErrorCategory::NotFound);
    let mut cleanup = on_db1.clone();
    cleanup.create = false;
    cleanup.edits = vec![KeyEdit::Delete];
    live.keys
        .apply(live.session, &cleanup, CancellationToken::new())
        .await
        .unwrap();

    // ---- pub/sub ----
    let channel = string(&fx, &["m7", "redis", "pubsub_channel"]);
    let pattern = string(&fx, &["m7", "redis", "pubsub_pattern"]);
    let mut subscription = live
        .pubsub
        .subscribe(
            live.session,
            &SubscribeRequest {
                channels: vec![BinaryText::text(&channel)],
                patterns: vec![BinaryText::text(&pattern)],
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        next(&mut subscription.events).await,
        PubSubEvent::Subscribed { .. }
    ));
    let receivers = live
        .pubsub
        .publish(
            live.session,
            &BinaryText::text(&channel),
            &BinaryText::text("hello"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(receivers >= 2, "{receivers}");
    for _ in 0..2 {
        assert!(matches!(
            next(&mut subscription.events).await,
            PubSubEvent::Message { .. }
        ));
    }
    assert_eq!(live.pubsub.open_count(), 1);
    assert!(live.pubsub.unsubscribe(subscription.id).await.unwrap());
    assert_eq!(
        next(&mut subscription.events).await,
        PubSubEvent::Closed { error: None }
    );
    assert!(!live.pubsub.unsubscribe(subscription.id).await.unwrap());
    // close_session ends what is left.
    let second = live
        .pubsub
        .subscribe(
            live.session,
            &SubscribeRequest {
                channels: vec![BinaryText::text(&channel)],
                patterns: Vec::new(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(live.pubsub.close_session(live.session).await, 1);
    drop(second);
    assert_eq!(live.pubsub.open_count(), 0);

    live.close().await;
}
