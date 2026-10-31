//! Live tests of the PostgreSQL dialect cells (M2) against the `postgres` fixture and its M2 seed
//! (`tests/docker/init/postgres/04-m2.sql`, names in `tests/fixtures.toml` `[m2]`).
//!
//! All are `#[ignore]`d: run them with
//! `cargo test -p oxbase-pg --test dialect_live -- --include-ignored --test-threads 1`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use oxbase_dialect::{DialectRegistry, Fetcher};
use oxbase_driver::{CancellationToken, ConnectParams, Connection, Driver, TlsMode, TlsOptions};
use oxbase_pg::PgDriver;
use oxbase_types::{
    ArgMode, DataType, Engine, Error, FkAction, ObjectDef, ObjectKind, ObjectRef, ObjectSummary,
    TriggerEvent, TriggerLevel, TriggerTiming, Value, props,
};

struct Fixture {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
    schema: String,
    other_database: String,
    other_table: String,
    objects: Vec<String>,
    m2: toml::Table,
}

fn fixture() -> Fixture {
    let path = PathBuf::from(std::env::var("OXBASE_FIXTURES").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures.toml").into()
    }));
    let text = std::fs::read_to_string(&path).expect("read tests/fixtures.toml");
    let doc: toml::Table = text.parse().expect("parse tests/fixtures.toml");
    let pg = doc["engines"]["postgres"]
        .as_table()
        .expect("[engines.postgres]");
    let m2 = doc["m2"].as_table().expect("[m2]").clone();
    let m2pg = m2["postgres"].as_table().expect("[m2.postgres]").clone();
    let s = |t: &toml::Table, k: &str| t[k].as_str().expect(k).to_owned();
    Fixture {
        host: s(pg, "host"),
        port: u16::try_from(pg["port"].as_integer().expect("port")).expect("port"),
        user: s(pg, "user"),
        password: s(pg, "password"),
        database: s(&m2pg, "database"),
        schema: s(&m2pg, "schema"),
        other_database: s(&m2pg, "other_database"),
        other_table: s(&m2pg, "other_table"),
        objects: m2pg["objects"]
            .as_array()
            .expect("objects")
            .iter()
            .map(|v| v.as_str().expect("object name").to_owned())
            .collect(),
        m2,
    }
}

impl Fixture {
    fn name(&self, key: &str) -> String {
        self.m2[key].as_str().expect(key).to_owned()
    }

    fn params(&self, database: &str) -> ConnectParams {
        ConnectParams::network(Engine::Postgres, &self.host)
            .with_port(self.port)
            .with_user(&self.user)
            .with_password(self.password.as_str())
            .with_database(database)
            .with_tls(TlsOptions::new(TlsMode::Disable))
    }

    fn database_ref(&self) -> ObjectRef {
        ObjectRef::database(&self.database)
    }

    fn schema_ref(&self) -> ObjectRef {
        self.database_ref().child(ObjectKind::Schema, &self.schema)
    }

    fn table_ref(&self, name: &str) -> ObjectRef {
        self.schema_ref().child(ObjectKind::Table, name)
    }
}

async fn connect(params: &ConnectParams) -> Box<dyn Connection> {
    match PgDriver.connect(params).await {
        Ok(c) => c,
        Err(e) => panic!("connect failed: {e}"),
    }
}

fn registry() -> DialectRegistry {
    let mut reg = DialectRegistry::new();
    oxbase_pg::dialect::register(&mut reg);
    reg
}

