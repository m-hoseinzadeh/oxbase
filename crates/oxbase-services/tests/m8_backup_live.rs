//! Live backup → restore round trips per SQL engine (PostgreSQL, MySQL, MariaDB, SQL Server,
//! SQLite) against the Docker fixtures (`docs/m8-contracts.md` §7, §12 *backup*).
//!
//! Per engine, on scratch tables in the fixture database's default schema:
//! `m8_t_bk_<token>_parent` / `_child` (a foreign key between them, a unique key, an index, the
//! common types, NULLs, empty and non-ASCII text) and `_node` (an identity / auto-increment key,
//! a foreign key to itself with a row that points at a row loaded after it, and on SQL Server a
//! `rowversion` column):
//!
//! - back up the three tables (zstd), list and verify the archive;
//! - drop them and restore: the definitions compare equal (the engine's `Comparator`), every row
//!   reads back equal, and a new row gets a fresh generated key (PostgreSQL once its
//!   `BulkLoader::identity_resync` cell exists);
//! - a default restore over the tables, which hold rows, is refused before it writes;
//! - data-only restore with truncate (confirmed): the same rows, no duplicates; the
//!   self-referencing table too where the engine checks foreign keys per statement or can
//!   switch them off;
//! - a restore that would drop objects without confirmation is refused before it starts; a
//!   structure restore over the equal existing tables keeps them;
//! - restore of one selected object (the child table: structure only, then data only);
//! - a cancelled backup ends `cancelled` and leaves no file, also when cancelled while a larger
//!   table's rows stream.
//!
//! The `[m8]` fixture round trip (`m8_src` → `m8_restore`: every object compared through the
//! engine's comparator after moving the source definition like the restore does, every table's
//! rows compared value by value, sequences past the restored values) runs once the conformance
//! stream's seeds exist; until then it reports "pending", unless `OXBASE_SERVICES_REQUIRE_M8=1`.
//!
//! ```sh
//! scripts/fixtures-up.sh
//! cargo test -p oxbase-services --test m8_backup_live -- --ignored --test-threads 1 --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxbase_backup::move_definition;
use oxbase_driver::{cell_value, collect};
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_services::backup::{
    BackupContent, BackupEvent, BackupReport, BackupRequest, BackupService, BackupSpec,
    Compression, RestoreContent, RestoreEvent, RestoreOptions, RestoreReport, RestoreRequest,
    RestoredStatus,
};
use oxbase_services::tree::TreeService;
use oxbase_services::{
    CancellationToken, ConnectionManager, ConnectionProfile, DialectRegistry, ProvidedSecrets,
    SecretRef, SecretStore, SessionId, default_dialects, default_drivers,
};
use oxbase_store::{MemorySecretStore, ProfileAddress, Secret, SecretSlot, TlsMode, TlsSettings};
use oxbase_types::{Engine, Location, ObjectDef, ObjectKind, ObjectRef, Value};

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
    toml::Value::Table(text.parse::<toml::Table>().expect("fixtures.toml parses"))
}

fn s(t: &toml::Value, key: &str) -> String {
    t[key]
        .as_str()
        .unwrap_or_else(|| panic!("fixtures.toml: missing {key}"))
        .to_owned()
}

fn report(line: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr().lock(), "[m8 backup] {line}");
}

/// `true` when missing `[m8]` seeds must fail instead of being reported as pending.
fn require_m8() -> bool {
    std::env::var("OXBASE_SERVICES_REQUIRE_M8").is_ok_and(|v| v == "1")
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
    dialects: Arc<DialectRegistry>,
    tree: Arc<TreeService>,
    backups: BackupService,
    session: SessionId,
    /// The scratch tables' container.
    container: ObjectRef,
    parent: String,
    child: String,
    /// Identity key, foreign key to itself (and `rowversion` on SQL Server).
    node: String,
    /// The larger table of the mid-table cancel.
    big: String,
    dir: tempfile::TempDir,
}

impl Live {
    async fn connect(key: &'static str, engine: Engine) -> Self {
        let doc = fixtures();
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecretStore::new());
        let (profile, container) = if engine == Engine::Sqlite {
            let profile = ConnectionProfile::new(
                format!("m8 {key}"),
                engine,
                ProfileAddress::File {
                    path: dir.path().join("m8.sqlite"),
                },
            );
            (profile, ObjectRef::database("main"))
        } else {
            let e = &doc["engines"][key];
            let mut p = ConnectionProfile::new(
                format!("m8 {key}"),
                engine,
                ProfileAddress::Network {
                    host: s(e, "host"),
                    port: e["port"].as_integer().and_then(|p| u16::try_from(p).ok()),
                },
            );
            p.user = Some(s(e, "user"));
            let database = s(e, "database");
            p.database = Some(database.clone());
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
            let container = match engine {
                Engine::Postgres => ObjectRef::schema(Some(&database), "public"),
                Engine::MsSql => ObjectRef::schema(Some(&database), "dbo"),
                _ => ObjectRef::database(database),
            };
            (p, container)
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
        let tree = Arc::new(TreeService::new(
            Arc::clone(&manager),
            Arc::clone(&dialects),
        ));
        let backups = BackupService::new(
            Arc::clone(&manager),
            Arc::clone(&tree),
            Arc::clone(&dialects),
        );
        let prefix = format!("m8_t_bk_{}", token());
        Self {
            key,
            engine,
            manager,
            dialects,
            tree,
            backups,
            session: base.id(),
            container,
            parent: format!("{prefix}_parent"),
            child: format!("{prefix}_child"),
            node: format!("{prefix}_node"),
            big: format!("{prefix}_big"),
            dir,
        }
    }

