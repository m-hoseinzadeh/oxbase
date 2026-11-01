//! `SchemaCacheService` with a scripted catalog reader: builds, persistence, startup from the
//! app database without the network, background `ensure`, joining, invalidation (direct and
//! through the bus), failures, LRU and `forget_profile`.

use std::sync::atomic::AtomicUsize;

use async_trait::async_trait;
use oxbase_dialect::CatalogReader;
use oxbase_driver::{ConnectParams, Connection, Driver, DriverRegistry, ResultStream, Tx};
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_store::{
    ConnectionProfile, MemorySchemaCacheRepository, MemorySecretStore, ProfileAddress,
};
use oxbase_types::{
    CatalogColumn, CatalogRelation, CatalogSchema, CatalogSnapshot, Engine, RelationKind,
    ServerVersion, Value,
};

use super::*;
use crate::ProvidedSecrets;

// --- a driver whose connections do nothing ------------------------------------------------

struct NullDriver {
    connects: Arc<AtomicUsize>,
}

#[async_trait]
impl Driver for NullDriver {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(NullConnection {
            database: params.database.clone().unwrap_or_else(|| "app".into()),
            version: ServerVersion::new(18, 0, 0),
        }))
    }
}

struct NullConnection {
    database: String,
    version: ServerVersion,
}

#[async_trait]
impl Connection for NullConnection {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn server_version(&self) -> &ServerVersion {
        &self.version
    }

    fn current_database(&self) -> Option<String> {
        Some(self.database.clone())
    }

    async fn execute(
        &self,
        _sql: &str,
        _params: &[Value],
        _cancel: CancellationToken,
    ) -> Result<ResultStream> {
        Ok(Box::pin(futures::stream::empty()))
    }

    async fn cancel_running(&self) -> Result<()> {
        Ok(())
    }

