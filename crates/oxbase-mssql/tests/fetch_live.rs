//! Live tests of the SQL Server catalog fetchers (docs/m4-contracts.md §3 and §12, fetch)
//! against the `mssql` fixture and its M2 seed (`tests/docker/init/mssql/04-m2.sql`, names in
//! `tests/fixtures.toml` `[m2]` / `[m2.mssql]`).
//!
//! The M4 shapes (identity, computed and rowversion columns, filtered and included-column
//! indexes, temporal tables, an indexed view with an INSTEAD OF trigger, table-valued functions,
//! alias and table types, a database DDL trigger) are created by the test itself in its own
//! schema `m4_fetch_live` (the names of `[m4]`), so they do not depend on the M4 seed script, and
//! dropped again at the end.
//!
//! All are `#[ignore]`d: the conformance CI job runs them with
//! `cargo test -p oxbase-mssql -- --include-ignored --test-threads 1`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxbase_dialect::{DialectRegistry, Fetcher};
use oxbase_driver::{CancellationToken, ConnectParams, Connection, collect};
use oxbase_mssql::{MsSqlConnection, MsSqlDriver};
use oxbase_types::{
    ArgMode, DataType, Engine, Error, FkAction, ObjectDef, ObjectKind, ObjectRef, ObjectSummary,
    TlsMode, TlsOptions, TriggerEvent, TriggerLevel, TriggerTiming, Value, props,
};

use oxbase_mssql::extras as x;

/// The test's own schema for the M4 shapes.
const M4: &str = "m4_fetch_live";
/// The database DDL trigger created with the M4 shapes (must never be listed).
const DDL_TRIGGER: &str = "m4_fetch_live_ddl_trg";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn fixtures() -> toml::Table {
    let path = std::env::var("OXBASE_FIXTURES")
        .ok()
        .filter(|p| !p.is_empty())
        .map_or_else(
            || workspace_root().join("tests/fixtures.toml"),
            |p| {
                let p = PathBuf::from(p);
                if p.is_absolute() {
                    p
                } else {
                    workspace_root().join(p)
                }
            },
        );
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.parse().expect("fixtures.toml parses")
}

fn get<'a>(t: &'a toml::Table, path: &[&str]) -> &'a toml::Value {
    let mut table = t;
    let mut out = None;
    for (i, key) in path.iter().enumerate() {
        let next = table
            .get(*key)
            .unwrap_or_else(|| panic!("fixtures.toml has no {}", path.join(".")));
        if i + 1 < path.len() {
            table = next.as_table().expect("table");
        }
        out = Some(next);
    }
    out.expect("non-empty path")
}

fn s(t: &toml::Table, path: &[&str]) -> String {
    get(t, path).as_str().expect("string").to_owned()
}

struct Fixture {
    doc: toml::Table,
    database: String,
    schema: String,
    other_database: String,
    other_table: String,
}

fn fixture() -> Fixture {
    let doc = fixtures();
    Fixture {
        database: s(&doc, &["m2", "mssql", "database"]),
        schema: s(&doc, &["m2", "mssql", "schema"]),
        other_database: s(&doc, &["m2", "mssql", "other_database"]),
        other_table: s(&doc, &["m2", "mssql", "other_table"]),
        doc,
    }
}

impl Fixture {
    fn m2(&self, key: &str) -> String {
        s(&self.doc, &["m2", key])
    }

    fn m4(&self, key: &str) -> String {
        s(&self.doc, &["m4", key])
    }

    fn params(&self, database: &str) -> ConnectParams {
        let port = get(&self.doc, &["engines", "mssql", "port"])
            .as_integer()
            .expect("port");
        ConnectParams::network(Engine::MsSql, s(&self.doc, &["engines", "mssql", "host"]))
            .with_port(u16::try_from(port).expect("port"))
            .with_user(s(&self.doc, &["engines", "mssql", "user"]))
            .with_password(s(&self.doc, &["engines", "mssql", "password"]))
            .with_database(database)
            .with_tls(TlsOptions::new(TlsMode::Disable))
    }

    fn database_ref(&self) -> ObjectRef {
        ObjectRef::database(&self.database)
    }

    fn schema_ref(&self) -> ObjectRef {
        self.database_ref().child(ObjectKind::Schema, &self.schema)
    }

    fn m4_schema_ref(&self) -> ObjectRef {
        self.database_ref().child(ObjectKind::Schema, M4)
    }
}

async fn connect(p: &ConnectParams) -> MsSqlConnection {
    MsSqlDriver::new()
        .connect_mssql(p)
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"))
}

async fn run(c: &dyn Connection, sql: &str) {
    let stream = c
        .execute(sql, &[], CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    collect(stream)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// Runs `sql`, ignoring errors (cleanup of objects that may not exist).
async fn try_run(c: &dyn Connection, sql: &str) {
    if let Ok(stream) = c.execute(sql, &[], CancellationToken::new()).await {
        let _ = collect(stream).await;
    }
}

fn fetcher(kind: ObjectKind) -> Arc<dyn Fetcher> {
    let mut reg = DialectRegistry::new();
    oxbase_mssql::dialect::register(&mut reg);
    reg.fetcher(Engine::MsSql, kind)
        .unwrap_or_else(|e| panic!("{kind}: {e}"))
}

async fn try_list(
    c: &dyn Connection,
    kind: ObjectKind,
    parent: &ObjectRef,
) -> Result<Vec<ObjectSummary>, Error> {
    fetcher(kind)
        .list(c, parent, CancellationToken::new())
        .await
}

async fn list(c: &dyn Connection, kind: ObjectKind, parent: &ObjectRef) -> Vec<ObjectSummary> {
    try_list(c, kind, parent)
        .await
        .unwrap_or_else(|e| panic!("list {kind} under {parent}: {e}"))
}

async fn try_load(c: &dyn Connection, obj: &ObjectRef) -> Result<ObjectDef, Error> {
    fetcher(obj.kind)
        .load(c, obj, CancellationToken::new())
        .await
}

async fn load(c: &dyn Connection, obj: &ObjectRef) -> ObjectDef {
    try_load(c, obj)
        .await
        .unwrap_or_else(|e| panic!("load {obj}: {e}"))
}

fn names(items: &[ObjectSummary]) -> Vec<String> {
    items.iter().map(|s| s.obj.name.clone()).collect()
}

fn find<'a>(items: &'a [ObjectSummary], name: &str) -> &'a ObjectSummary {
    items
        .iter()
        .find(|s| s.obj.name == name)
        .unwrap_or_else(|| panic!("{name} not listed in {:?}", names(items)))
}