    /// Runs `sql` on the base session (panics on failure).
    async fn exec(&self, sql: &str) -> Vec<Vec<Value>> {
        self.try_exec(sql)
            .await
            .unwrap_or_else(|e| panic!("{}: {sql}: {e}", self.key))
    }

    async fn try_exec(&self, sql: &str) -> oxbase_types::Result<Vec<Vec<Value>>> {
        let handle = self.manager.get(self.session).expect("base session");
        let _guard = handle.exclusive().await;
        let stream = handle
            .connection()
            .execute(sql, &[], CancellationToken::new())
            .await?;
        let collected = collect(stream).await?;
        let mut rows = Vec::new();
        for set in collected.result_sets {
            for batch in set.batches {
                for r in 0..batch.num_rows() {
                    rows.push(
                        batch
                            .columns()
                            .iter()
                            .map(|c| cell_value(c.as_ref(), r).unwrap())
                            .collect(),
                    );
                }
            }
        }
        Ok(rows)
    }

    /// `name` qualified as the engine's SQL spells it in these tests.
    fn q(&self, name: &str) -> String {
        match self.engine {
            Engine::Postgres => format!("public.{name}"),
            Engine::MsSql => format!("dbo.{name}"),
            _ => name.to_owned(),
        }
    }

    fn table_ref(&self, name: &str) -> ObjectRef {
        self.container.child(ObjectKind::Table, name)
    }

    async fn drop_tables(&self) {
        for t in [&self.child, &self.parent, &self.node, &self.big] {
            let _ = self.try_exec(&format!("DROP TABLE {}", self.q(t))).await;
        }
    }

    /// The scratch tables with their rows.
    async fn create_tables(&self) {
        let (p, c) = (self.q(&self.parent), self.q(&self.child));
        let (parent_ddl, child_ddl) = match self.engine {
            Engine::Postgres => (
                format!(
                    "CREATE TABLE {p} (id integer PRIMARY KEY, name varchar(40) NOT NULL, \
                     note text, amount numeric(12,2), born date, seen timestamp, flag boolean, \
                     data bytea)"
                ),
                format!(
                    "CREATE TABLE {c} (id integer PRIMARY KEY, parent_id integer NOT NULL \
                     REFERENCES {p} (id), qty integer DEFAULT 1, label varchar(20), \
                     UNIQUE (parent_id, label))"
                ),
            ),
            Engine::MySql | Engine::MariaDb => (
                format!(
                    "CREATE TABLE {p} (id INT PRIMARY KEY, name VARCHAR(40) NOT NULL, note TEXT, \
                     amount DECIMAL(12,2), born DATE, seen DATETIME(6), flag TINYINT(1), \
                     data VARBINARY(16)) ENGINE=InnoDB"
                ),
                format!(
                    "CREATE TABLE {c} (id INT PRIMARY KEY, parent_id INT NOT NULL, \
                     qty INT DEFAULT 1, label VARCHAR(20), UNIQUE KEY (parent_id, label), \
                     FOREIGN KEY (parent_id) REFERENCES {p} (id)) ENGINE=InnoDB"
                ),
            ),
            Engine::MsSql => (
                format!(
                    "CREATE TABLE {p} (id INT PRIMARY KEY, name NVARCHAR(40) NOT NULL, \
                     note NVARCHAR(MAX), amount DECIMAL(12,2), born DATE, seen DATETIME2(6), \
                     flag BIT, data VARBINARY(16))"
                ),
                format!(
                    "CREATE TABLE {c} (id INT PRIMARY KEY, parent_id INT NOT NULL \
                     REFERENCES {p} (id), qty INT DEFAULT 1, label NVARCHAR(20), \
                     UNIQUE (parent_id, label))"
                ),
            ),
            _ => (
                format!(
                    "CREATE TABLE {p} (id INTEGER PRIMARY KEY, name TEXT NOT NULL, note TEXT, \
                     amount NUMERIC, born TEXT, seen TEXT, flag INTEGER, data BLOB)"
                ),
                format!(
                    "CREATE TABLE {c} (id INTEGER PRIMARY KEY, parent_id INTEGER NOT NULL \
                     REFERENCES {p} (id), qty INTEGER DEFAULT 1, label TEXT, \
                     UNIQUE (parent_id, label))"
                ),
            ),
        };
        self.exec(&parent_ddl).await;
        self.exec(&child_ddl).await;
        self.exec(&format!("CREATE INDEX {}_qty ON {c} (qty)", self.child))
            .await;
        let (yes, no, bytes) = match self.engine {
            Engine::Postgres => ("true", "false", "'\\x00ff0a'::bytea"),
            Engine::MsSql => ("1", "0", "0x00FF0A"),
            _ => ("1", "0", "X'00FF0A'"),
        };
        let text = |t: &str| {
            if self.engine == Engine::MsSql {
                format!("N'{t}'")
            } else {
                format!("'{t}'")
            }
        };
        self.exec(&format!(
            "INSERT INTO {p} (id, name, note, amount, born, seen, flag, data) VALUES \
             (1, {}, {}, 12.50, '2026-03-01', '2026-03-01 10:00:00.123456', {yes}, {bytes}), \
             (2, {}, NULL, NULL, NULL, NULL, {no}, NULL), \
             (3, {}, {}, -0.01, '1970-01-01', '1999-12-31 23:59:59', NULL, NULL)",
            text("Ada"),
            text("first \u{e9}t\u{e9}"),
            text("Bob"),
            text("\u{dc}n\u{ef}code"),
            text(""),
        ))
        .await;
        self.exec(&format!(
            "INSERT INTO {c} (id, parent_id, qty, label) VALUES (10, 1, 5, 'a'), \
             (11, 1, 2, 'b'), (12, 2, 7, NULL)"
        ))
        .await;
    }

