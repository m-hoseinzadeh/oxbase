//! The SQLite [`DmlWriter`] (M3): grid edits as bound `INSERT`, `UPDATE` and `DELETE`.
//!
//! - Identifiers are quoted with [`AnsiQuoting`]; every value is a `?` parameter.
//! - `UPDATE` and `DELETE` match the row by its key only (`WHERE k = ? AND ...`).
//! - Generated columns are never written (`InvalidInput`). An insert with no values is
//!   `INSERT INTO t DEFAULT VALUES`.
//! - **Read-back.** SQLite 3.35+ returns the row from the write itself (`RETURNING <columns>`).
//!   Older libraries read it back with a `SELECT` in the same transaction: after an insert by
//!   `rowid = last_insert_rowid()` (rowid tables) or by the key values the insert gave (`WITHOUT
//!   ROWID` tables); after an update by the key, with any new key values.
//! - **`rowid` as the edit key.** A rowid table (the default kind of table) that has no primary
//!   key and no usable unique key can still be edited by its `rowid`: a [`RowKey`] whose only
//!   column is `rowid` (also `_rowid_` or `oid`), when the table has no real column of that name.
//!   The rowid is stable for a row as long as the table is not rebuilt or vacuumed without an
//!   `INTEGER PRIMARY KEY`; the grid must have read it in the same result (`SELECT rowid, *`).
//!   `WITHOUT ROWID` tables have no rowid and are edited by their primary key only.

use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{
    AnsiQuoting, DmlPlan, DmlTarget, DmlWriter, write_column_list, write_key_predicate,
};
use oxbase_types::{
    BoundStatement, ColumnDef, ColumnValue, Error, Result, RowEdit, RowKey, TableDef, Value, props,
};

/// The names under which SQLite exposes the rowid of a rowid table, unless a real column has
/// the name.
const ROWID_NAMES: [&str; 3] = ["rowid", "_rowid_", "oid"];

/// The SQLite grid writer. See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteDmlWriter;

fn is_without_rowid(t: &TableDef) -> bool {
    matches!(t.extras.get(&props::WITHOUT_ROWID), Some(Value::Bool(true)))
}

/// The column `name` of `t` (exact name first, then ASCII case-insensitive, as SQLite resolves
/// names).
fn find_column<'t>(t: &'t TableDef, name: &str) -> Option<&'t ColumnDef> {
    t.columns
        .iter()
        .find(|c| c.name == name)
        .or_else(|| t.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name)))
}

/// `true` when `name` reaches the rowid of `t`: a rowid name no real column shadows, on a
/// rowid table.
fn is_rowid(t: &TableDef, name: &str) -> bool {
    !is_without_rowid(t)
        && ROWID_NAMES.iter().any(|r| r.eq_ignore_ascii_case(name))
        && find_column(t, name).is_none()
}

/// The first rowid name no real column of `t` shadows (`None` on `WITHOUT ROWID` tables or when
/// every name is taken).
fn rowid_name(t: &TableDef) -> Option<&'static str> {
    ROWID_NAMES.into_iter().find(|name| is_rowid(t, name))
}

/// Checks that `name` is a column of `t` that an edit may write.
fn writable(t: &TableDef, name: &str) -> Result<()> {
    let column = find_column(t, name).ok_or_else(|| {
        Error::invalid_input(format!("{} has no column '{name}'", t.obj.display_name()))
    })?;
    if column.generated.is_some() {
        return Err(Error::invalid_input(format!(
            "column '{}' is generated and cannot be written",
            column.name
        )));
    }
    Ok(())
}

/// Checks the values of an insert or the changes of an update: known, writable, each column
/// once.
fn check_values(t: &TableDef, values: &[ColumnValue]) -> Result<()> {
    for (pos, value) in values.iter().enumerate() {
        writable(t, &value.column)?;
        if values[..pos]
            .iter()
            .any(|earlier| earlier.column.eq_ignore_ascii_case(&value.column))
        {
            return Err(Error::invalid_input(format!(
                "column '{}' is given twice",
                value.column
            )));
        }
    }
    Ok(())
}

