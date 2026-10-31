//! `TreeService` with the mock driver and fake fetchers: layout-driven children (folder and
//! direct groups), unsupported kinds hidden, system objects, metadata child sessions per
//! PostgreSQL database, cancel and errors.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oxbase_dialect::{
    Capabilities, ChildGroup, DatabaseScope, DialectRegistry, Fetcher, TreeLayout,
};
use oxbase_driver::mock::MockDriver;
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_services::tree::{TreeNode, TreeNodeRef, TreeOptions, TreeService};
use oxbase_services::{
    CancellationToken, Connection, ConnectionManager, DriverRegistry, ProvidedSecrets, SessionId,
};
use oxbase_store::{ConnectionProfile, MemorySecretStore, ProfileAddress};
use oxbase_types::{
    Engine, ErrorCategory, Extras, GenericDef, ObjectDef, ObjectKind, ObjectRef, ObjectSummary,
    PropId, Result, ServerVersion,
};
use pretty_assertions::assert_eq;

type Reads = Arc<Mutex<Vec<String>>>;

/// Lists two objects of its kind (one of them a system object) and records which database
/// each read ran on.
struct FakeFetcher {
    kind: ObjectKind,
    reads: Reads,
}

#[async_trait]
impl Fetcher for FakeFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        if cancel.is_cancelled() {
            return Err(oxbase_types::Error::Cancelled);
        }
        self.reads.lock().unwrap().push(format!(
            "list {} under {parent} on {}",
            self.kind,
            c.current_database().unwrap_or_default()
        ));
        let names: Vec<String> = if self.kind == ObjectKind::Database {
            vec!["db1".into(), "db2".into(), "sysdb".into()]
        } else {
            vec![format!("{}_a", self.kind), format!("{}_sys", self.kind)]
        };
        Ok(names
            .into_iter()
            .map(|name| {
                let mut row = ObjectSummary::new(parent.child(self.kind, name.clone()));
                row.system = name.contains("sys");
                row
            })
            .collect())
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        _cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        self.reads.lock().unwrap().push(format!(
            "load {obj} on {}",
            c.current_database().unwrap_or_default()
        ));
        Ok(ObjectDef::Generic(GenericDef {
            obj: obj.clone(),
            definition: Some("definition".into()),
            comment: None,
            extras: Extras::default(),
        }))
    }
}

/// Sequences need version 99.
struct Caps;

impl Capabilities for Caps {
    fn supports(&self, _kind: ObjectKind, _prop: PropId, _v: &ServerVersion) -> bool {
        true
    }

    fn supports_kind(&self, kind: ObjectKind, v: &ServerVersion) -> bool {
        kind != ObjectKind::Sequence || v.at_least(99, 0)
    }
}

fn registry(scope: DatabaseScope, reads: &Reads) -> DialectRegistry {
    use ObjectKind as K;
    let mut reg = DialectRegistry::new();
    for kind in [K::Database, K::Schema, K::Table, K::Column, K::Sequence] {
        reg.register_fetcher(
            Engine::Postgres,
            kind,
            Arc::new(FakeFetcher {
                kind,
                reads: reads.clone(),
            }),
        );
    }
    reg.register_capabilities(Engine::Postgres, Arc::new(Caps));
    reg.register_tree_layout(
        Engine::Postgres,
        TreeLayout::new(scope)
            .with_root(&[ChildGroup::direct(K::Database)])
            .with_children(K::Database, &[ChildGroup::direct(K::Schema)])
            .with_children(
                K::Schema,
                &[
                    ChildGroup::folder(K::Table),
                    ChildGroup::folder(K::Sequence),
                ],
            )
            .with_children(K::Table, &[ChildGroup::folder(K::Column)]),
    );
    reg
}

struct Harness {
    tree: TreeService,
    manager: Arc<ConnectionManager>,
    session: SessionId,
    reads: Reads,
}

