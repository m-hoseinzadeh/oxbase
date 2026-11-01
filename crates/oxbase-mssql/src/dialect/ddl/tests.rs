//! Golden SQL of the SQL Server DDL generator (docs/m4-contracts.md §4.1, §12 ddl).

use super::*;
use oxbase_types::{GeneratedColumn, IndexColumn, Rename};
use pretty_assertions::assert_eq;

fn v17() -> ServerVersion {
    ServerVersion::new(17, 0, 4005)
}

fn col(name: &str, native: &str, data_type: DataType) -> ColumnDef {
    let mut c = ColumnDef::new(name, data_type);
    c.native_type = native.to_owned();
    c
}

fn sql(s: &Script) -> Vec<&str> {
    s.steps.iter().map(|step| step.sql.as_str()).collect()
}

fn destructive(s: &Script) -> Vec<bool> {
    s.steps.iter().map(|step| step.destructive).collect()
}

fn index(name: &str, columns: &[&str]) -> IndexDef {
    IndexDef {
        name: name.to_owned(),
        columns: columns.iter().map(|c| IndexColumn::column(*c)).collect(),
        unique: false,
        is_constraint: false,
        predicate: None,
        extras: Extras::new(),
    }
}

fn check(name: &str, expression: &str) -> CheckDef {
    CheckDef {
        name: Some(name.to_owned()),
        expression: expression.to_owned(),
        extras: Extras::new(),
    }
}

/// `m3.t` as the fetcher reports it: id (PK, clustered), code with a named default, qty with
/// a named default, a computed column on code, an index on code and two checks.
fn base() -> TableDef {
    let mut t = TableDef::new(ObjectRef::table(Some("oxbase_test"), Some("m3"), "t"));
    t.extras.insert(props::CLUSTERED, true);
    let id = col("id", "int", DataType::INT32).not_null();
    let mut code = col(
        "code",
        "varchar(20)",
        DataType::VarChar { length: Some(20) },
    )
    .not_null();
    code.default = Some("('x')".into());
    code.extras.insert(extras::DEFAULT_CONSTRAINT, "DF_t_code");
    let mut qty = col("qty", "int", DataType::INT32);
    qty.default = Some("((0))".into());
    qty.extras.insert(extras::DEFAULT_CONSTRAINT, "DF_t_qty");
    let mut up = col("up", "varchar(20)", DataType::VarChar { length: Some(20) });
    up.generated = Some(GeneratedColumn {
        expression: "(upper([code]))".into(),
        stored: false,
    });
    t.columns = vec![id, code, qty, up];
    t.primary_key = Some(PrimaryKeyDef {
        name: Some("PK_t".into()),
        columns: vec!["id".into()],
    });
    let mut ix = index("ix_code", &["code"]);
    ix.extras.insert(props::CLUSTERED, false);
    ix.extras.insert(extras::INDEX_KIND, "nonclustered");
    t.indexes.push(ix);
    t.checks.push(check("ck_code", "(len([code])>(0))"));
    t.checks.push(check("ck_qty", "([qty]>=(0))"));
    t
}

fn alter(old: &TableDef, new: &TableDef, renames: &[Rename]) -> Result<Script> {
    let v = v17();
    let ctx = DdlContext::new(&v).with_table(old).with_renames(renames);
    MsSqlDdl.alter(
        &ObjectDef::Table(old.clone()),
        &ObjectDef::Table(new.clone()),
        &ctx,
    )
}

#[test]
fn create_table_golden() {
    let mut t = TableDef::new(ObjectRef::table(Some("oxbase_test"), Some("m3"), "orders"));
    let mut id = col("id", "int", DataType::INT32).not_null();
    id.identity = Some(IdentityDef {
        always: true,
        start: Some(1),
        increment: Some(1),
    });
    let mut order = col("order", "", DataType::VarChar { length: Some(40) }).not_null();
    order.default = Some(BracketQuoting.quote_str("it's \u{fc}"));
    let mut qty = col("qty", "int", DataType::INT32);
    qty.default = Some("0".into());
    let mut note = col("note", "", DataType::Text);
    note.comment = Some("it's a note".into());
    let odd = col("a]b", "decimal(12,2)", DataType::Text);
    let mut total = col("total", "", DataType::INT32);
    total.generated = Some(GeneratedColumn {
        expression: "qty * 2".into(),
        stored: true,
    });
    t.columns = vec![id, order, qty, note, odd, total];
    t.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".into()],
    });
    let mut ix = index("ix_qty", &["qty"]);
    ix.columns[0].descending = true;
    ix.predicate = Some("qty > 0".into());
    ix.extras.insert(
        props::INCLUDE_COLUMNS,
        Value::Json(serde_json::json!(["note"])),
    );
    let mut uq = index("uq_order", &["order"]);
    uq.unique = true;
    uq.is_constraint = true;
    t.indexes = vec![ix, uq];
    t.checks.push(check("ck_qty", "qty >= 0"));
    t.foreign_keys.push(ForeignKeyDef {
        name: Some("fk_cust".into()),
        columns: vec!["qty".into()],
        ref_table: ObjectRef::table(Some("oxbase_test"), Some("m3"), "customers"),
        ref_columns: vec!["id".into()],
        on_delete: FkAction::Cascade,
        on_update: FkAction::NoAction,
        extras: Extras::new(),
    });
    t.comment = Some("Orders: it's \u{fc}".into());
    let v = v17();
    let s = MsSqlDdl
        .create(&ObjectDef::Table(t), &DdlContext::new(&v))
        .unwrap();
    assert_eq!(
        sql(&s),
        [
            "CREATE TABLE [m3].[orders] (\n    \
             [id] int NOT NULL IDENTITY(1, 1),\n    \
             [order] nvarchar(40) NOT NULL CONSTRAINT [DF_orders_order] DEFAULT (N'it''s \u{fc}'),\n    \
             [qty] int NULL CONSTRAINT [DF_orders_qty] DEFAULT (0),\n    \
             [note] nvarchar(max) NULL,\n    \
             [a]]b] decimal(12, 2) NULL,\n    \
             [total] AS (qty * 2) PERSISTED,\n    \
             CONSTRAINT [PK_orders] PRIMARY KEY CLUSTERED ([id]),\n    \
             CONSTRAINT [uq_order] UNIQUE NONCLUSTERED ([order]),\n    \
             CONSTRAINT [ck_qty] CHECK (qty >= 0)\n)",
            "CREATE NONCLUSTERED INDEX [ix_qty] ON [m3].[orders] ([qty] DESC) INCLUDE ([note]) \
             WHERE (qty > 0)",
            "ALTER TABLE [m3].[orders] ADD CONSTRAINT [fk_cust] FOREIGN KEY ([qty]) REFERENCES \
             [m3].[customers] ([id]) ON DELETE CASCADE",
            "EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Orders: \
             it''s \u{fc}', @level0type = N'SCHEMA', @level0name = N'm3', @level1type = \
             N'TABLE', @level1name = N'orders'",
            "EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'it''s a \
             note', @level0type = N'SCHEMA', @level0name = N'm3', @level1type = N'TABLE', \
             @level1name = N'orders', @level2type = N'COLUMN', @level2name = N'note'",
        ]
    );
    assert!(s.is_atomic());
    assert_eq!(destructive(&s), [false; 5]);
    // Unicode and quotes in names.
    let mut u = TableDef::new(ObjectRef::table(None, Some("m3"), "t\u{e4}\"b"));
    u.columns
        .push(col("sel\"ect_\u{fc}", "int", DataType::INT32));
    let s = MsSqlDdl
        .create(&ObjectDef::Table(u), &DdlContext::new(&v))
        .unwrap();
    assert_eq!(
        sql(&s),
        ["CREATE TABLE [m3].[t\u{e4}\"b] (\n    [sel\"ect_\u{fc}] int NULL\n)"]
    );
}

