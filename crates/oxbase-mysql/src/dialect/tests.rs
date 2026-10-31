//! Registration, layout and capability tests, plus a recording fake connection that proves no
//! fetcher puts a name into SQL text.

use std::sync::Mutex;

use async_trait::async_trait;
use futures::StreamExt;
use oxbase_dialect::{Capabilities, CellStatus, ChildGroup, DatabaseScope, DialectRegistry};
use oxbase_driver::{CancellationToken, Connection, ResultStream, Tx};
use oxbase_types::{Engine, Error, ObjectKind, ObjectRef, Result, ServerVersion, Value, props};

use super::{MySqlCapabilities, register};

fn registry() -> DialectRegistry {
    let mut reg = DialectRegistry::new();
    register(&mut reg);
    reg
}

#[test]
fn every_layout_kind_has_a_fetcher() {
    let reg = registry();
    for engine in [Engine::MySql, Engine::MariaDb] {
        assert_eq!(
            reg.missing_layout_fetchers(engine),
            Vec::<ObjectKind>::new(),
            "{engine}"
        );
        assert!(reg.capabilities(engine).is_ok());
        assert!(reg.quoting(engine).is_ok());
        assert!(reg.fetcher(engine, ObjectKind::Event).is_ok());
    }
}

#[test]
fn every_designer_role_is_registered() {
    let reg = registry();
    let mysql = ServerVersion::parse("8.4.6").unwrap();
    let maria = ServerVersion::parse("11.8.9-MariaDB").unwrap();
    for (engine, v) in [(Engine::MySql, &mysql), (Engine::MariaDb, &maria)] {
        assert_eq!(
            reg.missing_designer_roles(engine),
            Vec::<String>::new(),
            "{engine}"
        );
        assert!(reg.dml_writer(engine).is_ok());
        let caps = reg.capabilities(engine).unwrap().designer(v);
        assert!(caps.create_table && !caps.transactional_ddl, "{engine}");
        assert_eq!(caps.max_identifier_length, Some(64));
    }
}

#[test]
fn layouts_follow_the_contract() {
    use ObjectKind as K;
    let reg = registry();
    let mysql = reg.tree_layout(Engine::MySql).unwrap();
    let mariadb = reg.tree_layout(Engine::MariaDb).unwrap();
    assert_eq!(mysql.scope, DatabaseScope::Catalog);
    assert_eq!(mysql.root(), &[ChildGroup::direct(K::Database)]);
    let kinds = |groups: &[ChildGroup]| groups.iter().map(|g| g.kind).collect::<Vec<_>>();
    assert_eq!(
        kinds(mysql.children(K::Database)),
        [K::Table, K::View, K::Function, K::Procedure]
    );
    assert_eq!(
        kinds(mariadb.children(K::Database)),
        [K::Table, K::View, K::Function, K::Procedure, K::Sequence]
    );
    for layout in [&mysql, &mariadb] {
        assert_eq!(
            kinds(layout.children(K::Table)),
            [K::Column, K::Index, K::ForeignKey, K::Check, K::Trigger]
        );
        assert_eq!(kinds(layout.children(K::View)), [K::Column]);
        assert!(layout.children(K::Database).iter().all(|g| g.folder));
    }
}

