//! Catalog queries: one statement with bound parameters, its rows as plain cells.
//!
//! Catalog columns differ in type between MySQL and MariaDB (and between versions): a count is
//! `BIGINT` on one and `INT UNSIGNED` on the other, a flag `VARCHAR(3)` or an integer. Cells are
//! therefore read loosely: any integer width becomes [`Cell::Int`], text and binary text become
//! [`Cell::Text`], and the getters on [`Row`] convert between the two.

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Decimal128Type, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type, UInt8Type,
    UInt16Type, UInt32Type, UInt64Type,
};
use arrow_array::{Array, RecordBatch};
use arrow_schema::DataType as ArrowType;
use oxbase_driver::{CancellationToken, Connection, collect};
use oxbase_types::{Error, Result, Value};

/// One catalog value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Cell {
    Null,
    Text(String),
    Int(i128),
    Float(f64),
}

/// One catalog row.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Row(pub(crate) Vec<Cell>);

impl Row {
    /// The cell as text (integers in decimal); `None` for NULL or a missing column.
    pub(crate) fn text(&self, i: usize) -> Option<String> {
        match self.0.get(i)? {
            Cell::Null => None,
            Cell::Text(s) => Some(s.clone()),
            Cell::Int(n) => Some(n.to_string()),
            Cell::Float(f) => Some(f.to_string()),
        }
    }

    /// The cell as text, `None` when NULL or empty.
    pub(crate) fn non_empty(&self, i: usize) -> Option<String> {
        self.text(i).filter(|s| !s.is_empty())
    }

    /// The cell as text, empty for NULL.
    pub(crate) fn string(&self, i: usize) -> String {
        self.text(i).unwrap_or_default()
    }

    /// The cell as an integer (text cells are parsed); `None` for NULL or non-numbers.
    pub(crate) fn int(&self, i: usize) -> Option<i128> {
        match self.0.get(i)? {
            Cell::Int(n) => Some(*n),
            Cell::Text(s) => s.trim().parse().ok(),
            Cell::Float(_) | Cell::Null => None,
        }
    }

    /// The cell as a `u32` (lengths, precisions, positions).
    pub(crate) fn u32(&self, i: usize) -> Option<u32> {
        self.int(i).and_then(|n| u32::try_from(n).ok())
    }

    /// The cell as a `u64` (row counts, sizes).
    pub(crate) fn u64(&self, i: usize) -> Option<u64> {
        self.int(i).and_then(|n| u64::try_from(n).ok())
    }

    /// `true` for `YES`, `Y`, `TRUE`, `ON` or a non-zero number (catalog flags).
    pub(crate) fn flag(&self, i: usize) -> bool {
        match self.0.get(i) {
            Some(Cell::Int(n)) => *n != 0,
            Some(Cell::Text(s)) => {
                let s = s.trim();
                ["YES", "Y", "TRUE", "ON", "1"]
                    .iter()
                    .any(|t| s.eq_ignore_ascii_case(t))
            }
            _ => false,
        }
    }
}

/// A text parameter.
pub(crate) fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

/// Runs one catalog statement with bound `params` and returns the rows of its first result set.
///
/// Honours `cancel`: a cancelled token yields `Error::Cancelled` and never a partial result.
pub(crate) async fn query(
    c: &dyn Connection,
    sql: &str,
    params: &[Value],
    cancel: &CancellationToken,
) -> Result<Vec<Row>> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let stream = c.execute(sql, params, cancel.clone()).await?;
    let collected = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(Error::Cancelled),
        out = collect(stream) => out?,
    };
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let mut rows = Vec::new();
    if let Some(set) = collected.result_sets.into_iter().next() {
        for batch in &set.batches {
            rows.extend(batch_rows(batch));
        }
    }
    Ok(rows)
}

/// The rows of one batch.
pub(crate) fn batch_rows(batch: &RecordBatch) -> Vec<Row> {
    (0..batch.num_rows())
        .map(|r| {
            Row(batch
                .columns()
                .iter()
                .map(|col| cell(col.as_ref(), r))
                .collect())
        })
        .collect()
}

