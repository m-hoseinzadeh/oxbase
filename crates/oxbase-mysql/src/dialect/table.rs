//! Tables and their children: columns, indexes, foreign keys and check constraints.

use async_trait::async_trait;
use oxbase_dialect::Fetcher;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    CheckDef, ColumnDef, Extras, FkAction, ForeignKeyDef, GeneratedColumn, IndexColumn, IndexDef,
    ObjectDef, ObjectKind, ObjectRef, ObjectSummary, PrimaryKeyDef, Result, TableDef, props,
};

use super::coltype::{TypeFacts, charset_of_collation, column_default, data_type};
use super::rows::{Row, query, text};
use super::{
    ENFORCED, INVISIBLE, ON_UPDATE_EXPRESSION, by_name, database_of, expect_kind, is_mariadb,
    is_system_database, not_found, owner_name, scoped,
};

/// The name MySQL and MariaDB give every primary key (index and constraint).
pub(crate) const PRIMARY: &str = "PRIMARY";

/// `TABLE_TYPE`s listed as tables (MariaDB adds system-versioned and, 11.2+, temporary ones).
const TABLE_TYPES: &str = "('BASE TABLE', 'SYSTEM VERSIONED', 'TEMPORARY')";

// ---- tables -----------------------------------------------------------------------------------

/// Lists the tables of a database and loads one as a [`TableDef`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TableFetcher;

#[async_trait]
impl Fetcher for TableFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let db = database_of(c, parent, &cancel).await?;
        let parent = scoped(parent, &db);
        let sql = format!(
            "SELECT TABLE_NAME, TABLE_TYPE, ENGINE, TABLE_ROWS, DATA_LENGTH, INDEX_LENGTH, \
             TABLE_COMMENT FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_TYPE IN {TABLE_TYPES} ORDER BY TABLE_NAME"
        );
        let rows = query(c, &sql, &[text(&db)], &cancel).await?;
        let system = is_system_database(&db);
        Ok(by_name(
            rows.iter()
                .map(|row| {
                    let mut s = ObjectSummary::new(parent.child(ObjectKind::Table, row.string(0)));
                    s.system = system;
                    s.comment = row.non_empty(6);
                    s.row_estimate = row.u64(3);
                    if let Some(engine) = row.non_empty(2) {
                        s.extras.insert(props::STORAGE_ENGINE, engine);
                    }
                    if let (Some(data), Some(index)) = (row.int(4), row.int(5))
                        && let Ok(size) = i64::try_from(data + index)
                    {
                        s.extras.insert(props::SIZE_BYTES, size);
                    }
                    table_type_extras(&mut s.extras, &row.string(1));
                    s
                })
                .collect(),
        ))
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        expect_kind(obj, ObjectKind::Table)?;
        let db = database_of(c, obj, &cancel).await?;
        let table = obj.name.as_str();
        let sql = format!(
            "SELECT TABLE_TYPE, ENGINE, ROW_FORMAT, TABLE_COLLATION, TABLE_COMMENT, CREATE_OPTIONS \
             FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND TABLE_TYPE IN {TABLE_TYPES}"
        );
        let rows = query(c, &sql, &[text(&db), text(table)], &cancel).await?;
        let row = rows.first().ok_or_else(|| not_found(obj))?;

        let mut def = TableDef::new(scoped(obj, &db));
        def.comment = row.non_empty(4);
        if let Some(engine) = row.non_empty(1) {
            def.extras.insert(props::STORAGE_ENGINE, engine);
        }
        if let Some(format) = row.non_empty(2) {
            def.extras.insert(props::ROW_FORMAT, format);
        }
        if let Some(collation) = row.non_empty(3) {
            def.extras
                .insert(props::CHARSET, charset_of_collation(&collation).to_owned());
            def.extras.insert(props::COLLATION, collation);
        }
        table_type_extras(&mut def.extras, &row.string(0));

        let checks = read_checks(c, &db, table, None, &cancel).await?;
        def.columns = read_columns(c, &db, table, None, &checks, &cancel)
            .await?
            .into_iter()
            .map(|(col, _)| col)
            .collect();
        for index in read_indexes(c, &db, table, None, &cancel).await? {
            if index.name == PRIMARY {
                def.primary_key = Some(PrimaryKeyDef {
                    name: Some(index.name),
                    columns: index.columns.into_iter().map(|k| k.name).collect(),
                });
            } else {
                def.indexes.push(index);
            }
        }
        def.foreign_keys = read_foreign_keys(c, &db, table, None, &cancel).await?;
        def.checks = checks
            .into_iter()
            .filter(|k| !k.json_auto)
            .map(|k| k.def)
            .collect();
        Ok(ObjectDef::Table(def))
    }
}

