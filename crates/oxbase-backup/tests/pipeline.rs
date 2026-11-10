//! Backup and restore end to end over the mock driver (no server): a backup of a one-table
//! container streams the mock's rows into an archive; restore verifies, plans the structure
//! script and loads the rows through the engine's `BulkLoader`; corrupt archives, unconfirmed
//! destructive restores, another engine and cancels are refused or stopped before anything is
//! written, and a cancelled backup leaves no file.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use arrow_array::RecordBatch;
use async_trait::async_trait;
use futures::FutureExt;
use oxbase_backup::archive::ArchiveReader;
use oxbase_backup::backup::{BackupJob, backup};
use oxbase_backup::format::data_entry;
use oxbase_backup::restore::{RestoreJob, ScriptRunner, restore_run};
use oxbase_backup::{
    BackupContent, BackupSpec, Compression, RestoreContent, RestoreOptions, RestoredStatus,
};
use oxbase_dialect::{
    BulkLimits, BulkLoader, BulkMethod, Comparator, DdlContext, DdlGenerator, DialectRegistry,
    Fetcher, LoadSession, LoadTarget, PgQuoting,
};
use oxbase_driver::mock::{MockConfig, MockDriver};
use oxbase_driver::{CancellationToken, ConnectParams, Connection, Driver};
use oxbase_types::{
    ColumnDef, DataType, Engine, Error, ErrorCategory, Extras, FkAction, ForeignKeyDef, ObjectDef,
    ObjectKind, ObjectRef, ObjectSummary, PrimaryKeyDef, PropertyDiff, Result, Script, ScriptStep,
    Secret, ServerVersion, TableDef,
};
use pretty_assertions::assert_eq;

const ENGINE: Engine = Engine::Postgres;

fn container() -> ObjectRef {
    ObjectRef::schema(Some("shop"), "sales")
}

fn items_def(schema: &ObjectRef) -> TableDef {
    let mut t = TableDef::new(schema.child(ObjectKind::Table, "items"));
    t.columns = vec![
        ColumnDef::new("id", DataType::INT64).not_null(),
        ColumnDef::new("name", DataType::Text).not_null(),
    ];
    t.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".into()],
    });
    t
}

/// Every schema holds one table, `items` (the mock's row shape). `self_ref` gives it a foreign
/// key to itself; `elsewhere` puts its definition in another schema (a forged archive).
#[derive(Default)]
struct Tables {
    self_ref: bool,
    elsewhere: bool,
}

#[async_trait]
impl Fetcher for Tables {
    async fn list(
        &self,
        _c: &dyn Connection,
        parent: &ObjectRef,
        _cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        Ok(vec![ObjectSummary::new(
            parent.child(ObjectKind::Table, "items"),
        )])
    }

    async fn load(
        &self,
        _c: &dyn Connection,
        obj: &ObjectRef,
        _cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        let schema = if self.elsewhere {
            ObjectRef::schema(obj.catalog.as_deref(), "prod")
        } else {
            ObjectRef::schema(obj.catalog.as_deref(), obj.schema.clone().unwrap())
        };
        let mut t = items_def(&schema);
        if self.self_ref {
            t.foreign_keys.push(ForeignKeyDef {
                name: Some("items_parent".into()),
                columns: vec!["id".into()],
                ref_table: t.obj.clone(),
                ref_columns: vec!["id".into()],
                on_delete: FkAction::NoAction,
                on_update: FkAction::NoAction,
                extras: Extras::default(),
            });
        }
        Ok(ObjectDef::Table(t))
    }
}

/// `CREATE TABLE <name>` / `DROP TABLE <name>`: enough for the mock, which accepts anything.
/// A table's create warns, as a MySQL trigger's does when its definer is not kept.
struct Ddl;

impl DdlGenerator for Ddl {
    fn create(&self, d: &ObjectDef, _ctx: &DdlContext<'_>) -> Result<Script> {
        let mut s = Script::new();
        let sql = match d {
            ObjectDef::ForeignKey { table, foreign_key } => format!(
                "ADD FOREIGN KEY {} ON {}",
                foreign_key.name.as_deref().unwrap_or("?"),
                table.display_name()
            ),
            ObjectDef::Table(t) => {
                s.warn(format!("{} gets a new definer", t.obj.display_name()));
                format!(
                    "CREATE TABLE {} ({} foreign keys)",
                    t.obj.display_name(),
                    t.foreign_keys.len()
                )
            }
            _ => format!("CREATE {}", d.object_ref().display_name()),
        };
        s.push_step(ScriptStep::new(sql).described("create"));
        Ok(s)
    }

