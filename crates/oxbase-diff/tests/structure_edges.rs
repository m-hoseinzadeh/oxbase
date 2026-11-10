//! Structure sync scripts for the cases that make a naive order fail on the server, on
//! hand-built snapshots with every engine's real dialect cells:
//!
//! - a widened parent key (`customers.id` and `orders.customer_id` INT to BIGINT): the foreign
//!   key that references it is dropped before the change and added back after it, also when
//!   only the parent is selected (golden scripts `tests/golden/structure_fk_<engine>.sql`);
//! - a column default moved to a new sequence: the old sequence is dropped after the table
//!   stopped using it (golden `tests/golden/structure_seq_postgres.sql`; the order is checked
//!   on SQL Server and MariaDB too);
//! - a routine a new table's default calls is created before the table;
//! - a view that reads a changed table but is not selected is created again from the target's
//!   own definition;
//! - a table drop that a foreign key of a table that stays still needs is refused;
//! - collations switched off: a column's character set is not a difference and a script never
//!   writes the source's character set with the target's collation.

use std::path::Path;

use oxbase_dialect::DialectRegistry;
use oxbase_diff::{DiffStatus, SchemaSnapshot, StructureOptions, compare, sync_script};
use oxbase_types::{
    ArgMode, ColumnDef, DataType, Engine, Extras, FkAction, ForeignKeyDef, ObjectDef, ObjectKind,
    ObjectRef, PrimaryKeyDef, RoutineArg, RoutineDef, SequenceDef, ServerVersion, TableDef, Value,
    ViewDef, props,
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

struct Side {
    engine: Engine,
    version: ServerVersion,
    source: ObjectRef,
    target: ObjectRef,
}

fn side(engine: Engine) -> Side {
    let (version, source, target) = match engine {
        Engine::Postgres => (
            ServerVersion::new(18, 6, 0),
            ObjectRef::schema(Some("oxbase_test"), "m8_src"),
            ObjectRef::schema(Some("oxbase_test"), "m8_tgt"),
        ),
        Engine::MySql => (
            ServerVersion::new(8, 4, 13),
            ObjectRef::database("m8_src"),
            ObjectRef::database("m8_tgt"),
        ),
        Engine::MariaDb => (
            ServerVersion::new(11, 8, 9),
            ObjectRef::database("m8_src"),
            ObjectRef::database("m8_tgt"),
        ),
        Engine::MsSql => (
            ServerVersion::new(17, 0, 0),
            ObjectRef::schema(Some("oxbase_test"), "m8_src"),
            ObjectRef::schema(Some("oxbase_test"), "m8_tgt"),
        ),
        _ => (
            ServerVersion::new(3, 50, 0),
            ObjectRef::database("main"),
            ObjectRef::database("main"),
        ),
    };
    Side {
        engine,
        version,
        source,
        target,
    }
}

fn key(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "postgres",
        Engine::MySql => "mysql",
        Engine::MariaDb => "mariadb",
        Engine::MsSql => "mssql",
        _ => "sqlite",
    }
}

fn snapshot(s: &Side, source: bool, objects: Vec<ObjectDef>) -> SchemaSnapshot {
    SchemaSnapshot {
        engine: s.engine,
        version: s.version.clone(),
        container: if source {
            s.source.clone()
        } else {
            s.target.clone()
        },
        objects,
        skipped: Vec::new(),
    }
}

fn col(name: &str, data_type: DataType, nullable: bool) -> ColumnDef {
    let mut c = ColumnDef::new(name, data_type);
    c.nullable = nullable;
    c
}

fn table(container: &ObjectRef, name: &str, columns: Vec<ColumnDef>) -> TableDef {
    let mut t = TableDef::new(container.child(ObjectKind::Table, name));
    let key = columns[0].name.clone();
    t.columns = columns;
    t.primary_key = Some(PrimaryKeyDef {
        name: Some(format!("pk_{name}")),
        columns: vec![key],
    });
    t
}

