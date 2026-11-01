//! `ImportService` with a scripted in-memory driver and in-memory sources (no Docker): plan
//! checks and rejections, the DDL of each target mode inside or outside the transaction,
//! conversions, the error policies with the capped error log, commits per policy, server
//! errors naming the rows, cancel, and the clean-up of a created table.

use std::time::Instant;

use arrow_array::cast::AsArray;
use arrow_array::types::Int64Type;
use arrow_array::{Array, ArrayRef, Int64Array, RecordBatch, StringArray};
use async_trait::async_trait;
use futures::{StreamExt, stream};
use oxbase_dialect::{
    BulkLimits, BulkLoader, BulkMethod, Capabilities, DdlGenerator, DesignerCapabilities, Fetcher,
    LoadSession, LoadTarget, PgQuoting,
};
use oxbase_driver::{
    ConnectParams, Connection, Driver, DriverRegistry, ResultItem, ResultStream, Tx,
};
use oxbase_io::{CsvReadOptions, RowIssue};
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_store::{ConnectionProfile, MemorySecretStore, ProfileAddress};
use oxbase_types::{GeneratedColumn, IdentityDef, ObjectSummary, PropId, Script, Value};
use pretty_assertions::assert_eq;

use super::*;
use crate::ProvidedSecrets;
use testing::MemoryReader;

type Log = Arc<Mutex<Vec<String>>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Logs every statement (`BEGIN` / `COMMIT` / `ROLLBACK` too); a statement containing
/// `FAIL` is an SQL error; everything else `RowsAffected(0)`.
struct LogDriver {
    log: Log,
}

#[async_trait]
impl Driver for LogDriver {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        Ok(Box::new(LogConnection {
            log: Arc::clone(&self.log),
            database: params.database.clone(),
            version: ServerVersion::new(17, 0, 0),
        }))
    }
}

struct LogConnection {
    log: Log,
    database: Option<String>,
    version: ServerVersion,
}

fn answer(log: &Log, sql: &str) -> Result<ResultStream> {
    lock(log).push(sql.to_owned());
    if sql.contains("FAIL") {
        return Err(Error::sql(Some("XX000"), "the server refused it"));
    }
    Ok(stream::iter([Ok(ResultItem::RowsAffected(0))]).boxed())
}

#[async_trait]
impl Connection for LogConnection {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    fn server_version(&self) -> &ServerVersion {
        &self.version
    }

    fn current_database(&self) -> Option<String> {
        self.database.clone()
    }

    async fn execute(
        &self,
        sql: &str,
        _params: &[Value],
        _cancel: CancellationToken,
    ) -> Result<ResultStream> {
        answer(&self.log, sql)
    }

    async fn cancel_running(&self) -> Result<()> {
        Ok(())
    }

    async fn begin(&self) -> Result<Box<dyn Tx>> {
        lock(&self.log).push("BEGIN".into());
        Ok(Box::new(LogTx {
            log: Arc::clone(&self.log),
            done: false,
        }))
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

struct LogTx {
    log: Log,
    done: bool,
}

#[async_trait]
impl Tx for LogTx {
    async fn execute(
        &self,
        sql: &str,
        _params: &[Value],
        _cancel: CancellationToken,
    ) -> Result<ResultStream> {
        answer(&self.log, &format!("tx: {sql}"))
    }

    async fn commit(mut self: Box<Self>) -> Result<()> {
        self.done = true;
        lock(&self.log).push("COMMIT".into());
        Ok(())
    }

    async fn rollback(mut self: Box<Self>) -> Result<()> {
        self.done = true;
        lock(&self.log).push("ROLLBACK".into());
        Ok(())
    }
}

impl Drop for LogTx {
    fn drop(&mut self) {
        if !self.done {
            lock(&self.log).push("ROLLBACK (dropped)".into());
        }
    }
}

/// Tables of `db1.public`: `t (id int64 PK, code varchar(5) NOT NULL, amount decimal(10,2),
/// total generated, note text)`, `req (id, needed NOT NULL)`, `ident (id identity always,
/// v)`.
struct Tables;

fn table_def(name: &str) -> Option<TableDef> {
    let mut def = TableDef::new(table(name));
    match name {
        "t" => {
            def.columns = vec![
                ColumnDef::new("id", DataType::INT64).not_null(),
                ColumnDef::new("code", DataType::VarChar { length: Some(5) }).not_null(),
                ColumnDef::new(
                    "amount",
                    DataType::Decimal {
                        precision: Some(10),
                        scale: Some(2),
                    },
                ),
                {
                    let mut c = ColumnDef::new("total", DataType::INT64);
                    c.generated = Some(GeneratedColumn {
                        expression: "id * 2".into(),
                        stored: true,
                    });
                    c
                },
                ColumnDef::new("note", DataType::Text),
            ];
            def.primary_key = Some(PrimaryKeyDef {
                name: None,
                columns: vec!["id".into()],
            });
        }
        "req" => {
            def.columns = vec![
                ColumnDef::new("id", DataType::INT64),
                ColumnDef::new("needed", DataType::INT64).not_null(),
            ];
        }
        "ident" => {
            let mut id = ColumnDef::new("id", DataType::INT64).not_null();
            id.auto_increment = true;
            id.identity = Some(IdentityDef {
                always: true,
                start: None,
                increment: None,
            });
            def.columns = vec![id, ColumnDef::new("v", DataType::Text)];
        }
        _ => return None,
    }
    Some(def)
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
            parent.child(ObjectKind::Table, "t"),
        )])
    }

    async fn load(
        &self,
        _c: &dyn Connection,
        obj: &ObjectRef,
        _cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        table_def(&obj.name)
            .map(ObjectDef::Table)
            .ok_or_else(|| Error::not_found(format!("no table {}", obj.name)))
    }
}