    /// The self-referencing table with an identity key; rows `(2, NULL)`, `(1 → 2)`, `(3 → 1)`:
    /// in key order, row 1 points at a row loaded after it.
    async fn create_node(&self) {
        let n = self.q(&self.node);
        let ddl = match self.engine {
            Engine::Postgres => format!(
                "CREATE TABLE {n} (id serial PRIMARY KEY, parent_id integer REFERENCES {n} (id), \
                 name varchar(20) NOT NULL)"
            ),
            Engine::MySql | Engine::MariaDb => format!(
                "CREATE TABLE {n} (id INT AUTO_INCREMENT PRIMARY KEY, parent_id INT, \
                 name VARCHAR(20) NOT NULL, FOREIGN KEY (parent_id) REFERENCES {n} (id)) \
                 ENGINE=InnoDB"
            ),
            Engine::MsSql => format!(
                "CREATE TABLE {n} (id INT IDENTITY(1,1) PRIMARY KEY, parent_id INT REFERENCES \
                 {n} (id), name NVARCHAR(20) NOT NULL, rv rowversion)"
            ),
            _ => format!(
                "CREATE TABLE {n} (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES {n} (id), \
                 name TEXT NOT NULL)"
            ),
        };
        self.exec(&ddl).await;
        let inserts = [(2, "NULL", "b"), (1, "2", "a"), (3, "1", "c")]
            .iter()
            .map(|(id, parent, name)| {
                format!("INSERT INTO {n} (id, parent_id, name) VALUES ({id}, {parent}, '{name}')")
            })
            .collect::<Vec<_>>();
        if self.engine == Engine::MsSql {
            self.exec(&format!(
                "SET IDENTITY_INSERT {n} ON; {}; SET IDENTITY_INSERT {n} OFF",
                inserts.join("; ")
            ))
            .await;
        } else {
            for sql in inserts {
                self.exec(&sql).await;
            }
        }
    }

    /// The node table's stored values (not the server-maintained `rowversion`).
    async fn node_rows(&self) -> Vec<Vec<Value>> {
        self.exec(&format!(
            "SELECT id, parent_id, name FROM {} ORDER BY id",
            self.q(&self.node)
        ))
        .await
    }

    async fn rows(&self, table: &str) -> Vec<Vec<Value>> {
        self.exec(&format!("SELECT * FROM {} ORDER BY id", self.q(table)))
            .await
    }

    async fn def(&self, table: &str) -> ObjectDef {
        self.tree
            .load(
                self.session,
                &self.table_ref(table),
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|e| panic!("{}: load {table}: {e}", self.key))
    }

    /// Asserts that `table` now matches `before` (definition through the engine's comparator).
    fn assert_same_def(&self, before: &ObjectDef, after: &ObjectDef) {
        let comparator = self
            .dialects
            .comparator(self.engine, ObjectKind::Table)
            .unwrap();
        let diff = comparator.diff(before, after);
        assert_eq!(
            diff,
            [],
            "{}: {} differs after the restore",
            self.key,
            before.name()
        );
    }

    async fn backup(&self, spec: BackupSpec, path: &Path) -> BackupReport {
        let mut handle = self
            .backups
            .start_backup(
                BackupRequest {
                    session: self.session,
                    spec,
                },
                path,
                false,
            )
            .await
            .unwrap_or_else(|e| panic!("{}: backup: {e}", self.key));
        let mut started = false;
        while let Some(event) = handle.events.recv().await {
            match event {
                BackupEvent::Started { .. } => started = true,
                BackupEvent::Progress { .. } => {}
                BackupEvent::Finished { report } => {
                    assert!(started, "{}: no started event", self.key);
                    return *report;
                }
                other => panic!("{}: backup ended {other:?}", self.key),
            }
        }
        panic!("{}: the backup sent no terminal event", self.key)
    }

    async fn restore(&self, options: RestoreOptions, path: &Path) -> RestoreReport {
        let mut handle = self
            .backups
            .start_restore(
                RestoreRequest {
                    session: self.session,
                    options,
                },
                path,
            )
            .await
            .unwrap_or_else(|e| panic!("{}: restore: {e}", self.key));
        while let Some(event) = handle.events.recv().await {
            match event {
                RestoreEvent::Started { .. } | RestoreEvent::Progress { .. } => {}
                RestoreEvent::Finished { report } => return report,
                other => panic!("{}: restore ended {other:?}", self.key),
            }
        }
        panic!("{}: the restore sent no terminal event", self.key)
    }

