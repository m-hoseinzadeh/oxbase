//! M6: the MySQL / MariaDB catalog reader (`docs/m6-contracts.md` §3.2) against the `mysql`
//! and `mariadb` fixtures: scratch objects (`[m6] scratch_prefix` and a token) are created in
//! `[m6.<engine>] database`, read in one full read and in partial scopes, and dropped again.
//!
//! `#[ignore]`d: `cargo test -p oxbase-mysql --test catalog_live -- --include-ignored`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use futures::FutureExt;
use oxbase_dialect::{CatalogReader, CatalogScope, DialectRegistry, RelationName};
use oxbase_driver::{CancellationToken, ConnectParams, Connection, collect};
use oxbase_mysql::MySqlDriver;
use oxbase_mysql::dialect::MySqlCatalogReader;
use oxbase_types::{
    CatalogSnapshot, Engine, IdentCase, RelationKind, RoutineKind, TlsMode, TlsOptions,
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
    key: &'static str,
    engine: Engine,
    database: String,
    p: String,
}

async fn connect(key: &'static str, engine: Engine) -> Live {
    let doc = fixtures();
    let e = &doc["engines"][key];
    let database = s(&doc["m6"][key], "database");
    let params = ConnectParams::network(engine, s(e, "host"))
        .with_port(u16::try_from(e["port"].as_integer().expect("port")).expect("port"))
        .with_user(s(e, "user"))
        .with_password(s(e, "password"))
        .with_database(&database)
        .with_tls(TlsOptions::new(TlsMode::Disable));
    let c = MySqlDriver::new()
        .connect_mysql(&params)
        .await
        .unwrap_or_else(|e| panic!("connect {key}: {e}"));
    Live {
        c: Box::new(c),
        key,
        engine,
        database,
        p: format!("{}{key}{}_", s(&doc["m6"], "scratch_prefix"), token()),
    }
}

impl Live {
    async fn run(&self, sql: &str) {
        collect(
            self.c
                .execute(sql, &[], CancellationToken::new())
                .await
                .unwrap_or_else(|e| panic!("{}: {sql}: {e}", self.key)),
        )
        .await
        .unwrap_or_else(|e| panic!("{}: {sql}: {e}", self.key));
    }

