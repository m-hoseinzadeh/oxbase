//! M6: the SQL Server catalog reader (`docs/m6-contracts.md` §3.2) against the `mssql`
//! fixture: scratch objects (`[m6] scratch_prefix` and a token) are created in
//! `[m6.mssql] database` / `schema`, read in one full read and in partial scopes, and dropped
//! again.
//!
//! `#[ignore]`d: `cargo test -p oxbase-mssql --test catalog_live -- --include-ignored`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use futures::FutureExt;
use oxbase_dialect::{CatalogReader, CatalogScope, DialectRegistry, RelationName};
use oxbase_driver::{CancellationToken, ConnectParams, Connection, collect};
use oxbase_mssql::MsSqlDriver;
use oxbase_mssql::dialect::MsSqlCatalogReader;
use oxbase_types::{
    CaseRules, CatalogSnapshot, Engine, IdentCase, RelationKind, RoutineKind, TlsMode, TlsOptions,
};

fn fixtures() -> toml::Value {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf();
    let path = std::env::var("OXBASE_FIXTURES")
        .ok()
        .filter(|p| !p.is_empty())
        .map_or_else(
            || root.join("tests/fixtures.toml"),
            |p| root.join(PathBuf::from(p)),
        );
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    toml::Value::Table(text.parse().expect("fixtures.toml parses"))
}

fn s(t: &toml::Value, key: &str) -> String {
    t[key].as_str().expect(key).to_owned()
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
    c: Box<dyn Connection>,
    database: String,
    schema: String,
    p: String,
}

async fn connect() -> Live {
    let doc = fixtures();
    let e = &doc["engines"]["mssql"];
    let m6 = &doc["m6"]["mssql"];
    let database = s(m6, "database");
    let params = ConnectParams::network(Engine::MsSql, s(e, "host"))
        .with_port(u16::try_from(e["port"].as_integer().expect("port")).expect("port"))
        .with_user(s(e, "user"))
        .with_password(s(e, "password"))
        .with_database(&database)
        .with_tls(TlsOptions::new(TlsMode::Disable));
    let c = MsSqlDriver::new()
        .connect_mssql(&params)
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"));
    Live {
        c: Box::new(c),
        database,
        schema: s(m6, "schema"),
        p: format!("{}mssql{}_", s(&doc["m6"], "scratch_prefix"), token()),
    }
}

impl Live {
    async fn run(&self, sql: &str) {
        collect(
            self.c
                .execute(sql, &[], CancellationToken::new())
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e}")),
        )
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    async fn read(&self, scope: &CatalogScope) -> CatalogSnapshot {
        MsSqlCatalogReader
            .read(self.c.as_ref(), scope, CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("read: {e}"))
    }

    async fn create(&self) {
        let (s, p) = (&self.schema, &self.p);
        for sql in [
            format!("CREATE TYPE {s}.{p}code FROM nvarchar(12) NULL"),
            format!("CREATE SEQUENCE {s}.{p}seq AS int START WITH 1"),
            format!(
                "CREATE TABLE {s}.{p}customer (region int NOT NULL, id int NOT NULL, \
                 [Display Name] nvarchar(40) NULL, code {s}.{p}code NULL, \
                 CONSTRAINT {p}customer_pk PRIMARY KEY (region, id))"
            ),
            format!(
                "EXEC sys.sp_addextendedproperty @name = N'MS_Description', \
                 @value = N'm6 parent table', @level0type = N'SCHEMA', @level0name = N'{s}', \
                 @level1type = N'TABLE', @level1name = N'{p}customer'"
            ),
            format!(
                "CREATE TABLE {s}.{p}order (oid int NOT NULL PRIMARY KEY, region int NULL, \
                 cust int NULL, CONSTRAINT {p}order_fk FOREIGN KEY (region, cust) \
                 REFERENCES {s}.{p}customer (region, id))"
            ),
            format!(
                "CREATE VIEW {s}.{p}summary AS SELECT region, COUNT(*) AS n \
                 FROM {s}.{p}customer GROUP BY region"
            ),
            format!("CREATE FUNCTION {s}.{p}double(@x int) RETURNS int AS BEGIN RETURN @x * 2 END"),
            format!("CREATE PROCEDURE {s}.{p}noop @a nvarchar(10), @b int OUTPUT AS SET @b = 1"),
        ] {
            self.run(&sql).await;
        }
    }