    /// Starts a restore that must fail; returns its error message.
    async fn restore_fails(&self, options: RestoreOptions, path: &Path) -> String {
        let mut handle = self
            .backups
            .start_restore(
                RestoreRequest {
                    session: self.session,
                    options,
                },
                path,
            )
            .await
            .unwrap_or_else(|e| panic!("{}: restore: {e}", self.key));
        while let Some(event) = handle.events.recv().await {
            match event {
                RestoreEvent::Started { .. } | RestoreEvent::Progress { .. } => {}
                RestoreEvent::Failed { error, report } => {
                    assert!(
                        report.left_behind.starts_with("nothing was written"),
                        "{}: {report:?}",
                        self.key
                    );
                    return error.message;
                }
                other => panic!("{}: the restore should fail, it ended {other:?}", self.key),
            }
        }
        panic!("{}: the restore sent no terminal event", self.key)
    }

    fn spec(&self) -> BackupSpec {
        BackupSpec {
            container: self.container.clone(),
            objects: vec![
                ObjectRef::new(ObjectKind::Table, self.parent.clone()),
                ObjectRef::new(ObjectKind::Table, self.child.clone()),
                ObjectRef::new(ObjectKind::Table, self.node.clone()),
            ],
            content: BackupContent::StructureAndData,
            compression: Compression::Zstd,
        }
    }
}

/// An integer cell (integers, decimals or text by engine).
fn integer(value: &Value) -> Option<i128> {
    match value {
        Value::Int(i) => Some(i128::from(*i)),
        Value::UInt(u) => Some(i128::from(*u)),
        Value::Decimal(t) | Value::Text(t) => t.trim().split('.').next()?.parse().ok(),
        _ => None,
    }
}

