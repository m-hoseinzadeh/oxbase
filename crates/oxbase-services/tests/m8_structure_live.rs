//! Live tests of structure sync (`docs/m8-contracts.md` §5, §12) against the Docker fixtures
//! (PostgreSQL, MySQL, MariaDB, SQL Server) and SQLite files, through
//! `oxbase_services::sync::StructureSyncService`:
//!
//! - **Scratch pairs** (always): two containers named `m8_t_<token>_s` / `_t` (schemas on
//!   PostgreSQL and SQL Server, databases on MySQL / MariaDB, two files on SQLite) seeded here
//!   with the designed differences of `[m8]`. The compare reports them; a destructive apply
//!   without confirmation and a stale apply (the target changed after the compare) are refused;
//!   the script of every item with drops runs and the re-compare is in sync. A second pair
//!   without drops keeps `legacy_notes` and `order_lines`, and the re-compare reports only
//!   them.
//! - **The `[m8]` fixture pair** (`m8_src` / `m8_tgt`, seeded by the conformance stream): the
//!   compare reports `[m8] structure_items` (minus `skip_items`); both sides are copied into
//!   scratch containers by structure sync itself (so the shared fixture is never changed), and
//!   the copy of `m8_tgt` is synced from the copy of `m8_src` with drops: the script runs and
//!   the re-compare is in sync. Missing seeds are reported as pending unless
//!   `OXBASE_SERVICES_REQUIRE_M8=1`.
//!
//! MySQL and MariaDB use the fixture's root account (scratch databases need `CREATE`).
//!
//! ```sh
//! scripts/fixtures-up.sh
//! cargo test -p oxbase-services --test m8_structure_live -- --include-ignored --test-threads 1
//! ```

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_services::sync::{
    DiffStatus, StructureApplyRequest, StructureCompareRequest, StructureCompareResult,
    StructureOptions, StructureSyncService,
};
use oxbase_services::tree::TreeService;
use oxbase_services::{
    CancellationToken, ConnectionManager, ConnectionProfile, ProvidedSecrets, SecretRef,
    SecretStore, SessionHandle, default_dialects, default_drivers,
};
use oxbase_store::{MemorySecretStore, ProfileAddress, Secret, SecretSlot, TlsMode, TlsSettings};
use oxbase_types::{Engine, ObjectRef};

fn report(line: &str) {
    let _ = writeln!(std::io::stderr().lock(), "[m8 structure] {line}");
}

fn require_m8() -> bool {
    std::env::var("OXBASE_SERVICES_REQUIRE_M8").is_ok_and(|v| v == "1")
}

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

fn table<'a>(t: &'a toml::Table, key: &str) -> &'a toml::Table {
    t.get(key)
        .and_then(toml::Value::as_table)
        .unwrap_or_else(|| panic!("fixtures.toml: missing table {key}"))
}

fn string(t: &toml::Table, key: &str) -> String {
    t.get(key)
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("fixtures.toml: missing string {key}"))
        .to_owned()
}

