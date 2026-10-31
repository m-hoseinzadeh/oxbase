//! Dialect cells (M2) against real SQLite databases: the fixture seed of
//! `tests/docker/init/sqlite/01-m2.sql` in a temporary file with an attached `m2aux`, plus extra
//! objects for the SQLite-specific shapes. No Docker.

use std::path::Path;
use std::sync::Arc;

use oxbase_dialect::{AnsiQuoting, DatabaseScope, DialectRegistry, Fetcher, Quoting};
use oxbase_driver::{CancellationToken, ConnectParams, Connection, Driver, collect};
use oxbase_sqlite::SqliteDriver;
use oxbase_sqlite::dialect::{self, AUTOINCREMENT, MODULE, UNNAMED};
use oxbase_types::{
    DataType, Engine, ErrorCategory, FkAction, ForeignKeyDef, IndexDef, ObjectDef, ObjectKind,
    ObjectRef, ObjectSummary, PropId, Result, TriggerEvent, TriggerLevel, TriggerTiming, Value,
    props,
};
use tempfile::TempDir;

const SEED: &str = include_str!("../../../tests/docker/init/sqlite/01-m2.sql");
const FIXTURES: &str = include_str!("../../../tests/fixtures.toml");

fn registry() -> DialectRegistry {
    let mut reg = DialectRegistry::new();
    dialect::register(&mut reg);
    reg
}

fn fetcher(kind: ObjectKind) -> Arc<dyn Fetcher> {
    registry().fetcher(Engine::Sqlite, kind).unwrap()
}

async fn run(c: &dyn Connection, sql: &str, params: &[Value]) {
    collect(
        c.execute(sql, params, CancellationToken::new())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
}

async fn open(path: &Path) -> Box<dyn Connection> {
    SqliteDriver
        .connect(&ConnectParams::sqlite(path))
        .await
        .unwrap()
}

/// The seeded fixture: `main` from the seed script, `m2aux` attached with `m2_aux_t`.
async fn fixture() -> (TempDir, Box<dyn Connection>) {
    let dir = tempfile::tempdir().unwrap();
    let c = open(&dir.path().join("oxbase_test.sqlite")).await;
    run(c.as_ref(), SEED, &[]).await;
    let aux = dir.path().join("m2aux.sqlite");
    run(
        c.as_ref(),
        "ATTACH DATABASE ?1 AS m2aux",
        &[Value::from(aux.display().to_string())],
    )
    .await;
    run(
        c.as_ref(),
        "CREATE TABLE m2aux.m2_aux_t (id INTEGER PRIMARY KEY, label TEXT)",
        &[],
    )
    .await;
    (dir, c)
}

async fn list(c: &dyn Connection, kind: ObjectKind, parent: &ObjectRef) -> Vec<ObjectSummary> {
    fetcher(kind)
        .list(c, parent, CancellationToken::new())
        .await
        .unwrap()
}

async fn load(c: &dyn Connection, obj: &ObjectRef) -> Result<ObjectDef> {
    fetcher(obj.kind)
        .load(c, obj, CancellationToken::new())
        .await
}

fn names(rows: &[ObjectSummary]) -> Vec<&str> {
    rows.iter().map(|s| s.obj.name.as_str()).collect()
}

fn user_names(rows: &[ObjectSummary]) -> Vec<&str> {
    rows.iter()
        .filter(|s| !s.system)
        .map(|s| s.obj.name.as_str())
        .collect()
}

fn extra<'a>(s: &'a ObjectSummary, p: &PropId) -> Option<&'a Value> {
    s.extras.get(p)
}

fn main_db() -> ObjectRef {
    ObjectRef::database("main")
}

#[test]
fn the_seed_matches_fixtures_toml() {
    for line in [
        r#"init_sql = "tests/docker/init/sqlite/01-m2.sql""#,
        r#"attached_as = "m2aux""#,
        r#"attached_table = "m2_aux_t""#,
        r#"objects = ["m2_parent", "m2_child", "m2_view", "m2_child_parent_idx", "m2_trg"]"#,
        r#"check_parent = "m2_parent_code_chk""#,
        r#"check_child = "m2_child_qty_chk""#,
        r#"foreign_key = "m2_child_parent_fk""#,
    ] {
        assert!(FIXTURES.contains(line), "fixtures.toml lost: {line}");
    }
}