#[test]
fn create_refuses_what_sql_server_cannot_do() {
    let v = v17();
    let ctx = DdlContext::new(&v);
    let mut t = TableDef::new(ObjectRef::table(None, Some("m3"), "t"));
    let mut id = col("id", "int", DataType::INT32);
    id.identity = Some(IdentityDef {
        always: true,
        start: None,
        increment: None,
    });
    t.columns.push(id.clone());
    // Identity columns are NOT NULL.
    let made = MsSqlDdl.create(&ObjectDef::Table(t.clone()), &ctx);
    assert!(matches!(made, Err(Error::InvalidInput { .. })), "{made:?}");
    // A type with a clause in it.
    t.columns[0] = col("id", "int NOT NULL DEFAULT 1", DataType::INT32);
    let made = MsSqlDdl.create(&ObjectDef::Table(t.clone()), &ctx);
    assert!(matches!(made, Err(Error::InvalidInput { .. })), "{made:?}");
    // A default that tries to leave its parentheses.
    let mut c = col("c", "int", DataType::INT32);
    c.default = Some("0) ; DROP TABLE x --".into());
    t.columns[0] = c;
    let made = MsSqlDdl.create(&ObjectDef::Table(t.clone()), &ctx);
    assert!(matches!(made, Err(Error::InvalidInput { .. })), "{made:?}");
    // Temporal tables and neutral types SQL Server does not have.
    t.columns[0] = col("c", "", DataType::Interval);
    let made = MsSqlDdl.create(&ObjectDef::Table(t.clone()), &ctx);
    assert!(matches!(made, Err(Error::Unsupported { .. })), "{made:?}");
    t.columns[0] = col("c", "int", DataType::INT32);
    t.extras.insert(extras::TEMPORAL_TYPE, "system_versioned");
    let made = MsSqlDdl.create(&ObjectDef::Table(t), &ctx);
    assert!(matches!(made, Err(Error::Unsupported { .. })), "{made:?}");
    // Names longer than 128 characters.
    let long = "x".repeat(129);
    let made = MsSqlDdl.create(
        &ObjectDef::Table(TableDef::new(ObjectRef::table(None, Some("m3"), long))),
        &ctx,
    );
    assert!(matches!(made, Err(Error::InvalidInput { .. })), "{made:?}");
}

#[test]
fn alter_column_type_drops_and_recreates_what_blocks_it() {
    let old = base();
    let mut new = base();
    new.columns[1].native_type = "nvarchar(10)".into();
    new.columns[1].data_type = DataType::VarChar { length: Some(10) };
    let s = alter(&old, &new, &[]).unwrap();
    assert_eq!(
        sql(&s),
        [
            "ALTER TABLE [m3].[t] DROP CONSTRAINT [ck_code]",
            "DROP INDEX [ix_code] ON [m3].[t]",
            "ALTER TABLE [m3].[t] DROP CONSTRAINT [DF_t_code]",
            "ALTER TABLE [m3].[t] DROP COLUMN [up]",
            "ALTER TABLE [m3].[t] ALTER COLUMN [code] nvarchar(10) NOT NULL",
            "ALTER TABLE [m3].[t] ADD [up] AS ((upper([code])))",
            "ALTER TABLE [m3].[t] ADD CONSTRAINT [DF_t_code] DEFAULT (('x')) FOR [code]",
            "CREATE NONCLUSTERED INDEX [ix_code] ON [m3].[t] ([code])",
            "ALTER TABLE [m3].[t] ADD CONSTRAINT [ck_code] CHECK ((len([code])>(0)))",
        ]
    );
    assert_eq!(
        destructive(&s),
        [false, false, false, true, true, true, false, false, false]
    );
    assert_eq!(s.warnings.len(), 1);
    assert!(s.is_atomic());
}

