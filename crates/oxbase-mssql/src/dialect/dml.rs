//! The grid [`DmlWriter`] (M4, **ddl** stream). Contract: `docs/m4-contracts.md` §4.5 and the M3
//! `DmlWriter` rules (bound values, key-only match, read-back).
//!
//! Every plan is one batch with `returning = true` whose single result set has exactly one row
//! per affected row, so the edit service's "exactly one row" count is right whatever triggers
//! do, `SET NOCOUNT` says, or `@@ROWCOUNT` reports:
//!
//! ```sql
//! SET NOCOUNT ON;
//! DECLARE @k TABLE ([id] int);                      -- key columns, their native types
//! UPDATE [s].[t] SET [name] = @P1
//!   OUTPUT INSERTED.[id] INTO @k                    -- INTO: allowed on tables with triggers
//!   WHERE [id] = @P2;
//! SELECT t.[id], t.[name], ... FROM [s].[t] AS t    -- read-back after triggers ran
//!   JOIN @k AS k ON t.[id] = k.[id];                -- (SELECT k.* FROM @k without read_back)
//! SET NOCOUNT OFF;                                  -- a batch without parameters runs in
//!                                                   -- the session's own scope
//! ```
//!
//! - `INSERT ... OUTPUT INSERTED.<key> INTO @k VALUES (...)` / `DEFAULT VALUES` gets generated
//!   keys (identity, `NEWID()` / `NEWSEQUENTIALID()` defaults) without `SCOPE_IDENTITY()`.
//! - `DELETE ... OUTPUT DELETED.<key> INTO @k WHERE <key>; SELECT k.* FROM @k`.
//! - An update that changes key values reads back by `INSERTED` (the new values).
//! - Never written: identity columns (`IdentityDef::always`), computed columns,
//!   [`crate::extras::ROW_VERSION`] and [`crate::extras::PERIOD`] columns (`InvalidInput`
//!   naming the column).
//! - `Value::Null` is written as the keyword `NULL`, not a parameter (an `nvarchar` NULL
//!   parameter does not convert to `varbinary`, `image` or CLR types). Text for a column whose
//!   type has no neutral equivalent (`xml`, `hierarchyid`, `geometry`, `geography`,
//!   `sql_variant`, alias types of those, ...) is bound as text and converted in SQL:
//!   `CAST(@Pn AS <native type>)`.
//! - Key columns of type `text`, `ntext`, `image`, `xml`, `json`, `vector`, CLR types or `(max)`
//!   types cannot be compared or declared in `@k`: `Unsupported` ("this key cannot identify rows
//!   on SQL Server"). A `rowversion` key is declared `binary(8)`; a key column with its own
//!   collation keeps it in `@k`, so the join does not mix collations.
//! - Optimistic concurrency is by key, as in M3: an update or delete that finds no row returns
//!   an empty result set (0 rows), which the edit service reports as a conflict. `rowversion`
//!   is not used as a version check in M4.
//! - Every table is transactional (`transactional()` keeps the default `true`).

#![allow(
    clippy::many_single_char_names,
    reason = "plans, keys and versions read best with short names in the tests"
)]

use std::collections::HashSet;

use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{BracketQuoting, DmlPlan, DmlTarget, DmlWriter, Quoting};
use oxbase_types::{
    ColumnDef, ColumnValue, DataType, Error, PropId, Result, RowEdit, RowKey, ServerVersion,
    TableDef, Value,
};

use super::compare::{canonical_type, split_type};
use super::ddl::{checked_type, neutral_type};
use crate::extras;

/// The SQL Server grid writer.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MsSqlDmlWriter;

fn column<'t>(table: &'t TableDef, name: &str) -> Result<&'t ColumnDef> {
    table.column(name).ok_or_else(|| {
        Error::invalid_input(format!(
            "{} has no column '{name}'",
            table.obj.display_name()
        ))
    })
}

fn flag(c: &ColumnDef, prop: &PropId) -> bool {
    c.extras.get(prop).and_then(Value::as_bool).unwrap_or(false)
}

/// The type of `c` as written in SQL.
fn type_text(c: &ColumnDef, v: &ServerVersion) -> Result<String> {
    if c.native_type.trim().is_empty() {
        neutral_type(&c.data_type, v)
    } else {
        checked_type(&c.name, &c.native_type)
    }
}

fn is_rowversion(c: &ColumnDef) -> bool {
    flag(c, &extras::ROW_VERSION) || canonical_type(&c.native_type) == "rowversion"
}