/// Extras derived from `TABLE_TYPE` (anything but a plain base table).
fn table_type_extras(extras: &mut Extras, table_type: &str) {
    match table_type {
        "BASE TABLE" | "" => {}
        "TEMPORARY" => {
            extras.insert(props::TEMPORARY, true);
        }
        other => {
            extras.insert(props::TABLE_TYPE, other.to_ascii_lowercase());
        }
    }
}

// ---- columns ----------------------------------------------------------------------------------

const COLUMNS_SQL: &str = "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_DEFAULT, IS_NULLABLE, \
     DATA_TYPE, COLUMN_TYPE, CHARACTER_MAXIMUM_LENGTH, NUMERIC_PRECISION, NUMERIC_SCALE, \
     DATETIME_PRECISION, CHARACTER_SET_NAME, COLLATION_NAME, COLUMN_KEY, EXTRA, COLUMN_COMMENT, \
     GENERATION_EXPRESSION FROM information_schema.COLUMNS \
     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?";

/// The columns of `table` in ordinal order (or the one named `only`), each with "is part of the
/// primary key". `checks` are the table's checks on MariaDB (for `JSON` columns), else empty.
async fn read_columns(
    c: &dyn Connection,
    db: &str,
    table: &str,
    only: Option<&str>,
    checks: &[RawCheck],
    cancel: &CancellationToken,
) -> Result<Vec<(ColumnDef, bool)>> {
    let mariadb = is_mariadb(c);
    let mut params = vec![text(db), text(table)];
    let sql = match only {
        Some(name) => {
            params.push(text(name));
            format!("{COLUMNS_SQL} AND COLUMN_NAME = ? ORDER BY ORDINAL_POSITION")
        }
        None => format!("{COLUMNS_SQL} ORDER BY ORDINAL_POSITION"),
    };
    let rows = query(c, &sql, &params, cancel).await?;
    Ok(rows
        .iter()
        .map(|row| column_from_row(row, mariadb, checks))
        .collect())
}

fn column_from_row(row: &Row, mariadb: bool, checks: &[RawCheck]) -> (ColumnDef, bool) {
    let name = row.string(0);
    let facts = TypeFacts {
        data_type: row.string(4),
        column_type: row.string(5),
        char_len: row.u32(6),
        precision: row.u32(7),
        scale: row.u32(8),
        dt_precision: row.int(9).and_then(|p| u8::try_from(p).ok()),
    };
    let mariadb_json = mariadb
        && facts.data_type.eq_ignore_ascii_case("longtext")
        && checks
            .iter()
            .any(|k| k.json_auto && k.def.name.as_deref() == Some(name.as_str()));
    let ty = data_type(&facts, mariadb_json);
    let extra = row.string(13);
    let extra_upper = extra.to_ascii_uppercase();

    let mut col = ColumnDef::new(name, ty);
    col.native_type = facts.column_type;
    col.nullable = row.flag(3);
    col.default = column_default(mariadb, row.text(2).as_deref(), &extra, &col.data_type);
    col.auto_increment = extra_upper.contains("AUTO_INCREMENT");
    col.generated = row.non_empty(15).map(|expression| GeneratedColumn {
        expression,
        stored: extra_upper.contains("STORED") || extra_upper.contains("PERSISTENT"),
    });
    if col.generated.is_some() {
        col.default = None;
    }
    col.collation = row.non_empty(11);
    col.comment = row.non_empty(14);
    if let Some(cs) = row.non_empty(10) {
        col.extras.insert(props::CHARSET, cs);
    }
    if let Some(pos) = extra_upper.find("ON UPDATE ")
        && let Some(expr) = extra[pos + "ON UPDATE ".len()..].split_whitespace().next()
    {
        col.extras.insert(ON_UPDATE_EXPRESSION, expr.to_owned());
    }
    if extra_upper.contains("INVISIBLE") {
        col.extras.insert(INVISIBLE, true);
    }
    (col, row.string(12).eq_ignore_ascii_case("PRI"))
}

