//! The filter and sort model of the grid's "table data" view (M3).
//!
//! A [`Filter`] is a typed tree: `and` / `or` groups, `not`, conditions (`column op values`) and
//! one escape hatch, [`Filter::RawSql`], a boolean SQL expression the user typed. A dialect
//! renders it into a `WHERE` clause with **bound parameters** (`oxbase_dialect::sql`): column
//! names are quoted with the engine's `Quoting`, values are never written into the SQL text.
//! [`SortKey`]s render into `ORDER BY`. [`TableDataQuery`] is the whole request:
//! `SELECT * FROM <table> [WHERE ...] [ORDER BY ...] [LIMIT n]`.
//!
//! Serde (this is a UI payload): camelCase, `type`-tagged (`{"type":"condition","column":"id",
//! "op":"eq","values":[{"type":"int","value":1}]}`). [`Value`]s keep their own snake_case form.
//!
//! Semantics every dialect keeps:
//!
//! - Comparisons follow SQL: `ne` does not match NULLs, `notIn` / `notBetween` neither. A NULL
//!   value in a comparison is refused (use `isNull` / `isNotNull`).
//! - `like` / `notLike` take the user's pattern as is (`%` and `_` are wildcards; no `ESCAPE`
//!   clause, so the engine's default escape applies). `contains`, `notContains`, `startsWith`
//!   and `endsWith` match the text literally: the dialect escapes `%`, `_` and its escape
//!   character and adds `ESCAPE`. Case sensitivity is the engine's (PostgreSQL: sensitive;
//!   MySQL/MariaDB: by the column's collation; SQLite: insensitive for ASCII). On a column whose
//!   type is known and not textual, the pattern operators compare the column cast to text.
//! - An empty group (`and` / `or` with no items left after dropping empty children) and a `not`
//!   of an empty group add no condition.
//! - [`Filter::RawSql`] is inserted as `(` *sql* `)` after a lexical safety check (one
//!   expression: no `;`, no comments, balanced quotes and parentheses; see
//!   `oxbase_dialect::sql::check_expression`). It runs with the same rights as the SQL editor.
//!   The UI marks it as raw SQL.

use serde::{Deserialize, Serialize};

use crate::{Error, ObjectRef, Result, Value};

/// Most nesting levels a filter may have.
pub const MAX_FILTER_DEPTH: usize = 32;
/// Most conditions (and raw expressions) one filter may hold.
pub const MAX_FILTER_CONDITIONS: usize = 1000;
/// Most values one `in` / `notIn` condition may list.
pub const MAX_IN_VALUES: usize = 1000;

/// A comparison operator of a [`Filter::Condition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FilterOp {
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `LIKE pattern` (user wildcards).
    Like,
    /// `NOT LIKE pattern` (user wildcards).
    NotLike,
    /// Text contains the value literally.
    Contains,
    /// Text does not contain the value.
    NotContains,
    /// Text starts with the value literally.
    StartsWith,
    /// Text ends with the value literally.
    EndsWith,
    /// `IN (v1, v2, ...)`
    In,
    /// `NOT IN (v1, v2, ...)`
    NotIn,
    IsNull,
    IsNotNull,
    /// `BETWEEN low AND high` (inclusive).
    Between,
    NotBetween,
}

/// How many values an operator takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    None,
    One,
    Two,
    /// One or more, up to [`MAX_IN_VALUES`].
    Many,
}

impl FilterOp {
    pub const ALL: [FilterOp; 18] = [
        FilterOp::Eq,
        FilterOp::Ne,
        FilterOp::Lt,
        FilterOp::Le,
        FilterOp::Gt,
        FilterOp::Ge,
        FilterOp::Like,
        FilterOp::NotLike,
        FilterOp::Contains,
        FilterOp::NotContains,
        FilterOp::StartsWith,
        FilterOp::EndsWith,
        FilterOp::In,
        FilterOp::NotIn,
        FilterOp::IsNull,
        FilterOp::IsNotNull,
        FilterOp::Between,
        FilterOp::NotBetween,
    ];