#[test]
fn widening_a_varchar_keeps_its_default_index_and_check() {
    let mut old = base();
    old.columns.pop(); // no computed column
    let mut new = old.clone();
    new.columns[1].native_type = "varchar(40)".into();
    let s = alter(&old, &new, &[]).unwrap();
    assert_eq!(
        sql(&s),
        ["ALTER TABLE [m3].[t] ALTER COLUMN [code] varchar(40) NOT NULL"]
    );
    assert_eq!(destructive(&s), [false]);
    // NULL -> NOT NULL on an indexed column recreates the index and the check.
    let mut new = old.clone();
    new.columns[1].nullable = true;
    let s = alter(&old, &new, &[]).unwrap();
    assert_eq!(
        sql(&s),
        [
            "ALTER TABLE [m3].[t] DROP CONSTRAINT [ck_code]",
            "DROP INDEX [ix_code] ON [m3].[t]",
            "ALTER TABLE [m3].[t] ALTER COLUMN [code] varchar(20) NULL",
            "CREATE NONCLUSTERED INDEX [ix_code] ON [m3].[t] ([code])",
            "ALTER TABLE [m3].[t] ADD CONSTRAINT [ck_code] CHECK ((len([code])>(0)))",
        ]
    );
    // A column with its own collation keeps it.
    let mut old2 = old.clone();
    old2.columns[1].collation = Some("Latin1_General_BIN2".into());
    let mut new = old2.clone();
    new.columns[1].native_type = "varchar(30)".into();
    let s = alter(&old2, &new, &[]).unwrap();
    assert_eq!(
        sql(&s),
        [
            "ALTER TABLE [m3].[t] ALTER COLUMN [code] varchar(30) COLLATE Latin1_General_BIN2 NOT NULL"
        ]
    );
}

#[test]
fn identity_cannot_be_added_removed_or_changed() {
    let old = base();
    let mut new = base();
    new.columns[0].identity = Some(IdentityDef {
        always: true,
        start: None,
        increment: None,
    });
    let made = alter(&old, &new, &[]);
    assert!(matches!(made, Err(Error::Unsupported { .. })), "{made:?}");
    let mut with_id = base();
    with_id.columns[0].identity = Some(IdentityDef {
        always: true,
        start: Some(1),
        increment: Some(1),
    });
    with_id.columns[0].auto_increment = true;
    let mut new = with_id.clone();
    new.columns[0].identity.as_mut().unwrap().increment = Some(2);
    let made = alter(&with_id, &new, &[]);
    assert!(matches!(made, Err(Error::Unsupported { .. })), "{made:?}");
    let mut new = with_id.clone();
    new.columns[0].identity = None;
    new.columns[0].auto_increment = false;
    let made = alter(&with_id, &new, &[]);
    assert!(matches!(made, Err(Error::Unsupported { .. })), "{made:?}");
    // A new column may be an identity column.
    let mut new = base();
    let mut n = col("n", "bigint", DataType::INT64).not_null();
    n.identity = Some(IdentityDef {
        always: true,
        start: Some(10),
        increment: Some(5),
    });
    new.columns.push(n);
    let s = alter(&old, &new, &[]).unwrap();
    assert_eq!(
        sql(&s),
        ["ALTER TABLE [m3].[t] ADD [n] bigint NOT NULL IDENTITY(10, 5)"]
    );
}

#[test]
fn renames_come_first_with_sp_rename() {
    let old = base();
    let mut new = base();
    new.obj.name = "t2".into();
    new.columns[1].name = "c]ode".into();
    new.indexes[0].name = "ix2".into();
    new.indexes[0].columns[0].name = "c]ode".into();
    new.checks[0].name = Some("ck2".into());
    new.comment = Some("x".into());
    let renames = [
        Rename::new(ObjectKind::Table, "t", "t2"),
        Rename::new(ObjectKind::Column, "code", "c]ode"),
        Rename::new(ObjectKind::Index, "ix_code", "ix2"),
        Rename::new(ObjectKind::Check, "ck_code", "ck2"),
    ];
    let s = alter(&old, &new, &renames).unwrap();
    assert_eq!(
        sql(&s),
        [
            "EXEC sp_rename N'[m3].[t]', N't2'",
            "EXEC sp_rename N'[m3].[t2].[code]', N'c]ode', N'COLUMN'",
            "EXEC sp_rename N'[m3].[t2].[ix_code]', N'ix2', N'INDEX'",
            "EXEC sp_rename N'[m3].[ck_code]', N'ck2', N'OBJECT'",
            "EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'x', \
             @level0type = N'SCHEMA', @level0name = N'm3', @level1type = N'TABLE', \
             @level1name = N't2'",
        ]
    );
    // Swapped names go through temporary names.
    let mut new = base();
    new.columns[1].name = "qty".into();
    new.columns[2].name = "code".into();
    new.indexes[0].columns[0].name = "qty".into();
    let renames = [
        Rename::new(ObjectKind::Column, "code", "qty"),
        Rename::new(ObjectKind::Column, "qty", "code"),
    ];
    let s = alter(&old, &new, &renames).unwrap();
    assert_eq!(
        sql(&s),
        [
            "EXEC sp_rename N'[m3].[t].[code]', N'__oxbase_rename_0', N'COLUMN'",
            "EXEC sp_rename N'[m3].[t].[qty]', N'__oxbase_rename_1', N'COLUMN'",
            "EXEC sp_rename N'[m3].[t].[__oxbase_rename_0]', N'qty', N'COLUMN'",
            "EXEC sp_rename N'[m3].[t].[__oxbase_rename_1]', N'code', N'COLUMN'",
        ]
    );
}

