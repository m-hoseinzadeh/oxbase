//! Runs a catalog query and reads its rows by column name.

use arrow_array::{Array, BooleanArray, Float64Array, Int64Array, LargeStringArray, StringArray};
use arrow_schema::DataType;
use oxbase_driver::{CancellationToken, Connection, collect};
use oxbase_types::{Error, Result, Value};

/// The rows of one catalog query, as [`Value`]s.
#[derive(Debug, Default)]
pub(crate) struct Rows {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

impl Rows {
    pub(crate) fn iter(&self) -> impl Iterator<Item = Row<'_>> {
        self.rows.iter().map(|values| Row {
            columns: &self.columns,
            values,
        })
    }

    pub(crate) fn first(&self) -> Option<Row<'_>> {
        self.iter().next()
    }
}

/// One row of [`Rows`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Row<'a> {
    columns: &'a [String],
    values: &'a [Value],
}

impl<'a> Row<'a> {
    fn get(&self, column: &str) -> Result<&'a Value> {
        self.columns
            .iter()
            .position(|c| c == column)
            .and_then(|i| self.values.get(i))
            .ok_or_else(|| Error::internal(format!("catalog query has no column {column}")))
    }

    fn mismatch(column: &str, value: &Value, want: &str) -> Error {
        Error::internal(format!(
            "catalog column {column} is {}, expected {want}",
            value.type_name()
        ))
    }

    /// A text column; `None` for NULL.
    pub(crate) fn opt_str(&self, column: &str) -> Result<Option<&'a str>> {
        match self.get(column)? {
            Value::Null => Ok(None),
            Value::Text(s) | Value::Decimal(s) => Ok(Some(s.as_str())),
            other => Err(Self::mismatch(column, other, "text")),
        }
    }

    /// A text column as an owned string; `None` for NULL.
    pub(crate) fn opt_string(&self, column: &str) -> Result<Option<String>> {
        Ok(self.opt_str(column)?.map(str::to_owned))
    }

    /// A text column that must not be NULL.
    pub(crate) fn string(&self, column: &str) -> Result<String> {
        self.opt_string(column)?
            .ok_or_else(|| Error::internal(format!("catalog column {column} is NULL")))
    }

    /// A text column, trimmed; NULL reads as the empty string (`char(2)` object types).
    pub(crate) fn code(&self, column: &str) -> Result<&'a str> {
        Ok(self.opt_str(column)?.unwrap_or("").trim())
    }

    /// An integer column; `None` for NULL.
    pub(crate) fn opt_i64(&self, column: &str) -> Result<Option<i64>> {
        match self.get(column)? {
            Value::Null => Ok(None),
            Value::Int(n) => Ok(Some(*n)),
            Value::Bool(b) => Ok(Some(i64::from(*b))),
            other => Err(Self::mismatch(column, other, "an integer")),
        }
    }

    /// An integer column that must not be NULL.
    pub(crate) fn i64(&self, column: &str) -> Result<i64> {
        self.opt_i64(column)?
            .ok_or_else(|| Error::internal(format!("catalog column {column} is NULL")))
    }

    /// A boolean (`bit`) column; NULL reads as `false`.
    pub(crate) fn flag(&self, column: &str) -> Result<bool> {
        match self.get(column)? {
            Value::Null => Ok(false),
            Value::Bool(b) => Ok(*b),
            Value::Int(n) => Ok(*n != 0),
            other => Err(Self::mismatch(column, other, "a boolean")),
        }
    }

    /// An exact integer written as text (`sql_variant` values cast to `nvarchar`: identity
    /// seeds, sequence bounds); `None` for NULL.
    pub(crate) fn opt_i128(&self, column: &str) -> Result<Option<i128>> {
        let Some(text) = self.opt_str(column)? else {
            return Ok(None);
        };
        parse_i128(text)
            .map(Some)
            .ok_or_else(|| Error::internal(format!("catalog column {column} is not an integer")))
    }
}

/// Parses an integer the server printed, allowing a zero fraction (`100`, `-5`, `100.0`).
pub(crate) fn parse_i128(text: &str) -> Option<i128> {
    let t = text.trim();
    let int = match t.split_once('.') {
        Some((int, frac)) if frac.bytes().all(|b| b == b'0') => int,
        Some(_) => return None,
        None => t,
    };
    int.parse().ok()
}

