//! Structure compare and sync scripts on hand-built snapshots of the `[m8]` pair
//! (`tests/fixtures.toml`), with every engine's real dialect cells: the item statuses, the
//! order of the script (dependents' drops first, foreign keys added last, a SQLite rebuild kept
//! together) and the golden scripts `tests/golden/structure_<engine>.sql`.
//!
//! The snapshots model the designed differences: `customers` gains a column, `products`
//! changes a column type, `orders` changes its foreign key, `select` (a reserved word) gains a
//! column named `order`; `order_audit`, `order_items` and `MixedCase` are new, `order_lines`
//! and `legacy_notes` exist only in the target; the view `active_customers` reads the new
//! column; the sequence `invoice_seq`, the function `order_total` and the trigger
//! `orders_audit` are new (where the engine has them).
//!
//! A golden file that is missing or differs fails the test and prints the generated script
//! between `BEGIN GOLDEN` / `END GOLDEN` markers.

use std::path::Path;

use oxbase_dialect::DialectRegistry;
use oxbase_diff::{DiffStatus, SchemaSnapshot, StructureOptions, compare, sync_script};
use oxbase_types::{
    ArgMode, ColumnDef, DataType, Engine, Extras, FkAction, ForeignKeyDef, ObjectDef, ObjectKind,
    ObjectRef, PrimaryKeyDef, RoutineArg, RoutineDef, SequenceDef, ServerVersion, StepPhase,
    TableDef, TriggerDef, TriggerEvent, TriggerLevel, TriggerTiming, ViewDef, props,
};
use pretty_assertions::assert_eq;

fn registry() -> DialectRegistry {
    let mut reg = DialectRegistry::new();
    oxbase_pg::dialect::register(&mut reg);
    oxbase_mysql::dialect::register(&mut reg);
    oxbase_mssql::dialect::register(&mut reg);
    oxbase_sqlite::dialect::register(&mut reg);
    reg
}

/// One engine's pair of containers.
struct Pair {
    key: &'static str,
    engine: Engine,
    version: ServerVersion,
    source: ObjectRef,
    target: ObjectRef,
}

fn pair(key: &'static str) -> Pair {
    let (engine, version, source, target) = match key {
        "postgres" => (
            Engine::Postgres,
            ServerVersion::new(18, 6, 0),
            ObjectRef::schema(Some("oxbase_test"), "m8_src"),
            ObjectRef::schema(Some("oxbase_test"), "m8_tgt"),
        ),
        "mysql" => (
            Engine::MySql,
            ServerVersion::new(8, 4, 13),
            ObjectRef::database("m8_src"),
            ObjectRef::database("m8_tgt"),
        ),
        "mariadb" => (
            Engine::MariaDb,
            ServerVersion::new(11, 8, 9),
            ObjectRef::database("m8_src"),
            ObjectRef::database("m8_tgt"),
        ),
        "mssql" => (
            Engine::MsSql,
            ServerVersion::new(17, 0, 0),
            ObjectRef::schema(Some("oxbase_test"), "m8_src"),
            ObjectRef::schema(Some("oxbase_test"), "m8_tgt"),
        ),
        _ => (
            Engine::Sqlite,
            ServerVersion::new(3, 50, 0),
            ObjectRef::database("main"),
            ObjectRef::database("main"),
        ),
    };
    Pair {
        key,
        engine,
        version,
        source,
        target,
    }
}

fn col(name: &str, data_type: DataType, nullable: bool) -> ColumnDef {
    let mut c = ColumnDef::new(name, data_type);
    c.nullable = nullable;
    c
}

fn varchar(length: u32) -> DataType {
    DataType::VarChar {
        length: Some(length),
    }
}

fn decimal(precision: u32, scale: u32) -> DataType {
    DataType::Decimal {
        precision: Some(precision),
        scale: Some(scale),
    }
}

fn table(container: &ObjectRef, name: &str, columns: Vec<ColumnDef>) -> TableDef {
    let mut t = TableDef::new(container.child(ObjectKind::Table, name));
    let key = columns[0].name.clone();
    t.columns = columns;
    t.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec![key],
    });
    t
}

