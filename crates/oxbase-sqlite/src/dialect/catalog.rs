//! Catalog reads: `sqlite_schema` and the table-valued pragma functions, one query per helper,
//! with every object name passed as a bound parameter.
//!
//! # The one name that reaches SQL text
//!
//! A database's catalog table can only be named in the `FROM` clause (`aux.sqlite_schema`); SQLite
//! has no way to bind a schema name there. `main` and `temp` use fixed text. Any other database
//! name is first looked up with a bound parameter in `pragma_database_list`, and only the name
//! SQLite returns is written, quoted as an identifier with embedded quotes doubled
//! ([`AnsiQuoting`]). A name that is not an attached database never reaches SQL. Everything else
//! (tables, indexes, triggers, and the schema arguments of the pragma functions) is bound.

use std::mem;

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int32Type, Int64Type};
use arrow_array::{Array, RecordBatch};
use arrow_schema::DataType as ArrowType;
use oxbase_dialect::{AnsiQuoting, Quoting};
use oxbase_driver::{CancellationToken, Connection, collect};
use oxbase_types::{Error, Result, Value};

/// One decoded catalog value.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) enum Cell {
    #[default]
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// One catalog row.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Row(Vec<Cell>);

impl Row {
    /// Column `i` as text (numbers are formatted, blobs read as UTF-8); `None` for `NULL`.
    pub(crate) fn text(&self, i: usize) -> Option<String> {
        match self.0.get(i)? {
            Cell::Null => None,
            Cell::Int(v) => Some(v.to_string()),
            Cell::Real(v) => Some(v.to_string()),
            Cell::Text(s) => Some(s.clone()),
            Cell::Blob(b) => Some(String::from_utf8_lossy(b).into_owned()),
        }
    }

    /// Column `i` as text, `""` for `NULL`.
    pub(crate) fn string(&self, i: usize) -> String {
        self.text(i).unwrap_or_default()
    }

    /// Column `i` as an integer (integral reals and numeric text count).
    pub(crate) fn int(&self, i: usize) -> Option<i64> {
        match self.0.get(i)? {
            Cell::Int(v) => Some(*v),
            Cell::Real(v) => real_to_int(*v),
            Cell::Text(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// Column `i` as a flag: a non-zero integer.
    pub(crate) fn flag(&self, i: usize) -> bool {
        self.int(i).is_some_and(|v| v != 0)
    }
}

/// An integral real within the `i64` range as an integer.
#[allow(
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "only integral values well inside the i64 range are converted"
)]
fn real_to_int(v: f64) -> Option<i64> {
    (v.is_finite() && v.fract() == 0.0 && v.abs() < 9.0e18).then_some(v as i64)
}

fn cells(a: &dyn Array) -> Result<Vec<Cell>> {
    let n = a.len();
    let each = |value: &dyn Fn(usize) -> Cell| -> Vec<Cell> {
        (0..n)
            .map(|i| if a.is_null(i) { Cell::Null } else { value(i) })
            .collect()
    };
    Ok(match a.data_type() {
        ArrowType::Null => vec![Cell::Null; n],
        ArrowType::Int64 => {
            let v = a.as_primitive::<Int64Type>();
            each(&|i| Cell::Int(v.value(i)))
        }
        ArrowType::Int32 => {
            let v = a.as_primitive::<Int32Type>();
            each(&|i| Cell::Int(i64::from(v.value(i))))
        }
        ArrowType::Float64 => {
            let v = a.as_primitive::<Float64Type>();
            each(&|i| Cell::Real(v.value(i)))
        }
        ArrowType::Boolean => {
            let v = a.as_boolean();
            each(&|i| Cell::Int(i64::from(v.value(i))))
        }
        ArrowType::Utf8 => {
            let v = a.as_string::<i32>();
            each(&|i| Cell::Text(v.value(i).to_owned()))
        }
        ArrowType::LargeUtf8 => {
            let v = a.as_string::<i64>();
            each(&|i| Cell::Text(v.value(i).to_owned()))
        }
        ArrowType::Binary => {
            let v = a.as_binary::<i32>();
            each(&|i| Cell::Blob(v.value(i).to_vec()))
        }
        other => {
            return Err(Error::internal(format!(
                "unexpected Arrow type {other} in a SQLite catalog query"
            )));
        }
    })
}

fn decode(batch: &RecordBatch, out: &mut Vec<Row>) -> Result<()> {
    let mut columns = batch
        .columns()
        .iter()
        .map(|a| cells(a.as_ref()))
        .collect::<Result<Vec<_>>>()?;
    for r in 0..batch.num_rows() {
        out.push(Row(columns
            .iter_mut()
            .map(|col| mem::take(&mut col[r]))
            .collect()));
    }
    Ok(())
}

/// Fails with `Cancelled` once `cancel` is cancelled.
pub(crate) fn check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// A row of `sqlite_schema`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchemaRow {
    /// `table`, `view`, `index` or `trigger`.
    pub kind: String,
    pub name: String,
    pub tbl_name: String,
    /// `NULL` for automatic indexes.
    pub sql: Option<String>,
}