    fn alter(&self, _old: &ObjectDef, _new: &ObjectDef, _ctx: &DdlContext<'_>) -> Result<Script> {
        Ok(Script::new())
    }

    fn drop_object(&self, r: &ObjectRef, _ctx: &DdlContext<'_>) -> Result<Script> {
        let mut s = Script::new();
        s.push_destructive(format!("DROP TABLE {}", r.display_name()));
        Ok(s)
    }
}

struct Equal;

impl Comparator for Equal {
    fn diff(&self, a: &ObjectDef, b: &ObjectDef) -> Vec<PropertyDiff> {
        match (a, b) {
            (ObjectDef::Table(x), ObjectDef::Table(y)) if x.columns == y.columns => Vec::new(),
            _ => vec![PropertyDiff::object_added(b.object_ref())],
        }
    }
}

/// Records what it loads.
#[derive(Default)]
struct Recorder {
    batches: Mutex<Vec<RecordBatch>>,
}

#[async_trait]
impl BulkLoader for Recorder {
    fn method(&self, _version: &ServerVersion) -> BulkMethod {
        BulkMethod::MultiRowInsert
    }

    fn limits(&self, _version: &ServerVersion) -> BulkLimits {
        BulkLimits {
            rows_per_call: 5,
            ..BulkLimits::for_engine(ENGINE)
        }
    }

    async fn load(
        &self,
        session: LoadSession<'_>,
        target: &LoadTarget<'_>,
        batch: &RecordBatch,
        cancel: CancellationToken,
    ) -> Result<u64> {
        target.check_batch(batch)?;
        // The mock logs the statement, so the test sees where it ran.
        drop(session.execute("INSERT rows", &[], cancel).await?);
        self.batches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(batch.clone());
        Ok(u64::try_from(batch.num_rows()).unwrap())
    }
}

fn registry(loader: Arc<Recorder>) -> DialectRegistry {
    registry_with(loader, Tables::default())
}

fn registry_with(loader: Arc<Recorder>, tables: Tables) -> DialectRegistry {
    let mut r = DialectRegistry::new();
    r.register_fetcher(ENGINE, ObjectKind::Table, Arc::new(tables))
        .register_ddl(ENGINE, ObjectKind::Table, Arc::new(Ddl))
        .register_ddl(ENGINE, ObjectKind::ForeignKey, Arc::new(Ddl))
        .register_comparator(ENGINE, ObjectKind::Table, Arc::new(Equal))
        .register_quoting(ENGINE, Arc::new(PgQuoting))
        .register_bulk_loader(ENGINE, loader);
    r
}

async fn connect(driver: &MockDriver) -> Box<dyn Connection> {
    let mut params = ConnectParams::new(ENGINE);
    params.host = Some("db.internal.example".into());
    params.user = Some("alice_secret_user".into());
    params.password = Some(Secret::new("s3cret-pw"));
    params.database = Some("shop".into());
    driver.connect(&params).await.unwrap()
}

/// Runs a script on the mock connection, step by step.
fn runner(connection: Arc<dyn Connection>) -> Box<ScriptRunner> {
    Box::new(move |script: Script| {
        let connection = Arc::clone(&connection);
        async move {
            for step in script.ordered() {
                drop(
                    connection
                        .execute(&step.sql, &[], CancellationToken::new())
                        .await?,
                );
            }
            Ok::<_, Error>(true)
        }
        .boxed()
    })
}

async fn make_backup(dir: &Path, config: MockConfig) -> (std::path::PathBuf, MockDriver) {
    make_backup_with(dir, config, Tables::default()).await
}

