//! `plan` and `copy_table` over the mock driver (`docs/m8-contracts.md` §12, transfer unit
//! tests): load order and cycles from the source foreign keys, foreign keys planned after the
//! load, blocked plans (unsupported columns, mode checks, name clashes), and the copy loop's
//! transactions, re-chunking, error modes and cancel.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use oxbase_dialect::{
    BacktickQuoting, BulkLimits, DdlContext, DdlGenerator, DialectRegistry, Fetcher, Fidelity,
    InsertLoader, LoadColumn, PgQuoting,
};
use oxbase_driver::mock::{MockConfig, MockDriver};
use oxbase_driver::{CancellationToken, ConnectParams, Connection, Driver};
use oxbase_transfer::pipeline::{
    CopyTable, PlanInputs, TableStatus, TransferPlan, copy_table, plan, prepare,
};
use oxbase_transfer::{
    ColumnOverride, ColumnPlan, ErrorMode, SourceObject, TableMode, TransactionScope,
    TransferOptions, TransferSpec,
};
use oxbase_types::{
    BoundStatement, ColumnDef, DataType, Engine, Error, FkAction, ForeignKeyDef, ObjectDef,
    ObjectKind, ObjectRef, ObjectSummary, PrimaryKeyDef, Result, Script, TableDef,
};
use pretty_assertions::assert_eq;

/// Serves fixed table definitions; anything else is `NotFound`.
struct Tables(Vec<TableDef>);

#[async_trait]
impl Fetcher for Tables {
    async fn list(
        &self,
        _c: &dyn Connection,
        _parent: &ObjectRef,
        _cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        Ok(Vec::new())
    }

    async fn load(
        &self,
        _c: &dyn Connection,
        obj: &ObjectRef,
        _cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        self.0
            .iter()
            .find(|t| t.obj.name == obj.name)
            .cloned()
            .map(ObjectDef::Table)
            .ok_or_else(|| Error::not_found(obj.name.clone()))
    }
}

/// Writes `CREATE <name>` / `DROP <name>` / `FK <table>.<name>`.
struct Ddl;

impl DdlGenerator for Ddl {
    fn create(&self, d: &ObjectDef, _ctx: &DdlContext<'_>) -> Result<Script> {
        Ok(match d {
            ObjectDef::Table(t) => Script::single(format!(
                "CREATE TABLE {} ({})",
                t.obj.name,
                t.columns
                    .iter()
                    .map(|c| format!("{} {}", c.name, c.data_type))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            ObjectDef::ForeignKey { table, foreign_key } => Script::single(format!(
                "ALTER TABLE {} ADD FOREIGN KEY ({}) REFERENCES {}",
                table.name,
                foreign_key.columns.join(", "),
                foreign_key.ref_table.name
            )),
            other => Script::single(format!("CREATE {}", other.name())),
        })
    }

    fn alter(&self, _old: &ObjectDef, _new: &ObjectDef, _ctx: &DdlContext<'_>) -> Result<Script> {
        Ok(Script::new())
    }

    fn drop_object(&self, r: &ObjectRef, _ctx: &DdlContext<'_>) -> Result<Script> {
        Ok(Script::single(format!("DROP TABLE {}", r.name)))
    }
}

fn column(name: &str, data_type: DataType) -> ColumnDef {
    ColumnDef::new(name, data_type)
}

fn table(name: &str, columns: Vec<ColumnDef>, fks: &[(&str, &str)]) -> TableDef {
    let mut t = TableDef::new(ObjectRef::table(Some("db"), Some("src"), name));
    t.columns = columns;
    t.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".into()],
    });
    t.foreign_keys = fks
        .iter()
        .map(|(col, parent)| ForeignKeyDef {
            name: Some(format!("{name}_{col}_fk")),
            columns: vec![(*col).to_owned()],
            ref_table: ObjectRef::table(Some("db"), Some("src"), *parent),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::NoAction,
            on_update: FkAction::NoAction,
            extras: oxbase_types::Extras::new(),
        })
        .collect();
    t
}