fn text(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
}

fn assert_sorted(items: &[ObjectSummary]) {
    let n = names(items);
    let mut sorted = n.clone();
    sorted.sort();
    assert_eq!(n, sorted, "listing is not sorted by name");
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

#[tokio::test]
#[ignore = "needs the mssql fixture with the M2 seed"]
async fn lists_databases_schemas_and_every_seeded_object() {
    let f = fixture();
    let conn = connect(&f.params(&f.database)).await;
    let c: &dyn Connection = &conn;
    assert_eq!(c.current_database().as_deref(), Some(f.database.as_str()));

    let dbs = list(c, ObjectKind::Database, &ObjectRef::root()).await;
    assert_sorted(&dbs);
    let current = find(&dbs, &f.database);
    assert_eq!(current.obj, f.database_ref());
    assert!(!current.system);
    assert_eq!(
        current.extras.get(&props::CURRENT),
        Some(&Value::Bool(true))
    );
    assert_eq!(text(current.extras.get(&x::STATE)), Some("ONLINE"));
    let other = find(&dbs, &f.other_database);
    assert_eq!(other.extras.get(&props::CURRENT), Some(&Value::Bool(false)));
    for system in ["master", "model", "msdb", "tempdb"] {
        assert!(find(&dbs, system).system, "{system}");
    }

    let schemas = list(c, ObjectKind::Schema, &f.database_ref()).await;
    assert_sorted(&schemas);
    let m2 = find(&schemas, &f.schema);
    assert_eq!(m2.obj, f.schema_ref());
    assert!(!m2.system);
    assert_eq!(m2.comment.as_deref(), Some("M2 seed objects"));
    assert!(!find(&schemas, "dbo").system);
    for system in [
        "sys",
        "INFORMATION_SCHEMA",
        "guest",
        "db_owner",
        "db_datareader",
    ] {
        assert!(find(&schemas, system).system, "{system}");
    }

    let schema = f.schema_ref();
    let tables = list(c, ObjectKind::Table, &schema).await;
    let want_tables: Vec<String> = get(&f.doc, &["m2", "tables"])
        .as_array()
        .expect("tables")
        .iter()
        .map(|t| t.as_str().expect("name").to_owned())
        .collect();
    assert_eq!(names(&tables), sorted(want_tables));
    let parent = find(&tables, "m2_parent");
    assert_eq!(parent.comment.as_deref(), Some("M2 parent table"));
    assert!(!parent.system);
    assert_eq!(parent.obj.catalog.as_deref(), Some(f.database.as_str()));
    assert_eq!(parent.obj.schema.as_deref(), Some(f.schema.as_str()));

    let views = list(c, ObjectKind::View, &schema).await;
    assert_eq!(names(&views), vec![f.m2("view")]);
    let functions = list(c, ObjectKind::Function, &schema).await;
    assert_eq!(names(&functions), vec![f.m2("function")]);
    assert_eq!(
        text(functions[0].extras.get(&x::FUNCTION_KIND)),
        Some("scalar")
    );
    assert_eq!(functions[0].comment.as_deref(), Some("Adds one"));
    let procedures = list(c, ObjectKind::Procedure, &schema).await;
    assert_eq!(names(&procedures), vec![f.m2("procedure")]);
    let sequences = list(c, ObjectKind::Sequence, &schema).await;
    assert_eq!(names(&sequences), vec![f.m2("sequence")]);
    assert_eq!(
        names(&list(c, ObjectKind::Type, &schema).await),
        Vec::<String>::new()
    );

    // Every [m2.mssql] object is listed under its parent.
    let objects: Vec<String> = get(&f.doc, &["m2", "mssql", "objects"])
        .as_array()
        .expect("objects")
        .iter()
        .map(|t| t.as_str().expect("name").to_owned())
        .collect();
    let child = schema.child(ObjectKind::Table, f.m2("trigger_table"));
    let triggers = list(c, ObjectKind::Trigger, &child).await;
    assert_eq!(names(&triggers), vec![f.m2("trigger")]);
    let mut listed: Vec<String> = [&tables, &views, &functions, &procedures, &sequences]
        .into_iter()
        .flat_map(|l| names(l.as_slice()))
        .chain(names(&triggers))
        .collect();
    listed.sort();
    assert_eq!(listed, sorted(objects));

    // Table children.
    let cols = list(c, ObjectKind::Column, &child).await;
    assert_eq!(names(&cols), vec!["id", "parent_id", "qty"]);
    assert_eq!(text(cols[0].extras.get(&props::NATIVE_TYPE)), Some("int"));
    assert_eq!(
        cols[0].extras.get(&props::PRIMARY_KEY),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        cols[1].extras.get(&props::PRIMARY_KEY),
        Some(&Value::Bool(false))
    );
    let indexes = list(c, ObjectKind::Index, &child).await;
    assert_sorted(&indexes);
    let idx = find(&indexes, &f.m2("index"));
    assert_eq!(
        idx.extras.get(&props::PRIMARY_KEY),
        Some(&Value::Bool(false))
    );
    assert_eq!(idx.extras.get(&props::CLUSTERED), Some(&Value::Bool(false)));
    let pk = find(&indexes, "m2_child_pkey");
    assert_eq!(pk.extras.get(&props::PRIMARY_KEY), Some(&Value::Bool(true)));
    assert_eq!(pk.extras.get(&props::CLUSTERED), Some(&Value::Bool(true)));
    let fks = list(c, ObjectKind::ForeignKey, &child).await;
    assert_eq!(names(&fks), vec![f.m2("foreign_key")]);
    let checks = list(c, ObjectKind::Check, &child).await;
    assert_eq!(names(&checks), vec![f.m2("check_child")]);
    let view = schema.child(ObjectKind::View, f.m2("view"));
    let view_cols = list(c, ObjectKind::Column, &view).await;
    assert_eq!(names(&view_cols), vec!["id", "qty", "code"]);
    assert_eq!(view_cols[0].obj.parent_kind, Some(ObjectKind::View));
    assert_eq!(
        names(&list(c, ObjectKind::Trigger, &view).await),
        Vec::<String>::new()
    );
    assert_eq!(
        names(&list(c, ObjectKind::Index, &view).await),
        Vec::<String>::new()
    );
}

#[tokio::test]
#[ignore = "needs the mssql fixture with the M2 seed"]
async fn loads_every_seeded_object() {
    let f = fixture();
    let conn = connect(&f.params(&f.database)).await;
    let c: &dyn Connection = &conn;
    let schema = f.schema_ref();

    let ObjectDef::Schema(db) = load(c, &f.database_ref()).await else {
        panic!("not a database");
    };
    assert_eq!(db.obj.kind, ObjectKind::Database);
    assert_eq!(db.owner.as_deref(), Some("sa"));
    assert_eq!(text(db.extras.get(&x::STATE)), Some("ONLINE"));
    assert!(matches!(
        db.extras.get(&x::COMPATIBILITY_LEVEL),
        Some(Value::Int(n)) if *n >= 140
    ));
    assert!(db.extras.get(&x::RECOVERY_MODEL).is_some());
    assert!(db.extras.get(&props::COLLATION).is_some());

    let ObjectDef::Schema(sch) = load(c, &schema).await else {
        panic!("not a schema");
    };
    assert_eq!(sch.owner.as_deref(), Some("dbo"));
    assert_eq!(sch.comment.as_deref(), Some("M2 seed objects"));

    // m2_parent
    let ObjectDef::Table(parent) = load(c, &schema.child(ObjectKind::Table, "m2_parent")).await
    else {
        panic!("not a table");
    };
    assert_eq!(
        parent.comment.as_deref(),
        Some(f.m2("table_comment").as_str())
    );
    let names: Vec<&str> = parent.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["id", "code", "note"]);
    let id = &parent.columns[0];
    assert_eq!(id.native_type, "int");
    assert_eq!(
        id.data_type,
        DataType::Int {
            bits: 32,
            unsigned: false
        }
    );
    assert!(!id.nullable);
    assert!(!id.auto_increment);
    let code = &parent.columns[1];
    assert_eq!(code.native_type, "varchar(20)");
    assert_eq!(code.data_type, DataType::VarChar { length: Some(20) });
    assert_eq!(code.comment.as_deref(), Some("Business code"));
    assert_eq!(code.collation, None);
    let note = &parent.columns[2];
    assert_eq!(note.native_type, "nvarchar(max)");
    assert_eq!(note.data_type, DataType::Text);
    assert!(note.nullable);
    let pk = parent.primary_key.as_ref().expect("primary key");
    assert_eq!(pk.name.as_deref(), Some("m2_parent_pkey"));
    assert_eq!(pk.columns, vec!["id"]);
    assert_eq!(
        parent.extras.get(&props::CLUSTERED),
        Some(&Value::Bool(true))
    );
    assert_eq!(parent.indexes.len(), 1, "{:?}", parent.indexes);
    let uq = &parent.indexes[0];
    assert_eq!(uq.name, "m2_parent_code_key");
    assert!(uq.unique && uq.is_constraint);
    assert_eq!(uq.columns.len(), 1);
    assert_eq!(uq.columns[0].name, "code");
    assert_eq!(parent.checks.len(), 1);
    assert_eq!(
        parent.checks[0].name.as_deref(),
        Some(f.m2("check_parent").as_str())
    );
    assert!(
        parent.checks[0].expression.to_lowercase().contains("len("),
        "{}",
        parent.checks[0].expression
    );
    assert_eq!(
        parent.checks[0].extras.get(&x::NOT_TRUSTED),
        Some(&Value::Bool(false))
    );

    // m2_child
    let child_ref = schema.child(ObjectKind::Table, "m2_child");
    let ObjectDef::Table(child) = load(c, &child_ref).await else {
        panic!("not a table");
    };
    let qty = child.column("qty").expect("qty");
    assert_eq!(qty.default.as_deref(), Some("((1))"));
    assert_eq!(
        text(qty.extras.get(&x::DEFAULT_CONSTRAINT)),
        Some("m2_child_qty_df")
    );
    assert_eq!(child.foreign_keys.len(), 1);
    let fk = &child.foreign_keys[0];
    assert_eq!(fk.name.as_deref(), Some(f.m2("foreign_key").as_str()));
    assert_eq!(fk.columns, vec!["parent_id"]);
    assert_eq!(fk.ref_columns, vec!["id"]);
    assert_eq!(
        fk.ref_table,
        ObjectRef::table(Some(&f.database), Some(&f.schema), "m2_parent")
    );
    assert_eq!(fk.on_delete, FkAction::Cascade);
    assert_eq!(fk.on_update, FkAction::NoAction);
    assert_eq!(child.checks[0].expression, "([qty]>=(0))");
    assert_eq!(child.indexes.len(), 1);
    assert_eq!(child.indexes[0].name, f.m2("index"));
    assert!(!child.indexes[0].unique);
    assert_eq!(
        child.indexes[0].extras.get(&props::INCLUDE_COLUMNS),
        Some(&Value::Json(serde_json::json!([])))
    );

    // Table children on their own.
    let ObjectDef::Column { table, column } =
        load(c, &child_ref.child(ObjectKind::Column, "qty")).await
    else {
        panic!("not a column");
    };
    assert_eq!(table, child_ref);
    assert_eq!(column, *qty);
    let ObjectDef::Index { index, .. } =
        load(c, &child_ref.child(ObjectKind::Index, "m2_child_pkey")).await
    else {
        panic!("not an index");
    };
    assert_eq!(
        index.extras.get(&props::PRIMARY_KEY),
        Some(&Value::Bool(true))
    );
    let ObjectDef::ForeignKey { foreign_key, .. } = load(
        c,
        &child_ref.child(ObjectKind::ForeignKey, f.m2("foreign_key")),
    )
    .await
    else {
        panic!("not a foreign key");
    };
    assert_eq!(&foreign_key, fk);
    let ObjectDef::Check { check, .. } =
        load(c, &child_ref.child(ObjectKind::Check, f.m2("check_child"))).await
    else {
        panic!("not a check");
    };
    assert_eq!(check.expression, "([qty]>=(0))");

    // Trigger
    let ObjectDef::Trigger(trg) =
        load(c, &child_ref.child(ObjectKind::Trigger, f.m2("trigger"))).await
    else {
        panic!("not a trigger");
    };
    assert_eq!(trg.obj.owner(), Some(child_ref.clone()));
    assert_eq!(trg.timing, TriggerTiming::After);
    assert_eq!(trg.events, vec![TriggerEvent::Insert]);
    assert_eq!(trg.level, TriggerLevel::Statement);
    assert!(trg.enabled);
    assert!(
        trg.definition
            .as_deref()
            .is_some_and(|d| d.contains("CREATE TRIGGER")),
        "{:?}",
        trg.definition
    );

    // View
    let ObjectDef::View(view) = load(c, &schema.child(ObjectKind::View, f.m2("view"))).await else {
        panic!("not a view");
    };
    assert!(
        view.definition.starts_with("SELECT c.id, c.qty, p.code"),
        "{}",
        view.definition
    );
    assert_eq!(view.columns, Vec::<String>::new());
    assert_eq!(
        view.extras.get(&x::SCHEMA_BINDING),
        Some(&Value::Bool(false))
    );
    assert_eq!(view.extras.get(&props::CHECK_OPTION), None);

    // Function and procedure
    let ObjectDef::Routine(fun) =
        load(c, &schema.child(ObjectKind::Function, f.m2("function"))).await
    else {
        panic!("not a routine");
    };
    assert_eq!(fun.obj.signature, None);
    assert_eq!(fun.args.len(), 1);
    assert_eq!(fun.args[0].name.as_deref(), Some("@n"));
    assert_eq!(fun.args[0].mode, ArgMode::In);
    assert_eq!(fun.args[0].native_type, "int");
    assert_eq!(fun.args[0].default, None);
    assert_eq!(fun.returns.as_deref(), Some("int"));
    assert_eq!(fun.language.as_deref(), Some("SQL"));
    assert_eq!(fun.body, None);
    assert_eq!(fun.comment.as_deref(), Some("Adds one"));
    assert_eq!(text(fun.extras.get(&x::FUNCTION_KIND)), Some("scalar"));
    assert!(
        fun.definition
            .as_deref()
            .is_some_and(|d| d.contains("CREATE FUNCTION"))
    );
    let ObjectDef::Routine(proc) =
        load(c, &schema.child(ObjectKind::Procedure, f.m2("procedure"))).await
    else {
        panic!("not a routine");
    };
    assert_eq!(proc.obj.kind, ObjectKind::Procedure);
    assert_eq!(proc.returns, None);
    assert_eq!(proc.args.len(), 1);
    assert_eq!(proc.args[0].name.as_deref(), Some("@n"));
    assert_eq!(proc.args[0].native_type, "int");
    // A function ref does not load a procedure.
    assert!(matches!(
        try_load(c, &schema.child(ObjectKind::Function, f.m2("procedure"))).await,
        Err(Error::NotFound { .. })
    ));

    // Sequence
    let ObjectDef::Sequence(seq) =
        load(c, &schema.child(ObjectKind::Sequence, f.m2("sequence"))).await
    else {
        panic!("not a sequence");
    };
    assert_eq!(seq.start, 100);
    assert_eq!(seq.increment, 5);
    assert_eq!(seq.data_type.as_deref(), Some("bigint"));
    assert_eq!(text(seq.extras.get(&props::DATA_TYPE)), Some("bigint"));
    assert_eq!(seq.max_value, Some(i128::from(i64::MAX)));
    assert!(!seq.cycle);
    assert_eq!(seq.owned_by, None);
}

