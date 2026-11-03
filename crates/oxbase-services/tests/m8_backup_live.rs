//! Live backup → restore round trips per SQL engine (PostgreSQL, MySQL, MariaDB, SQL Server,
//! SQLite) against the Docker fixtures (`docs/m8-contracts.md` §7, §12 *backup*).
//!
//! Per engine, on scratch tables `m8_t_bk_<token>_parent` / `_child` (a foreign key between
//! them, a unique key, an index, the common types, NULLs, empty and non-ASCII text) in the
//! fixture database's default schema:
//!
//! - back up both tables (zstd), list and verify the archive;
//! - drop them and restore: the definitions compare equal (the engine's `Comparator`) and every
//!   row reads back equal;
//! - data-only restore with truncate (confirmed): the same rows, no duplicates;
//! - a restore that would drop objects without confirmation is refused before it starts; a
//!   structure restore over the equal existing tables keeps them;
//! - restore of one selected object (the child table: structure only, then data only);
//! - a cancelled backup ends `cancelled` and leaves no file.
//!
//! The `[m8]` fixture round trip (`m8_src` → `m8_restore`, every table, definitions and rows)
//! runs once the conformance stream's seeds exist; until then it reports "pending", unless
//! `OXBASE_SERVICES_REQUIRE_M8=1`.
//!
//! ```sh
//! scripts/fixtures-up.sh
//! cargo test -p oxbase-services --test m8_backup_live -- --ignored --test-threads 1 --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use oxbase_types::{Engine, Location, ObjectDef, ObjectKind, ObjectRef, Value, relocate_def};

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
    text.parse().expect("fixtures.toml parses")
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
        for t in [&self.child, &self.parent] {
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
            Engine::Sqlite => ("1", "0", "X'00FF0A'"),
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
        assert!(
            diff.is_empty(),
            "{}: {} differs after the restore: {diff:?}",
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

    fn spec(&self) -> BackupSpec {
        BackupSpec {
            container: self.container.clone(),
            objects: vec![
                ObjectRef::new(ObjectKind::Table, self.parent.clone()),
                ObjectRef::new(ObjectKind::Table, self.child.clone()),
            ],
            content: BackupContent::StructureAndData,
            compression: Compression::Zstd,
        }
    }
}

fn statuses(report: &RestoreReport) -> Vec<(String, RestoredStatus)> {
    report
        .objects
        .iter()
        .map(|o| (o.object.name.clone(), o.status))
        .collect()
}

async fn round_trip(key: &'static str, engine: Engine) {
    let live = Live::connect(key, engine).await;
    live.drop_tables().await;
    live.create_tables().await;
    let result = std::panic::AssertUnwindSafe(scratch_round_trip(&live));
    let outcome = futures::FutureExt::catch_unwind(result).await;
    live.drop_tables().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    fixture_round_trip(&live).await;
}

async fn scratch_round_trip(live: &Live) {
    let key = live.key;
    let (parent, child) = (live.parent.clone(), live.child.clone());
    let parent_def = live.def(&parent).await;
    let child_def = live.def(&child).await;
    let parent_rows = live.rows(&parent).await;
    let child_rows = live.rows(&child).await;
    assert_eq!(parent_rows.len(), 3, "{key}");

    // Back up, list, verify.
    let path = live.dir.path().join("scratch.oxbackup");
    let made = live.backup(live.spec(), &path).await;
    let names: Vec<&str> = report
        .manifest
        .objects
        .iter()
        .map(|o| o.object.name.as_str())
        .collect();
    assert_eq!(
        names,
        [parent.as_str(), child.as_str()],
        "{key}: parents first"
    );
    assert_eq!(made.manifest.total_rows(), 6, "{key}");
    assert_eq!(made.manifest.engine, live.engine);
    assert_eq!(made.bytes, std::fs::metadata(&path).unwrap().len());
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
        "{key}: backup of 2 tables, {} bytes, verified",
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
        [
            (parent.clone(), RestoredStatus::Created),
            (child.clone(), RestoredStatus::Created)
        ],
        "{key}"
    );
    assert_eq!(restored.rows, 6, "{key}");
    live.assert_same_def(&parent_def, &live.def(&parent).await);
    live.assert_same_def(&child_def, &live.def(&child).await);
    assert_eq!(live.rows(&parent).await, parent_rows, "{key}: parent rows");
    assert_eq!(live.rows(&child).await, child_rows, "{key}: child rows");
    report(&format!("{key}: restore into an empty container: in sync"));

    // Data only, emptying the tables first (confirmed): no duplicates.
    let data_only = RestoreOptions {
        content: RestoreContent::DataOnly,
        truncate_before_load: true,
        confirm_destructive: true,
        ..RestoreOptions::into_container(live.container.clone())
    };
    let restored = live.restore(data_only, &path).await;
    assert_eq!(
        statuses(&restored),
        [
            (parent.clone(), RestoredStatus::Loaded),
            (child.clone(), RestoredStatus::Loaded)
        ],
        "{key}"
    );
    assert_eq!(live.rows(&parent).await, parent_rows, "{key}: parent rows");
    assert_eq!(live.rows(&child).await, child_rows, "{key}: child rows");

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
        .err()
        .expect("an unconfirmed destructive restore is refused");
    assert!(refused.to_string().contains("confirm"), "{key}: {refused}");

    // Structure over the equal existing tables: kept.
    let structure = RestoreOptions {
        content: RestoreContent::StructureOnly,
        ..RestoreOptions::into_container(live.container.clone())
    };
    let restored = live.restore(structure.clone(), &path).await;
    assert_eq!(
        statuses(&restored),
        [
            (parent.clone(), RestoredStatus::Kept),
            (child.clone(), RestoredStatus::Kept)
        ],
        "{key}"
    );

    // One selected object: the child table, structure then data.
    live.exec(&format!("DROP TABLE {}", live.q(&child))).await;
    let child_index = made.manifest.objects[1].index;
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
    let leftovers: Vec<String> = std::fs::read_dir(live.dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".part"))
        .collect();
    assert_eq!(leftovers, Vec::<String>::new(), "{key}");
    assert!(!live.backups.cancel(handle.id), "{key}: the run is over");
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
    for object in &backup.manifest.objects {
        if object.kind != ObjectKind::Table {
            continue;
        }
        let mut expected = live
            .tree
            .load(live.session, &object.object, CancellationToken::new())
            .await
            .unwrap();
        relocate_def(&mut expected, &from, &to);
        let moved = target.child(ObjectKind::Table, object.object.name.clone());
        let actual = live
            .tree
            .load(live.session, &moved, CancellationToken::new())
            .await
            .unwrap();
        live.assert_same_def(&expected, &actual);
        let quote = live.dialects.quoting(live.engine).unwrap();
        let count = |r: &ObjectRef| format!("SELECT COUNT(*) FROM {}", quote.qualify(r));
        assert_eq!(
            live.exec(&count(&object.object)).await,
            live.exec(&count(&moved)).await,
            "{key}: rows of {}",
            object.object.name
        );
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