/// Checks that every key column is a column of `t`, or the rowid of a rowid table.
fn check_key(t: &TableDef, key: &RowKey) -> Result<()> {
    for column in &key.columns {
        if find_column(t, column).is_some() || is_rowid(t, column) {
            continue;
        }
        let why =
            if is_without_rowid(t) && ROWID_NAMES.iter().any(|r| r.eq_ignore_ascii_case(column)) {
                " (a WITHOUT ROWID table has no rowid; edit it by its primary key)"
            } else {
                ""
            };
        return Err(Error::invalid_input(format!(
            "{} has no key column '{column}'{why}",
            t.obj.display_name()
        )));
    }
    Ok(())
}

/// `SELECT <read_back> FROM t WHERE <key>` for `key` values.
fn select_by_key(target: &DmlTarget<'_>, key: &[Value]) -> Result<BoundStatement> {
    let mut w = SqlWriter::new(&AnsiQuoting);
    w.push("SELECT ");
    write_column_list(&mut w, target.read_back);
    w.push(" FROM ")
        .qualified(&target.table.obj)
        .push(" WHERE ");
    write_key_predicate(&mut w, target.key, key)?;
    Ok(w.finish())
}

/// `RETURNING <read_back>` when the library has it and something is to be read back.
fn returning(w: &mut SqlWriter<'_>, target: &DmlTarget<'_>) -> bool {
    if target.read_back.is_empty() || !target.version.at_least(3, 35) {
        return false;
    }
    w.push(" RETURNING ");
    write_column_list(w, target.read_back);
    true
}

fn insert(values: &[ColumnValue], target: &DmlTarget<'_>) -> Result<DmlPlan> {
    let table = target.table;
    check_values(table, values)?;
    let mut w = SqlWriter::new(&AnsiQuoting);
    w.push("INSERT INTO ").qualified(&table.obj);
    if values.is_empty() {
        w.push(" DEFAULT VALUES");
    } else {
        w.push(" (");
        for (pos, value) in values.iter().enumerate() {
            if pos > 0 {
                w.push(", ");
            }
            w.ident(&value.column);
        }
        w.push(") VALUES (");
        for (pos, value) in values.iter().enumerate() {
            if pos > 0 {
                w.push(", ");
            }
            w.param(value.value.clone());
        }
        w.push(")");
    }
    if returning(&mut w, target) {
        return Ok(DmlPlan {
            write: w.finish(),
            returning: true,
            read_back: None,
        });
    }
    let read_back = if target.read_back.is_empty() {
        None
    } else if let Some(rowid) = rowid_name(table) {
        let mut r = SqlWriter::new(&AnsiQuoting);
        r.push("SELECT ");
        write_column_list(&mut r, target.read_back);
        r.push(" FROM ")
            .qualified(&table.obj)
            .push(" WHERE ")
            .push(rowid)
            .push(" = last_insert_rowid()");
        Some(r.finish())
    } else {
        // A WITHOUT ROWID table: found again only by key values the insert gave.
        let given: Option<Vec<Value>> = target
            .key
            .columns
            .iter()
            .map(|k| {
                values
                    .iter()
                    .find(|v| v.column.eq_ignore_ascii_case(k))
                    .map(|v| v.value.clone())
                    .filter(|v| !v.is_null())
            })
            .collect();
        match given {
            Some(key) => Some(select_by_key(target, &key)?),
            None => None,
        }
    };
    Ok(DmlPlan {
        write: w.finish(),
        returning: false,
        read_back,
    })
}

fn update(key: &[Value], changes: &[ColumnValue], target: &DmlTarget<'_>) -> Result<DmlPlan> {
    let table = target.table;
    check_values(table, changes)?;
    let mut w = SqlWriter::new(&AnsiQuoting);
    w.push("UPDATE ").qualified(&table.obj).push(" SET ");
    for (pos, change) in changes.iter().enumerate() {
        if pos > 0 {
            w.push(", ");
        }
        w.ident(&change.column)
            .push(" = ")
            .param(change.value.clone());
    }
    w.push(" WHERE ");
    write_key_predicate(&mut w, target.key, key)?;
    if returning(&mut w, target) {
        return Ok(DmlPlan {
            write: w.finish(),
            returning: true,
            read_back: None,
        });
    }
    let new_key: Vec<Value> = target
        .key
        .columns
        .iter()
        .zip(key)
        .map(|(column, old)| {
            changes
                .iter()
                .find(|c| c.column.eq_ignore_ascii_case(column))
                .map_or_else(|| old.clone(), |c| c.value.clone())
        })
        .collect();
    let read_back = if target.read_back.is_empty() || new_key.iter().any(Value::is_null) {
        None
    } else {
        Some(select_by_key(target, &new_key)?)
    };
    Ok(DmlPlan {
        write: w.finish(),
        returning: false,
        read_back,
    })
}