#[tokio::test]
#[ignore = "needs the mssql fixture with the M2 seed"]
async fn other_databases_need_their_own_session() {
    let f = fixture();
    let conn = connect(&f.params(&f.database)).await;
    let c: &dyn Connection = &conn;
    let other = ObjectRef::database(&f.other_database);
    let dbo = other.child(ObjectKind::Schema, "dbo");

    // From a session on oxbase_test: InvalidInput, never the wrong catalog.
    assert!(matches!(
        try_list(c, ObjectKind::Schema, &other).await,
        Err(Error::InvalidInput { .. })
    ));
    assert!(matches!(
        try_list(c, ObjectKind::Table, &dbo).await,
        Err(Error::InvalidInput { .. })
    ));
    assert!(matches!(
        try_load(c, &dbo.child(ObjectKind::Table, &f.other_table)).await,
        Err(Error::InvalidInput { .. })
    ));
    // The database itself loads from any session.
    let ObjectDef::Schema(db) = load(c, &other).await else {
        panic!("not a database");
    };
    assert_eq!(db.extras.get(&props::CURRENT), Some(&Value::Bool(false)));

    // A session on the other database reads it.
    let other_conn = connect(&f.params(&f.other_database)).await;
    let oc: &dyn Connection = &other_conn;
    let tables = list(oc, ObjectKind::Table, &dbo).await;
    assert_eq!(names(&tables), vec![f.other_table.clone()]);
    assert_eq!(
        tables[0].obj.catalog.as_deref(),
        Some(f.other_database.as_str())
    );
    let ObjectDef::Table(t) = load(oc, &dbo.child(ObjectKind::Table, &f.other_table)).await else {
        panic!("not a table");
    };
    assert_eq!(
        t.primary_key.map(|pk| pk.columns),
        Some(vec!["id".to_owned()])
    );
    // A ref without a catalog means the session's database.
    let bare = ObjectRef::table(None, Some("dbo"), &f.other_table);
    assert!(try_load(oc, &bare).await.is_ok());
}