/// `CREATE TABLE <name>` / `DROP TABLE <name>`, transactional or not.
struct ToyDdl {
    transactional: bool,
}

impl ToyDdl {
    fn step(&self, sql: String) -> ScriptStep {
        let step = ScriptStep::new(sql);
        if self.transactional {
            step
        } else {
            step.non_transactional()
        }
    }
}

impl DdlGenerator for ToyDdl {
    fn create(&self, d: &ObjectDef, _ctx: &DdlContext<'_>) -> Result<Script> {
        let ObjectDef::Table(t) = d else {
            return Err(Error::unsupported("only tables"));
        };
        let columns: Vec<String> = t
            .columns
            .iter()
            .map(|c| {
                let null = if c.nullable { "" } else { " NOT NULL" };
                format!("{} {}{null}", c.name, c.data_type)
            })
            .collect();
        let mut script = Script::new();
        script.push_step(self.step(format!(
            "CREATE TABLE {} ({})",
            t.obj.name,
            columns.join(", ")
        )));
        Ok(script)
    }

    fn alter(&self, _old: &ObjectDef, _new: &ObjectDef, _ctx: &DdlContext<'_>) -> Result<Script> {
        Err(Error::unsupported("not in these tests"))
    }

    fn drop_object(&self, r: &ObjectRef, _ctx: &DdlContext<'_>) -> Result<Script> {
        let mut script = Script::new();
        script.push_step(self.step(format!("DROP TABLE {}", r.name)));
        Ok(script)
    }
}

struct ToyCaps {
    transactional: bool,
}

impl Capabilities for ToyCaps {
    fn supports(&self, _kind: ObjectKind, _prop: PropId, _v: &ServerVersion) -> bool {
        true
    }

    fn designer(&self, _v: &ServerVersion) -> DesignerCapabilities {
        DesignerCapabilities {
            transactional_ddl: self.transactional,
            ..DesignerCapabilities::default()
        }
    }
}

/// Keeps the loaded batches; fails a call that holds the text `boom`; sleeps `delay` per call.
struct RecordingLoader {
    loaded: Arc<Mutex<Vec<RecordBatch>>>,
    log: Log,
    delay: Duration,
}

#[async_trait]
impl BulkLoader for RecordingLoader {
    fn method(&self, _version: &ServerVersion) -> BulkMethod {
        BulkMethod::CopyText
    }

    fn limits(&self, _version: &ServerVersion) -> BulkLimits {
        BulkLimits {
            rows_per_call: 4,
            ..BulkLimits::for_engine(Engine::Postgres)
        }
    }

    async fn load(
        &self,
        _session: LoadSession<'_>,
        target: &LoadTarget<'_>,
        batch: &RecordBatch,
        cancel: CancellationToken,
    ) -> Result<u64> {
        target.check_batch(batch)?;
        if !self.delay.is_zero() {
            tokio::select! {
                () = cancel.cancelled() => return Err(Error::Cancelled),
                () = tokio::time::sleep(self.delay) => {}
            }
        }
        let has_boom = batch.columns().iter().any(|c| {
            c.as_string_opt::<i32>()
                .is_some_and(|s| s.iter().any(|v| v == Some("boom")))
        });
        if has_boom {
            return Err(Error::sql(Some("23505"), "duplicate key"));
        }
        lock(&self.log).push(format!("LOAD {}", batch.num_rows()));
        lock(&self.loaded).push(batch.clone());
        Ok(u64::try_from(batch.num_rows()).unwrap())
    }
}