#[test]
fn applicability_and_versions() {
    use ObjectKind as K;
    let reg = registry();
    assert_eq!(
        reg.status(Engine::MySql, K::Sequence),
        CellStatus::NotApplicable
    );
    assert_eq!(
        reg.status(Engine::MySql, K::Schema),
        CellStatus::NotApplicable
    );
    assert_eq!(
        reg.status(Engine::MariaDb, K::MatView),
        CellStatus::NotApplicable
    );
    assert_ne!(
        reg.status(Engine::MariaDb, K::Sequence),
        CellStatus::NotApplicable
    );
    assert_ne!(
        reg.status(Engine::MySql, K::User),
        CellStatus::NotApplicable
    );

    let mysql84 = ServerVersion::parse("8.4.13").unwrap();
    let mysql80 = ServerVersion::parse("8.0.15").unwrap();
    let maria = ServerVersion::parse("11.8.9-MariaDB-ubu2404").unwrap();
    let maria102 = ServerVersion::parse("5.5.5-10.2.0-MariaDB").unwrap();
    assert!(reg.supports_kind(Engine::MySql, K::Check, &mysql84));
    assert!(!reg.supports_kind(Engine::MySql, K::Check, &mysql80));
    assert!(!reg.supports_kind(Engine::MySql, K::Sequence, &mysql84));
    assert!(reg.supports_kind(Engine::MariaDb, K::Sequence, &maria));
    assert!(!reg.supports_kind(Engine::MariaDb, K::Sequence, &maria102));
    assert!(!reg.supports_kind(Engine::MariaDb, K::Check, &maria102));
    assert!(!reg.supports_kind(Engine::MariaDb, K::Schema, &maria));

    let caps = MySqlCapabilities::new(Engine::MySql);
    assert!(caps.supports(K::Table, props::STORAGE_ENGINE, &mysql84));
    assert!(!caps.supports(K::Table, props::UNLOGGED, &mysql84));
    assert!(!caps.supports(K::Function, props::VOLATILITY, &mysql84));
    assert!(caps.supports(K::Function, props::DETERMINISTIC, &mysql84));
    assert!(caps.supports(K::View, props::VIEW_ALGORITHM, &mysql84));
    assert!(!caps.supports(K::Table, props::VIEW_ALGORITHM, &mysql84));
    assert!(!caps.supports(K::Sequence, props::COMMENT, &mysql84));
    assert!(!caps.supports(K::Database, props::COMMENT, &mysql84));
    assert!(caps.supports(K::Table, props::COMMENT, &mysql84));
    assert!(!caps.supports(K::Table, props::TEMPORARY, &mysql84));
    let maria_caps = MySqlCapabilities::new(Engine::MariaDb);
    assert!(maria_caps.supports(K::Database, props::COMMENT, &maria));
    assert!(!maria_caps.supports(K::Database, props::COMMENT, &maria102));
    assert!(!maria_caps.supports(K::Table, props::TEMPORARY, &maria));
}

// ---- a connection that records statements ----------------------------------------------------

struct Recorder {
    engine: Engine,
    version: ServerVersion,
    calls: Mutex<Vec<(String, Vec<Value>)>>,
}