async fn harness(scope: DatabaseScope) -> Harness {
    let mut drivers = DriverRegistry::new();
    drivers.register(Arc::new(MockDriver::new(Engine::Postgres)));
    let manager = Arc::new(ConnectionManager::new(
        drivers,
        Arc::new(MemorySecretStore::new()),
        Arc::new(RusshTunnelProvider::new()),
    ));
    let mut profile = ConnectionProfile::new(
        "mock",
        Engine::Postgres,
        ProfileAddress::Network {
            host: "mock.invalid".into(),
            port: Some(5432),
        },
    );
    profile.database = Some("db1".into());
    let base = manager
        .open(&profile, &ProvidedSecrets::none(), CancellationToken::new())
        .await
        .unwrap();
    let reads = Reads::default();
    let tree = TreeService::new(manager.clone(), Arc::new(registry(scope, &reads)));
    Harness {
        tree,
        manager,
        session: base.id(),
        reads,
    }
}

fn labels(nodes: &[TreeNode]) -> Vec<(String, bool)> {
    nodes
        .iter()
        .map(|n| (n.label.clone(), n.expandable))
        .collect()
}

fn reads(h: &Harness) -> Vec<String> {
    std::mem::take(&mut *h.reads.lock().unwrap())
}

fn token() -> CancellationToken {
    CancellationToken::new()
}

#[tokio::test]
async fn layouts_drive_the_tree() {
    let h = harness(DatabaseScope::Session).await;
    let opts = TreeOptions::default();

    // Root: databases, listed right away on the base session, system ones hidden.
    let dbs = h
        .tree
        .children(h.session, &TreeNodeRef::Connection, &opts, token())
        .await
        .unwrap();
    assert_eq!(
        labels(&dbs),
        [("db1".to_owned(), true), ("db2".to_owned(), true)]
    );
    assert_eq!(reads(&h), ["list database under database: on db1"]);
    let all = h
        .tree
        .children(
            h.session,
            &TreeNodeRef::Connection,
            &TreeOptions { show_system: true },
            token(),
        )
        .await
        .unwrap();
    assert_eq!(all.len(), 3);
    assert!(all[2].summary.as_ref().unwrap().system);
    reads(&h);

    // Schemas of the session's own database: base session.
    let schemas = h
        .tree
        .children(h.session, &dbs[0].node, &opts, token())
        .await
        .unwrap();
    assert_eq!(labels(&schemas), [("schema_a".to_owned(), true)]);
    assert_eq!(reads(&h), ["list schema under database:db1 on db1"]);

    // A schema: folders only (no query), the unsupported kind left out.
    let folders = h
        .tree
        .children(h.session, &schemas[0].node, &opts, token())
        .await
        .unwrap();
    assert_eq!(labels(&folders), [("Tables".to_owned(), true)]);
    assert_eq!(
        folders[0].node,
        TreeNodeRef::Folder {
            parent: ObjectRef::schema(Some("db1"), "schema_a"),
            kind: ObjectKind::Table
        }
    );
    assert_eq!(reads(&h), Vec::<String>::new());

    // The folder lists the tables; a table expands into its own folders.
    let tables = h
        .tree
        .children(h.session, &folders[0].node, &opts, token())
        .await
        .unwrap();
    assert_eq!(labels(&tables), [("table_a".to_owned(), true)]);
    assert_eq!(tables[0].key, "object:table:db1.schema_a.table_a");
    let table_folders = h
        .tree
        .children(h.session, &tables[0].node, &opts, token())
        .await
        .unwrap();
    assert_eq!(labels(&table_folders), [("Columns".to_owned(), true)]);
    let columns = h
        .tree
        .children(h.session, &table_folders[0].node, &opts, token())
        .await
        .unwrap();
    assert_eq!(labels(&columns), [("column_a".to_owned(), false)]);
    assert_eq!(
        columns[0].summary.as_ref().unwrap().obj.parent.as_deref(),
        Some("table_a")
    );
}