fn options(include_drops: bool) -> StructureOptions {
    StructureOptions {
        include_drops,
        ..StructureOptions::default()
    }
}

/// Compares `sql` with golden file `name`; a missing or different file prints the script
/// and is collected into `failed` (the test fails once every engine printed its script).
fn check_golden(name: &str, sql: &str, failed: &mut Vec<String>) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.sql"));
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    if expected != sql {
        eprintln!("===== BEGIN GOLDEN {name} =====\n{sql}===== END GOLDEN {name} =====");
        failed.push(path.display().to_string());
    }
}

fn position(sql: &[&str], what: &str, found: impl Fn(&str) -> bool) -> usize {
    sql.iter()
        .position(|&s| found(s))
        .unwrap_or_else(|| panic!("no step {what}: {sql:#?}"))
}

/// `customers` and `orders` of one side: the parent key and the child column are INT, or
/// BIGINT on the source side when `wide`.
fn keyed_tables(c: &ObjectRef, wide: bool) -> Vec<ObjectDef> {
    let int = if wide {
        DataType::INT64
    } else {
        DataType::INT32
    };
    let customers = table(
        c,
        "customers",
        vec![
            col("id", int.clone(), false),
            col("name", DataType::VarChar { length: Some(100) }, false),
        ],
    );
    let mut orders = table(
        c,
        "orders",
        vec![
            col("id", DataType::INT32, false),
            col("customer_id", int, false),
        ],
    );
    orders.foreign_keys.push(ForeignKeyDef {
        name: Some("orders_customer_fk".into()),
        columns: vec!["customer_id".into()],
        ref_table: c.child(ObjectKind::Table, "customers"),
        ref_columns: vec!["id".into()],
        on_delete: FkAction::NoAction,
        on_update: FkAction::NoAction,
        extras: Extras::new(),
    });
    vec![ObjectDef::Table(customers), ObjectDef::Table(orders)]
}

/// Drop of the foreign key, the parent's change and the key added back, in that order.
fn check_fk_order(k: &str, sql: &[&str]) {
    let drop = position(sql, "dropping the foreign key", |s| {
        s.contains("orders_customer_fk") && s.to_uppercase().contains("DROP")
    });
    let add = sql
        .iter()
        .rposition(|s| s.contains("orders_customer_fk") && s.to_uppercase().contains("ADD"))
        .unwrap_or_else(|| panic!("{k}: the foreign key is not added back: {sql:#?}"));
    let parent = position(sql, "changing customers", |s| {
        s.contains("customers") && !s.contains("orders_customer_fk")
    });
    assert!(drop < parent && parent < add, "{k}: {sql:#?}");
}

#[test]
fn foreign_keys_on_a_widened_parent_key_are_dropped_and_added_back() {
    let reg = registry();
    let mut failed = Vec::new();
    for engine in [
        Engine::Postgres,
        Engine::MySql,
        Engine::MariaDb,
        Engine::MsSql,
    ] {
        let k = key(engine);
        let s = side(engine);
        let source = snapshot(&s, true, keyed_tables(&s.source, true));
        let target = snapshot(&s, false, keyed_tables(&s.target, false));
        let opts = options(true);
        let diff = compare(&reg, &source, &target, &opts).unwrap();
        for id in ["table:customers", "table:orders"] {
            assert_eq!(
                diff.item(id).map(|i| i.status),
                Some(DiffStatus::Different),
                "{k}: {id}"
            );
        }
        // Both tables selected: the child's own alter drops and adds the key.
        let script = sync_script(&reg, &diff, &source, &target, &[], &opts).unwrap();
        let sql: Vec<&str> = script.script.ordered().map(|s| s.sql.as_str()).collect();
        check_fk_order(k, &sql);
        check_golden(&format!("structure_fk_{k}"), &script.sql, &mut failed);

        // Only the parent selected: the child's key (as the target has it) still goes and
        // comes back.
        let script = sync_script(
            &reg,
            &diff,
            &source,
            &target,
            &["table:customers".to_owned()],
            &opts,
        )
        .unwrap();
        assert_eq!(script.items, ["table:customers"], "{k}");
        let sql: Vec<&str> = script.script.ordered().map(|s| s.sql.as_str()).collect();
        check_fk_order(k, &sql);
    }
    assert_eq!(
        failed,
        Vec::<String>::new(),
        "golden scripts differ (printed above)"
    );
}

