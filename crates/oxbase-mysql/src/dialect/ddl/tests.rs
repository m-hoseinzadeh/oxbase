//! Golden tests of the generated DDL: quoting of reserved words, quotes, backslashes and unicode;
//! literal defaults and comments; the order and flags of an alter; version gates; refusals.

use super::*;
use oxbase_types::{GeneratedColumn, IndexColumn};

fn v(text: &str) -> ServerVersion {
    ServerVersion::parse(text).unwrap()
}

fn mysql() -> MySqlDdl {
    MySqlDdl { mariadb: false }
}

fn mariadb() -> MySqlDdl {
    MySqlDdl { mariadb: true }
}

fn col(name: &str, native: &str, data_type: DataType) -> ColumnDef {
    let mut c = ColumnDef::new(name, data_type);
    c.native_type = native.into();
    c
}

fn table_ref() -> ObjectRef {
    ObjectRef::table(Some("db"), None, "order")
}

/// A table with a reserved name, a reserved column name, a quoted literal default, a unicode
/// comment with quotes and a backslash, a generated column, an `ON UPDATE`, two indexes, a check
/// and a foreign key.
fn table() -> TableDef {
    let mut t = TableDef::new(table_ref());
    let mut id = col("id", "int", DataType::INT32).not_null();
    id.auto_increment = true;
    t.columns.push(id);
    let mut code = col(
        "select",
        "varchar(20)",
        DataType::VarChar { length: Some(20) },
    )
    .not_null();
    code.default = Some(BacktickQuoting.quote_str("it's a \\ \"q\""));
    code.comment = Some("ünï 'c' \\".into());
    code.collation = Some("utf8mb4_bin".into());
    code.extras.insert(props::CHARSET, "utf8mb4");
    t.columns.push(code);
    let mut amount = col(
        "amount",
        "decimal(12,2)",
        DataType::Decimal {
            precision: Some(12),
            scale: Some(2),
        },
    );
    amount.default = Some("'0'".into());
    t.columns.push(amount);
    let mut at = col("at", "datetime", DataType::Timestamp { precision: None }).not_null();
    at.default = Some("CURRENT_TIMESTAMP".into());
    at.extras.insert(ON_UPDATE_EXPRESSION, "CURRENT_TIMESTAMP");
    t.columns.push(at);
    let mut total = col(
        "total",
        "decimal(13,2)",
        DataType::Decimal {
            precision: Some(13),
            scale: Some(2),
        },
    );
    total.generated = Some(GeneratedColumn {
        expression: "amount * 2".into(),
        stored: true,
    });
    t.columns.push(total);
    t.columns.push(col("parent", "int", DataType::INT32));
    t.primary_key = Some(PrimaryKeyDef {
        name: Some("PRIMARY".into()),
        columns: vec!["id".into()],
    });
    t.indexes.push(IndexDef {
        name: "uq_select".into(),
        columns: vec![IndexColumn::column("select")],
        unique: true,
        is_constraint: true,
        predicate: None,
        extras: Extras::new(),
    });
    t.indexes.push(IndexDef {
        name: "ix_amount".into(),
        columns: vec![
            IndexColumn {
                length: Some(10),
                ..IndexColumn::column("select")
            },
            IndexColumn {
                descending: true,
                ..IndexColumn::column("amount")
            },
        ],
        unique: false,
        is_constraint: false,
        predicate: None,
        extras: Extras::new()
            .with(props::INDEX_METHOD, "btree")
            .with(props::COMMENT, "by 'amount'"),
    });
    t.checks.push(CheckDef {
        name: Some("chk_amount".into()),
        expression: "amount >= 0".into(),
        extras: Extras::new(),
    });
    t.foreign_keys.push(ForeignKeyDef {
        name: Some("fk_parent".into()),
        columns: vec!["parent".into()],
        ref_table: ObjectRef::table(None, None, "parent"),
        ref_columns: vec!["id".into()],
        on_delete: FkAction::Cascade,
        on_update: FkAction::NoAction,
        extras: Extras::new(),
    });
    t.comment = Some("Orders: it's \\ ü".into());
    t.extras.insert(props::STORAGE_ENGINE, "InnoDB");
    t.extras.insert(props::CHARSET, "utf8mb4");
    t.extras.insert(props::COLLATION, "utf8mb4_0900_ai_ci");
    t
}