#[test]
fn defaults_are_named_constraints() {
    let old = base();
    // Changed: drop by the old name, add with the same name.
    let mut new = base();
    new.columns[2].default = Some("1".into());
    let s = alter(&old, &new, &[]).unwrap();
    assert_eq!(
        sql(&s),
        [
            "ALTER TABLE [m3].[t] DROP CONSTRAINT [DF_t_qty]",
            "ALTER TABLE [m3].[t] ADD CONSTRAINT [DF_t_qty] DEFAULT (1) FOR [qty]",
        ]
    );
    // Removed.
    let mut new = base();
    new.columns[2].default = None;
    let s = alter(&old, &new, &[]).unwrap();
    assert_eq!(sql(&s), ["ALTER TABLE [m3].[t] DROP CONSTRAINT [DF_t_qty]"]);
    // Added to a column without one: DF_<table>_<column>.
    let mut new = base();
    new.columns[0].default = Some("5".into());
    let s = alter(&old, &new, &[]).unwrap();
    assert_eq!(
        sql(&s),
        ["ALTER TABLE [m3].[t] ADD CONSTRAINT [DF_t_id] DEFAULT (5) FOR [id]"]
    );
    // A default whose name is not known is looked up.
    let mut nameless = base();
    nameless.columns[2].extras = Extras::new();
    let mut new = nameless.clone();
    new.columns[2].default = None;
    let s = alter(&nameless, &new, &[]).unwrap();
    assert_eq!(s.len(), 1);
    let text = &s.steps[0].sql;
    assert!(
        text.starts_with("DECLARE @df sysname = (SELECT d.name FROM sys.default_constraints"),
        "{text}"
    );
    assert!(text.contains("OBJECT_ID(N'[m3].[t]')"), "{text}");
    assert!(text.contains("c.name = N'qty'"), "{text}");
    assert!(
        text.ends_with(
            "), @sql nvarchar(max); IF @df IS NOT NULL BEGIN SET @sql = N'ALTER TABLE [m3].[t] \
             DROP CONSTRAINT ' + QUOTENAME(@df); EXEC sys.sp_executesql @sql; END"
        ),
        "{text}"
    );
    // `EXEC (...)` takes no function calls: the statement is built in a variable.
    assert!(!text.contains("EXEC ("), "{text}");
    // Generated names are cut to 128 characters.
    let long = "y".repeat(140);
    assert_eq!(generated_name("DF", &[&long, "c"]).chars().count(), 128);
}

#[test]
fn a_primary_key_whose_name_is_not_known_is_looked_up() {
    let mut old = base();
    old.primary_key.as_mut().unwrap().name = None;
    let mut new = old.clone();
    new.primary_key.as_mut().unwrap().columns = vec!["id".into(), "code".into()];
    let s = alter(&old, &new, &[]).unwrap();
    let text = s
        .steps
        .iter()
        .map(|step| step.sql.as_str())
        .find(|sql| sql.contains("@pk"))
        .unwrap();
    assert!(
        text.starts_with(
            "DECLARE @pk sysname = (SELECT name FROM sys.key_constraints WHERE parent_object_id \
             = OBJECT_ID(N'[m3].[t]') AND type = 'PK'), @sql nvarchar(max); IF @pk IS NOT NULL \
             BEGIN SET @sql = N'ALTER TABLE [m3].[t] DROP CONSTRAINT ' + QUOTENAME(@pk); EXEC \
             sys.sp_executesql @sql; END"
        ),
        "{text}"
    );
    // Names with `]` and `'` stay quoted inside the built statement.
    let odd = ObjectRef::table(None, Some("s]x"), "it's");
    let step = drop_pk_step(&odd, None, false);
    assert!(
        step.sql
            .contains("SET @sql = N'ALTER TABLE [s]]x].[it''s] DROP CONSTRAINT ' + QUOTENAME"),
        "{}",
        step.sql
    );
}

#[test]
fn dropping_a_column_drops_its_default_and_checks_first() {
    let old = base();
    let v = v17();
    let ctx = DdlContext::new(&v).with_table(&old);
    let qty = old.obj.child(ObjectKind::Column, "qty");
    let s = MsSqlDdl.drop_object(&qty, &ctx).unwrap();
    assert_eq!(
        sql(&s),
        [
            "ALTER TABLE [m3].[t] DROP CONSTRAINT [ck_qty]",
            "ALTER TABLE [m3].[t] DROP CONSTRAINT [DF_t_qty]",
            "ALTER TABLE [m3].[t] DROP COLUMN [qty]",
        ]
    );
    assert_eq!(destructive(&s), [true, false, true]);
    // Without the table: the default is looked up.
    let s = MsSqlDdl.drop_object(&qty, &DdlContext::new(&v)).unwrap();
    assert_eq!(s.len(), 2);
    assert!(s.steps[0].sql.starts_with("DECLARE @df sysname"));
    assert!(
        s.steps[0].sql.contains("EXEC sys.sp_executesql @sql"),
        "{}",
        s.steps[0].sql
    );
    assert_eq!(s.steps[1].sql, "ALTER TABLE [m3].[t] DROP COLUMN [qty]");
    // Other kinds.
    let s = MsSqlDdl.drop_object(&old.obj, &ctx).unwrap();
    assert_eq!(sql(&s), ["DROP TABLE [m3].[t]"]);
    assert!(s.is_destructive());
    let ix = old.obj.child(ObjectKind::Index, "ix_code");
    let s = MsSqlDdl.drop_object(&ix, &ctx).unwrap();
    assert_eq!(sql(&s), ["DROP INDEX [ix_code] ON [m3].[t]"]);
    let ck = old.obj.child(ObjectKind::Check, "ck_qty");
    let s = MsSqlDdl.drop_object(&ck, &ctx).unwrap();
    assert_eq!(sql(&s), ["ALTER TABLE [m3].[t] DROP CONSTRAINT [ck_qty]"]);
    let view = ObjectRef::new(ObjectKind::View, "v");
    let made = MsSqlDdl.drop_object(&view, &ctx);
    assert!(matches!(made, Err(Error::Unsupported { .. })), "{made:?}");
}