    /// How many values the operator takes.
    pub const fn arity(self) -> Arity {
        match self {
            Self::IsNull | Self::IsNotNull => Arity::None,
            Self::Between | Self::NotBetween => Arity::Two,
            Self::In | Self::NotIn => Arity::Many,
            _ => Arity::One,
        }
    }

    /// `true` for the text-pattern operators (their value must be text).
    pub const fn is_pattern(self) -> bool {
        matches!(
            self,
            Self::Like
                | Self::NotLike
                | Self::Contains
                | Self::NotContains
                | Self::StartsWith
                | Self::EndsWith
        )
    }

    /// The camelCase serde name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Lt => "lt",
            Self::Le => "le",
            Self::Gt => "gt",
            Self::Ge => "ge",
            Self::Like => "like",
            Self::NotLike => "notLike",
            Self::Contains => "contains",
            Self::NotContains => "notContains",
            Self::StartsWith => "startsWith",
            Self::EndsWith => "endsWith",
            Self::In => "in",
            Self::NotIn => "notIn",
            Self::IsNull => "isNull",
            Self::IsNotNull => "isNotNull",
            Self::Between => "between",
            Self::NotBetween => "notBetween",
        }
    }
}

/// A filter tree. See the module docs for the semantics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Filter {
    /// Every item must hold.
    And { items: Vec<Filter> },
    /// At least one item must hold.
    Or { items: Vec<Filter> },
    /// The item must not hold.
    Not { item: Box<Filter> },
    /// `column op values`. The column is a name of the table (unquoted, exact case).
    Condition {
        column: String,
        op: FilterOp,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        values: Vec<Value>,
    },
    /// RAW SQL: a boolean expression typed by the user, inserted as is (after the lexical check
    /// of the module docs). Never built from data.
    RawSql { sql: String },
}

impl Filter {
    /// `column op values`.
    pub fn condition(column: impl Into<String>, op: FilterOp, values: Vec<Value>) -> Self {
        Self::Condition {
            column: column.into(),
            op,
            values,
        }
    }

    pub fn and(items: Vec<Filter>) -> Self {
        Self::And { items }
    }

    pub fn or(items: Vec<Filter>) -> Self {
        Self::Or { items }
    }

    pub fn negate(item: Filter) -> Self {
        Self::Not {
            item: Box::new(item),
        }
    }

    pub fn raw_sql(sql: impl Into<String>) -> Self {
        Self::RawSql { sql: sql.into() }
    }

    /// `true` when the filter adds no condition (only empty groups).
    pub fn is_empty(&self) -> bool {
        match self {
            Self::And { items } | Self::Or { items } => items.iter().all(Filter::is_empty),
            Self::Not { item } => item.is_empty(),
            Self::Condition { .. } | Self::RawSql { .. } => false,
        }
    }

    /// Checks the shape: depth at most [`MAX_FILTER_DEPTH`], at most [`MAX_FILTER_CONDITIONS`]
    /// conditions, non-empty column names, the operator's arity, no NULL values, text values for
    /// pattern operators, non-blank raw SQL. The raw SQL's lexical check is the dialect's
    /// (`oxbase_dialect::sql::check_expression`).
    pub fn validate(&self) -> Result<()> {
        let mut conditions = 0usize;
        self.validate_at(1, &mut conditions)
    }

    fn validate_at(&self, depth: usize, conditions: &mut usize) -> Result<()> {
        if depth > MAX_FILTER_DEPTH {
            return Err(Error::invalid_input(format!(
                "the filter is nested more than {MAX_FILTER_DEPTH} levels deep"
            )));
        }
        match self {
            Self::And { items } | Self::Or { items } => {
                for item in items {
                    item.validate_at(depth + 1, conditions)?;
                }
                Ok(())
            }
            Self::Not { item } => item.validate_at(depth + 1, conditions),
            Self::Condition { column, op, values } => {
                count(conditions)?;
                validate_condition(column, *op, values)
            }
            Self::RawSql { sql } => {
                count(conditions)?;
                if sql.trim().is_empty() {
                    return Err(Error::invalid_input("the raw SQL filter is empty"));
                }
                Ok(())
            }
        }
    }
}