struct Harness {
    import: ImportService,
    log: Log,
    loaded: Arc<Mutex<Vec<RecordBatch>>>,
    session: SessionId,
    dir: tempfile::TempDir,
}

#[derive(Clone, Copy)]
struct Setup {
    transactional_ddl: bool,
    /// Register the recording loader (else the fallback `InsertLoader` runs).
    recording: bool,
    delay: Duration,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            transactional_ddl: true,
            recording: true,
            delay: Duration::ZERO,
        }
    }
}

async fn harness(setup: Setup) -> Harness {
    let log = Log::default();
    let mut drivers = DriverRegistry::new();
    drivers.register(Arc::new(LogDriver {
        log: Arc::clone(&log),
    }));
    let manager = Arc::new(ConnectionManager::new(
        drivers,
        Arc::new(MemorySecretStore::new()),
        Arc::new(RusshTunnelProvider::new()),
    ));
    let mut profile = ConnectionProfile::new(
        "import",
        Engine::Postgres,
        ProfileAddress::Network {
            host: "import.invalid".into(),
            port: Some(5432),
        },
    );
    profile.database = Some("db1".into());
    let base = manager
        .open(&profile, &ProvidedSecrets::none(), CancellationToken::new())
        .await
        .unwrap();
    let loaded = Arc::new(Mutex::new(Vec::new()));
    let mut dialects = DialectRegistry::new();
    dialects.register_fetcher(Engine::Postgres, ObjectKind::Table, Arc::new(Tables));
    dialects.register_quoting(Engine::Postgres, Arc::new(PgQuoting));
    dialects.register_ddl(
        Engine::Postgres,
        ObjectKind::Table,
        Arc::new(ToyDdl {
            transactional: setup.transactional_ddl,
        }),
    );
    dialects.register_capabilities(
        Engine::Postgres,
        Arc::new(ToyCaps {
            transactional: setup.transactional_ddl,
        }),
    );
    if setup.recording {
        dialects.register_bulk_loader(
            Engine::Postgres,
            Arc::new(RecordingLoader {
                loaded: Arc::clone(&loaded),
                log: Arc::clone(&log),
                delay: setup.delay,
            }),
        );
    }
    let dialects = Arc::new(dialects);
    let tree = Arc::new(TreeService::new(
        Arc::clone(&manager),
        Arc::clone(&dialects),
    ));
    let config = ImportConfig {
        progress_interval: Duration::ZERO,
        ..ImportConfig::default()
    };
    let import = ImportService::new(manager, tree, dialects, config);
    Harness {
        import,
        log,
        loaded,
        session: base.id(),
        dir: tempfile::tempdir().unwrap(),
    }
}

fn table(name: &str) -> ObjectRef {
    ObjectRef::table(Some("db1"), Some("public"), name)
}

fn text_specs(names: &[&str]) -> Vec<ColumnSpec> {
    names
        .iter()
        .map(|n| ColumnSpec::new(*n, DataType::Text))
        .collect()
}

fn text_rows(columns: &[Vec<Option<&str>>]) -> RecordBatch {
    let specs: Vec<ColumnSpec> = (0..columns.len())
        .map(|i| ColumnSpec::new(format!("c{i}"), DataType::Text))
        .collect();
    let arrays: Vec<ArrayRef> = columns
        .iter()
        .map(|c| Arc::new(StringArray::from(c.clone())) as ArrayRef)
        .collect();
    RecordBatch::try_new(ColumnSpec::schema(&specs), arrays).unwrap()
}

fn map(source: usize, target: &str) -> ColumnMapping {
    ColumnMapping {
        source,
        target: Some(target.into()),
        data_type: None,
    }
}

impl Harness {
    /// A small file whose size the transaction policy reads.
    fn source(&self) -> ImportSource {
        let path = self.dir.path().join("data.csv");
        std::fs::write(&path, b"id,code\n1,a\n").unwrap();
        ImportSource {
            path,
            options: ReadOptions::Csv(CsvReadOptions::default()),
        }
    }

    fn request(
        &self,
        specs: Vec<ColumnSpec>,
        mapping: Vec<ColumnMapping>,
        target: &str,
        mode: TargetMode,
    ) -> ImportRequest {
        ImportRequest {
            options: ReadOptions::Csv(CsvReadOptions::default()),
            columns: specs,
            mapping,
            target: ImportTarget {
                session: self.session,
                table: table(target),
                mode,
            },
            primary_key: Vec::new(),
            conversion: ConversionOptions::default(),
            errors: ErrorPolicy::Stop,
            transaction: TransactionPolicy::Single,
        }
    }