/// The column `name`, refusing columns the server maintains.
fn writable<'t>(table: &'t TableDef, name: &str) -> Result<&'t ColumnDef> {
    let col = column(table, name)?;
    let refuse = |why: &str| {
        Err(Error::invalid_input(format!(
            "column '{name}' is {why} and cannot be written"
        )))
    };
    if col.generated.is_some() {
        return refuse("a computed column");
    }
    if col.identity.is_some() || col.auto_increment {
        return refuse("an IDENTITY column");
    }
    if is_rowversion(col) {
        return refuse("a rowversion column");
    }
    if col.extras.contains(&extras::PERIOD) {
        return refuse("a period column of a system-versioned table");
    }
    Ok(col)
}

/// `true` when a text value for `col` is converted in SQL (types without a neutral
/// equivalent, which the driver binds as `nvarchar(max)`).
fn needs_cast(col: &ColumnDef) -> bool {
    matches!(col.data_type, DataType::Other { .. })
}

/// Appends `NULL`, a placeholder, or `CAST(@Pn AS <type>)` for `value` in column `col`.
fn bind(w: &mut SqlWriter<'_>, col: &ColumnDef, value: &Value, v: &ServerVersion) -> Result<()> {
    match value {
        Value::Null => {
            w.push("NULL");
        }
        Value::Text(_) if needs_cast(col) => {
            let ty = type_text(col, v)?;
            w.push("CAST(")
                .param(value.clone())
                .push(" AS ")
                .push(&ty)
                .push(")");
        }
        other => {
            w.param(other.clone());
        }
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

/// Types that cannot be compared or be index keys (CLR assembly types are refused by the
/// server).
const NOT_KEYS: [&str; 9] = [
    "text",
    "ntext",
    "image",
    "xml",
    "json",
    "vector",
    "geometry",
    "geography",
    "hierarchyid",
];

/// The `@k` column of key column `c`: `[c] <type> [COLLATE x]`.
fn key_decl(c: &ColumnDef, v: &ServerVersion) -> Result<String> {
    let q = BracketQuoting;
    if is_rowversion(c) {
        return Ok(format!("{} binary(8)", q.quote_ident(&c.name)));
    }
    let ty = type_text(c, v)?;
    let canonical = canonical_type(&ty);
    let (base, args) = split_type(&canonical);
    if NOT_KEYS.contains(&base) || args.first() == Some(&"max") {
        return Err(Error::unsupported(format!(
            "key column '{}' ({ty}): this key cannot identify rows on SQL Server",
            c.name
        )));
    }
    let mut decl = format!("{} {ty}", q.quote_ident(&c.name));
    if let Some(collation) = &c.collation {
        if !collation
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            return Err(Error::invalid_input(format!(
                "column '{}': '{collation}' is not a collation name",
                c.name
            )));
        }
        decl.push_str(" COLLATE ");
        decl.push_str(collation);
    }
    Ok(decl)
}

/// `k1 = @Pn AND k2 = @Pm` for the key values (NULL never matches a key).
fn key_predicate(
    w: &mut SqlWriter<'_>,
    table: &TableDef,
    key: &RowKey,
    values: &[Value],
    v: &ServerVersion,
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
        bind(w, col, value, v)?;
    }
    Ok(())
}

/// `OUTPUT <prefix>.[k1], <prefix>.[k2] INTO @k`.
fn output(w: &mut SqlWriter<'_>, prefix: &str, key: &RowKey) {
    w.push(" OUTPUT ");
    for (pos, name) in key.columns.iter().enumerate() {
        if pos > 0 {
            w.push(", ");
        }
        w.push(prefix).push(".").ident(name);
    }
    w.push(" INTO @k");
}