#[test]
fn indexes_constraints_and_options() {
    let v = v17();
    let ctx = DdlContext::new(&v);
    let table = ObjectRef::table(None, Some("m3"), "t");
    let create = |idx: IndexDef| {
        MsSqlDdl.create(
            &ObjectDef::Index {
                table: table.clone(),
                index: idx,
            },
            &ctx,
        )
    };
    let mut uq = index("uq", &["code"]);
    uq.unique = true;
    uq.predicate = Some("code IS NOT NULL".into());
    uq.extras.insert(
        props::INCLUDE_COLUMNS,
        Value::Json(serde_json::json!(["qty"])),
    );
    assert_eq!(
        sql(&create(uq).unwrap()),
        [
            "CREATE UNIQUE NONCLUSTERED INDEX [uq] ON [m3].[t] ([code]) INCLUDE ([qty]) WHERE \
             (code IS NOT NULL)"
        ]
    );
    let mut uq2 = index("uq2", &["code"]);
    uq2.unique = true;
    uq2.is_constraint = true;
    uq2.columns[0].descending = true;
    uq2.extras.insert(props::CLUSTERED, true);
    uq2.extras.insert(props::FILL_FACTOR, Value::Int(80));
    assert_eq!(
        sql(&create(uq2).unwrap()),
        [
            "ALTER TABLE [m3].[t] ADD CONSTRAINT [uq2] UNIQUE CLUSTERED ([code] DESC) WITH \
             (FILLFACTOR = 80)"
        ]
    );
    let mut cl = index("cl", &["a"]);
    cl.extras.insert(extras::INDEX_KIND, "clustered");
    cl.extras.insert(extras::DATA_COMPRESSION, "PAGE");
    cl.extras.insert(extras::DISABLED, true);
    assert_eq!(
        sql(&create(cl).unwrap()),
        [
            "CREATE CLUSTERED INDEX [cl] ON [m3].[t] ([a]) WITH (DATA_COMPRESSION = PAGE)",
            "ALTER INDEX [cl] ON [m3].[t] DISABLE",
        ]
    );
    let mut expr = index("e", &["a"]);
    expr.columns[0].is_expression = true;
    assert!(matches!(create(expr), Err(Error::Unsupported { .. })));
    let mut prefix = index("p", &["a"]);
    prefix.columns[0].length = Some(10);
    assert!(matches!(create(prefix), Err(Error::Unsupported { .. })));
    let mut cs = index("cs", &["a"]);
    cs.extras
        .insert(extras::INDEX_KIND, "nonclustered_columnstore");
    assert!(matches!(create(cs.clone()), Err(Error::Unsupported { .. })));
    let mut filtered_clustered = index("fc", &["a"]);
    filtered_clustered.extras.insert(props::CLUSTERED, true);
    filtered_clustered.predicate = Some("a > 0".into());
    assert!(matches!(
        create(filtered_clustered),
        Err(Error::InvalidInput { .. })
    ));
    // A columnstore index is kept, and can be dropped.
    let mut t = base();
    t.indexes.push(cs);
    let mut new = t.clone();
    new.columns[2].default = None;
    assert_eq!(alter(&t, &new, &[]).unwrap().len(), 1);
    let mut new = t.clone();
    new.indexes.pop();
    assert_eq!(
        sql(&alter(&t, &new, &[]).unwrap()),
        ["DROP INDEX [cs] ON [m3].[t]"]
    );
    // Changing an index: drop, then create.
    let old = base();
    let mut new = base();
    new.indexes[0].columns.push(IndexColumn::column("qty"));
    new.indexes[0].unique = true;
    assert_eq!(
        sql(&alter(&old, &new, &[]).unwrap()),
        [
            "DROP INDEX [ix_code] ON [m3].[t]",
            "CREATE UNIQUE NONCLUSTERED INDEX [ix_code] ON [m3].[t] ([code], [qty])",
        ]
    );
}