    async fn plan_err(&self, request: &ImportRequest) -> Error {
        self.import
            .plan(&self.source(), request, CancellationToken::new())
            .await
            .unwrap_err()
    }

    async fn run(
        &self,
        request: ImportRequest,
        batches: Vec<(RecordBatch, Vec<RowIssue>)>,
    ) -> Vec<ImportEvent> {
        let reader = MemoryReader::new(&request.columns, batches);
        let handle = self
            .import
            .start_with_reader(&self.source(), request, Box::new(reader))
            .await
            .unwrap();
        drain(handle.events).await
    }

    fn log(&self) -> Vec<String> {
        lock(&self.log).clone()
    }

    fn loaded_rows(&self) -> usize {
        lock(&self.loaded).iter().map(RecordBatch::num_rows).sum()
    }
}

async fn drain(mut events: mpsc::Receiver<ImportEvent>) -> Vec<ImportEvent> {
    let mut out = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("the import must not stall")
            .expect("the import must end with a terminal event");
        let terminal = event.is_terminal();
        out.push(event);
        if terminal {
            return out;
        }
    }
}

fn report_of(events: &[ImportEvent]) -> &ImportReport {
    match events.last() {
        Some(
            ImportEvent::Finished { report }
            | ImportEvent::Failed { report, .. }
            | ImportEvent::Cancelled { report },
        ) => report,
        other => panic!("no terminal event: {other:?}"),
    }
}

fn failure(events: &[ImportEvent]) -> &ErrorInfo {
    match events.last() {
        Some(ImportEvent::Failed { error, .. }) => error,
        other => panic!("not a failure: {other:?}"),
    }
}

#[tokio::test]
async fn plan_rejects_what_cannot_work() {
    let h = harness(Setup::default()).await;
    let specs = text_specs(&["a", "b"]);

    let unknown = h.request(specs.clone(), vec![map(0, "nope")], "t", TargetMode::Append);
    assert!(
        h.plan_err(&unknown)
            .await
            .to_string()
            .contains("no column 'nope'")
    );

    let generated = h.request(
        specs.clone(),
        vec![map(0, "id"), map(1, "code"), map(0, "total")],
        "t",
        TargetMode::Append,
    );
    assert!(
        h.plan_err(&generated)
            .await
            .to_string()
            .contains("generated")
    );

    let twice = h.request(
        specs.clone(),
        vec![map(0, "id"), map(1, "id")],
        "t",
        TargetMode::Append,
    );
    assert!(
        h.plan_err(&twice)
            .await
            .to_string()
            .contains("more than once")
    );

    // `code` is NOT NULL without a default.
    let required = h.request(specs.clone(), vec![map(0, "id")], "t", TargetMode::Append);
    let e = h.plan_err(&required).await;
    assert_eq!(e.category(), ErrorCategory::InvalidInput);
    assert!(e.to_string().contains("code"), "{e}");

    let identity = h.request(
        specs.clone(),
        vec![map(0, "id"), map(1, "v")],
        "ident",
        TargetMode::Append,
    );
    assert!(
        h.plan_err(&identity)
            .await
            .to_string()
            .contains("GENERATED ALWAYS")
    );

    let binary = vec![
        ColumnSpec::new("a", DataType::VarBinary { length: None }),
        ColumnSpec::new("b", DataType::Text),
    ];
    let no_conversion = h.request(
        binary,
        vec![map(0, "amount"), map(1, "code"), map(1, "id")],
        "t",
        TargetMode::Append,
    );
    assert!(
        h.plan_err(&no_conversion)
            .await
            .to_string()
            .contains("no conversion")
    );

    let exists = h.request(specs.clone(), vec![map(0, "id")], "t", TargetMode::Create);
    assert!(
        h.plan_err(&exists)
            .await
            .to_string()
            .contains("already exists")
    );

    let missing = h.request(
        specs.clone(),
        vec![map(0, "id")],
        "missing",
        TargetMode::Append,
    );
    assert_eq!(
        h.plan_err(&missing).await.category(),
        ErrorCategory::NotFound
    );

    let nothing = h.request(
        specs.clone(),
        vec![ColumnMapping {
            source: 0,
            target: None,
            data_type: None,
        }],
        "t",
        TargetMode::Append,
    );
    assert!(
        h.plan_err(&nothing)
            .await
            .to_string()
            .contains("at least one")
    );

    let out_of_range = h.request(specs.clone(), vec![map(5, "id")], "t", TargetMode::Append);
    assert!(
        h.plan_err(&out_of_range)
            .await
            .to_string()
            .contains("source column 6")
    );

    let mut bad_key = h.request(specs, vec![map(0, "x")], "new_t", TargetMode::Create);
    bad_key.primary_key = vec!["y".into()];
    assert!(
        h.plan_err(&bad_key)
            .await
            .to_string()
            .contains("primary key column 'y'")
    );

    // Nothing ran.
    assert_eq!(h.log().len(), 0, "{:?}", h.log());
}