#[tokio::test]
async fn other_databases_are_read_on_a_cached_metadata_child() {
    let h = harness(DatabaseScope::Session).await;
    let opts = TreeOptions::default();
    let db2 = TreeNodeRef::Object {
        object: ObjectRef::database("db2"),
    };
    let schemas = h
        .tree
        .children(h.session, &db2, &opts, token())
        .await
        .unwrap();
    assert_eq!(labels(&schemas), [("schema_a".to_owned(), true)]);
    assert_eq!(reads(&h), ["list schema under database:db2 on db2"]);

    // The same child serves the next read and `load`; it is not an open connection to list.
    let child = h
        .tree
        .session_for(h.session, Some("db2"), token())
        .await
        .unwrap();
    assert_eq!(child.summary().parent, Some(h.session));
    assert_eq!(child.current_database().as_deref(), Some("db2"));
    let again = h
        .tree
        .session_for(h.session, Some("db2"), token())
        .await
        .unwrap();
    assert_eq!(again.id(), child.id());
    let base = h
        .tree
        .session_for(h.session, Some("db1"), token())
        .await
        .unwrap();
    assert_eq!(base.id(), h.session);
    assert_eq!(h.manager.list_open().len(), 1);

    let table = ObjectRef::table(Some("db2"), Some("schema_a"), "table_a");
    let def = h.tree.load(h.session, &table, token()).await.unwrap();
    assert!(matches!(def, ObjectDef::Generic(ref g) if g.obj == table));
    assert_eq!(reads(&h), ["load table:db2.schema_a.table_a on db2"]);

    // Forget closes the children; the next read opens a new one.
    h.tree.forget(h.session).await.unwrap();
    assert!(h.manager.get(child.id()).is_none());
    h.tree.forget(h.session).await.unwrap();
    let fresh = h
        .tree
        .session_for(h.session, Some("db2"), token())
        .await
        .unwrap();
    assert_ne!(fresh.id(), child.id());

    // Closing the base takes the child along; reads then fail with NotFound.
    h.manager.close(h.session).await.unwrap();
    assert!(h.manager.get(fresh.id()).is_none());
    let err = h
        .tree
        .children(h.session, &db2, &opts, token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);
}

#[tokio::test]
async fn catalog_engines_read_everything_on_the_base_session() {
    let h = harness(DatabaseScope::Catalog).await;
    let db2 = TreeNodeRef::Object {
        object: ObjectRef::database("db2"),
    };
    h.tree
        .children(h.session, &db2, &TreeOptions::default(), token())
        .await
        .unwrap();
    assert_eq!(reads(&h), ["list schema under database:db2 on db1"]);
    let s = h
        .tree
        .session_for(h.session, Some("db2"), token())
        .await
        .unwrap();
    assert_eq!(s.id(), h.session);
}

#[tokio::test]
async fn cancel_and_errors() {
    let h = harness(DatabaseScope::Session).await;
    let opts = TreeOptions::default();
    let cancelled = token();
    cancelled.cancel();
    let err = h
        .tree
        .children(
            h.session,
            &TreeNodeRef::Connection,
            &opts,
            cancelled.clone(),
        )
        .await
        .unwrap_err();
    assert!(err.is_cancelled());
    let err = h
        .tree
        .load(h.session, &ObjectRef::database("db1"), cancelled)
        .await
        .unwrap_err();
    assert!(err.is_cancelled());
    assert_eq!(reads(&h), Vec::<String>::new());

    let err = h
        .tree
        .children(SessionId::new(), &TreeNodeRef::Connection, &opts, token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);

    // A kind without a fetcher is Unsupported.
    let views = TreeNodeRef::Folder {
        parent: ObjectRef::schema(Some("db1"), "s"),
        kind: ObjectKind::View,
    };
    let err = h
        .tree
        .children(h.session, &views, &opts, token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Unsupported);

    // An engine without a layout is Unsupported.
    let bare = TreeService::new(h.manager.clone(), Arc::new(DialectRegistry::new()));
    let err = bare
        .children(h.session, &TreeNodeRef::Connection, &opts, token())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Unsupported);
}