    async fn begin(&self) -> Result<Box<dyn Tx>> {
        Err(Error::unsupported("no transactions here"))
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

// --- a catalog reader over a scripted catalog ---------------------------------------------

#[derive(Default)]
struct Server {
    /// The catalog per database.
    catalogs: HashMap<String, CatalogSnapshot>,
    reads: Vec<(String, CatalogScope)>,
    fail: bool,
    delay: Duration,
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Server>>);

impl Shared {
    fn with<T>(&self, f: impl FnOnce(&mut Server) -> T) -> T {
        f(&mut lock(&*self.0))
    }

    fn reads(&self) -> Vec<(String, CatalogScope)> {
        self.with(|s| s.reads.clone())
    }
}

struct ScriptedReader(Shared);

fn column(name: &str) -> CatalogColumn {
    CatalogColumn {
        name: name.into(),
        native_type: "integer".into(),
        nullable: false,
        primary_key: name == "id",
    }
}

fn table(name: &str, columns: &[&str]) -> CatalogRelation {
    CatalogRelation {
        name: name.into(),
        kind: RelationKind::Table,
        columns: columns.iter().map(|c| column(c)).collect(),
        foreign_keys: vec![],
        comment: None,
    }
}

fn catalog(database: &str, tables: &[(&str, &[&str])]) -> CatalogSnapshot {
    let mut snap = CatalogSnapshot::new(Engine::Postgres, ServerVersion::new(18, 0, 0), database);
    snap.default_schema = Some("public".into());
    snap.search_path = vec!["public".into()];
    snap.databases = vec!["app".into(), "other".into()];
    let mut public = CatalogSchema::new("public");
    public.relations = tables.iter().map(|(n, c)| table(n, c)).collect();
    snap.schemas = vec![public];
    snap
}

/// `snap` limited to `scope` (relations match ignoring case, like the real readers).
fn scoped(snap: &CatalogSnapshot, scope: &CatalogScope) -> CatalogSnapshot {
    if scope.is_full() {
        return snap.clone();
    }
    let mut out = snap.clone();
    out.schemas.clear();
    for schema in &snap.schemas {
        if scope.schemas.iter().flatten().any(|s| s == &schema.name) {
            out.schemas.push(schema.clone());
            continue;
        }
        let wanted: Vec<CatalogRelation> = schema
            .relations
            .iter()
            .filter(|r| {
                scope.relations.iter().flatten().any(|t| {
                    t.schema.as_deref().unwrap_or("public") == schema.name
                        && t.name.eq_ignore_ascii_case(&r.name)
                })
            })
            .cloned()
            .collect();
        if !wanted.is_empty() {
            let mut s = CatalogSchema::new(schema.name.as_str());
            s.relations = wanted;
            out.schemas.push(s);
        }
    }
    out
}

#[async_trait]
impl CatalogReader for ScriptedReader {
    async fn read(
        &self,
        c: &dyn Connection,
        scope: &CatalogScope,
        cancel: CancellationToken,
    ) -> Result<CatalogSnapshot> {
        let database = c.current_database().unwrap_or_default();
        let delay = self.0.with(|s| s.delay);
        if !delay.is_zero() {
            tokio::select! {
                () = cancel.cancelled() => return Err(Error::Cancelled),
                () = tokio::time::sleep(delay) => {}
            }
        }
        self.0.with(|s| {
            s.reads.push((database.clone(), scope.clone()));
            if s.fail {
                return Err(Error::connection("the server went away"));
            }
            let snap = s
                .catalogs
                .get(&database)
                .ok_or_else(|| Error::not_found("no such database"))?;
            let mut out = scoped(snap, scope);
            out.read_at_ms = now_ms();
            Ok(out)
        })
    }
}

// --- harness ----------------------------------------------------------------------------------

struct Harness {
    service: SchemaCacheService,
    server: Shared,
    repo: Arc<MemorySchemaCacheRepository>,
    manager: Arc<ConnectionManager>,
    dialects: Arc<DialectRegistry>,
    session: SessionId,
    connects: Arc<AtomicUsize>,
}

fn config() -> SchemaCacheConfig {
    SchemaCacheConfig::default()
}

async fn harness(config: SchemaCacheConfig) -> Harness {
    let server = Shared::default();
    server.with(|s| {
        s.catalogs.insert(
            "app".into(),
            catalog(
                "app",
                &[("orders", &["id", "total"]), ("customers", &["id", "name"])],
            ),
        );
        s.catalogs
            .insert("other".into(), catalog("other", &[("things", &["id"])]));
    });
    let connects = Arc::new(AtomicUsize::new(0));
    let mut drivers = DriverRegistry::new();
    drivers.register(Arc::new(NullDriver {
        connects: Arc::clone(&connects),
    }));
    let manager = Arc::new(ConnectionManager::new(
        drivers,
        Arc::new(MemorySecretStore::new()),
        Arc::new(RusshTunnelProvider::new()),
    ));
    let mut profile = ConnectionProfile::new(
        "fake",
        Engine::Postgres,
        ProfileAddress::Network {
            host: "fake.invalid".into(),
            port: Some(5432),
        },
    );
    profile.database = Some("app".into());
    let base = manager
        .open(&profile, &ProvidedSecrets::none(), CancellationToken::new())
        .await
        .unwrap();
    let mut dialects = DialectRegistry::new();
    dialects.register_catalog_reader(Engine::Postgres, Arc::new(ScriptedReader(server.clone())));
    let dialects = Arc::new(dialects);
    let repo = Arc::new(MemorySchemaCacheRepository::new());
    let service = SchemaCacheService::new(
        Arc::clone(&manager),
        Arc::clone(&dialects),
        repo.clone(),
        SchemaEvents::new(),
        config,
    );
    Harness {
        service,
        server,
        repo,
        manager,
        dialects,
        session: base.id(),
        connects,
    }
}

impl Harness {
    async fn refresh(&self, scope: RefreshScope) -> Result<CacheStatus> {
        self.service
            .refresh(self.session, None, scope, CancellationToken::new())
            .await
    }

    fn key(&self) -> CacheKey {
        self.service.key_for(self.session, None).unwrap()
    }

    async fn names(&self) -> Vec<String> {
        let cache = self.service.cache(&self.key()).await.expect("a cache");
        cache.snapshot().schemas[0]
            .relations
            .iter()
            .map(|r| r.name.clone())
            .collect()
    }

    /// Waits until `check` holds on the relation names (at most 3 s).
    async fn wait_for(&self, check: impl Fn(&[String]) -> bool) -> Vec<String> {
        let started = Instant::now();
        loop {
            let names = match self.service.cache(&self.key()).await {
                Some(_) => self.names().await,
                None => Vec::new(),
            };
            if check(&names) {
                return names;
            }
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "timed out; names: {names:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

// --- tests --------------------------------------------------------------------------------

#[tokio::test]
async fn refresh_builds_persists_and_closes_its_session() {
    let h = harness(config()).await;
    let missing = h.service.status(h.session, None).await.unwrap();
    assert_eq!(missing.state, CacheState::Missing);
    assert!(missing.source.is_none());

    let status = h.refresh(RefreshScope::Full).await.unwrap();
    assert_eq!(status.state, CacheState::Ready);
    assert_eq!(status.source, Some(CacheSource::Live));
    assert_eq!((status.relation_count, status.column_count), (2, 4));
    assert!(status.last_build_ms.is_some());
    assert!(!status.building);
    assert_eq!(status.key.database, "app");
    assert_eq!(h.names().await, ["customers", "orders"]);
    // A child session was opened for the read and closed again.
    assert_eq!(h.connects.load(Ordering::SeqCst), 2);
    assert_eq!(h.manager.list_open().len(), 1);
    let stored = h
        .repo
        .load(status.key.profile_id, "app")
        .await
        .unwrap()
        .expect("persisted");
    assert_eq!(stored.format, CatalogSnapshot::FORMAT);
    assert_eq!(codec::decode(&stored.bytes).unwrap().relation_count(), 2);
}

#[tokio::test]
async fn startup_reads_the_snapshot_without_the_network() {
    let h = harness(config()).await;
    h.refresh(RefreshScope::Full).await.unwrap();
    let reads = h.server.reads().len();

    // A second service (a restart) on the same app database.
    let second = SchemaCacheService::new(
        Arc::clone(&h.manager),
        Arc::clone(&h.dialects),
        h.repo.clone(),
        SchemaEvents::new(),
        config(),
    );
    let cache = second.cache(&h.key()).await.expect("from disk");
    assert_eq!(cache.snapshot().relation_count(), 2);
    let status = second.status(h.session, None).await.unwrap();
    assert_eq!(status.source, Some(CacheSource::Disk));
    assert_eq!(status.state, CacheState::Ready);
    assert_eq!(h.server.reads().len(), reads, "no catalog read");

    // An unreadable snapshot is dropped, and the key is then simply missing.
    let key = CacheKey {
        profile_id: h.key().profile_id,
        database: "other".into(),
    };
    h.repo
        .save(&StoredSnapshot {
            profile_id: key.profile_id,
            database: key.database.clone(),
            format: 99,
            saved_at: Utc::now(),
            bytes: b"{not a snapshot".to_vec(),
        })
        .await
        .unwrap();
    assert!(second.cache(&key).await.is_none());
    assert!(
        h.repo
            .load(key.profile_id, "other")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn ensure_builds_once_in_the_background() {
    let h = harness(config()).await;
    h.server.with(|s| s.delay = Duration::from_millis(150));
    let first = h.service.ensure(h.session, None).await.unwrap();
    assert!(first.building);
    assert_eq!(first.state, CacheState::Missing);
    let again = h.service.ensure(h.session, None).await.unwrap();
    assert!(again.building);
    // A full refresh asked for now joins the running build.
    let joined = h.refresh(RefreshScope::Full).await.unwrap();
    assert_eq!(joined.state, CacheState::Ready);
    assert_eq!(h.server.reads().len(), 1, "{:?}", h.server.reads());
    let ready = h.service.ensure(h.session, None).await.unwrap();
    assert!(!ready.building);
    assert_eq!(h.server.reads().len(), 1);
}

#[tokio::test]
async fn stale_caches_are_served_and_rebuilt() {
    let h = harness(SchemaCacheConfig {
        stale_after: Duration::ZERO,
        ..config()
    })
    .await;
    h.refresh(RefreshScope::Full).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let status = h.service.ensure(h.session, None).await.unwrap();
    assert_eq!(status.state, CacheState::Stale);
    assert!(status.building);
    assert!(h.service.cache(&h.key()).await.is_some(), "still served");
    let started = Instant::now();
    while h.server.reads().len() < 2 {
        assert!(started.elapsed() < Duration::from_secs(3));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn partial_refreshes_merge_relations() {
    let h = harness(config()).await;
    h.refresh(RefreshScope::Full).await.unwrap();
    h.server.with(|s| {
        let app = s.catalogs.get_mut("app").unwrap();
        app.schemas[0].relations.retain(|r| r.name != "orders");
        app.schemas[0]
            .relations
            .push(table("Invoices", &["id", "due"]));
    });
    let status = h
        .refresh(RefreshScope::Relations {
            targets: vec![
                RefreshTarget {
                    schema: Some("public".into()),
                    name: "orders".into(),
                },
                RefreshTarget {
                    schema: None,
                    name: "invoices".into(),
                },
            ],
        })
        .await
        .unwrap();
    assert_eq!(status.relation_count, 2);
    assert_eq!(h.names().await, ["Invoices", "customers"]);
    let (_, scope) = h.server.reads().pop().unwrap();
    assert!(scope.relations.is_some() && scope.schemas.is_none());
}

#[tokio::test]
async fn invalidation_refreshes_only_the_changed_relations() {
    let h = harness(config()).await;
    h.refresh(RefreshScope::Full).await.unwrap();
    h.server.with(|s| {
        let app = s.catalogs.get_mut("app").unwrap();
        app.schemas[0].relations.retain(|r| r.name != "orders");
        app.schemas[0].relations.push(table("fresh", &["id"]));
    });
    let started = Instant::now();
    h.service.invalidate(&SchemaChangeEvent {
        session: h.session,
        database: None,
        source: ChangeSource::Query,
        changes: ["DROP TABLE Orders", "CREATE TABLE public.FRESH (id int)"]
            .into_iter()
            .flat_map(|sql| oxbase_sql::schema_changes(oxbase_sql::SqlDialect::Postgres, sql))
            .collect(),
    });
    let status = h.service.status(h.session, None).await.unwrap();
    assert_eq!(status.state, CacheState::Stale);
    assert!(status.building);
    let names = h
        .wait_for(|n| n.iter().any(|x| x == "fresh") && !n.iter().any(|x| x == "orders"))
        .await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(names, ["customers", "fresh"]);
    let reads = h.server.reads();
    assert_eq!(reads.len(), 2, "one coalesced partial read");
    assert!(reads[1].1.relations.is_some());
    let status = h.service.status(h.session, None).await.unwrap();
    assert_eq!(status.state, CacheState::Ready);
}

#[tokio::test]
async fn events_on_the_bus_reach_the_cache() {
    let h = harness(config()).await;
    h.refresh(RefreshScope::Full).await.unwrap();
    h.server.with(|s| {
        s.catalogs.get_mut("app").unwrap().schemas[0]
            .relations
            .push(table("from_bus", &["id"]));
    });
    // A function change is not about a relation: everything is read again.
    h.service.events().publish(SchemaChangeEvent {
        session: h.session,
        database: Some("app".into()),
        source: ChangeSource::Query,
        changes: oxbase_sql::schema_changes(
            oxbase_sql::SqlDialect::Postgres,
            "CREATE FUNCTION f() RETURNS int AS 'select 1' LANGUAGE sql",
        ),
    });
    h.wait_for(|n| n.iter().any(|x| x == "from_bus")).await;
    let reads = h.server.reads();
    assert!(reads.last().unwrap().1.is_full());
}

#[tokio::test]
async fn designer_events_name_the_table_exactly() {
    let h = harness(config()).await;
    let table = ObjectRef::database("app")
        .child(ObjectKind::Schema, "public")
        .child(ObjectKind::Table, "Orders");
    let renamed = ObjectRef::database("app")
        .child(ObjectKind::Schema, "public")
        .child(ObjectKind::Table, "orders_2");
    let event = object_event(
        &h.dialects,
        Engine::Postgres,
        h.session,
        ChangeSource::Designer,
        &[(DdlVerb::Alter, &table), (DdlVerb::Rename, &renamed)],
    );
    assert_eq!(event.database.as_deref(), Some("app"));
    assert_eq!(event.changes.len(), 2);
    assert_eq!(event.changes[0].object, DdlObject::Table);
    assert_eq!(
        event.changes[0].name,
        [
            NamePart {
                text: "public".into(),
                quoted: true
            },
            NamePart {
                text: "Orders".into(),
                quoted: true
            }
        ]
    );
    assert_eq!(event.changes[1].verb, DdlVerb::Rename);
}

#[tokio::test]
async fn failed_builds_keep_the_previous_names() {
    let h = harness(config()).await;
    // No names yet: a failure is `failed`.
    h.server.with(|s| s.fail = true);
    let err = h.refresh(RefreshScope::Full).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Connection);
    let status = h.service.status(h.session, None).await.unwrap();
    assert_eq!(status.state, CacheState::Failed);
    assert_eq!(
        status.last_error.map(|e| e.category()),
        Some(ErrorCategory::Connection)
    );
    // `ensure` does not hammer a failing server.
    let resting = h.service.ensure(h.session, None).await.unwrap();
    assert!(!resting.building);

    h.server.with(|s| s.fail = false);
    h.refresh(RefreshScope::Full).await.unwrap();
    h.server.with(|s| s.fail = true);
    assert!(h.refresh(RefreshScope::Full).await.is_err());
    let status = h.service.status(h.session, None).await.unwrap();
    assert_eq!(status.state, CacheState::Ready);
    assert!(status.last_error.is_some());
    assert_eq!(h.names().await, ["customers", "orders"]);
}

#[tokio::test]
async fn cancelled_refreshes_leave_the_cache_alone() {
    let h = harness(config()).await;
    h.refresh(RefreshScope::Full).await.unwrap();
    h.server.with(|s| s.delay = Duration::from_secs(5));
    let cancel = CancellationToken::new();
    let refresh = h
        .service
        .refresh(h.session, None, RefreshScope::Full, cancel.clone());
    let stop = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(refresh, stop);
    assert!(result.unwrap_err().is_cancelled());
    let status = h.service.status(h.session, None).await.unwrap();
    assert_eq!(status.state, CacheState::Ready);
    assert!(status.last_error.is_none());
    assert!(!status.building);
}

#[tokio::test]
async fn least_recently_used_caches_leave_memory_but_not_disk() {
    let h = harness(SchemaCacheConfig {
        max_caches: 1,
        ..config()
    })
    .await;
    h.refresh(RefreshScope::Full).await.unwrap();
    h.service
        .refresh(
            h.session,
            Some("other".into()),
            RefreshScope::Full,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let in_memory = lock(&h.service.core.entries)
        .values()
        .filter(|e| e.cache.is_some())
        .count();
    assert_eq!(in_memory, 1);
    // The evicted one comes back from the app database.
    let reads = h.server.reads().len();
    assert_eq!(h.names().await, ["customers", "orders"]);
    assert_eq!(h.server.reads().len(), reads);
    let status = h.service.status(h.session, None).await.unwrap();
    assert_eq!(status.source, Some(CacheSource::Disk));
}

#[tokio::test]
async fn forgetting_a_profile_drops_its_caches_and_snapshots() {
    let h = harness(config()).await;
    h.refresh(RefreshScope::Full).await.unwrap();
    let key = h.key();
    h.service.forget_profile(key.profile_id).await.unwrap();
    assert!(h.repo.load(key.profile_id, "app").await.unwrap().is_none());
    assert!(h.service.cache(&key).await.is_none());
    let unknown = h.service.status(SessionId::new(), None).await.unwrap_err();
    assert_eq!(unknown.category(), ErrorCategory::NotFound);
}

#[tokio::test]
async fn engines_without_a_reader_fail_once() {
    let mut h = harness(config()).await;
    h.service = SchemaCacheService::new(
        Arc::clone(&h.manager),
        Arc::new(DialectRegistry::new()),
        h.repo.clone(),
        SchemaEvents::new(),
        config(),
    );
    let err = h.refresh(RefreshScope::Full).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Unsupported);
    let status = h.service.ensure(h.session, None).await.unwrap();
    assert_eq!(status.state, CacheState::Failed);
    assert!(!status.building);
}

#[test]
fn transaction_ends_republish_the_changes_they_held() {
    let events = SchemaEvents::new();
    let mut rx = events.subscribe();
    let pg = oxbase_sql::SqlDialect::Postgres;
    let session = SessionId::new();
    let other = SessionId::new();
    let db = Some("app".to_owned());
    // Plain statements publish nothing.
    events.statement_succeeded(session, db.clone(), pg, "BEGIN");
    events.statement_succeeded(session, db.clone(), pg, "INSERT INTO t VALUES (1)");
    assert!(rx.try_recv().is_err());
    events.statement_succeeded(session, db.clone(), pg, "CREATE TABLE t (id int)");
    let created = rx.try_recv().unwrap();
    assert_eq!(created.source, ChangeSource::Query);
    assert_eq!(created.changes.len(), 1);
    // Another session's COMMIT does not touch this session's changes.
    events.statement_succeeded(other, db.clone(), pg, "COMMIT");
    assert!(rx.try_recv().is_err());
    // A savepoint rollback republishes and keeps them; the COMMIT republishes and forgets.
    events.statement_succeeded(session, db.clone(), pg, "ROLLBACK TO SAVEPOINT s");
    assert_eq!(rx.try_recv().unwrap().changes, created.changes);
    events.statement_succeeded(session, db.clone(), pg, "commit");
    let again = rx.try_recv().unwrap();
    assert_eq!(
        (again.session, again.database.clone(), again.changes.clone()),
        (session, db.clone(), created.changes.clone())
    );
    events.statement_succeeded(session, db.clone(), pg, "COMMIT");
    assert!(rx.try_recv().is_err());
    // Too many changes in one transaction: its end refreshes everything.
    for i in 0..=SchemaEvents::MAX_OPEN_CHANGES {
        events.statement_succeeded(
            session,
            db.clone(),
            pg,
            &format!("CREATE TABLE t{i} (id int)"),
        );
        rx.try_recv().unwrap();
    }
    events.statement_succeeded(session, db, pg, "END");
    assert_eq!(rx.try_recv().unwrap().changes.len(), 0);
}

#[tokio::test]
async fn changes_during_the_first_build_are_read_after_it() {
    let h = harness(config()).await;
    h.server.with(|s| s.delay = Duration::from_millis(200));
    let first = h.service.ensure(h.session, None).await.unwrap();
    assert!(first.building);
    tokio::time::sleep(Duration::from_millis(30)).await;
    h.service.invalidate(&SchemaChangeEvent {
        session: h.session,
        database: None,
        source: ChangeSource::Query,
        changes: oxbase_sql::schema_changes(
            oxbase_sql::SqlDialect::Postgres,
            "CREATE TABLE public.late (id int)",
        ),
    });
    let started = Instant::now();
    loop {
        let status = h.service.status(h.session, None).await.unwrap();
        if status.state == CacheState::Ready && !status.building {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "still {status:?}; reads {:?}",
            h.server.reads()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let reads = h.server.reads();
    assert_eq!(reads.len(), 2, "{reads:?}");
    assert!(
        reads[1].1.is_full(),
        "no names to merge into yet: read everything"
    );
}

#[tokio::test]
async fn a_profile_forgotten_during_a_build_keeps_nothing() {
    let h = harness(config()).await;
    h.server.with(|s| s.delay = Duration::from_millis(200));
    let key = h.key();
    let forget = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        h.service.forget_profile(key.profile_id).await.unwrap();
    };
    let (built, ()) = tokio::join!(h.refresh(RefreshScope::Full), forget);
    assert_eq!(built.unwrap_err().category(), ErrorCategory::NotFound);
    assert!(h.repo.load(key.profile_id, "app").await.unwrap().is_none());
    assert!(h.service.cache(&key).await.is_none());
    let status = h.service.status(h.session, None).await.unwrap();
    assert_eq!(status.state, CacheState::Missing);
    assert!(!status.building);
}

#[tokio::test]
async fn creates_the_partial_read_cannot_find_refresh_everything() {
    let h = harness(config()).await;
    h.refresh(RefreshScope::Full).await.unwrap();
    h.service
        .refresh(
            h.session,
            Some("other".into()),
            RefreshScope::Full,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    // `ghost` was created where this cache does not look (`USE other`, another search path).
    h.service.invalidate(&SchemaChangeEvent {
        session: h.session,
        database: None,
        source: ChangeSource::Query,
        changes: oxbase_sql::schema_changes(
            oxbase_sql::SqlDialect::Postgres,
            "CREATE TABLE ghost (id int)",
        ),
    });
    let started = Instant::now();
    while h.server.reads().len() < 4 {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            h.server.reads()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let reads = h.server.reads();
    assert_eq!(reads[2].0, "app");
    assert!(reads[2].1.relations.is_some(), "the partial read first");
    assert_eq!(reads[3].0, "app");
    assert!(reads[3].1.is_full(), "then everything");
    let other = h
        .service
        .status(h.session, Some("other".into()))
        .await
        .unwrap();
    assert_eq!(
        other.state,
        CacheState::Stale,
        "the other database may have it"
    );
}

#[tokio::test]
async fn idle_entries_are_pruned() {
    let h = harness(SchemaCacheConfig {
        max_caches: 1,
        ..config()
    })
    .await;
    let profile = h.key().profile_id;
    for i in 0..20 {
        let key = CacheKey {
            profile_id: profile,
            database: format!("db{i}"),
        };
        assert!(h.service.cache(&key).await.is_none());
    }
    let entries = lock(&h.service.core.entries).len();
    assert!(entries <= ENTRIES_PER_CACHE + 1, "{entries} entries");
}