#[tokio::test]
async fn plan_lists_ddl_columns_and_policy() {
    let h = harness(Setup::default()).await;
    let specs = vec![
        ColumnSpec::new("ID", DataType::Text),
        ColumnSpec::new("code", DataType::Text),
        ColumnSpec::new("amount", DataType::FLOAT64),
    ];
    let mut request = h.request(
        specs.clone(),
        vec![map(0, "ID"), map(1, "code"), map(2, "amount")],
        "t",
        TargetMode::Truncate,
    );
    request.transaction = TransactionPolicy::Batches { rows: 1000 };
    let plan = h
        .import
        .plan(&h.source(), &request, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(plan.method, BulkMethod::CopyText);
    assert_eq!(plan.ddl, ["TRUNCATE TABLE \"public\".\"t\""]);
    assert!(
        !plan.destructive,
        "transactional TRUNCATE is undone by a rollback"
    );
    assert_eq!(plan.transaction.commit_every, Some(1000));
    // `ID` matched `id` ignoring case.
    assert_eq!(plan.columns[0].target, "id");
    assert!(plan.columns[0].conversion.is_some());
    assert!(
        plan.columns[2]
            .conversion
            .as_deref()
            .unwrap()
            .contains("rounded")
    );

    request.transaction = TransactionPolicy::Auto;
    request.target.mode = TargetMode::Create;
    request.target.table = table("fresh");
    request.primary_key = vec!["ID".into()];
    let plan = h
        .import
        .plan(&h.source(), &request, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        plan.transaction.commit_every, None,
        "a small file is one transaction"
    );
    assert!(plan.creates_table);
    assert_eq!(plan.ddl.len(), 1);
    assert!(
        plan.ddl[0].starts_with("CREATE TABLE fresh (ID text NOT NULL"),
        "{:?}",
        plan.ddl
    );

    // MySQL-like DDL: truncate and replace are destructive.
    let h = harness(Setup {
        transactional_ddl: false,
        ..Setup::default()
    })
    .await;
    let mut request = h.request(
        specs,
        vec![map(0, "id"), map(1, "code")],
        "t",
        TargetMode::Truncate,
    );
    let plan = h
        .import
        .plan(&h.source(), &request, CancellationToken::new())
        .await
        .unwrap();
    assert!(plan.destructive);
    request.target.mode = TargetMode::Replace;
    let plan = h
        .import
        .plan(&h.source(), &request, CancellationToken::new())
        .await
        .unwrap();
    assert!(plan.destructive);
    assert_eq!(
        plan.ddl,
        ["DROP TABLE t", "CREATE TABLE t (id text, code text)"]
    );
    assert!(
        plan.warnings.iter().any(|w| w.contains("dropped")),
        "{:?}",
        plan.warnings
    );
}

#[tokio::test]
async fn create_mode_runs_ddl_in_the_transaction_and_converts() {
    let h = harness(Setup::default()).await;
    let specs = text_specs(&["id", "amount"]);
    let mut request = h.request(
        specs,
        vec![
            ColumnMapping {
                source: 0,
                target: Some("id".into()),
                data_type: Some(DataType::INT64),
            },
            ColumnMapping {
                source: 1,
                target: Some("amount".into()),
                data_type: Some(DataType::Decimal {
                    precision: Some(6),
                    scale: Some(2),
                }),
            },
        ],
        "fresh",
        TargetMode::Create,
    );
    request.primary_key = vec!["id".into()];
    let batches = vec![
        (
            text_rows(&[
                vec![Some("1"), Some("2"), Some("3")],
                vec![Some("1.5"), None, Some("-2.345")],
            ]),
            Vec::new(),
        ),
        (
            text_rows(&[vec![Some("4"), Some("5")], vec![Some("10"), Some("0.001")]]),
            Vec::new(),
        ),
    ];
    let events = h.run(request, batches).await;
    let report = report_of(&events);
    assert!(matches!(events[0], ImportEvent::Started { .. }));
    assert!(
        matches!(events.last(), Some(ImportEvent::Finished { .. })),
        "{events:?}"
    );
    assert_eq!(report.rows_read, 5);
    assert_eq!(report.rows_inserted, 5);
    assert_eq!(report.rows_committed, 5);
    assert!(report.table_created);
    assert!(!report.table_dropped);
    assert_eq!(report.bytes_read, 5 * testing::ROW_BYTES);
    let log = h.log();
    let begin = log.iter().position(|l| l == "BEGIN").unwrap();
    assert!(
        log[begin + 1].starts_with("tx: CREATE TABLE fresh"),
        "{log:?}"
    );
    assert_eq!(log.iter().filter(|l| *l == "COMMIT").count(), 1, "{log:?}");
    // Rows per call: 4, then the rest.
    assert_eq!(
        log.iter()
            .filter(|l| l.starts_with("LOAD"))
            .cloned()
            .collect::<Vec<_>>(),
        ["LOAD 4", "LOAD 1"]
    );
    let loaded = lock(&h.loaded);
    let ids = loaded[0].column(0).as_primitive::<Int64Type>();
    assert_eq!(ids.value(3), 4);
    let amounts = loaded[0].column(1).as_string::<i32>();
    assert_eq!(amounts.value(0), "1.50".trim_end_matches('0'));
    assert!(amounts.is_null(1));
    assert_eq!(amounts.value(2), "-2.35");
    assert_eq!(loaded[1].column(1).as_string::<i32>().value(0), "0.00");
    let committed: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            ImportEvent::Committed { rows_committed } => Some(*rows_committed),
            _ => None,
        })
        .collect();
    assert_eq!(committed, [5]);
}