impl DmlWriter for MsSqlDmlWriter {
    fn plan(&self, edit: &RowEdit, target: &DmlTarget<'_>) -> Result<DmlPlan> {
        target.key.validate(edit)?;
        let table = target.table;
        let key = target.key;
        let v = target.version;
        for name in target.read_back {
            column(table, name)?;
        }
        let mut decls = Vec::with_capacity(key.columns.len());
        for name in &key.columns {
            decls.push(key_decl(column(table, name)?, v)?);
        }
        let mut w = SqlWriter::new(&BracketQuoting);
        w.push("SET NOCOUNT ON;\nDECLARE @k TABLE (")
            .push(&decls.join(", "))
            .push(");\n");
        let read_rows = match edit {
            RowEdit::Insert { values } => {
                no_duplicates(values)?;
                w.push("INSERT INTO ").qualified(&table.obj);
                if values.is_empty() {
                    output(&mut w, "INSERTED", key);
                    w.push(" DEFAULT VALUES;\n");
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
                    w.push(")");
                    output(&mut w, "INSERTED", key);
                    w.push(" VALUES (");
                    for (pos, (col, cv)) in cols.iter().zip(values).enumerate() {
                        if pos > 0 {
                            w.push(", ");
                        }
                        bind(&mut w, col, &cv.value, v)?;
                    }
                    w.push(");\n");
                }
                true
            }
            RowEdit::Update {
                key: values,
                changes,
            } => {
                no_duplicates(changes)?;
                w.push("UPDATE ").qualified(&table.obj).push(" SET ");
                for (pos, cv) in changes.iter().enumerate() {
                    let col = writable(table, &cv.column)?;
                    if pos > 0 {
                        w.push(", ");
                    }
                    w.ident(&col.name).push(" = ");
                    bind(&mut w, col, &cv.value, v)?;
                }
                output(&mut w, "INSERTED", key);
                w.push(" WHERE ");
                key_predicate(&mut w, table, key, values, v)?;
                w.push(";\n");
                true
            }
            RowEdit::Delete { key: values } => {
                w.push("DELETE FROM ").qualified(&table.obj);
                output(&mut w, "DELETED", key);
                w.push(" WHERE ");
                key_predicate(&mut w, table, key, values, v)?;
                w.push(";\n");
                false
            }
        };
        if read_rows && !target.read_back.is_empty() {
            w.push("SELECT ");
            for (pos, name) in target.read_back.iter().enumerate() {
                if pos > 0 {
                    w.push(", ");
                }
                w.push("t.").ident(name);
            }
            w.push(" FROM ")
                .qualified(&table.obj)
                .push(" AS t JOIN @k AS k ON ");
            for (pos, name) in key.columns.iter().enumerate() {
                if pos > 0 {
                    w.push(" AND ");
                }
                w.push("t.").ident(name).push(" = k.").ident(name);
            }
            w.push(";\n");
        } else {
            w.push("SELECT k.* FROM @k AS k;\n");
        }
        w.push("SET NOCOUNT OFF;");
        Ok(DmlPlan {
            write: w.finish(),
            returning: true,
            read_back: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{GeneratedColumn, IdentityDef, KeyKind, ObjectRef, PrimaryKeyDef};
    use pretty_assertions::assert_eq;

    fn col(name: &str, native: &str, data_type: DataType) -> ColumnDef {
        let mut c = ColumnDef::new(name, data_type);
        c.native_type = native.to_owned();
        c
    }

    /// Like the `m3.m3_edit_pk` seed table of the mssql fixture, plus a few special columns.
    fn table() -> TableDef {
        let mut t = TableDef::new(ObjectRef::table(
            Some("oxbase_test"),
            Some("m3"),
            "m3_edit_pk",
        ));
        let mut id = col("id", "int", DataType::INT32).not_null();
        id.identity = Some(IdentityDef {
            always: true,
            start: Some(1),
            increment: Some(1),
        });
        id.auto_increment = true;
        let code = col(
            "code",
            "varchar(20)",
            DataType::VarChar { length: Some(20) },
        )
        .not_null();
        let name = col("name", "nvarchar(max)", DataType::Text);
        let data = col(
            "data",
            "varbinary(64)",
            DataType::VarBinary { length: Some(64) },
        );
        let mut total = col(
            "total",
            "decimal(13,2)",
            DataType::Decimal {
                precision: Some(13),
                scale: Some(2),
            },
        );
        total.generated = Some(GeneratedColumn {
            expression: "([amount]*(2))".into(),
            stored: true,
        });
        let mut rv = col("rv", "rowversion", DataType::Binary { length: Some(8) }).not_null();
        rv.extras.insert(extras::ROW_VERSION, true);
        let geo = col(
            "geo",
            "geometry",
            DataType::Other {
                name: "geometry".into(),
            },
        );
        let mut from = col(
            "valid_from",
            "datetime2(7)",
            DataType::Timestamp { precision: Some(7) },
        );
        from.extras.insert(extras::PERIOD, "row_start");
        t.columns = vec![id, code, name, data, total, rv, geo, from];
        t.primary_key = Some(PrimaryKeyDef {
            name: Some("PK_m3_edit_pk".into()),
            columns: vec!["id".into()],
        });
        t
    }

    fn key() -> RowKey {
        RowKey::new(
            ObjectRef::table(Some("oxbase_test"), Some("m3"), "m3_edit_pk"),
            KeyKind::Primary,
            vec!["id".into()],
        )
    }

    fn plan(edit: &RowEdit, t: &TableDef, k: &RowKey, read_back: &[String]) -> Result<DmlPlan> {
        let v = ServerVersion::new(17, 0, 4005);
        MsSqlDmlWriter.plan(
            edit,
            &DmlTarget {
                table: t,
                key: k,
                version: &v,
                read_back,
            },
        )
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn insert_outputs_the_generated_key_and_reads_the_row_back() {
        let t = table();
        let edit = RowEdit::Insert {
            values: vec![
                ColumnValue::new("code", "a'b"),
                ColumnValue::new("name", Value::Null),
                ColumnValue::new("geo", "POINT(1 2)"),
                ColumnValue::new("data", Value::Bytes(vec![0xca, 0xfe])),
            ],
        };
        let p = plan(&edit, &t, &key(), &names(&["id", "code", "total", "rv"])).unwrap();
        assert_eq!(
            p.write.sql,
            "SET NOCOUNT ON;\nDECLARE @k TABLE ([id] int);\nINSERT INTO [m3].[m3_edit_pk] \
             ([code], [name], [geo], [data]) OUTPUT INSERTED.[id] INTO @k VALUES (@P1, NULL, \
             CAST(@P2 AS geometry), @P3);\nSELECT t.[id], t.[code], t.[total], t.[rv] FROM \
             [m3].[m3_edit_pk] AS t JOIN @k AS k ON t.[id] = k.[id];\nSET NOCOUNT OFF;"
        );
        assert_eq!(
            p.write.params,
            [
                Value::Text("a'b".into()),
                Value::Text("POINT(1 2)".into()),
                Value::Bytes(vec![0xca, 0xfe]),
            ]
        );
        assert!(p.returning);
        assert_eq!(p.read_back, None);
    }

    #[test]
    fn defaults_only_insert() {
        let p = plan(&RowEdit::Insert { values: vec![] }, &table(), &key(), &[]).unwrap();
        assert_eq!(
            p.write.sql,
            "SET NOCOUNT ON;\nDECLARE @k TABLE ([id] int);\nINSERT INTO [m3].[m3_edit_pk] \
             OUTPUT INSERTED.[id] INTO @k DEFAULT VALUES;\nSELECT k.* FROM @k AS k;\nSET \
             NOCOUNT OFF;"
        );
        assert_eq!(p.write.params, Vec::<Value>::new());
        assert!(p.returning);
    }

    #[test]
    fn update_and_delete_match_by_key() {
        let t = table();
        let edit = RowEdit::Update {
            key: vec![Value::Int(7)],
            changes: vec![
                ColumnValue::new("code", "z"),
                ColumnValue::new("data", Value::Null),
            ],
        };
        let p = plan(&edit, &t, &key(), &names(&["id", "code"])).unwrap();
        assert_eq!(
            p.write.sql,
            "SET NOCOUNT ON;\nDECLARE @k TABLE ([id] int);\nUPDATE [m3].[m3_edit_pk] SET \
             [code] = @P1, [data] = NULL OUTPUT INSERTED.[id] INTO @k WHERE [id] = @P2;\n\
             SELECT t.[id], t.[code] FROM [m3].[m3_edit_pk] AS t JOIN @k AS k ON t.[id] = \
             k.[id];\nSET NOCOUNT OFF;"
        );
        assert_eq!(p.write.params, [Value::Text("z".into()), Value::Int(7)]);
        let p = plan(
            &RowEdit::Delete {
                key: vec![Value::Int(7)],
            },
            &t,
            &key(),
            &names(&["id", "code"]),
        )
        .unwrap();
        assert_eq!(
            p.write.sql,
            "SET NOCOUNT ON;\nDECLARE @k TABLE ([id] int);\nDELETE FROM [m3].[m3_edit_pk] \
             OUTPUT DELETED.[id] INTO @k WHERE [id] = @P1;\nSELECT k.* FROM @k AS k;\nSET \
             NOCOUNT OFF;"
        );
        assert_eq!(p.write.params, [Value::Int(7)]);
        assert!(p.returning);
    }

    #[test]
    fn composite_and_collated_unique_keys() {
        let mut t = TableDef::new(ObjectRef::table(None, Some("m3"), "m3_edit_uq"));
        let mut code = col(
            "co]de",
            "varchar(20)",
            DataType::VarChar { length: Some(20) },
        )
        .not_null();
        code.collation = Some("Latin1_General_BIN2".into());
        let part = col("part", "", DataType::INT64).not_null();
        t.columns = vec![code, part, col("val", "int", DataType::INT32)];
        let k = RowKey::new(t.obj.clone(), KeyKind::Unique, names(&["co]de", "part"]));
        let edit = RowEdit::Update {
            key: vec![Value::Text("x".into()), Value::Int(2)],
            changes: vec![ColumnValue::new("val", Value::Int(3))],
        };
        let p = plan(&edit, &t, &k, &names(&["val"])).unwrap();
        assert_eq!(
            p.write.sql,
            "SET NOCOUNT ON;\nDECLARE @k TABLE ([co]]de] varchar(20) COLLATE \
             Latin1_General_BIN2, [part] bigint);\nUPDATE [m3].[m3_edit_uq] SET [val] = @P1 \
             OUTPUT INSERTED.[co]]de], INSERTED.[part] INTO @k WHERE [co]]de] = @P2 AND [part] \
             = @P3;\nSELECT t.[val] FROM [m3].[m3_edit_uq] AS t JOIN @k AS k ON t.[co]]de] = \
             k.[co]]de] AND t.[part] = k.[part];\nSET NOCOUNT OFF;"
        );
        assert_eq!(
            p.write.params,
            [Value::Int(3), Value::Text("x".into()), Value::Int(2)]
        );
    }

    #[test]
    fn refused_columns_and_keys() {
        let t = table();
        for name in ["id", "total", "rv", "valid_from"] {
            let edit = RowEdit::Insert {
                values: vec![ColumnValue::new(name, Value::Int(1))],
            };
            let made = plan(&edit, &t, &key(), &[]);
            match made {
                Err(Error::InvalidInput { message }) => {
                    assert!(message.contains(name), "{message}");
                }
                other => panic!("{name}: {other:?}"),
            }
            let edit = RowEdit::Update {
                key: vec![Value::Int(1)],
                changes: vec![ColumnValue::new(name, Value::Int(1))],
            };
            assert!(matches!(
                plan(&edit, &t, &key(), &[]),
                Err(Error::InvalidInput { .. })
            ));
        }
        // Unknown columns, duplicates, NULL keys.
        let edit = RowEdit::Insert {
            values: vec![ColumnValue::new("nope", Value::Int(1))],
        };
        assert!(matches!(
            plan(&edit, &t, &key(), &[]),
            Err(Error::InvalidInput { .. })
        ));
        let edit = RowEdit::Insert {
            values: vec![ColumnValue::new("code", "a"), ColumnValue::new("code", "b")],
        };
        assert!(matches!(
            plan(&edit, &t, &key(), &[]),
            Err(Error::InvalidInput { .. })
        ));
        let edit = RowEdit::Delete {
            key: vec![Value::Null],
        };
        assert!(matches!(
            plan(&edit, &t, &key(), &[]),
            Err(Error::InvalidInput { .. })
        ));
        assert!(matches!(
            plan(
                &RowEdit::Insert { values: vec![] },
                &t,
                &key(),
                &names(&["nope"])
            ),
            Err(Error::InvalidInput { .. })
        ));
        // Keys that cannot identify rows.
        for (name, native, dt) in [
            ("name", "nvarchar(max)", DataType::Text),
            (
                "geo",
                "geometry",
                DataType::Other {
                    name: "geometry".into(),
                },
            ),
            ("doc", "xml", DataType::Other { name: "xml".into() }),
            ("old", "text", DataType::Text),
        ] {
            let mut t = TableDef::new(ObjectRef::table(None, Some("m3"), "k"));
            t.columns = vec![col(name, native, dt).not_null()];
            let k = RowKey::new(t.obj.clone(), KeyKind::Unique, names(&[name]));
            let made = plan(
                &RowEdit::Delete {
                    key: vec![Value::Text("x".into())],
                },
                &t,
                &k,
                &[],
            );
            assert!(
                matches!(made, Err(Error::Unsupported { .. })),
                "{name}: {made:?}"
            );
        }
        // A rowversion key is declared binary(8).
        let mut t = TableDef::new(ObjectRef::table(None, Some("m3"), "k"));
        t.columns = vec![col("rv", "timestamp", DataType::Binary { length: Some(8) }).not_null()];
        let k = RowKey::new(t.obj.clone(), KeyKind::Unique, names(&["rv"]));
        let p = plan(
            &RowEdit::Delete {
                key: vec![Value::Bytes(vec![0; 8])],
            },
            &t,
            &k,
            &[],
        )
        .unwrap();
        assert!(
            p.write.sql.contains("DECLARE @k TABLE ([rv] binary(8));"),
            "{}",
            p.write.sql
        );
        assert!(MsSqlDmlWriter.transactional(&t));
    }
}