/// Lists the columns of a table or view and loads one.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ColumnFetcher;

#[async_trait]
impl Fetcher for ColumnFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let db = database_of(c, parent, &cancel).await?;
        let parent = scoped(parent, &db);
        let checks = mariadb_checks(c, &db, &parent.name, &cancel).await?;
        let cols = read_columns(c, &db, &parent.name, None, &checks, &cancel).await?;
        let system = is_system_database(&db);
        Ok(cols
            .into_iter()
            .map(|(col, pk)| {
                let mut s = ObjectSummary::new(parent.child(ObjectKind::Column, col.name));
                s.system = system;
                s.comment = col.comment;
                s.extras.insert(props::NATIVE_TYPE, col.native_type);
                s.extras.insert(props::PRIMARY_KEY, pk);
                s
            })
            .collect())
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        expect_kind(obj, ObjectKind::Column)?;
        let db = database_of(c, obj, &cancel).await?;
        let table = owner_name(obj)?;
        let checks = mariadb_checks(c, &db, table, &cancel).await?;
        let (column, _) = read_columns(c, &db, table, Some(&obj.name), &checks, &cancel)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| not_found(obj))?;
        let table = scoped(obj, &db)
            .owner()
            .unwrap_or_else(|| ObjectRef::table(Some(&db), None, table));
        Ok(ObjectDef::Column { table, column })
    }
}

/// The table's checks on MariaDB (needed to recognise `JSON` columns), else nothing.
async fn mariadb_checks(
    c: &dyn Connection,
    db: &str,
    table: &str,
    cancel: &CancellationToken,
) -> Result<Vec<RawCheck>> {
    if is_mariadb(c) {
        read_checks(c, db, table, None, cancel).await
    } else {
        Ok(Vec::new())
    }
}

// ---- indexes ----------------------------------------------------------------------------------

/// `STATISTICS` query; version-dependent columns are fixed SQL text.
fn index_sql(c: &dyn Connection, one: bool) -> String {
    let v = c.server_version();
    let mariadb = is_mariadb(c);
    let expression = if !mariadb && v.at_least_patch(8, 0, 13) {
        "EXPRESSION"
    } else {
        "NULL"
    };
    let visible = match (mariadb, v.at_least(10, 6), v.at_least(8, 0)) {
        (true, true, _) => "CASE IGNORED WHEN 'YES' THEN 'NO' ELSE 'YES' END",
        (false, _, true) => "IS_VISIBLE",
        _ => "'YES'",
    };
    let filter = if one { " AND INDEX_NAME = ?" } else { "" };
    format!(
        "SELECT INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, COLUMN_NAME, SUB_PART, COLLATION, \
         INDEX_TYPE, INDEX_COMMENT, {expression}, {visible} FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?{filter} ORDER BY INDEX_NAME, SEQ_IN_INDEX"
    )
}