#[tokio::test]
async fn skip_policy_drops_bad_rows_and_caps_the_log() {
    let h = harness(Setup::default()).await;
    let specs = text_specs(&["id", "code"]);
    let mut request = h.request(
        specs,
        vec![map(0, "id"), map(1, "code")],
        "t",
        TargetMode::Append,
    );
    request.errors = ErrorPolicy::Skip { max_errors: 10 };
    let reader_issue = RowIssue {
        row: 2,
        batch_row: 1,
        column: Some("code".into()),
        message: "unreadable".into(),
    };
    let batches = vec![(
        text_rows(&[
            vec![Some("1"), Some("2"), Some("x"), Some("4"), Some("5")],
            vec![Some("a"), Some("b"), Some("c"), Some("toolong"), None],
        ]),
        vec![reader_issue],
    )];
    let events = h.run(request.clone(), batches).await;
    let report = report_of(&events);
    assert!(
        matches!(events.last(), Some(ImportEvent::Finished { .. })),
        "{events:?}"
    );
    // Row 2: reader issue; row 3: not an int; row 4: too long; row 5: NULL in NOT NULL.
    assert_eq!(report.rows_skipped, 4);
    assert_eq!(report.rows_inserted, 1);
    let rows: Vec<u64> = report.errors.iter().map(|e| e.row).collect();
    assert_eq!(rows, [2, 3, 4, 5]);
    let row_errors = events
        .iter()
        .filter(|e| matches!(e, ImportEvent::RowError { .. }))
        .count();
    assert_eq!(row_errors, 4);
    assert_eq!(h.loaded_rows(), 1);

    // Past max_errors: failed, rolled back.
    request.errors = ErrorPolicy::Skip { max_errors: 1 };
    let batches = vec![(
        text_rows(&[
            vec![Some("x"), Some("y"), Some("3")],
            vec![Some("a"), Some("b"), Some("c")],
        ]),
        Vec::new(),
    )];
    let events = h.run(request.clone(), batches).await;
    assert!(
        failure(&events).message.contains("more than 1 rows"),
        "{events:?}"
    );
    assert_eq!(
        report_of(&events).left_behind,
        "nothing was inserted: t is as it was"
    );

    // The report keeps at most `max_reported_errors`.
    let mut small = harness(Setup::default()).await;
    small.import.config.max_reported_errors = 2;
    request.target.session = small.session;
    request.errors = ErrorPolicy::Skip { max_errors: 100 };
    let bad: Vec<Option<&str>> = vec![Some("x"); 5];
    let codes: Vec<Option<&str>> = vec![Some("a"); 5];
    let events = small
        .run(request, vec![(text_rows(&[bad, codes]), Vec::new())])
        .await;
    let report = report_of(&events);
    assert_eq!(report.errors.len(), 2);
    assert!(report.errors_truncated);
    assert_eq!(report.rows_skipped, 5);
}