impl Recorder {
    fn new(engine: Engine, version: &str) -> Self {
        Self {
            engine,
            version: ServerVersion::parse(version).unwrap(),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<(String, Vec<Value>)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Connection for Recorder {
    fn engine(&self) -> Engine {
        self.engine
    }

    fn server_version(&self) -> &ServerVersion {
        &self.version
    }

    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        self.calls
            .lock()
            .unwrap()
            .push((sql.to_owned(), params.to_vec()));
        Ok(futures::stream::empty().boxed())
    }

    async fn cancel_running(&self) -> Result<()> {
        Ok(())
    }

    async fn begin(&self) -> Result<Box<dyn Tx>> {
        Err(Error::unsupported("recorder"))
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// Names that would break or change a query if they were pasted into SQL.
const DB: &str = "zzdb'; DROP TABLE x; --";
const TABLE: &str = "zztab`\"\\";
const NAME: &str = "zzobj' OR '1'='1";

/// Every (parent for list, ref for load) pair of `kind`.
fn refs(kind: ObjectKind) -> (ObjectRef, ObjectRef) {
    let db = ObjectRef::database(DB);
    if kind == ObjectKind::Database {
        return (ObjectRef::root(), db);
    }
    if kind.is_table_child() {
        let table = db.child(ObjectKind::Table, TABLE);
        let obj = table.child(kind, NAME);
        return (table, obj);
    }
    let obj = db.child(kind, NAME);
    (db, obj)
}

#[tokio::test]
async fn fetchers_bind_every_name() {
    let reg = registry();
    for (engine, version) in [
        (Engine::MySql, "8.4.13"),
        (Engine::MariaDb, "11.8.9-MariaDB"),
    ] {
        for kind in ObjectKind::ALL {
            let Ok(fetcher) = reg.fetcher(engine, kind) else {
                continue;
            };
            let c = Recorder::new(engine, version);
            let (parent, obj) = refs(kind);
            let listed = fetcher
                .list(&c, &parent, CancellationToken::new())
                .await
                .unwrap_or_else(|e| panic!("{engine}/{kind} list: {e}"));
            assert_empty(&listed, "listed");
            let err = fetcher
                .load(&c, &obj, CancellationToken::new())
                .await
                .expect_err("nothing exists");
            assert!(
                matches!(err, Error::NotFound { .. }),
                "{engine}/{kind} load: {err}"
            );
            let calls = c.calls();
            let mut bound = false;
            for (sql, params) in &calls {
                assert!(
                    !sql.contains("zz"),
                    "{engine}/{kind}: a name is in the SQL: {sql}"
                );
                bound |= params
                    .iter()
                    .any(|p| matches!(p, Value::Text(t) if t.starts_with("zz")));
            }
            assert!(bound, "{engine}/{kind}: no name was bound");
        }
    }
}

#[tokio::test]
async fn cancelled_tokens_run_nothing() {
    let reg = registry();
    let cancel = CancellationToken::new();
    cancel.cancel();
    for (engine, version) in [
        (Engine::MySql, "8.4.13"),
        (Engine::MariaDb, "11.8.9-MariaDB"),
    ] {
        for kind in ObjectKind::ALL {
            let Ok(fetcher) = reg.fetcher(engine, kind) else {
                continue;
            };
            let c = Recorder::new(engine, version);
            let (parent, obj) = refs(kind);
            let err = fetcher.list(&c, &parent, cancel.clone()).await.unwrap_err();
            assert!(err.is_cancelled(), "{engine}/{kind} list: {err}");
            let err = fetcher.load(&c, &obj, cancel.clone()).await.unwrap_err();
            assert!(err.is_cancelled(), "{engine}/{kind} load: {err}");
            assert_empty(&c.calls(), &format!("{engine}/{kind} ran a query"));
        }
    }
}

#[tokio::test]
async fn old_servers_skip_what_they_lack() {
    let reg = registry();
    // MySQL 8.0.15 has no check constraints: listing them runs nothing.
    let c = Recorder::new(Engine::MySql, "8.0.15");
    let (parent, _) = refs(ObjectKind::Check);
    let fetcher = reg.fetcher(Engine::MySql, ObjectKind::Check).unwrap();
    assert_empty(
        &fetcher
            .list(&c, &parent, CancellationToken::new())
            .await
            .unwrap(),
        "fetcher .list(&c, &parent, CancellationToken::new()) .await .unwrap()",
    );
    assert_empty(&c.calls(), "c.calls()");
    // MariaDB before 11.5 lists sequences but cannot read their options without the name in SQL.
    let c = Recorder::new(Engine::MariaDb, "10.11.6-MariaDB");
    let (_, obj) = refs(ObjectKind::Sequence);
    let fetcher = reg.fetcher(Engine::MariaDb, ObjectKind::Sequence).unwrap();
    let err = fetcher
        .load(&c, &obj, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported { .. }), "{err}");
}

#[tokio::test]
async fn columns_only_under_tables_and_views() {
    let reg = registry();
    for (engine, version) in [
        (Engine::MySql, "8.4.13"),
        (Engine::MariaDb, "11.8.9-MariaDB"),
    ] {
        let c = Recorder::new(engine, version);
        let fetcher = reg.fetcher(engine, ObjectKind::Column).unwrap();
        let listed = fetcher
            .list(&c, &ObjectRef::database(DB), CancellationToken::new())
            .await
            .unwrap();
        assert_empty(&listed, "columns under a database");
        assert_empty(
            &c.calls(),
            &format!("{engine}: a column list under a database ran"),
        );
    }
}

#[tokio::test]
async fn catalog_queries_shape() {
    let reg = registry();
    let db = ObjectRef::database(DB);
    let table = db.child(ObjectKind::Table, TABLE);
    // A UNIQUE key may share an FK's name: only referencing key columns join.
    let c = Recorder::new(Engine::MySql, "8.4.13");
    let fk = table.child(ObjectKind::ForeignKey, NAME);
    let fetcher = reg.fetcher(Engine::MySql, ObjectKind::ForeignKey).unwrap();
    let err = fetcher
        .load(&c, &fk, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound { .. }), "{err}");
    assert!(
        c.calls()
            .iter()
            .any(|(sql, _)| sql.contains("REFERENCED_TABLE_NAME IS NOT NULL")),
        "{:?}",
        c.calls()
    );
    // Session temporary tables (MariaDB 11.2+) are not tables of the tree.
    let c = Recorder::new(Engine::MariaDb, "11.8.9-MariaDB");
    let fetcher = reg.fetcher(Engine::MariaDb, ObjectKind::Table).unwrap();
    let listed = fetcher
        .list(&c, &db, CancellationToken::new())
        .await
        .unwrap();
    assert_empty(&listed, "tables");
    let calls = c.calls();
    calls.first().expect("the table list ran a query");
    assert!(
        calls.iter().all(|(sql, _)| !sql.contains("TEMPORARY")),
        "{calls:?}"
    );
}

/// Fails with the first element when `v` is not empty.
#[track_caller]
fn assert_empty<T: std::fmt::Debug>(v: &[T], what: &str) {
    if let Some(first) = v.first() {
        panic!(
            "{what}: expected nothing, got {} item(s), first {first:?}",
            v.len()
        );
    }
}