/// The indexes of `table` (or the one named `only`), the primary key included as `PRIMARY`
/// with `PRIMARY_KEY = true`.
async fn read_indexes(
    c: &dyn Connection,
    db: &str,
    table: &str,
    only: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Vec<IndexDef>> {
    let mut params = vec![text(db), text(table)];
    if let Some(name) = only {
        params.push(text(name));
    }
    let rows = query(c, &index_sql(c, only.is_some()), &params, cancel).await?;
    let mut out: Vec<IndexDef> = Vec::new();
    for row in &rows {
        let name = row.string(0);
        let key = match (row.non_empty(3), row.non_empty(8)) {
            (Some(column), _) => IndexColumn::column(column),
            (None, Some(expression)) => IndexColumn {
                name: expression,
                is_expression: true,
                descending: false,
                length: None,
            },
            (None, None) => IndexColumn::column(String::new()),
        };
        let key = IndexColumn {
            descending: row.string(5).eq_ignore_ascii_case("D"),
            length: row.u32(4),
            ..key
        };
        if let Some(last) = out.last_mut().filter(|i| i.name == name) {
            last.columns.push(key);
            continue;
        }
        let unique = !row.flag(1);
        let mut index = IndexDef {
            name,
            columns: vec![key],
            unique,
            // A unique index is also listed as a UNIQUE constraint (they are the same thing).
            is_constraint: unique,
            predicate: None,
            extras: Extras::new(),
        };
        if let Some(method) = row.non_empty(6) {
            index.extras.insert(props::INDEX_METHOD, method);
        }
        if let Some(comment) = row.non_empty(7) {
            index.extras.insert(props::COMMENT, comment);
        }
        if row.string(9).eq_ignore_ascii_case("NO") {
            index.extras.insert(INVISIBLE, true);
        }
        if index.name == PRIMARY {
            index.extras.insert(props::PRIMARY_KEY, true);
        }
        out.push(index);
    }
    Ok(out)
}

/// Lists the indexes of a table (the primary key as `PRIMARY`) and loads one.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct IndexFetcher;

#[async_trait]
impl Fetcher for IndexFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        if parent.kind != ObjectKind::Table {
            return Ok(Vec::new());
        }
        let db = database_of(c, parent, &cancel).await?;
        let parent = scoped(parent, &db);
        let system = is_system_database(&db);
        Ok(by_name(
            read_indexes(c, &db, &parent.name, None, &cancel)
                .await?
                .into_iter()
                .map(|index| {
                    let mut s = ObjectSummary::new(parent.child(ObjectKind::Index, index.name));
                    s.system = system;
                    s.extras.insert(
                        props::PRIMARY_KEY,
                        index.extras.contains(&props::PRIMARY_KEY),
                    );
                    s.extras.insert(props::UNIQUE, index.unique);
                    s
                })
                .collect(),
        ))
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        expect_kind(obj, ObjectKind::Index)?;
        let db = database_of(c, obj, &cancel).await?;
        let table = owner_name(obj)?;
        let index = read_indexes(c, &db, table, Some(&obj.name), &cancel)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| not_found(obj))?;
        Ok(ObjectDef::Index {
            table: ObjectRef::table(Some(&db), None, table),
            index,
        })
    }
}

// ---- foreign keys -----------------------------------------------------------------------------