async fn make_backup_with(
    dir: &Path,
    config: MockConfig,
    tables: Tables,
) -> (std::path::PathBuf, MockDriver) {
    let driver = MockDriver::new(ENGINE).with_config(config);
    let connection = connect(&driver).await;
    let loader = Arc::new(Recorder::default());
    let dialects = registry_with(loader, tables);
    let spec = BackupSpec {
        container: container(),
        objects: Vec::new(),
        content: BackupContent::StructureAndData,
        compression: Compression::Zstd,
    };
    let path = dir.join("shop.oxbackup");
    let report = backup(
        BackupJob {
            dialects: &dialects,
            metadata: connection.as_ref(),
            rows: connection.as_ref(),
            spec: &spec,
        },
        &path,
        false,
        &|_| {},
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(report.manifest.total_rows(), 12);
    assert_eq!(report.manifest.objects.len(), 1);
    assert_eq!(
        report.bytes,
        std::fs::metadata(&path).unwrap().len(),
        "the report names the file's size"
    );
    (path, driver)
}

fn options() -> RestoreOptions {
    RestoreOptions::into_container(container())
}

#[tokio::test]
async fn backup_then_restore_over_the_mock() {
    let dir = tempfile::tempdir().unwrap();
    let (path, driver) = make_backup(dir.path(), MockConfig::default()).await;
    // The rows were read in one transaction, ordered by the primary key.
    let log = driver.log();
    assert!(
        log.iter()
            .any(|s| s == "SELECT \"id\", \"name\" FROM \"sales\".\"items\" ORDER BY \"id\""),
        "{log:?}"
    );
    assert!(log.contains(&"BEGIN".to_owned()), "{log:?}");

    // No connection details anywhere in the archive's text.
    let mut reader = ArchiveReader::open(&path).unwrap();
    let mut text = serde_json::to_string(reader.manifest()).unwrap();
    text.push_str(&serde_json::to_string(&reader.definition(1).unwrap()).unwrap());
    text.push_str(&reader.ddl(1).unwrap().unwrap_or_default());
    for secret in ["db.internal.example", "alice_secret_user", "s3cret-pw"] {
        assert!(!text.contains(secret), "{secret} leaked");
    }
    let verify = reader.verify(&mut |_| {}, &|| false).unwrap();
    assert!(verify.ok, "{verify:?}");

    // Restore over the existing (equal) table that holds rows: refused before any write.
    let (outcome, log) = try_restore(&path, &options(), ENGINE).await;
    let err = outcome.error.unwrap();
    assert!(err.to_string().contains("already hold rows"), "{err}");
    assert!(log.iter().all(|s| s.starts_with("SELECT")), "{log:?}");
    assert_eq!(
        log,
        ["SELECT 1 FROM \"sales\".\"items\" LIMIT 1"],
        "only the emptiness check ran"
    );

    // Restore over the existing (equal) empty table: kept, rows added in calls of 5.
    let target_driver = MockDriver::new(ENGINE).with_config(MockConfig {
        batches: 0,
        ..MockConfig::default()
    });
    let target: Arc<dyn Connection> = Arc::from(connect(&target_driver).await);
    let loader = Arc::new(Recorder::default());
    let dialects = registry(Arc::clone(&loader));
    let opts = options();
    let run = runner(Arc::clone(&target));
    let outcome = restore_run(
        RestoreJob {
            dialects: &dialects,
            target: target.as_ref(),
            options: &opts,
        },
        &path,
        run.as_ref(),
        &|_| {},
        CancellationToken::new(),
    )
    .await;
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    let report = outcome.report;
    assert_eq!(report.rows, 12);
    assert_eq!(report.objects.len(), 1);
    assert_eq!(report.objects[0].status, RestoredStatus::Loaded);
    let sizes: Vec<usize> = loader
        .batches
        .lock()
        .unwrap()
        .iter()
        .map(RecordBatch::num_rows)
        .collect();
    assert_eq!(sizes.iter().sum::<usize>(), 12);
    assert!(sizes.iter().all(|n| *n <= 5), "{sizes:?}");
    let log = target_driver.log();
    assert!(!log.iter().any(|s| s.starts_with("CREATE")), "{log:?}");
    assert_eq!(log.iter().filter(|s| *s == "COMMIT").count(), 1, "{log:?}");

    // Drop existing (confirmed): DROP, CREATE, then the rows.
    let target_driver = MockDriver::new(ENGINE);
    let target: Arc<dyn Connection> = Arc::from(connect(&target_driver).await);
    let opts = RestoreOptions {
        drop_existing: true,
        confirm_destructive: true,
        ..options()
    };
    let run = runner(Arc::clone(&target));
    let outcome = restore_run(
        RestoreJob {
            dialects: &dialects,
            target: target.as_ref(),
            options: &opts,
        },
        &path,
        run.as_ref(),
        &|_| {},
        CancellationToken::new(),
    )
    .await;
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.report.objects[0].status, RestoredStatus::Replaced);
    // The structure script's warnings are the report's.
    assert_eq!(
        outcome.report.warnings,
        ["items gets a new definer"],
        "{:?}",
        outcome.report.warnings
    );
    let log = target_driver.log();
    let drop_at = log
        .iter()
        .position(|s| s.starts_with("DROP TABLE"))
        .unwrap();
    let create_at = log
        .iter()
        .position(|s| s.starts_with("CREATE TABLE"))
        .unwrap();
    let insert_at = log.iter().position(|s| s == "INSERT rows").unwrap();
    assert!(drop_at < create_at && create_at < insert_at, "{log:?}");

    // Data only with truncate: one DELETE inside the table's transaction.
    let target_driver = MockDriver::new(ENGINE);
    let target: Arc<dyn Connection> = Arc::from(connect(&target_driver).await);
    let opts = RestoreOptions {
        content: RestoreContent::DataOnly,
        truncate_before_load: true,
        confirm_destructive: true,
        ..options()
    };
    let run = runner(Arc::clone(&target));
    let outcome = restore_run(
        RestoreJob {
            dialects: &dialects,
            target: target.as_ref(),
            options: &opts,
        },
        &path,
        run.as_ref(),
        &|_| {},
        CancellationToken::new(),
    )
    .await;
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    let log = target_driver.log();
    let begin = log.iter().position(|s| s == "BEGIN").unwrap();
    let delete = log
        .iter()
        .position(|s| s == "DELETE FROM \"sales\".\"items\"")
        .unwrap_or_else(|| panic!("{log:?}"));
    assert!(begin < delete, "{log:?}");
    assert!(!log.iter().any(|s| s.starts_with("CREATE")), "{log:?}");
}