fn fetcher(kind: ObjectKind) -> std::sync::Arc<dyn Fetcher> {
    registry()
        .fetcher(Engine::Postgres, kind)
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

#[tokio::test]
#[ignore = "needs the postgres fixture with the M2 seed"]
async fn lists_databases_schemas_and_every_seeded_object() {
    let f = fixture();
    let conn = connect(&f.params(&f.database)).await;
    assert_eq!(
        conn.current_database().as_deref(),
        Some(f.database.as_str())
    );

    let dbs = list(&*conn, ObjectKind::Database, &ObjectRef::root()).await;
    assert_sorted(&dbs);
    let current = find(&dbs, &f.database);
    assert_eq!(current.obj, f.database_ref());
    assert_eq!(
        current.extras.get(&props::CURRENT),
        Some(&Value::Bool(true))
    );
    assert_eq!(text(current.extras.get(&props::ENCODING)), Some("UTF8"));
    let other = find(&dbs, &f.other_database);
    assert_eq!(other.extras.get(&props::CURRENT), Some(&Value::Bool(false)));
    assert!(!names(&dbs).iter().any(|n| n.starts_with("template")));

    let schemas = list(&*conn, ObjectKind::Schema, &f.database_ref()).await;
    assert_sorted(&schemas);
    let m2 = find(&schemas, &f.schema);
    assert_eq!(m2.obj, f.schema_ref());
    assert!(!m2.system);
    assert_eq!(m2.comment.as_deref(), Some("M2 seed objects"));
    assert!(!find(&schemas, "public").system);
    assert!(find(&schemas, "pg_catalog").system);
    assert!(find(&schemas, "information_schema").system);

    let schema = f.schema_ref();
    let tables = list(&*conn, ObjectKind::Table, &schema).await;
    assert_eq!(
        names(&tables),
        f.m2["tables"].as_array().map_or_else(Vec::new, |a| {
            let mut v: Vec<String> = a.iter().map(|t| t.as_str().unwrap().to_owned()).collect();
            v.sort();
            v
        })
    );
    let parent = find(&tables, "m2_parent");
    assert_eq!(parent.obj, f.table_ref("m2_parent"));
    assert_eq!(parent.obj.catalog.as_deref(), Some(f.database.as_str()));
    assert_eq!(
        parent.comment.as_deref(),
        Some(f.name("table_comment").as_str())
    );
    assert!(!parent.system);

    let views = list(&*conn, ObjectKind::View, &schema).await;
    assert_eq!(names(&views), [f.name("view")]);
    let matview_name = f.m2["postgres"]["matview"].as_str().unwrap().to_owned();
    let mviews = list(&*conn, ObjectKind::MatView, &schema).await;
    assert_eq!(names(&mviews), [matview_name.as_str()]);
    assert_eq!(
        mviews[0].extras.get(&props::POPULATED),
        Some(&Value::Bool(true))
    );
    let sequences = list(&*conn, ObjectKind::Sequence, &schema).await;
    assert_eq!(names(&sequences), [f.name("sequence")]);

    let functions = list(&*conn, ObjectKind::Function, &schema).await;
    assert_sorted(&functions);
    let fn_name = f.name("function");
    let signatures: BTreeSet<String> = functions
        .iter()
        .filter(|s| s.obj.name == fn_name)
        .map(|s| s.obj.signature.clone().expect("routine signature"))
        .collect();
    assert_eq!(
        signatures,
        BTreeSet::from(["n integer".to_owned(), "t text, n integer".to_owned()]),
        "overloads get distinct signatures"
    );
    let trg_fn_name = f.m2["postgres"]["trigger_function"].as_str().unwrap();
    let trg_fn = find(&functions, trg_fn_name);
    assert_eq!(trg_fn.obj.signature.as_deref(), Some(""));
    let procedures = list(&*conn, ObjectKind::Procedure, &schema).await;
    assert_eq!(names(&procedures), [f.name("procedure")]);
    // PostgreSQL 14+ spells out `IN` for procedure arguments.
    let proc_signature = procedures[0].obj.signature.as_deref().unwrap_or_default();
    assert!(
        proc_signature == "n integer" || proc_signature == "IN n integer",
        "{proc_signature}"
    );
    assert_eq!(procedures[0].obj.kind, ObjectKind::Procedure);

    let trigger_table = f.table_ref(&f.name("trigger_table"));
    let triggers = list(&*conn, ObjectKind::Trigger, &trigger_table).await;
    assert_eq!(names(&triggers), [f.name("trigger")]);

    // Every `[m2.postgres].objects` entry is listed under its parent.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for group in [
        &tables,
        &views,
        &mviews,
        &sequences,
        &functions,
        &procedures,
        &triggers,
    ] {
        seen.extend(names(group));
    }
    for object in &f.objects {
        assert!(
            seen.contains(object),
            "{object} is not listed; saw {seen:?}"
        );
    }
    conn.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the postgres fixture with the M2 seed"]
async fn loads_tables_with_columns_keys_and_constraints() {
    let f = fixture();
    let conn = connect(&f.params(&f.database)).await;

    let ObjectDef::Table(parent) = load(&*conn, &f.table_ref("m2_parent")).await else {
        panic!("not a table");
    };
    assert_eq!(parent.obj, f.table_ref("m2_parent"));
    assert_eq!(
        parent.comment.as_deref(),
        Some(f.name("table_comment").as_str())
    );
    let cols: Vec<&str> = parent.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cols, ["id", "code", "note"]);
    let id = parent.column("id").unwrap();
    assert_eq!(id.data_type, DataType::INT32);
    assert_eq!(id.native_type, "integer");
    assert!(!id.nullable);
    assert!(!id.auto_increment);
    let code = parent.column("code").unwrap();
    assert_eq!(code.data_type, DataType::VarChar { length: Some(20) });
    assert_eq!(code.native_type, "character varying(20)");
    assert!(!code.nullable);
    assert_eq!(code.comment.as_deref(), Some("Business code"));
    let note = parent.column("note").unwrap();
    assert_eq!(note.data_type, DataType::Text);
    assert!(note.nullable);
    let pk = parent.primary_key.as_ref().expect("primary key");
    assert_eq!(pk.columns, ["id"]);
    assert_eq!(parent.indexes.len(), 1, "{:?}", parent.indexes);
    let unique = &parent.indexes[0];
    assert!(unique.unique && unique.is_constraint);
    assert_eq!(unique.columns.len(), 1);
    assert_eq!(unique.columns[0].name, "code");
    assert_eq!(parent.foreign_keys.len(), 0, "{:?}", parent.foreign_keys);
    assert_eq!(parent.checks.len(), 1);
    let check = &parent.checks[0];
    assert_eq!(check.name.as_deref(), Some(f.name("check_parent").as_str()));
    assert!(
        check.expression.contains("char_length"),
        "{}",
        check.expression
    );
    assert!(check.expression.contains("> 0"), "{}", check.expression);

    let ObjectDef::Table(child) = load(&*conn, &f.table_ref("m2_child")).await else {
        panic!("not a table");
    };
    let cols: Vec<&str> = child.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cols, ["id", "parent_id", "qty"]);
    assert_eq!(child.column("qty").unwrap().default.as_deref(), Some("1"));
    assert_eq!(child.foreign_keys.len(), 1);
    let fk = &child.foreign_keys[0];
    assert_eq!(fk.name.as_deref(), Some(f.name("foreign_key").as_str()));
    assert_eq!(fk.columns, ["parent_id"]);
    assert_eq!(fk.ref_table, f.table_ref("m2_parent"));
    assert_eq!(fk.ref_columns, ["id"]);
    assert_eq!(fk.on_delete, FkAction::Cascade);
    assert_eq!(fk.on_update, FkAction::NoAction);
    assert_eq!(fk.extras.get(&props::DEFERRABLE), Some(&Value::Bool(false)));
    assert_eq!(child.checks.len(), 1);
    assert_eq!(
        child.checks[0].name.as_deref(),
        Some(f.name("check_child").as_str())
    );
    assert!(child.checks[0].expression.contains("qty >= 0"));
    let index_names: Vec<&str> = child.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(index_names, [f.name("index").as_str()]);
    let idx = &child.indexes[0];
    assert!(!idx.unique && !idx.is_constraint);
    assert_eq!(idx.columns[0].name, "parent_id");
    assert!(!idx.columns[0].is_expression);
    assert_eq!(text(idx.extras.get(&props::INDEX_METHOD)), Some("btree"));

    // Identity and serial-like columns of the M0 schema.
    let public = f.database_ref().child(ObjectKind::Schema, "public");
    let ObjectDef::Table(customers) =
        load(&*conn, &public.child(ObjectKind::Table, "customers")).await
    else {
        panic!("not a table");
    };
    let id = customers.column("id").unwrap();
    assert!(id.auto_increment);
    let identity = id.identity.as_ref().expect("identity column");
    assert!(!identity.always);
    assert_eq!(identity.start, Some(1));
    assert_eq!(identity.increment, Some(1));
    let ObjectDef::Table(orders) = load(&*conn, &public.child(ObjectKind::Table, "orders")).await
    else {
        panic!("not a table");
    };
    let order_no = orders.column("order_no").unwrap();
    assert!(order_no.auto_increment, "{order_no:?}");
    assert!(order_no.identity.is_none());
    assert!(
        order_no
            .default
            .as_deref()
            .is_some_and(|d| d.starts_with("nextval(")),
        "{order_no:?}"
    );
    conn.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the postgres fixture with the M2 seed"]
