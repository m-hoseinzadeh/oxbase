//! Live tests of the MySQL / MariaDB dialect fetchers against the `mysql` and `mariadb` fixtures
//! (`tests/docker/init/{mysql,mariadb}/04-m2.sql`, names in `tests/fixtures.toml` `[m2]`).
//!
//! All are `#[ignore]`d: run them with
//! `cargo test -p oxbase-mysql --test dialect_live -- --include-ignored`. They only read.

use std::path::{Path, PathBuf};

use oxbase_dialect::{DialectRegistry, Fetcher};
use oxbase_driver::{CancellationToken, ConnectParams, Connection};
use oxbase_mysql::{MySqlConnection, MySqlDriver};
use oxbase_types::{
    ArgMode, DataType, Engine, Error, FkAction, ObjectDef, ObjectKind, ObjectRef, ObjectSummary,
    TlsMode, TlsOptions, TriggerEvent, TriggerLevel, TriggerTiming, Value, props,
};

// ---- fixtures ---------------------------------------------------------------------------------

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
    let (last, tables) = path.split_last().expect("non-empty path");
    let mut table = t;
    for key in tables {
        table = table
            .get(*key)
            .and_then(toml::Value::as_table)
            .unwrap_or_else(|| panic!("fixtures.toml has no table {key}"));
    }
    table
        .get(*last)
        .unwrap_or_else(|| panic!("fixtures.toml has no {}", path.join(".")))
}

fn s(t: &toml::Table, path: &[&str]) -> String {
    get(t, path).as_str().expect("string").to_owned()
}

fn engine_of(key: &str) -> Engine {
    match key {
        "mysql" => Engine::MySql,
        "mariadb" => Engine::MariaDb,
        other => panic!("not a MySQL-family fixture: {other}"),
    }
}

async fn connect(key: &str) -> MySqlConnection {
    let f = fixtures();
    let port = u16::try_from(get(&f, &["engines", key, "port"]).as_integer().unwrap()).unwrap();
    let p = ConnectParams::network(engine_of(key), s(&f, &["engines", key, "host"]))
        .with_port(port)
        .with_user(s(&f, &["engines", key, "user"]))
        .with_password(s(&f, &["engines", key, "password"]))
        .with_database(s(&f, &["engines", key, "database"]))
        .with_tls(TlsOptions::new(TlsMode::Disable));
    MySqlDriver::new()
        .connect_mysql(&p)
        .await
        .unwrap_or_else(|e| panic!("connect {key}: {e}"))
}

/// What a scenario needs: the connection, the registry and the fixture names.
struct Ctx {
    key: &'static str,
    c: MySqlConnection,
    reg: DialectRegistry,
    f: toml::Table,
}

impl Ctx {
    async fn new(key: &'static str) -> Self {
        let mut reg = DialectRegistry::new();
        oxbase_mysql::dialect::register(&mut reg);
        Self {
            key,
            c: connect(key).await,
            reg,
            f: fixtures(),
        }
    }

    fn engine(&self) -> Engine {
        self.c.engine()
    }

    fn mariadb(&self) -> bool {
        self.engine() == Engine::MariaDb
    }

    fn name(&self, key: &str) -> String {
        s(&self.f, &["m2", key])
    }

    fn database(&self) -> ObjectRef {
        ObjectRef::database(s(&self.f, &["m2", self.key, "database"]))
    }

    fn table(&self, name: &str) -> ObjectRef {
        self.database().child(ObjectKind::Table, name)
    }

    fn fetcher(&self, kind: ObjectKind) -> std::sync::Arc<dyn Fetcher> {
        self.reg
            .fetcher(self.engine(), kind)
            .unwrap_or_else(|e| panic!("{kind}: {e}"))
    }

    async fn list(&self, kind: ObjectKind, parent: &ObjectRef) -> Vec<ObjectSummary> {
        self.fetcher(kind)
            .list(&self.c, parent, CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{} list {kind} under {parent}: {e}", self.key))
    }

    async fn load(&self, obj: &ObjectRef) -> ObjectDef {
        self.fetcher(obj.kind)
            .load(&self.c, obj, CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{} load {obj}: {e}", self.key))
    }

    async fn load_err(&self, obj: &ObjectRef) -> Error {
        self.fetcher(obj.kind)
            .load(&self.c, obj, CancellationToken::new())
            .await
            .expect_err("load should fail")
    }
}

fn names(list: &[ObjectSummary]) -> Vec<&str> {
    list.iter().map(|s| s.obj.name.as_str()).collect()
}

fn find<'a>(list: &'a [ObjectSummary], name: &str) -> &'a ObjectSummary {
    list.iter()
        .find(|s| s.obj.name == name)
        .unwrap_or_else(|| panic!("{name} not in {:?}", names(list)))
}