fn create(g: MySqlDdl, version: &str, def: &ObjectDef) -> Result<Script> {
    let version = v(version);
    g.create(def, &DdlContext::new(&version))
}

fn alter(
    g: MySqlDdl,
    version: &str,
    old: &TableDef,
    new: &TableDef,
    renames: &[Rename],
) -> Result<Script> {
    let version = v(version);
    g.alter(
        &ObjectDef::Table(old.clone()),
        &ObjectDef::Table(new.clone()),
        &DdlContext::new(&version).with_renames(renames),
    )
}

fn sqls(script: &Script) -> Vec<&str> {
    script.steps.iter().map(|s| s.sql.as_str()).collect()
}

#[test]
fn create_table_golden() {
    let script = create(mysql(), "8.4.6", &ObjectDef::Table(table())).unwrap();
    assert_eq!(script.len(), 1);
    let step = &script.steps[0];
    assert_eq!(
        step.sql,
        r#"CREATE TABLE `db`.`order` (
  `id` int NOT NULL AUTO_INCREMENT,
  `select` varchar(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL DEFAULT 'it''s a \\ "q"' COMMENT 'ünï ''c'' \\',
  `amount` decimal(12,2) NULL DEFAULT '0',
  `at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  `total` decimal(13,2) GENERATED ALWAYS AS (amount * 2) STORED NULL,
  `parent` int NULL,
  PRIMARY KEY (`id`),
  UNIQUE INDEX `uq_select` (`select`),
  INDEX `ix_amount` (`select`(10), `amount` DESC) USING BTREE COMMENT 'by ''amount''',
  CONSTRAINT `chk_amount` CHECK (amount >= 0),
  CONSTRAINT `fk_parent` FOREIGN KEY (`parent`) REFERENCES `db`.`parent` (`id`) ON DELETE CASCADE ON UPDATE NO ACTION
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci COMMENT = 'Orders: it''s \\ ü'"#
    );
    assert_eq!(step.description, "Create table order");
    assert!(!step.transactional && !step.destructive);
    assert!(!script.is_atomic());
    assert!(script.warnings.is_empty(), "{:?}", script.warnings);
}

#[test]
fn create_on_mariadb_and_old_servers() {
    let mut t = table();
    t.indexes.truncate(1);
    let mut doc = col("doc", "longtext", DataType::Json { binary: false });
    doc.collation = Some("utf8mb4_bin".into());
    doc.extras.insert(props::CHARSET, "utf8mb4");
    t.columns.push(doc);
    let script = create(mariadb(), "11.8.9-MariaDB", &ObjectDef::Table(t.clone())).unwrap();
    // MariaDB's JSON is written `json` (it adds the json_valid check itself).
    assert!(
        script.steps[0].sql.contains("\n  `doc` json NULL,\n"),
        "{}",
        script.steps[0].sql
    );

    // Checks are parsed but dropped before MySQL 8.0.16: a warning says so.
    let script = create(mysql(), "8.0.15", &ObjectDef::Table(table())).unwrap();
    assert!(
        script.warnings[0].contains("8.0.16"),
        "{:?}",
        script.warnings
    );
    // Descending keys need MySQL 8.0.
    let err = create(mysql(), "5.7.44", &ObjectDef::Table(table())).unwrap_err();
    assert!(matches!(err, Error::Unsupported { .. }), "{err}");
    let err = create(mariadb(), "10.6.20-MariaDB", &ObjectDef::Table(table())).unwrap_err();
    assert!(err.to_string().contains("descending"), "{err}");
}

#[test]
fn refusals() {
    let check = |t: TableDef, g: MySqlDdl| {
        create(g, "8.4.6", &ObjectDef::Table(t)).expect_err("should be refused")
    };
    let mut t = table();
    t.foreign_keys[0].on_delete = FkAction::SetDefault;
    assert!(matches!(check(t, mysql()), Error::Unsupported { .. }));
    let mut t = table();
    t.columns[0].identity = Some(oxbase_types::IdentityDef {
        always: false,
        start: None,
        increment: None,
    });
    assert!(matches!(check(t, mysql()), Error::Unsupported { .. }));
    let mut t = table();
    t.indexes[0].predicate = Some("amount > 0".into());
    assert!(matches!(check(t, mysql()), Error::Unsupported { .. }));
    let mut t = table();
    t.indexes[1].columns[0] = IndexColumn {
        is_expression: true,
        ..IndexColumn::column("lower(`select`)")
    };
    assert!(matches!(
        check(t.clone(), mariadb()),
        Error::Unsupported { .. }
    ));
    // MySQL 8.0.13+ takes functional key parts.
    let ok = create(mysql(), "8.4.6", &ObjectDef::Table(t)).unwrap();
    assert!(
        ok.steps[0]
            .sql
            .contains("((lower(`select`)), `amount` DESC)")
    );
    let mut t = table();
    t.checks[0].extras.insert(ENFORCED, false);
    assert!(matches!(
        check(t.clone(), mariadb()),
        Error::Unsupported { .. }
    ));
    let ok = create(mysql(), "8.4.6", &ObjectDef::Table(t)).unwrap();
    assert!(ok.steps[0].sql.contains("CHECK (amount >= 0) NOT ENFORCED"));
    let mut t = table();
    t.columns[1].native_type = "varchar(20)) ; DROP TABLE x".into();
    assert!(matches!(check(t, mysql()), Error::InvalidInput { .. }));
    let mut t = table();
    t.columns[1].collation = Some("utf8mb4_bin; DROP".into());
    assert!(matches!(check(t, mysql()), Error::InvalidInput { .. }));
    let mut t = table();
    t.columns.clear();
    assert!(matches!(check(t, mysql()), Error::InvalidInput { .. }));
    let mut t = table();
    t.foreign_keys[0].ref_columns.push("x".into());
    assert!(matches!(check(t, mysql()), Error::InvalidInput { .. }));
    let mut t = table();
    t.indexes[0].name = "PRIMARY".into();
    assert!(matches!(check(t, mysql()), Error::InvalidInput { .. }));
}

#[test]
fn unchanged_tables_give_empty_scripts() {
    let t = table();
    assert!(alter(mysql(), "8.4.6", &t, &t, &[]).unwrap().is_empty());
    // The same table as the server spells it back (MariaDB): display widths, default and
    // expression spellings, an unnamed primary key, a unique index flagged as a constraint, the
    // RESTRICT / NO ACTION pair and the index InnoDB made for the foreign key.
    let mut back = table();
    back.columns[0].native_type = "int(11)".into();
    back.columns[2].default = Some("0.00".into());
    back.columns[3].default = Some("current_timestamp()".into());
    back.columns[3]
        .extras
        .insert(ON_UPDATE_EXPRESSION, "current_timestamp()");
    back.columns[4].generated.as_mut().unwrap().expression = "`amount` * 2".into();
    back.primary_key.as_mut().unwrap().name = None;
    back.indexes[0].is_constraint = false;
    back.foreign_keys[0].on_update = FkAction::Restrict;
    back.checks[0].expression = "`amount` >= 0".into();
    back.indexes.push(IndexDef {
        name: "fk_parent".into(),
        columns: vec![IndexColumn::column("parent")],
        unique: false,
        is_constraint: false,
        predicate: None,
        extras: Extras::new(),
    });
    let script = alter(mariadb(), "11.8.9-MariaDB", &t, &back, &[]).unwrap();
    assert!(script.is_empty(), "{:?}", sqls(&script));
    let script = alter(mariadb(), "11.8.9-MariaDB", &back, &t, &[]).unwrap();
    assert!(script.is_empty(), "{:?}", sqls(&script));
}

#[test]
fn alter_golden_order_and_flags() {
    let old = table();
    let mut new = apply_renames(
        &old,
        &[Rename::new(ObjectKind::Column, "select", "code")],
        true,
    );
    new.columns.retain(|c| c.name != "at");
    new.columns[2].native_type = "decimal(10,2)".into();
    new.columns[2].data_type = DataType::Decimal {
        precision: Some(10),
        scale: Some(2),
    };
    new.columns.insert(2, col("note", "text", DataType::Text));
    new.indexes.retain(|i| i.name != "ix_amount");
    new.indexes.push(IndexDef {
        name: "ix_note".into(),
        columns: vec![IndexColumn {
            length: Some(20),
            ..IndexColumn::column("note")
        }],
        unique: false,
        is_constraint: false,
        predicate: None,
        extras: Extras::new(),
    });
    new.checks[0].expression = "amount > 0".into();
    new.comment = Some("New".into());
    let renames = [Rename::new(ObjectKind::Column, "select", "code")];
    let script = alter(mysql(), "8.4.6", &old, &new, &renames).unwrap();
    assert_eq!(
        sqls(&script),
        [
            "ALTER TABLE `db`.`order`\n  RENAME COLUMN `select` TO `code`",
            "ALTER TABLE `db`.`order`\n  DROP CHECK `chk_amount`",
            "ALTER TABLE `db`.`order`\n  \
             DROP INDEX `ix_amount`,\n  \
             DROP COLUMN `at`,\n  \
             ADD COLUMN `note` text NULL AFTER `code`,\n  \
             MODIFY COLUMN `amount` decimal(10,2) NULL DEFAULT '0',\n  \
             ADD INDEX `ix_note` (`note`(20)),\n  \
             ADD CONSTRAINT `chk_amount` CHECK (amount > 0),\n  \
             COMMENT = 'New'",
        ]
    );
    let flags: Vec<(bool, bool)> = script
        .steps
        .iter()
        .map(|s| (s.destructive, s.transactional))
        .collect();
    assert_eq!(flags, [(false, false), (true, false), (true, false)]);
    assert_eq!(
        script.steps[2].description,
        "Alter table order: drop index ix_amount, drop column at, add column note, modify column \
         amount, add index ix_note, add check chk_amount, set the table comment"
    );
    assert_eq!(script.warnings, [NOT_ATOMIC]);

    // MariaDB drops a check with DROP CONSTRAINT.
    let script = alter(mariadb(), "11.8.9-MariaDB", &old, &new, &renames).unwrap();
    assert_eq!(
        script.steps[1].sql,
        "ALTER TABLE `db`.`order`\n  DROP CONSTRAINT `chk_amount`"
    );
}

#[test]
fn alter_keeps_attributes_and_moves_columns() {
    let old = table();
    // Only the comment of `select` changes, and it moves to the end; the new definition does
    // not repeat the character set: MODIFY still carries every attribute.
    let mut new = old.clone();
    let mut code = new.columns.remove(1);
    code.comment = Some("new comment".into());
    code.extras = Extras::new();
    code.collation = None;
    new.columns.push(code);
    let script = alter(mysql(), "8.4.6", &old, &new, &[]).unwrap();
    assert_eq!(
        sqls(&script),
        [
            "ALTER TABLE `db`.`order`\n  MODIFY COLUMN `select` varchar(20) CHARACTER SET utf8mb4 \
             COLLATE utf8mb4_bin NOT NULL DEFAULT 'it''s a \\\\ \"q\"' COMMENT 'new comment' \
             AFTER `parent`"
        ]
    );
    assert!(script.warnings.is_empty());
    assert!(!script.steps[0].destructive);

    // A column moved first; a widening type change is not destructive.
    let mut new = old.clone();
    let parent = new.columns.pop().unwrap();
    new.columns.insert(0, parent);
    new.columns[0].native_type = "bigint".into();
    new.columns[0].data_type = DataType::INT64;
    let script = alter(mysql(), "8.4.6", &old, &new, &[]).unwrap();
    assert_eq!(
        sqls(&script),
        ["ALTER TABLE `db`.`order`\n  MODIFY COLUMN `parent` bigint NULL FIRST"]
    );
    assert!(!script.steps[0].destructive);
}

#[test]
fn renames_on_old_servers_and_tables() {
    let old = table();
    let renames = [
        Rename::new(ObjectKind::Table, "order", "orders"),
        Rename::new(ObjectKind::Column, "parent", "parent_id"),
        Rename::new(ObjectKind::Index, "uq_select", "uq_code"),
    ];
    let mut new = apply_renames(&old, &renames, true);
    new.obj.name = "orders".into();
    let script = alter(mysql(), "8.4.6", &old, &new, &renames).unwrap();
    assert_eq!(
        sqls(&script),
        [
            "RENAME TABLE `db`.`order` TO `db`.`orders`",
            "ALTER TABLE `db`.`orders`\n  RENAME COLUMN `parent` TO `parent_id`,\n  RENAME INDEX \
             `uq_select` TO `uq_code`",
        ]
    );
    assert_eq!(script.steps[0].description, "Rename table order to orders");

    // MariaDB 10.4: CHANGE with the full definition, and the index is dropped and added.
    let script = alter(mariadb(), "10.4.34-MariaDB", &old, &new, &renames).unwrap();
    let sql = sqls(&script);
    assert_eq!(
        sql[1],
        "ALTER TABLE `db`.`orders`\n  CHANGE COLUMN `parent` `parent_id` int NULL"
    );
    assert_eq!(
        sql[2],
        "ALTER TABLE `db`.`orders`\n  DROP INDEX `uq_select`,\n  ADD UNIQUE INDEX `uq_code` \
         (`select`)"
    );

    // A renamed check is dropped and added under its new name.
    let mut new = old.clone();
    new.checks[0].name = Some("chk_amount2".into());
    let script = alter(mysql(), "8.4.6", &old, &new, &[]).unwrap();
    assert_eq!(
        sqls(&script),
        [
            "ALTER TABLE `db`.`order`\n  DROP CHECK `chk_amount`",
            "ALTER TABLE `db`.`order`\n  ADD CONSTRAINT `chk_amount2` CHECK (amount >= 0)",
        ]
    );
}

#[test]
fn keys_and_foreign_keys() {
    let old = table();
    // A new primary key, a changed foreign key and a removed one with InnoDB's index.
    let mut with_fk_index = old.clone();
    with_fk_index.indexes.push(IndexDef {
        name: "fk_parent".into(),
        columns: vec![IndexColumn::column("parent")],
        unique: false,
        is_constraint: false,
        predicate: None,
        extras: Extras::new(),
    });
    let mut new = old.clone();
    new.foreign_keys.clear();
    new.primary_key = Some(PrimaryKeyDef {
        name: None,
        columns: vec!["id".into(), "parent".into()],
    });
    let script = alter(mysql(), "8.4.6", &with_fk_index, &new, &[]).unwrap();
    assert_eq!(
        sqls(&script),
        [
            "ALTER TABLE `db`.`order`\n  DROP FOREIGN KEY `fk_parent`",
            "ALTER TABLE `db`.`order`\n  DROP INDEX `fk_parent`,\n  DROP PRIMARY KEY,\n  ADD \
             PRIMARY KEY (`id`, `parent`)",
        ]
    );
    assert!(script.steps.iter().all(|s| s.destructive));

    let mut new = old.clone();
    new.foreign_keys[0].on_delete = FkAction::SetNull;
    let script = alter(mysql(), "8.4.6", &with_fk_index, &new, &[]).unwrap();
    assert_eq!(
        sqls(&script),
        [
            "ALTER TABLE `db`.`order`\n  DROP FOREIGN KEY `fk_parent`",
            "ALTER TABLE `db`.`order`\n  ADD CONSTRAINT `fk_parent` FOREIGN KEY (`parent`) \
             REFERENCES `db`.`parent` (`id`) ON DELETE SET NULL ON UPDATE NO ACTION",
        ]
    );
}

#[test]
fn child_cells() {
    let t = table();
    let version = v("8.4.6");
    let ctx = DdlContext::new(&version);
    let g = mysql();
    let column = ObjectDef::Column {
        table: table_ref(),
        column: col(
            "key",
            "int unsigned",
            DataType::Int {
                bits: 32,
                unsigned: true,
            },
        ),
    };
    assert_eq!(
        sqls(&g.create(&column, &ctx).unwrap()),
        ["ALTER TABLE `db`.`order` ADD COLUMN `key` int unsigned NULL"]
    );
    let index = ObjectDef::Index {
        table: table_ref(),
        index: t.indexes[0].clone(),
    };
    assert_eq!(
        sqls(&g.create(&index, &ctx).unwrap()),
        ["ALTER TABLE `db`.`order` ADD UNIQUE INDEX `uq_select` (`select`)"]
    );
    let fk = ObjectDef::ForeignKey {
        table: table_ref(),
        foreign_key: t.foreign_keys[0].clone(),
    };
    assert!(
        g.create(&fk, &ctx).unwrap().steps[0]
            .sql
            .starts_with("ALTER TABLE `db`.`order` ADD CONSTRAINT `fk_parent` FOREIGN KEY")
    );
    let check = ObjectDef::Check {
        table: table_ref(),
        check: t.checks[0].clone(),
    };
    assert_eq!(
        sqls(&g.create(&check, &ctx).unwrap()),
        ["ALTER TABLE `db`.`order` ADD CONSTRAINT `chk_amount` CHECK (amount >= 0)"]
    );

    // A child alter needs the table, and renames a column by itself.
    let mut renamed = t.columns[5].clone();
    renamed.name = "parent_id".into();
    renamed.comment = Some("the parent".into());
    let before = ObjectDef::Column {
        table: table_ref(),
        column: t.columns[5].clone(),
    };
    let after = ObjectDef::Column {
        table: table_ref(),
        column: renamed,
    };
    assert!(matches!(
        g.alter(&before, &after, &ctx),
        Err(Error::InvalidInput { .. })
    ));
    let script = g.alter(&before, &after, &ctx.with_table(&t)).unwrap();
    assert_eq!(
        sqls(&script),
        [
            "ALTER TABLE `db`.`order`\n  RENAME COLUMN `parent` TO `parent_id`",
            "ALTER TABLE `db`.`order`\n  MODIFY COLUMN `parent_id` int NULL COMMENT 'the parent'",
        ]
    );
    let mut changed = t.checks[0].clone();
    changed.expression = "amount > 1".into();
    let script = g
        .alter(
            &check,
            &ObjectDef::Check {
                table: table_ref(),
                check: changed,
            },
            &ctx.with_table(&t),
        )
        .unwrap();
    assert_eq!(script.len(), 2);
}

#[test]
fn drops() {
    let version = v("8.4.6");
    let ctx = DdlContext::new(&version);
    let t = table_ref();
    let drop = |g: MySqlDdl, r: &ObjectRef| g.drop_object(r, &ctx).unwrap();
    let script = drop(mysql(), &t);
    assert_eq!(sqls(&script), ["DROP TABLE `db`.`order`"]);
    assert!(script.steps[0].destructive && !script.steps[0].transactional);
    for (kind, name, sql) in [
        (
            ObjectKind::Column,
            "a`b",
            "ALTER TABLE `db`.`order` DROP COLUMN `a``b`",
        ),
        (
            ObjectKind::Index,
            "PRIMARY",
            "ALTER TABLE `db`.`order` DROP PRIMARY KEY",
        ),
        (
            ObjectKind::Index,
            "ix",
            "ALTER TABLE `db`.`order` DROP INDEX `ix`",
        ),
        (
            ObjectKind::ForeignKey,
            "fk",
            "ALTER TABLE `db`.`order` DROP FOREIGN KEY `fk`",
        ),
        (
            ObjectKind::Check,
            "c",
            "ALTER TABLE `db`.`order` DROP CHECK `c`",
        ),
    ] {
        assert_eq!(sqls(&drop(mysql(), &t.child(kind, name))), [sql]);
    }
    assert_eq!(
        sqls(&drop(mariadb(), &t.child(ObjectKind::Check, "c"))),
        ["ALTER TABLE `db`.`order` DROP CONSTRAINT `c`"]
    );
    assert!(
        mysql()
            .drop_object(&ObjectRef::new(ObjectKind::Column, "orphan"), &ctx)
            .is_err()
    );
    assert!(matches!(
        mysql().drop_object(&ObjectRef::new(ObjectKind::View, "v"), &ctx),
        Err(Error::Unsupported { .. })
    ));
}