/// Restores `path` with `opts` on a fresh mock and returns the outcome and the mock's log.
async fn try_restore(
    path: &Path,
    opts: &RestoreOptions,
    engine: Engine,
) -> (oxbase_backup::RestoreOutcome, Vec<String>) {
    let driver = MockDriver::new(engine);
    let mut params = ConnectParams::new(engine);
    params.database = Some("shop".into());
    let target: Arc<dyn Connection> = Arc::from(driver.connect(&params).await.unwrap());
    let loader = Arc::new(Recorder::default());
    let dialects = registry(loader);
    let run = runner(Arc::clone(&target));
    let outcome = restore_run(
        RestoreJob {
            dialects: &dialects,
            target: target.as_ref(),
            options: opts,
        },
        path,
        run.as_ref(),
        &|_| {},
        CancellationToken::new(),
    )
    .await;
    (outcome, driver.log())
}

#[tokio::test]
async fn nothing_is_written_before_the_archive_is_verified() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = make_backup(dir.path(), MockConfig::default()).await;
    let mut bytes = std::fs::read(&path).unwrap();
    let start = {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).unwrap();
        let entry = zip.by_name(&data_entry(1)).unwrap();
        usize::try_from(entry.data_start().unwrap()).unwrap()
    };
    bytes[start + 40] ^= 0x10;
    let corrupt = dir.path().join("corrupt.oxbackup");
    std::fs::write(&corrupt, &bytes).unwrap();

    let (outcome, log) = try_restore(&corrupt, &options(), ENGINE).await;
    let err = outcome.error.unwrap();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert!(err.to_string().contains("verification"), "{err}");
    assert_eq!(log, Vec::<String>::new(), "the target was touched");
    assert!(
        outcome
            .report
            .left_behind
            .starts_with("nothing was written")
    );

    // Truncated, foreign and missing files: refused, nothing written.
    let truncated = dir.path().join("short.oxbackup");
    std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
    let foreign = dir.path().join("foreign.oxbackup");
    std::fs::write(&foreign, b"PK\x03\x04 definitely not a zip").unwrap();
    for file in [truncated, foreign, dir.path().join("missing.oxbackup")] {
        let (outcome, log) = try_restore(&file, &options(), ENGINE).await;
        assert!(outcome.error.is_some(), "{}", file.display());
        assert_eq!(log, Vec::<String>::new());
    }
}

