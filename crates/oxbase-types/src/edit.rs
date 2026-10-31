//! Grid edits and the row key used to write them back.
//!
//! The grid only allows edits when the result maps to one table with a primary or unique key
//! (PLAN §5). [`RowKey`] names that key; each [`RowEdit`] carries key values in the same column
//! order. A dialect `DmlWriter` turns `(edits, key)` into a script.

use serde::{Deserialize, Serialize};

use crate::{Error, ObjectRef, Result, Value};

/// Which kind of key identifies rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyKind {
    Primary,
    Unique,
}

/// The table being edited and the key columns that identify a row in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowKey {
    pub table: ObjectRef,
    pub kind: KeyKind,
    /// Key column names, in the order key values are given.
    pub columns: Vec<String>,
}

/// A column name paired with a value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnValue {
    pub column: String,
    pub value: Value,
}

impl ColumnValue {
    pub fn new(column: impl Into<String>, value: impl Into<Value>) -> Self {
        Self {
            column: column.into(),
            value: value.into(),
        }
    }
}

/// One pending change from the grid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RowEdit {
    /// A new row. Columns not listed take their defaults.
    Insert { values: Vec<ColumnValue> },
    /// Changes to an existing row identified by `key` (values in [`RowKey::columns`] order).
    Update {
        key: Vec<Value>,
        changes: Vec<ColumnValue>,
    },
    /// Deletion of the row identified by `key`.
    Delete { key: Vec<Value> },
}

impl RowKey {
    pub fn new(table: ObjectRef, kind: KeyKind, columns: Vec<String>) -> Self {
        Self {
            table,
            kind,
            columns,
        }
    }

    /// Checks that an edit can be written with this key: the key is non-empty, key values have
    /// the key's arity and contain no NULL, and updates/inserts change at least one column.
    pub fn validate(&self, edit: &RowEdit) -> Result<()> {
        if self.columns.is_empty() {
            return Err(Error::invalid_input(format!(
                "{} has no key columns; editing is not possible",
                self.table
            )));
        }
        let key = match edit {
            RowEdit::Insert { values } => {
                if values.is_empty() {
                    return Err(Error::invalid_input("insert with no values"));
                }
                return Ok(());
            }
            RowEdit::Update { key, changes } => {
                if changes.is_empty() {
                    return Err(Error::invalid_input("update with no changes"));
                }
                key
            }
            RowEdit::Delete { key } => key,
        };
        if key.len() != self.columns.len() {
            return Err(Error::invalid_input(format!(
                "key has {} values but {} key columns ({})",
                key.len(),
                self.columns.len(),
                self.columns.join(", ")
            )));
        }
        if let Some(i) = key.iter().position(Value::is_null) {
            return Err(Error::invalid_input(format!(
                "key column '{}' is NULL",
                self.columns[i]
            )));
        }
        Ok(())
    }

    /// Pairs key column names with an edit's key values.
    pub fn bind<'a>(&'a self, key: &'a [Value]) -> impl Iterator<Item = (&'a str, &'a Value)> {
        self.columns.iter().map(String::as_str).zip(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> RowKey {
        RowKey::new(
            ObjectRef::table(None, None, "t"),
            KeyKind::Primary,
            vec!["a".into(), "b".into()],
        )
    }

    #[test]
    fn validate() {
        let k = key();
        let ok = RowEdit::Update {
            key: vec![1.into(), "x".into()],
            changes: vec![ColumnValue::new("c", 3)],
        };
        assert!(k.validate(&ok).is_ok());
        let short = RowEdit::Delete {
            key: vec![1.into()],
        };
        assert!(k.validate(&short).is_err());
        let null = RowEdit::Delete {
            key: vec![1.into(), Value::Null],
        };
        assert!(k.validate(&null).unwrap_err().to_string().contains("'b'"));
        let empty = RowEdit::Update {
            key: vec![1.into(), 2.into()],
            changes: vec![],
        };
        assert!(k.validate(&empty).is_err());
        let ins = RowEdit::Insert {
            values: vec![ColumnValue::new("a", 1)],
        };
        assert!(k.validate(&ins).is_ok());
        let none = RowKey::new(ObjectRef::table(None, None, "t"), KeyKind::Unique, vec![]);
        assert!(none.validate(&ins).is_err());
    }

    #[test]
    fn bind_and_serde() {
        let k = key();
        let vals = [Value::Int(1), Value::Text("x".into())];
        let pairs: Vec<_> = k.bind(&vals).collect();
        assert_eq!(pairs[1], ("b", &Value::Text("x".into())));
        let e = RowEdit::Delete { key: vals.to_vec() };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.starts_with(r#"{"op":"delete""#));
        assert_eq!(serde_json::from_str::<RowEdit>(&json).unwrap(), e);
    }
}