#[test]
fn foreign_keys_and_checks() {
    let v = v17();
    let ctx = DdlContext::new(&v);
    let table = ObjectRef::table(Some("oxbase_test"), Some("m3"), "child");
    let mut fk = ForeignKeyDef {
        name: Some("fk".into()),
        columns: vec!["ref_id".into()],
        ref_table: ObjectRef::table(Some("oxbase_test"), Some("m3"), "parent"),
        ref_columns: vec!["id".into()],
        on_delete: FkAction::SetNull,
        on_update: FkAction::SetDefault,
        extras: Extras::new(),
    };
    let add = |fk: &ForeignKeyDef| {
        MsSqlDdl.create(
            &ObjectDef::ForeignKey {
                table: table.clone(),
                foreign_key: fk.clone(),
            },
            &ctx,
        )
    };
    assert_eq!(
        sql(&add(&fk).unwrap()),
        [
            "ALTER TABLE [m3].[child] ADD CONSTRAINT [fk] FOREIGN KEY ([ref_id]) REFERENCES \
             [m3].[parent] ([id]) ON DELETE SET NULL ON UPDATE SET DEFAULT"
        ]
    );
    fk.extras.insert(extras::NOT_TRUSTED, true);
    fk.extras.insert(extras::DISABLED, true);
    fk.on_update = FkAction::NoAction;
    assert_eq!(
        sql(&add(&fk).unwrap()),
        [
            "ALTER TABLE [m3].[child] WITH NOCHECK ADD CONSTRAINT [fk] FOREIGN KEY ([ref_id]) \
             REFERENCES [m3].[parent] ([id]) ON DELETE SET NULL",
            "ALTER TABLE [m3].[child] NOCHECK CONSTRAINT [fk]",
        ]
    );
    fk.on_delete = FkAction::Restrict;
    assert!(matches!(add(&fk), Err(Error::Unsupported { .. })));
    fk.on_delete = FkAction::NoAction;
    fk.ref_table.catalog = Some("other_db".into());
    assert!(matches!(add(&fk), Err(Error::Unsupported { .. })));

    // Change a foreign key's action through the table: drop, then add.
    let mut parent_fk = fk.clone();
    parent_fk.ref_table.catalog = None;
    parent_fk.extras = Extras::new();
    parent_fk.on_delete = FkAction::Cascade;
    let mut t = TableDef::new(table.clone());
    t.columns.push(col("ref_id", "bigint", DataType::INT64));
    t.foreign_keys.push(parent_fk.clone());
    let old = ObjectDef::ForeignKey {
        table: table.clone(),
        foreign_key: parent_fk.clone(),
    };
    parent_fk.on_delete = FkAction::SetNull;
    let new = ObjectDef::ForeignKey {
        table: table.clone(),
        foreign_key: parent_fk,
    };
    let s = MsSqlDdl
        .alter(&old, &new, &DdlContext::new(&v).with_table(&t))
        .unwrap();
    assert_eq!(
        sql(&s),
        [
            "ALTER TABLE [m3].[child] DROP CONSTRAINT [fk]",
            "ALTER TABLE [m3].[child] ADD CONSTRAINT [fk] FOREIGN KEY ([ref_id]) REFERENCES \
             [m3].[parent] ([id]) ON DELETE SET NULL",
        ]
    );
    // The same without the table.
    assert_eq!(MsSqlDdl.alter(&old, &new, &DdlContext::new(&v)).unwrap(), s);

    // Checks.
    let mut ck = check("ck", "qty >= 0");
    ck.extras.insert(extras::NOT_FOR_REPLICATION, true);
    let s = MsSqlDdl
        .create(
            &ObjectDef::Check {
                table: table.clone(),
                check: ck.clone(),
            },
            &ctx,
        )
        .unwrap();
    assert_eq!(
        sql(&s),
        ["ALTER TABLE [m3].[child] ADD CONSTRAINT [ck] CHECK NOT FOR REPLICATION (qty >= 0)"]
    );
    let old = ObjectDef::Check {
        table: table.clone(),
        check: check("ck", "([qty]>=(0))"),
    };
    let same = ObjectDef::Check {
        table: table.clone(),
        check: check("ck", "qty >= 0"),
    };
    assert_eq!(MsSqlDdl.alter(&old, &same, &ctx).unwrap().len(), 0);
    let changed = ObjectDef::Check {
        table: table.clone(),
        check: check("ck", "qty >= 1"),
    };
    assert_eq!(
        sql(&MsSqlDdl.alter(&old, &changed, &ctx).unwrap()),
        [
            "ALTER TABLE [m3].[child] DROP CONSTRAINT [ck]",
            "ALTER TABLE [m3].[child] ADD CONSTRAINT [ck] CHECK (qty >= 1)",
        ]
    );
}

#[test]
fn primary_key_changes_keep_the_name() {
    let old = base();
    let mut new = base();
    new.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".into(), "code".into()],
    });
    let s = alter(&old, &new, &[]).unwrap();
    assert_eq!(
        sql(&s),
        [
            "ALTER TABLE [m3].[t] DROP CONSTRAINT [PK_t]",
            "ALTER TABLE [m3].[t] ADD CONSTRAINT [PK_t] PRIMARY KEY CLUSTERED ([id], [code])",
        ]
    );
    assert_eq!(s.warnings.len(), 1);
    // Nonclustered.
    let mut new = base();
    new.extras.insert(props::CLUSTERED, false);
    let s = alter(&old, &new, &[]).unwrap();
    assert_eq!(
        sql(&s),
        [
            "ALTER TABLE [m3].[t] DROP CONSTRAINT [PK_t]",
            "ALTER TABLE [m3].[t] ADD CONSTRAINT [PK_t] PRIMARY KEY NONCLUSTERED ([id])",
        ]
    );
    // Removed.
    let mut new = base();
    new.primary_key = None;
    new.extras = Extras::new();
    let s = alter(&old, &new, &[]).unwrap();
    assert_eq!(sql(&s), ["ALTER TABLE [m3].[t] DROP CONSTRAINT [PK_t]"]);
    assert_eq!(destructive(&s), [true]);
    // Changing the type of a key column drops and adds the key.
    let mut new = base();
    new.columns[0].native_type = "bigint".into();
    let s = alter(&old, &new, &[]).unwrap();
    assert_eq!(
        sql(&s),
        [
            "ALTER TABLE [m3].[t] DROP CONSTRAINT [PK_t]",
            "ALTER TABLE [m3].[t] ALTER COLUMN [id] bigint NOT NULL",
            "ALTER TABLE [m3].[t] ADD CONSTRAINT [PK_t] PRIMARY KEY CLUSTERED ([id])",
        ]
    );
}

