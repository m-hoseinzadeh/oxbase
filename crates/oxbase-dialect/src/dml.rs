//! Grid write-back (M3): what a [`DmlWriter`](crate::DmlWriter) gets ([`DmlTarget`]) and
//! returns ([`DmlPlan`]), plus helpers every engine's writer uses.
//!
//! # Contract
//!
//! For one [`RowEdit`](oxbase_types::RowEdit) the writer returns a [`DmlPlan`]:
//!
//! - `write`: one `INSERT`, `UPDATE` or `DELETE` as a [`BoundStatement`]. Identifiers are quoted
//!   with the engine's `Quoting`; **every value is a bound parameter** (never SQL text).
//!   `UPDATE` and `DELETE` match the row by its key only: `WHERE k1 = ? AND k2 = ?` with the
//!   edit's key values ([`write_key_predicate`]). Generated columns and `GENERATED ALWAYS`
//!   identity columns are never written (`InvalidInput` if an edit names one). An insert with
//!   no values inserts a row of defaults (`DEFAULT VALUES`, MySQL `() VALUES ()`).
//! - `returning`: the write itself returns the affected row (`RETURNING <read_back columns>`:
//!   PostgreSQL; SQLite 3.35+; MariaDB 10.5+ for `INSERT` and `DELETE`).
//! - `read_back`: when the write cannot return the row and `target.read_back` is not empty, a
//!   `SELECT <read_back columns> FROM t WHERE <key>` run right after the write in the same
//!   transaction. For a new row whose key the engine generated, the key comes from the session
//!   (MySQL/MariaDB `WHERE id = LAST_INSERT_ID()`); when an edit changed key values, the new
//!   ones are used. `None` when the row cannot be found again (no key value known): the service
//!   then keeps the values as typed and says the row was not refreshed.
//!
//! # Optimistic concurrency (enforced by the edit service)
//!
//! The service runs every plan of one apply in **one transaction**. A write's affected-row count
//! is the number of rows its result set returned when `returning`, else the sum of its
//! `RowsAffected` items. Anything but exactly 1 fails that edit as a conflict ("the row was
//! changed or deleted by someone else") and rolls the whole apply back. MySQL/MariaDB report
//! *matched* rows for `UPDATE` only with the `CLIENT_FOUND_ROWS` connect flag, which the mysql
//! driver sets (M3), so an `UPDATE` that writes the value a row already has still counts 1.

use oxbase_types::{BoundStatement, Error, Result, RowKey, ServerVersion, TableDef, Value};

use crate::sql::SqlWriter;

/// What an engine's [`DmlWriter`](crate::DmlWriter) writes to.
#[derive(Debug, Clone, Copy)]
pub struct DmlTarget<'a> {
    /// The table as loaded (column types, generated and identity columns, auto-increment).
    pub table: &'a TableDef,
    /// The key that identifies rows (primary key, else a NOT NULL unique key).
    pub key: &'a RowKey,
    pub version: &'a ServerVersion,
    /// Columns to read back after an insert or update, in this order (the grid's result
    /// columns, `SELECT *` order). Empty: no read-back.
    pub read_back: &'a [String],
}

/// How to write one edit and read the row back. See the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct DmlPlan {
    pub write: BoundStatement,
    /// `write` returns the affected row(s) (`RETURNING`): count its rows, read the row from it.
    pub returning: bool,
    /// Run after `write` (same transaction) to read the row back, when `returning` is false.
    pub read_back: Option<BoundStatement>,
}

impl DmlPlan {
    /// A write with no read-back (deletes, or nothing to read back).
    pub fn write_only(write: BoundStatement) -> Self {
        Self {
            write,
            returning: false,
            read_back: None,
        }
    }
}

/// Appends `k1 = ? AND k2 = ? ...` for `key`'s columns, binding `values` (in key column order).
/// `InvalidInput` when the counts differ or a value is NULL (a key never matches NULL).
pub fn write_key_predicate(w: &mut SqlWriter<'_>, key: &RowKey, values: &[Value]) -> Result<()> {
    if key.columns.is_empty() || values.len() != key.columns.len() {
        return Err(Error::invalid_input(format!(
            "{} key values for {} key columns",
            values.len(),
            key.columns.len()
        )));
    }
    for (pos, (column, value)) in key.bind(values).enumerate() {
        if value.is_null() {
            return Err(Error::invalid_input(format!(
                "key column '{column}' is NULL"
            )));
        }
        if pos > 0 {
            w.push(" AND ");
        }
        w.ident(column).push(" = ").param(value.clone());
    }
    Ok(())
}

/// Appends `c1, c2, ...` (quoted), e.g. for a `RETURNING` or `SELECT` list.
pub fn write_column_list(w: &mut SqlWriter<'_>, columns: &[String]) {
    for (pos, column) in columns.iter().enumerate() {
        if pos > 0 {
            w.push(", ");
        }
        w.ident(column);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BacktickQuoting, PgQuoting};
    use oxbase_types::{KeyKind, ObjectRef};
    use pretty_assertions::assert_eq;

    #[test]
    fn key_predicates_bind_every_key_value() {
        let key = RowKey::new(
            ObjectRef::table(None, Some("s"), "t"),
            KeyKind::Primary,
            vec!["a".into(), "b c".into()],
        );
        let mut w = SqlWriter::new(&PgQuoting);
        w.push("DELETE FROM ").qualified(&key.table).push(" WHERE ");
        write_key_predicate(&mut w, &key, &[Value::Int(1), "x'y".into()]).unwrap();
        let st = w.finish();
        assert_eq!(
            st.sql,
            "DELETE FROM \"s\".\"t\" WHERE \"a\" = $1 AND \"b c\" = $2"
        );
        assert_eq!(st.params, [Value::Int(1), Value::Text("x'y".into())]);

        let mut w = SqlWriter::new(&BacktickQuoting);
        assert!(write_key_predicate(&mut w, &key, &[Value::Int(1)]).is_err());
        assert!(write_key_predicate(&mut w, &key, &[Value::Int(1), Value::Null]).is_err());

        let mut w = SqlWriter::new(&BacktickQuoting);
        write_column_list(&mut w, &["id".into(), "na`me".into()]);
        assert_eq!(w.sql(), "`id`, `na``me`");
        let plan = DmlPlan::write_only(w.finish());
        assert!(!plan.returning && plan.read_back.is_none());
    }
}
