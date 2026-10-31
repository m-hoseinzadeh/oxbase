//! The MySQL / MariaDB [`DmlWriter`] (M3): grid edits as `INSERT` / `UPDATE` / `DELETE` with
//! bound `?` parameters.
//!
//! - Rows are matched by the key only (`WHERE k1 = ? AND k2 = ?`).
//! - Generated columns are never written (`InvalidInput`). An insert with no values is
//!   `INSERT INTO t () VALUES ()` (a row of defaults).
//! - **Read-back.** MariaDB 10.5+ returns an inserted row with `INSERT ... RETURNING`. Everything
//!   else (MySQL, older MariaDB, every update) reads the row back with
//!   `SELECT <read_back> FROM t WHERE <key>` in the same transaction: for an insert, a key value
//!   the edit did not give comes from `LAST_INSERT_ID()` when its column is `AUTO_INCREMENT`
//!   (no read-back when another key value is unknown); for an update, changed key values are
//!   used. Deletes have no read-back.
//! - An `UPDATE` reports *matched* rows because the driver sets `CLIENT_FOUND_ROWS`, so writing a
//!   value a row already has still counts 1 and the edit service's "exactly one row" check holds.
//! - Values are bound as they come: the server converts text to the column type (dates, decimals,
//!   JSON, enums), so no placeholder needs a cast.

use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{
    BacktickQuoting, DmlPlan, DmlTarget, DmlWriter, write_column_list, write_key_predicate,
};
use oxbase_types::{BoundStatement, ColumnValue, Error, Result, RowEdit, TableDef, Value};

use super::caps::Features;

/// The grid write-back of MySQL (`mariadb = false`) or MariaDB.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MySqlDmlWriter {
    pub mariadb: bool,
}