fn assert_sorted(list: &[ObjectSummary]) {
    let got = names(list);
    let mut sorted = got.clone();
    sorted.sort_unstable();
    assert_eq!(got, sorted, "listing is not sorted by name");
}

// ---- scenarios --------------------------------------------------------------------------------

async fn databases(key: &'static str) {
    let t = Ctx::new(key).await;
    let dbs = t.list(ObjectKind::Database, &ObjectRef::root()).await;
    assert_sorted(&dbs);
    let db = find(&dbs, &t.database().name);
    assert_eq!(db.obj, t.database());
    assert!(!db.system);
    assert_eq!(db.extras.get(&props::CURRENT), Some(&Value::Bool(true)));
    let is = find(&dbs, "information_schema");
    assert!(is.system);
    assert!(!is.extras.contains(&props::CURRENT));

    let ObjectDef::Schema(def) = t.load(&t.database()).await else {
        panic!("a database loads as a SchemaDef");
    };
    assert_eq!(def.obj, t.database());
    assert_eq!(
        def.extras.get(&props::CHARSET),
        Some(&Value::Text("utf8mb4".into()))
    );
    assert!(def.extras.contains(&props::COLLATION));

    let err = t.load_err(&ObjectRef::database("oxbase_no_such_db")).await;
    assert!(matches!(err, Error::NotFound { .. }), "{err}");
}