fn fk(
    container: &ObjectRef,
    name: &str,
    column: &str,
    parent: &str,
    on_delete: FkAction,
) -> ForeignKeyDef {
    ForeignKeyDef {
        name: Some(name.into()),
        columns: vec![column.into()],
        ref_table: container.child(ObjectKind::Table, parent),
        ref_columns: vec!["id".into()],
        on_delete,
        on_update: FkAction::NoAction,
        extras: Extras::new(),
    }
}

fn id() -> ColumnDef {
    col("id", DataType::INT32, false)
}

/// The tables of one side (`source`: the newer design).
fn tables(c: &ObjectRef, source: bool) -> Vec<TableDef> {
    let mut out = Vec::new();
    let mut customers = vec![id(), col("name", varchar(100), false)];
    if source {
        customers.push(col("email", varchar(200), true));
    }
    out.push(table(c, "customers", customers));
    out.push(table(
        c,
        "products",
        vec![
            id(),
            col("name", varchar(100), false),
            col(
                "price",
                if source {
                    decimal(10, 2)
                } else {
                    decimal(8, 2)
                },
                true,
            ),
        ],
    ));
    let mut orders = table(
        c,
        "orders",
        vec![
            id(),
            col("customer_id", DataType::INT32, false),
            col("total", decimal(12, 2), true),
        ],
    );
    orders.foreign_keys.push(fk(
        c,
        "orders_customer_fk",
        "customer_id",
        "customers",
        if source {
            FkAction::Cascade
        } else {
            FkAction::NoAction
        },
    ));
    out.push(orders);
    let mut select = vec![id(), col("from", varchar(10), true)];
    if source {
        select.push(col("order", DataType::INT32, true));
    }
    out.push(table(c, "select", select));
    if source {
        out.push(table(
            c,
            "order_audit",
            vec![
                id(),
                col("order_id", DataType::INT32, false),
                col("at", DataType::Timestamp { precision: None }, true),
            ],
        ));
        // Listed before its parents: the script creates the parents first.
        let mut items = table(
            c,
            "order_items",
            vec![
                id(),
                col("order_id", DataType::INT32, false),
                col("product_id", DataType::INT32, false),
                col("qty", DataType::INT32, false),
            ],
        );
        items.foreign_keys.push(fk(
            c,
            "order_items_order_fk",
            "order_id",
            "orders",
            FkAction::Cascade,
        ));
        items.foreign_keys.push(fk(
            c,
            "order_items_product_fk",
            "product_id",
            "products",
            FkAction::NoAction,
        ));
        out.insert(0, items);
        out.push(table(
            c,
            "MixedCase",
            vec![
                col("Id", DataType::INT32, false),
                col("Label", varchar(20), true),
            ],
        ));
    } else {
        let mut lines = table(
            c,
            "order_lines",
            vec![id(), col("order_id", DataType::INT32, false)],
        );
        lines.foreign_keys.push(fk(
            c,
            "order_lines_order_fk",
            "order_id",
            "orders",
            FkAction::NoAction,
        ));
        out.push(lines);
        out.push(table(
            c,
            "legacy_notes",
            vec![id(), col("note", DataType::Text, true)],
        ));
    }
    out
}