/// Runs `sql` with `params` and gathers its first result set.
///
/// `sql` is `'static`: catalog queries are fixed text (see `sql.rs`), names only travel as
/// `params`. Fails with `Error::Cancelled` (and no rows) once `cancel` fires.
pub(crate) async fn query(
    c: &dyn Connection,
    sql: &'static str,
    params: &[Value],
    cancel: &CancellationToken,
) -> Result<Rows> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let stream = c.execute(sql, params, cancel.clone()).await?;
    let collected = collect(stream).await?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let Some(set) = collected.result_sets.into_iter().next() else {
        return Ok(Rows::default());
    };
    let columns = set
        .schema
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    let mut rows = Vec::new();
    for batch in &set.batches {
        for row in 0..batch.num_rows() {
            let values = batch
                .columns()
                .iter()
                .map(|col| cell(col.as_ref(), row))
                .collect::<Result<Vec<_>>>()?;
            rows.push(values);
        }
    }
    Ok(Rows { columns, rows })
}

/// The value at `row` of `col`, for the Arrow types catalog queries produce (driver mapping,
/// docs/m4-contracts.md §2.5).
fn cell(col: &dyn Array, row: usize) -> Result<Value> {
    if col.data_type() == &DataType::Null || col.is_null(row) {
        return Ok(Value::Null);
    }
    let any = col.as_any();
    if let Some(a) = any.downcast_ref::<StringArray>() {
        return Ok(Value::Text(a.value(row).to_owned()));
    }
    if let Some(a) = any.downcast_ref::<LargeStringArray>() {
        return Ok(Value::Text(a.value(row).to_owned()));
    }
    if let Some(a) = any.downcast_ref::<Int64Array>() {
        return Ok(Value::Int(a.value(row)));
    }
    if let Some(a) = any.downcast_ref::<BooleanArray>() {
        return Ok(Value::Bool(a.value(row)));
    }
    if let Some(a) = any.downcast_ref::<Float64Array>() {
        return Ok(Value::Float(a.value(row)));
    }
    Err(Error::internal(format!(
        "unexpected catalog column type {}",
        col.data_type()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow_array::{NullArray, RecordBatch};
    use arrow_schema::{Field, Schema};

    fn rows_of(batch: &RecordBatch) -> Rows {
        let columns = batch
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect();
        let rows = (0..batch.num_rows())
            .map(|r| {
                batch
                    .columns()
                    .iter()
                    .map(|c| cell(c.as_ref(), r).unwrap())
                    .collect()
            })
            .collect();
        Rows { columns, rows }
    }

    #[test]
    fn reads_typed_cells_by_name() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("name", DataType::Utf8, true),
            Field::new("n", DataType::Int64, true),
            Field::new("flag", DataType::Boolean, true),
            Field::new("nothing", DataType::Null, true),
            Field::new("seed", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from(vec![Some("U "), None])),
                Arc::new(Int64Array::from(vec![Some(7), None])),
                Arc::new(BooleanArray::from(vec![Some(true), None])),
                Arc::new(NullArray::new(2)),
                Arc::new(StringArray::from(vec![Some("100"), Some("x")])),
            ],
        )
        .unwrap();
        let rows = rows_of(&batch);
        let first = rows.first().unwrap();
        assert_eq!(first.string("name").unwrap(), "U ");
        assert_eq!(first.code("name").unwrap(), "U");
        assert_eq!(first.i64("n").unwrap(), 7);
        assert!(first.flag("flag").unwrap());
        assert_eq!(first.opt_str("nothing").unwrap(), None);
        assert_eq!(first.opt_i128("seed").unwrap(), Some(100));
        let second = rows.iter().nth(1).unwrap();
        assert_eq!(second.opt_str("name").unwrap(), None);
        assert_eq!(second.code("name").unwrap(), "");
        assert_eq!(second.opt_i64("n").unwrap(), None);
        assert!(second.string("name").is_err());
        assert!(!second.flag("flag").unwrap());
        assert!(second.opt_i128("seed").is_err());
        assert!(first.opt_str("n").is_err());
        assert!(first.opt_str("missing").is_err());
    }

    #[test]
    fn parses_server_integers() {
        assert_eq!(parse_i128("100"), Some(100));
        assert_eq!(parse_i128(" -5 "), Some(-5));
        assert_eq!(parse_i128("100.000"), Some(100));
        assert_eq!(
            parse_i128("99999999999999999999999999999999999999"),
            Some(99_999_999_999_999_999_999_999_999_999_999_999_999)
        );
        assert_eq!(parse_i128("1.5"), None);
        assert_eq!(parse_i128("abc"), None);
    }
}