fn count(conditions: &mut usize) -> Result<()> {
    *conditions += 1;
    if *conditions > MAX_FILTER_CONDITIONS {
        return Err(Error::invalid_input(format!(
            "the filter has more than {MAX_FILTER_CONDITIONS} conditions"
        )));
    }
    Ok(())
}

fn validate_condition(column: &str, op: FilterOp, values: &[Value]) -> Result<()> {
    if column.is_empty() {
        return Err(Error::invalid_input("a filter condition has no column"));
    }
    let n = values.len();
    let ok = match op.arity() {
        Arity::None => n == 0,
        Arity::One => n == 1,
        Arity::Two => n == 2,
        Arity::Many => (1..=MAX_IN_VALUES).contains(&n),
    };
    if !ok {
        let wanted = match op.arity() {
            Arity::None => "no value".to_owned(),
            Arity::One => "one value".to_owned(),
            Arity::Two => "two values".to_owned(),
            Arity::Many => format!("1 to {MAX_IN_VALUES} values"),
        };
        return Err(Error::invalid_input(format!(
            "filter on '{column}': '{}' takes {wanted}, got {n}",
            op.as_str()
        )));
    }
    if values.iter().any(Value::is_null) {
        return Err(Error::invalid_input(format!(
            "filter on '{column}': compare with NULL using isNull / isNotNull"
        )));
    }
    if op.is_pattern() && !values.iter().all(|v| matches!(v, Value::Text(_))) {
        return Err(Error::invalid_input(format!(
            "filter on '{column}': '{}' needs a text value",
            op.as_str()
        )));
    }
    Ok(())
}

/// Where NULLs sort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NullsOrder {
    First,
    Last,
}

/// One `ORDER BY` key. `nulls: None` keeps the engine's default (PostgreSQL: NULLs last when
/// ascending; MySQL/MariaDB and SQLite: NULLs first when ascending).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SortKey {
    pub column: String,
    #[serde(default)]
    pub descending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nulls: Option<NullsOrder>,
}

impl SortKey {
    pub fn asc(column: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            descending: false,
            nulls: None,
        }
    }

    pub fn desc(column: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            descending: true,
            nulls: None,
        }
    }
}

/// The grid's table-data request: `SELECT * FROM table [WHERE filter] [ORDER BY sort]
/// [LIMIT limit]`. `limit: None` reads every row (the grid pages through the result store).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableDataQuery {
    /// A `Table` (or `View`, read-only) ref as the object tree lists it.
    pub table: ObjectRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Filter>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<SortKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

impl TableDataQuery {
    /// Every row of `table`, unfiltered and unsorted.
    pub fn new(table: ObjectRef) -> Self {
        Self {
            table,
            filter: None,
            sort: Vec::new(),
            limit: None,
        }
    }