/// `orders.id` takes its default from `seq_name` (the engine's own spelling).
fn sequence_side(s: &Side, source: bool) -> Vec<ObjectDef> {
    let c = if source { &s.source } else { &s.target };
    let schema = &c.name;
    let seq_name = if source { "new_seq" } else { "old_seq" };
    let default = match s.engine {
        Engine::Postgres => format!("nextval('{schema}.{seq_name}'::regclass)"),
        Engine::MsSql => format!("(NEXT VALUE FOR [{schema}].[{seq_name}])"),
        _ => format!("nextval(`{schema}`.`{seq_name}`)"),
    };
    let mut id = col("id", DataType::INT64, false);
    id.default = Some(default);
    let orders = table(c, "orders", vec![id]);
    let mut seq = SequenceDef::new(c.child(ObjectKind::Sequence, seq_name));
    seq.data_type = Some("bigint".into());
    seq.start = 1;
    seq.min_value = Some(1);
    seq.max_value = Some(9_999_999);
    vec![ObjectDef::Table(orders), ObjectDef::Sequence(seq)]
}

#[test]
fn a_sequence_only_in_the_target_goes_after_the_tables_stop_using_it() {
    let reg = registry();
    for engine in [Engine::Postgres, Engine::MsSql, Engine::MariaDb] {
        let k = key(engine);
        let s = side(engine);
        let source = snapshot(&s, true, sequence_side(&s, true));
        let target = snapshot(&s, false, sequence_side(&s, false));
        let opts = options(true);
        let diff = compare(&reg, &source, &target, &opts).unwrap();
        assert_eq!(
            diff.item("sequence:old_seq").map(|i| i.status),
            Some(DiffStatus::OnlyInTarget),
            "{k}"
        );
        let script = sync_script(&reg, &diff, &source, &target, &[], &opts).unwrap();
        let sql: Vec<&str> = script.script.ordered().map(|s| s.sql.as_str()).collect();
        let create = position(&sql, "creating new_seq", |s| {
            s.contains("new_seq") && s.to_uppercase().starts_with("CREATE")
        });
        let alter = position(&sql, "changing orders", |s| {
            s.contains("orders") && s.contains("new_seq")
        });
        let drop = position(&sql, "dropping old_seq", |s| {
            s.contains("old_seq") && s.to_uppercase().starts_with("DROP")
        });
        assert!(create < alter && alter < drop, "{k}: {sql:#?}");
        if engine == Engine::Postgres {
            let mut failed = Vec::new();
            check_golden("structure_seq_postgres", &script.sql, &mut failed);
            assert_eq!(
                failed,
                Vec::<String>::new(),
                "golden script differs (printed above)"
            );
        }
    }
}

