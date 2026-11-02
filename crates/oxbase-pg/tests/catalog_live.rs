//! M6: the PostgreSQL catalog reader (`docs/m6-contracts.md` §3.2) against the `postgres`
//! fixture: scratch objects (`[m6] scratch_prefix` and a token) are created, read in one full
//! read and in partial scopes, and dropped again.
//!
//! `#[ignore]`d: `cargo test -p oxbase-pg --test catalog_live -- --include-ignored`.

use std::path::PathBuf;
use std::time::Instant;

use futures::FutureExt;
use oxbase_dialect::{CatalogReader, CatalogScope, DialectRegistry, RelationName};
use oxbase_driver::{CancellationToken, ConnectParams, Connection, Driver, TlsMode, TlsOptions};
use oxbase_pg::PgDriver;
use oxbase_pg::dialect::PgCatalogReader;
use oxbase_types::{CaseRules, CatalogSnapshot, Engine, IdentCase, RelationKind, RoutineKind};

fn fixtures() -> toml::Table {
    let path = PathBuf::from(std::env::var("OXBASE_FIXTURES").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures.toml").into()
    }));
    let text = std::fs::read_to_string(&path).expect("read tests/fixtures.toml");
    text.parse().expect("parse tests/fixtures.toml")
}

fn s(t: &toml::Value, key: &str) -> String {
    t[key].as_str().expect(key).to_owned()
}