#[tokio::test]
async fn refusals_come_before_any_write() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = make_backup(dir.path(), MockConfig::default()).await;

    let unconfirmed = RestoreOptions {
        drop_existing: true,
        ..options()
    };
    let (outcome, log) = try_restore(&path, &unconfirmed, ENGINE).await;
    assert!(outcome.error.unwrap().to_string().contains("confirm"));
    assert_eq!(log, Vec::<String>::new());

    let (outcome, log) = try_restore(&path, &options(), Engine::MySql).await;
    let err = outcome.error.unwrap();
    assert!(
        err.to_string()
            .contains("the backup is from PostgreSQL, the target is MySQL"),
        "{err}"
    );
    assert_eq!(log, Vec::<String>::new());

    let unknown = RestoreOptions {
        objects: vec![7],
        ..options()
    };
    let (outcome, log) = try_restore(&path, &unknown, ENGINE).await;
    assert!(outcome.error.unwrap().to_string().contains("no object 7"));
    assert_eq!(log, Vec::<String>::new());
}

#[tokio::test]
async fn existing_files_need_overwrite_and_cancelled_backups_leave_none() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = make_backup(dir.path(), MockConfig::default()).await;
    let driver = MockDriver::new(ENGINE);
    let connection = connect(&driver).await;
    let dialects = registry(Arc::new(Recorder::default()));
    let spec = BackupSpec {
        container: container(),
        objects: Vec::new(),
        content: BackupContent::StructureAndData,
        compression: Compression::Zstd,
    };
    let job = BackupJob {
        dialects: &dialects,
        metadata: connection.as_ref(),
        rows: connection.as_ref(),
        spec: &spec,
    };
    let err = backup(job, &path, false, &|_| {}, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");

    // A slow table: cancel mid-stream.
    let slow = MockDriver::new(ENGINE).with_config(MockConfig {
        batches: 100,
        rows_per_batch: 10,
        batch_delay: Duration::from_millis(50),
    });
    let connection = connect(&slow).await;
    let job = BackupJob {
        dialects: &dialects,
        metadata: connection.as_ref(),
        rows: connection.as_ref(),
        spec: &spec,
    };
    let target = dir.path().join("cancelled.oxbackup");
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        trigger.cancel();
    });
    let started = std::time::Instant::now();
    let err = backup(job, &target, false, &|_| {}, cancel)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!target.exists());
    let left: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        left,
        vec!["shop.oxbackup".to_owned()],
        "temporary files stayed"
    );
}

#[tokio::test]
async fn structure_only_backups_restore_without_rows() {
    let dir = tempfile::tempdir().unwrap();
    let driver = MockDriver::new(ENGINE);
    let connection = connect(&driver).await;
    let dialects = registry(Arc::new(Recorder::default()));
    let spec = BackupSpec {
        container: container(),
        objects: vec![ObjectRef::new(ObjectKind::Table, "items")],
        content: BackupContent::StructureOnly,
        compression: Compression::None,
    };
    let path = dir.path().join("structure.oxbackup");
    let report = backup(
        BackupJob {
            dialects: &dialects,
            metadata: connection.as_ref(),
            rows: connection.as_ref(),
            spec: &spec,
        },
        &path,
        false,
        &|_| {},
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(report.manifest.objects[0].data.is_none());
    assert!(
        !driver.log().iter().any(|s| s.starts_with("SELECT")),
        "no rows are read"
    );
    let opts = RestoreOptions {
        drop_existing: true,
        confirm_destructive: true,
        ..options()
    };
    let (outcome, log) = try_restore(&path, &opts, ENGINE).await;
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.report.rows, 0);
    assert!(outcome.report.warnings[0].contains("no table rows"));
    assert!(!log.iter().any(|s| s == "INSERT rows"), "{log:?}");
}