async fn tables(key: &'static str) {
    let t = Ctx::new(key).await;
    let tables = t.list(ObjectKind::Table, &t.database()).await;
    assert_sorted(&tables);
    for name in ["m2_parent", "m2_child", "m2_doc", "customers", "orders"] {
        let s = find(&tables, name);
        assert_eq!(s.obj, t.table(name));
        assert!(!s.system);
        assert_eq!(
            s.extras.get(&props::STORAGE_ENGINE),
            Some(&Value::Text("InnoDB".into()))
        );
    }
    // Views and sequences are not tables.
    assert!(!names(&tables).contains(&t.name("view").as_str()));
    assert!(!names(&tables).contains(&"m2_seq"));
    let parent = find(&tables, "m2_parent");
    assert_eq!(
        parent.comment.as_deref(),
        Some(t.name("table_comment").as_str())
    );

    // m2_parent
    let ObjectDef::Table(p) = t.load(&t.table("m2_parent")).await else {
        panic!("not a table");
    };
    assert_eq!(p.obj, t.table("m2_parent"));
    assert_eq!(p.comment.as_deref(), Some(t.name("table_comment").as_str()));
    let cols: Vec<&str> = p.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cols, ["id", "code", "note"]);
    let id = &p.columns[0];
    assert_eq!(id.data_type, DataType::INT32);
    assert!(
        id.native_type.to_lowercase().starts_with("int"),
        "{}",
        id.native_type
    );
    assert!(!id.nullable);
    assert!(!id.auto_increment);
    let code = &p.columns[1];
    assert_eq!(code.data_type, DataType::VarChar { length: Some(20) });
    assert!(!code.nullable);
    assert_eq!(code.default, None);
    assert_eq!(p.columns[2].data_type, DataType::Text);
    assert!(p.columns[2].nullable);
    let pk = p.primary_key.as_ref().expect("primary key");
    assert_eq!(pk.columns, ["id"]);
    assert_eq!(pk.name.as_deref(), Some("PRIMARY"));
    let unique = p
        .indexes
        .iter()
        .find(|i| i.columns.len() == 1 && i.columns[0].name == "code")
        .expect("unique index on code");
    assert!(unique.unique);
    assert!(p.indexes.iter().all(|i| i.name != "PRIMARY"));
    let check = p
        .checks
        .iter()
        .find(|k| k.name.as_deref() == Some(t.name("check_parent").as_str()))
        .expect("parent check");
    assert!(check.expression.contains("code"), "{}", check.expression);
    assert!(p.foreign_keys.is_empty());
    assert!(p.extras.contains(&props::COLLATION));

    // m2_child
    let child = t.table("m2_child");
    let ObjectDef::Table(ch) = t.load(&child).await else {
        panic!("not a table");
    };
    let cols: Vec<&str> = ch.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cols, ["id", "parent_id", "qty"]);
    let qty = &ch.columns[2];
    assert_eq!(qty.default.as_deref(), Some("1"));
    assert!(!qty.nullable);
    assert_eq!(ch.foreign_keys.len(), 1);
    let fk = &ch.foreign_keys[0];
    assert_eq!(fk.name.as_deref(), Some(t.name("foreign_key").as_str()));
    assert_eq!(fk.columns, ["parent_id"]);
    assert_eq!(fk.ref_table, t.table("m2_parent"));
    assert_eq!(fk.ref_columns, ["id"]);
    assert_eq!(fk.on_delete, FkAction::Cascade);
    assert!(matches!(
        fk.on_update,
        FkAction::NoAction | FkAction::Restrict
    ));
    let idx = ch
        .indexes
        .iter()
        .find(|i| i.name == t.name("index"))
        .expect("child index");
    assert!(!idx.unique);
    assert_eq!(idx.columns[0].name, "parent_id");
    let checks: Vec<&str> = ch.checks.iter().filter_map(|k| k.name.as_deref()).collect();
    assert_eq!(checks, [t.name("check_child").as_str()]);

    // Children of m2_child, one fetcher each.
    let cols = t.list(ObjectKind::Column, &child).await;
    assert_eq!(names(&cols), ["id", "parent_id", "qty"]);
    assert_eq!(cols[0].obj, child.child(ObjectKind::Column, "id"));
    assert_eq!(
        cols[0].extras.get(&props::PRIMARY_KEY),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        cols[1].extras.get(&props::PRIMARY_KEY),
        Some(&Value::Bool(false))
    );
    assert!(matches!(
        cols[2].extras.get(&props::NATIVE_TYPE),
        Some(Value::Text(ty)) if ty.to_lowercase().starts_with("int")
    ));
    let ObjectDef::Column { table, column } = t.load(&child.child(ObjectKind::Column, "qty")).await
    else {
        panic!("not a column");
    };
    assert_eq!(table, child);
    assert_eq!(column.default.as_deref(), Some("1"));

    let indexes = t.list(ObjectKind::Index, &child).await;
    let primary = find(&indexes, "PRIMARY");
    assert_eq!(
        primary.extras.get(&props::PRIMARY_KEY),
        Some(&Value::Bool(true))
    );
    let named = find(&indexes, &t.name("index"));
    assert_eq!(
        named.extras.get(&props::PRIMARY_KEY),
        Some(&Value::Bool(false))
    );
    assert_eq!(named.obj.parent.as_deref(), Some("m2_child"));
    let ObjectDef::Index { table, index } = t
        .load(&child.child(ObjectKind::Index, t.name("index")))
        .await
    else {
        panic!("not an index");
    };
    assert_eq!(table, child);
    assert_eq!(index.columns.len(), 1);

    let fks = t.list(ObjectKind::ForeignKey, &child).await;
    assert_eq!(names(&fks), [t.name("foreign_key").as_str()]);
    let ObjectDef::ForeignKey { foreign_key, .. } = t.load(&fks[0].obj).await else {
        panic!("not a foreign key");
    };
    assert_eq!(foreign_key.on_delete, FkAction::Cascade);

    let checks = t.list(ObjectKind::Check, &child).await;
    assert_eq!(names(&checks), [t.name("check_child").as_str()]);
    let ObjectDef::Check { check, .. } = t.load(&checks[0].obj).await else {
        panic!("not a check");
    };
    assert!(check.expression.contains("qty"), "{}", check.expression);

    // Missing children.
    for kind in [
        ObjectKind::Column,
        ObjectKind::Index,
        ObjectKind::ForeignKey,
        ObjectKind::Check,
        ObjectKind::Trigger,
    ] {
        let err = t.load_err(&child.child(kind, "m2_no_such")).await;
        assert!(matches!(err, Error::NotFound { .. }), "{kind}: {err}");
    }
}