async fn lists_and_loads_table_children() {
    let f = fixture();
    let conn = connect(&f.params(&f.database)).await;
    let child = f.table_ref("m2_child");

    let columns = list(&*conn, ObjectKind::Column, &child).await;
    assert_eq!(names(&columns), ["id", "parent_id", "qty"]);
    let id = find(&columns, "id");
    assert_eq!(id.obj.parent.as_deref(), Some("m2_child"));
    assert_eq!(id.obj.parent_kind, Some(ObjectKind::Table));
    assert_eq!(id.obj.schema.as_deref(), Some(f.schema.as_str()));
    assert_eq!(text(id.extras.get(&props::NATIVE_TYPE)), Some("integer"));
    assert_eq!(id.extras.get(&props::PRIMARY_KEY), Some(&Value::Bool(true)));
    assert_eq!(
        find(&columns, "qty").extras.get(&props::PRIMARY_KEY),
        Some(&Value::Bool(false))
    );
    let ObjectDef::Column { table, column } =
        load(&*conn, &child.child(ObjectKind::Column, "qty")).await
    else {
        panic!("not a column");
    };
    assert_eq!(table, child);
    assert_eq!(column.default.as_deref(), Some("1"));
    assert!(!column.nullable);

    let indexes = list(&*conn, ObjectKind::Index, &child).await;
    assert_eq!(
        names(&indexes),
        [f.name("index"), "m2_child_pkey".to_owned()]
    );
    assert_eq!(
        find(&indexes, "m2_child_pkey")
            .extras
            .get(&props::PRIMARY_KEY),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        find(&indexes, &f.name("index"))
            .extras
            .get(&props::PRIMARY_KEY),
        Some(&Value::Bool(false))
    );
    let ObjectDef::Index { table, index } =
        load(&*conn, &child.child(ObjectKind::Index, f.name("index"))).await
    else {
        panic!("not an index");
    };
    assert_eq!(table, child);
    assert_eq!(index.columns[0].name, "parent_id");

    let fks = list(&*conn, ObjectKind::ForeignKey, &child).await;
    assert_eq!(names(&fks), [f.name("foreign_key")]);
    let ObjectDef::ForeignKey { foreign_key, .. } = load(&*conn, &fks[0].obj).await else {
        panic!("not a foreign key");
    };
    assert_eq!(foreign_key.on_delete, FkAction::Cascade);

    let checks = list(&*conn, ObjectKind::Check, &child).await;
    assert_eq!(names(&checks), [f.name("check_child")]);
    let ObjectDef::Check { check, .. } = load(&*conn, &checks[0].obj).await else {
        panic!("not a check");
    };
    assert!(check.expression.contains("qty >= 0"));

    let triggers = list(&*conn, ObjectKind::Trigger, &child).await;
    let ObjectDef::Trigger(trg) = load(&*conn, &triggers[0].obj).await else {
        panic!("not a trigger");
    };
    assert_eq!(trg.obj, child.child(ObjectKind::Trigger, f.name("trigger")));
    assert_eq!(trg.timing, TriggerTiming::Before);
    assert_eq!(trg.events, [TriggerEvent::Insert]);
    assert_eq!(trg.level, TriggerLevel::Row);
    assert_eq!(trg.update_columns, Vec::<String>::new());
    assert!(trg.condition.is_none());
    assert!(trg.enabled);
    let trg_fn = f.m2["postgres"]["trigger_function"].as_str().unwrap();
    assert_eq!(
        trg.function,
        Some(
            f.schema_ref()
                .child(ObjectKind::Function, trg_fn)
                .with_signature("")
        )
    );
    let def = trg.definition.as_deref().unwrap_or_default();
    assert!(
        def.starts_with("CREATE TRIGGER m2_trg BEFORE INSERT ON ") && def.contains("m2_child"),
        "{def}"
    );

    // An UPDATE trigger of the M0 schema.
    let customers = f
        .database_ref()
        .child(ObjectKind::Schema, "public")
        .child(ObjectKind::Table, "customers");
    let ObjectDef::Trigger(upd) = load(
        &*conn,
        &customers.child(ObjectKind::Trigger, "trg_customers_updated_at"),
    )
    .await
    else {
        panic!("not a trigger");
    };
    assert_eq!(upd.events, [TriggerEvent::Update]);
    assert_eq!(upd.timing, TriggerTiming::Before);

    // Views: columns and (no) triggers; materialized views: columns and indexes.
    let view = f.schema_ref().child(ObjectKind::View, f.name("view"));
    assert_eq!(
        names(&list(&*conn, ObjectKind::Column, &view).await),
        ["id", "qty", "code"]
    );
    assert_eq!(
        names(&list(&*conn, ObjectKind::Trigger, &view).await),
        Vec::<String>::new()
    );
    let mview = f.schema_ref().child(
        ObjectKind::MatView,
        f.m2["postgres"]["matview"].as_str().unwrap(),
    );
    let mcols = list(&*conn, ObjectKind::Column, &mview).await;
    assert_eq!(names(&mcols), ["parent_id", "count"]);
    assert_eq!(mcols[0].obj.parent_kind, Some(ObjectKind::MatView));
    assert_eq!(
        names(&list(&*conn, ObjectKind::Index, &mview).await),
        ["m2_mview_parent_idx"]
    );
    conn.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the postgres fixture with the M2 seed"]
async fn loads_views_routines_and_sequences() {
    let f = fixture();
    let conn = connect(&f.params(&f.database)).await;
    let schema = f.schema_ref();

    let ObjectDef::View(view) = load(&*conn, &schema.child(ObjectKind::View, f.name("view"))).await
    else {
        panic!("not a view");
    };
    assert_eq!(view.obj.kind, ObjectKind::View);
    assert!(view.definition.starts_with("SELECT"), "{}", view.definition);
    assert!(view.definition.contains("JOIN"), "{}", view.definition);
    assert!(view.definition.contains("m2_parent"), "{}", view.definition);
    assert!(!view.definition.ends_with(';'));
    // A table is not a view.
    let err = try_load(&*conn, &schema.child(ObjectKind::View, "m2_parent"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound { .. }), "{err}");

    let mview_ref = schema.child(
        ObjectKind::MatView,
        f.m2["postgres"]["matview"].as_str().unwrap(),
    );
    let ObjectDef::View(mview) = load(&*conn, &mview_ref).await else {
        panic!("not a materialized view");
    };
    assert_eq!(mview.obj.kind, ObjectKind::MatView);
    assert!(
        mview.definition.contains("count(*)"),
        "{}",
        mview.definition
    );
    assert!(
        mview.definition.contains("GROUP BY"),
        "{}",
        mview.definition
    );
    assert_eq!(
        mview.extras.get(&props::POPULATED),
        Some(&Value::Bool(true))
    );

    let fn_name = f.name("function");
    let fn_int = schema
        .child(ObjectKind::Function, &fn_name)
        .with_signature("n integer");
    let ObjectDef::Routine(r) = load(&*conn, &fn_int).await else {
        panic!("not a routine");
    };
    assert_eq!(r.obj, fn_int);
    assert_eq!(r.args.len(), 1);
    assert_eq!(r.args[0].name.as_deref(), Some("n"));
    assert_eq!(r.args[0].mode, ArgMode::In);
    assert_eq!(r.args[0].native_type, "integer");
    assert_eq!(r.args[0].default, None);
    assert_eq!(r.returns.as_deref(), Some("integer"));
    assert_eq!(r.language.as_deref(), Some("sql"));
    assert!(
        r.body.as_deref().is_some_and(|b| b.contains("n + 1")),
        "{:?}",
        r.body
    );
    let def = r.definition.as_deref().unwrap_or_default();
    assert!(
        def.starts_with("CREATE OR REPLACE FUNCTION m2.m2_fn(n integer)"),
        "{def}"
    );
    assert_eq!(r.comment.as_deref(), Some("Adds one"));
    assert_eq!(text(r.extras.get(&props::VOLATILITY)), Some("immutable"));
    assert_eq!(r.extras.get(&props::STRICT_INPUT), Some(&Value::Bool(true)));
    assert_eq!(text(r.extras.get(&props::SECURITY)), Some("invoker"));

    let fn_text = schema
        .child(ObjectKind::Function, &fn_name)
        .with_signature("t text, n integer");
    let ObjectDef::Routine(r) = load(&*conn, &fn_text).await else {
        panic!("not a routine");
    };
    assert_eq!(r.returns.as_deref(), Some("text"));
    assert_eq!(r.args.len(), 2);
    assert_eq!(r.args[1].name.as_deref(), Some("n"));
    assert_eq!(r.args[1].default.as_deref(), Some("2"));
    assert_eq!(text(r.extras.get(&props::VOLATILITY)), Some("stable"));

    // Without a signature, two overloads are ambiguous; a wrong signature is missing.
    let err = try_load(&*conn, &schema.child(ObjectKind::Function, &fn_name))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput { .. }), "{err}");
    let err = try_load(
        &*conn,
        &schema
            .child(ObjectKind::Function, &fn_name)
            .with_signature("x bigint"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::NotFound { .. }), "{err}");

    let trg_fn = f.m2["postgres"]["trigger_function"].as_str().unwrap();
    let ObjectDef::Routine(r) = load(&*conn, &schema.child(ObjectKind::Function, trg_fn)).await
    else {
        panic!("not a routine");
    };
    assert_eq!(r.obj.signature.as_deref(), Some(""));
    assert_eq!(r.returns.as_deref(), Some("trigger"));
    assert_eq!(r.language.as_deref(), Some("plpgsql"));
    assert_eq!(r.args.len(), 0, "{:?}", r.args);

    let procedures = list(&*conn, ObjectKind::Procedure, &schema).await;
    let proc_ref = procedures[0].obj.clone();
    let ObjectDef::Routine(p) = load(&*conn, &proc_ref).await else {
        panic!("not a routine");
    };
    assert_eq!(p.obj.kind, ObjectKind::Procedure);
    assert_eq!(p.returns, None);
    assert_eq!(p.args.len(), 1);
    assert_eq!(p.args[0].native_type, "integer");
    assert_eq!(p.language.as_deref(), Some("plpgsql"));
    let def = p.definition.as_deref().unwrap_or_default();
    assert!(def.contains("PROCEDURE m2.m2_proc("), "{def}");
    // A function is not a procedure.
    let err = try_load(
        &*conn,
        &schema
            .child(ObjectKind::Procedure, &fn_name)
            .with_signature("n integer"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::NotFound { .. }), "{err}");

    let ObjectDef::Sequence(seq) = load(
        &*conn,
        &schema.child(ObjectKind::Sequence, f.name("sequence")),
    )
    .await
    else {
        panic!("not a sequence");
    };
    assert_eq!(seq.start, 100);
    assert_eq!(seq.increment, 5);
    assert_eq!(seq.data_type.as_deref(), Some("bigint"));
    assert_eq!(seq.min_value, Some(1));
    assert_eq!(seq.max_value, Some(i128::from(i64::MAX)));
    assert_eq!(seq.cache, Some(1));
    assert!(!seq.cycle);
    assert_eq!(seq.owned_by, None);

    let public = f.database_ref().child(ObjectKind::Schema, "public");
    let ObjectDef::Sequence(owned) =
        load(&*conn, &public.child(ObjectKind::Sequence, "order_no_seq")).await
    else {
        panic!("not a sequence");
    };
    assert_eq!(owned.data_type.as_deref(), Some("integer"));
    assert_eq!(owned.start, 1000);
    assert_eq!(
        owned.owned_by,
        Some(
            public
                .child(ObjectKind::Table, "orders")
                .child(ObjectKind::Column, "order_no")
        )
    );
    conn.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the postgres fixture with the M2 seed"]