#[tokio::test]
#[ignore = "needs the mssql fixture with the M2 seed"]
async fn missing_objects_and_cancelled_tokens() {
    let f = fixture();
    let conn = connect(&f.params(&f.database)).await;
    let c: &dyn Connection = &conn;
    let schema = f.schema_ref();
    for obj in [
        schema.child(ObjectKind::Table, "no_such_table"),
        schema.child(ObjectKind::View, "no_such_view"),
        schema.child(ObjectKind::Function, "no_such_fn"),
        schema.child(ObjectKind::Procedure, "no_such_proc"),
        schema.child(ObjectKind::Sequence, "no_such_seq"),
        schema.child(ObjectKind::Type, "no_such_type"),
        schema
            .child(ObjectKind::Table, "m2_parent")
            .child(ObjectKind::Column, "no_such_column"),
        schema
            .child(ObjectKind::Table, "m2_parent")
            .child(ObjectKind::Index, "no_such_index"),
        schema
            .child(ObjectKind::Table, "m2_parent")
            .child(ObjectKind::Trigger, "no_such_trigger"),
        f.database_ref().child(ObjectKind::Schema, "no_such_schema"),
        ObjectRef::database("no_such_database"),
    ] {
        match try_load(c, &obj).await {
            Err(Error::NotFound { .. }) => {}
            other => panic!("load {obj}: expected NotFound, got {other:?}"),
        }
    }
    // A table ref does not load a view.
    assert!(matches!(
        try_load(c, &schema.child(ObjectKind::Table, f.m2("view"))).await,
        Err(Error::NotFound { .. })
    ));
    // Children of a missing table.
    assert!(matches!(
        try_list(
            c,
            ObjectKind::Column,
            &schema.child(ObjectKind::Table, "no_such_table")
        )
        .await,
        Err(Error::NotFound { .. })
    ));
    // Wrong parents.
    assert!(matches!(
        try_list(c, ObjectKind::Table, &f.database_ref()).await,
        Err(Error::InvalidInput { .. })
    ));
    assert!(matches!(
        try_list(c, ObjectKind::Database, &f.database_ref()).await,
        Err(Error::InvalidInput { .. })
    ));

    // A cancelled token: Cancelled, nothing partial, and the session still works.
    let token = CancellationToken::new();
    token.cancel();
    for kind in [ObjectKind::Table, ObjectKind::View, ObjectKind::Function] {
        assert!(matches!(
            fetcher(kind).list(c, &schema, token.clone()).await,
            Err(Error::Cancelled)
        ));
    }
    assert!(matches!(
        fetcher(ObjectKind::Table)
            .load(
                c,
                &schema.child(ObjectKind::Table, "m2_parent"),
                token.clone()
            )
            .await,
        Err(Error::Cancelled)
    ));
    assert_eq!(list(c, ObjectKind::View, &schema).await.len(), 1);
}