/// The view, sequence, routines and trigger of one side, in the engine's own text.
fn others(p: &Pair, c: &ObjectRef, source: bool) -> Vec<ObjectDef> {
    let schema = if source { "m8_src" } else { "m8_tgt" };
    let mut out = Vec::new();
    let view = |definition: String| {
        ObjectDef::View(ViewDef {
            obj: c.child(ObjectKind::View, "active_customers"),
            definition,
            columns: Vec::new(),
            comment: None,
            extras: Extras::new(),
        })
    };
    let orders = c.child(ObjectKind::Table, "orders");
    match p.engine {
        Engine::Postgres => {
            let email = if source { ",\n    customers.email" } else { "" };
            out.push(view(format!(
                "SELECT customers.id,\n    customers.name{email}\n   FROM {schema}.customers"
            )));
            let mut audit = RoutineDef::new(
                c.child(ObjectKind::Function, "audit_order")
                    .with_signature(""),
            );
            audit.returns = Some("trigger".into());
            audit.language = Some("plpgsql".into());
            audit.body = Some(format!(
                "\nBEGIN\n  INSERT INTO {schema}.order_audit (order_id) VALUES (NEW.id);\n  RETURN NEW;\nEND\n"
            ));
            audit.extras.insert(props::VOLATILITY, "volatile");
            audit.extras.insert(props::STRICT_INPUT, false);
            audit.extras.insert(props::SECURITY, "invoker");
            audit.extras.insert(oxbase_pg::dialect::PARALLEL, "unsafe");
            out.push(ObjectDef::Routine(audit.clone()));
            if source {
                let mut seq = SequenceDef::new(c.child(ObjectKind::Sequence, "invoice_seq"));
                seq.data_type = Some("bigint".into());
                seq.start = 1000;
                seq.min_value = Some(1);
                seq.max_value = Some(9_223_372_036_854_775_807);
                seq.cache = Some(1);
                out.push(ObjectDef::Sequence(seq));
                let mut total = audit;
                total.obj = c
                    .child(ObjectKind::Function, "order_total")
                    .with_signature("p_id integer");
                total.args = vec![RoutineArg {
                    name: Some("p_id".into()),
                    mode: ArgMode::In,
                    native_type: "integer".into(),
                    default: None,
                }];
                total.returns = Some("numeric".into());
                total.body = Some(format!(
                    "\nBEGIN\n  RETURN (SELECT sum(qty) FROM {schema}.order_items WHERE order_id = p_id);\nEND\n"
                ));
                total.extras.insert(props::VOLATILITY, "stable");
                out.push(ObjectDef::Routine(total));
                out.push(ObjectDef::Trigger(TriggerDef {
                    obj: orders.child(ObjectKind::Trigger, "orders_audit"),
                    timing: TriggerTiming::After,
                    events: vec![TriggerEvent::Insert],
                    update_columns: Vec::new(),
                    level: TriggerLevel::Row,
                    condition: None,
                    function: Some(c.child(ObjectKind::Function, "audit_order")),
                    body: None,
                    definition: Some(format!(
                        "CREATE TRIGGER orders_audit AFTER INSERT ON {schema}.orders FOR EACH ROW EXECUTE FUNCTION {schema}.audit_order()"
                    )),
                    enabled: true,
                    comment: None,
                    extras: Extras::new(),
                }));
            }
        }
        Engine::MySql | Engine::MariaDb => {
            let email = if source {
                format!(",`{schema}`.`customers`.`email` AS `email`")
            } else {
                String::new()
            };
            out.push(view(format!(
                "select `{schema}`.`customers`.`id` AS `id`,`{schema}`.`customers`.`name` AS `name`{email} from `{schema}`.`customers`"
            )));
            if source {
                if p.engine == Engine::MariaDb {
                    let mut seq = SequenceDef::new(c.child(ObjectKind::Sequence, "invoice_seq"));
                    seq.data_type = Some("bigint".into());
                    seq.start = 1000;
                    seq.min_value = Some(1);
                    seq.max_value = Some(9_999_999);
                    out.push(ObjectDef::Sequence(seq));
                }
                let mut total = RoutineDef::new(c.child(ObjectKind::Function, "order_total"));
                total.args.push(RoutineArg {
                    name: Some("p_id".into()),
                    mode: ArgMode::In,
                    native_type: "int".into(),
                    default: None,
                });
                total.returns = Some("decimal(12,2)".into());
                total.language = Some("SQL".into());
                total.body = Some(format!(
                    "RETURN (SELECT SUM(qty) FROM `{schema}`.`order_items` WHERE order_id = p_id)"
                ));
                total.extras.insert(props::DETERMINISTIC, false);
                total
                    .extras
                    .insert(props::SQL_DATA_ACCESS, "READS SQL DATA");
                total.extras.insert(props::SECURITY, "DEFINER");
                out.push(ObjectDef::Routine(total));
                out.push(ObjectDef::Trigger(TriggerDef {
                    obj: orders.child(ObjectKind::Trigger, "orders_audit"),
                    timing: TriggerTiming::After,
                    events: vec![TriggerEvent::Insert],
                    update_columns: Vec::new(),
                    level: TriggerLevel::Row,
                    condition: None,
                    function: None,
                    body: Some(format!(
                        "INSERT INTO `{schema}`.`order_audit` (order_id) VALUES (NEW.id)"
                    )),
                    definition: None,
                    enabled: true,
                    comment: None,
                    extras: Extras::new(),
                }));
            }
        }
        Engine::MsSql => {
            let email = if source { ", email" } else { "" };
            out.push(view(format!(
                "SELECT id, name{email} FROM [{schema}].[customers]"
            )));
            if source {
                let mut seq = SequenceDef::new(c.child(ObjectKind::Sequence, "invoice_seq"));
                seq.data_type = Some("bigint".into());
                seq.start = 1000;
                seq.min_value = Some(1);
                seq.max_value = Some(9_999_999);
                out.push(ObjectDef::Sequence(seq));
                let mut total = RoutineDef::new(c.child(ObjectKind::Function, "order_total"));
                total.language = Some("SQL".into());
                total.definition = Some(format!(
                    "CREATE FUNCTION [{schema}].[order_total](@p_id int) RETURNS int AS\nBEGIN\n    RETURN (SELECT SUM(qty) FROM [{schema}].[order_items] WHERE order_id = @p_id);\nEND"
                ));
                out.push(ObjectDef::Routine(total));
                out.push(ObjectDef::Trigger(TriggerDef {
                    obj: orders.child(ObjectKind::Trigger, "orders_audit"),
                    timing: TriggerTiming::After,
                    events: vec![TriggerEvent::Insert],
                    update_columns: Vec::new(),
                    level: TriggerLevel::Statement,
                    condition: None,
                    function: None,
                    body: None,
                    definition: Some(format!(
                        "CREATE TRIGGER [{schema}].[orders_audit] ON [{schema}].[orders] AFTER INSERT AS\nINSERT INTO [{schema}].[order_audit] (order_id) SELECT id FROM inserted"
                    )),
                    enabled: true,
                    comment: None,
                    extras: Extras::new(),
                }));
            }
        }
        _ => {
            let email = if source { ", email" } else { "" };
            out.push(view(format!("SELECT id, name{email} FROM customers")));
            if source {
                out.push(ObjectDef::Trigger(TriggerDef {
                    obj: orders.child(ObjectKind::Trigger, "orders_audit"),
                    timing: TriggerTiming::After,
                    events: vec![TriggerEvent::Insert],
                    update_columns: Vec::new(),
                    level: TriggerLevel::Row,
                    condition: None,
                    function: None,
                    body: Some("INSERT INTO order_audit (order_id) VALUES (NEW.id);".into()),
                    definition: Some(
                        "CREATE TRIGGER orders_audit AFTER INSERT ON orders BEGIN INSERT INTO order_audit (order_id) VALUES (NEW.id); END"
                            .into(),
                    ),
                    enabled: true,
                    comment: None,
                    extras: Extras::new(),
                }));
            }
        }
    }
    out
}

