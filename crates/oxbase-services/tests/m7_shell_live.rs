//! Live tests of the mongo shell and the Redis console through `QueryService` (docs/m7-contracts.md
//! §4, §11 shell): the editor languages split, guard and redact the scripts, statements run,
//! page and cancel like SQL ones, and history keeps them redacted.
//!
//! The registry here registers only the two editor languages (`oxbase_mongo::shell::MongoShell`,
//! `oxbase_redis::console::RedisConsole`) on top of `default_dialects()`: the engines' other
//! cells are registered at M7 integration and are not needed for a query run.
//!
//! All are `#[ignore]`d; the conformance CI job runs them with `--include-ignored`. The tests
//! write only collections / keys of their own and remove them at the end.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arrow_array::Array;
use arrow_array::cast::AsArray;
use oxbase_dialect::CONFIRMATION_REQUIRED;
use oxbase_mongo::shell::MongoShell;
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_redis::console::RedisConsole;
use oxbase_services::history::HistoryService;
use oxbase_services::query::{
    EditorId, QueryConfig, QueryService, RunEvent, RunHandle, RunOptions, RunTarget,
};
use oxbase_services::{
    CancellationToken, ConnectionManager, ProvidedSecrets, default_dialects, default_drivers,
};
use oxbase_store::{
    ConnectionProfile, HistoryQuery, HistoryStatus, MemoryHistoryRepository, MemorySecretStore,
    ProfileAddress, Secret, SecretRef, SecretSlot, SecretStore, TlsMode, TlsSettings,
};
use oxbase_types::{Engine, ErrorCategory};

const CANCEL_LIMIT: Duration = Duration::from_secs(2);

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

fn engine_value(key: &str, field: &str) -> String {
    let f = fixtures();
    let value = f
        .get("engines")
        .and_then(|e| e.get(key))
        .and_then(|e| e.get(field))
        .unwrap_or_else(|| panic!("fixtures.toml has no engines.{key}.{field}"));
    match value {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{:x}{:x}", nanos % 0xffff_ffff, std::process::id())
}

struct Live {
    service: QueryService,
    history: Arc<HistoryService>,
    target: RunTarget,
    _dirs: tempfile::TempDir,
}

impl Live {
    async fn connect(key: &str, engine: Engine) -> Self {
        let dirs = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecretStore::new());
        let port: u16 = engine_value(key, "port").parse().expect("port");
        let mut p = ConnectionProfile::new(
            format!("m7 shell {key}"),
            engine,
            ProfileAddress::Network {
                host: engine_value(key, "host"),
                port: Some(port),
            },
        );
        p.user = Some(engine_value(key, "user"));
        p.database = Some(engine_value(key, "database"));
        let secret = p.secret_key(SecretSlot::Password);
        secrets
            .set(&secret, &Secret::new(engine_value(key, "password")))
            .await
            .unwrap();
        p.password = Some(SecretRef::Keychain { key: secret });
        p.tls = TlsSettings {
            mode: TlsMode::Disable,
            ..TlsSettings::default()
        };
        p.connect_timeout_secs = Some(15);
        if engine == Engine::MongoDb {
            p.options.insert(
                oxbase_mongo::options::AUTH_SOURCE.into(),
                engine_value(key, "auth_source"),
            );
        }
        let manager = Arc::new(ConnectionManager::new(
            default_drivers(),
            secrets,
            Arc::new(RusshTunnelProvider::new()),
        ));
        let base = manager
            .open(&p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{key}: connect failed: {e}"));
        let mut dialects = default_dialects();
        dialects.register_script_language(Engine::MongoDb, Arc::new(MongoShell));
        dialects.register_script_language(Engine::Redis, Arc::new(RedisConsole));
        let history = Arc::new(HistoryService::new(
            Arc::new(MemoryHistoryRepository::new()),
        ));
        let config = QueryConfig {
            spill_dir: dirs.path().join("results"),
            ..QueryConfig::default()
        };
        let service = QueryService::new(manager, Arc::new(dialects), Arc::clone(&history), config);
        Self {
            service,
            history,
            target: RunTarget {
                session: base.id(),
                editor: EditorId::new("m7-shell"),
            },
            _dirs: dirs,
        }
    }

    async fn start(&self, script: &str, options: RunOptions) -> oxbase_types::Result<RunHandle> {
        self.service.run(self.target.clone(), script, options).await
    }

    /// Runs `script` to its end; returns every event.
    async fn run(&self, script: &str, options: RunOptions) -> (RunHandle, Vec<RunEvent>) {
        let mut handle = self
            .start(script, options)
            .await
            .unwrap_or_else(|e| panic!("{script}: {e}"));
        let events = drain(&mut handle).await;
        (handle, events)
    }
}

async fn drain(handle: &mut RunHandle) -> Vec<RunEvent> {
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(60), handle.events.recv())
            .await
            .expect("the run stalled")
            .expect("the events ended before a terminal event");
        let terminal = matches!(
            event,
            RunEvent::Finished { .. } | RunEvent::Failed { .. } | RunEvent::Cancelled { .. }
        );
        events.push(event);
        if terminal {
            return events;
        }
    }
}