#[tokio::test]
#[ignore = "needs the mssql fixture with the M2 seed"]
async fn cancel_stops_a_blocked_catalog_query() {
    let f = fixture();
    // Session A creates a table in an open transaction: its catalog rows stay locked, so a
    // listing of that schema in session B waits on them.
    let blocker = connect(&f.params(&f.database)).await;
    run(
        &blocker,
        "BEGIN TRANSACTION; CREATE TABLE m2.m4_fetch_live_blocker (id int);",
    )
    .await;

    let conn = connect(&f.params(&f.database)).await;
    let token = CancellationToken::new();
    let schema = f.schema_ref();
    let fetch = fetcher(ObjectKind::Table);
    let canceller = token.clone();
    let cancel_at = Duration::from_millis(500);
    tokio::spawn(async move {
        tokio::time::sleep(cancel_at).await;
        canceller.cancel();
    });
    let started = Instant::now();
    let out = tokio::time::timeout(
        Duration::from_secs(20),
        fetch.list(&conn, &schema, token.clone()),
    )
    .await
    .expect("the cancelled listing did not end within 20 s");
    let elapsed = started.elapsed();
    try_run(&blocker, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await;
    match out {
        Err(Error::Cancelled) => {}
        Ok(items) => panic!(
            "the listing was not blocked (finished in {elapsed:?} with {:?})",
            names(&items)
        ),
        Err(e) => panic!("expected Cancelled, got {e}"),
    }
    assert!(
        elapsed < cancel_at + Duration::from_secs(2),
        "the cancel took {elapsed:?}"
    );
    // The lock is gone: a fresh session lists the schema without the rolled-back table.
    let fresh = connect(&f.params(&f.database)).await;
    let tables = list(&fresh, ObjectKind::Table, &schema).await;
    assert!(!names(&tables).iter().any(|n| n == "m4_fetch_live_blocker"));
}

// ---------------------------------------------------------------------------------------------
// M4 shapes

/// Creates the M4 shapes in schema [`M4`] (names of `[m4]`), each statement its own batch.
fn m4_setup(f: &Fixture) -> Vec<String> {
    let s = M4;
    let features = f.m4("features_table");
    let temporal = f.m4("temporal_table");
    let history = f.m4("temporal_history");
    let iview = f.m4("indexed_view");
    let iview_ix = f.m4("indexed_view_index");
    let view_trg = f.m4("view_trigger");
    let itvf = f.m4("inline_tvf");
    let mtvf = f.m4("multi_tvf");
    let alias = f.m4("alias_type");
    let table_type = f.m4("table_type");
    let tvp = f.m4("tvp_procedure");
    vec![
        format!("CREATE SCHEMA {s}"),
        format!("CREATE TYPE {s}.{alias} FROM varchar(20) NOT NULL"),
        format!(
            "CREATE TYPE {s}.{table_type} AS TABLE (id int NOT NULL PRIMARY KEY, \
             name nvarchar(50) NULL, CHECK (id > 0))"
        ),
        format!(
            "CREATE TABLE {s}.{features} (\
               id int IDENTITY(100, 5) NOT NULL, \
               code varchar(20) NULL, \
               qty int NOT NULL CONSTRAINT df_m4_features_qty DEFAULT (1), \
               price decimal(10,2) NOT NULL, \
               total AS (qty * price) PERSISTED, \
               label AS (upper(code)), \
               rv rowversion, \
               status nvarchar(10) NOT NULL CONSTRAINT df_m4_features_status DEFAULT (N'new'), \
               tag {s}.{alias} NULL, \
               CONSTRAINT pk_m4_features PRIMARY KEY CLUSTERED (id), \
               CONSTRAINT ck_m4_features_qty CHECK (qty >= 0))"
        ),
        format!(
            "CREATE UNIQUE INDEX m4_features_code_uq ON {s}.{features} (code) \
             WHERE code IS NOT NULL"
        ),
        format!(
            "CREATE NONCLUSTERED INDEX m4_features_qty_ix ON {s}.{features} (qty DESC) \
             INCLUDE (price)"
        ),
        format!(
            "EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'M4 features', \
             @level0type = N'SCHEMA', @level0name = N'{s}', @level1type = N'TABLE', \
             @level1name = N'{features}'"
        ),
        format!(
            "EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'how many', \
             @level0type = N'SCHEMA', @level0name = N'{s}', @level1type = N'TABLE', \
             @level1name = N'{features}', @level2type = N'COLUMN', @level2name = N'qty'"
        ),
        format!(
            "CREATE TABLE {s}.m4_fetch_child (id int NOT NULL CONSTRAINT pk_m4_fetch_child \
             PRIMARY KEY NONCLUSTERED, f_id int NULL CONSTRAINT fk_m4_fetch_child FOREIGN KEY \
             REFERENCES {s}.{features} (id) ON DELETE SET NULL ON UPDATE NO ACTION)"
        ),
        format!(
            "CREATE TABLE {s}.{temporal} (\
               id int NOT NULL CONSTRAINT pk_m4_temporal PRIMARY KEY, \
               val nvarchar(20) NULL, \
               valid_from datetime2 GENERATED ALWAYS AS ROW START HIDDEN NOT NULL, \
               valid_to datetime2 GENERATED ALWAYS AS ROW END HIDDEN NOT NULL, \
               PERIOD FOR SYSTEM_TIME (valid_from, valid_to)) \
             WITH (SYSTEM_VERSIONING = ON (HISTORY_TABLE = {s}.{history}))"
        ),
        format!(
            "CREATE VIEW {s}.{iview} WITH SCHEMABINDING AS SELECT id, qty FROM {s}.{features} \
             WHERE qty > 0"
        ),
        format!("CREATE UNIQUE CLUSTERED INDEX {iview_ix} ON {s}.{iview} (id)"),
        format!(
            "CREATE TRIGGER {s}.{view_trg} ON {s}.{iview} INSTEAD OF INSERT AS \
             BEGIN SET NOCOUNT ON; END"
        ),
        format!(
            "/* leading comment AS */ CREATE VIEW {s}.m4_fetch_cols_v (a, [b c]) AS -- body\n\
             SELECT id, qty FROM {s}.{features} WITH CHECK OPTION"
        ),
        format!(
            "CREATE FUNCTION {s}.{itvf}(@n int) RETURNS TABLE AS RETURN \
             SELECT id, qty FROM {s}.{features} WHERE id > @n"
        ),
        format!(
            "CREATE FUNCTION {s}.{mtvf}(@n int) RETURNS @t TABLE (n int) AS \
             BEGIN INSERT INTO @t VALUES (@n); RETURN; END"
        ),
        format!(
            "CREATE PROCEDURE {s}.{tvp} @rows {s}.{table_type} READONLY, @out int OUTPUT AS \
             BEGIN SELECT @out = COUNT(*) FROM @rows; END"
        ),
        format!(
            "CREATE SEQUENCE {s}.m4_fetch_seq AS decimal(20,0) START WITH 10 INCREMENT BY -2 \
             MINVALUE -100 MAXVALUE 1000 CYCLE CACHE 20"
        ),
        format!(
            "CREATE TRIGGER {DDL_TRIGGER} ON DATABASE FOR CREATE_SYNONYM AS \
             BEGIN SET NOCOUNT ON; END"
        ),
    ]
}

/// Drops everything [`m4_setup`] creates; each statement may fail (object missing).
fn m4_teardown(f: &Fixture) -> Vec<String> {
    let s = M4;
    let temporal = f.m4("temporal_table");
    vec![
        format!(
            "IF EXISTS (SELECT 1 FROM sys.triggers WHERE parent_class = 0 AND name = \
             N'{DDL_TRIGGER}') DROP TRIGGER {DDL_TRIGGER} ON DATABASE"
        ),
        format!("DROP PROCEDURE IF EXISTS {s}.{}", f.m4("tvp_procedure")),
        format!("DROP FUNCTION IF EXISTS {s}.{}", f.m4("inline_tvf")),
        format!("DROP FUNCTION IF EXISTS {s}.{}", f.m4("multi_tvf")),
        format!("DROP VIEW IF EXISTS {s}.m4_fetch_cols_v"),
        format!("DROP VIEW IF EXISTS {s}.{}", f.m4("indexed_view")),
        format!("DROP TABLE IF EXISTS {s}.m4_fetch_child"),
        format!("ALTER TABLE {s}.{temporal} SET (SYSTEM_VERSIONING = OFF)"),
        format!("DROP TABLE IF EXISTS {s}.{temporal}"),
        format!("DROP TABLE IF EXISTS {s}.{}", f.m4("temporal_history")),
        format!("DROP TABLE IF EXISTS {s}.{}", f.m4("features_table")),
        format!("DROP SEQUENCE IF EXISTS {s}.m4_fetch_seq"),
        format!("DROP TYPE IF EXISTS {s}.{}", f.m4("table_type")),
        format!("DROP TYPE IF EXISTS {s}.{}", f.m4("alias_type")),
        format!("DROP SCHEMA IF EXISTS {s}"),
    ]
}

async fn m4_drop(c: &dyn Connection, f: &Fixture) {
    for sql in m4_teardown(f) {
        try_run(c, &sql).await;
    }
}

#[tokio::test]
#[ignore = "needs the mssql fixture"]
async fn m4_shapes_list_and_load() {
    let f = fixture();
    let conn = connect(&f.params(&f.database)).await;
    let c: &dyn Connection = &conn;
    m4_drop(c, &f).await;
    for sql in m4_setup(&f) {
        run(c, &sql).await;
    }
    // Assertions run in a task so the objects are dropped even when one fails.
    let outcome = {
        let f2 = fixture();
        let params = f.params(&f.database);
        tokio::spawn(async move {
            let conn = connect(&params).await;
            m4_assertions(&conn, &f2).await;
        })
        .await
    };
    m4_drop(c, &f).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn m4_assertions(c: &dyn Connection, f: &Fixture) {
    let schema = f.m4_schema_ref();
    let features_ref = schema.child(ObjectKind::Table, f.m4("features_table"));

    // Listings.
    let tables = list(c, ObjectKind::Table, &schema).await;
    assert_sorted(&tables);
    let temporal = find(&tables, &f.m4("temporal_table"));
    assert_eq!(
        text(temporal.extras.get(&x::TEMPORAL_TYPE)),
        Some("system_versioned")
    );
    let history = find(&tables, &f.m4("temporal_history"));
    assert_eq!(text(history.extras.get(&x::TEMPORAL_TYPE)), Some("history"));
    assert_eq!(
        find(&tables, &f.m4("features_table")).comment.as_deref(),
        Some("M4 features")
    );

    let functions = list(c, ObjectKind::Function, &schema).await;
    assert_eq!(
        text(
            find(&functions, &f.m4("inline_tvf"))
                .extras
                .get(&x::FUNCTION_KIND)
        ),
        Some("inline_table")
    );
    assert_eq!(
        text(
            find(&functions, &f.m4("multi_tvf"))
                .extras
                .get(&x::FUNCTION_KIND)
        ),
        Some("table")
    );
    let types = list(c, ObjectKind::Type, &schema).await;
    assert_eq!(
        names(&types),
        sorted(vec![f.m4("alias_type"), f.m4("table_type")])
    );
    assert_eq!(
        text(find(&types, &f.m4("alias_type")).extras.get(&x::TYPE_KIND)),
        Some("alias")
    );
    assert_eq!(
        text(find(&types, &f.m4("table_type")).extras.get(&x::TYPE_KIND)),
        Some("table")
    );
    let sequences = list(c, ObjectKind::Sequence, &schema).await;
    assert_eq!(names(&sequences), vec!["m4_fetch_seq"]);

    // The database DDL trigger is under no table or view.
    for owner in tables.iter().map(|t| t.obj.clone()).chain(
        list(c, ObjectKind::View, &schema)
            .await
            .into_iter()
            .map(|v| v.obj),
    ) {
        let triggers = list(c, ObjectKind::Trigger, &owner).await;
        assert!(
            !names(&triggers).contains(&DDL_TRIGGER.to_owned()),
            "{owner}"
        );
    }

    // m4_features
    let ObjectDef::Table(t) = load(c, &features_ref).await else {
        panic!("not a table");
    };
    assert_eq!(t.comment.as_deref(), Some("M4 features"));
    let id = t.column("id").expect("id");
    assert!(id.auto_increment);
    let identity = id.identity.as_ref().expect("identity");
    assert!(identity.always);
    assert_eq!(identity.start, Some(100));
    assert_eq!(identity.increment, Some(5));
    let qty = t.column("qty").expect("qty");
    assert_eq!(qty.default.as_deref(), Some("((1))"));
    assert_eq!(
        text(qty.extras.get(&x::DEFAULT_CONSTRAINT)),
        Some("df_m4_features_qty")
    );
    assert_eq!(qty.comment.as_deref(), Some("how many"));
    let price = t.column("price").expect("price");
    assert_eq!(price.native_type, "decimal(10,2)");
    assert_eq!(
        price.data_type,
        DataType::Decimal {
            precision: Some(10),
            scale: Some(2)
        }
    );
    let total = t.column("total").expect("total");
    let generated = total.generated.as_ref().expect("computed");
    assert!(generated.stored);
    assert!(
        generated.expression.contains("[qty]"),
        "{}",
        generated.expression
    );
    let label = t.column("label").expect("label");
    assert!(!label.generated.as_ref().expect("computed").stored);
    let rv = t.column("rv").expect("rv");
    assert_eq!(rv.native_type, "rowversion");
    assert_eq!(rv.data_type, DataType::Binary { length: Some(8) });
    assert_eq!(rv.extras.get(&x::ROW_VERSION), Some(&Value::Bool(true)));
    let status = t.column("status").expect("status");
    assert_eq!(status.native_type, "nvarchar(10)");
    assert_eq!(status.default.as_deref(), Some("(N'new')"));
    assert_eq!(
        text(status.extras.get(&x::DEFAULT_CONSTRAINT)),
        Some("df_m4_features_status")
    );
    let tag = t.column("tag").expect("tag");
    assert_eq!(tag.native_type, format!("{M4}.{}", f.m4("alias_type")));
    assert_eq!(tag.data_type, DataType::VarChar { length: Some(20) });
    assert!(tag.nullable);
    let pk = t.primary_key.as_ref().expect("primary key");
    assert_eq!(pk.name.as_deref(), Some("pk_m4_features"));
    assert_eq!(t.extras.get(&props::CLUSTERED), Some(&Value::Bool(true)));
    assert_eq!(
        t.indexes
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        vec!["m4_features_code_uq", "m4_features_qty_ix"]
    );
    let uq = &t.indexes[0];
    assert!(uq.unique);
    assert!(!uq.is_constraint);
    assert_eq!(uq.predicate.as_deref(), Some("([code] IS NOT NULL)"));
    assert_eq!(uq.extras.get(&props::CLUSTERED), Some(&Value::Bool(false)));
    assert_eq!(text(uq.extras.get(&x::INDEX_KIND)), Some("nonclustered"));
    let ix = &t.indexes[1];
    assert_eq!(ix.columns.len(), 1);
    assert_eq!(ix.columns[0].name, "qty");
    assert!(ix.columns[0].descending);
    assert_eq!(
        ix.extras.get(&props::INCLUDE_COLUMNS),
        Some(&Value::Json(serde_json::json!(["price"])))
    );
    assert_eq!(t.checks.len(), 1);
    assert_eq!(t.checks[0].expression, "([qty]>=(0))");

    // The foreign key with SET NULL, and a nonclustered primary key.
    let ObjectDef::Table(child) = load(c, &schema.child(ObjectKind::Table, "m4_fetch_child")).await
    else {
        panic!("not a table");
    };
    assert_eq!(
        child.extras.get(&props::CLUSTERED),
        Some(&Value::Bool(false))
    );
    assert_eq!(child.foreign_keys[0].on_delete, FkAction::SetNull);
    assert_eq!(child.foreign_keys[0].ref_table, features_ref);

    // Temporal table
    let ObjectDef::Table(temporal) =
        load(c, &schema.child(ObjectKind::Table, f.m4("temporal_table"))).await
    else {
        panic!("not a table");
    };
    assert_eq!(
        text(temporal.extras.get(&x::TEMPORAL_TYPE)),
        Some("system_versioned")
    );
    assert_eq!(
        text(temporal.extras.get(&x::HISTORY_TABLE)),
        Some(format!("{M4}.{}", f.m4("temporal_history")).as_str())
    );
    let from = temporal.column("valid_from").expect("valid_from");
    assert_eq!(text(from.extras.get(&x::PERIOD)), Some("row_start"));
    assert_eq!(from.extras.get(&x::HIDDEN), Some(&Value::Bool(true)));
    assert_eq!(from.native_type, "datetime2(7)");
    let to = temporal.column("valid_to").expect("valid_to");
    assert_eq!(text(to.extras.get(&x::PERIOD)), Some("row_end"));

    // Indexed view: its index and its INSTEAD OF trigger list under the view.
    let iview = schema.child(ObjectKind::View, f.m4("indexed_view"));
    let ObjectDef::View(v) = load(c, &iview).await else {
        panic!("not a view");
    };
    assert_eq!(v.extras.get(&x::SCHEMA_BINDING), Some(&Value::Bool(true)));
    assert!(
        v.definition.starts_with("SELECT id, qty"),
        "{}",
        v.definition
    );
    let vix = list(c, ObjectKind::Index, &iview).await;
    assert_eq!(names(&vix), vec![f.m4("indexed_view_index")]);
    assert_eq!(vix[0].obj.parent_kind, Some(ObjectKind::View));
    assert_eq!(
        vix[0].extras.get(&props::CLUSTERED),
        Some(&Value::Bool(true))
    );
    assert_eq!(vix[0].extras.get(&props::UNIQUE), Some(&Value::Bool(true)));
    let ObjectDef::Index { table, index } = load(c, &vix[0].obj).await else {
        panic!("not an index");
    };
    assert_eq!(table, iview);
    assert_eq!(index.columns[0].name, "id");
    let vtrg = list(c, ObjectKind::Trigger, &iview).await;
    assert_eq!(names(&vtrg), vec![f.m4("view_trigger")]);
    let ObjectDef::Trigger(trg) = load(c, &vtrg[0].obj).await else {
        panic!("not a trigger");
    };
    assert_eq!(trg.timing, TriggerTiming::InsteadOf);
    assert_eq!(trg.events, vec![TriggerEvent::Insert]);
    assert_eq!(trg.obj.owner(), Some(iview.clone()));

    // A view with a column list, comments and CHECK OPTION.
    let ObjectDef::View(cv) = load(c, &schema.child(ObjectKind::View, "m4_fetch_cols_v")).await
    else {
        panic!("not a view");
    };
    assert_eq!(cv.columns, vec!["a".to_owned(), "b c".to_owned()]);
    assert!(cv.definition.starts_with("-- body\n"), "{}", cv.definition);
    assert!(
        cv.definition.ends_with("WITH CHECK OPTION"),
        "{}",
        cv.definition
    );
    assert_eq!(text(cv.extras.get(&props::CHECK_OPTION)), Some("cascaded"));

    // Table-valued functions
    let ObjectDef::Routine(itvf) =
        load(c, &schema.child(ObjectKind::Function, f.m4("inline_tvf"))).await
    else {
        panic!("not a routine");
    };
    assert_eq!(itvf.returns.as_deref(), Some("TABLE"));
    assert_eq!(
        text(itvf.extras.get(&x::FUNCTION_KIND)),
        Some("inline_table")
    );
    assert_eq!(itvf.args.len(), 1);
    assert_eq!(itvf.args[0].name.as_deref(), Some("@n"));
    let ObjectDef::Routine(mtvf) =
        load(c, &schema.child(ObjectKind::Function, f.m4("multi_tvf"))).await
    else {
        panic!("not a routine");
    };
    assert_eq!(mtvf.returns.as_deref(), Some("TABLE"));
    assert_eq!(text(mtvf.extras.get(&x::FUNCTION_KIND)), Some("table"));
    assert_eq!(mtvf.args.len(), 1);

    // Procedure with a table-valued parameter and an OUTPUT parameter.
    let ObjectDef::Routine(tvp) = load(
        c,
        &schema.child(ObjectKind::Procedure, f.m4("tvp_procedure")),
    )
    .await
    else {
        panic!("not a routine");
    };
    assert_eq!(tvp.args.len(), 2);
    assert_eq!(tvp.args[0].name.as_deref(), Some("@rows"));
    assert_eq!(
        tvp.args[0].native_type,
        format!("{M4}.{}", f.m4("table_type"))
    );
    assert_eq!(tvp.args[1].mode, ArgMode::InOut);
    assert_eq!(
        tvp.extras.get(&x::READONLY_ARGS),
        Some(&Value::Json(serde_json::json!(["@rows"])))
    );

    // Types
    let ObjectDef::Generic(alias) =
        load(c, &schema.child(ObjectKind::Type, f.m4("alias_type"))).await
    else {
        panic!("not a generic definition");
    };
    assert_eq!(
        alias.definition.as_deref(),
        Some(
            format!(
                "CREATE TYPE [{M4}].[{}] FROM varchar(20) NOT NULL",
                f.m4("alias_type")
            )
            .as_str()
        )
    );
    assert_eq!(text(alias.extras.get(&x::BASE_TYPE)), Some("varchar(20)"));
    assert_eq!(alias.extras.get(&x::NOT_NULL), Some(&Value::Bool(true)));
    let ObjectDef::Generic(tt) = load(c, &schema.child(ObjectKind::Type, f.m4("table_type"))).await
    else {
        panic!("not a generic definition");
    };
    let def = tt.definition.as_deref().expect("definition");
    assert!(
        def.starts_with(&format!(
            "CREATE TYPE [{M4}].[{}] AS TABLE (",
            f.m4("table_type")
        )),
        "{def}"
    );
    assert!(def.contains("[id] int NOT NULL"), "{def}");
    assert!(def.contains("[name] nvarchar(50) NULL"), "{def}");
    assert!(def.contains("PRIMARY KEY"), "{def}");
    assert!(def.contains("CHECK ([id]>(0))"), "{def}");
    assert_eq!(text(tt.extras.get(&x::TYPE_KIND)), Some("table"));

    // Sequence
    let ObjectDef::Sequence(seq) =
        load(c, &schema.child(ObjectKind::Sequence, "m4_fetch_seq")).await
    else {
        panic!("not a sequence");
    };
    assert_eq!(seq.data_type.as_deref(), Some("decimal(20,0)"));
    assert_eq!(seq.start, 10);
    assert_eq!(seq.increment, -2);
    assert_eq!(seq.min_value, Some(-100));
    assert_eq!(seq.max_value, Some(1000));
    assert!(seq.cycle);
    assert_eq!(seq.cache, Some(20));

    // System objects: the sys schema's objects are flagged, user objects are not.
    assert!(!tables.iter().any(|t| t.system));
}