fn strings(t: &toml::Table, key: &str) -> Vec<String> {
    t.get(key)
        .and_then(toml::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// 10 lowercase base36 characters from the time and the process id.
fn token() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock");
    let mut n = (u128::from(now.as_secs()) << 40)
        | (u128::from(std::process::id()) << 8)
        | u128::from(now.subsec_nanos() % 256);
    let mut out = String::new();
    while out.len() < 10 {
        let digit = u32::try_from(n % 36).unwrap_or_default();
        out.push(char::from_digit(digit, 36).unwrap_or('0'));
        n /= 36;
    }
    out
}

/// A connected engine with the structure sync service.
struct Live {
    key: &'static str,
    engine: Engine,
    fx: toml::Table,
    manager: Arc<ConnectionManager>,
    secrets: Arc<MemorySecretStore>,
    sync: StructureSyncService,
    /// The fixture database (PostgreSQL / SQL Server catalog).
    database: String,
    dir: tempfile::TempDir,
}

impl Live {
    fn new(key: &'static str, engine: Engine) -> Self {
        let secrets = Arc::new(MemorySecretStore::new());
        let manager = Arc::new(ConnectionManager::new(
            default_drivers(),
            secrets.clone(),
            Arc::new(RusshTunnelProvider::new()),
        ));
        let dialects = Arc::new(default_dialects());
        let tree = Arc::new(TreeService::new(
            Arc::clone(&manager),
            Arc::clone(&dialects),
        ));
        let sync = StructureSyncService::new(Arc::clone(&manager), tree, dialects);
        let fx = fixtures();
        let database = if engine == Engine::Sqlite {
            "main".to_owned()
        } else {
            string(table(table(&fx, "engines"), key), "database")
        };
        Self {
            key,
            engine,
            fx,
            manager,
            secrets,
            sync,
            database,
            dir: tempfile::tempdir().expect("temp dir"),
        }
    }

    /// A base session: the fixture server (MySQL / MariaDB as root), or SQLite file `file`.
    async fn open(&self, file: &str) -> SessionHandle {
        let profile = if self.engine == Engine::Sqlite {
            ConnectionProfile::new(
                format!("m8 {}", self.key),
                self.engine,
                ProfileAddress::File {
                    path: self.dir.path().join(file),
                },
            )
        } else {
            let e = table(table(&self.fx, "engines"), self.key);
            let mut p = ConnectionProfile::new(
                format!("m8 {} {file}", self.key),
                self.engine,
                ProfileAddress::Network {
                    host: string(e, "host"),
                    port: e
                        .get("port")
                        .and_then(toml::Value::as_integer)
                        .and_then(|p| u16::try_from(p).ok()),
                },
            );
            let mysql = matches!(self.engine, Engine::MySql | Engine::MariaDb);
            let (user, password) = if mysql {
                ("root".to_owned(), string(e, "root_password"))
            } else {
                (string(e, "user"), string(e, "password"))
            };
            p.user = Some(user);
            p.database = Some(self.database.clone());
            let secret = p.secret_key(SecretSlot::Password);
            self.secrets
                .set(&secret, &Secret::new(password))
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
        self.manager
            .open(&profile, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{}: connect failed: {e}", self.key))
    }

    /// The container ref of `name` (a schema of the fixture database, or a database).
    fn container(&self, name: &str) -> ObjectRef {
        match self.engine {
            Engine::Postgres | Engine::MsSql => ObjectRef::schema(Some(&self.database), name),
            Engine::Sqlite => ObjectRef::database("main"),
            _ => ObjectRef::database(name),
        }
    }

    /// Runs `statements` on `session`, panicking on the first failure.
    async fn run(&self, session: &SessionHandle, statements: &[String]) {
        for sql in statements {
            let stream = session
                .connection()
                .execute(sql, &[], CancellationToken::new())
                .await
                .unwrap_or_else(|e| panic!("{}: {sql}: {e}", self.key));
            oxbase_driver::collect(stream)
                .await
                .unwrap_or_else(|e| panic!("{}: {sql}: {e}", self.key));
        }
    }

    /// Runs `statements`, ignoring failures (clean-up).
    async fn run_quietly(&self, session: &SessionHandle, statements: &[String]) {
        for sql in statements {
            if let Ok(stream) = session
                .connection()
                .execute(sql, &[], CancellationToken::new())
                .await
            {
                let _ = oxbase_driver::collect(stream).await;
            }
        }
    }

    async fn compare(
        &self,
        source: (&SessionHandle, &ObjectRef),
        target: (&SessionHandle, &ObjectRef),
        include_drops: bool,
    ) -> StructureCompareResult {
        self.try_compare(source, target, include_drops)
            .await
            .unwrap_or_else(|e| panic!("{}: compare: {e}", self.key))
    }

    async fn try_compare(
        &self,
        source: (&SessionHandle, &ObjectRef),
        target: (&SessionHandle, &ObjectRef),
        include_drops: bool,
    ) -> oxbase_types::Result<StructureCompareResult> {
        let request = StructureCompareRequest {
            source_session: source.0.id(),
            source: source.1.clone(),
            target_session: target.0.id(),
            target: target.1.clone(),
            options: StructureOptions {
                include_drops,
                ..StructureOptions::default()
            },
        };
        self.sync.compare(&request, CancellationToken::new()).await
    }

    /// Applies every item of `compared` (confirmed) and checks the run and the re-compare.
    async fn apply_all(
        &self,
        compared: &StructureCompareResult,
        what: &str,
    ) -> StructureCompareResult {
        let applied = self
            .sync
            .apply(
                &StructureApplyRequest {
                    compare_id: compared.compare_id,
                    selection: Vec::new(),
                    confirm_destructive: true,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|e| panic!("{}: {what}: apply: {e}", self.key));
        report(&format!(
            "{}: {what}: {} items, atomic {}, destructive {}\n{}",
            self.key,
            applied.script.items.len(),
            applied.script.atomic,
            applied.script.destructive,
            applied.script.sql
        ));
        let failed: Vec<String> = applied
            .outcome
            .steps
            .iter()
            .filter_map(|o| {
                o.error.as_ref().map(|e| {
                    format!(
                        "step {} ({}): {}",
                        o.index, applied.script.script.steps[o.index].sql, e.message
                    )
                })
            })
            .collect();
        assert!(
            applied.outcome.committed && failed.is_empty(),
            "{}: {what}: the script failed: {failed:#?}",
            self.key
        );
        let left: Vec<String> = applied
            .recompare
            .diff
            .items
            .iter()
            .filter(|i| i.status != DiffStatus::Same)
            .map(|i| format!("{} {:?} {:?}", i.id, i.status, i.properties))
            .collect();
        assert!(
            applied.in_sync,
            "{}: {what}: not in sync after the apply: {left:#?}",
            self.key
        );
        applied.recompare
    }
}

/// The seed of one side of a scratch pair, in the engine's SQL (`c` is the container name).
fn seed(engine: Engine, c: &str, source: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| out.push(s);
    match engine {
        Engine::Postgres => {
            push(format!("CREATE SCHEMA {c}"));
            if source {
                push(format!(
                    "CREATE TABLE {c}.customers (id integer PRIMARY KEY, name varchar(100) NOT NULL, email varchar(200))"
                ));
                push(format!(
                    "CREATE TABLE {c}.products (id integer PRIMARY KEY, name varchar(100) NOT NULL, price numeric(10,2))"
                ));
                push(format!(
                    "CREATE TABLE {c}.orders (id integer PRIMARY KEY, customer_id integer NOT NULL, total numeric(12,2), CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES {c}.customers (id) ON DELETE CASCADE)"
                ));
                push(format!(
                    "CREATE TABLE {c}.order_audit (id serial PRIMARY KEY, order_id integer NOT NULL, at timestamp DEFAULT now())"
                ));
                push(format!(
                    "CREATE TABLE {c}.order_items (id integer PRIMARY KEY, order_id integer NOT NULL, product_id integer NOT NULL, qty integer NOT NULL, CONSTRAINT order_items_qty_check CHECK (qty > 0), CONSTRAINT order_items_order_fk FOREIGN KEY (order_id) REFERENCES {c}.orders (id) ON DELETE CASCADE, CONSTRAINT order_items_product_fk FOREIGN KEY (product_id) REFERENCES {c}.products (id))"
                ));
                push(format!(
                    "CREATE INDEX order_items_product_idx ON {c}.order_items (product_id)"
                ));
                push(format!(
                    "CREATE TABLE {c}.\"MixedCase\" (\"Id\" integer PRIMARY KEY, \"Label\" varchar(20))"
                ));
                push(format!(
                    "CREATE TABLE {c}.\"select\" (id integer PRIMARY KEY, \"from\" varchar(10), \"order\" integer)"
                ));
                push(format!("CREATE SEQUENCE {c}.invoice_seq START WITH 1000"));
                push(format!("CREATE SEQUENCE {c}.ticket_seq INCREMENT BY 5"));
                push(format!(
                    "CREATE VIEW {c}.active_customers AS SELECT id, name, email FROM {c}.customers WHERE email IS NOT NULL"
                ));
                push(format!(
                    "CREATE FUNCTION {c}.order_total(p_id integer) RETURNS numeric LANGUAGE plpgsql STABLE AS $$ BEGIN RETURN (SELECT sum(qty) FROM {c}.order_items WHERE order_id = p_id); END $$"
                ));
                push(format!(
                    "CREATE FUNCTION {c}.add_one(n integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT n + 1'"
                ));
            } else {
                push(format!(
                    "CREATE TABLE {c}.customers (id integer PRIMARY KEY, name varchar(100) NOT NULL)"
                ));
                push(format!(
                    "CREATE TABLE {c}.products (id integer PRIMARY KEY, name varchar(100) NOT NULL, price numeric(8,2))"
                ));
                push(format!(
                    "CREATE TABLE {c}.orders (id integer PRIMARY KEY, customer_id integer NOT NULL, total numeric(12,2), CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES {c}.customers (id))"
                ));
                push(format!(
                    "CREATE TABLE {c}.order_lines (id integer PRIMARY KEY, order_id integer NOT NULL, CONSTRAINT order_lines_order_fk FOREIGN KEY (order_id) REFERENCES {c}.orders (id))"
                ));
                push(format!(
                    "CREATE TABLE {c}.legacy_notes (id integer PRIMARY KEY, note text)"
                ));
                push(format!(
                    "CREATE TABLE {c}.\"select\" (id integer PRIMARY KEY, \"from\" varchar(10))"
                ));
                push(format!("CREATE SEQUENCE {c}.ticket_seq INCREMENT BY 1"));
                push(format!(
                    "CREATE VIEW {c}.active_customers AS SELECT id, name FROM {c}.customers"
                ));
                push(format!(
                    "CREATE FUNCTION {c}.add_one(n integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT n + 2'"
                ));
            }
            // The trigger function exists on both sides (equal once the schema is moved).
            push(format!(
                "CREATE FUNCTION {c}.audit_order() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO {c}.order_audit (order_id) VALUES (NEW.id); RETURN NEW; END $$"
            ));
            if source {
                push(format!(
                    "CREATE TRIGGER orders_audit AFTER INSERT ON {c}.orders FOR EACH ROW EXECUTE FUNCTION {c}.audit_order()"
                ));
            }
        }
        Engine::MySql | Engine::MariaDb => {
            push(format!("CREATE DATABASE `{c}`"));
            if source {
                push(format!(
                    "CREATE TABLE `{c}`.customers (id INT NOT NULL PRIMARY KEY, name VARCHAR(100) NOT NULL, email VARCHAR(200) NULL)"
                ));
                push(format!(
                    "CREATE TABLE `{c}`.products (id INT NOT NULL PRIMARY KEY, name VARCHAR(100) NOT NULL, price DECIMAL(10,2) NULL)"
                ));
                push(format!(
                    "CREATE TABLE `{c}`.orders (id INT NOT NULL PRIMARY KEY, customer_id INT NOT NULL, total DECIMAL(12,2) NULL, CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES `{c}`.customers (id) ON DELETE CASCADE)"
                ));
                push(format!(
                    "CREATE TABLE `{c}`.order_audit (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, order_id INT NOT NULL, at DATETIME NULL)"
                ));
                push(format!(
                    "CREATE TABLE `{c}`.order_items (id INT NOT NULL PRIMARY KEY, order_id INT NOT NULL, product_id INT NOT NULL, qty INT NOT NULL, KEY order_items_product_idx (product_id), CONSTRAINT order_items_order_fk FOREIGN KEY (order_id) REFERENCES `{c}`.orders (id) ON DELETE CASCADE, CONSTRAINT order_items_product_fk FOREIGN KEY (product_id) REFERENCES `{c}`.products (id))"
                ));
                push(format!(
                    "CREATE TABLE `{c}`.`MixedCase` (`Id` INT NOT NULL PRIMARY KEY, `Label` VARCHAR(20) NULL)"
                ));
                push(format!(
                    "CREATE TABLE `{c}`.`select` (id INT NOT NULL PRIMARY KEY, `from` VARCHAR(10) NULL, `order` INT NULL)"
                ));
                push(format!(
                    "CREATE VIEW `{c}`.active_customers AS SELECT id, name, email FROM `{c}`.customers WHERE email IS NOT NULL"
                ));
                push(format!(
                    "CREATE FUNCTION `{c}`.order_total(p_id INT) RETURNS DECIMAL(12,2) READS SQL DATA RETURN (SELECT SUM(qty) FROM `{c}`.order_items WHERE order_id = p_id)"
                ));
                push(format!(
                    "CREATE PROCEDURE `{c}`.touch_order(IN p_id INT) MODIFIES SQL DATA UPDATE `{c}`.orders SET total = total WHERE id = p_id"
                ));
                push(format!(
                    "CREATE TRIGGER `{c}`.orders_audit AFTER INSERT ON `{c}`.orders FOR EACH ROW INSERT INTO `{c}`.order_audit (order_id) VALUES (NEW.id)"
                ));
                if engine == Engine::MariaDb {
                    push(format!("CREATE SEQUENCE `{c}`.invoice_seq START WITH 1000"));
                }
            } else {
                push(format!(
                    "CREATE TABLE `{c}`.customers (id INT NOT NULL PRIMARY KEY, name VARCHAR(100) NOT NULL)"
                ));
                push(format!(
                    "CREATE TABLE `{c}`.products (id INT NOT NULL PRIMARY KEY, name VARCHAR(100) NOT NULL, price DECIMAL(8,2) NULL)"
                ));
                push(format!(
                    "CREATE TABLE `{c}`.orders (id INT NOT NULL PRIMARY KEY, customer_id INT NOT NULL, total DECIMAL(12,2) NULL, CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES `{c}`.customers (id))"
                ));
                push(format!(
                    "CREATE TABLE `{c}`.order_lines (id INT NOT NULL PRIMARY KEY, order_id INT NOT NULL, CONSTRAINT order_lines_order_fk FOREIGN KEY (order_id) REFERENCES `{c}`.orders (id))"
                ));
                push(format!(
                    "CREATE TABLE `{c}`.legacy_notes (id INT NOT NULL PRIMARY KEY, note TEXT NULL)"
                ));
                push(format!(
                    "CREATE TABLE `{c}`.`select` (id INT NOT NULL PRIMARY KEY, `from` VARCHAR(10) NULL)"
                ));
                push(format!(
                    "CREATE VIEW `{c}`.active_customers AS SELECT id, name FROM `{c}`.customers"
                ));
                push(format!(
                    "CREATE PROCEDURE `{c}`.touch_order(IN p_id INT) MODIFIES SQL DATA UPDATE `{c}`.orders SET total = total + 0 WHERE id = p_id"
                ));
            }
        }
        Engine::MsSql => {
            push(format!("CREATE SCHEMA [{c}]"));
            if source {
                push(format!(
                    "CREATE TABLE [{c}].customers (id int NOT NULL CONSTRAINT pk_customers PRIMARY KEY, name nvarchar(100) NOT NULL, email nvarchar(200) NULL)"
                ));
                push(format!(
                    "CREATE TABLE [{c}].products (id int NOT NULL CONSTRAINT pk_products PRIMARY KEY, name nvarchar(100) NOT NULL, price decimal(10,2) NULL)"
                ));
                push(format!(
                    "CREATE TABLE [{c}].orders (id int NOT NULL CONSTRAINT pk_orders PRIMARY KEY, customer_id int NOT NULL, total decimal(12,2) NULL, CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES [{c}].customers (id) ON DELETE CASCADE)"
                ));
                push(format!(
                    "CREATE TABLE [{c}].order_audit (id int IDENTITY(1,1) NOT NULL CONSTRAINT pk_order_audit PRIMARY KEY, order_id int NOT NULL, at datetime2 NULL)"
                ));
                push(format!(
                    "CREATE TABLE [{c}].order_items (id int NOT NULL CONSTRAINT pk_order_items PRIMARY KEY, order_id int NOT NULL, product_id int NOT NULL, qty int NOT NULL CONSTRAINT order_items_qty_check CHECK (qty > 0), CONSTRAINT order_items_order_fk FOREIGN KEY (order_id) REFERENCES [{c}].orders (id) ON DELETE CASCADE, CONSTRAINT order_items_product_fk FOREIGN KEY (product_id) REFERENCES [{c}].products (id))"
                ));
                push(format!(
                    "CREATE TABLE [{c}].[MixedCase] ([Id] int NOT NULL CONSTRAINT pk_mixedcase PRIMARY KEY, [Label] nvarchar(20) NULL)"
                ));
                push(format!(
                    "CREATE TABLE [{c}].[select] (id int NOT NULL CONSTRAINT pk_select PRIMARY KEY, [from] nvarchar(10) NULL, [order] int NULL)"
                ));
                push(format!(
                    "CREATE SEQUENCE [{c}].invoice_seq AS bigint START WITH 1000"
                ));
                push(format!(
                    "CREATE VIEW [{c}].active_customers AS SELECT id, name, email FROM [{c}].customers WHERE email IS NOT NULL"
                ));
                push(format!(
                    "CREATE FUNCTION [{c}].order_total(@p_id int) RETURNS int AS BEGIN RETURN (SELECT SUM(qty) FROM [{c}].order_items WHERE order_id = @p_id) END"
                ));
                push(format!(
                    "CREATE PROCEDURE [{c}].touch_order @p_id int AS UPDATE [{c}].orders SET total = total WHERE id = @p_id"
                ));
                push(format!(
                    "CREATE TRIGGER [{c}].orders_audit ON [{c}].orders AFTER INSERT AS INSERT INTO [{c}].order_audit (order_id) SELECT id FROM inserted"
                ));
            } else {
                push(format!(
                    "CREATE TABLE [{c}].customers (id int NOT NULL CONSTRAINT pk_customers PRIMARY KEY, name nvarchar(100) NOT NULL)"
                ));
                push(format!(
                    "CREATE TABLE [{c}].products (id int NOT NULL CONSTRAINT pk_products PRIMARY KEY, name nvarchar(100) NOT NULL, price decimal(8,2) NULL)"
                ));
                push(format!(
                    "CREATE TABLE [{c}].orders (id int NOT NULL CONSTRAINT pk_orders PRIMARY KEY, customer_id int NOT NULL, total decimal(12,2) NULL, CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES [{c}].customers (id))"
                ));
                push(format!(
                    "CREATE TABLE [{c}].order_lines (id int NOT NULL CONSTRAINT pk_order_lines PRIMARY KEY, order_id int NOT NULL, CONSTRAINT order_lines_order_fk FOREIGN KEY (order_id) REFERENCES [{c}].orders (id))"
                ));
                push(format!(
                    "CREATE TABLE [{c}].legacy_notes (id int NOT NULL CONSTRAINT pk_legacy_notes PRIMARY KEY, note nvarchar(max) NULL)"
                ));
                push(format!(
                    "CREATE TABLE [{c}].[select] (id int NOT NULL CONSTRAINT pk_select PRIMARY KEY, [from] nvarchar(10) NULL)"
                ));
                push(format!(
                    "CREATE VIEW [{c}].active_customers AS SELECT id, name FROM [{c}].customers"
                ));
                push(format!(
                    "CREATE PROCEDURE [{c}].touch_order @p_id int AS UPDATE [{c}].orders SET total = total + 0 WHERE id = @p_id"
                ));
            }
        }
        _ => {
            if source {
                push("CREATE TABLE customers (id INTEGER NOT NULL PRIMARY KEY, name VARCHAR(100) NOT NULL, email VARCHAR(200))".into());
                push("CREATE TABLE products (id INTEGER NOT NULL PRIMARY KEY, name VARCHAR(100) NOT NULL, price DECIMAL(10,2))".into());
                push("CREATE TABLE orders (id INTEGER NOT NULL PRIMARY KEY, customer_id INTEGER NOT NULL, total DECIMAL(12,2), CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES customers (id) ON DELETE CASCADE)".into());
                push("CREATE TABLE order_audit (id INTEGER PRIMARY KEY AUTOINCREMENT, order_id INTEGER NOT NULL, at TIMESTAMP)".into());
                push("CREATE TABLE order_items (id INTEGER NOT NULL PRIMARY KEY, order_id INTEGER NOT NULL, product_id INTEGER NOT NULL, qty INTEGER NOT NULL, CONSTRAINT order_items_qty_check CHECK (qty > 0), CONSTRAINT order_items_order_fk FOREIGN KEY (order_id) REFERENCES orders (id) ON DELETE CASCADE, CONSTRAINT order_items_product_fk FOREIGN KEY (product_id) REFERENCES products (id))".into());
                push("CREATE INDEX order_items_product_idx ON order_items (product_id)".into());
                push("CREATE TABLE \"MixedCase\" (\"Id\" INTEGER NOT NULL PRIMARY KEY, \"Label\" VARCHAR(20))".into());
                push("CREATE TABLE \"select\" (id INTEGER NOT NULL PRIMARY KEY, \"from\" VARCHAR(10), \"order\" INTEGER)".into());
                push("CREATE VIEW active_customers AS SELECT id, name, email FROM customers WHERE email IS NOT NULL".into());
                push("CREATE TRIGGER orders_audit AFTER INSERT ON orders BEGIN INSERT INTO order_audit (order_id) VALUES (NEW.id); END".into());
            } else {
                push("CREATE TABLE customers (id INTEGER NOT NULL PRIMARY KEY, name VARCHAR(100) NOT NULL)".into());
                push("CREATE TABLE products (id INTEGER NOT NULL PRIMARY KEY, name VARCHAR(100) NOT NULL, price DECIMAL(8,2))".into());
                push("CREATE TABLE orders (id INTEGER NOT NULL PRIMARY KEY, customer_id INTEGER NOT NULL, total DECIMAL(12,2), CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES customers (id))".into());
                push("CREATE TABLE order_lines (id INTEGER NOT NULL PRIMARY KEY, order_id INTEGER NOT NULL, CONSTRAINT order_lines_order_fk FOREIGN KEY (order_id) REFERENCES orders (id))".into());
                push(
                    "CREATE TABLE legacy_notes (id INTEGER NOT NULL PRIMARY KEY, note TEXT)".into(),
                );
                push("CREATE TABLE \"select\" (id INTEGER NOT NULL PRIMARY KEY, \"from\" VARCHAR(10))".into());
                push("CREATE VIEW active_customers AS SELECT id, name FROM customers".into());
            }
        }
    }
    out
}

/// Drops a scratch container (best effort).
fn drop_container(engine: Engine, c: &str) -> Vec<String> {
    match engine {
        Engine::Postgres => vec![format!("DROP SCHEMA IF EXISTS {c} CASCADE")],
        Engine::MySql | Engine::MariaDb => vec![format!("DROP DATABASE IF EXISTS `{c}`")],
        Engine::MsSql => vec![
            format!(
                "DECLARE @sql nvarchar(max) = N''; \
                 SELECT @sql += N'ALTER TABLE ' + QUOTENAME(s.name) + N'.' + QUOTENAME(t.name) + N' DROP CONSTRAINT ' + QUOTENAME(fk.name) + N'; ' \
                 FROM sys.foreign_keys fk JOIN sys.tables t ON fk.parent_object_id = t.object_id \
                 JOIN sys.schemas s ON t.schema_id = s.schema_id WHERE s.name = N'{c}'; \
                 EXEC sp_executesql @sql"
            ),
            format!(
                "DECLARE @sql nvarchar(max) = N''; \
                 SELECT @sql += N'DROP ' + CASE o.type WHEN 'V' THEN N'VIEW' WHEN 'U' THEN N'TABLE' \
                 WHEN 'SO' THEN N'SEQUENCE' WHEN 'P' THEN N'PROCEDURE' ELSE N'FUNCTION' END \
                 + N' ' + QUOTENAME(s.name) + N'.' + QUOTENAME(o.name) + N'; ' \
                 FROM sys.objects o JOIN sys.schemas s ON o.schema_id = s.schema_id \
                 WHERE s.name = N'{c}' AND o.type IN ('V', 'P', 'FN', 'IF', 'TF'); \
                 EXEC sp_executesql @sql"
            ),
            format!(
                "DECLARE @sql nvarchar(max) = N''; \
                 SELECT @sql += N'DROP ' + CASE o.type WHEN 'U' THEN N'TABLE' ELSE N'SEQUENCE' END \
                 + N' ' + QUOTENAME(s.name) + N'.' + QUOTENAME(o.name) + N'; ' \
                 FROM sys.objects o JOIN sys.schemas s ON o.schema_id = s.schema_id \
                 WHERE s.name = N'{c}' AND o.type IN ('U', 'SO'); \
                 EXEC sp_executesql @sql"
            ),
            format!("DROP SCHEMA IF EXISTS [{c}]"),
        ],
        _ => Vec::new(),
    }
}

/// Status of every designed difference of the scratch pair.
fn expected(engine: Engine) -> Vec<(String, DiffStatus)> {
    let mut out: Vec<(String, DiffStatus)> = [
        ("table:customers", DiffStatus::Different),
        ("table:products", DiffStatus::Different),
        ("table:orders", DiffStatus::Different),
        ("table:select", DiffStatus::Different),
        ("table:order_audit", DiffStatus::OnlyInSource),
        ("table:order_items", DiffStatus::OnlyInSource),
        ("table:MixedCase", DiffStatus::OnlyInSource),
        ("table:order_lines", DiffStatus::OnlyInTarget),
        ("table:legacy_notes", DiffStatus::OnlyInTarget),
        ("view:active_customers", DiffStatus::Different),
        ("trigger:orders.orders_audit", DiffStatus::OnlyInSource),
    ]
    .into_iter()
    .map(|(id, s)| (id.to_owned(), s))
    .collect();
    let mut add = |id: &str, s: DiffStatus| out.push((id.to_owned(), s));
    match engine {
        Engine::Postgres => {
            add("sequence:invoice_seq", DiffStatus::OnlyInSource);
            add("sequence:ticket_seq", DiffStatus::Different);
            add(
                "function:order_total(p_id integer)",
                DiffStatus::OnlyInSource,
            );
            add("function:add_one(n integer)", DiffStatus::Different);
            add("function:audit_order()", DiffStatus::Same);
        }
        Engine::MySql | Engine::MariaDb | Engine::MsSql => {
            add("function:order_total", DiffStatus::OnlyInSource);
            add("procedure:touch_order", DiffStatus::Different);
            if engine != Engine::MySql {
                add("sequence:invoice_seq", DiffStatus::OnlyInSource);
            }
        }
        _ => {}
    }
    out
}

fn check_statuses(key: &str, result: &StructureCompareResult, expected: &[(String, DiffStatus)]) {
    for (id, status) in expected {
        let item = result
            .diff
            .item(id)
            .unwrap_or_else(|| panic!("{key}: no item {id}: {:#?}", result.diff.items));
        assert_eq!(item.status, *status, "{key}: {id}: {:?}", item.properties);
    }
}

/// The scratch pair cycle of one engine (module docs).
async fn scratch_cycle(key: &'static str, engine: Engine) {
    let live = Live::new(key, engine);
    let token = token();
    let names = [
        format!("m8_t_{token}_s"),
        format!("m8_t_{token}_t"),
        format!("m8_t_{token}_s2"),
        format!("m8_t_{token}_t2"),
    ];
    let sessions = [
        live.open("source.sqlite").await,
        live.open("target.sqlite").await,
        live.open("source2.sqlite").await,
        live.open("target2.sqlite").await,
    ];
    for (i, name) in names.iter().enumerate() {
        live.run_quietly(&sessions[i], &drop_container(engine, name))
            .await;
    }
    let steps = std::panic::AssertUnwindSafe(scratch_steps(&live, &names, &sessions));
    let result = futures::FutureExt::catch_unwind(steps).await;
    for (i, name) in names.iter().enumerate() {
        live.run_quietly(&sessions[i], &drop_container(engine, name))
            .await;
    }
    live.manager.close_all().await.unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    report(&format!(
        "{key}: scratch pairs compared, applied and in sync"
    ));
}

async fn scratch_steps(live: &Live, names: &[String; 4], sessions: &[SessionHandle; 4]) {
    let engine = live.engine;
    let key = live.key;
    for (i, name) in names.iter().enumerate() {
        live.run(&sessions[i], &seed(engine, name, i % 2 == 0))
            .await;
    }
    let source = live.container(&names[0]);
    let target = live.container(&names[1]);

    // Compare with drops.
    let first = live
        .compare((&sessions[0], &source), (&sessions[1], &target), true)
        .await;
    check_statuses(key, &first, &expected(engine));
    report(&format!("{key}: {:?}", first.counts));

    // A destructive apply needs confirmation.
    let refused = live
        .sync
        .apply(
            &StructureApplyRequest {
                compare_id: first.compare_id,
                selection: Vec::new(),
                confirm_destructive: false,
            },
            CancellationToken::new(),
        )
        .await;
    let message = refused.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(message.contains("confirm"), "{key}: {message}");

    // The target changed after the compare: refused.
    let note = match engine {
        Engine::Postgres => format!(
            "ALTER TABLE {}.legacy_notes ADD COLUMN extra integer",
            names[1]
        ),
        Engine::MySql | Engine::MariaDb => {
            format!(
                "ALTER TABLE `{}`.legacy_notes ADD COLUMN extra INT NULL",
                names[1]
            )
        }
        Engine::MsSql => format!("ALTER TABLE [{}].legacy_notes ADD extra int NULL", names[1]),
        _ => "ALTER TABLE legacy_notes ADD COLUMN extra INTEGER".to_owned(),
    };
    live.run(&sessions[1], &[note]).await;
    let stale = live
        .sync
        .apply(
            &StructureApplyRequest {
                compare_id: first.compare_id,
                selection: Vec::new(),
                confirm_destructive: true,
            },
            CancellationToken::new(),
        )
        .await;
    let message = stale.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(
        message.contains("changed since the compare"),
        "{key}: a stale apply ran: {message}"
    );
    assert!(live.sync.forget(first.compare_id));
    assert!(!live.sync.forget(first.compare_id));

    // Every item with drops: the script runs and the re-compare is in sync.
    let second = live
        .compare((&sessions[0], &source), (&sessions[1], &target), true)
        .await;
    let recompare = live.apply_all(&second, "every item with drops").await;
    assert!(recompare.diff.is_in_sync(true), "{key}");

    // Without drops: legacy_notes and order_lines stay, and only they are reported.
    let source = live.container(&names[2]);
    let target = live.container(&names[3]);
    let third = live
        .compare((&sessions[2], &source), (&sessions[3], &target), false)
        .await;
    let recompare = live.apply_all(&third, "without drops").await;
    let left: Vec<&str> = recompare
        .diff
        .items
        .iter()
        .filter(|i| i.status != DiffStatus::Same)
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(left, ["table:legacy_notes", "table:order_lines"], "{key}");
    assert!(recompare.diff.is_in_sync(false), "{key}");
}

#[tokio::test]
#[ignore = "needs the Docker fixtures"]
async fn postgres_scratch_pairs() {
    scratch_cycle("postgres", Engine::Postgres).await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures"]
async fn mysql_scratch_pairs() {
    scratch_cycle("mysql", Engine::MySql).await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures"]
async fn mariadb_scratch_pairs() {
    scratch_cycle("mariadb", Engine::MariaDb).await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures"]
async fn mssql_scratch_pairs() {
    scratch_cycle("mssql", Engine::MsSql).await;
}

#[tokio::test]
#[ignore = "runs against SQLite files (with the live tests)"]
async fn sqlite_scratch_pairs() {
    scratch_cycle("sqlite", Engine::Sqlite).await;
}

// --- the [m8] fixture pair --------------------------------------------------------------------

/// The `[m8]` item ids this engine must report.
fn fixture_items(live: &Live) -> Vec<String> {
    let m8 = table(&live.fx, "m8");
    let engine = table(m8, live.key);
    let skip = strings(engine, "skip_items");
    let signature = engine
        .get("function_signature")
        .and_then(toml::Value::as_str);
    strings(m8, "structure_items")
        .into_iter()
        .filter(|id| !skip.contains(id))
        .map(|id| match signature {
            Some(sig) if id.starts_with("function:") => format!("{id}({sig})"),
            _ => id,
        })
        .collect()
}

/// Statements that run a SQLite seed script file (one per statement; `END;` closes a trigger).
fn sqlite_script(path: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut out = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") || trimmed.is_empty() {
            continue;
        }
        current.push_str(line);
        current.push('\n');
        let in_trigger = current.to_ascii_uppercase().contains("CREATE TRIGGER");
        let ends = if in_trigger {
            trimmed.eq_ignore_ascii_case("END;")
        } else {
            trimmed.ends_with(';')
        };
        if ends {
            out.push(current.trim().trim_end_matches(';').to_owned());
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_owned());
    }
    Some(out)
}

async fn fixture_cycle(key: &'static str, engine: Engine) {
    let live = Live::new(key, engine);
    let m8 = table(&live.fx, "m8");
    let names = table(m8, key);
    let sessions = [
        live.open("m8_src.sqlite").await,
        live.open("m8_tgt.sqlite").await,
    ];
    let (src, tgt) = if engine == Engine::Sqlite {
        let source = sqlite_script(&root().join(string(names, "source_sql")));
        let target = sqlite_script(&root().join(string(names, "target_sql")));
        let (Some(source), Some(target)) = (source, target) else {
            report(&format!(
                "{key}: [m8] fixture pair PENDING (no SQLite seed scripts)"
            ));
            assert!(!require_m8(), "{key}: [m8] SQLite seeds missing");
            live.manager.close_all().await.unwrap();
            return;
        };
        live.run(&sessions[0], &source).await;
        live.run(&sessions[1], &target).await;
        (live.container("main"), live.container("main"))
    } else {
        let (s, t) = match engine {
            Engine::Postgres | Engine::MsSql => (
                string(names, "source_schema"),
                string(names, "target_schema"),
            ),
            _ => (
                string(names, "source_database"),
                string(names, "target_database"),
            ),
        };
        (live.container(&s), live.container(&t))
    };
    let compared = match live
        .try_compare((&sessions[0], &src), (&sessions[1], &tgt), true)
        .await
    {
        Ok(compared) if !compared.diff.items.is_empty() => compared,
        other => {
            let why = other.err().map(|e| e.to_string()).unwrap_or_default();
            report(&format!(
                "{key}: [m8] fixture pair PENDING (no m8_src / m8_tgt seed) {why}"
            ));
            assert!(!require_m8(), "{key}: [m8] seeds missing: {why}");
            live.manager.close_all().await.unwrap();
            return;
        }
    };
    let items = fixture_items(&live);
    for id in &items {
        let item = compared
            .diff
            .item(id)
            .unwrap_or_else(|| panic!("{key}: [m8] item {id} not reported"));
        assert_ne!(item.status, DiffStatus::Same, "{key}: {id}");
    }
    for id in ["table:legacy_notes", "table:order_lines"] {
        assert_eq!(
            compared.diff.item(id).map(|i| i.status),
            Some(DiffStatus::OnlyInTarget),
            "{key}: {id}"
        );
    }
    if engine == Engine::Sqlite {
        // Temp files: apply in place.
        let recompare = live.apply_all(&compared, "[m8] with drops").await;
        assert!(recompare.diff.is_in_sync(true), "{key}");
    } else {
        // Copy both sides into scratch containers, then sync the copies.
        let token = token();
        let copies = [format!("m8_t_{token}_fs"), format!("m8_t_{token}_ft")];
        for copy in &copies {
            live.run_quietly(&sessions[0], &drop_container(engine, copy))
                .await;
        }
        let steps = std::panic::AssertUnwindSafe(async {
            for copy in &copies {
                let create = match engine {
                    Engine::Postgres => format!("CREATE SCHEMA {copy}"),
                    Engine::MsSql => format!("CREATE SCHEMA [{copy}]"),
                    _ => format!("CREATE DATABASE `{copy}`"),
                };
                live.run(&sessions[0], &[create]).await;
            }
            let fs = live.container(&copies[0]);
            let ft = live.container(&copies[1]);
            let copy_src = live
                .compare((&sessions[0], &src), (&sessions[0], &fs), true)
                .await;
            live.apply_all(&copy_src, "copy m8_src").await;
            let copy_tgt = live
                .compare((&sessions[1], &tgt), (&sessions[1], &ft), true)
                .await;
            live.apply_all(&copy_tgt, "copy m8_tgt").await;
            let pair = live
                .compare((&sessions[0], &fs), (&sessions[1], &ft), true)
                .await;
            for id in &items {
                assert!(
                    pair.diff
                        .item(id)
                        .is_some_and(|i| i.status != DiffStatus::Same),
                    "{key}: copy: {id}"
                );
            }
            let recompare = live.apply_all(&pair, "[m8] copies with drops").await;
            assert!(recompare.diff.is_in_sync(true), "{key}");
        });
        let result = futures::FutureExt::catch_unwind(steps).await;
        for copy in &copies {
            live.run_quietly(&sessions[0], &drop_container(engine, copy))
                .await;
        }
        if let Err(panic) = result {
            live.manager.close_all().await.unwrap();
            std::panic::resume_unwind(panic);
        }
    }
    live.manager.close_all().await.unwrap();
    report(&format!("{key}: [m8] fixture pair synced"));
}

#[tokio::test]
#[ignore = "needs the Docker fixtures"]
async fn postgres_fixture_pair() {
    fixture_cycle("postgres", Engine::Postgres).await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures"]
async fn mysql_fixture_pair() {
    fixture_cycle("mysql", Engine::MySql).await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures"]
async fn mariadb_fixture_pair() {
    fixture_cycle("mariadb", Engine::MariaDb).await;
}

#[tokio::test]
#[ignore = "needs the Docker fixtures"]
async fn mssql_fixture_pair() {
    fixture_cycle("mssql", Engine::MsSql).await;
}

#[tokio::test]
#[ignore = "runs against SQLite files (with the live tests)"]
async fn sqlite_fixture_pair() {
    fixture_cycle("sqlite", Engine::Sqlite).await;
}