fn cell(a: &dyn Array, row: usize) -> Cell {
    if a.is_null(row) {
        return Cell::Null;
    }
    match a.data_type() {
        ArrowType::Utf8 => Cell::Text(a.as_string::<i32>().value(row).to_owned()),
        ArrowType::LargeUtf8 => Cell::Text(a.as_string::<i64>().value(row).to_owned()),
        ArrowType::Binary => {
            Cell::Text(String::from_utf8_lossy(a.as_binary::<i32>().value(row)).into_owned())
        }
        ArrowType::LargeBinary => {
            Cell::Text(String::from_utf8_lossy(a.as_binary::<i64>().value(row)).into_owned())
        }
        ArrowType::Boolean => Cell::Int(i128::from(a.as_boolean().value(row))),
        ArrowType::Int8 => Cell::Int(a.as_primitive::<Int8Type>().value(row).into()),
        ArrowType::Int16 => Cell::Int(a.as_primitive::<Int16Type>().value(row).into()),
        ArrowType::Int32 => Cell::Int(a.as_primitive::<Int32Type>().value(row).into()),
        ArrowType::Int64 => Cell::Int(a.as_primitive::<Int64Type>().value(row).into()),
        ArrowType::UInt8 => Cell::Int(a.as_primitive::<UInt8Type>().value(row).into()),
        ArrowType::UInt16 => Cell::Int(a.as_primitive::<UInt16Type>().value(row).into()),
        ArrowType::UInt32 => Cell::Int(a.as_primitive::<UInt32Type>().value(row).into()),
        ArrowType::UInt64 => Cell::Int(a.as_primitive::<UInt64Type>().value(row).into()),
        ArrowType::Float32 => Cell::Float(f64::from(a.as_primitive::<Float32Type>().value(row))),
        ArrowType::Float64 => Cell::Float(a.as_primitive::<Float64Type>().value(row)),
        ArrowType::Decimal128(_, scale) => {
            decimal_cell(a.as_primitive::<Decimal128Type>().value(row), *scale)
        }
        _ => Cell::Null,
    }
}

/// A `Decimal128` value: an integer when the scale is 0, else its exact decimal text.
fn decimal_cell(v: i128, scale: i8) -> Cell {
    let Ok(scale) = usize::try_from(scale) else {
        return Cell::Null;
    };
    if scale == 0 {
        return Cell::Int(v);
    }
    let digits = v.unsigned_abs().to_string();
    let digits = if digits.len() <= scale {
        format!("{}{digits}", "0".repeat(scale + 1 - digits.len()))
    } else {
        digits
    };
    let (int, frac) = digits.split_at(digits.len() - scale);
    let sign = if v < 0 { "-" } else { "" };
    Cell::Text(format!("{sign}{int}.{frac}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow_array::{BinaryArray, Int32Array, StringArray, UInt64Array};
    use arrow_schema::{Field, Schema};

    #[test]
    fn reads_loose_cells() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("t", ArrowType::Utf8, true),
            Field::new("b", ArrowType::Binary, true),
            Field::new("i", ArrowType::Int32, true),
            Field::new("u", ArrowType::UInt64, true),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from(vec![Some("YES"), None])),
                Arc::new(BinaryArray::from(vec![Some(&b"x"[..]), Some(&b""[..])])),
                Arc::new(Int32Array::from(vec![Some(-3), Some(0)])),
                Arc::new(UInt64Array::from(vec![Some(u64::MAX), None])),
            ],
        )
        .unwrap();
        let rows = batch_rows(&batch);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].flag(0));
        assert!(!rows[1].flag(0));
        assert_eq!(rows[0].text(1).as_deref(), Some("x"));
        assert_eq!(rows[1].non_empty(1), None);
        assert_eq!(rows[0].int(2), Some(-3));
        assert_eq!(rows[0].u32(2), None);
        assert_eq!(rows[0].u64(3), Some(u64::MAX));
        assert_eq!(rows[1].text(3), None);
        assert_eq!(rows[1].string(3), "");
        assert_eq!(rows[0].text(9), None);
    }

    #[test]
    fn decimals() {
        assert_eq!(decimal_cell(12_345, 2), Cell::Text("123.45".into()));
        assert_eq!(decimal_cell(-5, 3), Cell::Text("-0.005".into()));
        assert_eq!(decimal_cell(7, 0), Cell::Int(7));
        assert_eq!(decimal_cell(7, -1), Cell::Null);
        let row = Row(vec![Cell::Text(" 42 ".into())]);
        assert_eq!(row.int(0), Some(42));
    }
}