impl DmlWriter for MySqlDmlWriter {
    fn plan(&self, edit: &RowEdit, target: &DmlTarget<'_>) -> Result<DmlPlan> {
        target.key.validate(edit)?;
        let f = Features::new(self.mariadb, target.version);
        let q = BacktickQuoting;
        let table = target.table;
        for column in &target.key.columns {
            if table.column(column).is_none() {
                return Err(Error::invalid_input(format!(
                    "key column '{column}' is not a column of {}",
                    table.obj.name
                )));
            }
        }
        match edit {
            RowEdit::Insert { values } => {
                check_writable(table, values)?;
                let mut w = SqlWriter::new(&q);
                w.push("INSERT INTO ").qualified(&table.obj).push(" (");
                let columns: Vec<String> = values.iter().map(|v| v.column.clone()).collect();
                write_column_list(&mut w, &columns);
                w.push(") VALUES (");
                for (pos, v) in values.iter().enumerate() {
                    if pos > 0 {
                        w.push(", ");
                    }
                    w.param(v.value.clone());
                }
                w.push(")");
                if f.insert_returning && !target.read_back.is_empty() {
                    w.push(" RETURNING ");
                    write_column_list(&mut w, target.read_back);
                    return Ok(DmlPlan {
                        write: w.finish(),
                        returning: true,
                        read_back: None,
                    });
                }
                let read_back = inserted_row(target, values);
                Ok(DmlPlan {
                    write: w.finish(),
                    returning: false,
                    read_back,
                })
            }
            RowEdit::Update { key, changes } => {
                check_writable(table, changes)?;
                let mut w = SqlWriter::new(&q);
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
                // The row's key after the update: changed key values win.
                let new_key: Vec<Value> = target
                    .key
                    .columns
                    .iter()
                    .zip(key)
                    .map(|(column, old)| {
                        changes
                            .iter()
                            .rev()
                            .find(|c| c.column == *column)
                            .map_or_else(|| old.clone(), |c| c.value.clone())
                    })
                    .collect();
                let read_back = if target.read_back.is_empty() || new_key.iter().any(Value::is_null)
                {
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
            RowEdit::Delete { key } => {
                let mut w = SqlWriter::new(&q);
                w.push("DELETE FROM ").qualified(&table.obj).push(" WHERE ");
                write_key_predicate(&mut w, target.key, key)?;
                Ok(DmlPlan::write_only(w.finish()))
            }
        }
    }
}

/// Every column exists, is named once and is not generated.
fn check_writable(table: &TableDef, values: &[ColumnValue]) -> Result<()> {
    for (pos, v) in values.iter().enumerate() {
        let column = table.column(&v.column).ok_or_else(|| {
            Error::invalid_input(format!(
                "'{}' is not a column of {}",
                v.column, table.obj.name
            ))
        })?;
        if column.generated.is_some() {
            return Err(Error::invalid_input(format!(
                "column '{}' is generated and cannot be written",
                v.column
            )));
        }
        if values[..pos].iter().any(|other| other.column == v.column) {
            return Err(Error::invalid_input(format!(
                "column '{}' is given twice",
                v.column
            )));
        }
    }
    Ok(())
}

/// The read-back of an inserted row: `SELECT ... WHERE key`, each key value from the edit or,
/// for an `AUTO_INCREMENT` key column the edit left out (or set to NULL), `LAST_INSERT_ID()`.
/// `None` when nothing is read back or a key value is unknown.
fn inserted_row(target: &DmlTarget<'_>, values: &[ColumnValue]) -> Option<BoundStatement> {
    if target.read_back.is_empty() {
        return None;
    }
    let q = BacktickQuoting;
    let mut w = SqlWriter::new(&q);
    w.push("SELECT ");
    write_column_list(&mut w, target.read_back);
    w.push(" FROM ")
        .qualified(&target.table.obj)
        .push(" WHERE ");
    for (pos, column) in target.key.columns.iter().enumerate() {
        if pos > 0 {
            w.push(" AND ");
        }
        let given = values
            .iter()
            .find(|v| v.column == *column)
            .map(|v| &v.value)
            .filter(|v| !v.is_null());
        let auto = target
            .table
            .column(column)
            .is_some_and(|c| c.auto_increment);
        match given {
            Some(value) => {
                w.ident(column).push(" = ").param(value.clone());
            }
            None if auto => {
                w.ident(column).push(" = LAST_INSERT_ID()");
            }
            None => return None,
        }
    }
    Some(w.finish())
}

/// `SELECT <read_back> FROM t WHERE <key> = <values>`.
fn select_by_key(target: &DmlTarget<'_>, key: &[Value]) -> Result<BoundStatement> {
    let q = BacktickQuoting;
    let mut w = SqlWriter::new(&q);
    w.push("SELECT ");
    write_column_list(&mut w, target.read_back);
    w.push(" FROM ")
        .qualified(&target.table.obj)
        .push(" WHERE ");
    write_key_predicate(&mut w, target.key, key)?;
    Ok(w.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{
        ColumnDef, DataType, GeneratedColumn, KeyKind, ObjectRef, PrimaryKeyDef, RowKey,
        ServerVersion,
    };

    fn table() -> TableDef {
        let mut t = TableDef::new(ObjectRef::table(Some("db"), None, "order"));
        let mut id = ColumnDef::new("id", DataType::INT64).not_null();
        id.auto_increment = true;
        t.columns.push(id);
        t.columns.push(ColumnDef::new(
            "co`de",
            DataType::VarChar { length: Some(20) },
        ));
        t.columns.push(ColumnDef::new(
            "amount",
            DataType::Decimal {
                precision: Some(12),
                scale: Some(2),
            },
        ));
        let mut total = ColumnDef::new(
            "total",
            DataType::Decimal {
                precision: None,
                scale: None,
            },
        );
        total.generated = Some(GeneratedColumn {
            expression: "amount * 2".into(),
            stored: true,
        });
        t.columns.push(total);
        t.primary_key = Some(PrimaryKeyDef {
            name: None,
            columns: vec!["id".into()],
        });
        t
    }

    fn pk() -> RowKey {
        RowKey::new(
            ObjectRef::table(Some("db"), None, "order"),
            KeyKind::Primary,
            vec!["id".into()],
        )
    }

    fn plan(
        mariadb: bool,
        version: &str,
        key: &RowKey,
        edit: &RowEdit,
        read_back: &[String],
    ) -> Result<DmlPlan> {
        let t = table();
        let v = ServerVersion::parse(version).unwrap();
        MySqlDmlWriter { mariadb }.plan(
            edit,
            &DmlTarget {
                table: &t,
                key,
                version: &v,
                read_back,
            },
        )
    }

    fn all() -> Vec<String> {
        ["id", "co`de", "amount", "total"]
            .map(str::to_owned)
            .to_vec()
    }

    #[test]
    fn inserts() {
        let edit = RowEdit::Insert {
            values: vec![
                ColumnValue::new("co`de", "it's; DROP TABLE x"),
                ColumnValue::new("amount", Value::Decimal("1.50".into())),
            ],
        };
        let p = plan(false, "8.4.6", &pk(), &edit, &all()).unwrap();
        assert_eq!(
            p.write.sql,
            "INSERT INTO `db`.`order` (`co``de`, `amount`) VALUES (?, ?)"
        );
        assert_eq!(
            p.write.params,
            [
                Value::Text("it's; DROP TABLE x".into()),
                Value::Decimal("1.50".into())
            ]
        );
        assert!(!p.returning);
        let rb = p.read_back.unwrap();
        assert_eq!(
            rb.sql,
            "SELECT `id`, `co``de`, `amount`, `total` FROM `db`.`order` WHERE `id` = LAST_INSERT_ID()"
        );
        assert_eq!(rb.params, Vec::<Value>::new());

        // MariaDB 10.5+: RETURNING.
        let p = plan(true, "11.8.9-MariaDB", &pk(), &edit, &all()).unwrap();
        assert!(p.returning && p.read_back.is_none());
        assert!(
            p.write
                .sql
                .ends_with(" RETURNING `id`, `co``de`, `amount`, `total`")
        );
        // MariaDB 10.4: read-back like MySQL.
        let p = plan(true, "10.4.34-MariaDB", &pk(), &edit, &all()).unwrap();
        assert!(!p.returning && p.read_back.is_some());

        // A row of defaults.
        let p = plan(
            false,
            "8.4.6",
            &pk(),
            &RowEdit::Insert { values: vec![] },
            &[],
        )
        .unwrap();
        assert_eq!(p.write.sql, "INSERT INTO `db`.`order` () VALUES ()");
        assert!(p.read_back.is_none());

        // An explicit key value is bound in the read-back.
        let edit = RowEdit::Insert {
            values: vec![ColumnValue::new("id", 7)],
        };
        let rb = plan(false, "8.4.6", &pk(), &edit, &all())
            .unwrap()
            .read_back
            .unwrap();
        assert!(rb.sql.ends_with("WHERE `id` = ?"), "{}", rb.sql);
        assert_eq!(rb.params, [Value::Int(7)]);

        // A key that is neither given nor generated cannot be read back.
        let by_code = RowKey::new(pk().table, KeyKind::Unique, vec!["co`de".into()]);
        let edit = RowEdit::Insert {
            values: vec![ColumnValue::new("amount", 1)],
        };
        assert!(
            plan(false, "8.4.6", &by_code, &edit, &all())
                .unwrap()
                .read_back
                .is_none()
        );
    }

    #[test]
    fn updates_and_deletes() {
        let edit = RowEdit::Update {
            key: vec![Value::Int(3)],
            changes: vec![
                ColumnValue::new("co`de", Value::Text(String::new())),
                ColumnValue::new("amount", Value::Null),
            ],
        };
        let p = plan(true, "11.8.9-MariaDB", &pk(), &edit, &all()).unwrap();
        assert_eq!(
            p.write.sql,
            "UPDATE `db`.`order` SET `co``de` = ?, `amount` = ? WHERE `id` = ?"
        );
        // NULL and the empty string stay different values.
        assert_eq!(
            p.write.params,
            [Value::Text(String::new()), Value::Null, Value::Int(3)]
        );
        assert!(!p.returning);
        let rb = p.read_back.unwrap();
        assert_eq!(
            rb.sql,
            "SELECT `id`, `co``de`, `amount`, `total` FROM `db`.`order` WHERE `id` = ?"
        );
        assert_eq!(rb.params, [Value::Int(3)]);

        // A changed key is read back by its new value.
        let edit = RowEdit::Update {
            key: vec![Value::Int(3)],
            changes: vec![ColumnValue::new("id", 9)],
        };
        let rb = plan(false, "8.4.6", &pk(), &edit, &all())
            .unwrap()
            .read_back
            .unwrap();
        assert_eq!(rb.params, [Value::Int(9)]);

        let p = plan(
            false,
            "8.4.6",
            &pk(),
            &RowEdit::Delete {
                key: vec![3.into()],
            },
            &all(),
        )
        .unwrap();
        assert_eq!(p.write.sql, "DELETE FROM `db`.`order` WHERE `id` = ?");
        assert_eq!(p.write.params, [Value::Int(3)]);
        assert!(!p.returning && p.read_back.is_none());
    }

    #[test]
    fn refusals() {
        let generated = RowEdit::Update {
            key: vec![1.into()],
            changes: vec![ColumnValue::new("total", 1)],
        };
        assert!(matches!(
            plan(false, "8.4.6", &pk(), &generated, &[]),
            Err(Error::InvalidInput { .. })
        ));
        let unknown = RowEdit::Insert {
            values: vec![ColumnValue::new("nope", 1)],
        };
        assert!(plan(false, "8.4.6", &pk(), &unknown, &[]).is_err());
        let twice = RowEdit::Insert {
            values: vec![ColumnValue::new("amount", 1), ColumnValue::new("amount", 2)],
        };
        assert!(plan(false, "8.4.6", &pk(), &twice, &[]).is_err());
        let null_key = RowEdit::Delete {
            key: vec![Value::Null],
        };
        assert!(plan(false, "8.4.6", &pk(), &null_key, &[]).is_err());
        let bad_key = RowKey::new(pk().table, KeyKind::Unique, vec!["nope".into()]);
        assert!(
            plan(
                false,
                "8.4.6",
                &bad_key,
                &RowEdit::Delete {
                    key: vec![1.into()]
                },
                &[]
            )
            .is_err()
        );
    }
}