#[tokio::test]
async fn registers_the_layout_and_reports_main() {
    let reg = registry();
    assert_eq!(
        reg.missing_layout_fetchers(Engine::Sqlite),
        Vec::<ObjectKind>::new()
    );
    let layout = reg.tree_layout(Engine::Sqlite).unwrap();
    assert_eq!(layout.scope, DatabaseScope::Attached);
    assert_eq!(layout.root()[0].kind, ObjectKind::Database);
    let view_kids: Vec<ObjectKind> = layout
        .children(ObjectKind::View)
        .iter()
        .map(|g| g.kind)
        .collect();
    assert_eq!(view_kids, [ObjectKind::Column, ObjectKind::Trigger]);

    let (_dir, c) = fixture().await;
    assert_eq!(c.current_database().as_deref(), Some("main"));
}

#[tokio::test]
async fn lists_main_attached_and_used_temp_databases() {
    let (_dir, c) = fixture().await;
    let c = c.as_ref();
    let dbs = list(c, ObjectKind::Database, &ObjectRef::root()).await;
    assert_eq!(names(&dbs), ["m2aux", "main"]);
    let main = &dbs[1];
    assert_eq!(main.obj, ObjectRef::database("main"));
    assert_eq!(extra(main, &props::CURRENT), Some(&Value::Bool(true)));
    let file = extra(main, &props::FILE).and_then(Value::as_str).unwrap();
    assert!(file.ends_with("oxbase_test.sqlite"), "{file}");
    assert_eq!(extra(&dbs[0], &props::CURRENT), Some(&Value::Bool(false)));
    assert!(
        extra(&dbs[0], &props::FILE)
            .and_then(Value::as_str)
            .unwrap()
            .ends_with("m2aux.sqlite")
    );

    run(c, "CREATE TEMP TABLE scratch (x)", &[]).await;
    let dbs = list(c, ObjectKind::Database, &ObjectRef::root()).await;
    assert_eq!(names(&dbs), ["m2aux", "main", "temp"]);
    assert_eq!(extra(&dbs[2], &props::TEMPORARY), Some(&Value::Bool(true)));
    let temp_tables = list(c, ObjectKind::Table, &ObjectRef::database("temp")).await;
    assert_eq!(names(&temp_tables), ["scratch"]);
    assert_eq!(
        extra(&temp_tables[0], &props::TEMPORARY),
        Some(&Value::Bool(true))
    );

    let ObjectDef::Schema(def) = load(c, &ObjectRef::database("m2aux")).await.unwrap() else {
        panic!("not a schema def");
    };
    assert_eq!(def.obj, ObjectRef::database("m2aux"));
    assert!(def.extras.contains(&props::FILE));
    assert_eq!(
        def.extras.get(&props::ENCODING).and_then(Value::as_str),
        Some("UTF-8")
    );
}