async fn partitioned_foreign_keys_are_listed_once() {
    let f = fixture();
    let conn = connect(&f.params(&f.database)).await;
    let pg = f.database_ref().child(ObjectKind::Schema, "m2_pg");
    let part_p = pg.child(ObjectKind::Table, "part_p");

    // A foreign key to a partitioned table: PostgreSQL adds hidden rows per referenced
    // partition, which are not foreign keys of their own.
    let part_ref = pg.child(ObjectKind::Table, "part_ref");
    let fks = list(&*conn, ObjectKind::ForeignKey, &part_ref).await;
    assert_eq!(names(&fks), ["part_ref_pid_fk"]);
    let ObjectDef::ForeignKey { foreign_key, .. } = load(&*conn, &fks[0].obj).await else {
        panic!("not a foreign key");
    };
    assert_eq!(foreign_key.ref_table, part_p);
    let ObjectDef::Table(t) = load(&*conn, &part_ref).await else {
        panic!("not a table");
    };
    assert_eq!(t.foreign_keys.len(), 1, "{:?}", t.foreign_keys);
    let fk = &t.foreign_keys[0];
    assert_eq!(fk.name.as_deref(), Some("part_ref_pid_fk"));
    assert_eq!(fk.ref_table, part_p);
    assert_eq!(fk.columns, ["pid"]);
    assert_eq!(fk.ref_columns, ["id"]);

    // PostgreSQL 18 NOT ENFORCED: reported as such, not as NOT VALID.
    assert_eq!(t.checks.len(), 1, "{:?}", t.checks);
    let check = &t.checks[0];
    assert_eq!(check.name.as_deref(), Some("part_ref_pid_chk"));
    assert_eq!(
        check.extras.get(&oxbase_pg::dialect::NOT_ENFORCED),
        Some(&Value::Bool(true))
    );
    assert_eq!(check.extras.get(&oxbase_pg::dialect::NOT_VALID), None);
    assert_eq!(fk.extras.get(&oxbase_pg::dialect::NOT_ENFORCED), None);

    // Partitioned on both sides: one foreign key on the parent, and the inherited one on the
    // partition (its parent constraint is on another table, so it stays).
    for table in ["part_c", "part_c1"] {
        let r = pg.child(ObjectKind::Table, table);
        let ObjectDef::Table(t) = load(&*conn, &r).await else {
            panic!("not a table");
        };
        assert_eq!(t.foreign_keys.len(), 1, "{table}: {:?}", t.foreign_keys);
        assert_eq!(t.foreign_keys[0].ref_table, part_p, "{table}");
        assert_eq!(t.foreign_keys[0].columns, ["pid"], "{table}");
        assert_eq!(
            list(&*conn, ObjectKind::ForeignKey, &r).await.len(),
            1,
            "{table}"
        );
    }
    conn.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the postgres fixture with the M2 seed"]
async fn trigger_conditions_exclusions_and_null_ordering() {
    let f = fixture();
    let conn = connect(&f.params(&f.database)).await;
    let pg = f.database_ref().child(ObjectKind::Schema, "m2_pg");
    let trg_t = pg.child(ObjectKind::Table, "trg_t");

    let triggers = list(&*conn, ObjectKind::Trigger, &trg_t).await;
    assert_eq!(names(&triggers), ["trg_case", "trg_distinct"]);

    let ObjectDef::Trigger(case) =
        load(&*conn, &trg_t.child(ObjectKind::Trigger, "trg_case")).await
    else {
        panic!("not a trigger");
    };
    assert_eq!(case.timing, TriggerTiming::Before);
    assert_eq!(case.events, [TriggerEvent::Update]);
    assert_eq!(case.update_columns, ["code", "n"]);
    let cond = case.condition.as_deref().expect("WHEN condition");
    // The whole CASE expression, not the text after its inner WHEN.
    assert!(
        cond.starts_with("CASE") && cond.ends_with("END") && cond.contains("new.n > 0"),
        "{cond:?}"
    );

    let ObjectDef::Trigger(distinct) =
        load(&*conn, &trg_t.child(ObjectKind::Trigger, "trg_distinct")).await
    else {
        panic!("not a trigger");
    };
    assert_eq!(distinct.timing, TriggerTiming::After);
    assert_eq!(distinct.update_columns, ["code"]);
    assert_eq!(
        distinct.condition.as_deref(),
        Some("old.code IS DISTINCT FROM new.code")
    );
    assert_eq!(
        distinct.function,
        Some(
            pg.child(ObjectKind::Function, "trg_noop")
                .with_signature("")
        )
    );

    let ObjectDef::Table(t) = load(&*conn, &trg_t).await else {
        panic!("not a table");
    };
    let excl = t
        .indexes
        .iter()
        .find(|i| i.name == "trg_t_r_excl")
        .unwrap_or_else(|| panic!("no exclusion index in {:?}", t.indexes));
    assert!(excl.is_constraint && !excl.unique, "{excl:?}");
    assert_eq!(text(excl.extras.get(&props::INDEX_METHOD)), Some("gist"));
    let exclusion = text(excl.extras.get(&oxbase_pg::dialect::EXCLUSION)).unwrap_or_default();
    assert!(
        exclusion.starts_with("EXCLUDE USING gist") && exclusion.contains("&&"),
        "{exclusion}"
    );
    let nulls = t
        .indexes
        .iter()
        .find(|i| i.name == "trg_t_nulls_idx")
        .unwrap_or_else(|| panic!("no trg_t_nulls_idx in {:?}", t.indexes));
    assert!(!nulls.is_constraint);
    assert_eq!(
        nulls
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.descending))
            .collect::<Vec<_>>(),
        [("code", false), ("n", true)]
    );
    assert_eq!(
        nulls.extras.get(&oxbase_pg::dialect::NULLS_FIRST),
        Some(&Value::Json(serde_json::json!([true, false])))
    );
    let indexes = list(&*conn, ObjectKind::Index, &trg_t).await;
    assert_eq!(
        names(&indexes),
        ["trg_t_nulls_idx", "trg_t_pkey", "trg_t_r_excl"]
    );
    let ObjectDef::Index { index, .. } =
        load(&*conn, &trg_t.child(ObjectKind::Index, "trg_t_pkey")).await
    else {
        panic!("not an index");
    };
    assert_eq!(index.extras.get(&oxbase_pg::dialect::NULLS_FIRST), None);
    conn.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the postgres fixture with the M2 seed"]