#[test]
fn a_routine_a_new_table_calls_is_created_first() {
    let reg = registry();
    let s = side(Engine::Postgres);
    let mut f = RoutineDef::new(
        s.source
            .child(ObjectKind::Function, "next_code")
            .with_signature(""),
    );
    f.returns = Some("integer".into());
    f.language = Some("sql".into());
    f.body = Some("SELECT 1".into());
    f.extras.insert(props::VOLATILITY, "volatile");
    let mut code = col("code", DataType::INT32, false);
    code.default = Some("m8_src.next_code()".into());
    let tickets = table(
        &s.source,
        "tickets",
        vec![col("id", DataType::INT32, false), code],
    );
    let source = snapshot(
        &s,
        true,
        vec![ObjectDef::Table(tickets), ObjectDef::Routine(f)],
    );
    let target = snapshot(&s, false, Vec::new());
    let opts = options(false);
    let diff = compare(&reg, &source, &target, &opts).unwrap();
    let script = sync_script(&reg, &diff, &source, &target, &[], &opts).unwrap();
    let sql: Vec<&str> = script.script.ordered().map(|s| s.sql.as_str()).collect();
    let function = position(&sql, "creating next_code", |s| {
        s.starts_with("CREATE FUNCTION")
    });
    let table = position(&sql, "creating tickets", |s| s.starts_with("CREATE TABLE"));
    assert!(function < table, "{sql:#?}");
    assert!(sql[table].contains("m8_tgt.next_code()"), "{sql:#?}");

    // A routine that names another one comes after it (name order would put it first).
    let mut callee = RoutineDef::new(
        s.source
            .child(ObjectKind::Function, "b_inner")
            .with_signature(""),
    );
    callee.returns = Some("integer".into());
    callee.language = Some("sql".into());
    callee.body = Some("SELECT 2".into());
    let mut caller = RoutineDef::new(
        s.source
            .child(ObjectKind::Function, "a_outer")
            .with_signature("n integer"),
    );
    caller.args = vec![RoutineArg {
        name: Some("n".into()),
        mode: ArgMode::In,
        native_type: "integer".into(),
        default: None,
    }];
    caller.returns = Some("integer".into());
    caller.language = Some("sql".into());
    caller.body = Some("SELECT m8_src.b_inner() + n".into());
    let source = snapshot(
        &s,
        true,
        vec![ObjectDef::Routine(caller), ObjectDef::Routine(callee)],
    );
    let diff = compare(&reg, &source, &target, &opts).unwrap();
    let script = sync_script(&reg, &diff, &source, &target, &[], &opts).unwrap();
    let sql: Vec<&str> = script.script.ordered().map(|s| s.sql.as_str()).collect();
    let callee = position(&sql, "creating b_inner", |s| {
        s.starts_with("CREATE FUNCTION") && s.contains("b_inner") && !s.contains("a_outer")
    });
    let caller = position(&sql, "creating a_outer", |s| {
        s.starts_with("CREATE FUNCTION") && s.contains("a_outer")
    });
    assert!(callee < caller, "{sql:#?}");
}