impl SchemaRow {
    fn from_row(r: &Row) -> Self {
        Self {
            kind: r.string(0),
            name: r.string(1),
            tbl_name: r.string(2),
            sql: r.text(3),
        }
    }
}

/// A row of `pragma_database_list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DatabaseRow {
    pub name: String,
    /// The file path; empty for in-memory and temporary databases.
    pub file: String,
}

/// Flags of a row of `pragma_table_list` (SQLite 3.37+).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TableFlags {
    /// `table`, `view`, `shadow` or `virtual`.
    pub kind: String,
    pub without_rowid: bool,
    pub strict: bool,
}

/// A row of `pragma_table_xinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnRow {
    pub name: String,
    pub decl: String,
    pub not_null: bool,
    pub default: Option<String>,
    /// 1-based position in the primary key, 0 when not part of it.
    pub pk: i64,
    /// 0 normal, 1 hidden (virtual tables), 2 generated virtual, 3 generated stored.
    pub hidden: i64,
}

/// A row of `pragma_index_list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexRow {
    pub name: String,
    pub unique: bool,
    /// `c` (`CREATE INDEX`), `u` (`UNIQUE` constraint) or `pk` (primary key).
    pub origin: String,
    pub partial: bool,
}

/// A key row of `pragma_index_xinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexKeyRow {
    /// Column number; -1 for the rowid, -2 for an expression.
    pub cid: i64,
    pub name: Option<String>,
    pub desc: bool,
}

/// A row of `pragma_foreign_key_list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForeignKeyRow {
    pub id: i64,
    pub table: String,
    pub from: String,
    /// `NULL` when the key references the parent's primary key implicitly.
    pub to: Option<String>,
    pub on_update: String,
    pub on_delete: String,
}

/// Catalog reads on one connection, all under one cancellation token.
pub(crate) struct Catalog<'a> {
    c: &'a dyn Connection,
    cancel: &'a CancellationToken,
}

impl<'a> Catalog<'a> {
    pub(crate) fn new(c: &'a dyn Connection, cancel: &'a CancellationToken) -> Self {
        Self { c, cancel }
    }

    /// Runs one catalog query and decodes its rows. Fails with `Cancelled` (and returns
    /// nothing) when the token is cancelled before or during the query.
    pub(crate) async fn rows(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        check(self.cancel)?;
        let got = collect(self.c.execute(sql, params, self.cancel.clone()).await?).await?;
        check(self.cancel)?;
        let mut rows = Vec::new();
        for set in &got.result_sets {
            for batch in &set.batches {
                decode(batch, &mut rows)?;
            }
        }
        Ok(rows)
    }