async fn connect() -> (Box<dyn Connection>, String, String, String) {
    let doc = toml::Value::Table(fixtures());
    let pg = &doc["engines"]["postgres"];
    let m6 = &doc["m6"]["postgres"];
    let database = s(m6, "database");
    let params = ConnectParams::network(Engine::Postgres, s(pg, "host"))
        .with_port(u16::try_from(pg["port"].as_integer().expect("port")).expect("port"))
        .with_user(s(pg, "user"))
        .with_password(s(pg, "password").as_str())
        .with_database(&database)
        .with_tls(TlsOptions::new(TlsMode::Disable));
    let c = PgDriver
        .connect(&params)
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"));
    let prefix = format!("{}pg{}_", s(&doc["m6"], "scratch_prefix"), token());
    (c, database, s(m6, "schema"), prefix)
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

async fn run(c: &dyn Connection, sql: &str) {
    oxbase_driver::collect(
        c.execute(sql, &[], CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}")),
    )
    .await
    .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn read(c: &dyn Connection, scope: &CatalogScope) -> CatalogSnapshot {
    PgCatalogReader
        .read(c, scope, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("read: {e}"))
}

async fn create(c: &dyn Connection, schema: &str, p: &str) {
    for sql in [
        format!("CREATE TYPE {schema}.{p}mood AS ENUM ('calm', 'busy')"),
        format!("CREATE SEQUENCE {schema}.{p}seq"),
        format!(
            "CREATE TABLE {schema}.{p}customer (region integer NOT NULL, id integer NOT NULL, \
             \"Display Name\" varchar(40), mood {schema}.{p}mood, PRIMARY KEY (region, id))"
        ),
        format!("COMMENT ON TABLE {schema}.{p}customer IS 'm6 parent table'"),
        format!(
            "CREATE TABLE {schema}.{p}order (oid integer PRIMARY KEY, region integer, \
             cust integer, FOREIGN KEY (region, cust) REFERENCES {schema}.{p}customer (region, id))"
        ),
        format!(
            "CREATE VIEW {schema}.{p}summary AS \
             SELECT region, count(*) AS n FROM {schema}.{p}customer GROUP BY region"
        ),
        format!(
            "CREATE FUNCTION {schema}.{p}double(x integer) RETURNS integer \
             LANGUAGE sql AS 'SELECT x * 2'"
        ),
        format!("CREATE PROCEDURE {schema}.{p}noop(a text) LANGUAGE sql AS 'SELECT 1'"),
    ] {
        run(c, &sql).await;
    }
}

async fn drop_all(c: &dyn Connection, schema: &str, p: &str) {
    for sql in [
        format!("DROP VIEW IF EXISTS {schema}.{p}summary"),
        format!("DROP TABLE IF EXISTS {schema}.{p}order"),
        format!("DROP TABLE IF EXISTS {schema}.{p}customer"),
        format!("DROP FUNCTION IF EXISTS {schema}.{p}double(integer)"),
        format!("DROP PROCEDURE IF EXISTS {schema}.{p}noop(text)"),
        format!("DROP SEQUENCE IF EXISTS {schema}.{p}seq"),
        format!("DROP TYPE IF EXISTS {schema}.{p}mood"),
    ] {
        let _ = c.execute(&sql, &[], CancellationToken::new()).await;
    }
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn pg_catalog_reader_reads_the_seeded_objects() {
    let (c, database, schema, p) = connect().await;
    create(c.as_ref(), &schema, &p).await;
    let checks = std::panic::AssertUnwindSafe(checks(c.as_ref(), &database, &schema, &p))
        .catch_unwind()
        .await;
    drop_all(c.as_ref(), &schema, &p).await;
    if let Err(panic) = checks {
        std::panic::resume_unwind(panic);
    }
}

async fn checks(c: &dyn Connection, database: &str, schema: &str, p: &str) {
    let started = Instant::now();
    let snap = read(c, &CatalogScope::full()).await;
    eprintln!(
        "[m6 pg catalog] full read: {} relations, {} columns, {} routines in {} ms",
        snap.relation_count(),
        snap.column_count(),
        snap.routine_count(),
        started.elapsed().as_millis()
    );
    assert_eq!(snap.engine, Engine::Postgres);
    assert_eq!(snap.database, database);
    assert_eq!(snap.default_schema.as_deref(), Some(schema));
    assert!(
        snap.search_path.iter().any(|s| s == schema),
        "{:?}",
        snap.search_path
    );
    assert!(snap.databases.iter().any(|d| d == database));
    assert_eq!(snap.case_rules, CaseRules::uniform(IdentCase::FoldLower));
    assert!(!snap.truncated);
    assert!(
        !snap
            .schemas
            .iter()
            .any(|s| s.name == "pg_catalog" || s.name == "information_schema"),
        "system schemas are left out by default"
    );

    let s = snap
        .schemas
        .iter()
        .find(|x| x.name == schema)
        .expect("the fixture schema");
    let rel = |name: &str| {
        s.relations
            .iter()
            .find(|r| r.name == format!("{p}{name}"))
            .unwrap_or_else(|| panic!("{p}{name} missing"))
    };
    let customer = rel("customer");
    assert_eq!(customer.kind, RelationKind::Table);
    assert_eq!(customer.comment.as_deref(), Some("m6 parent table"));
    let cols: Vec<(&str, &str, bool, bool)> = customer
        .columns
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.native_type.as_str(),
                c.nullable,
                c.primary_key,
            )
        })
        .collect();
    let mood = format!("{p}mood");
    assert_eq!(
        cols,
        [
            ("region", "integer", false, true),
            ("id", "integer", false, true),
            ("Display Name", "character varying(40)", true, false),
            ("mood", mood.as_str(), true, false),
        ]
    );
    let order = rel("order");
    assert_eq!(order.foreign_keys.len(), 1);
    let fk = &order.foreign_keys[0];
    assert_eq!(fk.columns, ["region", "cust"]);
    assert_eq!(fk.ref_table, format!("{p}customer"));
    assert_eq!(fk.ref_columns, ["region", "id"]);
    assert_eq!(fk.ref_schema, None);
    let summary = rel("summary");
    assert_eq!(summary.kind, RelationKind::View);
    let view_cols: Vec<&str> = summary.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(view_cols, ["region", "n"]);
    let double = s
        .routines
        .iter()
        .find(|r| r.name == format!("{p}double"))
        .expect("function");
    assert_eq!(double.kind, RoutineKind::Function);
    assert_eq!(double.args, "x integer");
    assert_eq!(double.returns.as_deref(), Some("integer"));
    let noop = s
        .routines
        .iter()
        .find(|r| r.name == format!("{p}noop"))
        .expect("procedure");
    assert_eq!(noop.kind, RoutineKind::Procedure);
    assert_eq!(noop.returns, None);
    assert!(s.sequences.contains(&format!("{p}seq")));
    assert!(s.types.contains(&mood));
    // Sequences are not relations.
    assert!(!s.relations.iter().any(|r| r.name == format!("{p}seq")));

    // Relations only (another case of the name: the reader matches ignoring case).
    let scope = CatalogScope {
        relations: Some(vec![
            RelationName {
                schema: Some(schema.to_owned()),
                name: format!("{p}ORDER"),
            },
            RelationName {
                schema: None,
                name: format!("{p}summary"),
            },
        ]),
        ..CatalogScope::full()
    };
    let part = read(c, &scope).await;
    let names: Vec<&str> = part
        .schemas
        .iter()
        .flat_map(|s| s.relations.iter().map(|r| r.name.as_str()))
        .collect();
    assert_eq!(names, [format!("{p}order"), format!("{p}summary")]);
    assert_eq!(part.routine_count(), 0);
    assert_eq!(part.schemas[0].relations[0].foreign_keys.len(), 1);

    // One schema, whole.
    let scope = CatalogScope {
        schemas: Some(vec![schema.to_owned()]),
        ..CatalogScope::full()
    };
    let part = read(c, &scope).await;
    assert_eq!(part.schemas.len(), 1);
    assert!(
        part.schemas[0]
            .routines
            .iter()
            .any(|r| r.name == format!("{p}double"))
    );

    // System schemas on request.
    let all = read(
        c,
        &CatalogScope {
            include_system: true,
            ..CatalogScope::full()
        },
    )
    .await;
    let catalog = all
        .schemas
        .iter()
        .find(|s| s.name == "pg_catalog")
        .expect("pg_catalog");
    assert!(catalog.system);
    assert!(catalog.relations.iter().any(|r| r.name == "pg_class"));

    // The column limit.
    let limited = read(
        c,
        &CatalogScope {
            max_columns: 2,
            ..CatalogScope::full()
        },
    )
    .await;
    assert!(limited.truncated);
    assert!(limited.column_count() <= 2);

    // Cancel.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = PgCatalogReader
        .read(c, &CatalogScope::full(), cancel)
        .await
        .unwrap_err();
    assert!(err.is_cancelled());
}

#[test]
fn the_reader_is_registered() {
    let mut reg = DialectRegistry::new();
    oxbase_pg::dialect::register(&mut reg);
    assert!(reg.has_catalog_reader(Engine::Postgres));
}