#[tokio::test]
async fn stop_policy_fails_at_the_first_issue() {
    let h = harness(Setup::default()).await;
    let specs = text_specs(&["id", "code"]);
    let request = h.request(
        specs,
        vec![map(0, "id"), map(1, "code")],
        "t",
        TargetMode::DeleteAll,
    );
    let batches = vec![
        (
            text_rows(&[
                vec![Some("1"), Some("2"), Some("3"), Some("4")],
                vec![Some("a"); 4],
            ]),
            Vec::new(),
        ),
        (
            text_rows(&[vec![Some("5"), Some("six")], vec![Some("a"); 2]]),
            Vec::new(),
        ),
    ];
    let events = h.run(request, batches).await;
    let error = failure(&events);
    assert_eq!(error.category, ErrorCategory::InvalidInput);
    assert!(error.message.contains("row 6, column 'id'"), "{error:?}");
    let report = report_of(&events);
    assert_eq!(report.rows_committed, 0);
    assert_eq!(report.errors.len(), 1);
    assert!(
        report.left_behind.contains("as it was"),
        "{}",
        report.left_behind
    );
    let log = h.log();
    assert!(
        log.contains(&"tx: DELETE FROM \"public\".\"t\"".to_owned()),
        "{log:?}"
    );
    assert_eq!(log.last().map(String::as_str), Some("ROLLBACK"), "{log:?}");
}

#[tokio::test]
async fn batches_commit_and_failures_keep_committed_rows() {
    let h = harness(Setup::default()).await;
    let specs = text_specs(&["id", "code"]);
    let mut request = h.request(
        specs,
        vec![map(0, "id"), map(1, "code")],
        "t",
        TargetMode::Append,
    );
    request.transaction = TransactionPolicy::Batches { rows: 2 };
    let ids: Vec<Option<&str>> = vec![Some("1"), Some("2"), Some("3"), Some("4"), Some("5")];
    let codes = vec![Some("a"), Some("b"), Some("c"), Some("d"), Some("boom")];
    let events = h
        .run(request, vec![(text_rows(&[ids, codes]), Vec::new())])
        .await;
    let error = failure(&events);
    assert_eq!(error.category, ErrorCategory::Sql);
    assert!(error.message.contains("row 5"), "{error:?}");
    let report = report_of(&events);
    assert_eq!(report.rows_committed, 4);
    assert_eq!(report.rows_inserted, 4);
    assert!(
        report.left_behind.contains("keeps the 4 rows"),
        "{}",
        report.left_behind
    );
    let committed: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            ImportEvent::Committed { rows_committed } => Some(*rows_committed),
            _ => None,
        })
        .collect();
    assert_eq!(committed, [2, 4]);
    assert_eq!(h.log().iter().filter(|l| *l == "COMMIT").count(), 2);
}

#[tokio::test]
async fn failed_create_drops_the_table_when_ddl_commits() {
    let h = harness(Setup {
        transactional_ddl: false,
        ..Setup::default()
    })
    .await;
    let specs = text_specs(&["id"]);
    let request = h.request(specs, vec![map(0, "id")], "fresh", TargetMode::Create);
    let events = h
        .run(request, vec![(text_batch_of(&["1", "boom"]), Vec::new())])
        .await;
    assert!(
        matches!(events.last(), Some(ImportEvent::Failed { .. })),
        "{events:?}"
    );
    let report = report_of(&events);
    assert!(report.table_created);
    assert!(report.table_dropped);
    assert_eq!(
        report.left_behind,
        "the table the import created was dropped"
    );
    let log = h.log();
    let create = log
        .iter()
        .position(|l| l.starts_with("CREATE TABLE fresh"))
        .unwrap();
    let begin = log.iter().position(|l| l == "BEGIN").unwrap();
    assert!(
        create < begin,
        "DDL that commits runs before the transaction: {log:?}"
    );
    assert_eq!(
        log.last().map(String::as_str),
        Some("DROP TABLE fresh"),
        "{log:?}"
    );

    // With transactional DDL the rollback undoes the CREATE: nothing to drop.
    let h = harness(Setup::default()).await;
    let request = h.request(
        text_specs(&["id"]),
        vec![map(0, "id")],
        "fresh",
        TargetMode::Create,
    );
    let events = h
        .run(request, vec![(text_batch_of(&["1", "boom"]), Vec::new())])
        .await;
    let report = report_of(&events);
    assert!(!report.table_created);
    assert!(!report.table_dropped);
    assert!(
        report.left_behind.contains("no table was created"),
        "{}",
        report.left_behind
    );
    assert!(
        !h.log().iter().any(|l| l.starts_with("DROP")),
        "{:?}",
        h.log()
    );
}

fn text_batch_of(values: &[&str]) -> RecordBatch {
    let values: Vec<Option<&str>> = values.iter().map(|v| Some(*v)).collect();
    testing::text_batch(&values)
}

