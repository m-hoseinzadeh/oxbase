//! M6: `SchemaCacheService` against the five SQL fixtures (`docs/m6-contracts.md` §3.5, §9):
//!
//! - a full build on a session of its own finds a table created in an editor;
//! - the snapshot persisted in the app database is reloaded by a new service (a restart)
//!   without any catalog read;
//! - editor DDL (`CREATE TABLE`, `DROP TABLE` run through `QueryService`), a `CREATE TABLE`
//!   inside an explicit transaction (committed by a later run), a designer apply and an import
//!   that creates its table reach the cache within `[m6] invalidation_limit_ms` of finishing;
//! - `forget_profile` drops the caches and the snapshots.
//!
//! `#[ignore]`d: `cargo test -p oxbase-services --test schema_cache_live -- --include-ignored`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt;
use oxbase_designer::{DesignColumn, TableDesign};
use oxbase_io::{ColumnSpec, CsvReadOptions, ReadOptions};
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_services::designer::{DesignApplyOptions, DesignerService};
use oxbase_services::history::HistoryService;
use oxbase_services::import::{
    ColumnMapping, ConversionOptions, ErrorPolicy, ImportConfig, ImportEvent, ImportRequest,
    ImportService, ImportSource, ImportTarget, TargetMode, TransactionPolicy,
};
use oxbase_services::query::{
    EditorId, QueryConfig, QueryService, RunEvent, RunOptions, RunTarget,
};
use oxbase_services::schema_cache::{
    COALESCE, CacheKey, CacheSource, CacheState, RefreshScope, SchemaCacheConfig,
    SchemaCacheService, SchemaEvents,
};
use oxbase_services::tree::TreeService;
use oxbase_services::{
    CancellationToken, ConnectionManager, ProvidedSecrets, SessionId, default_dialects,
    default_drivers,
};
use oxbase_store::{
    ConnectionProfile, MemoryHistoryRepository, MemorySchemaCacheRepository, MemorySecretStore,
    ProfileAddress, SchemaCacheRepository, Secret, SecretRef, SecretSlot, SecretStore, TlsMode,
    TlsSettings,
};
use oxbase_types::{ColumnDef, DataType, Engine, ObjectRef, PrimaryKeyDef};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn fixtures() -> toml::Value {
    let path = std::env::var_os("OXBASE_FIXTURES")
        .map(PathBuf::from)
        .map_or_else(|| root().join("tests/fixtures.toml"), |p| root().join(p));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    toml::Value::Table(text.parse().expect("fixtures.toml parses"))
}

fn s(t: &toml::Value, key: &str) -> String {
    t[key]
        .as_str()
        .unwrap_or_else(|| panic!("fixtures.toml: missing {key}"))
        .to_owned()
}

fn report(line: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr().lock(), "[m6 cache] {line}");
}

/// Lower-case letters and digits from the clock and the process id.
fn token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut n = nanos ^ (u128::from(std::process::id()) << 64);
    let mut out = String::new();
    while out.len() < 8 {
        out.push(char::from_digit(u32::try_from(n % 36).unwrap_or(0), 36).unwrap_or('0'));
        n /= 36;
    }
    out
}

struct Live {
    key: &'static str,
    engine: Engine,
    manager: Arc<ConnectionManager>,
    caches: SchemaCacheService,
    repo: Arc<MemorySchemaCacheRepository>,
    query: QueryService,
    designer: DesignerService,
    import: ImportService,
    session: SessionId,
    /// Where tables live: the catalog and schema of an `ObjectRef`.
    catalog: String,
    schema: Option<String>,
    prefix: String,
    limit: Duration,
    dir: tempfile::TempDir,
}