async fn json_columns(key: &'static str) {
    let t = Ctx::new(key).await;
    let doc_table = t.table("m2_doc");
    let ObjectDef::Table(doc) = t.load(&doc_table).await else {
        panic!("not a table");
    };
    let col = doc.column("doc").expect("doc column");
    if t.mariadb() {
        // MariaDB: JSON is LONGTEXT plus an automatic json_valid check, which is not listed.
        assert_eq!(col.data_type, DataType::Json { binary: false });
        assert_eq!(col.native_type.to_lowercase(), "longtext");
        assert!(doc.checks.is_empty(), "{:?}", doc.checks);
        assert!(t.list(ObjectKind::Check, &doc_table).await.is_empty());
    } else {
        assert_eq!(col.data_type, DataType::Json { binary: true });
        assert_eq!(col.native_type.to_lowercase(), "json");
    }
    let ObjectDef::Column { column, .. } =
        t.load(&doc_table.child(ObjectKind::Column, "doc")).await
    else {
        panic!("not a column");
    };
    assert_eq!(column.data_type, col.data_type);
}

async fn views(key: &'static str) {
    let t = Ctx::new(key).await;
    let view_name = t.name("view");
    let views = t.list(ObjectKind::View, &t.database()).await;
    assert_sorted(&views);
    let v = find(&views, &view_name);
    assert!(!v.system);
    let ObjectDef::View(def) = t.load(&v.obj).await else {
        panic!("not a view");
    };
    assert_eq!(
        def.obj,
        t.database().child(ObjectKind::View, view_name.as_str())
    );
    assert!(
        def.definition.to_lowercase().contains("m2_child"),
        "{}",
        def.definition
    );
    assert!(def.extras.contains(&props::DEFINER));
    assert_eq!(
        def.extras.get(&props::SECURITY),
        Some(&Value::Text("DEFINER".into()))
    );
    let cols = t.list(ObjectKind::Column, &v.obj).await;
    assert_eq!(names(&cols), ["id", "qty", "code"]);
    assert_eq!(cols[0].obj.parent_kind, Some(ObjectKind::View));
    // Views have no indexes, keys, checks or triggers here.
    for kind in [
        ObjectKind::Index,
        ObjectKind::ForeignKey,
        ObjectKind::Check,
        ObjectKind::Trigger,
    ] {
        assert!(t.list(kind, &v.obj).await.is_empty(), "{kind}");
    }
    let err = t
        .load_err(&t.database().child(ObjectKind::View, "m2_no_such_view"))
        .await;
    assert!(matches!(err, Error::NotFound { .. }), "{err}");
    // A table is not a view.
    let err = t
        .load_err(&t.database().child(ObjectKind::View, "m2_parent"))
        .await;
    assert!(matches!(err, Error::NotFound { .. }), "{err}");
}

async fn routines(key: &'static str) {
    let t = Ctx::new(key).await;
    let fn_name = t.name("function");
    let proc_name = t.name("procedure");

    let functions = t.list(ObjectKind::Function, &t.database()).await;
    let f = find(&functions, &fn_name);
    assert_eq!(f.obj.signature, None);
    assert_eq!(f.obj.catalog.as_deref(), Some(t.database().name.as_str()));
    assert!(!names(&functions).contains(&proc_name.as_str()));
    let ObjectDef::Routine(def) = t.load(&f.obj).await else {
        panic!("not a routine");
    };
    assert_eq!(def.obj.kind, ObjectKind::Function);
    assert_eq!(def.args.len(), 1);
    assert_eq!(def.args[0].name.as_deref(), Some("n"));
    assert_eq!(def.args[0].mode, ArgMode::In);
    assert!(def.args[0].native_type.to_lowercase().starts_with("int"));
    assert!(
        def.returns
            .as_deref()
            .is_some_and(|r| r.to_lowercase().starts_with("int")),
        "{:?}",
        def.returns
    );
    assert_eq!(def.language.as_deref(), Some("SQL"));
    let body = def.body.as_deref().expect("the definer sees the body");
    assert!(body.to_uppercase().contains("RETURN"), "{body}");
    let ddl = def.definition.as_deref().expect("definition");
    assert!(ddl.starts_with("CREATE DEFINER="), "{ddl}");
    assert!(ddl.contains(&format!("FUNCTION `{fn_name}`(")), "{ddl}");
    assert!(ddl.ends_with(body), "{ddl}");
    assert_eq!(
        def.extras.get(&props::DETERMINISTIC),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        def.extras.get(&props::SQL_DATA_ACCESS),
        Some(&Value::Text("NO SQL".into()))
    );
    assert_eq!(def.comment.as_deref(), Some("M2 function"));

    let procedures = t.list(ObjectKind::Procedure, &t.database()).await;
    let p = find(&procedures, &proc_name);
    assert!(!names(&procedures).contains(&fn_name.as_str()));
    let ObjectDef::Routine(def) = t.load(&p.obj).await else {
        panic!("not a routine");
    };
    assert_eq!(def.obj.kind, ObjectKind::Procedure);
    assert_eq!(def.returns, None);
    assert_eq!(def.args.len(), 1);
    assert_eq!(def.args[0].mode, ArgMode::In);
    assert!(
        def.definition
            .as_deref()
            .is_some_and(|d| d.contains("PROCEDURE") && d.contains("IN `n`")),
        "{:?}",
        def.definition
    );

    // A function is not a procedure, and gone is gone.
    let err = t
        .load_err(&t.database().child(ObjectKind::Procedure, fn_name.as_str()))
        .await;
    assert!(matches!(err, Error::NotFound { .. }), "{err}");
    let err = t
        .load_err(&t.database().child(ObjectKind::Function, "m2_no_such_fn"))
        .await;
    assert!(matches!(err, Error::NotFound { .. }), "{err}");
}

