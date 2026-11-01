//! M6: the SQLite catalog reader (`docs/m6-contracts.md` §3.2) on a fresh database file with an
//! attached one: tables, views, columns in order with primary-key flags, foreign keys (also
//! implicit ones), case rules, partial scopes, the column limit and cancellation.
//!
//! No server is needed, so these run in every test job.

use std::path::Path;

use oxbase_dialect::{CatalogReader, CatalogScope, DialectRegistry, RelationName};
use oxbase_driver::{CancellationToken, ConnectParams, Connection, Driver, collect};
use oxbase_sqlite::SqliteDriver;
use oxbase_sqlite::dialect::{self, SqliteCatalogReader};
use oxbase_types::{CaseRules, CatalogSnapshot, Engine, IdentCase, RelationKind};

async fn run(c: &dyn Connection, sql: &str) {
    collect(
        c.execute(sql, &[], CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}")),
    )
    .await
    .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn open(path: &Path) -> Box<dyn Connection> {
    SqliteDriver
        .connect(&ConnectParams::sqlite(path))
        .await
        .unwrap()
}

const P: &str = "m6_t_sqlite_";

async fn seeded() -> (tempfile::TempDir, Box<dyn Connection>) {
    let dir = tempfile::tempdir().unwrap();
    let c = open(&dir.path().join("main.sqlite")).await;
    for sql in [
        format!(
            "CREATE TABLE {P}customer (region INTEGER NOT NULL, id INTEGER NOT NULL, \
             \"Display Name\" TEXT, PRIMARY KEY (region, id))"
        ),
        format!(
            "CREATE TABLE {P}order (oid INTEGER PRIMARY KEY, region INTEGER, cust INTEGER, \
             note VARCHAR(40) NOT NULL DEFAULT '', \
             FOREIGN KEY (region, cust) REFERENCES {P}customer (region, id))"
        ),
        format!("CREATE TABLE {P}line (oid INTEGER REFERENCES {P}order, qty REAL)"),
        format!(
            "CREATE VIEW {P}summary AS SELECT region, count(*) AS n FROM {P}customer GROUP BY region"
        ),
        format!("CREATE TABLE {P}auto (id INTEGER PRIMARY KEY AUTOINCREMENT, x TEXT)"),
    ] {
        run(c.as_ref(), &sql).await;
    }
    let aux = dir.path().join("aux.sqlite");
    let aux = aux.to_string_lossy().replace('\'', "''");
    run(c.as_ref(), &format!("ATTACH DATABASE '{aux}' AS aux")).await;
    run(
        c.as_ref(),
        "CREATE TABLE aux.things (id INTEGER PRIMARY KEY, label TEXT)",
    )
    .await;
    (dir, c)
}

async fn read(c: &dyn Connection, scope: &CatalogScope) -> CatalogSnapshot {
    SqliteCatalogReader
        .read(c, scope, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("read: {e}"))
}

#[tokio::test]
async fn reads_every_database_of_the_session() {
    let (_dir, c) = seeded().await;
    let snap = read(c.as_ref(), &CatalogScope::full()).await;
    assert_eq!(snap.engine, Engine::Sqlite);
    assert_eq!(snap.database, "main");
    assert_eq!(snap.default_schema.as_deref(), Some("main"));
    assert_eq!(snap.databases, ["aux"]);
    assert_eq!(snap.search_path, ["main", "aux"]);
    assert_eq!(snap.case_rules, CaseRules::uniform(IdentCase::Insensitive));
    assert!(snap.read_at_ms > 0);
    assert!(!snap.truncated);
    let names: Vec<&str> = snap.schemas.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["aux", "main"]);

    let main = &snap.schemas[1];
    let rels: Vec<&str> = main.relations.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        rels,
        [
            "m6_t_sqlite_auto",
            "m6_t_sqlite_customer",
            "m6_t_sqlite_line",
            "m6_t_sqlite_order",
            "m6_t_sqlite_summary"
        ],
        "sqlite_sequence and other internal tables are left out"
    );
    let customer = &main.relations[1];
    assert_eq!(customer.kind, RelationKind::Table);
    let cols: Vec<(&str, bool, bool)> = customer
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.primary_key, c.nullable))
        .collect();
    assert_eq!(
        cols,
        [
            ("region", true, false),
            ("id", true, false),
            ("Display Name", false, true)
        ]
    );
    let order = &main.relations[3];
    assert_eq!(order.columns[3].native_type, "VARCHAR(40)");
    assert_eq!(order.foreign_keys.len(), 1);
    let fk = &order.foreign_keys[0];
    assert_eq!(fk.columns, ["region", "cust"]);
    assert_eq!(fk.ref_table, "m6_t_sqlite_customer");
    assert_eq!(fk.ref_columns, ["region", "id"]);
    assert_eq!(fk.ref_schema, None);
    // `REFERENCES parent` without columns: the parent's primary key.
    let line = &main.relations[2];
    assert_eq!(line.foreign_keys[0].ref_columns, ["oid"]);
    let summary = &main.relations[4];
    assert_eq!(summary.kind, RelationKind::View);
    let view_cols: Vec<&str> = summary.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(view_cols, ["region", "n"]);
    assert!(main.routines.is_empty() && main.sequences.is_empty() && main.types.is_empty());

    let aux = &snap.schemas[0];
    assert_eq!(aux.relations.len(), 1);
    assert_eq!(aux.relations[0].columns.len(), 2);
}