impl Live {
    async fn connect(key: &'static str, engine: Engine) -> Self {
        let doc = fixtures();
        let m6 = &doc["m6"];
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecretStore::new());
        let profile = if engine == Engine::Sqlite {
            ConnectionProfile::new(
                format!("m6 {key}"),
                engine,
                ProfileAddress::File {
                    path: dir.path().join("m6.sqlite"),
                },
            )
        } else {
            let e = &doc["engines"][key];
            let mut p = ConnectionProfile::new(
                format!("m6 {key}"),
                engine,
                ProfileAddress::Network {
                    host: s(e, "host"),
                    port: e["port"].as_integer().and_then(|p| u16::try_from(p).ok()),
                },
            );
            p.user = Some(s(e, "user"));
            p.database = Some(s(&m6[key], "database"));
            let secret = p.secret_key(SecretSlot::Password);
            secrets
                .set(&secret, &Secret::new(s(e, "password")))
                .await
                .unwrap();
            p.password = Some(SecretRef::Keychain { key: secret });
            p.tls = TlsSettings {
                mode: TlsMode::Disable,
                ..TlsSettings::default()
            };
            p.connect_timeout_secs = Some(15);
            p
        };
        let manager = Arc::new(ConnectionManager::new(
            default_drivers(),
            secrets,
            Arc::new(RusshTunnelProvider::new()),
        ));
        let base = manager
            .open(&profile, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{key}: connect failed: {e}"));
        let dialects = Arc::new(default_dialects());
        let events = SchemaEvents::new();
        let repo = Arc::new(MemorySchemaCacheRepository::new());
        let caches = SchemaCacheService::new(
            Arc::clone(&manager),
            Arc::clone(&dialects),
            repo.clone(),
            events.clone(),
            SchemaCacheConfig::default(),
        );
        let history = Arc::new(HistoryService::new(
            Arc::new(MemoryHistoryRepository::new()),
        ));
        let query = QueryService::new(
            Arc::clone(&manager),
            Arc::clone(&dialects),
            history,
            QueryConfig {
                spill_dir: dir.path().join("runs"),
                ..QueryConfig::default()
            },
        )
        .with_schema_events(events.clone());
        let tree = Arc::new(TreeService::new(
            Arc::clone(&manager),
            Arc::clone(&dialects),
        ));
        let import = ImportService::new(
            Arc::clone(&manager),
            Arc::clone(&tree),
            Arc::clone(&dialects),
            ImportConfig::default(),
        )
        .with_schema_events(events.clone());
        let designer =
            DesignerService::new(Arc::clone(&manager), tree, dialects).with_schema_events(events);
        let (catalog, schema) = match engine {
            Engine::Sqlite => ("main".to_owned(), None),
            Engine::MySql | Engine::MariaDb => (s(&m6[key], "database"), None),
            _ => (s(&m6[key], "database"), Some(s(&m6[key], "schema"))),
        };
        let limit = Duration::from_millis(
            m6["invalidation_limit_ms"]
                .as_integer()
                .and_then(|n| u64::try_from(n).ok())
                .expect("[m6] invalidation_limit_ms"),
        );
        Self {
            key,
            engine,
            manager,
            caches,
            repo,
            query,
            designer,
            import,
            session: base.id(),
            catalog,
            schema,
            prefix: format!("{}{}_", s(m6, "scratch_prefix"), token()),
            limit,
            dir,
        }
    }

    fn cache_key(&self) -> CacheKey {
        self.caches.key_for(self.session, None).unwrap()
    }

    /// Runs `sql` in an editor; returns when the run ended (panics on failure).
    async fn run(&self, sql: &str) {
        let mut handle = self
            .query
            .run(
                RunTarget {
                    session: self.session,
                    editor: EditorId::new("m6-cache"),
                },
                sql,
                RunOptions::default(),
            )
            .await
            .unwrap_or_else(|e| panic!("{}: {sql}: {e}", self.key));
        while let Some(event) = handle.events.recv().await {
            match event {
                RunEvent::Failed { error, .. } | RunEvent::StatementFailed { error, .. } => {
                    panic!("{}: {sql}: {}", self.key, error.message)
                }
                e if e.is_terminal() => return,
                _ => {}
            }
        }
    }

    /// Runs `sql` and ignores failures (clean-up).
    async fn try_run(&self, sql: &str) {
        if let Ok(mut handle) = self
            .query
            .run(
                RunTarget {
                    session: self.session,
                    editor: EditorId::new("m6-cleanup"),
                },
                sql,
                RunOptions::default(),
            )
            .await
        {
            while let Some(event) = handle.events.recv().await {
                if event.is_terminal() {
                    break;
                }
            }
        }
    }

    fn table(&self, name: &str) -> ObjectRef {
        ObjectRef::table(Some(&self.catalog), self.schema.as_deref(), name)
    }