async fn triggers(key: &'static str) {
    let t = Ctx::new(key).await;
    let table = t.table(&t.name("trigger_table"));
    let trg = t.name("trigger");
    let list = t.list(ObjectKind::Trigger, &table).await;
    assert_eq!(names(&list), [trg.as_str()]);
    assert_eq!(list[0].obj, table.child(ObjectKind::Trigger, trg.as_str()));
    let ObjectDef::Trigger(def) = t.load(&list[0].obj).await else {
        panic!("not a trigger");
    };
    assert_eq!(def.obj, list[0].obj);
    assert_eq!(def.timing, TriggerTiming::Before);
    assert_eq!(def.events, [TriggerEvent::Insert]);
    assert_eq!(def.level, TriggerLevel::Row);
    assert!(def.enabled);
    let body = def.body.as_deref().expect("body");
    assert!(body.to_uppercase().contains("NEW.QTY"), "{body}");
    let ddl = def.definition.as_deref().expect("definition");
    assert!(
        ddl.contains(&format!(
            "TRIGGER `{trg}` BEFORE INSERT ON `{}` FOR EACH ROW",
            table.name
        )),
        "{ddl}"
    );
    // The trigger is on m2_child, not m2_parent.
    let err = t
        .load_err(
            &t.table("m2_parent")
                .child(ObjectKind::Trigger, trg.as_str()),
        )
        .await;
    assert!(matches!(err, Error::NotFound { .. }), "{err}");
    assert!(
        t.list(ObjectKind::Trigger, &t.table("m2_parent"))
            .await
            .is_empty()
    );
}

async fn sequences(key: &'static str) {
    let t = Ctx::new(key).await;
    let v = t.c.server_version().clone();
    let supported = t.reg.supports_kind(t.engine(), ObjectKind::Sequence, &v);
    if !t.mariadb() {
        assert!(!supported);
        assert!(t.reg.fetcher(t.engine(), ObjectKind::Sequence).is_err());
        return;
    }
    assert!(supported);
    let name = t.name("sequence");
    let list = t.list(ObjectKind::Sequence, &t.database()).await;
    assert_eq!(names(&list), [name.as_str()]);
    let ObjectDef::Sequence(def) = t.load(&list[0].obj).await else {
        panic!("not a sequence");
    };
    assert_eq!(
        def.obj,
        t.database().child(ObjectKind::Sequence, name.as_str())
    );
    assert_eq!(def.start, 100);
    assert_eq!(def.increment, 5);
    assert_eq!(def.min_value, Some(1));
    assert!(def.max_value.is_some_and(|m| m >= i128::from(i32::MAX)));
    assert!(!def.cycle);
    let err = t
        .load_err(&t.database().child(ObjectKind::Sequence, "m2_no_such_seq"))
        .await;
    assert!(matches!(err, Error::NotFound { .. }), "{err}");
}