#[test]
fn partial_selections_keep_unselected_objects_as_they_are() {
    let reg = registry();
    let s = side(Engine::Postgres);
    let view = |c: &ObjectRef, definition: &str| {
        ObjectDef::View(ViewDef {
            obj: c.child(ObjectKind::View, "customer_names"),
            definition: definition.to_owned(),
            columns: Vec::new(),
            comment: None,
            extras: Extras::new(),
        })
    };
    let mut source_objects = keyed_tables(&s.source, true);
    source_objects.push(view(
        &s.source,
        "SELECT customers.name,\n    customers.id\n   FROM m8_src.customers",
    ));
    let mut target_objects = keyed_tables(&s.target, false);
    target_objects.push(view(
        &s.target,
        "SELECT customers.name\n   FROM m8_tgt.customers",
    ));
    let source = snapshot(&s, true, source_objects);
    let target = snapshot(&s, false, target_objects);
    let opts = options(true);
    let diff = compare(&reg, &source, &target, &opts).unwrap();
    assert_eq!(
        diff.item("view:customer_names").map(|i| i.status),
        Some(DiffStatus::Different)
    );
    let script = sync_script(
        &reg,
        &diff,
        &source,
        &target,
        &["table:customers".to_owned()],
        &opts,
    )
    .unwrap();
    let created: Vec<&str> = script
        .script
        .ordered()
        .map(|s| s.sql.as_str())
        .filter(|s| s.starts_with("CREATE VIEW"))
        .collect();
    assert_eq!(created.len(), 1, "{:#?}", script.sql);
    // The target's own definition, not the unselected change.
    assert!(
        created[0].contains("SELECT customers.name\n") && !created[0].contains("customers.id"),
        "{}",
        created[0]
    );
    assert!(
        script.warnings.iter().any(|w| w.contains("privileges")),
        "{:?}",
        script.warnings
    );

    // A table drop that a foreign key of a table that stays needs is refused.
    let mut target_objects = keyed_tables(&s.target, false);
    target_objects.push(ObjectDef::Table(table(
        &s.target,
        "old_parent",
        vec![col("id", DataType::INT32, false)],
    )));
    if let Some(ObjectDef::Table(orders)) = target_objects
        .iter_mut()
        .find(|d| d.object_ref().name == "orders")
    {
        let mut fk = orders.foreign_keys[0].clone();
        fk.name = Some("orders_old_parent_fk".into());
        fk.ref_table = s.target.child(ObjectKind::Table, "old_parent");
        orders.foreign_keys.push(fk);
    }
    let source = snapshot(&s, true, keyed_tables(&s.source, false));
    let target = snapshot(&s, false, target_objects);
    let diff = compare(&reg, &source, &target, &opts).unwrap();
    let refused = sync_script(
        &reg,
        &diff,
        &source,
        &target,
        &["table:old_parent".to_owned()],
        &opts,
    );
    let message = refused.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(
        message.contains("old_parent") && message.contains("orders"),
        "{message}"
    );
    // With the child selected too, its alter drops the key first.
    let script = sync_script(
        &reg,
        &diff,
        &source,
        &target,
        &["table:old_parent".to_owned(), "table:orders".to_owned()],
        &opts,
    )
    .unwrap();
    let sql: Vec<&str> = script.script.ordered().map(|s| s.sql.as_str()).collect();
    let fk_drop = position(&sql, "dropping orders_old_parent_fk", |s| {
        s.contains("orders_old_parent_fk")
    });
    let table_drop = position(&sql, "dropping old_parent", |s| s.starts_with("DROP TABLE"));
    assert!(fk_drop < table_drop, "{sql:#?}");
}

#[test]
fn collations_off_leave_character_sets_alone() {
    let reg = registry();
    let s = side(Engine::MySql);
    let name = |c: &ObjectRef, charset: &str, collation: &str, length: u32| {
        let mut name = col(
            "name",
            DataType::VarChar {
                length: Some(length),
            },
            true,
        );
        name.extras.insert(props::CHARSET, charset);
        name.collation = Some(collation.into());
        let t = table(c, "people", vec![col("id", DataType::INT32, false), name]);
        vec![ObjectDef::Table(t)]
    };
    let quiet = StructureOptions {
        compare_collations: false,
        ..options(false)
    };
    // Only the character set and collation differ: equal with collations off.
    let source = snapshot(&s, true, name(&s.source, "latin1", "latin1_swedish_ci", 20));
    let target = snapshot(
        &s,
        false,
        name(&s.target, "utf8mb4", "utf8mb4_0900_ai_ci", 20),
    );
    let diff = compare(&reg, &source, &target, &quiet).unwrap();
    assert!(diff.is_in_sync(true), "{:#?}", diff.items);
    let diff = compare(&reg, &source, &target, &options(false)).unwrap();
    assert_eq!(
        diff.item("table:people").map(|i| i.status),
        Some(DiffStatus::Different)
    );
    // A length change too: the column keeps the target's character set and collation.
    let source = snapshot(&s, true, name(&s.source, "latin1", "latin1_swedish_ci", 40));
    let diff = compare(&reg, &source, &target, &quiet).unwrap();
    let script = sync_script(&reg, &diff, &source, &target, &[], &quiet).unwrap();
    assert!(
        script.sql.contains("varchar(40)") || script.sql.contains("VARCHAR(40)"),
        "{}",
        script.sql
    );
    assert!(!script.sql.contains("latin1"), "{}", script.sql);
}

fn policy(name: &str, using: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name, "command": "all", "permissive": true, "roles": ["public"],
        "using": using, "check": null,
    })
}