fn statuses(report: &RestoreReport) -> Vec<(String, RestoredStatus)> {
    let mut out: Vec<(String, RestoredStatus)> = report
        .objects
        .iter()
        .map(|o| (o.object.name.clone(), o.status))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// `(name, status)` for each of `names`, sorted like [`statuses`].
fn all(names: &[&String], status: RestoredStatus) -> Vec<(String, RestoredStatus)> {
    let mut out: Vec<(String, RestoredStatus)> =
        names.iter().map(|n| ((*n).clone(), status)).collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The manifest index of table `name`.
fn index_of(report: &BackupReport, name: &str) -> u32 {
    report
        .manifest
        .objects
        .iter()
        .find(|o| o.object.name == name)
        .unwrap_or_else(|| panic!("{name} is not in the backup"))
        .index
}

async fn round_trip(key: &'static str, engine: Engine) {
    let live = Live::connect(key, engine).await;
    live.drop_tables().await;
    live.create_tables().await;
    live.create_node().await;
    let result = std::panic::AssertUnwindSafe(async {
        scratch_round_trip(&live).await;
        cancel_mid_table(&live).await;
    });
    let outcome = futures::FutureExt::catch_unwind(result).await;
    live.drop_tables().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    fixture_round_trip(&live).await;
}

async fn scratch_round_trip(live: &Live) {
    let key = live.key;
    let (parent, child, node) = (live.parent.clone(), live.child.clone(), live.node.clone());
    let parent_def = live.def(&parent).await;
    let child_def = live.def(&child).await;
    let node_def = live.def(&node).await;
    let parent_rows = live.rows(&parent).await;
    let child_rows = live.rows(&child).await;
    let node_rows = live.node_rows().await;
    assert_eq!(parent_rows.len(), 3, "{key}");
    assert_eq!(node_rows.len(), 3, "{key}");

    // Back up, list, verify.
    let path = live.dir.path().join("scratch.oxbackup");
    let made = live.backup(live.spec(), &path).await;
    let names: Vec<&str> = made
        .manifest
        .objects
        .iter()
        .map(|o| o.object.name.as_str())
        .collect();
    let at = |n: &str| names.iter().position(|x| *x == n);
    assert!(
        at(&parent).is_some() && at(&parent) < at(&child) && at(&node).is_some(),
        "{key}: parents first: {names:?}"
    );
    assert_eq!(made.manifest.total_rows(), 9, "{key}");
    assert_eq!(made.manifest.engine, live.engine);
    assert_eq!(made.bytes, std::fs::metadata(&path).unwrap().len());
    if live.engine == Engine::MsSql {
        assert!(
            made.manifest
                .warnings
                .iter()
                .any(|w| w.contains("column rv")),
            "{key}: the rowversion column is left out with a warning: {:?}",
            made.manifest.warnings
        );
    }
    let listed = live
        .backups
        .list(&path, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(listed, made.manifest, "{key}");
    let verified = live
        .backups
        .verify(&path, CancellationToken::new())
        .await
        .unwrap();
    assert!(verified.ok, "{key}: {verified:?}");
    report(&format!(
        "{key}: backup of 3 tables, {} bytes, verified",
        made.bytes
    ));

    // Drop and restore: same definitions, same rows.
    live.drop_tables().await;
    let restored = live
        .restore(
            RestoreOptions::into_container(live.container.clone()),
            &path,
        )
        .await;
    assert_eq!(
        statuses(&restored),
        all(&[&parent, &child, &node], RestoredStatus::Created),
        "{key}"
    );
    assert_eq!(restored.rows, 9, "{key}");
    live.assert_same_def(&parent_def, &live.def(&parent).await);
    live.assert_same_def(&child_def, &live.def(&child).await);
    live.assert_same_def(&node_def, &live.def(&node).await);
    assert_eq!(live.rows(&parent).await, parent_rows, "{key}: parent rows");
    assert_eq!(live.rows(&child).await, child_rows, "{key}: child rows");
    assert_eq!(live.node_rows().await, node_rows, "{key}: node rows");
    report(&format!(
        "{key}: restore into an empty container (forward self-reference): in sync"
    ));

    // A new row gets a fresh key: the identity counter follows the restored values.
    let ObjectDef::Table(node_table) = &node_def else {
        panic!("{key}: {node} is not a table")
    };
    let id_column = node_table.column("id").cloned().unwrap();
    let resyncs = !live
        .dialects
        .bulk_loader(live.engine)
        .unwrap()
        .identity_resync(&live.table_ref(&node), &[id_column])
        .is_empty();
    let n = live.q(&node);
    if live.engine != Engine::Postgres || resyncs {
        live.exec(&format!(
            "INSERT INTO {n} (parent_id, name) VALUES (NULL, 'new')"
        ))
        .await;
        let rows = live.node_rows().await;
        assert_eq!(rows.len(), 4, "{key}");
        assert!(
            matches!(rows[3][0], Value::Int(i) if i > 3),
            "{key}: the new row's key {:?} follows the restored ones",
            rows[3][0]
        );
        live.exec(&format!("DELETE FROM {n} WHERE name = 'new'"))
            .await;
        report(&format!(
            "{key}: a new row after the restore gets a fresh key"
        ));
    } else {
        report(&format!(
            "{key}: new row after the restore PENDING (no identity_resync cell yet)"
        ));
    }

    // The tables hold rows: a default restore is refused before it writes.
    let refused = live
        .restore_fails(
            RestoreOptions::into_container(live.container.clone()),
            &path,
        )
        .await;
    assert!(refused.contains("already hold rows"), "{key}: {refused}");
    assert_eq!(live.rows(&parent).await, parent_rows, "{key}: untouched");

    // Data only, emptying the tables first (confirmed): no duplicates.
    let data_only = RestoreOptions {
        content: RestoreContent::DataOnly,
        objects: vec![index_of(&made, &parent), index_of(&made, &child)],
        truncate_before_load: true,
        confirm_destructive: true,
        ..RestoreOptions::into_container(live.container.clone())
    };
    let restored = live.restore(data_only.clone(), &path).await;
    assert_eq!(
        statuses(&restored),
        all(&[&parent, &child], RestoredStatus::Loaded),
        "{key}"
    );
    assert_eq!(live.rows(&parent).await, parent_rows, "{key}: parent rows");
    assert_eq!(live.rows(&child).await, child_rows, "{key}: child rows");

    // The self-referencing table, data only: engines that check foreign keys per statement, or
    // whose loader can switch the checks off for the load.
    let per_row_checks = matches!(live.engine, Engine::MySql | Engine::MariaDb);
    let can_relax = live
        .dialects
        .bulk_loader(live.engine)
        .unwrap()
        .foreign_key_checks(&live.table_ref(&node), false)
        .is_some();
    if !per_row_checks || can_relax {
        let restored = live
            .restore(
                RestoreOptions {
                    objects: vec![index_of(&made, &node)],
                    ..data_only
                },
                &path,
            )
            .await;
        assert_eq!(restored.rows, 3, "{key}");
        assert_eq!(live.node_rows().await, node_rows, "{key}: node rows");
        report(&format!(
            "{key}: data-only restore of the self-reference: in sync"
        ));
    } else {
        report(&format!(
            "{key}: data-only restore of the self-reference PENDING (no foreign_key_checks cell \
             yet)"
        ));
    }

    // Destructive without confirmation: refused before it starts.
    let unconfirmed = RestoreOptions {
        drop_existing: true,
        ..RestoreOptions::into_container(live.container.clone())
    };
    let refused = live
        .backups
        .start_restore(
            RestoreRequest {
                session: live.session,
                options: unconfirmed,
            },
            &path,
        )
        .await
        .expect_err("an unconfirmed destructive restore is refused");
    assert!(refused.to_string().contains("confirm"), "{key}: {refused}");

    // Structure over the equal existing tables: kept.
    let structure = RestoreOptions {
        content: RestoreContent::StructureOnly,
        ..RestoreOptions::into_container(live.container.clone())
    };
    let restored = live.restore(structure.clone(), &path).await;
    assert_eq!(
        statuses(&restored),
        all(&[&parent, &child, &node], RestoredStatus::Kept),
        "{key}"
    );

    // One selected object: the child table, structure then data.
    live.exec(&format!("DROP TABLE {}", live.q(&child))).await;
    let child_index = index_of(&made, &child);
    let restored = live
        .restore(
            RestoreOptions {
                objects: vec![child_index],
                ..structure
            },
            &path,
        )
        .await;
    assert_eq!(
        statuses(&restored),
        [(child.clone(), RestoredStatus::Created)],
        "{key}"
    );
    assert_eq!(live.rows(&child).await, Vec::<Vec<Value>>::new(), "{key}");
    live.assert_same_def(&child_def, &live.def(&child).await);
    let restored = live
        .restore(
            RestoreOptions {
                objects: vec![child_index],
                content: RestoreContent::DataOnly,
                ..RestoreOptions::into_container(live.container.clone())
            },
            &path,
        )
        .await;
    assert_eq!(restored.rows, 3, "{key}");
    assert_eq!(live.rows(&child).await, child_rows, "{key}: child rows");
    assert_eq!(
        live.rows(&parent).await,
        parent_rows,
        "{key}: parent untouched"
    );
    report(&format!(
        "{key}: data-only, kept and selected restores: in sync"
    ));

    // A cancelled backup leaves no file.
    let cancelled = live.dir.path().join("cancelled.oxbackup");
    let mut handle = live
        .backups
        .start_backup(
            BackupRequest {
                session: live.session,
                spec: live.spec(),
            },
            &cancelled,
            false,
        )
        .await
        .unwrap();
    let asked = Instant::now();
    assert!(live.backups.cancel(handle.id), "{key}");
    let mut last = None;
    while let Some(event) = handle.events.recv().await {
        if event.is_terminal() {
            last = Some(event);
        }
    }
    assert!(
        matches!(last, Some(BackupEvent::Cancelled { .. })),
        "{key}: {last:?}"
    );
    assert!(asked.elapsed() < Duration::from_secs(2), "{key}");
    assert!(!cancelled.exists(), "{key}");
    assert_eq!(leftovers(live), Vec::<String>::new(), "{key}");
    assert!(!live.backups.cancel(handle.id), "{key}: the run is over");
}

/// Temporary files of the backups in the test directory.
fn leftovers(live: &Live) -> Vec<String> {
    std::fs::read_dir(live.dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".oxbase-"))
        .collect()
}

/// A backup cancelled while a larger table's rows stream ends `cancelled` within 2 s and
/// leaves no file. A server fast enough to finish first is reported, not failed.
async fn cancel_mid_table(live: &Live) {
    let key = live.key;
    let b = live.q(&live.big);
    let text = if live.engine == Engine::MsSql {
        "NVARCHAR(200)"
    } else if live.engine == Engine::Sqlite {
        "TEXT"
    } else {
        "VARCHAR(200)"
    };
    live.exec(&format!(
        "CREATE TABLE {b} (id INTEGER PRIMARY KEY, pad {text} NOT NULL)"
    ))
    .await;
    live.exec(&format!(
        "INSERT INTO {b} (id, pad) VALUES (1, '{}')",
        "x".repeat(40)
    ))
    .await;
    // Doubling: 2^20 rows, enough that streaming them outlasts the first rows progress event
    // (250 ms) on the fixtures.
    let mut rows = 1_u64;
    for _ in 0..20 {
        live.exec(&format!(
            "INSERT INTO {b} (id, pad) SELECT id + {rows}, pad FROM {b}"
        ))
        .await;
        rows *= 2;
    }
    let path = live.dir.path().join("big.oxbackup");
    let mut handle = live
        .backups
        .start_backup(
            BackupRequest {
                session: live.session,
                spec: BackupSpec {
                    container: live.container.clone(),
                    objects: vec![ObjectRef::new(ObjectKind::Table, live.big.clone())],
                    content: BackupContent::StructureAndData,
                    compression: Compression::Zstd,
                },
            },
            &path,
            false,
        )
        .await
        .unwrap();
    let mut asked: Option<Instant> = None;
    let mut last = None;
    while let Some(event) = handle.events.recv().await {
        if let BackupEvent::Progress { progress } = &event
            && progress.rows > 0
            && asked.is_none()
        {
            asked = Some(Instant::now());
            assert!(live.backups.cancel(handle.id), "{key}");
        }
        if event.is_terminal() {
            last = Some(event);
        }
    }
    match (last, asked) {
        (Some(BackupEvent::Cancelled { .. }), Some(asked)) => {
            // The cancel came while rows streamed.
            assert!(asked.elapsed() < Duration::from_secs(2), "{key}");
            assert!(!path.exists(), "{key}");
            assert_eq!(leftovers(live), Vec::<String>::new(), "{key}");
            report(&format!(
                "{key}: backup cancelled while {rows} rows streamed: no file"
            ));
        }
        (Some(BackupEvent::Finished { .. }), _) => {
            std::fs::remove_file(&path).unwrap();
            report(&format!(
                "{key}: the {rows}-row backup finished before the cancel took effect; cancel \
                 mid-table not exercised"
            ));
        }
        (other, asked) => panic!("{key}: ended {other:?} (cancel asked: {})", asked.is_some()),
    }
    let _ = live.try_exec(&format!("DROP TABLE {b}")).await;
}

/// The `[m8]` fixture: back up `m8_src`, restore it into the emptied `m8_restore`, compare
/// every table's definition (moved to `m8_restore`) and rows.
async fn fixture_round_trip(live: &Live) {
    let key = live.key;
    let doc = fixtures();
    let Some(m8) = doc.get("m8").and_then(|m| m.get(key)) else {
        return;
    };
    let (source, target) = match live.engine {
        Engine::Postgres | Engine::MsSql => {
            let database = s(m8, "database");
            (
                ObjectRef::schema(Some(&database), s(m8, "source_schema")),
                ObjectRef::schema(Some(&database), s(m8, "restore_schema")),
            )
        }
        Engine::MySql | Engine::MariaDb => (
            ObjectRef::database(s(m8, "source_database")),
            ObjectRef::database(s(m8, "restore_database")),
        ),
        // SQLite: the seed scripts build files of their own (conformance stream).
        _ => return,
    };
    let tables = live
        .dialects
        .fetcher(live.engine, ObjectKind::Table)
        .unwrap();
    let handle = live.manager.get(live.session).unwrap();
    let listed = {
        let _guard = handle.exclusive().await;
        tables
            .list(
                handle.connection().as_ref(),
                &source,
                CancellationToken::new(),
            )
            .await
            .unwrap_or_default()
    };
    if listed.is_empty() {
        report(&format!(
            "{key}: [m8] backup of {} PENDING (no seed yet)",
            source.display_name()
        ));
        assert!(!require_m8(), "{key}: the [m8] seed is missing");
        return;
    }
    let path = live.dir.path().join("m8_src.oxbackup");
    let backup = live
        .backup(
            BackupSpec {
                container: source.clone(),
                objects: Vec::new(),
                content: BackupContent::StructureAndData,
                compression: Compression::Zstd,
            },
            &path,
        )
        .await;
    let verified = live
        .backups
        .verify(&path, CancellationToken::new())
        .await
        .unwrap();
    assert!(verified.ok, "{key}: {verified:?}");
    let restored = live
        .restore(
            RestoreOptions {
                drop_existing: true,
                confirm_destructive: true,
                ..RestoreOptions::into_container(target.clone())
            },
            &path,
        )
        .await;
    assert_eq!(restored.rows, backup.manifest.total_rows(), "{key}");
    let (from, to) = (
        Location::of_container(&source),
        Location::of_container(&target),
    );
    let quote = live.dialects.quoting(live.engine).unwrap();
    // (table in the target, its definition) of every restored table: sequence users.
    let mut tables: Vec<(ObjectRef, ObjectDef)> = Vec::new();
    for object in &backup.manifest.objects {
        let mut expected = live
            .tree
            .load(live.session, &object.object, CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{key}: load {}: {e}", object.object.name));
        move_definition(live.engine, &mut expected, &from, &to);
        let moved = expected.object_ref();
        let actual = live
            .tree
            .load(live.session, &moved, CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{key}: restored {}: {e}", moved.name));
        match live.dialects.comparator(live.engine, object.kind) {
            Ok(comparator) => assert_eq!(
                comparator.diff(&expected, &actual),
                [],
                "{key}: {} {} differs after the restore",
                object.kind,
                object.object.name
            ),
            Err(_) => report(&format!(
                "{key}: no {} comparator; {} was restored but not compared",
                object.kind, object.object.name
            )),
        }
        // Every stored value, as a sorted list of rows.
        if let Some(data) = &object.data {
            let columns: Vec<String> = data.columns.iter().map(|c| quote.quote_ident(c)).collect();
            let select =
                |r: &ObjectRef| format!("SELECT {} FROM {}", columns.join(", "), quote.qualify(r));
            let sorted = |rows: Vec<Vec<Value>>| {
                let mut out: Vec<String> = rows.iter().map(|r| format!("{r:?}")).collect();
                out.sort();
                out
            };
            assert_eq!(
                sorted(live.exec(&select(&moved)).await),
                sorted(live.exec(&select(&object.object)).await),
                "{key}: rows of {}",
                object.object.name
            );
            tables.push((moved, actual));
        }
    }
    // Sequences: the next value is past the restored values of the columns that use them.
    for object in &backup.manifest.objects {
        if object.kind != ObjectKind::Sequence {
            continue;
        }
        let mut seq = object.object.clone();
        oxbase_types::relocate_ref(&mut seq, &from, &to);
        let name = quote.qualify(&seq);
        let next_sql = match live.engine {
            Engine::Postgres => format!("SELECT nextval('{}')", name.replace('\'', "''")),
            Engine::MsSql => format!("SELECT NEXT VALUE FOR {name}"),
            Engine::MariaDb => format!("SELECT NEXTVAL({name})"),
            _ => continue,
        };
        for (table, def) in &tables {
            let ObjectDef::Table(t) = def else { continue };
            for c in &t.columns {
                let uses = c
                    .default
                    .as_deref()
                    .is_some_and(|d| d.to_lowercase().contains(&seq.name.to_lowercase()));
                if !uses {
                    continue;
                }
                let max = live
                    .exec(&format!(
                        "SELECT MAX({}) FROM {}",
                        quote.quote_ident(&c.name),
                        quote.qualify(table)
                    ))
                    .await;
                let next = live.exec(&next_sql).await;
                let (Some(max), Some(next)) = (integer(&max[0][0]), integer(&next[0][0])) else {
                    continue;
                };
                assert!(
                    next > max,
                    "{key}: {} gives {next}, {}.{} holds {max}",
                    seq.name,
                    t.obj.name,
                    c.name
                );
            }
        }
    }
    report(&format!(
        "{key}: [m8] {} → {}: {} objects, {} rows, in sync",
        source.display_name(),
        target.display_name(),
        backup.manifest.objects.len(),
        backup.manifest.total_rows()
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn postgres_round_trip() {
    round_trip("postgres", Engine::Postgres).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn mysql_round_trip() {
    round_trip("mysql", Engine::MySql).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn mariadb_round_trip() {
    round_trip("mariadb", Engine::MariaDb).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn mssql_round_trip() {
    round_trip("mssql", Engine::MsSql).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn sqlite_round_trip() {
    round_trip("sqlite", Engine::Sqlite).await;
}

/// Backs up the whole PostgreSQL schema `src` and restores it into the new, empty schema `dst`.
async fn postgres_schema_restore(live: &Live, src: &str, dst: &str) {
    let database = live.container.catalog.clone();
    let schema = |name: &str| ObjectRef::schema(database.as_deref(), name);
    live.exec(&format!("CREATE SCHEMA {dst}")).await;
    let path = live.dir.path().join(format!("{src}.oxbackup"));
    live.backup(
        BackupSpec {
            container: schema(src),
            objects: Vec::new(),
            content: BackupContent::StructureAndData,
            compression: Compression::Zstd,
        },
        &path,
    )
    .await;
    live.restore(RestoreOptions::into_container(schema(dst)), &path)
        .await;
}

/// A restored `SECURITY DEFINER` function keeps its `SET search_path` pin (audit lead 13).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn postgres_restore_keeps_routine_settings() {
    let live = Live::connect("postgres", Engine::Postgres).await;
    let src = format!("oxbase_bk_{}_l13", token());
    let dst = format!("{src}_restored");
    live.exec(&format!("CREATE SCHEMA {src}")).await;
    live.exec(&format!(
        "CREATE FUNCTION {src}.f() RETURNS text LANGUAGE sql SECURITY DEFINER \
         SET search_path = pg_catalog, pg_temp AS 'SELECT lower(''A''::text)'"
    ))
    .await;
    postgres_schema_restore(&live, &src, &dst).await;
    let settings = live
        .exec(&format!(
            "SELECT pg_catalog.array_to_json(proconfig)::text FROM pg_catalog.pg_proc \
             WHERE oid = '{dst}.f()'::regprocedure"
        ))
        .await;
    for schema in [&src, &dst] {
        live.exec(&format!("DROP SCHEMA {schema} CASCADE")).await;
    }
    assert_eq!(
        settings,
        vec![vec![Value::Text(
            r#"["search_path=pg_catalog, pg_temp"]"#.to_owned()
        )]]
    );
}

/// A restored table keeps row-level security (`ENABLE`, `FORCE`) and its policies (switched on
/// after its rows, which all load), and a view its `security_barrier` / `security_invoker`
/// options (audit lead 11).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh)"]
async fn postgres_restore_keeps_row_security() {
    let live = Live::connect("postgres", Engine::Postgres).await;
    let src = format!("oxbase_bk_{}_l11", token());
    let dst = format!("{src}_restored");
    for sql in [
        format!("CREATE SCHEMA {src}"),
        format!("CREATE TABLE {src}.t (id integer PRIMARY KEY, tenant text NOT NULL)"),
        format!("INSERT INTO {src}.t VALUES (1, 'a'), (2, 'b')"),
        format!("ALTER TABLE {src}.t ENABLE ROW LEVEL SECURITY"),
        format!("ALTER TABLE {src}.t FORCE ROW LEVEL SECURITY"),
        format!("CREATE POLICY own ON {src}.t USING (tenant = current_user)"),
        format!(
            "CREATE VIEW {src}.vi WITH (security_barrier = true, security_invoker = true) AS \
             SELECT id, tenant FROM {src}.t"
        ),
    ] {
        live.exec(&sql).await;
    }
    let state = |schema: &str| {
        format!(
            "SELECT c.relrowsecurity::text || ' ' || c.relforcerowsecurity::text, \
                    (SELECT pg_catalog.count(*) FROM {schema}.t)::text, \
                    (SELECT pg_catalog.string_agg(p.polname::text || ' ' || \
                            pg_catalog.pg_get_expr(p.polqual, p.polrelid), ', ') \
                       FROM pg_catalog.pg_policy p WHERE p.polrelid = c.oid), \
                    (SELECT pg_catalog.array_to_json(v.reloptions)::text \
                       FROM pg_catalog.pg_class v WHERE v.oid = '{schema}.vi'::regclass) \
             FROM pg_catalog.pg_class c WHERE c.oid = '{schema}.t'::regclass"
        )
    };
    let before = live.exec(&state(&src)).await;
    postgres_schema_restore(&live, &src, &dst).await;
    let after = live.exec(&state(&dst)).await;
    for schema in [&src, &dst] {
        live.exec(&format!("DROP SCHEMA {schema} CASCADE")).await;
    }
    assert_eq!(before[0][0], Value::Text("true true".to_owned()));
    assert_eq!(before[0][1], Value::Text("2".to_owned()));
    assert_eq!(after, before);
}
