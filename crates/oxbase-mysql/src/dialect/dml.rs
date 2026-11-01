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
//!   JSON, enums), so no placeholder needs a cast. The exception is `BIT(n)`, which reads a
//!   string as raw bytes (`'5'` would store `0x35`): a value for a `BIT` column (to write or in a
//!   key) is turned into an unsigned integer first, from a number, `true` / `false`, decimal text,
//!   `0x...` / `x'...'` hex or `0b...` / `b'...'` binary text; bytes are bound as they are (the
//!   bits themselves). Anything else is `InvalidInput`.

use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{
    BacktickQuoting, DmlPlan, DmlTarget, DmlWriter, write_column_list, write_key_predicate,
};
use oxbase_types::{
    BoundStatement, ColumnDef, ColumnValue, DataType, Error, Result, RowEdit, TableDef, Value,
    props,
};

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
                let values = &bindable_values(table, values)?;
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
                let changes = &bindable_values(table, changes)?;
                let key = &bindable_key(table, target, key)?;
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
                let key = &bindable_key(table, target, key)?;
                let mut w = SqlWriter::new(&q);
                w.push("DELETE FROM ").qualified(&table.obj).push(" WHERE ");
                write_key_predicate(&mut w, target.key, key)?;
                Ok(DmlPlan::write_only(w.finish()))
            }
        }
    }

    /// Only tables on a transactional storage engine (`InnoDB`, NDB, `RocksDB`, `TokuDB`)
    /// roll back; `MyISAM`, `MEMORY`, `ARCHIVE`, `Aria`, `CSV`, `MERGE` and other engines keep
    /// each write as it runs. A table loaded without its engine is taken as transactional (the
    /// server default, `InnoDB`).
    fn transactional(&self, table: &TableDef) -> bool {
        table
            .extras
            .get(&props::STORAGE_ENGINE)
            .and_then(Value::as_str)
            .is_none_or(is_transactional_engine)
    }
}

/// `true` for a storage engine whose writes a `ROLLBACK` undoes.
fn is_transactional_engine(engine: &str) -> bool {
    const TRANSACTIONAL: [&str; 6] = ["InnoDB", "XtraDB", "NDB", "ndbcluster", "RocksDB", "TokuDB"];
    let engine = engine.trim();
    TRANSACTIONAL.iter().any(|t| t.eq_ignore_ascii_case(engine))
}

/// `true` for a `BIT(n)` column.
fn is_bit(column: &ColumnDef) -> bool {
    let native = column.native_type.trim();
    let head = native.split(['(', ' ']).next().unwrap_or_default();
    matches!(&column.data_type, DataType::Other { name } if name.eq_ignore_ascii_case("bit"))
        || head.eq_ignore_ascii_case("bit")
}

/// The value bound for `column`: a `BIT` column's value as an unsigned integer (see the module
/// docs), anything else as it is.
fn bindable(column: Option<&ColumnDef>, value: &Value) -> Result<Value> {
    let Some(column) = column.filter(|c| is_bit(c)) else {
        return Ok(value.clone());
    };
    let bad = |shown: &str| {
        let shown: String = shown.chars().take(40).collect();
        Error::invalid_input(format!(
            "column '{}': '{shown}' is not a bit value (a number, 0x... hex or 0b... binary)",
            column.name
        ))
    };
    Ok(match value {
        Value::Null | Value::UInt(_) | Value::Bytes(_) => value.clone(),
        Value::Bool(b) => Value::UInt(u64::from(*b)),
        Value::Int(i) => Value::UInt(u64::try_from(*i).map_err(|_| bad(&i.to_string()))?),
        Value::Text(text) | Value::Decimal(text) => {
            Value::UInt(parse_bits(text).ok_or_else(|| bad(text))?)
        }
        other => return Err(bad(&format!("{other:?}"))),
    })
}

/// A bit value written as text: decimal, `0x..` / `x'..'` hex, `0b..` / `b'..'` binary,
/// `true` / `false`.
fn parse_bits(text: &str) -> Option<u64> {
    let t = text.trim();
    let lower = t.to_ascii_lowercase();
    let quoted = |prefix: &str| {
        lower
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix('\''))
            .map(str::to_owned)
    };
    let (digits, radix) = if let Some(hex) = lower.strip_prefix("0x") {
        (hex.to_owned(), 16)
    } else if let Some(hex) = quoted("x'") {
        (hex, 16)
    } else if let Some(bin) = lower.strip_prefix("0b") {
        (bin.to_owned(), 2)
    } else if let Some(bin) = quoted("b'") {
        (bin, 2)
    } else {
        match lower.as_str() {
            "true" => return Some(1),
            "false" => return Some(0),
            _ => (lower.clone(), 10),
        }
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    u64::from_str_radix(&digits, radix).ok()
}