fn finished(events: &[RunEvent]) -> bool {
    matches!(events.last(), Some(RunEvent::Finished { .. }))
}

fn rows_affected(events: &[RunEvent]) -> Vec<Option<u64>> {
    events
        .iter()
        .filter_map(|e| match e {
            RunEvent::StatementDone { rows_affected, .. } => Some(*rows_affected),
            _ => None,
        })
        .collect()
}

fn totals(events: &[RunEvent]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|e| match e {
            RunEvent::ResultDone { total_rows, .. } => Some(*total_rows),
            _ => None,
        })
        .collect()
}

fn confirm_options() -> RunOptions {
    RunOptions {
        confirm_destructive: true,
        ..RunOptions::default()
    }
}

#[tokio::test]
#[ignore = "needs the mongodb fixture"]
async fn mongo_shell_scripts_run_page_cancel_and_record_redacted_history() {
    let live = Live::connect("mongodb", Engine::MongoDb).await;
    let coll = format!("m7shs_t{}", token());

    // A script of several statements on one editor session.
    let script = format!(
        "// fill\n\
         db.{coll}.insertMany(Array.from({{length: 1000}}, (_, i) => ({{_id: i + 1, g: (i + 1) % 100}})));\n\
         var seven = db.{coll}.find({{g: 7}})\n  .sort({{_id: 1}})\n\
         db.{coll}.countDocuments({{}})\n\
         seven.limit(3)\n"
    );
    let (handle, events) = live.run(&script, RunOptions::default()).await;
    assert!(finished(&events), "{events:?}");
    let Some(RunEvent::Started { statements, .. }) = events.first() else {
        panic!("{events:?}");
    };
    assert_eq!(statements.len(), 4);
    assert_eq!(rows_affected(&events)[0], Some(1000));
    // insertMany's one-row result, the count, the three documents.
    assert_eq!(totals(&events), [1, 1, 3]);
    let page = live.service.page(handle.id, 2, 0, 10).await.unwrap();
    let ids = page.column(page.schema().index_of("_id").unwrap());
    assert_eq!(ids.len(), 3);
    assert_eq!(
        ids.as_primitive::<arrow_array::types::Int32Type>()
            .values()
            .to_vec(),
        [7, 107, 207]
    );

    // A whole collection pages at an offset.
    let (handle, events) = live
        .run(
            &format!("db.{coll}.find().sort({{_id: 1}})"),
            RunOptions::default(),
        )
        .await;
    assert_eq!(totals(&events), [1000]);
    let page = live.service.page(handle.id, 0, 500, 5).await.unwrap();
    let ids = page.column(page.schema().index_of("_id").unwrap());
    assert_eq!(
        ids.as_primitive::<arrow_array::types::Int32Type>()
            .values()
            .to_vec(),
        [501, 502, 503, 504, 505]
    );

    // Guards: drop() needs a confirmation; with it the statement runs.
    let err = live
        .start(&format!("db.{coll}.drop()"), RunOptions::default())
        .await
        .expect_err("needs confirmation");
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert!(err.to_string().contains(CONFIRMATION_REQUIRED), "{err}");

    // Cancel: an endless loop ends within 2 s and the editor keeps working.
    let mut handle = live
        .start("while (true) {}", RunOptions::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let requested = Instant::now();
    assert!(live.service.cancel(handle.id));
    let events = drain(&mut handle).await;
    assert!(
        matches!(events.last(), Some(RunEvent::Cancelled { .. })),
        "{events:?}"
    );
    assert!(
        requested.elapsed() < CANCEL_LIMIT,
        "{:?}",
        requested.elapsed()
    );
    let (_, events) = live
        .run(
            &format!("db.{coll}.countDocuments({{g: 1}})"),
            RunOptions::default(),
        )
        .await;
    assert!(finished(&events), "{events:?}");

    // History keeps passwords out: a createUser that fails (unknown role) is recorded redacted.
    let secret = format!("S3cret-{}", token());
    let (_, events) = live
        .run(
            &format!(
                "db.runCommand({{createUser: 'm7shs_never', pwd: '{secret}', \
                 roles: [{{role: 'no_such_role_m7', db: 'oxbase_test'}}]}})"
            ),
            RunOptions::default(),
        )
        .await;
    assert!(
        matches!(events.last(), Some(RunEvent::Failed { .. })),
        "{events:?}"
    );
    let entries = live.history.list(&HistoryQuery::default()).await.unwrap();
    let json = serde_json::to_string(&entries).unwrap();
    assert!(!json.contains(&secret), "{json}");
    let created = entries
        .iter()
        .find(|e| e.sql.contains("createUser"))
        .expect("the createUser entry");
    assert!(created.sql.contains("pwd: '***'"), "{}", created.sql);
    assert_eq!(created.status, HistoryStatus::Error);
    assert!(entries.iter().any(|e| e.sql.starts_with("var seven")));

    let (_, events) = live
        .run(&format!("db.{coll}.drop()"), confirm_options())
        .await;
    assert!(finished(&events), "{events:?}");
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn redis_console_scripts_run_guard_and_record_redacted_history() {
    let live = Live::connect("redis", Engine::Redis).await;
    let prefix = format!("oxbase:shellsvc:t{}", token());
    let script = format!(
        "# console\n\
         HSET {prefix}:hash a 1 b 2\n\
         HGETALL {prefix}:hash\n\
         ZADD {prefix}:zset 1 x 2 y 3 z\n\
         ZRANGE {prefix}:zset 0 -1 WITHSCORES\n"
    );
    let (handle, events) = live.run(&script, RunOptions::default()).await;
    assert!(finished(&events), "{events:?}");
    assert_eq!(totals(&events), [2, 3]);
    let page = live.service.page(handle.id, 1, 1, 2).await.unwrap();
    let members = page.column(page.schema().index_of("member").unwrap());
    let members: Vec<&[u8]> = (0..members.len())
        .map(|i| members.as_binary::<i32>().value(i))
        .collect();
    assert_eq!(members, [b"y".as_slice(), b"z".as_slice()]);

    // FLUSHDB (and KEYS) are refused without a confirmation; nothing runs.
    for line in ["FLUSHDB", "KEYS *"] {
        let script = format!("GET {prefix}:hash\n{line}\n");
        let err = live
            .start(&script, RunOptions::default())
            .await
            .expect_err("needs confirmation");
        assert!(
            err.to_string().contains(CONFIRMATION_REQUIRED),
            "{line}: {err}"
        );
    }
    // Blocked lines are refused even when confirmed.
    let err = live
        .start("SUBSCRIBE m7:news", confirm_options())
        .await
        .expect_err("blocked");
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert!(!err.to_string().contains(CONFIRMATION_REQUIRED), "{err}");
    // A confirmed KEYS on the test's own prefix runs.
    let (_, events) = live
        .run(&format!("KEYS {prefix}:*"), confirm_options())
        .await;
    assert!(finished(&events), "{events:?}");
    assert_eq!(totals(&events), [2]);

    // History redaction: AUTH with a wrong password fails and is recorded without it.
    let secret = format!("wrong-{}", token());
    let (_, events) = live
        .run(&format!("AUTH nobody {secret}"), RunOptions::default())
        .await;
    assert!(
        matches!(events.last(), Some(RunEvent::Failed { .. })),
        "{events:?}"
    );
    let entries = live.history.list(&HistoryQuery::default()).await.unwrap();
    let json = serde_json::to_string(&entries).unwrap();
    assert!(!json.contains(&secret), "{json}");
    assert!(entries.iter().any(|e| e.sql == "AUTH nobody ***"), "{json}");

    let (_, events) = live
        .run(
            &format!("DEL {prefix}:hash {prefix}:zset"),
            RunOptions::default(),
        )
        .await;
    assert_eq!(rows_affected(&events), [Some(2)]);
}