#[test]
fn comments_use_extended_properties() {
    let mut old = base();
    old.columns[2].comment = Some("old".into());
    old.comment = Some("table".into());
    let mut new = old.clone();
    new.columns[2].comment = Some("it's new".into());
    new.comment = None;
    let s = alter(&old, &new, &[]).unwrap();
    assert_eq!(
        sql(&s),
        [
            "EXEC sys.sp_dropextendedproperty @name = N'MS_Description', @level0type = \
             N'SCHEMA', @level0name = N'm3', @level1type = N'TABLE', @level1name = N't'",
            "EXEC sys.sp_updateextendedproperty @name = N'MS_Description', @value = N'it''s \
             new', @level0type = N'SCHEMA', @level0name = N'm3', @level1type = N'TABLE', \
             @level1name = N't', @level2type = N'COLUMN', @level2name = N'qty'",
        ]
    );
    // Nothing changed: no script.
    assert_eq!(alter(&old, &old, &[]).unwrap().len(), 0);
}

#[test]
fn unsupported_table_changes() {
    let old = base();
    // Reordering.
    let mut new = base();
    new.columns.swap(1, 2);
    let made = alter(&old, &new, &[]);
    assert!(matches!(made, Err(Error::Unsupported { .. })), "{made:?}");
    // Temporal tables.
    let mut temporal = base();
    temporal
        .extras
        .insert(extras::TEMPORAL_TYPE, "system_versioned");
    let made = alter(&temporal, &temporal, &[]);
    assert!(matches!(made, Err(Error::Unsupported { .. })), "{made:?}");
    // Rowversion columns cannot be altered.
    let mut rv = base();
    rv.columns
        .push(col("rv", "rowversion", DataType::Binary { length: Some(8) }).not_null());
    let mut new = rv.clone();
    new.columns[4].nullable = true;
    let made = alter(&rv, &new, &[]);
    assert!(matches!(made, Err(Error::Unsupported { .. })), "{made:?}");
    // text only becomes varchar(max) / nvarchar(max).
    let mut txt = base();
    txt.columns.push(col("doc", "text", DataType::Text));
    let mut new = txt.clone();
    new.columns[4].native_type = "nvarchar(max)".into();
    assert_eq!(
        sql(&alter(&txt, &new, &[]).unwrap()),
        ["ALTER TABLE [m3].[t] ALTER COLUMN [doc] nvarchar(max) NULL"]
    );
    new.columns[4].native_type = "int".into();
    let made = alter(&txt, &new, &[]);
    assert!(matches!(made, Err(Error::Unsupported { .. })), "{made:?}");
    // Moving the table to another schema.
    let mut moved = base();
    moved.obj.schema = Some("m4".into());
    assert_eq!(
        sql(&alter(&old, &moved, &[]).unwrap()),
        ["ALTER SCHEMA [m4] TRANSFER [m3].[t]"]
    );
}

/// The steps of the conformance column cell, on a table built from neutral types.
#[test]
fn column_cell_steps() {
    let v = v17();
    let mut t = TableDef::new(ObjectRef::table(Some("oxbase_test"), Some("m3"), "c"));
    t.extras.insert(props::CLUSTERED, true);
    t.columns = vec![
        col("id", "bigint", DataType::INT64).not_null(),
        col(
            "c_extra",
            "nvarchar(40)",
            DataType::VarChar { length: Some(40) },
        ),
    ];
    t.primary_key = Some(PrimaryKeyDef {
        name: Some("PK_c".into()),
        columns: vec!["id".into()],
    });
    let obj = t.obj.clone();
    let child = |c: &ColumnDef| ObjectDef::Column {
        table: obj.clone(),
        column: c.clone(),
    };
    let step = |t: &TableDef, old: &ColumnDef, new: &ColumnDef, renames: &[Rename]| {
        let ctx = DdlContext::new(&v).with_table(t).with_renames(renames);
        MsSqlDdl.alter(&child(old), &child(new), &ctx).unwrap()
    };
    // Add (neutral type).
    let added = ColumnDef::new("c_new", DataType::VarChar { length: Some(40) });
    let s = MsSqlDdl
        .create(&child(&added), &DdlContext::new(&v).with_table(&t))
        .unwrap();
    assert_eq!(
        sql(&s),
        ["ALTER TABLE [m3].[c] ADD [c_new] nvarchar(40) NULL"]
    );
    // A nullable column with a default fills existing rows (WITH VALUES).
    let mut filled = added.clone();
    filled.default = Some("N'n/a'".into());
    let s = MsSqlDdl
        .create(&child(&filled), &DdlContext::new(&v).with_table(&t))
        .unwrap();
    assert_eq!(
        sql(&s),
        [
            "ALTER TABLE [m3].[c] ADD [c_new] nvarchar(40) NULL CONSTRAINT [DF_c_c_new] DEFAULT \
          (N'n/a') WITH VALUES"
        ]
    );
    // NOT NULL.
    let c = t.columns[1].clone();
    let mut n = c.clone();
    n.nullable = false;
    assert_eq!(
        sql(&step(&t, &c, &n, &[])),
        ["ALTER TABLE [m3].[c] ALTER COLUMN [c_extra] nvarchar(40) NOT NULL"]
    );
    // A literal default.
    let mut d = n.clone();
    d.default = Some(BracketQuoting.quote_str("it's \u{fc}"));
    t.columns[1] = n.clone();
    assert_eq!(
        sql(&step(&t, &n, &d, &[])),
        [
            "ALTER TABLE [m3].[c] ADD CONSTRAINT [DF_c_c_extra] DEFAULT (N'it''s \u{fc}') FOR \
          [c_extra]"
        ]
    );
    // As fetched afterwards, then a comment and a wider neutral type.
    let mut fetched = d.clone();
    fetched.default = Some("(N'it''s \u{fc}')".into());
    fetched
        .extras
        .insert(extras::DEFAULT_CONSTRAINT, "DF_c_c_extra");
    fetched.comment = Some("note".into());
    t.columns[1] = fetched.clone();
    let mut wide = fetched.clone();
    wide.data_type = DataType::VarChar { length: Some(80) };
    wide.native_type = String::new();
    assert_eq!(
        sql(&step(&t, &fetched, &wide, &[])),
        ["ALTER TABLE [m3].[c] ALTER COLUMN [c_extra] nvarchar(80) NOT NULL"]
    );
    // Rename to a reserved word with a quote and non-ASCII text.
    let mut renamed = fetched.clone();
    renamed.name = "sel\"ect_\u{fc}".into();
    let renames = [Rename::new(
        ObjectKind::Column,
        "c_extra",
        "sel\"ect_\u{fc}",
    )];
    assert_eq!(
        sql(&step(&t, &fetched, &renamed, &renames)),
        ["EXEC sp_rename N'[m3].[c].[c_extra]', N'sel\"ect_\u{fc}', N'COLUMN'"]
    );
    // Drop: the default first.
    let gone = t.obj.child(ObjectKind::Column, "c_extra");
    let s = MsSqlDdl
        .drop_object(&gone, &DdlContext::new(&v).with_table(&t))
        .unwrap();
    assert_eq!(
        sql(&s),
        [
            "ALTER TABLE [m3].[c] DROP CONSTRAINT [DF_c_c_extra]",
            "ALTER TABLE [m3].[c] DROP COLUMN [c_extra]",
        ]
    );
    // A column change needs the table.
    let made = MsSqlDdl.alter(&child(&c), &child(&n), &DdlContext::new(&v));
    assert!(matches!(made, Err(Error::InvalidInput { .. })), "{made:?}");
}