fn snapshot(p: &Pair, source: bool) -> SchemaSnapshot {
    let c = if source { &p.source } else { &p.target };
    let mut objects: Vec<ObjectDef> = tables(c, source)
        .into_iter()
        .map(ObjectDef::Table)
        .collect();
    objects.extend(others(p, c, source));
    SchemaSnapshot {
        engine: p.engine,
        version: p.version.clone(),
        container: c.clone(),
        objects,
        skipped: Vec::new(),
    }
}

/// The expected status of every reported item (`same` items left out).
fn expected_items(p: &Pair) -> Vec<(&'static str, DiffStatus)> {
    let mut out = vec![
        ("table:MixedCase", DiffStatus::OnlyInSource),
        ("table:customers", DiffStatus::Different),
        ("table:legacy_notes", DiffStatus::OnlyInTarget),
        ("table:order_audit", DiffStatus::OnlyInSource),
        ("table:order_items", DiffStatus::OnlyInSource),
        ("table:order_lines", DiffStatus::OnlyInTarget),
        ("table:orders", DiffStatus::Different),
        ("table:products", DiffStatus::Different),
        ("table:select", DiffStatus::Different),
        ("view:active_customers", DiffStatus::Different),
    ];
    if matches!(p.engine, Engine::Postgres | Engine::MariaDb | Engine::MsSql) {
        out.push(("sequence:invoice_seq", DiffStatus::OnlyInSource));
    }
    match p.engine {
        Engine::Postgres => out.push((
            "function:order_total(p_id integer)",
            DiffStatus::OnlyInSource,
        )),
        Engine::Sqlite => {}
        _ => out.push(("function:order_total", DiffStatus::OnlyInSource)),
    }
    out.push(("trigger:orders.orders_audit", DiffStatus::OnlyInSource));
    out
}

fn options(include_drops: bool) -> StructureOptions {
    StructureOptions {
        include_drops,
        ..StructureOptions::default()
    }
}