#[tokio::test]
async fn partial_scopes_limits_and_cancel() {
    let (_dir, c) = seeded().await;
    let scope = CatalogScope {
        relations: Some(vec![
            RelationName {
                schema: None,
                // Another case of the name: SQLite names ignore case.
                name: "M6_T_SQLITE_ORDER".into(),
            },
            RelationName {
                schema: Some("aux".into()),
                name: "things".into(),
            },
            RelationName {
                schema: None,
                name: "missing".into(),
            },
        ]),
        ..CatalogScope::full()
    };
    let snap = read(c.as_ref(), &scope).await;
    let found: Vec<(&str, &str)> = snap
        .schemas
        .iter()
        .flat_map(|s| {
            s.relations
                .iter()
                .map(move |r| (s.name.as_str(), r.name.as_str()))
        })
        .collect();
    assert_eq!(found, [("aux", "things"), ("main", "m6_t_sqlite_order")]);
    assert_eq!(snap.schemas[1].relations[0].foreign_keys.len(), 1);

    let schemas = CatalogScope {
        schemas: Some(vec!["aux".into()]),
        ..CatalogScope::full()
    };
    let snap = read(c.as_ref(), &schemas).await;
    assert_eq!(snap.schemas.len(), 1);
    assert_eq!(snap.schemas[0].name, "aux");

    let limited = CatalogScope {
        max_columns: 3,
        ..CatalogScope::full()
    };
    let snap = read(c.as_ref(), &limited).await;
    assert!(snap.truncated);
    assert_eq!(snap.column_count(), 3);

    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = SqliteCatalogReader
        .read(c.as_ref(), &CatalogScope::full(), cancel)
        .await
        .unwrap_err();
    assert!(err.is_cancelled());
}

#[tokio::test]
async fn a_broken_view_does_not_hide_the_tables() {
    let (_dir, c) = seeded().await;
    run(c.as_ref(), "CREATE TABLE gone (a INTEGER)").await;
    run(c.as_ref(), "CREATE VIEW broken AS SELECT a FROM gone").await;
    run(c.as_ref(), "DROP TABLE gone").await;
    let snap = read(c.as_ref(), &CatalogScope::full()).await;
    let main = snap.schemas.iter().find(|s| s.name == "main").unwrap();
    let customer = main
        .relations
        .iter()
        .find(|r| r.name == "m6_t_sqlite_customer")
        .unwrap();
    assert_eq!(customer.columns.len(), 3);
    assert!(main.relations.iter().any(|r| r.name == "broken"));
}

#[test]
fn the_reader_is_registered() {
    let mut reg = DialectRegistry::new();
    dialect::register(&mut reg);
    assert!(reg.has_catalog_reader(Engine::Sqlite));
    assert!(
        !reg.missing_insight_roles(Engine::Sqlite)
            .iter()
            .any(|m| m.contains("CatalogReader")),
        "{:?}",
        reg.missing_insight_roles(Engine::Sqlite)
    );
}