/// `values` with each value [`bindable`] for its column.
fn bindable_values(table: &TableDef, values: &[ColumnValue]) -> Result<Vec<ColumnValue>> {
    values
        .iter()
        .map(|v| {
            Ok(ColumnValue {
                column: v.column.clone(),
                value: bindable(table.column(&v.column), &v.value)?,
            })
        })
        .collect()
}

/// The key values [`bindable`] for the key columns (in key order).
fn bindable_key(table: &TableDef, target: &DmlTarget<'_>, key: &[Value]) -> Result<Vec<Value>> {
    if key.len() != target.key.columns.len() {
        // `write_key_predicate` reports the mismatch.
        return Ok(key.to_vec());
    }
    target
        .key
        .columns
        .iter()
        .zip(key)
        .map(|(column, value)| bindable(table.column(column), value))
        .collect()
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
    use oxbase_types::{GeneratedColumn, KeyKind, ObjectRef, PrimaryKeyDef, RowKey, ServerVersion};

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
    fn bit_values_are_bound_as_numbers() {
        let mut t = table();
        let mut bits = ColumnDef::new("b", DataType::Other { name: "bit".into() });
        bits.native_type = "bit(8)".into();
        t.columns.push(bits);
        let v = ServerVersion::parse("8.4.6").unwrap();
        let by_bits = RowKey::new(pk().table, KeyKind::Unique, vec!["b".into()]);
        let plan = |key: &RowKey, edit: &RowEdit| {
            MySqlDmlWriter { mariadb: false }.plan(
                edit,
                &DmlTarget {
                    table: &t,
                    key,
                    version: &v,
                    read_back: &[],
                },
            )
        };
        for (text, want) in [
            ("5", 5),
            (" 255 ", 255),
            ("0x0F", 15),
            ("x'ff'", 255),
            ("0b101", 5),
            ("B'11'", 3),
            ("true", 1),
            ("18446744073709551615", u64::MAX),
        ] {
            let edit = RowEdit::Insert {
                values: vec![ColumnValue::new("b", text), ColumnValue::new("co`de", "5")],
            };
            let p = plan(&pk(), &edit).unwrap();
            assert_eq!(
                p.write.sql,
                "INSERT INTO `db`.`order` (`b`, `co``de`) VALUES (?, ?)"
            );
            // Only the BIT column's text becomes a number.
            assert_eq!(
                p.write.params,
                [Value::UInt(want), Value::Text("5".into())],
                "{text}"
            );
        }
        let edit = RowEdit::Update {
            key: vec![Value::Text("3".into())],
            changes: vec![
                ColumnValue::new("b", Value::Int(7)),
                ColumnValue::new("amount", Value::Null),
            ],
        };
        let p = plan(&by_bits, &edit).unwrap();
        assert_eq!(
            p.write.params,
            [Value::UInt(7), Value::Null, Value::UInt(3)]
        );
        let p = plan(
            &by_bits,
            &RowEdit::Delete {
                key: vec![Value::Bytes(vec![0x80])],
            },
        )
        .unwrap();
        assert_eq!(p.write.params, [Value::Bytes(vec![0x80])]);
        for bad in [
            Value::Text("abc".into()),
            Value::Text("0x".into()),
            Value::Text("1.5".into()),
            Value::Text("18446744073709551616".into()),
            Value::Int(-1),
            Value::Float(1.0),
        ] {
            let edit = RowEdit::Update {
                key: vec![Value::Int(1)],
                changes: vec![ColumnValue::new("b", bad.clone())],
            };
            assert!(
                matches!(plan(&pk(), &edit), Err(Error::InvalidInput { .. })),
                "{bad:?}"
            );
        }
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

    #[test]
    fn only_tables_on_transactional_engines_roll_back() {
        let writer = MySqlDmlWriter { mariadb: false };
        let mut t = table();
        // Loaded without its engine: the server default (InnoDB).
        assert!(writer.transactional(&t));
        for (engine, transactional) in [
            ("InnoDB", true),
            ("innodb", true),
            ("ndbcluster", true),
            ("RocksDB", true),
            ("MyISAM", false),
            ("MEMORY", false),
            ("ARCHIVE", false),
            ("Aria", false),
            ("CSV", false),
            ("MRG_MyISAM", false),
        ] {
            t.extras.insert(props::STORAGE_ENGINE, engine);
            assert_eq!(writer.transactional(&t), transactional, "{engine}");
            assert_eq!(
                MySqlDmlWriter { mariadb: true }.transactional(&t),
                transactional,
                "{engine}"
            );
        }
    }
}