#[tokio::test]
async fn data_only_backups_load_rows_into_existing_tables() {
    let dir = tempfile::tempdir().unwrap();
    let driver = MockDriver::new(ENGINE);
    let connection = connect(&driver).await;
    let dialects = registry(Arc::new(Recorder::default()));
    let spec = BackupSpec {
        container: container(),
        objects: vec![ObjectRef::new(ObjectKind::Table, "items")],
        content: BackupContent::DataOnly,
        compression: Compression::None,
    };
    let path = dir.path().join("rows.oxbackup");
    let report = backup(
        BackupJob {
            dialects: &dialects,
            metadata: connection.as_ref(),
            rows: connection.as_ref(),
            spec: &spec,
        },
        &path,
        false,
        &|_| {},
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(report.manifest.content, BackupContent::DataOnly);
    assert_eq!(report.manifest.total_rows(), 12);

    // Any restore of it loads the rows and creates nothing.
    let (outcome, log) = try_restore(
        &path,
        &RestoreOptions {
            truncate_before_load: true,
            confirm_destructive: true,
            ..options()
        },
        ENGINE,
    )
    .await;
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.report.rows, 12);
    assert!(
        outcome
            .report
            .warnings
            .iter()
            .any(|w| w.contains("table rows only")),
        "{:?}",
        outcome.report.warnings
    );
    assert!(log.iter().any(|s| s == "INSERT rows"), "{log:?}");
    assert!(
        !log.iter()
            .any(|s| s.starts_with("CREATE") || s.starts_with("DROP")),
        "{log:?}"
    );

    // Structure only has nothing to restore from it.
    let (outcome, log) = try_restore(
        &path,
        &RestoreOptions {
            content: RestoreContent::StructureOnly,
            ..options()
        },
        ENGINE,
    )
    .await;
    let err = outcome.error.unwrap();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert!(err.to_string().contains("restore it as data only"), "{err}");
    assert_eq!(log, Vec::<String>::new(), "the target was touched");
}

#[tokio::test]
async fn foreign_keys_to_the_table_itself_wait_for_the_rows() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = make_backup_with(
        dir.path(),
        MockConfig::default(),
        Tables {
            self_ref: true,
            elsewhere: false,
        },
    )
    .await;
    let driver = MockDriver::new(ENGINE);
    let mut params = ConnectParams::new(ENGINE);
    params.database = Some("shop".into());
    let target: Arc<dyn Connection> = Arc::from(driver.connect(&params).await.unwrap());
    let dialects = registry_with(
        Arc::new(Recorder::default()),
        Tables {
            self_ref: true,
            elsewhere: false,
        },
    );
    let opts = RestoreOptions {
        drop_existing: true,
        confirm_destructive: true,
        ..options()
    };
    let run = runner(Arc::clone(&target));
    let outcome = restore_run(
        RestoreJob {
            dialects: &dialects,
            target: target.as_ref(),
            options: &opts,
        },
        &path,
        run.as_ref(),
        &|_| {},
        CancellationToken::new(),
    )
    .await;
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    let log = driver.log();
    let create = log
        .iter()
        .position(|s| s == "CREATE TABLE items (0 foreign keys)")
        .unwrap_or_else(|| panic!("{log:?}"));
    let insert = log.iter().position(|s| s == "INSERT rows").unwrap();
    let fk = log
        .iter()
        .position(|s| s.starts_with("ADD FOREIGN KEY items_parent"))
        .unwrap_or_else(|| panic!("{log:?}"));
    assert!(create < insert && insert < fk, "{log:?}");
}

#[tokio::test]
async fn definitions_outside_the_backed_up_container_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = make_backup_with(
        dir.path(),
        MockConfig::default(),
        Tables {
            self_ref: false,
            elsewhere: true,
        },
    )
    .await;
    for content in [RestoreContent::DataOnly, RestoreContent::StructureAndData] {
        let opts = RestoreOptions {
            content,
            truncate_before_load: true,
            confirm_destructive: true,
            ..options()
        };
        let (outcome, log) = try_restore(&path, &opts, ENGINE).await;
        let err = outcome.error.unwrap();
        assert_eq!(err.category(), ErrorCategory::InvalidInput);
        assert!(err.to_string().contains("does not lie in"), "{err}");
        assert_eq!(log, Vec::<String>::new(), "the target was touched");
    }
}