    /// `sqlite_schema` from SQLite 3.33, `sqlite_master` before (the same table).
    fn schema_table_name(&self) -> &'static str {
        if self.c.server_version().at_least(3, 33) {
            "sqlite_schema"
        } else {
            "sqlite_master"
        }
    }

    /// `true` from SQLite 3.37 (`pragma_table_list`, `STRICT` tables).
    pub(crate) fn has_table_list(&self) -> bool {
        self.c.server_version().at_least(3, 37)
    }

    /// The `FROM` target for the catalog table of database `schema`. See the module docs.
    async fn schema_from(&self, schema: &str) -> Result<String> {
        let table = self.schema_table_name();
        if schema.eq_ignore_ascii_case("main") {
            return Ok(format!("main.{table}"));
        }
        if schema.eq_ignore_ascii_case("temp") {
            return Ok(format!("temp.{table}"));
        }
        let rows = self
            .rows(
                "SELECT name FROM pragma_database_list WHERE name = ?1 COLLATE NOCASE",
                &[Value::from(schema)],
            )
            .await?;
        let name = rows
            .first()
            .and_then(|r| r.text(0))
            .ok_or_else(|| Error::not_found(format!("database {schema} is not attached")))?;
        Ok(format!("{}.{table}", AnsiQuoting.quote_ident(&name)))
    }

    /// The databases of the session: `main`, attached ones, and `temp` when it holds objects.
    pub(crate) async fn databases(&self) -> Result<Vec<DatabaseRow>> {
        let mut out: Vec<DatabaseRow> = self
            .rows(
                "SELECT name, file FROM pragma_database_list ORDER BY seq",
                &[],
            )
            .await?
            .iter()
            .map(|r| DatabaseRow {
                name: r.string(0),
                file: r.string(1),
            })
            .collect();
        let temp_sql = format!("SELECT count(*) FROM temp.{}", self.schema_table_name());
        let temp_used = self
            .rows(&temp_sql, &[])
            .await?
            .first()
            .and_then(|r| r.int(0))
            .is_some_and(|n| n > 0);
        out.retain(|d| temp_used || !d.name.eq_ignore_ascii_case("temp"));
        if temp_used && !out.iter().any(|d| d.name.eq_ignore_ascii_case("temp")) {
            out.push(DatabaseRow {
                name: "temp".to_owned(),
                file: String::new(),
            });
        }
        Ok(out)
    }

    /// The database `schema` as the session names it, `None` when it is not there.
    pub(crate) async fn database(&self, schema: &str) -> Result<Option<DatabaseRow>> {
        Ok(self
            .databases()
            .await?
            .into_iter()
            .find(|d| d.name.eq_ignore_ascii_case(schema)))
    }

    /// The encoding of the session's databases (`UTF-8`, `UTF-16le`, ...).
    pub(crate) async fn encoding(&self) -> Result<Option<String>> {
        Ok(self
            .rows("SELECT encoding FROM pragma_encoding", &[])
            .await?
            .first()
            .and_then(|r| r.text(0)))
    }

    /// `sqlite_schema` rows of `kind` in `schema`, sorted by name.
    pub(crate) async fn objects(&self, schema: &str, kind: &str) -> Result<Vec<SchemaRow>> {
        let sql = format!(
            "SELECT type, name, tbl_name, sql FROM {} WHERE type = ?1 ORDER BY name",
            self.schema_from(schema).await?
        );
        let rows = self.rows(&sql, &[Value::from(kind)]).await?;
        Ok(rows.iter().map(SchemaRow::from_row).collect())
    }

    /// `sqlite_schema` rows of `kind` in `schema` that belong to table or view `table`, sorted by
    /// name.
    pub(crate) async fn objects_on(
        &self,
        schema: &str,
        kind: &str,
        table: &str,
    ) -> Result<Vec<SchemaRow>> {
        let sql = format!(
            "SELECT type, name, tbl_name, sql FROM {} \
             WHERE type = ?1 AND tbl_name = ?2 COLLATE NOCASE ORDER BY name",
            self.schema_from(schema).await?
        );
        let rows = self
            .rows(&sql, &[Value::from(kind), Value::from(table)])
            .await?;
        Ok(rows.iter().map(SchemaRow::from_row).collect())
    }

    /// The `sqlite_schema` row named `name` in `schema` (names are case-insensitive), if its type
    /// is one of `kinds`.
    pub(crate) async fn object(
        &self,
        schema: &str,
        name: &str,
        kinds: &[&str],
    ) -> Result<Option<SchemaRow>> {
        let sql = format!(
            "SELECT type, name, tbl_name, sql FROM {} WHERE name = ?1 COLLATE NOCASE",
            self.schema_from(schema).await?
        );
        let rows = self.rows(&sql, &[Value::from(name)]).await?;
        Ok(rows
            .iter()
            .map(SchemaRow::from_row)
            .find(|r| kinds.contains(&r.kind.as_str())))
    }

    /// `pragma_table_list` flags of the tables and views of `schema`, keyed by lower-cased
    /// name. Empty before SQLite 3.37.
    pub(crate) async fn table_flags(&self, schema: &str) -> Result<Vec<(String, TableFlags)>> {
        if !self.has_table_list() {
            return Ok(Vec::new());
        }
        let rows = self
            .rows(
                "SELECT name, type, wr, strict FROM pragma_table_list \
                 WHERE schema = ?1 COLLATE NOCASE",
                &[Value::from(schema)],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.string(0).to_ascii_lowercase(),
                    TableFlags {
                        kind: r.string(1),
                        without_rowid: r.flag(2),
                        strict: r.flag(3),
                    },
                )
            })
            .collect())
    }

    /// Columns of table or view `table` in `schema`, in ordinal order (generated and hidden
    /// columns included from SQLite 3.26).
    pub(crate) async fn columns(&self, schema: &str, table: &str) -> Result<Vec<ColumnRow>> {
        let sql = if self.c.server_version().at_least(3, 26) {
            "SELECT name, type, \"notnull\", dflt_value, pk, hidden \
             FROM pragma_table_xinfo(?1, ?2) ORDER BY cid"
        } else {
            "SELECT name, type, \"notnull\", dflt_value, pk, 0 \
             FROM pragma_table_info(?1, ?2) ORDER BY cid"
        };
        let rows = self
            .rows(sql, &[Value::from(table), Value::from(schema)])
            .await?;
        Ok(rows
            .iter()
            .map(|r| ColumnRow {
                name: r.string(0),
                decl: r.string(1),
                not_null: r.flag(2),
                default: r.text(3),
                pk: r.int(4).unwrap_or(0),
                hidden: r.int(5).unwrap_or(0),
            })
            .collect())
    }

    /// Indexes of `table` in `schema` (including automatic and primary-key ones).
    pub(crate) async fn indexes(&self, schema: &str, table: &str) -> Result<Vec<IndexRow>> {
        let rows = self
            .rows(
                "SELECT name, \"unique\", origin, partial FROM pragma_index_list(?1, ?2)",
                &[Value::from(table), Value::from(schema)],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| IndexRow {
                name: r.string(0),
                unique: r.flag(1),
                origin: r.string(2),
                partial: r.flag(3),
            })
            .collect())
    }

    /// Key columns of index `index` in `schema`, in key order.
    pub(crate) async fn index_keys(&self, schema: &str, index: &str) -> Result<Vec<IndexKeyRow>> {
        let rows = self
            .rows(
                "SELECT cid, name, \"desc\" FROM pragma_index_xinfo(?1, ?2) \
                 WHERE \"key\" = 1 ORDER BY seqno",
                &[Value::from(index), Value::from(schema)],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| IndexKeyRow {
                cid: r.int(0).unwrap_or(0),
                name: r.text(1),
                desc: r.flag(2),
            })
            .collect())
    }

    /// Foreign-key rows of `table` in `schema`, by key id then column position.
    pub(crate) async fn foreign_keys(
        &self,
        schema: &str,
        table: &str,
    ) -> Result<Vec<ForeignKeyRow>> {
        let rows = self
            .rows(
                "SELECT id, \"table\", \"from\", \"to\", on_update, on_delete \
                 FROM pragma_foreign_key_list(?1, ?2) ORDER BY id, seq",
                &[Value::from(table), Value::from(schema)],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| ForeignKeyRow {
                id: r.int(0).unwrap_or(0),
                table: r.string(1),
                from: r.string(2),
                to: r.text(3),
                on_update: r.string(4),
                on_delete: r.string(5),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{BinaryArray, BooleanArray, Float64Array, Int64Array, StringArray};
    use arrow_schema::{Field, Schema};

    use super::*;

    #[test]
    fn decodes_every_catalog_column_type() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("i", ArrowType::Int64, true),
            Field::new("f", ArrowType::Float64, true),
            Field::new("s", ArrowType::Utf8, true),
            Field::new("b", ArrowType::Binary, true),
            Field::new("t", ArrowType::Boolean, true),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![Some(7), None])),
                Arc::new(Float64Array::from(vec![Some(2.0), Some(2.5)])),
                Arc::new(StringArray::from(vec![Some("12"), None])),
                Arc::new(BinaryArray::from(vec![Some(&b"ab"[..]), None])),
                Arc::new(BooleanArray::from(vec![Some(true), None])),
            ],
        )
        .unwrap();
        let mut rows = Vec::new();
        decode(&batch, &mut rows).unwrap();
        assert_eq!(rows.len(), 2);
        let r = &rows[0];
        assert_eq!(r.int(0), Some(7));
        assert_eq!(r.int(1), Some(2));
        assert_eq!(r.int(2), Some(12));
        assert_eq!(r.text(3).as_deref(), Some("ab"));
        assert!(r.flag(4));
        let r = &rows[1];
        assert_eq!(r.int(0), None);
        assert_eq!(r.int(1), None);
        assert_eq!(r.text(2), None);
        assert_eq!(r.string(3), "");
        assert!(!r.flag(4));
        assert_eq!(r.text(9), None);
    }
}