async fn extension_objects_are_flagged_system() {
    let f = fixture();
    let conn = connect(&f.params(&f.database)).await;
    let schemas = list(&*conn, ObjectKind::Schema, &f.database_ref()).await;
    assert!(!find(&schemas, "m2_pg_ext").system);
    let ext = f.database_ref().child(ObjectKind::Schema, "m2_pg_ext");
    let functions = list(&*conn, ObjectKind::Function, &ext).await;
    assert!(
        names(&functions).contains(&"citext".to_owned()),
        "{:?}",
        names(&functions)
    );
    assert!(functions.iter().all(|s| s.system), "{functions:?}");
    // Objects of the same schema kinds that no extension owns stay user objects.
    let pg = f.database_ref().child(ObjectKind::Schema, "m2_pg");
    let user_functions = list(&*conn, ObjectKind::Function, &pg).await;
    assert!(!find(&user_functions, "trg_noop").system);
    let user_tables = list(&*conn, ObjectKind::Table, &pg).await;
    assert!(user_tables.iter().all(|t| !t.system), "{user_tables:?}");
    let columns = list(
        &*conn,
        ObjectKind::Column,
        &pg.child(ObjectKind::Table, "trg_t"),
    )
    .await;
    assert!(columns.iter().all(|c| !c.system));
    conn.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the postgres fixture with the M2 seed"]
async fn system_objects_are_flagged() {
    let f = fixture();
    let conn = connect(&f.params(&f.database)).await;
    let catalog = f.database_ref().child(ObjectKind::Schema, "pg_catalog");
    let tables = list(&*conn, ObjectKind::Table, &catalog).await;
    assert!(names(&tables).contains(&"pg_class".to_owned()));
    assert!(tables.iter().all(|t| t.system));
    let views = list(
        &*conn,
        ObjectKind::View,
        &f.database_ref()
            .child(ObjectKind::Schema, "information_schema"),
    )
    .await;
    assert!(names(&views).contains(&"tables".to_owned()));
    assert!(views.iter().all(|v| v.system));
    let columns = list(
        &*conn,
        ObjectKind::Column,
        &catalog.child(ObjectKind::Table, "pg_class"),
    )
    .await;
    assert!(columns.iter().all(|c| c.system));
    assert!(names(&columns).contains(&"relname".to_owned()));
    let user_tables = list(&*conn, ObjectKind::Table, &f.schema_ref()).await;
    assert!(user_tables.iter().all(|t| !t.system));
    conn.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the postgres fixture with the M2 seed"]
async fn cancelled_missing_and_misplaced_requests_fail_cleanly() {
    let f = fixture();
    let conn = connect(&f.params(&f.database)).await;
    let schema = f.schema_ref();

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let err = fetcher(ObjectKind::Table)
        .list(&*conn, &schema, cancelled.clone())
        .await
        .unwrap_err();
    assert!(err.is_cancelled(), "{err}");
    let err = fetcher(ObjectKind::Table)
        .load(&*conn, &f.table_ref("m2_parent"), cancelled)
        .await
        .unwrap_err();
    assert!(err.is_cancelled(), "{err}");

    for missing in [
        f.table_ref("m2_missing"),
        f.table_ref("m2_child").child(ObjectKind::Column, "nope"),
        f.table_ref("m2_child").child(ObjectKind::Index, "nope"),
        f.table_ref("m2_child")
            .child(ObjectKind::ForeignKey, "nope"),
        f.table_ref("m2_child")
            .child(ObjectKind::Check, "m2_child_parent_fk"),
        f.table_ref("m2_child").child(ObjectKind::Trigger, "nope"),
        f.table_ref("m2_missing").child(ObjectKind::Column, "id"),
        schema.child(ObjectKind::Sequence, "m2_missing"),
        schema.child(ObjectKind::Function, "m2_missing"),
        schema.child(ObjectKind::MatView, "m2_missing"),
        f.database_ref().child(ObjectKind::Schema, "m2_missing"),
        ObjectRef::database("oxbase_m2_missing"),
    ] {
        let err = try_load(&*conn, &missing).await.unwrap_err();
        assert!(matches!(err, Error::NotFound { .. }), "{missing}: {err}");
    }
    let err = try_list(&*conn, ObjectKind::Column, &f.table_ref("m2_missing"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound { .. }), "{err}");

    // Another database is read through its own session.
    let err = try_list(
        &*conn,
        ObjectKind::Schema,
        &ObjectRef::database(&f.other_database),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::InvalidInput { .. }), "{err}");
    // Wrong parent kinds.
    let err = try_list(&*conn, ObjectKind::Table, &f.table_ref("m2_child"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput { .. }), "{err}");
    let err = try_list(&*conn, ObjectKind::Database, &f.database_ref())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidInput { .. }), "{err}");

    // The session still works.
    assert_ne!(list(&*conn, ObjectKind::Table, &schema).await.len(), 0);
    conn.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the postgres fixture with the M2 seed"]