fn source_tables() -> Vec<TableDef> {
    let id = || column("id", DataType::INT32).not_null();
    vec![
        table("parent", vec![id(), column("name", DataType::Text)], &[]),
        table(
            "child",
            vec![id(), column("parent_id", DataType::INT32)],
            &[("parent_id", "parent")],
        ),
        table(
            "tree",
            vec![id(), column("up", DataType::INT32)],
            &[("up", "tree")],
        ),
        table(
            "a",
            vec![id(), column("b_id", DataType::INT32)],
            &[("b_id", "b")],
        ),
        table(
            "b",
            vec![id(), column("a_id", DataType::INT32)],
            &[("a_id", "a")],
        ),
        table(
            "docs",
            vec![
                id(),
                column(
                    "tsv",
                    DataType::Other {
                        name: "tsvector".into(),
                    },
                ),
            ],
            &[],
        ),
        table(
            "outside",
            vec![id(), column("x_id", DataType::INT32)],
            &[("x_id", "not_copied")],
        ),
    ]
}

fn registry(existing: Vec<TableDef>) -> DialectRegistry {
    let mut reg = DialectRegistry::new();
    oxbase_transfer::register(&mut reg);
    reg.register_quoting(Engine::Postgres, Arc::new(PgQuoting))
        .register_quoting(Engine::MySql, Arc::new(BacktickQuoting))
        .register_fetcher(
            Engine::Postgres,
            ObjectKind::Table,
            Arc::new(Tables(source_tables())),
        )
        .register_fetcher(Engine::MySql, ObjectKind::Table, Arc::new(Tables(existing)))
        .register_ddl(Engine::MySql, ObjectKind::Table, Arc::new(Ddl))
        .register_ddl(Engine::MySql, ObjectKind::ForeignKey, Arc::new(Ddl));
    reg
}

async fn connect(engine: Engine, config: MockConfig) -> Box<dyn Connection> {
    MockDriver::new(engine)
        .with_config(config)
        .connect(&ConnectParams::new(engine))
        .await
        .unwrap()
}

fn tables(names: &[&str]) -> TransferSpec {
    TransferSpec {
        objects: names
            .iter()
            .map(|n| SourceObject::Table {
                table: ObjectRef::table(Some("db"), Some("src"), *n),
                target_name: None,
                filter: None,
            })
            .collect(),
        target: ObjectRef::database("dst"),
        options: TransferOptions::default(),
        overrides: Vec::new(),
    }
}