/// The foreign keys of `table` (or the one named `only`), columns in key order.
async fn read_foreign_keys(
    c: &dyn Connection,
    db: &str,
    table: &str,
    only: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Vec<ForeignKeyDef>> {
    let mut params = vec![text(db), text(table)];
    let filter = match only {
        Some(name) => {
            params.push(text(name));
            " AND rc.CONSTRAINT_NAME = ?"
        }
        None => "",
    };
    let sql = format!(
        "SELECT rc.CONSTRAINT_NAME, rc.UPDATE_RULE, rc.DELETE_RULE, k.COLUMN_NAME, \
         k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME \
         FROM information_schema.REFERENTIAL_CONSTRAINTS rc \
         JOIN information_schema.KEY_COLUMN_USAGE k \
           ON k.CONSTRAINT_SCHEMA = rc.CONSTRAINT_SCHEMA \
          AND k.CONSTRAINT_NAME = rc.CONSTRAINT_NAME \
          AND k.TABLE_NAME = rc.TABLE_NAME \
         WHERE rc.CONSTRAINT_SCHEMA = ? AND rc.TABLE_NAME = ?{filter} \
         ORDER BY rc.CONSTRAINT_NAME, k.ORDINAL_POSITION"
    );
    let rows = query(c, &sql, &params, cancel).await?;
    let mut out: Vec<ForeignKeyDef> = Vec::new();
    for row in &rows {
        let name = row.string(0);
        if let Some(last) = out
            .last_mut()
            .filter(|fk| fk.name.as_deref() == Some(name.as_str()))
        {
            last.columns.push(row.string(3));
            last.ref_columns.push(row.string(6));
            continue;
        }
        let ref_db = row.non_empty(4).unwrap_or_else(|| db.to_owned());
        out.push(ForeignKeyDef {
            name: Some(name),
            columns: vec![row.string(3)],
            ref_table: ObjectRef::table(Some(&ref_db), None, row.string(5)),
            ref_columns: vec![row.string(6)],
            on_delete: fk_action(&row.string(2)),
            on_update: fk_action(&row.string(1)),
            extras: Extras::new(),
        });
    }
    Ok(out)
}

fn fk_action(rule: &str) -> FkAction {
    match rule.to_ascii_uppercase().as_str() {
        "CASCADE" => FkAction::Cascade,
        "SET NULL" => FkAction::SetNull,
        "SET DEFAULT" => FkAction::SetDefault,
        "RESTRICT" => FkAction::Restrict,
        _ => FkAction::NoAction,
    }
}

/// Lists the foreign keys of a table and loads one.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ForeignKeyFetcher;

#[async_trait]
impl Fetcher for ForeignKeyFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        if parent.kind != ObjectKind::Table {
            return Ok(Vec::new());
        }
        let db = database_of(c, parent, &cancel).await?;
        let parent = scoped(parent, &db);
        let rows = query(
            c,
            "SELECT CONSTRAINT_NAME, REFERENCED_TABLE_NAME \
             FROM information_schema.REFERENTIAL_CONSTRAINTS \
             WHERE CONSTRAINT_SCHEMA = ? AND TABLE_NAME = ? ORDER BY CONSTRAINT_NAME",
            &[text(&db), text(&parent.name)],
            &cancel,
        )
        .await?;
        let system = is_system_database(&db);
        Ok(by_name(
            rows.iter()
                .map(|row| {
                    let mut s =
                        ObjectSummary::new(parent.child(ObjectKind::ForeignKey, row.string(0)));
                    s.system = system;
                    if let Some(target) = row.non_empty(1) {
                        s.extras.insert(props::REF_TABLE, target);
                    }
                    s
                })
                .collect(),
        ))
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        expect_kind(obj, ObjectKind::ForeignKey)?;
        let db = database_of(c, obj, &cancel).await?;
        let table = owner_name(obj)?;
        let foreign_key = read_foreign_keys(c, &db, table, Some(&obj.name), &cancel)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| not_found(obj))?;
        Ok(ObjectDef::ForeignKey {
            table: ObjectRef::table(Some(&db), None, table),
            foreign_key,
        })
    }
}

// ---- checks -----------------------------------------------------------------------------------

/// A check constraint as read, with "MariaDB created it for a `JSON` column".
#[derive(Debug, Clone)]
pub(crate) struct RawCheck {
    pub def: CheckDef,
    /// MariaDB's automatic `CHECK (json_valid(col))` of a `JSON` column (named after it).
    pub json_auto: bool,
}

/// `true` for MariaDB's automatic check of a `JSON` column: named after the column and exactly
/// `json_valid(`col`)`.
fn is_json_auto(name: &str, clause: &str) -> bool {
    let quoted = name.replace('`', "``");
    clause
        .trim()
        .eq_ignore_ascii_case(&format!("json_valid(`{quoted}`)"))
}