fn secure(t: &mut TableDef, on: bool, policies: Vec<serde_json::Value>) {
    t.extras.insert(props::ROW_SECURITY, on);
    t.extras.insert(props::FORCE_ROW_SECURITY, false);
    t.extras.insert(
        props::POLICIES,
        Value::Json(serde_json::Value::Array(policies)),
    );
}

/// Row-level security and new policies come after every table, view and routine the script
/// creates, so a policy may name a table created later or a view (audit lead 11); a policy
/// that goes is dropped with its table's changes, before them.
#[test]
fn policies_wait_for_the_objects_they_name() {
    let reg = registry();
    let s = side(Engine::Postgres);
    let id = || col("id", DataType::INT32, false);
    // `accounts` sorts before `zz_members`, so it is created first.
    let mut accounts = table(&s.source, "accounts", vec![id()]);
    secure(
        &mut accounts,
        true,
        vec![policy(
            "by_member",
            "(id IN (SELECT member_ids.id FROM m8_src.member_ids))",
        )],
    );
    let members = table(&s.source, "zz_members", vec![id()]);
    let view = ViewDef {
        obj: s.source.child(ObjectKind::View, "member_ids"),
        definition: "SELECT zz_members.id FROM m8_src.zz_members".into(),
        columns: Vec::new(),
        comment: None,
        extras: Extras::new(),
    };
    let mut ledger = table(&s.source, "ledger", vec![id()]);
    secure(
        &mut ledger,
        true,
        vec![policy(
            "mine",
            "(id IN (SELECT zz_members.id FROM m8_src.zz_members))",
        )],
    );
    let mut ledger_now = table(&s.target, "ledger", vec![id()]);
    secure(&mut ledger_now, false, vec![policy("old", "true")]);
    let source = snapshot(
        &s,
        true,
        vec![
            ObjectDef::Table(accounts),
            ObjectDef::Table(members),
            ObjectDef::Table(ledger),
            ObjectDef::View(view),
        ],
    );
    let target = snapshot(&s, false, vec![ObjectDef::Table(ledger_now)]);
    let opts = options(false);
    let diff = compare(&reg, &source, &target, &opts).unwrap();
    let script = sync_script(&reg, &diff, &source, &target, &[], &opts).unwrap();
    let sql: Vec<&str> = script.script.ordered().map(|s| s.sql.as_str()).collect();
    let at = |what: &str, needle: &str| position(&sql, what, |s| s.contains(needle));
    let accounts_table = at("creating accounts", "CREATE TABLE \"m8_tgt\".\"accounts\"");
    let members_table = at(
        "creating zz_members",
        "CREATE TABLE \"m8_tgt\".\"zz_members\"",
    );
    let view = at(
        "creating member_ids",
        "CREATE VIEW \"m8_tgt\".\"member_ids\"",
    );
    let drop_old = at("dropping policy old", "DROP POLICY \"old\"");
    let enable_accounts = at(
        "enabling accounts",
        "ALTER TABLE \"m8_tgt\".\"accounts\" ENABLE ROW LEVEL SECURITY",
    );
    let enable_ledger = at(
        "enabling ledger",
        "ALTER TABLE \"m8_tgt\".\"ledger\" ENABLE ROW LEVEL SECURITY",
    );
    let by_member = at("creating policy by_member", "CREATE POLICY \"by_member\"");
    let mine = at("creating policy mine", "CREATE POLICY \"mine\"");
    assert!(accounts_table < members_table, "{sql:#?}");
    for late in [enable_accounts, enable_ledger, by_member, mine] {
        assert!(members_table < late && view < late, "{sql:#?}");
        assert!(drop_old < late, "{sql:#?}");
    }
    assert!(
        !sql[accounts_table].contains("POLICY") && !sql[accounts_table].contains("ROW LEVEL"),
        "{sql:#?}"
    );
    // The policy names the target's objects.
    assert!(sql[by_member].contains("m8_tgt.member_ids"), "{sql:#?}");
    assert!(sql[mine].contains("m8_tgt.zz_members"), "{sql:#?}");
}