    /// The relation names of the cache in memory (every schema).
    async fn names(&self) -> Vec<String> {
        self.caches
            .cache(&self.cache_key())
            .await
            .map(|c| {
                c.snapshot()
                    .schemas
                    .iter()
                    .flat_map(|s| s.relations.iter().map(|r| r.name.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Waits until `present` says whether `name` is in the cache; returns how long it took.
    async fn wait_for(&self, name: &str, present: bool, what: &str) -> Duration {
        let started = Instant::now();
        loop {
            let names = self.names().await;
            if names.iter().any(|n| n.eq_ignore_ascii_case(name)) == present {
                let took = started.elapsed();
                report(&format!(
                    "{}: {what} reached the cache in {} ms",
                    self.key,
                    took.as_millis()
                ));
                return took;
            }
            assert!(
                started.elapsed() < self.limit + Duration::from_secs(5),
                "{}: {what}: {name} present={present} not reached after {} ms",
                self.key,
                started.elapsed().as_millis()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

fn column(name: &str, data_type: DataType, nullable: bool) -> DesignColumn {
    let mut column = ColumnDef::new(name, data_type);
    column.nullable = nullable;
    DesignColumn {
        origin: None,
        column,
        default_literal: None,
    }
}

async fn scenario(key: &'static str, engine: Engine) {
    let live = Live::connect(key, engine).await;
    let names = ["a", "b", "c", "d", "e"].map(|n| format!("{}{n}", live.prefix));
    let outcome = std::panic::AssertUnwindSafe(steps(&live, &names))
        .catch_unwind()
        .await;
    for name in &names {
        live.try_run(&format!("DROP TABLE {name}")).await;
    }
    live.manager.close_all().await.unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    report(&format!(
        "{key}: build, reload, editor DDL and designer apply ok"
    ));
}

async fn steps(live: &Live, names: &[String; 5]) {
    let key = live.key;
    let [t_build, t_edit, t_design, t_tx, t_import] = names;
    live.run(&format!(
        "CREATE TABLE {t_build} (id INT NOT NULL PRIMARY KEY, name VARCHAR(20))"
    ))
    .await;

    // Build.
    let started = Instant::now();
    let status = live
        .caches
        .refresh(
            live.session,
            None,
            RefreshScope::Full,
            CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{key}: build: {e}"));
    report(&format!(
        "{key}: full build: {} relations, {} columns, {} routines in {} ms",
        status.relation_count,
        status.column_count,
        status.routine_count,
        started.elapsed().as_millis()
    ));
    assert_eq!(status.state, CacheState::Ready, "{key}");
    assert_eq!(status.source, Some(CacheSource::Live), "{key}");
    let cache = live.caches.cache(&live.cache_key()).await.expect("cache");
    let rel = cache
        .snapshot()
        .schemas
        .iter()
        .flat_map(|s| &s.relations)
        .find(|r| r.name.eq_ignore_ascii_case(t_build))
        .unwrap_or_else(|| panic!("{key}: {t_build} missing"));
    let cols: Vec<(String, bool)> = rel
        .columns
        .iter()
        .map(|c| (c.name.to_ascii_lowercase(), c.primary_key))
        .collect();
    assert_eq!(
        cols,
        [("id".to_owned(), true), ("name".to_owned(), false)],
        "{key}"
    );
    // The build's session is closed again.
    assert_eq!(live.manager.list_open().len(), 1, "{key}");

    // Restart: the snapshot comes back from the app database without the network.
    let stored = live
        .repo
        .load(live.cache_key().profile_id, &live.cache_key().database)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("{key}: nothing persisted"));
    report(&format!("{key}: snapshot {} bytes", stored.bytes.len()));
    let restarted = SchemaCacheService::new(
        Arc::clone(&live.manager),
        Arc::new(default_dialects()),
        live.repo.clone(),
        SchemaEvents::new(),
        SchemaCacheConfig::default(),
    );
    let started = Instant::now();
    let reloaded = restarted.cache(&live.cache_key()).await.expect("reloaded");
    report(&format!(
        "{key}: reloaded from the app database in {} ms",
        started.elapsed().as_millis()
    ));
    assert_eq!(
        reloaded.snapshot().relation_count(),
        cache.snapshot().relation_count()
    );
    let status = restarted.status(live.session, None).await.unwrap();
    assert_eq!(status.source, Some(CacheSource::Disk), "{key}");
    drop(restarted);

    // Editor DDL.
    live.run(&format!(
        "CREATE TABLE {t_edit} (id INT NOT NULL PRIMARY KEY, label VARCHAR(10))"
    ))
    .await;
    let took = live.wait_for(t_edit, true, "editor CREATE TABLE").await;
    assert!(took <= live.limit, "{key}: {} ms", took.as_millis());
    live.run(&format!("DROP TABLE {t_edit}")).await;
    let took = live.wait_for(t_edit, false, "editor DROP TABLE").await;
    assert!(took <= live.limit, "{key}: {} ms", took.as_millis());

    // Designer apply.
    let mut design = TableDesign::new_table(live.table(t_design));
    design.columns.push(column("id", DataType::INT32, false));
    design
        .columns
        .push(column("note", DataType::VarChar { length: Some(30) }, true));
    design.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".into()],
    });
    let applied = live
        .designer
        .apply(
            live.session,
            &design,
            DesignApplyOptions {
                confirm_destructive: true,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{key}: designer apply: {e}"));
    assert!(applied.outcome.committed, "{key}: {:?}", applied.outcome);
    let took = live.wait_for(t_design, true, "designer apply").await;
    assert!(took <= live.limit, "{key}: {} ms", took.as_millis());
    let cache = live.caches.cache(&live.cache_key()).await.unwrap();
    let designed = cache
        .snapshot()
        .schemas
        .iter()
        .flat_map(|s| &s.relations)
        .find(|r| r.name.eq_ignore_ascii_case(t_design))
        .unwrap();
    assert_eq!(designed.columns.len(), 2, "{key}");
    let status = live.caches.status(live.session, None).await.unwrap();
    assert_eq!(status.state, CacheState::Ready, "{key}");
    assert!(!status.building, "{key}");

    // Editor DDL inside an explicit transaction, committed by a later run: the refresh that
    // runs before the COMMIT cannot see the table (transactional DDL), the COMMIT brings it.
    let begin = match live.engine {
        Engine::MySql | Engine::MariaDb => "START TRANSACTION",
        _ => "BEGIN TRANSACTION",
    };
    live.run(begin).await;
    live.run(&format!(
        "CREATE TABLE {t_tx} (id INT NOT NULL PRIMARY KEY)"
    ))
    .await;
    tokio::time::sleep(COALESCE + Duration::from_millis(700)).await;
    live.run("COMMIT").await;
    let took = live
        .wait_for(t_tx, true, "editor CREATE TABLE committed by a later run")
        .await;
    assert!(took <= live.limit, "{key}: {} ms", took.as_millis());

    // An import that creates its table.
    let csv = live.dir.path().join("m6-import.csv");
    std::fs::write(&csv, "id,label\n1,one\n2,two\n").unwrap();
    let request = ImportRequest {
        options: ReadOptions::Csv(CsvReadOptions::default()),
        columns: vec![
            ColumnSpec::new("id", DataType::Text),
            ColumnSpec::new("label", DataType::Text),
        ],
        mapping: vec![
            ColumnMapping {
                source: 0,
                target: Some("id".into()),
                data_type: Some(DataType::INT64),
            },
            ColumnMapping {
                source: 1,
                target: Some("label".into()),
                data_type: Some(DataType::VarChar { length: Some(20) }),
            },
        ],
        target: ImportTarget {
            session: live.session,
            table: live.table(t_import),
            mode: TargetMode::Create,
        },
        primary_key: vec!["id".into()],
        conversion: ConversionOptions::default(),
        errors: ErrorPolicy::Stop,
        transaction: TransactionPolicy::Auto,
    };
    let source = ImportSource {
        path: csv,
        options: request.options.clone(),
    };
    let mut handle = live
        .import
        .start(source, request)
        .await
        .unwrap_or_else(|err| panic!("{key}: import did not start: {err}"));
    loop {
        let event = tokio::time::timeout(Duration::from_secs(120), handle.events.recv())
            .await
            .unwrap_or_else(|_| panic!("{key}: the import stalled"))
            .unwrap_or_else(|| panic!("{key}: the import ended without a terminal event"));
        match event {
            ImportEvent::Finished { report } => {
                assert!(report.table_created, "{key}: {report:?}");
                break;
            }
            other if other.is_terminal() => panic!("{key}: import: {other:?}"),
            _ => {}
        }
    }
    let took = live.wait_for(t_import, true, "import create").await;
    assert!(took <= live.limit, "{key}: {} ms", took.as_millis());

    // Forget the profile.
    let profile = live.cache_key().profile_id;
    live.caches.forget_profile(profile).await.unwrap();
    assert!(
        live.repo
            .load(profile, &live.cache_key().database)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn postgres_schema_cache_live() {
    scenario("postgres", Engine::Postgres).await;
}

#[tokio::test]
#[ignore = "needs the mysql fixture"]
async fn mysql_schema_cache_live() {
    scenario("mysql", Engine::MySql).await;
}

#[tokio::test]
#[ignore = "needs the mariadb fixture"]
async fn mariadb_schema_cache_live() {
    scenario("mariadb", Engine::MariaDb).await;
}

#[tokio::test]
#[ignore = "needs the mssql fixture"]
async fn mssql_schema_cache_live() {
    scenario("mssql", Engine::MsSql).await;
}

#[tokio::test]
async fn sqlite_schema_cache_live() {
    scenario("sqlite", Engine::Sqlite).await;
}