/// The check constraints of `table` (or the one named `only`), sorted by name. Empty on servers
/// without check constraints. MariaDB names checks per table; MySQL per database, so its
/// `CHECK_CONSTRAINTS` is joined to `TABLE_CONSTRAINTS` for the table.
async fn read_checks(
    c: &dyn Connection,
    db: &str,
    table: &str,
    only: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Vec<RawCheck>> {
    let v = c.server_version();
    let mariadb = is_mariadb(c);
    let mut params = vec![text(db), text(table)];
    let sql = if mariadb {
        if !v.at_least_patch(10, 2, 1) {
            return Ok(Vec::new());
        }
        let filter = match only {
            Some(name) => {
                params.push(text(name));
                " AND CONSTRAINT_NAME = ?"
            }
            None => "",
        };
        format!(
            "SELECT CONSTRAINT_NAME, CHECK_CLAUSE, NULL FROM information_schema.CHECK_CONSTRAINTS \
             WHERE CONSTRAINT_SCHEMA = ? AND TABLE_NAME = ?{filter} ORDER BY CONSTRAINT_NAME"
        )
    } else {
        if !v.at_least_patch(8, 0, 16) {
            return Ok(Vec::new());
        }
        let filter = match only {
            Some(name) => {
                params.push(text(name));
                " AND tc.CONSTRAINT_NAME = ?"
            }
            None => "",
        };
        format!(
            "SELECT tc.CONSTRAINT_NAME, cc.CHECK_CLAUSE, tc.ENFORCED \
             FROM information_schema.TABLE_CONSTRAINTS tc \
             JOIN information_schema.CHECK_CONSTRAINTS cc \
               ON cc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
              AND cc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME \
             WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? AND tc.CONSTRAINT_TYPE = 'CHECK'\
             {filter} ORDER BY tc.CONSTRAINT_NAME"
        )
    };
    let rows = query(c, &sql, &params, cancel).await?;
    Ok(rows
        .iter()
        .map(|row| {
            let name = row.string(0);
            let expression = row.string(1);
            let json_auto = mariadb && is_json_auto(&name, &expression);
            let mut def = CheckDef {
                name: Some(name),
                expression,
                extras: Extras::new(),
            };
            if row.text(2).is_some() {
                def.extras.insert(ENFORCED, row.flag(2));
            }
            RawCheck { def, json_auto }
        })
        .collect())
}

/// Lists the check constraints of a table and loads one. MariaDB's automatic `JSON` checks are
/// left out (the column is typed `Json` instead).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CheckFetcher;

#[async_trait]
impl Fetcher for CheckFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        if parent.kind != ObjectKind::Table {
            return Ok(Vec::new());
        }
        let db = database_of(c, parent, &cancel).await?;
        let parent = scoped(parent, &db);
        let system = is_system_database(&db);
        Ok(by_name(
            read_checks(c, &db, &parent.name, None, &cancel)
                .await?
                .into_iter()
                .filter(|k| !k.json_auto)
                .map(|k| {
                    let name = k.def.name.unwrap_or_default();
                    let mut s = ObjectSummary::new(parent.child(ObjectKind::Check, name));
                    s.system = system;
                    s.extras.insert(props::EXPRESSION, k.def.expression);
                    s
                })
                .collect(),
        ))
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        expect_kind(obj, ObjectKind::Check)?;
        let db = database_of(c, obj, &cancel).await?;
        let table = owner_name(obj)?;
        let check = read_checks(c, &db, table, Some(&obj.name), &cancel)
            .await?
            .into_iter()
            .find(|k| !k.json_auto)
            .ok_or_else(|| not_found(obj))?
            .def;
        Ok(ObjectDef::Check {
            table: ObjectRef::table(Some(&db), None, table),
            check,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_auto_checks() {
        assert!(is_json_auto("doc", "json_valid(`doc`)"));
        assert!(is_json_auto("doc", " JSON_VALID(`doc`) "));
        assert!(!is_json_auto("doc", "json_valid(`other`)"));
        assert!(!is_json_auto("doc", "char_length(`doc`) > 0"));
    }

    #[test]
    fn fk_rules() {
        assert_eq!(fk_action("CASCADE"), FkAction::Cascade);
        assert_eq!(fk_action("SET NULL"), FkAction::SetNull);
        assert_eq!(fk_action("SET DEFAULT"), FkAction::SetDefault);
        assert_eq!(fk_action("RESTRICT"), FkAction::Restrict);
        assert_eq!(fk_action("NO ACTION"), FkAction::NoAction);
    }
}