async fn another_database_is_read_through_its_own_session() {
    let f = fixture();
    let conn = connect(&f.params(&f.other_database)).await;
    assert_eq!(
        conn.current_database().as_deref(),
        Some(f.other_database.as_str())
    );
    let db = ObjectRef::database(&f.other_database);
    let schemas = list(&*conn, ObjectKind::Schema, &db).await;
    let public = find(&schemas, "public").obj.clone();
    assert_eq!(public.catalog.as_deref(), Some(f.other_database.as_str()));
    let tables = list(&*conn, ObjectKind::Table, &public).await;
    assert_eq!(names(&tables), [f.other_table.as_str()]);
    let ObjectDef::Table(t) = load(&*conn, &tables[0].obj).await else {
        panic!("not a table");
    };
    assert_eq!(t.obj.catalog.as_deref(), Some(f.other_database.as_str()));
    assert_eq!(
        t.primary_key.map(|pk| pk.columns),
        Some(vec!["id".to_owned()])
    );
    // The database list is server-wide, from any session.
    let dbs = list(&*conn, ObjectKind::Database, &ObjectRef::root()).await;
    assert!(names(&dbs).contains(&f.database));
    conn.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the postgres fixture with the M2 seed"]
async fn cancel_interrupts_a_blocked_catalog_query() {
    let f = fixture();
    let holder = connect(&f.params(&f.database)).await;
    let reader = connect(&f.params(&f.database)).await;
    let view = f.schema_ref().child(ObjectKind::View, f.name("view"));

    // An exclusive lock on the view makes `pg_get_viewdef` wait: a slow catalog query.
    let tx = holder.begin().await.expect("begin");
    let stream = tx
        .execute(
            "LOCK TABLE m2.m2_view IN ACCESS EXCLUSIVE MODE",
            &[],
            CancellationToken::new(),
        )
        .await
        .expect("lock");
    oxbase_driver::collect(stream).await.expect("lock");

    let fetcher = fetcher(ObjectKind::View);
    let token = CancellationToken::new();
    let fut = fetcher.load(&*reader, &view, token.clone());
    tokio::pin!(fut);
    assert!(
        tokio::time::timeout(Duration::from_millis(700), &mut fut)
            .await
            .is_err(),
        "the view load should wait for the lock"
    );
    let started = Instant::now();
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), &mut fut)
        .await
        .expect("cancel ends the load within 2 s");
    assert!(
        matches!(result, Err(Error::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));

    tx.rollback().await.expect("rollback");
    // The reader session is usable again once the lock is gone.
    let ObjectDef::View(v) = load(&*reader, &view).await else {
        panic!("not a view");
    };
    assert!(v.definition.starts_with("SELECT"));
    reader.close().await.unwrap();
    holder.close().await.unwrap();
}
