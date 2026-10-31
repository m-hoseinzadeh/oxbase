//! The PostgreSQL [`DmlWriter`] (M3): grid edits as `INSERT` / `UPDATE` / `DELETE` with bound
//! parameters (`$1`, `$2`, ...), rows matched by their key only, and `RETURNING` for the
//! read-back of inserts and updates (so an update that changes key values returns the row under
//! its new key, and generated and identity values come back with an insert).
//!
//! Values the driver cannot bind for the column's type (arrays, intervals, enums, domains and
//! other types without a neutral equivalent) arrive as text; their placeholder is cast in SQL:
//! `CAST($n::text AS <native type>)`.

#![allow(
    clippy::many_single_char_names,
    reason = "plans, keys and versions read best with short names in the tests"
)]

use std::collections::HashSet;

use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{DmlPlan, DmlTarget, DmlWriter, PgQuoting, write_column_list};
use oxbase_types::{
    ColumnDef, ColumnValue, DataType, Error, Result, RowEdit, RowKey, TableDef, Value,
};

use super::ddl::{checked_type, neutral_type};

/// The PostgreSQL grid writer.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PgDmlWriter;

fn column<'t>(table: &'t TableDef, name: &str) -> Result<&'t ColumnDef> {
    table.column(name).ok_or_else(|| {
        Error::invalid_input(format!(
            "{} has no column '{name}'",
            table.obj.display_name()
        ))
    })
}

/// The column `name`, refusing columns the server computes.
fn writable<'t>(table: &'t TableDef, name: &str) -> Result<&'t ColumnDef> {
    let col = column(table, name)?;
    if col.generated.is_some() {
        return Err(Error::invalid_input(format!(
            "column '{name}' is generated and cannot be written"
        )));
    }
    if col.identity.as_ref().is_some_and(|id| id.always) {
        return Err(Error::invalid_input(format!(
            "column '{name}' is a GENERATED ALWAYS identity column and cannot be written"
        )));
    }
    Ok(col)
}

/// `true` when a text value for `col` must be cast in SQL because the driver has no binary
/// conversion for the column's type.
fn needs_cast(col: &ColumnDef) -> bool {
    matches!(
        col.data_type,
        DataType::Array { .. }
            | DataType::Interval
            | DataType::Enum { .. }
            | DataType::Other { .. }
    )
}

/// Appends the placeholder for `value` in column `col` and binds it.
fn bind(w: &mut SqlWriter<'_>, col: &ColumnDef, value: Value) -> Result<()> {
    if matches!(value, Value::Text(_)) && needs_cast(col) {
        let ty = if col.native_type.trim().is_empty() {
            neutral_type(&col.data_type)?
        } else {
            checked_type(&col.name, &col.native_type)?.to_owned()
        };
        w.push("CAST(")
            .param(value)
            .push("::text AS ")
            .push(&ty)
            .push(")");
    } else {
        w.param(value);
    }
    Ok(())
}

fn no_duplicates(values: &[ColumnValue]) -> Result<()> {
    let mut seen = HashSet::new();
    for cv in values {
        if !seen.insert(cv.column.as_str()) {
            return Err(Error::invalid_input(format!(
                "column '{}' is written twice",
                cv.column
            )));
        }
    }
    Ok(())
}

/// `k1 = $n AND k2 = $m` for the key values (like `oxbase_dialect::write_key_predicate`, with
/// the cast of [`bind`] for key columns of types the driver cannot bind).
fn key_predicate(
    w: &mut SqlWriter<'_>,
    table: &TableDef,
    key: &RowKey,
    values: &[Value],
) -> Result<()> {
    if key.columns.is_empty() || values.len() != key.columns.len() {
        return Err(Error::invalid_input(format!(
            "{} key values for {} key columns",
            values.len(),
            key.columns.len()
        )));
    }
    for (pos, (name, value)) in key.bind(values).enumerate() {
        if value.is_null() {
            return Err(Error::invalid_input(format!("key column '{name}' is NULL")));
        }
        if pos > 0 {
            w.push(" AND ");
        }
        let col = column(table, name)?;
        w.ident(name).push(" = ");
        bind(w, col, value.clone())?;
    }
    Ok(())
}

/// Finishes an insert or update: `RETURNING <read_back>` when there is anything to read back.
fn finish(mut w: SqlWriter<'_>, read_back: &[String]) -> DmlPlan {
    if read_back.is_empty() {
        return DmlPlan::write_only(w.finish());
    }
    w.push(" RETURNING ");
    write_column_list(&mut w, read_back);
    DmlPlan {
        write: w.finish(),
        returning: true,
        read_back: None,
    }
}