    async fn drop_all(&self) {
        let (s, p) = (&self.schema, &self.p);
        for sql in [
            format!("DROP VIEW IF EXISTS {s}.{p}summary"),
            format!("DROP TABLE IF EXISTS {s}.{p}order"),
            format!("DROP TABLE IF EXISTS {s}.{p}customer"),
            format!("DROP FUNCTION IF EXISTS {s}.{p}double"),
            format!("DROP PROCEDURE IF EXISTS {s}.{p}noop"),
            format!("DROP SEQUENCE IF EXISTS {s}.{p}seq"),
            format!("DROP TYPE IF EXISTS {s}.{p}code"),
        ] {
            let _ = self.c.execute(&sql, &[], CancellationToken::new()).await;
        }
    }
}

#[tokio::test]
#[ignore = "needs the mssql fixture"]
async fn mssql_catalog_reader_reads_the_seeded_objects() {
    let live = connect().await;
    live.create().await;
    let outcome = std::panic::AssertUnwindSafe(checks(&live))
        .catch_unwind()
        .await;
    live.drop_all().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn checks(live: &Live) {
    let (schema, p) = (&live.schema, &live.p);
    let started = Instant::now();
    let snap = live.read(&CatalogScope::full()).await;
    eprintln!(
        "[m6 mssql catalog] full read: {} relations, {} columns, {} routines in {} ms",
        snap.relation_count(),
        snap.column_count(),
        snap.routine_count(),
        started.elapsed().as_millis()
    );
    assert_eq!(snap.engine, Engine::MsSql);
    assert_eq!(snap.database, live.database);
    assert_eq!(snap.default_schema.as_deref(), Some(schema.as_str()));
    assert!(snap.databases.contains(&live.database));
    // The fixture database uses the server's case-insensitive default collation.
    assert_eq!(snap.case_rules, CaseRules::uniform(IdentCase::Insensitive));
    assert!(
        !snap
            .schemas
            .iter()
            .any(|s| s.name == "sys" || s.name == "db_owner"),
        "system schemas are left out by default"
    );
    let s = snap
        .schemas
        .iter()
        .find(|x| &x.name == schema)
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
    let code = format!("{schema}.{p}code");
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
    assert_eq!(
        cols,
        [
            ("region", "int", false, true),
            ("id", "int", false, true),
            ("Display Name", "nvarchar(40)", true, false),
            ("code", code.as_str(), true, false),
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
    assert_eq!(double.args, "@x int");
    assert_eq!(double.returns.as_deref(), Some("int"));
    let noop = s
        .routines
        .iter()
        .find(|r| r.name == format!("{p}noop"))
        .expect("procedure");
    assert_eq!(noop.kind, RoutineKind::Procedure);
    assert_eq!(noop.args, "@a nvarchar(10), @b int OUTPUT");
    assert_eq!(noop.returns, None);
    assert!(s.sequences.contains(&format!("{p}seq")));
    assert!(s.types.contains(&format!("{p}code")));

    // Relations only (another case of the name; the collation ignores case anyway).
    let scope = CatalogScope {
        relations: Some(vec![
            RelationName {
                schema: Some(schema.clone()),
                name: format!("{p}ORDER"),
            },
            RelationName {
                schema: None,
                name: format!("{p}summary"),
            },
        ]),
        ..CatalogScope::full()
    };
    let part = live.read(&scope).await;
    let names: Vec<&str> = part
        .schemas
        .iter()
        .flat_map(|s| s.relations.iter().map(|r| r.name.as_str()))
        .collect();
    assert_eq!(names, [format!("{p}order"), format!("{p}summary")]);
    assert_eq!(part.routine_count(), 0);

    // System schemas on request.
    let all = live
        .read(&CatalogScope {
            include_system: true,
            ..CatalogScope::full()
        })
        .await;
    let sys = all.schemas.iter().find(|s| s.name == "sys").expect("sys");
    assert!(sys.system);
    assert!(sys.relations.iter().any(|r| r.name == "objects"));

    let limited = live
        .read(&CatalogScope {
            max_columns: 2,
            ..CatalogScope::full()
        })
        .await;
    assert!(limited.truncated);
    assert!(limited.column_count() <= 2);

    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = MsSqlCatalogReader
        .read(live.c.as_ref(), &CatalogScope::full(), cancel)
        .await
        .unwrap_err();
    assert!(err.is_cancelled());
}

#[test]
fn the_reader_is_registered() {
    let mut reg = DialectRegistry::new();
    oxbase_mssql::dialect::register(&mut reg);
    assert!(reg.has_catalog_reader(Engine::MsSql));
}