async fn plan_of(spec: &TransferSpec, existing: Vec<TableDef>) -> TransferPlan {
    let reg = registry(existing);
    let source = connect(Engine::Postgres, MockConfig::default()).await;
    let target = connect(Engine::MySql, MockConfig::default()).await;
    plan(
        PlanInputs {
            dialects: &reg,
            source: source.as_ref(),
            target: target.as_ref(),
            spec,
        },
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn parents_load_first_and_foreign_keys_come_after_the_load() {
    let spec = tables(&["child", "tree", "parent", "a", "b", "outside"]);
    let plan = plan_of(&spec, Vec::new()).await;
    assert!(plan.runnable, "{:?}", plan.blocked);
    assert_eq!(plan.source_engine, Engine::Postgres);
    assert_eq!(plan.target_engine, Engine::MySql);
    let names: Vec<&str> = plan
        .order
        .iter()
        .map(|&i| plan.tables[i].target.name.as_str())
        .collect();
    assert_eq!(names, ["parent", "child", "tree", "a", "b", "outside"]);
    assert_eq!(plan.cycles, vec![vec![3, 4]]);
    let child = &plan.tables[0];
    assert_eq!(child.target, ObjectRef::table(Some("dst"), None, "child"));
    assert!(!child.exists);
    assert_eq!(child.mode, TableMode::Create);
    assert_eq!(
        child.ddl,
        ["CREATE TABLE child (id int32, parent_id int32);\n"]
    );
    assert_eq!(
        child.foreign_keys,
        ["ALTER TABLE child ADD FOREIGN KEY (parent_id) REFERENCES parent;\n"]
    );
    // A self-reference is planned like any other foreign key.
    assert_eq!(plan.tables[1].foreign_keys.len(), 1);
    let outside = &plan.tables[5];
    assert_eq!(outside.foreign_keys, Vec::<String>::new());
    assert!(
        outside
            .warnings
            .iter()
            .any(|w| w.contains("not_copied is not part of the transfer")),
        "{:?}",
        outside.warnings
    );
    assert_eq!(plan.fidelity, Fidelity::Exact);
    assert!(!plan.destructive);
    let json = serde_json::to_value(&plan).unwrap();
    assert_eq!(
        json["tables"][0]["columns"][0]["mapping"]["fidelity"],
        "exact"
    );
}

#[tokio::test]
async fn blocked_plans_say_why() {
    // An unsupported column.
    let plan = plan_of(&tables(&["docs"]), Vec::new()).await;
    assert!(!plan.runnable);
    assert_eq!(plan.fidelity, Fidelity::Unsupported);
    assert!(plan.blocked[0].contains("docs.tsv"), "{:?}", plan.blocked);

    // ... unless it is skipped.
    let mut skip = tables(&["docs"]);
    skip.overrides.push(ColumnOverride {
        object: 0,
        column: "tsv".into(),
        skip: true,
        target_name: None,
        data_type: None,
        native_type: None,
    });
    let plan = plan_of(&skip, Vec::new()).await;
    assert!(plan.runnable, "{:?}", plan.blocked);
    assert!(plan.tables[0].columns[1].skipped);

    // Create over an existing table; append into a missing one.
    let existing = vec![{
        let mut t = source_tables()[0].clone();
        t.obj = ObjectRef::table(Some("dst"), None, "parent");
        t
    }];
    let plan = plan_of(&tables(&["parent"]), existing.clone()).await;
    assert!(!plan.runnable);
    assert!(plan.tables[0].exists);
    assert!(
        plan.blocked[0].contains("already exists"),
        "{:?}",
        plan.blocked
    );
    let mut append = tables(&["parent", "child"]);
    append.options.mode = TableMode::Append;
    let plan = plan_of(&append, existing.clone()).await;
    assert!(!plan.runnable);
    assert_eq!(plan.blocked.len(), 1, "{:?}", plan.blocked);
    assert!(plan.blocked[0].contains("child does not exist"));

    // Replace and truncate are destructive when the table exists.
    let mut replace = tables(&["parent"]);
    replace.options.mode = TableMode::Replace;
    let plan = plan_of(&replace, existing.clone()).await;
    assert!(plan.runnable && plan.destructive);
    assert_eq!(
        plan.tables[0].ddl,
        [
            "DROP TABLE parent;\n",
            "CREATE TABLE parent (id int32, name text);\n"
        ]
    );
    let mut truncate = tables(&["parent"]);
    truncate.options.mode = TableMode::Truncate;
    let plan = plan_of(&truncate, existing).await;
    assert!(plan.runnable && plan.destructive);
    assert_eq!(plan.tables[0].ddl, ["TRUNCATE TABLE `dst`.`parent`;\n"]);

    // Two objects into one table name (MySQL table names are case-sensitive: no clash).
    let mut clash = tables(&["parent", "child"]);
    if let SourceObject::Table { target_name, .. } = &mut clash.objects[1] {
        *target_name = Some("Parent".into());
    }
    assert!(plan_of(&clash, Vec::new()).await.runnable);
}

#[tokio::test]
async fn query_sources_take_their_columns_from_the_result() {
    let mut spec = tables(&[]);
    spec.objects.push(SourceObject::Query {
        sql: "SELECT id, name FROM anywhere".into(),
        target_name: "copied".into(),
    });
    let reg = registry(Vec::new());
    let source = connect(Engine::Postgres, MockConfig::default()).await;
    let target = connect(Engine::MySql, MockConfig::default()).await;
    let prepared = prepare(
        PlanInputs {
            dialects: &reg,
            source: source.as_ref(),
            target: target.as_ref(),
            spec: &spec,
        },
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let planned = &prepared.plan;
    assert!(planned.runnable, "{:?}", planned.blocked);
    assert_eq!(planned.tables[0].source, None);
    let types: Vec<&DataType> = planned.tables[0]
        .columns
        .iter()
        .map(|c| &c.source_type)
        .collect();
    assert_eq!(types, [&DataType::INT64, &DataType::Text]);
    let table = &prepared.tables[0];
    assert_eq!(table.select.sql, "SELECT id, name FROM anywhere");
    assert_eq!(table.columns.len(), 2);
    assert!(table.ddl_in_transaction && table.creates);

    // MongoDB is not a transfer endpoint.
    let mongo = connect_mongo().await;
    let refused = plan(
        PlanInputs {
            dialects: &reg,
            source: source.as_ref(),
            target: mongo.as_ref(),
            spec: &spec,
        },
        CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(refused, Err(Error::Unsupported { .. })),
        "{refused:?}"
    );
}

async fn connect_mongo() -> Box<dyn Connection> {
    connect(Engine::MongoDb, MockConfig::default()).await
}

fn copy_columns() -> (Vec<ColumnPlan>, Vec<LoadColumn>) {
    let plans = vec![
        ColumnPlan {
            source: "id".into(),
            source_type: DataType::INT64,
            source_native: String::new(),
            target: "id".into(),
            mapping: oxbase_dialect::TypeMapping::exact(DataType::INT64),
            overridden: false,
            skipped: false,
        },
        ColumnPlan {
            source: "name".into(),
            source_type: DataType::Text,
            source_native: String::new(),
            target: "name".into(),
            mapping: oxbase_dialect::TypeMapping::exact(DataType::VarChar { length: Some(5) }),
            overridden: false,
            skipped: false,
        },
    ];
    let columns = vec![
        LoadColumn::new("id", DataType::INT64),
        LoadColumn::new("name", DataType::VarChar { length: Some(5) }),
    ];
    (plans, columns)
}

struct CopyRun {
    report: oxbase_transfer::TableReport,
    log: Vec<String>,
    progress: Vec<oxbase_transfer::TableProgress>,
}

async fn run_copy(
    config: MockConfig,
    options: &TransferOptions,
    cancel: CancellationToken,
) -> CopyRun {
    let driver = MockDriver::new(Engine::MySql).with_config(config);
    let source = driver
        .connect(&ConnectParams::new(Engine::MySql))
        .await
        .unwrap();
    let target = driver
        .connect(&ConnectParams::new(Engine::MySql))
        .await
        .unwrap();
    let (plans, columns) = copy_columns();
    let table_plan = oxbase_transfer::TablePlan {
        object: 0,
        source: None,
        target: ObjectRef::table(Some("dst"), None, "t"),
        exists: false,
        mode: TableMode::Create,
        columns: plans,
        fidelity: Fidelity::Exact,
        ddl: Vec::new(),
        foreign_keys: Vec::new(),
        estimated_rows: None,
        warnings: Vec::new(),
    };
    let loader = InsertLoader::new(
        Arc::new(BacktickQuoting),
        BulkLimits::for_engine(Engine::MySql),
    );
    let ddl = [BoundStatement::new("CREATE TABLE t", Vec::new())];
    let select = BoundStatement::new("SELECT * FROM src", Vec::new());
    let version = target.server_version().clone();
    let progress = Mutex::new(Vec::new());
    let on_progress = |p: &oxbase_transfer::TableProgress| {
        progress.lock().unwrap().push(p.clone());
    };
    let report = copy_table(
        CopyTable {
            plan: &table_plan,
            ddl: &ddl,
            source: source.as_ref(),
            select: &select,
            target: target.as_ref(),
            target_version: &version,
            loader: &loader,
            columns: &columns,
            options,
        },
        &on_progress,
        cancel,
    )
    .await
    .unwrap();
    CopyRun {
        report,
        log: driver.log(),
        progress: progress.into_inner().unwrap(),
    }
}

fn inserts(log: &[String]) -> usize {
    log.iter().filter(|s| s.starts_with("INSERT")).count()
}

#[tokio::test]
async fn copies_in_one_transaction_with_the_ddl_inside() {
    // 3 batches of 4 rows: names `row 0` .. `row 11` are too long for varchar(5) from row 10.
    let copy = run_copy(
        MockConfig::default(),
        &TransferOptions {
            errors: ErrorMode::Continue,
            ..TransferOptions::default()
        },
        CancellationToken::new(),
    )
    .await;
    let r = &copy.report;
    assert_eq!(r.status, TableStatus::Done, "{r:?}");
    assert_eq!(r.progress.rows_read, 12);
    assert_eq!(r.progress.rows_skipped, 2);
    assert_eq!(r.progress.rows_written, 10);
    assert_eq!(r.progress.rows_committed, 10);
    assert_eq!(r.issues.len(), 2);
    assert_eq!(r.issues[0].row, 11);
    assert_eq!(r.issues[0].column.as_deref(), Some("name"));
    assert_eq!(r.left_behind, "t holds the 10 copied rows");
    let log = &copy.log;
    let begin = log.iter().position(|s| s == "BEGIN").unwrap();
    let create = log.iter().position(|s| s == "CREATE TABLE t").unwrap();
    let commit = log.iter().position(|s| s == "COMMIT").unwrap();
    assert!(begin < create && create < commit, "{log:?}");
    // One load call: 10 rows are below the batch size.
    assert_eq!(inserts(log), 1, "{log:?}");
    assert_eq!(copy.progress.last().unwrap().rows_committed, 10);
}

#[tokio::test]
async fn per_batch_commits_and_stop_on_the_first_issue() {
    let copy = run_copy(
        MockConfig::default(),
        &TransferOptions {
            batch_rows: 3,
            transaction: TransactionScope::PerBatch,
            errors: ErrorMode::Stop,
            ..TransferOptions::default()
        },
        CancellationToken::new(),
    )
    .await;
    let r = &copy.report;
    assert_eq!(r.status, TableStatus::Failed);
    let error = r.error.as_deref().unwrap();
    assert!(error.contains("row 11, column 'name'"), "{error}");
    // Rows 1-6 went in 2 committed batches of 3 before row 11 (in the third source batch)
    // stopped the table.
    assert_eq!(r.progress.rows_committed, 6);
    assert!(r.left_behind.contains("6 rows"), "{}", r.left_behind);
    assert_eq!(copy.log.iter().filter(|s| *s == "COMMIT").count(), 2);
    assert_eq!(copy.log.last().map(String::as_str), Some("ROLLBACK"));
}

#[tokio::test]
async fn a_cancelled_copy_rolls_back() {
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        trigger.cancel();
    });
    let started = std::time::Instant::now();
    let copy = run_copy(
        MockConfig {
            batches: 1_000,
            rows_per_batch: 2,
            batch_delay: Duration::from_millis(20),
        },
        &TransferOptions {
            errors: ErrorMode::Continue,
            ..TransferOptions::default()
        },
        cancel,
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(copy.report.status, TableStatus::Cancelled);
    assert_eq!(copy.report.progress.rows_committed, 0);
    assert_eq!(copy.report.left_behind, "rolled back: no table was created");
    assert!(copy.log.iter().any(|s| s == "ROLLBACK"), "{:?}", copy.log);
}