#[test]
fn types_are_checked_and_spelled() {
    assert_eq!(
        checked_type("c", "decimal(12,2)").unwrap(),
        "decimal(12, 2)"
    );
    assert_eq!(checked_type("c", "nvarchar(MAX)").unwrap(), "nvarchar(max)");
    assert_eq!(checked_type("c", "dbo.phone").unwrap(), "[dbo].[phone]");
    assert_eq!(
        checked_type("c", "[my]]schema].[t]").unwrap(),
        "[my]]schema].[t]"
    );
    assert_eq!(
        checked_type("c", "double  precision").unwrap(),
        "double precision"
    );
    for bad in [
        "int NOT NULL",
        "int; DROP TABLE x",
        "varchar(10) COLLATE x",
        "nvarchar(1) DEFAULT 1",
        "int)",
        "varchar('x')",
        "a.b.c",
        "[unclosed",
        "",
    ] {
        assert!(checked_type("c", bad).is_err(), "{bad}");
    }
    let v16 = ServerVersion::new(16, 0, 0);
    let t = |d: DataType, v: &ServerVersion| neutral_type(&d, v).unwrap();
    assert_eq!(
        t(DataType::VarChar { length: Some(40) }, &v17()),
        "nvarchar(40)"
    );
    assert_eq!(
        t(DataType::VarChar { length: None }, &v17()),
        "nvarchar(max)"
    );
    assert_eq!(
        t(DataType::VarChar { length: Some(5000) }, &v17()),
        "nvarchar(max)"
    );
    assert_eq!(t(DataType::Json { binary: true }, &v17()), "json");
    assert_eq!(t(DataType::Json { binary: true }, &v16), "nvarchar(max)");
    assert_eq!(
        t(
            DataType::Int {
                bits: 64,
                unsigned: true
            },
            &v17()
        ),
        "decimal(20, 0)"
    );
    assert_eq!(
        t(
            DataType::Int {
                bits: 8,
                unsigned: true
            },
            &v17()
        ),
        "tinyint"
    );
    assert_eq!(
        t(DataType::Timestamp { precision: Some(6) }, &v17()),
        "datetime2(6)"
    );
    assert_eq!(t(DataType::Uuid, &v17()), "uniqueidentifier");
    assert_eq!(t(DataType::Bool, &v17()), "bit");
    assert!(matches!(
        neutral_type(&DataType::Interval, &v17()),
        Err(Error::Unsupported { .. })
    ));
}

#[test]
fn widening_and_narrowing() {
    for (old, new, wide) in [
        ("int", "bigint", true),
        ("bigint", "int", false),
        ("tinyint", "smallint", true),
        ("varchar(20)", "nvarchar(20)", true),
        ("nvarchar(20)", "varchar(20)", false),
        ("varchar(20)", "varchar(10)", false),
        ("varchar(20)", "varchar(max)", true),
        ("varchar(max)", "varchar(8000)", false),
        ("decimal(10,2)", "decimal(12,2)", true),
        ("decimal(10,2)", "decimal(10,3)", false),
        ("int", "decimal(10,0)", true),
        ("int", "decimal(9,0)", false),
        ("datetime2(3)", "datetime2(7)", true),
        ("datetime2(7)", "datetime2(3)", false),
        ("text", "nvarchar(max)", true),
        ("real", "float(53)", true),
        ("float(53)", "real", false),
        ("varbinary(10)", "varbinary(max)", true),
        ("date", "datetime2(7)", true),
        ("date", "datetime", false),
        ("datetime", "datetime2(7)", true),
        ("datetime", "datetime2(3)", false),
        ("bit", "tinyint", true),
        ("bit", "bigint", true),
        ("int", "bit", false),
    ] {
        assert_eq!(is_widening(old, new), wide, "{old} -> {new}");
    }
}