fn delete(key: &[Value], target: &DmlTarget<'_>) -> Result<DmlPlan> {
    let mut w = SqlWriter::new(&AnsiQuoting);
    w.push("DELETE FROM ")
        .qualified(&target.table.obj)
        .push(" WHERE ");
    write_key_predicate(&mut w, target.key, key)?;
    Ok(DmlPlan::write_only(w.finish()))
}

impl DmlWriter for SqliteDmlWriter {
    fn plan(&self, edit: &RowEdit, target: &DmlTarget<'_>) -> Result<DmlPlan> {
        target.key.validate(edit)?;
        check_key(target.table, target.key)?;
        match edit {
            RowEdit::Insert { values } => insert(values, target),
            RowEdit::Update { key, changes } => update(key, changes, target),
            RowEdit::Delete { key } => delete(key, target),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{
        DataType, GeneratedColumn, KeyKind, ObjectRef, PrimaryKeyDef, ServerVersion,
    };

    fn table() -> TableDef {
        let mut t = TableDef::new(ObjectRef::table(Some("main"), None, "m3 \"edit\""));
        let mut id = ColumnDef::new("id", DataType::INT64).not_null();
        id.native_type = "INTEGER".into();
        id.auto_increment = true;
        t.columns.push(id);
        t.columns.push(ColumnDef::new("select", DataType::Text));
        t.columns.push(ColumnDef::new("名前", DataType::Text));
        let mut total = ColumnDef::new("total", DataType::FLOAT64);
        total.generated = Some(GeneratedColumn {
            expression: "id * 2".into(),
            stored: true,
        });
        t.columns.push(total);
        t.primary_key = Some(PrimaryKeyDef {
            name: None,
            columns: vec!["id".into()],
        });
        t
    }

    fn key(columns: &[&str]) -> RowKey {
        RowKey::new(
            ObjectRef::table(Some("main"), None, "m3 \"edit\""),
            KeyKind::Primary,
            columns.iter().map(|c| (*c).to_owned()).collect(),
        )
    }

    fn plan(
        edit: &RowEdit,
        t: &TableDef,
        k: &RowKey,
        version: &str,
        read_back: &[&str],
    ) -> Result<DmlPlan> {
        let version: ServerVersion = version.parse().unwrap();
        let read_back: Vec<String> = read_back.iter().map(|c| (*c).to_owned()).collect();
        let target = DmlTarget {
            table: t,
            key: k,
            version: &version,
            read_back: &read_back,
        };
        SqliteDmlWriter.plan(edit, &target)
    }

    const TABLE: &str = "\"main\".\"m3 \"\"edit\"\"\"";

    #[test]
    fn writes_bind_every_value_and_return_the_row() {
        let t = table();
        let k = key(&["id"]);
        let ins = RowEdit::Insert {
            values: vec![
                ColumnValue::new("select", "it's"),
                ColumnValue::new("名前", Value::Null),
            ],
        };
        let p = plan(&ins, &t, &k, "3.51.0", &["id", "select", "total"]).unwrap();
        assert_eq!(
            p.write.sql,
            format!(
                "INSERT INTO {TABLE} (\"select\", \"名前\") VALUES (?, ?) RETURNING \"id\", \
                 \"select\", \"total\""
            )
        );
        assert_eq!(p.write.params, [Value::Text("it's".into()), Value::Null]);
        assert!(p.returning && p.read_back.is_none());

        let upd = RowEdit::Update {
            key: vec![Value::Int(7)],
            changes: vec![ColumnValue::new("select", "")],
        };
        let p = plan(&upd, &t, &k, "3.51.0", &["id"]).unwrap();
        assert_eq!(
            p.write.sql,
            format!("UPDATE {TABLE} SET \"select\" = ? WHERE \"id\" = ? RETURNING \"id\"")
        );
        assert_eq!(p.write.params, [Value::Text(String::new()), Value::Int(7)]);

        let del = RowEdit::Delete {
            key: vec![Value::Int(7)],
        };
        let p = plan(&del, &t, &k, "3.51.0", &["id"]).unwrap();
        assert_eq!(p.write.sql, format!("DELETE FROM {TABLE} WHERE \"id\" = ?"));
        assert!(!p.returning && p.read_back.is_none());

        let defaults = RowEdit::Insert { values: vec![] };
        let p = plan(&defaults, &t, &k, "3.51.0", &[]).unwrap();
        assert_eq!(p.write.sql, format!("INSERT INTO {TABLE} DEFAULT VALUES"));
        assert_eq!(p.write.params, []);
        assert!(!p.returning && p.read_back.is_none());
    }

    #[test]
    fn older_libraries_read_back_with_a_select() {
        let t = table();
        let k = key(&["id"]);
        let ins = RowEdit::Insert {
            values: vec![ColumnValue::new("select", "x")],
        };
        let p = plan(&ins, &t, &k, "3.34.1", &["id", "total"]).unwrap();
        assert_eq!(
            p.write.sql,
            format!("INSERT INTO {TABLE} (\"select\") VALUES (?)")
        );
        assert!(!p.returning);
        assert_eq!(
            p.read_back.unwrap().sql,
            format!("SELECT \"id\", \"total\" FROM {TABLE} WHERE rowid = last_insert_rowid()")
        );

        // An update that changes the key reads back by the new key.
        let upd = RowEdit::Update {
            key: vec![Value::Int(1)],
            changes: vec![ColumnValue::new("id", 5), ColumnValue::new("select", "y")],
        };
        let p = plan(&upd, &t, &k, "3.34.1", &["id"]).unwrap();
        let back = p.read_back.unwrap();
        assert_eq!(
            back.sql,
            format!("SELECT \"id\" FROM {TABLE} WHERE \"id\" = ?")
        );
        assert_eq!(back.params, [Value::Int(5)]);

        // A WITHOUT ROWID table is found again by the key values the insert gave.
        let mut wr = table();
        wr.extras.insert(props::WITHOUT_ROWID, true);
        let given = RowEdit::Insert {
            values: vec![ColumnValue::new("id", 9)],
        };
        let p = plan(&given, &wr, &k, "3.34.1", &["id"]).unwrap();
        assert_eq!(p.read_back.unwrap().params, [Value::Int(9)]);
        let p = plan(&ins, &wr, &k, "3.34.1", &["id"]).unwrap();
        assert!(p.read_back.is_none());
    }

    #[test]
    fn rowid_keys_and_refusals() {
        let mut t = table();
        t.primary_key = None;
        let by_rowid = key(&["rowid"]);
        let del = RowEdit::Delete {
            key: vec![Value::Int(3)],
        };
        let p = plan(&del, &t, &by_rowid, "3.51.0", &[]).unwrap();
        assert_eq!(
            p.write.sql,
            format!("DELETE FROM {TABLE} WHERE \"rowid\" = ?")
        );

        let mut wr = t.clone();
        wr.extras.insert(props::WITHOUT_ROWID, true);
        let err = plan(&del, &wr, &by_rowid, "3.51.0", &[]).unwrap_err();
        assert!(err.to_string().contains("WITHOUT ROWID"), "{err}");

        let k = key(&["id"]);
        let generated = RowEdit::Update {
            key: vec![Value::Int(1)],
            changes: vec![ColumnValue::new("total", 1)],
        };
        let err = plan(&generated, &t, &k, "3.51.0", &[]).unwrap_err();
        assert!(err.to_string().contains("generated"), "{err}");
        let unknown = RowEdit::Insert {
            values: vec![ColumnValue::new("nope", 1)],
        };
        assert!(plan(&unknown, &t, &k, "3.51.0", &[]).is_err());
        let twice = RowEdit::Insert {
            values: vec![ColumnValue::new("select", 1), ColumnValue::new("SELECT", 2)],
        };
        assert!(plan(&twice, &t, &k, "3.51.0", &[]).is_err());
        let null_key = RowEdit::Delete {
            key: vec![Value::Null],
        };
        assert!(plan(&null_key, &t, &k, "3.51.0", &[]).is_err());
        assert!(plan(&del, &t, &key(&["missing"]), "3.51.0", &[]).is_err());
    }
}