    /// Validates the filter and the sort keys (non-empty, distinct column names).
    pub fn validate(&self) -> Result<()> {
        if let Some(filter) = &self.filter {
            filter.validate()?;
        }
        for (i, key) in self.sort.iter().enumerate() {
            if key.column.is_empty() {
                return Err(Error::invalid_input("a sort key has no column"));
            }
            if self.sort[..i].iter().any(|k| k.column == key.column) {
                return Err(Error::invalid_input(format!(
                    "column '{}' is sorted on twice",
                    key.column
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectKind;
    use pretty_assertions::assert_eq;

    fn cond(op: FilterOp, values: Vec<Value>) -> Filter {
        Filter::condition("c", op, values)
    }

    #[test]
    fn arity_and_values_are_checked() {
        assert!(cond(FilterOp::Eq, vec![1.into()]).validate().is_ok());
        assert!(cond(FilterOp::Eq, vec![]).validate().is_err());
        assert!(cond(FilterOp::IsNull, vec![]).validate().is_ok());
        assert!(cond(FilterOp::IsNull, vec![1.into()]).validate().is_err());
        assert!(
            cond(FilterOp::Between, vec![1.into(), 2.into()])
                .validate()
                .is_ok()
        );
        assert!(cond(FilterOp::Between, vec![1.into()]).validate().is_err());
        assert!(cond(FilterOp::In, vec![]).validate().is_err());
        let many: Vec<Value> = (0..=MAX_IN_VALUES)
            .map(|i| Value::from(i64::try_from(i).unwrap()))
            .collect();
        assert!(cond(FilterOp::In, many).validate().is_err());
        let err = cond(FilterOp::Eq, vec![Value::Null])
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("isNull"), "{err}");
        assert!(cond(FilterOp::Contains, vec![1.into()]).validate().is_err());
        assert!(
            cond(FilterOp::Contains, vec!["x".into()])
                .validate()
                .is_ok()
        );
        assert!(
            Filter::condition("", FilterOp::IsNull, vec![])
                .validate()
                .is_err()
        );
        assert!(Filter::raw_sql("  ").validate().is_err());
        for op in FilterOp::ALL {
            let json = serde_json::to_value(op).unwrap();
            assert_eq!(json, op.as_str());
        }
    }

    #[test]
    fn depth_and_size_are_bounded() {
        let mut f = cond(FilterOp::IsNull, vec![]);
        for _ in 0..MAX_FILTER_DEPTH {
            f = Filter::negate(f);
        }
        assert!(f.validate().is_err());
        let wide = Filter::and(
            (0..=MAX_FILTER_CONDITIONS)
                .map(|_| cond(FilterOp::IsNull, vec![]))
                .collect(),
        );
        assert!(wide.validate().is_err());
    }

    #[test]
    fn empty_groups_add_nothing() {
        assert!(Filter::and(vec![]).is_empty());
        assert!(Filter::or(vec![Filter::and(vec![])]).is_empty());
        assert!(Filter::negate(Filter::or(vec![])).is_empty());
        assert!(!Filter::and(vec![Filter::raw_sql("x > 1")]).is_empty());
    }

    #[test]
    fn serde_is_camel_case_and_type_tagged() {
        let f = Filter::and(vec![
            Filter::condition("id", FilterOp::Ge, vec![Value::Int(3)]),
            Filter::negate(Filter::condition(
                "name",
                FilterOp::StartsWith,
                vec!["a%".into()],
            )),
            Filter::raw_sql("x <> 1"),
        ]);
        let json = serde_json::to_value(&f).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "and",
                "items": [
                    {"type": "condition", "column": "id", "op": "ge",
                     "values": [{"type": "int", "value": 3}]},
                    {"type": "not", "item": {"type": "condition", "column": "name",
                     "op": "startsWith", "values": [{"type": "text", "value": "a%"}]}},
                    {"type": "rawSql", "sql": "x <> 1"}
                ]
            })
        );
        assert_eq!(serde_json::from_value::<Filter>(json).unwrap(), f);
        let null: Filter =
            serde_json::from_str(r#"{"type":"condition","column":"a","op":"isNull"}"#).unwrap();
        assert_eq!(null, Filter::condition("a", FilterOp::IsNull, vec![]));
    }

    #[test]
    fn table_data_query() {
        let t = ObjectRef::table(Some("db"), Some("public"), "t");
        let mut q = TableDataQuery::new(t);
        q.sort = vec![SortKey::asc("a"), SortKey::desc("b")];
        assert!(q.validate().is_ok());
        q.sort.push(SortKey::asc("a"));
        assert!(q.validate().is_err());
        let back: TableDataQuery = serde_json::from_value(serde_json::json!({
            "table": {"kind": "table", "catalog": "db", "schema": "public", "name": "t"},
            "sort": [{"column": "a", "nulls": "last"}],
            "limit": 10
        }))
        .unwrap();
        assert_eq!(back.table.kind, ObjectKind::Table);
        assert_eq!(back.sort[0].nulls, Some(NullsOrder::Last));
        assert!(!back.sort[0].descending);
        assert_eq!(back.limit, Some(10));
    }
}
