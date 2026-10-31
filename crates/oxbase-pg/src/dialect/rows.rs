//! Runs a catalog query and reads its rows by column name.

use arrow_array::{
    Array, BooleanArray, Float32Array, Float64Array, Int16Array, Int32Array, Int64Array,
    LargeStringArray, StringArray, UInt32Array,
};
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
            Value::Text(s) => Ok(Some(s.as_str())),
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

    /// A text column; NULL reads as the empty string (catalog `"char"` flags).
    pub(crate) fn code(&self, column: &str) -> Result<&'a str> {
        Ok(self.opt_str(column)?.unwrap_or(""))
    }

    /// An integer column; `None` for NULL.
    pub(crate) fn opt_i64(&self, column: &str) -> Result<Option<i64>> {
        match self.get(column)? {
            Value::Null => Ok(None),
            Value::Int(n) => Ok(Some(*n)),
            other => Err(Self::mismatch(column, other, "an integer")),
        }
    }

    /// An integer column that must not be NULL.
    pub(crate) fn i64(&self, column: &str) -> Result<i64> {
        self.opt_i64(column)?
            .ok_or_else(|| Error::internal(format!("catalog column {column} is NULL")))
    }

    /// A boolean column; NULL reads as `false`.
    pub(crate) fn flag(&self, column: &str) -> Result<bool> {
        match self.get(column)? {
            Value::Null => Ok(false),
            Value::Bool(b) => Ok(*b),
            other => Err(Self::mismatch(column, other, "a boolean")),
        }
    }
}

/// Runs `sql` with `params` and gathers its rows.
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

/// The value at `row` of `col`, for the Arrow types catalog queries produce.
fn cell(col: &dyn Array, row: usize) -> Result<Value> {
    if col.is_null(row) {
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
    if let Some(a) = any.downcast_ref::<Int32Array>() {
        return Ok(Value::Int(i64::from(a.value(row))));
    }
    if let Some(a) = any.downcast_ref::<Int16Array>() {
        return Ok(Value::Int(i64::from(a.value(row))));
    }
    if let Some(a) = any.downcast_ref::<UInt32Array>() {
        return Ok(Value::Int(i64::from(a.value(row))));
    }
    if let Some(a) = any.downcast_ref::<BooleanArray>() {
        return Ok(Value::Bool(a.value(row)));
    }
    if let Some(a) = any.downcast_ref::<Float64Array>() {
        return Ok(Value::Float(a.value(row)));
    }
    if let Some(a) = any.downcast_ref::<Float32Array>() {
        return Ok(Value::Float(f64::from(a.value(row))));
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

    use arrow_array::RecordBatch;
    use arrow_schema::{DataType, Field, Schema};

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
            Field::new("small", DataType::Int16, true),
            Field::new("flag", DataType::Boolean, true),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from(vec![Some("a"), None])),
                Arc::new(Int64Array::from(vec![Some(7), None])),
                Arc::new(Int16Array::from(vec![Some(3), Some(-1)])),
                Arc::new(BooleanArray::from(vec![Some(true), None])),
            ],
        )
        .unwrap();
        let rows = rows_of(&batch);
        let first = rows.first().unwrap();
        assert_eq!(first.string("name").unwrap(), "a");
        assert_eq!(first.i64("n").unwrap(), 7);
        assert_eq!(first.i64("small").unwrap(), 3);
        assert!(first.flag("flag").unwrap());
        let second = rows.iter().nth(1).unwrap();
        assert_eq!(second.opt_str("name").unwrap(), None);
        assert_eq!(second.code("name").unwrap(), "");
        assert_eq!(second.opt_i64("n").unwrap(), None);
        assert!(second.string("name").is_err());
        assert!(!second.flag("flag").unwrap());
        assert!(first.opt_str("n").is_err());
        assert!(first.opt_str("missing").is_err());
    }
}