#[tokio::test]
async fn lists_and_loads_every_fixture_object() {
    let (_dir, c) = fixture().await;
    let c = c.as_ref();
    let db = main_db();

    let tables = list(c, ObjectKind::Table, &db).await;
    assert_eq!(user_names(&tables), ["m2_child", "m2_parent"]);
    assert!(
        tables
            .iter()
            .all(|t| t.obj.catalog.as_deref() == Some("main"))
    );
    let views = list(c, ObjectKind::View, &db).await;
    assert_eq!(names(&views), ["m2_view"]);

    // m2_parent
    let parent = db.child(ObjectKind::Table, "m2_parent");
    let ObjectDef::Table(t) = load(c, &parent).await.unwrap() else {
        panic!("not a table");
    };
    assert_eq!(t.obj, parent);
    let cols: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cols, ["id", "code", "note"]);
    let id = &t.columns[0];
    assert_eq!(id.data_type, DataType::INT64);
    assert_eq!(id.native_type, "INTEGER");
    assert!(id.auto_increment && !id.nullable);
    let code = &t.columns[1];
    assert_eq!(code.data_type, DataType::VarChar { length: Some(20) });
    assert!(!code.nullable);
    assert!(t.columns[2].nullable);
    assert_eq!(t.columns[2].data_type, DataType::Text);
    assert_eq!(t.primary_key.as_ref().unwrap().columns, ["id"]);
    assert_eq!(t.indexes.len(), 1, "{:?}", t.indexes);
    let unique = &t.indexes[0];
    assert!(unique.unique && unique.is_constraint);
    assert_eq!(unique.columns[0].name, "code");
    assert_eq!(t.checks.len(), 1);
    assert_eq!(t.checks[0].name.as_deref(), Some("m2_parent_code_chk"));
    assert_eq!(t.checks[0].expression, "length(code) > 0");
    assert_eq!(t.foreign_keys, Vec::<ForeignKeyDef>::new());
    assert_eq!(t.comment, None);

    // m2_child
    let child = db.child(ObjectKind::Table, "m2_child");
    let ObjectDef::Table(t) = load(c, &child).await.unwrap() else {
        panic!("not a table");
    };
    assert_eq!(t.column("qty").unwrap().default.as_deref(), Some("1"));
    assert!(!t.column("parent_id").unwrap().nullable);
    let fk = &t.foreign_keys[0];
    assert_eq!(t.foreign_keys.len(), 1);
    assert_eq!(fk.name.as_deref(), Some("m2_child_parent_fk"));
    assert_eq!(fk.columns, ["parent_id"]);
    assert_eq!(
        fk.ref_table,
        ObjectRef::table(Some("main"), None, "m2_parent")
    );
    assert_eq!(fk.ref_columns, ["id"]);
    assert_eq!(fk.on_delete, FkAction::Cascade);
    assert_eq!(fk.on_update, FkAction::NoAction);
    assert_eq!(fk.extras.get(&props::DEFERRABLE), Some(&Value::Bool(false)));
    assert_eq!(t.checks[0].name.as_deref(), Some("m2_child_qty_chk"));
    assert_eq!(t.checks[0].expression, "qty >= 0");
    assert_eq!(t.indexes.len(), 1);
    assert_eq!(t.indexes[0].name, "m2_child_parent_idx");
    assert!(!t.indexes[0].unique);
    assert_eq!(t.indexes[0].columns[0].name, "parent_id");

    // Table children.
    let columns = list(c, ObjectKind::Column, &child).await;
    assert_eq!(names(&columns), ["id", "parent_id", "qty"]);
    assert_eq!(columns[0].obj.parent.as_deref(), Some("m2_child"));
    assert_eq!(columns[0].obj.parent_kind, Some(ObjectKind::Table));
    assert_eq!(
        extra(&columns[0], &props::NATIVE_TYPE).and_then(Value::as_str),
        Some("INTEGER")
    );
    assert_eq!(
        extra(&columns[0], &props::PRIMARY_KEY),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        extra(&columns[1], &props::PRIMARY_KEY),
        Some(&Value::Bool(false))
    );
    let ObjectDef::Column { table, column } = load(c, &columns[2].obj).await.unwrap() else {
        panic!("not a column");
    };
    assert_eq!(table, child);
    assert_eq!(column.name, "qty");
    assert!(!column.nullable);

    let indexes = list(c, ObjectKind::Index, &child).await;
    assert_eq!(names(&indexes), ["m2_child_parent_idx"]);
    let ObjectDef::Index { table, index } = load(c, &indexes[0].obj).await.unwrap() else {
        panic!("not an index");
    };
    assert_eq!(table, child);
    assert_eq!(index.columns[0].name, "parent_id");
    let parent_indexes = list(c, ObjectKind::Index, &parent).await;
    assert_eq!(parent_indexes.len(), 1);
    assert_eq!(
        extra(&parent_indexes[0], &props::UNIQUE),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        extra(&parent_indexes[0], &props::PRIMARY_KEY),
        Some(&Value::Bool(false))
    );

    let fks = list(c, ObjectKind::ForeignKey, &child).await;
    assert_eq!(names(&fks), ["m2_child_parent_fk"]);
    let ObjectDef::ForeignKey { foreign_key, .. } = load(c, &fks[0].obj).await.unwrap() else {
        panic!("not a foreign key");
    };
    assert_eq!(foreign_key.on_delete, FkAction::Cascade);

    let checks = list(c, ObjectKind::Check, &child).await;
    assert_eq!(names(&checks), ["m2_child_qty_chk"]);
    let ObjectDef::Check { check, .. } = load(c, &checks[0].obj).await.unwrap() else {
        panic!("not a check");
    };
    assert_eq!(check.expression, "qty >= 0");
    assert_eq!(
        names(&list(c, ObjectKind::Check, &parent).await),
        ["m2_parent_code_chk"]
    );

    let triggers = list(c, ObjectKind::Trigger, &child).await;
    assert_eq!(names(&triggers), ["m2_trg"]);
    let ObjectDef::Trigger(trg) = load(c, &triggers[0].obj).await.unwrap() else {
        panic!("not a trigger");
    };
    assert_eq!(trg.obj.parent.as_deref(), Some("m2_child"));
    assert_eq!(trg.timing, TriggerTiming::Before);
    assert_eq!(trg.events, [TriggerEvent::Insert]);
    assert_eq!(trg.level, TriggerLevel::Row);
    assert_eq!(trg.condition.as_deref(), Some("NEW.qty < 0"));
    let body = trg.body.as_deref().unwrap();
    assert!(body.starts_with("BEGIN") && body.ends_with("END"), "{body}");
    assert!(
        trg.definition
            .as_deref()
            .unwrap()
            .starts_with("CREATE TRIGGER m2_trg")
    );
    assert!(trg.enabled && trg.function.is_none());

    // The view.
    let view = &views[0].obj;
    let ObjectDef::View(v) = load(c, view).await.unwrap() else {
        panic!("not a view");
    };
    assert_eq!(
        v.definition,
        "SELECT c.id, c.qty, p.code FROM m2_child c JOIN m2_parent p ON p.id = c.parent_id"
    );
    assert_eq!(v.columns, Vec::<String>::new());
    assert_eq!(
        names(&list(c, ObjectKind::Column, view).await),
        ["id", "qty", "code"]
    );
    assert_eq!(
        list(c, ObjectKind::Trigger, view).await,
        Vec::<ObjectSummary>::new()
    );
    assert_eq!(
        list(c, ObjectKind::Index, view).await,
        Vec::<ObjectSummary>::new()
    );

    // Every `[m2.sqlite].objects` entry was found above under its parent.
    let found: Vec<&str> = tables
        .iter()
        .chain(&views)
        .chain(&indexes)
        .chain(&triggers)
        .map(|s| s.obj.name.as_str())
        .collect();
    for name in [
        "m2_parent",
        "m2_child",
        "m2_view",
        "m2_child_parent_idx",
        "m2_trg",
    ] {
        assert!(found.contains(&name), "{name}");
    }

    // The attached database.
    let aux = ObjectRef::database("m2aux");
    let aux_tables = list(c, ObjectKind::Table, &aux).await;
    assert_eq!(names(&aux_tables), ["m2_aux_t"]);
    assert_eq!(aux_tables[0].obj.catalog.as_deref(), Some("m2aux"));
    let ObjectDef::Table(t) = load(c, &aux_tables[0].obj).await.unwrap() else {
        panic!("not a table");
    };
    assert_eq!(t.columns.len(), 2);
    assert_eq!(
        list(c, ObjectKind::View, &aux).await,
        Vec::<ObjectSummary>::new()
    );
    // A ref without a catalog means `main`.
    assert!(
        load(c, &ObjectRef::table(None, None, "m2_parent"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn reads_sqlite_specific_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let c = open(&dir.path().join("shapes.sqlite")).await;
    let c = c.as_ref();
    run(
        c,
        "CREATE TABLE wr (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID;
         CREATE TABLE st (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             n INTEGER NOT NULL,
             g INTEGER GENERATED ALWAYS AS (n * 2) STORED,
             h INTEGER AS (n + 1),
             name TEXT COLLATE NOCASE
         ) STRICT;
         CREATE TABLE refs (
             a INTEGER,
             b INTEGER REFERENCES st ON DELETE SET NULL DEFERRABLE INITIALLY DEFERRED,
             CHECK (a > 0),
             FOREIGN KEY (a) REFERENCES st (id)
         );
         CREATE INDEX refs_expr ON refs (lower(b) DESC, a) WHERE a IS NOT NULL;
         CREATE VIRTUAL TABLE docs USING fts5(title, body);
         CREATE VIEW v2 (x, y) AS SELECT a, b FROM refs;
         CREATE TRIGGER v2_ins INSTEAD OF INSERT ON v2
         BEGIN INSERT INTO refs (a, b) VALUES (NEW.x, NEW.y); END;
         CREATE TRIGGER refs_upd AFTER UPDATE OF a, b ON refs BEGIN SELECT 1; END;",
        &[],
    )
    .await;
    let db = main_db();

    let tables = list(c, ObjectKind::Table, &db).await;
    assert_eq!(user_names(&tables), ["docs", "refs", "st", "wr"]);
    let by_name = |n: &str| tables.iter().find(|t| t.obj.name == n).unwrap();
    assert_eq!(
        extra(by_name("wr"), &props::WITHOUT_ROWID),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        extra(by_name("st"), &props::STRICT),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        extra(by_name("docs"), &props::TABLE_TYPE).and_then(Value::as_str),
        Some("virtual")
    );
    assert_eq!(
        extra(by_name("docs"), &MODULE).and_then(Value::as_str),
        Some("fts5")
    );
    // AUTOINCREMENT's bookkeeping table and the fts5 shadow tables are system objects.
    assert!(by_name("sqlite_sequence").system);
    let shadow = by_name("docs_data");
    assert!(shadow.system);
    assert_eq!(
        extra(shadow, &props::TABLE_TYPE).and_then(Value::as_str),
        Some("shadow")
    );

    // WITHOUT ROWID: the key index is listed (marked) but not part of TableDef.indexes.
    let wr = db.child(ObjectKind::Table, "wr");
    let ObjectDef::Table(t) = load(c, &wr).await.unwrap() else {
        panic!("not a table");
    };
    assert_eq!(t.indexes, Vec::<IndexDef>::new());
    assert_eq!(t.primary_key.unwrap().columns, ["k"]);
    assert!(!t.columns[0].nullable && !t.columns[0].auto_increment);
    assert_eq!(
        t.extras.get(&props::WITHOUT_ROWID),
        Some(&Value::Bool(true))
    );
    let wr_indexes = list(c, ObjectKind::Index, &wr).await;
    assert_eq!(wr_indexes.len(), 1);
    assert_eq!(
        extra(&wr_indexes[0], &props::PRIMARY_KEY),
        Some(&Value::Bool(true))
    );
    let ObjectDef::Index { index, .. } = load(c, &wr_indexes[0].obj).await.unwrap() else {
        panic!("not an index");
    };
    assert_eq!(
        index.extras.get(&props::PRIMARY_KEY),
        Some(&Value::Bool(true))
    );

    // STRICT, AUTOINCREMENT, generated columns, collations.
    let ObjectDef::Table(t) = load(c, &db.child(ObjectKind::Table, "st")).await.unwrap() else {
        panic!("not a table");
    };
    assert_eq!(t.extras.get(&props::STRICT), Some(&Value::Bool(true)));
    let id = t.column("id").unwrap();
    assert!(id.auto_increment);
    assert_eq!(id.extras.get(&AUTOINCREMENT), Some(&Value::Bool(true)));
    let g = t.column("g").unwrap().generated.clone().unwrap();
    assert_eq!((g.expression.as_str(), g.stored), ("n * 2", true));
    let h = t.column("h").unwrap().generated.clone().unwrap();
    assert_eq!((h.expression.as_str(), h.stored), ("n + 1", false));
    assert_eq!(
        t.column("name").unwrap().collation.as_deref(),
        Some("NOCASE")
    );

    // Unnamed foreign keys and checks, implicit parent keys, expression and partial indexes.
    let refs = db.child(ObjectKind::Table, "refs");
    let ObjectDef::Table(t) = load(c, &refs).await.unwrap() else {
        panic!("not a table");
    };
    assert_eq!(t.foreign_keys.len(), 2);
    let (first, second) = (&t.foreign_keys[0], &t.foreign_keys[1]);
    assert_eq!(first.name, None);
    assert_eq!(first.columns, ["b"]);
    assert_eq!(first.ref_columns, ["id"]);
    assert_eq!(first.on_delete, FkAction::SetNull);
    assert_eq!(
        first.extras.get(&props::DEFERRABLE),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        first.extras.get(&props::INITIALLY_DEFERRED),
        Some(&Value::Bool(true))
    );
    assert_eq!(second.columns, ["a"]);
    assert_eq!(second.on_delete, FkAction::NoAction);
    let index = &t.indexes[0];
    assert_eq!(index.name, "refs_expr");
    assert!(index.columns[0].is_expression && index.columns[0].descending);
    assert_eq!(index.columns[0].name, "lower(b)");
    assert_eq!(index.columns[1].name, "a");
    assert_eq!(index.predicate.as_deref(), Some("a IS NOT NULL"));

    let fks = list(c, ObjectKind::ForeignKey, &refs).await;
    assert_eq!(names(&fks), ["(foreign key 1)", "(foreign key 2)"]);
    assert_eq!(extra(&fks[0], &UNNAMED), Some(&Value::Bool(true)));
    let ObjectDef::ForeignKey { foreign_key, .. } = load(c, &fks[1].obj).await.unwrap() else {
        panic!("not a foreign key");
    };
    assert_eq!(foreign_key.columns, ["a"]);
    let checks = list(c, ObjectKind::Check, &refs).await;
    assert_eq!(names(&checks), ["(check 1)"]);
    let ObjectDef::Check { check, .. } = load(c, &checks[0].obj).await.unwrap() else {
        panic!("not a check");
    };
    assert_eq!((check.name, check.expression.as_str()), (None, "a > 0"));

    let triggers = list(c, ObjectKind::Trigger, &refs).await;
    let ObjectDef::Trigger(trg) = load(c, &triggers[0].obj).await.unwrap() else {
        panic!("not a trigger");
    };
    assert_eq!(trg.timing, TriggerTiming::After);
    assert_eq!(trg.events, [TriggerEvent::Update]);
    assert_eq!(trg.update_columns, ["a", "b"]);

    // A view with a column list and an INSTEAD OF trigger.
    let v2 = db.child(ObjectKind::View, "v2");
    let ObjectDef::View(v) = load(c, &v2).await.unwrap() else {
        panic!("not a view");
    };
    assert_eq!(v.columns, ["x", "y"]);
    assert_eq!(v.definition, "SELECT a, b FROM refs");
    let view_triggers = list(c, ObjectKind::Trigger, &v2).await;
    assert_eq!(names(&view_triggers), ["v2_ins"]);
    assert_eq!(view_triggers[0].obj.parent_kind, Some(ObjectKind::View));
    // Loading a trigger without its owner fills the owner in.
    let ObjectDef::Trigger(trg) = load(
        c,
        &ObjectRef::new(ObjectKind::Trigger, "v2_ins").in_catalog(Some("main")),
    )
    .await
    .unwrap() else {
        panic!("not a trigger");
    };
    assert_eq!(trg.timing, TriggerTiming::InsteadOf);
    assert_eq!(trg.obj.parent.as_deref(), Some("v2"));
    assert_eq!(trg.obj.parent_kind, Some(ObjectKind::View));
    // ... and under the wrong table it is not there.
    let wrong = db
        .child(ObjectKind::Table, "refs")
        .child(ObjectKind::Trigger, "v2_ins");
    assert_eq!(
        load(c, &wrong).await.unwrap_err().category(),
        ErrorCategory::NotFound
    );

    // Virtual tables list their hidden columns as system columns.
    let docs = list(c, ObjectKind::Column, &db.child(ObjectKind::Table, "docs")).await;
    assert_eq!(user_names(&docs), ["title", "body"]);
}

#[tokio::test]
async fn names_are_bound_or_quoted_never_spliced() {
    let (dir, c) = fixture().await;
    let c = c.as_ref();
    let evil_db = "x\"y'; DROP TABLE m2_parent; --";
    let evil_table = "t'); DROP TABLE m2_child; --\"";
    let path = dir.path().join("evil.sqlite");
    run(
        c,
        "ATTACH DATABASE ?1 AS ?2",
        &[
            Value::from(path.display().to_string()),
            Value::from(evil_db),
        ],
    )
    .await;
    let q = AnsiQuoting;
    run(
        c,
        &format!(
            "CREATE TABLE {}.{} (id INTEGER PRIMARY KEY, {} TEXT CHECK ({} <> ''))",
            q.quote_ident(evil_db),
            q.quote_ident(evil_table),
            q.quote_ident("c\"q"),
            q.quote_ident("c\"q"),
        ),
        &[],
    )
    .await;

    let dbs = list(c, ObjectKind::Database, &ObjectRef::root()).await;
    assert!(names(&dbs).contains(&evil_db));
    let db = ObjectRef::database(evil_db);
    let tables = list(c, ObjectKind::Table, &db).await;
    assert_eq!(names(&tables), [evil_table]);
    let ObjectDef::Table(t) = load(c, &tables[0].obj).await.unwrap() else {
        panic!("not a table");
    };
    let cols: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cols, ["id", "c\"q"]);
    assert_eq!(t.checks[0].expression, "\"c\"\"q\" <> ''");
    for kind in [
        ObjectKind::Column,
        ObjectKind::Index,
        ObjectKind::ForeignKey,
        ObjectKind::Check,
        ObjectKind::Trigger,
    ] {
        fetcher(kind)
            .list(c, &tables[0].obj, CancellationToken::new())
            .await
            .unwrap();
    }
    assert!(load(c, &ObjectRef::database(evil_db)).await.is_ok());

    // A database name that is not attached never reaches SQL.
    let err = fetcher(ObjectKind::Table)
        .list(
            c,
            &ObjectRef::database("nope\"; DROP TABLE m2_parent; --"),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);

    // Nothing was dropped.
    let main_tables = list(c, ObjectKind::Table, &main_db()).await;
    assert_eq!(user_names(&main_tables), ["m2_child", "m2_parent"]);
}

#[tokio::test]
async fn cancelled_tokens_and_missing_objects() {
    let (_dir, c) = fixture().await;
    let c = c.as_ref();
    let db = main_db();
    let child = db.child(ObjectKind::Table, "m2_child");
    let refs = [
        (ObjectRef::root(), ObjectRef::database("main")),
        (db.clone(), child.clone()),
        (db.clone(), db.child(ObjectKind::View, "m2_view")),
        (child.clone(), child.child(ObjectKind::Column, "qty")),
        (
            child.clone(),
            child.child(ObjectKind::Index, "m2_child_parent_idx"),
        ),
        (
            child.clone(),
            child.child(ObjectKind::ForeignKey, "m2_child_parent_fk"),
        ),
        (
            child.clone(),
            child.child(ObjectKind::Check, "m2_child_qty_chk"),
        ),
        (child.clone(), child.child(ObjectKind::Trigger, "m2_trg")),
    ];
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    for (parent, obj) in &refs {
        let f = fetcher(obj.kind);
        let err = f.list(c, parent, cancelled.clone()).await.unwrap_err();
        assert!(err.is_cancelled(), "list {}: {err}", obj.kind);
        let err = f.load(c, obj, cancelled.clone()).await.unwrap_err();
        assert!(err.is_cancelled(), "load {}: {err}", obj.kind);
        // The objects themselves are there.
        load(c, obj).await.unwrap();
    }

    let missing = [
        ObjectRef::database("nope"),
        db.child(ObjectKind::Table, "nope"),
        db.child(ObjectKind::Table, "m2_view"),
        db.child(ObjectKind::View, "m2_child"),
        child.child(ObjectKind::Column, "nope"),
        child.child(ObjectKind::Index, "nope"),
        child.child(ObjectKind::ForeignKey, "nope"),
        child.child(ObjectKind::Check, "nope"),
        child.child(ObjectKind::Trigger, "nope"),
        db.child(ObjectKind::Table, "nope")
            .child(ObjectKind::Column, "id"),
        ObjectRef::table(Some("nope"), None, "m2_child"),
    ];
    for obj in &missing {
        let err = load(c, obj).await.unwrap_err();
        assert_eq!(err.category(), ErrorCategory::NotFound, "{obj}: {err}");
    }
    let err = fetcher(ObjectKind::Column)
        .list(
            c,
            &db.child(ObjectKind::Table, "nope"),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::NotFound);
}