fn check_golden(key: &str, sql: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("structure_{key}.sql"));
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    if expected != sql {
        eprintln!("===== BEGIN GOLDEN {key} =====\n{sql}===== END GOLDEN {key} =====");
        panic!(
            "{} is missing or differs from the generated script (printed above)",
            path.display()
        );
    }
}

fn first(sql: &[&str], needle: &str) -> usize {
    sql.iter()
        .position(|s| s.contains(needle))
        .unwrap_or_else(|| panic!("no step contains {needle:?}: {sql:#?}"))
}

fn last(sql: &[&str], needle: &str) -> usize {
    sql.iter()
        .rposition(|s| s.contains(needle))
        .unwrap_or_else(|| panic!("no step contains {needle:?}: {sql:#?}"))
}

/// Compare, statuses, script flags and order for one engine; returns the script text.
fn run(key: &'static str) -> String {
    let reg = registry();
    let p = pair(key);
    let source = snapshot(&p, true);
    let target = snapshot(&p, false);
    let opts = options(true);
    let diff = compare(&reg, &source, &target, &opts).unwrap();
    let reported: Vec<(&str, DiffStatus)> = diff
        .items
        .iter()
        .filter(|i| i.status != DiffStatus::Same)
        .map(|i| (i.id.as_str(), i.status))
        .collect();
    let mut expected = expected_items(&p);
    expected.sort_by_key(|a| a.0);
    let mut sorted = reported.clone();
    sorted.sort_by_key(|a| a.0);
    assert_eq!(sorted, expected, "{key}");
    assert!(!diff.is_in_sync(true), "{key}");
    // Every item carries both sides' CREATE text where it exists.
    for item in &diff.items {
        if item.status != DiffStatus::OnlyInTarget {
            assert!(
                item.source_ddl.is_some(),
                "{key}: {} has no source DDL",
                item.id
            );
        }
    }

    let script = sync_script(&reg, &diff, &source, &target, &[], &opts).unwrap();
    assert!(script.destructive, "{key}");
    assert_eq!(
        script.transactional_ddl,
        !matches!(p.engine, Engine::MySql | Engine::MariaDb),
        "{key}"
    );
    assert_eq!(script.atomic, script.transactional_ddl, "{key}");
    // Every reported item is in the script.
    for (id, _) in &reported {
        assert!(
            script.items.iter().any(|i| i == id),
            "{key}: {id} not scripted: {:?}",
            script.items
        );
    }
    let sql: Vec<&str> = script.script.ordered().map(|s| s.sql.as_str()).collect();
    // Dependents go first, the view comes back after the tables, the trigger last.
    let view_drop = first(&sql, "DROP VIEW");
    let view_create = first(&sql, "CREATE VIEW");
    let table_drop = first(&sql, "legacy_notes");
    let table_create = first(&sql, "CREATE TABLE");
    let trigger = first(&sql, "CREATE TRIGGER");
    assert!(
        view_drop < table_drop && table_drop < table_create,
        "{key}: {sql:#?}"
    );
    assert!(
        table_create < view_create && view_create < trigger,
        "{key}: {sql:#?}"
    );
    if p.engine == Engine::Sqlite {
        // The rebuild of a table stays together; foreign keys stay inline.
        let start = first(&sql, "products__oxbase_new");
        let end = last(&sql, "products__oxbase_new");
        assert!(
            sql[start..=end]
                .iter()
                .all(|s| !s.contains("orders__oxbase_new")),
            "{key}: {sql:#?}"
        );
        let prepares = script
            .script
            .steps
            .iter()
            .filter(|s| s.phase == StepPhase::Prepare && s.sql.trim() == "PRAGMA foreign_keys")
            .count();
        assert_eq!(
            prepares, 1,
            "{key}: one foreign-key switch for every rebuild"
        );
    } else {
        // Foreign keys: changed ones dropped before the tables change, added after all exist.
        let fk_add = first(&sql, "order_items_order_fk");
        assert!(last(&sql, "CREATE TABLE") < fk_add, "{key}: {sql:#?}");
        assert!(fk_add < view_create, "{key}: {sql:#?}");
        let fk_drop = first(&sql, "orders_customer_fk");
        assert!(fk_drop < table_drop, "{key}: {sql:#?}");
        // The sequence before the tables, the function before the view.
        if let Some(seq) = sql.iter().position(|s| s.contains("CREATE SEQUENCE")) {
            assert!(table_drop < seq && seq < table_create, "{key}: {sql:#?}");
        }
        assert!(first(&sql, "order_total") < view_create, "{key}: {sql:#?}");
    }
    script.sql
}