async fn events(key: &'static str) {
    let t = Ctx::new(key).await;
    let list = t.list(ObjectKind::Event, &t.database()).await;
    let e = find(&list, "m2_evt");
    assert_eq!(e.comment.as_deref(), Some("M2 event (never runs)"));
    let ObjectDef::Generic(def) = t.load(&e.obj).await else {
        panic!("an event loads as a GenericDef");
    };
    let ddl = def.definition.as_deref().expect("definition");
    assert!(
        ddl.contains("EVENT `m2_evt` ON SCHEDULE EVERY 1 DAY"),
        "{ddl}"
    );
    assert!(ddl.contains("ON COMPLETION PRESERVE DISABLE"), "{ddl}");
    assert!(
        ddl.ends_with("DO DELETE FROM m2_child WHERE 1 = 0"),
        "{ddl}"
    );
    let err = t
        .load_err(&t.database().child(ObjectKind::Event, "m2_no_such_evt"))
        .await;
    assert!(matches!(err, Error::NotFound { .. }), "{err}");
}

async fn system_objects(key: &'static str) {
    let t = Ctx::new(key).await;
    let is = ObjectRef::database("information_schema");
    let views = t.list(ObjectKind::View, &is).await;
    let tables = t.list(ObjectKind::Table, &is).await;
    let all: Vec<&ObjectSummary> = views.iter().chain(&tables).collect();
    assert!(
        all.iter()
            .any(|s| s.obj.name.eq_ignore_ascii_case("TABLES")),
        "information_schema.TABLES is listed"
    );
    assert!(all.iter().all(|s| s.system));
    let cols = t
        .list(
            ObjectKind::Column,
            &all.iter()
                .find(|s| s.obj.name.eq_ignore_ascii_case("TABLES"))
                .unwrap()
                .obj,
        )
        .await;
    assert!(names(&cols).contains(&"TABLE_NAME"));
    assert!(cols.iter().all(|s| s.system));
}

async fn cancel_and_bound_names(key: &'static str) {
    let t = Ctx::new(key).await;
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    for kind in [ObjectKind::Table, ObjectKind::View, ObjectKind::Function] {
        let err = t
            .fetcher(kind)
            .list(&t.c, &t.database(), cancelled.clone())
            .await
            .unwrap_err();
        assert!(err.is_cancelled(), "{kind}: {err}");
    }
    let err = t
        .fetcher(ObjectKind::Table)
        .load(&t.c, &t.table("m2_parent"), cancelled.clone())
        .await
        .unwrap_err();
    assert!(err.is_cancelled(), "{err}");
    let err = t
        .fetcher(ObjectKind::Database)
        .list(&t.c, &ObjectRef::root(), cancelled)
        .await
        .unwrap_err();
    assert!(err.is_cancelled(), "{err}");

    // Names are data: quotes and comment markers change nothing, they just match nothing.
    for name in [
        "m2_parent' OR '1'='1",
        "m2_parent`; DROP TABLE m2_parent; --",
        "%",
    ] {
        let err = t.load_err(&t.table(name)).await;
        assert!(matches!(err, Error::NotFound { .. }), "{name}: {err}");
    }
    let tricky = ObjectRef::database("oxbase_test' OR '1'='1");
    assert!(t.list(ObjectKind::Table, &tricky).await.is_empty());

    // A ref without a catalog means the session's database.
    let bare = ObjectRef::table(None, None, "m2_parent");
    let ObjectDef::Table(def) = t.load(&bare).await else {
        panic!("not a table");
    };
    assert_eq!(def.obj, t.table("m2_parent"));

    // The session stays usable.
    let tables = t.list(ObjectKind::Table, &t.database()).await;
    assert!(!tables.is_empty());
    t.c.close().await.unwrap();
}

macro_rules! live {
    ($($name:ident),* $(,)?) => {
        mod mysql {
            $(
                #[tokio::test]
                #[ignore = "needs the mysql fixture (scripts/fixtures-up.sh mysql)"]
                async fn $name() {
                    super::$name("mysql").await;
                }
            )*
        }
        mod mariadb {
            $(
                #[tokio::test]
                #[ignore = "needs the mariadb fixture (scripts/fixtures-up.sh mariadb)"]
                async fn $name() {
                    super::$name("mariadb").await;
                }
            )*
        }
    };
}

live!(
    databases,
    tables,
    json_columns,
    views,
    routines,
    triggers,
    sequences,
    events,
    system_objects,
    cancel_and_bound_names,
);