#[tokio::test]
async fn fallback_loader_binds_values() {
    let h = harness(Setup {
        recording: false,
        ..Setup::default()
    })
    .await;
    let specs = vec![
        ColumnSpec::new("id", DataType::INT64),
        ColumnSpec::new("code", DataType::Text),
    ];
    let request = h.request(
        specs.clone(),
        vec![map(0, "id"), map(1, "code")],
        "t",
        TargetMode::Append,
    );
    let batch = RecordBatch::try_new(
        ColumnSpec::schema(&specs),
        vec![
            Arc::new(Int64Array::from(vec![7, 8])),
            Arc::new(StringArray::from(vec!["x';--", "y"])),
        ],
    )
    .unwrap();
    let events = h.run(request, vec![(batch, Vec::new())]).await;
    assert!(
        matches!(events.last(), Some(ImportEvent::Finished { .. })),
        "{events:?}"
    );
    assert_eq!(report_of(&events).method, BulkMethod::MultiRowInsert);
    let log = h.log();
    let insert = log.iter().find(|l| l.contains("INSERT")).unwrap();
    assert_eq!(
        insert,
        "tx: INSERT INTO \"public\".\"t\" (\"id\", \"code\") VALUES ($1, $2), ($3, $4)"
    );
}

#[tokio::test]
async fn cancel_ends_quickly_and_keeps_committed_batches() {
    let h = harness(Setup {
        delay: Duration::from_millis(50),
        ..Setup::default()
    })
    .await;
    let specs = text_specs(&["id", "code"]);
    let mut request = h.request(
        specs,
        vec![map(0, "id"), map(1, "code")],
        "t",
        TargetMode::Append,
    );
    request.transaction = TransactionPolicy::Batches { rows: 4 };
    let batches: Vec<(RecordBatch, Vec<RowIssue>)> = (0..200)
        .map(|i| {
            let id = (i + 1).to_string();
            (
                text_rows(&[vec![Some(id.as_str()); 4], vec![Some("a"); 4]]),
                Vec::new(),
            )
        })
        .collect();
    let reader = MemoryReader::new(&request.columns, batches).slow(Duration::from_millis(5));
    let handle = h
        .import
        .start_with_reader(&h.source(), request, Box::new(reader))
        .await
        .unwrap();
    assert!(h.import.is_running(handle.id));
    let mut events = handle.events;
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        let committed =
            matches!(event, ImportEvent::Committed { rows_committed } if rows_committed >= 8);
        seen.push(event);
        if committed {
            break;
        }
    }
    let asked = Instant::now();
    assert!(h.import.cancel(handle.id));
    let rest = drain(events).await;
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "{:?}",
        asked.elapsed()
    );
    let report = report_of(&rest);
    assert!(
        matches!(rest.last(), Some(ImportEvent::Cancelled { .. })),
        "{rest:?}"
    );
    assert!(report.rows_committed >= 8);
    assert_eq!(report.rows_committed % 4, 0);
    assert!(!h.import.is_running(handle.id));
    assert!(!h.import.cancel(handle.id));
    assert_eq!(
        h.log().last().map(String::as_str),
        Some("ROLLBACK"),
        "{:?}",
        h.log()
    );
    assert_eq!(
        u64::try_from(h.loaded_rows()).unwrap(),
        report.rows_inserted,
        "every loaded call is counted"
    );
}

#[tokio::test]
async fn progress_is_monotonic_and_ends_with_every_byte() {
    let h = harness(Setup::default()).await;
    let request = h.request(
        text_specs(&["id", "code"]),
        vec![map(0, "id"), map(1, "code")],
        "t",
        TargetMode::Append,
    );
    let batches: Vec<(RecordBatch, Vec<RowIssue>)> = (0..5)
        .map(|_| {
            (
                text_rows(&[vec![Some("1"); 3], vec![Some("a"); 3]]),
                Vec::new(),
            )
        })
        .collect();
    let events = h.run(request, batches).await;
    let progress: Vec<(u64, u64)> = events
        .iter()
        .filter_map(|e| match e {
            ImportEvent::Progress {
                bytes_read,
                rows_read,
                ..
            } => Some((*bytes_read, *rows_read)),
            _ => None,
        })
        .collect();
    assert!(progress.len() >= 2, "{events:?}");
    assert!(
        progress
            .windows(2)
            .all(|w| w[0].0 <= w[1].0 && w[0].1 <= w[1].1)
    );
    assert_eq!(progress.last().unwrap().0, 15 * testing::ROW_BYTES);
    assert_eq!(progress.last().unwrap().1, 15);
}

#[test]
fn conversion_notes_are_public() {
    assert_eq!(
        conversion_note(&DataType::INT32, &DataType::INT64).unwrap(),
        None
    );
    assert!(conversion_note(&DataType::Date, &DataType::Uuid).is_err());
}