#[test]
fn postgres_script() {
    let sql = run("postgres");
    check_golden("postgres", &sql);
}

#[test]
fn mysql_script() {
    let sql = run("mysql");
    check_golden("mysql", &sql);
}

#[test]
fn mariadb_script() {
    let sql = run("mariadb");
    check_golden("mariadb", &sql);
}

#[test]
fn mssql_script() {
    let sql = run("mssql");
    check_golden("mssql", &sql);
}

#[test]
fn sqlite_script() {
    let sql = run("sqlite");
    check_golden("sqlite", &sql);
}

#[test]
fn drops_only_when_asked_and_selections_pick_items() {
    let reg = registry();
    let p = pair("postgres");
    let source = snapshot(&p, true);
    let target = snapshot(&p, false);
    let opts = options(false);
    let diff = compare(&reg, &source, &target, &opts).unwrap();
    let script = sync_script(&reg, &diff, &source, &target, &[], &opts).unwrap();
    assert!(!script.sql.contains("legacy_notes") && !script.sql.contains("order_lines"));
    // One item: the view is dropped and created again, nothing else.
    let script = sync_script(
        &reg,
        &diff,
        &source,
        &target,
        &["view:active_customers".to_owned()],
        &opts,
    )
    .unwrap();
    assert_eq!(script.items, ["view:active_customers"]);
    assert_eq!(script.script.len(), 2);
    assert!(!script.destructive);
    // A table change re-creates the view that reads it, though the view is not selected.
    let script = sync_script(
        &reg,
        &diff,
        &source,
        &target,
        &["table:customers".to_owned()],
        &opts,
    )
    .unwrap();
    assert_eq!(script.items, ["view:active_customers", "table:customers"]);
    // Unknown ids are refused.
    assert!(
        sync_script(
            &reg,
            &diff,
            &source,
            &target,
            &["table:nope".to_owned()],
            &opts
        )
        .is_err()
    );
    // Excluding a table excludes its triggers.
    let excluded = StructureOptions {
        exclude: vec![ObjectRef::table(None, None, "orders")],
        ..options(true)
    };
    let diff = compare(&reg, &source, &target, &excluded).unwrap();
    assert!(
        diff.item("table:orders").is_none() && diff.item("trigger:orders.orders_audit").is_none()
    );
    // Engines must match.
    let mut other = target.clone();
    other.engine = Engine::MySql;
    assert!(compare(&reg, &source, &other, &opts).is_err());
}

#[test]
fn switched_off_properties_are_not_differences() {
    let reg = registry();
    let p = pair("postgres");
    let mut source = snapshot(&p, true);
    let mut target = snapshot(&p, true);
    target.container = p.target.clone();
    for def in &mut target.objects {
        oxbase_types::relocate_def(
            def,
            &oxbase_types::Location::of_container(&p.source),
            &oxbase_types::Location::of_container(&p.target),
        );
    }
    // Same objects except comments and owners.
    if let ObjectDef::Table(t) = &mut source.objects[1] {
        t.comment = Some("new comment".into());
        t.extras.insert(props::OWNER, "alice");
    }
    if let ObjectDef::Table(t) = &mut target.objects[1] {
        t.extras.insert(props::OWNER, "bob");
    }
    let tables_only = StructureOptions {
        kinds: vec![ObjectKind::Table],
        ..options(true)
    };
    let quiet = StructureOptions {
        compare_comments: false,
        ..tables_only.clone()
    };
    let diff = compare(&reg, &source, &target, &quiet).unwrap();
    assert!(
        diff.is_in_sync(true),
        "{:#?}",
        diff.items
            .iter()
            .filter(|i| i.status != DiffStatus::Same)
            .collect::<Vec<_>>()
    );
    let diff = compare(&reg, &source, &target, &tables_only).unwrap();
    assert!(!diff.is_in_sync(true));
    assert_eq!(
        diff.items
            .iter()
            .filter(|i| i.status != DiffStatus::Same)
            .map(|i| i.id.as_str())
            .collect::<Vec<_>>(),
        ["table:customers"]
    );
}