impl DmlWriter for PgDmlWriter {
    fn plan(&self, edit: &RowEdit, target: &DmlTarget<'_>) -> Result<DmlPlan> {
        target.key.validate(edit)?;
        let table = target.table;
        for name in target.read_back {
            column(table, name)?;
        }
        let mut w = SqlWriter::new(&PgQuoting);
        match edit {
            RowEdit::Insert { values } => {
                no_duplicates(values)?;
                w.push("INSERT INTO ").qualified(&table.obj);
                if values.is_empty() {
                    w.push(" DEFAULT VALUES");
                } else {
                    let cols = values
                        .iter()
                        .map(|cv| writable(table, &cv.column))
                        .collect::<Result<Vec<_>>>()?;
                    w.push(" (");
                    for (pos, col) in cols.iter().enumerate() {
                        if pos > 0 {
                            w.push(", ");
                        }
                        w.ident(&col.name);
                    }
                    w.push(") VALUES (");
                    for (pos, (col, cv)) in cols.iter().zip(values).enumerate() {
                        if pos > 0 {
                            w.push(", ");
                        }
                        bind(&mut w, col, cv.value.clone())?;
                    }
                    w.push(")");
                }
                Ok(finish(w, target.read_back))
            }
            RowEdit::Update { key, changes } => {
                no_duplicates(changes)?;
                w.push("UPDATE ").qualified(&table.obj).push(" SET ");
                for (pos, cv) in changes.iter().enumerate() {
                    let col = writable(table, &cv.column)?;
                    if pos > 0 {
                        w.push(", ");
                    }
                    w.ident(&col.name).push(" = ");
                    bind(&mut w, col, cv.value.clone())?;
                }
                w.push(" WHERE ");
                key_predicate(&mut w, table, target.key, key)?;
                Ok(finish(w, target.read_back))
            }
            RowEdit::Delete { key } => {
                w.push("DELETE FROM ").qualified(&table.obj).push(" WHERE ");
                key_predicate(&mut w, table, target.key, key)?;
                Ok(DmlPlan::write_only(w.finish()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{GeneratedColumn, IdentityDef, KeyKind, ObjectRef, ServerVersion};

    fn table() -> TableDef {
        let mut t = TableDef::new(ObjectRef::table(
            Some("oxbase_test"),
            Some("m3"),
            "m3_edit_pk",
        ));
        let mut id = ColumnDef::new("id", DataType::INT32).not_null();
        id.native_type = "integer".into();
        id.auto_increment = true;
        id.identity = Some(IdentityDef {
            always: false,
            start: Some(1),
            increment: Some(1),
        });
        t.columns.push(id);
        let mut code = ColumnDef::new("code", DataType::VarChar { length: Some(20) }).not_null();
        code.native_type = "character varying(20)".into();
        t.columns.push(code);
        let mut name = ColumnDef::new("na\"me", DataType::Text);
        name.native_type = "text".into();
        t.columns.push(name);
        let mut tags = ColumnDef::new(
            "tags",
            DataType::Array {
                element: Box::new(DataType::Text),
            },
        );
        tags.native_type = "text[]".into();
        t.columns.push(tags);
        let mut mood = ColumnDef::new(
            "mood",
            DataType::Other {
                name: "m3.\"Mood\"".into(),
            },
        );
        mood.native_type = "m3.\"Mood\"".into();
        t.columns.push(mood);
        let mut total = ColumnDef::new("total", DataType::INT64);
        total.native_type = "bigint".into();
        total.generated = Some(GeneratedColumn {
            expression: "id * 2".into(),
            stored: true,
        });
        t.columns.push(total);
        t
    }

    fn key(columns: &[&str]) -> RowKey {
        RowKey::new(
            ObjectRef::table(Some("oxbase_test"), Some("m3"), "m3_edit_pk"),
            KeyKind::Primary,
            columns.iter().map(|c| (*c).to_owned()).collect(),
        )
    }

    fn plan(edit: &RowEdit, key_columns: &[&str], read_back: &[&str]) -> Result<DmlPlan> {
        let t = table();
        let k = key(key_columns);
        let v = ServerVersion::new(18, 6, 0);
        let read_back: Vec<String> = read_back.iter().map(|c| (*c).to_owned()).collect();
        PgDmlWriter.plan(
            edit,
            &DmlTarget {
                table: &t,
                key: &k,
                version: &v,
                read_back: &read_back,
            },
        )
    }

    #[test]
    fn inserts_bind_every_value_and_return_the_row() {
        let edit = RowEdit::Insert {
            values: vec![
                ColumnValue::new("code", "it's"),
                ColumnValue::new("na\"me", Value::Null),
                ColumnValue::new("tags", "{a,b}"),
                ColumnValue::new("mood", "happy"),
            ],
        };
        let p = plan(&edit, &["id"], &["id", "code", "total"]).unwrap();
        assert_eq!(
            p.write.sql,
            "INSERT INTO \"m3\".\"m3_edit_pk\" (\"code\", \"na\"\"me\", \"tags\", \"mood\") \
             VALUES ($1, $2, CAST($3::text AS text[]), CAST($4::text AS m3.\"Mood\")) \
             RETURNING \"id\", \"code\", \"total\""
        );
        assert_eq!(
            p.write.params,
            [
                Value::Text("it's".into()),
                Value::Null,
                Value::Text("{a,b}".into()),
                Value::Text("happy".into())
            ]
        );
        assert!(p.returning);
        assert!(p.read_back.is_none());

        // A row of defaults; no read-back wanted.
        let p = plan(&RowEdit::Insert { values: vec![] }, &["id"], &[]).unwrap();
        assert_eq!(
            p.write.sql,
            "INSERT INTO \"m3\".\"m3_edit_pk\" DEFAULT VALUES"
        );
        assert_eq!(p.write.params, Vec::<Value>::new());
        assert!(!p.returning);
        let p = plan(&RowEdit::Insert { values: vec![] }, &["id"], &["id"]).unwrap();
        assert_eq!(
            p.write.sql,
            "INSERT INTO \"m3\".\"m3_edit_pk\" DEFAULT VALUES RETURNING \"id\""
        );
    }

    #[test]
    fn updates_and_deletes_match_by_key_only() {
        let edit = RowEdit::Update {
            key: vec![Value::Int(7), Value::Text("x".into())],
            changes: vec![
                ColumnValue::new("na\"me", ""),
                ColumnValue::new("code", "y"),
            ],
        };
        let p = plan(&edit, &["id", "code"], &["id", "code"]).unwrap();
        assert_eq!(
            p.write.sql,
            "UPDATE \"m3\".\"m3_edit_pk\" SET \"na\"\"me\" = $1, \"code\" = $2 \
             WHERE \"id\" = $3 AND \"code\" = $4 RETURNING \"id\", \"code\""
        );
        assert_eq!(
            p.write.params,
            [
                Value::Text(String::new()),
                Value::Text("y".into()),
                Value::Int(7),
                Value::Text("x".into())
            ]
        );
        assert!(p.returning);

        let p = plan(
            &RowEdit::Delete {
                key: vec![Value::Int(7)],
            },
            &["id"],
            &["id"],
        )
        .unwrap();
        assert_eq!(
            p.write.sql,
            "DELETE FROM \"m3\".\"m3_edit_pk\" WHERE \"id\" = $1"
        );
        assert_eq!(p.write.params, [Value::Int(7)]);
        assert!(!p.returning && p.read_back.is_none());

        // An enum key value is cast like any other value.
        let p = plan(
            &RowEdit::Delete {
                key: vec![Value::Text("sad".into())],
            },
            &["mood"],
            &[],
        )
        .unwrap();
        assert_eq!(
            p.write.sql,
            "DELETE FROM \"m3\".\"m3_edit_pk\" WHERE \"mood\" = CAST($1::text AS m3.\"Mood\")"
        );
    }

    #[test]
    fn refuses_generated_unknown_and_duplicate_columns() {
        let err = |edit: RowEdit| plan(&edit, &["id"], &["id"]).unwrap_err().to_string();
        assert!(
            err(RowEdit::Insert {
                values: vec![ColumnValue::new("total", 1)]
            })
            .contains("generated")
        );
        assert!(
            err(RowEdit::Update {
                key: vec![Value::Int(1)],
                changes: vec![ColumnValue::new("nope", 1)]
            })
            .contains("no column 'nope'")
        );
        assert!(
            err(RowEdit::Update {
                key: vec![Value::Int(1)],
                changes: vec![ColumnValue::new("code", "a"), ColumnValue::new("code", "b")]
            })
            .contains("twice")
        );
        assert!(
            err(RowEdit::Delete {
                key: vec![Value::Null]
            })
            .contains("NULL")
        );
        assert!(
            plan(&RowEdit::Insert { values: vec![] }, &["id"], &["nope"])
                .unwrap_err()
                .to_string()
                .contains("nope")
        );

        // GENERATED ALWAYS identity columns are never written; BY DEFAULT ones may be.
        let mut t = table();
        if let Some(id) = t.columns[0].identity.as_mut() {
            id.always = true;
        }
        let k = key(&["id"]);
        let v = ServerVersion::new(18, 6, 0);
        let target = DmlTarget {
            table: &t,
            key: &k,
            version: &v,
            read_back: &[],
        };
        let insert = RowEdit::Insert {
            values: vec![ColumnValue::new("id", 5), ColumnValue::new("code", "z")],
        };
        let e = PgDmlWriter.plan(&insert, &target).unwrap_err();
        assert!(e.to_string().contains("GENERATED ALWAYS"), "{e}");
        assert!(plan(&insert, &["id"], &[]).is_ok());

        // A native type that hides a clause is refused before it reaches SQL.
        let mut bad = table();
        bad.columns[4].native_type = "text) ; DROP TABLE x; --".into();
        let edit = RowEdit::Insert {
            values: vec![ColumnValue::new("mood", "x")],
        };
        let target = DmlTarget {
            table: &bad,
            key: &k,
            version: &v,
            read_back: &[],
        };
        assert!(PgDmlWriter.plan(&edit, &target).is_err());
    }
}