    async fn read(&self, scope: &CatalogScope) -> CatalogSnapshot {
        MySqlCatalogReader
            .read(self.c.as_ref(), scope, CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{}: read: {e}", self.key))
    }

    async fn create(&self) {
        let p = &self.p;
        let mut statements = vec![
            format!(
                "CREATE TABLE {p}customer (region INT NOT NULL, id INT NOT NULL, \
                 `Display Name` VARCHAR(40), PRIMARY KEY (region, id)) COMMENT = 'm6 parent table'"
            ),
            format!(
                "CREATE TABLE {p}order (oid INT PRIMARY KEY, region INT, cust INT, \
                 FOREIGN KEY (region, cust) REFERENCES {p}customer (region, id))"
            ),
            format!(
                "CREATE VIEW {p}summary AS SELECT region, COUNT(*) AS n FROM {p}customer \
                 GROUP BY region"
            ),
            format!("CREATE FUNCTION {p}double(x INT) RETURNS INT DETERMINISTIC RETURN x * 2"),
            format!("CREATE PROCEDURE {p}noop(IN a VARCHAR(10), OUT b INT) SET b = 1"),
        ];
        if self.engine == Engine::MariaDb {
            statements.push(format!("CREATE SEQUENCE {p}seq"));
        }
        for sql in statements {
            self.run(&sql).await;
        }
    }

    async fn drop_all(&self) {
        let p = &self.p;
        for sql in [
            format!("DROP VIEW IF EXISTS {p}summary"),
            format!("DROP TABLE IF EXISTS {p}order"),
            format!("DROP TABLE IF EXISTS {p}customer"),
            format!("DROP FUNCTION IF EXISTS {p}double"),
            format!("DROP PROCEDURE IF EXISTS {p}noop"),
            format!("DROP SEQUENCE IF EXISTS {p}seq"),
        ] {
            if self.engine == Engine::MySql && sql.contains("SEQUENCE") {
                continue;
            }
            let _ = self.c.execute(&sql, &[], CancellationToken::new()).await;
        }
    }
}

async fn scenario(key: &'static str, engine: Engine) {
    let live = connect(key, engine).await;
    live.drop_all().await;
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
    let p = &live.p;
    let key = live.key;
    let started = Instant::now();
    let snap = live.read(&CatalogScope::full()).await;
    eprintln!(
        "[m6 {key} catalog] full read: {} relations, {} columns, {} routines in {} ms",
        snap.relation_count(),
        snap.column_count(),
        snap.routine_count(),
        started.elapsed().as_millis()
    );
    assert_eq!(snap.engine, live.engine);
    assert_eq!(snap.database, live.database);
    assert_eq!(snap.default_schema.as_deref(), Some(live.database.as_str()));
    assert_eq!(snap.search_path, std::slice::from_ref(&live.database));
    assert!(snap.databases.contains(&live.database));
    // The Linux fixtures run with lower_case_table_names = 0.
    assert_eq!(snap.case_rules.relations, IdentCase::Sensitive, "{key}");
    assert_eq!(snap.case_rules.columns, IdentCase::Insensitive, "{key}");
    assert_eq!(snap.schemas.len(), 1);
    let s = &snap.schemas[0];
    assert_eq!(s.name, live.database);
    let rel = |name: &str| {
        s.relations
            .iter()
            .find(|r| r.name == format!("{p}{name}"))
            .unwrap_or_else(|| panic!("{key}: {p}{name} missing"))
    };
    let customer = rel("customer");
    assert_eq!(customer.kind, RelationKind::Table);
    assert_eq!(customer.comment.as_deref(), Some("m6 parent table"));
    // MariaDB keeps the display width of integer types (`int(11)`).
    let norm = |t: &str| t.replace("(11)", "");
    let cols: Vec<(&str, String, bool, bool)> = customer
        .columns
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                norm(&c.native_type),
                c.nullable,
                c.primary_key,
            )
        })
        .collect();
    assert_eq!(
        cols,
        [
            ("region", "int".to_owned(), false, true),
            ("id", "int".to_owned(), false, true),
            ("Display Name", "varchar(40)".to_owned(), true, false),
        ],
        "{key}"
    );
    let order = rel("order");
    assert_eq!(order.foreign_keys.len(), 1, "{key}");
    let fk = &order.foreign_keys[0];
    assert_eq!(fk.columns, ["region", "cust"]);
    assert_eq!(fk.ref_table, format!("{p}customer"));
    assert_eq!(fk.ref_columns, ["region", "id"]);
    assert_eq!(fk.ref_schema, None);
    let summary = rel("summary");
    assert_eq!(summary.kind, RelationKind::View);
    assert_eq!(summary.comment, None);
    let view_cols: Vec<&str> = summary.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(view_cols, ["region", "n"]);
    let double = s
        .routines
        .iter()
        .find(|r| r.name == format!("{p}double"))
        .expect("function");
    assert_eq!(double.kind, RoutineKind::Function);
    assert_eq!(norm(&double.args), "x int");
    assert_eq!(double.returns.as_deref().map(norm).as_deref(), Some("int"));
    let noop = s
        .routines
        .iter()
        .find(|r| r.name == format!("{p}noop"))
        .expect("procedure");
    assert_eq!(noop.kind, RoutineKind::Procedure);
    assert_eq!(norm(&noop.args), "IN a varchar(10), OUT b int");
    if live.engine == Engine::MariaDb {
        assert!(s.sequences.contains(&format!("{p}seq")), "{key}");
        assert!(!s.relations.iter().any(|r| r.name == format!("{p}seq")));
    }

    // Relations only.
    let scope = CatalogScope {
        relations: Some(vec![
            RelationName {
                schema: None,
                name: format!("{p}order"),
            },
            RelationName {
                schema: Some(live.database.clone()),
                name: format!("{p}summary"),
            },
            RelationName {
                schema: Some("another_database".into()),
                name: format!("{p}customer"),
            },
        ]),
        ..CatalogScope::full()
    };
    let part = live.read(&scope).await;
    let names: Vec<&str> = part.schemas[0]
        .relations
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(names, [format!("{p}order"), format!("{p}summary")], "{key}");
    assert_eq!(part.schemas[0].routines.len(), 0);
    assert_eq!(part.schemas[0].relations[0].foreign_keys.len(), 1);

    // A scope on another database reads nothing of this one.
    let other = live
        .read(&CatalogScope {
            schemas: Some(vec!["another_database".into()]),
            ..CatalogScope::full()
        })
        .await;
    assert_eq!(other.schemas.len(), 0);

    // The column limit.
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
    let err = MySqlCatalogReader
        .read(live.c.as_ref(), &CatalogScope::full(), cancel)
        .await
        .unwrap_err();
    assert!(err.is_cancelled());
}

#[tokio::test]
#[ignore = "needs the mysql fixture"]
async fn mysql_catalog_reader_reads_the_seeded_objects() {
    scenario("mysql", Engine::MySql).await;
}

#[tokio::test]
#[ignore = "needs the mariadb fixture"]
async fn mariadb_catalog_reader_reads_the_seeded_objects() {
    scenario("mariadb", Engine::MariaDb).await;
}

#[test]
fn the_reader_is_registered_for_both_engines() {
    let mut reg = DialectRegistry::new();
    oxbase_mysql::dialect::register(&mut reg);
    assert!(reg.has_catalog_reader(Engine::MySql));
    assert!(reg.has_catalog_reader(Engine::MariaDb));
}
