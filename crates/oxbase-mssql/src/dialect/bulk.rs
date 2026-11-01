//! The SQL Server bulk loader (M5, `docs/m5-contracts.md` §4.3): batched multi-row `INSERT`s
//! with bound parameters ([`BulkMethod::MultiRowInsert`]), inside the import's transaction.
//!
//! # Statements
//!
//! `INSERT INTO [schema].[table] ([c1], ...) VALUES (@P1, ...), (...)`, sent by the driver as
//! `sp_executesql` (`docs/m4-contracts.md` §2). One statement binds at most 2 098 parameters
//! (2 100 per RPC request, two of which are `sp_executesql`'s own `@stmt` and `@params`) and
//! at most 1 000 rows (the row-constructor limit), so a 14-column table takes 149 rows per
//! statement ([`BulkLimits::for_engine`]).
//!
//! A `VALUES` list gets one type per column, derived by SQL Server from every row's parameter
//! type (data type precedence), before the values are converted to the column types. Two
//! cases need help, so the loader:
//!
//! - wraps binary parameters in `CONVERT(varbinary(max), @Pn)`: a NULL parameter is declared
//!   `nvarchar(1)` by the driver, and `nvarchar` (higher precedence) would turn the whole
//!   column into text, which SQL Server refuses to convert to `varbinary` implicitly;
//! - pads decimal text to the same scale in every row of a column (the column's declared
//!   scale, or more when a value has more digits): the driver declares each decimal as
//!   `decimal(38, <its scale>)`, and a `VALUES` column mixing scales is typed with fewer
//!   decimal places than its widest value (precision is capped at 38), which would round
//!   values before they reach the table.
//!
//! # Identity columns
//!
//! [`BulkLoader::prepare`] looks the table up in `sys.columns` (`OBJECT_ID` of the quoted name,
//! bound as a parameter) and, when a loaded column is the identity column, runs
//! `SET IDENTITY_INSERT <table> ON` as a plain batch, so it holds for the session;
//! [`BulkLoader::finish`] turns it off again (SQL Server allows it on one table per session).
//!
//! # Why not TDS bulk load
//!
//! tiberius' `bulk_insert` (`INSERT BULK`) needs every value as the exact `ColumnData` variant
//! of the column's server type (an `int` column refuses an `I64`, a `datetime2(3)` needs its
//! scale, NULLs of `NOT NULL`-typed metadata are rejected, ...), and a half-sent bulk stream
//! cannot be abandoned in-band: a cancel would have to finish the stream (inserting what was
//! sent) or close the session. Parameterized `INSERT`s keep the server's own conversions,
//! errors per statement and the driver's attention-based cancel. The contract allows this as
//! the `multi_row_insert` method (§4.3, recorded in §13).
//!
//! # Cancel
//!
//! The token is checked before each statement; a running one is cancelled by the driver with a
//! TDS attention. The caller rolls back.

use std::ops::Range;
use std::sync::Arc;

use arrow_array::{Array, RecordBatch, StringArray};
use arrow_schema::DataType as ArrowType;
use async_trait::async_trait;
use oxbase_dialect::bulk::{
    BulkLimits, BulkLoader, BulkMethod, LoadSession, LoadTarget, bind_value,
};
use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{BracketQuoting, InsertLoader, Quoting};
use oxbase_driver::{CancellationToken, cell_value, collect};
use oxbase_types::{BoundStatement, DataType, Engine, Error, Result, ServerVersion, Value};

/// The identity column of a table (`@P1` = its quoted name).
const IDENTITY_SQL: &str = "SELECT c.name FROM sys.columns AS c \
     WHERE c.object_id = OBJECT_ID(@P1) AND c.is_identity = 1";

/// The SQL Server [`BulkLoader`]: [`BulkMethod::MultiRowInsert`]. See the module docs.
pub struct MsSqlBulkLoader {
    limits: BulkLimits,
    /// Only for [`InsertLoader::statement_ranges`] (the split by the limits).
    splitter: InsertLoader,
}

impl MsSqlBulkLoader {
    pub fn new() -> Self {
        let limits = BulkLimits::for_engine(Engine::MsSql);
        Self {
            limits,
            splitter: InsertLoader::new(Arc::new(BracketQuoting), limits),
        }
    }

    /// Row ranges of `batch`, one per statement, within the limits.
    pub fn statement_ranges(&self, batch: &RecordBatch) -> Vec<Range<usize>> {
        self.splitter.statement_ranges(batch)
    }

    /// The `INSERT` for rows `rows` of `batch` (load schema of `target`), with the binary and
    /// decimal handling of the module docs.
    pub fn insert_statement(
        target: &LoadTarget<'_>,
        batch: &RecordBatch,
        rows: Range<usize>,
    ) -> Result<BoundStatement> {
        if target.columns.is_empty() || rows.is_empty() || rows.end > batch.num_rows() {
            return Err(Error::invalid_input(format!(
                "cannot insert rows {rows:?} of a {}-row batch into {} columns",
                batch.num_rows(),
                target.columns.len()
            )));
        }
        let scales: Vec<Option<usize>> = batch
            .columns()
            .iter()
            .zip(target.columns)
            .map(|(array, column)| decimal_scale(array.as_ref(), &column.data_type, &rows))
            .collect();
        let mut w = SqlWriter::new(&BracketQuoting);
        w.push("INSERT INTO ").qualified(target.table).push(" (");
        for (pos, column) in target.columns.iter().enumerate() {
            if pos > 0 {
                w.push(", ");
            }
            w.ident(&column.name);
        }
        w.push(") VALUES ");
        for (n, row) in rows.enumerate() {
            w.push(if n == 0 { "(" } else { ", (" });
            for (pos, (array, column)) in batch.columns().iter().zip(target.columns).enumerate() {
                if pos > 0 {
                    w.push(", ");
                }
                let mut value = bind_value(array.as_ref(), row, column)?;
                if let (Value::Decimal(text), Some(scale)) = (&value, scales[pos]) {
                    value = Value::Decimal(pad_scale(text, scale));
                }
                if array.data_type() == &ArrowType::Binary {
                    w.push("CONVERT(varbinary(max), ").param(value).push(")");
                } else {
                    w.param(value);
                }
            }
            w.push(")");
        }
        Ok(w.finish())
    }
}

impl Default for MsSqlBulkLoader {
    fn default() -> Self {
        Self::new()
    }
}

/// Fraction digits of a plain decimal text (`-12.50` → 2); `None` for other spellings
/// (exponents), which are left alone.
fn fraction_digits(text: &str) -> Option<usize> {
    if text.bytes().any(|b| b == b'e' || b == b'E') {
        return None;
    }
    Some(text.split_once('.').map_or(0, |(_, frac)| frac.len()))
}

/// The scale every decimal of `rows` is padded to: the declared scale, or the most fraction
/// digits a value has, whichever is larger. `None` for non-decimal columns.
fn decimal_scale(array: &dyn Array, data_type: &DataType, rows: &Range<usize>) -> Option<usize> {
    let DataType::Decimal { scale, .. } = data_type else {
        return None;
    };
    let texts = array.as_any().downcast_ref::<StringArray>()?;
    let widest = rows
        .clone()
        .filter(|&row| !texts.is_null(row))
        .filter_map(|row| fraction_digits(texts.value(row)))
        .max()
        .unwrap_or(0);
    let declared = scale.map_or(0, |s| usize::try_from(s).unwrap_or(0));
    Some(widest.max(declared))
}

/// `text` with zeros appended up to `scale` fraction digits (never removes digits).
fn pad_scale(text: &str, scale: usize) -> String {
    let Some(have) = fraction_digits(text) else {
        return text.to_owned();
    };
    if have >= scale {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + scale - have + 1);
    out.push_str(text);
    if !text.contains('.') {
        out.push('.');
    }
    out.extend(std::iter::repeat_n('0', scale - have));
    out
}

/// `true` when a column of `target` is the table's identity column.
async fn loads_identity(
    session: LoadSession<'_>,
    target: &LoadTarget<'_>,
    cancel: CancellationToken,
) -> Result<bool> {
    if target.columns.is_empty() {
        return Ok(false);
    }
    let name = BracketQuoting.qualify(target.table);
    let found = collect(
        session
            .execute(IDENTITY_SQL, &[Value::Text(name)], cancel)
            .await?,
    )
    .await?;
    for set in &found.result_sets {
        for batch in &set.batches {
            if batch.num_columns() == 0 {
                continue;
            }
            let names = batch.column(0);
            for row in 0..batch.num_rows() {
                if let Value::Text(identity) = cell_value(names.as_ref(), row)?
                    && target
                        .columns
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(&identity))
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

async fn set_identity_insert(
    session: LoadSession<'_>,
    target: &LoadTarget<'_>,
    on: bool,
    cancel: CancellationToken,
) -> Result<()> {
    if !loads_identity(session, target, cancel.clone()).await? {
        return Ok(());
    }
    let sql = format!(
        "SET IDENTITY_INSERT {} {}",
        BracketQuoting.qualify(target.table),
        if on { "ON" } else { "OFF" }
    );
    session
        .run(&BoundStatement::new(sql, Vec::new()), cancel)
        .await
        .map(|_| ())
}

#[async_trait]
impl BulkLoader for MsSqlBulkLoader {
    fn method(&self, _version: &ServerVersion) -> BulkMethod {
        BulkMethod::MultiRowInsert
    }

    fn limits(&self, _version: &ServerVersion) -> BulkLimits {
        self.limits
    }

    async fn prepare(
        &self,
        session: LoadSession<'_>,
        target: &LoadTarget<'_>,
        cancel: CancellationToken,
    ) -> Result<()> {
        set_identity_insert(session, target, true, cancel).await
    }

    async fn finish(
        &self,
        session: LoadSession<'_>,
        target: &LoadTarget<'_>,
        cancel: CancellationToken,
    ) -> Result<()> {
        set_identity_insert(session, target, false, cancel).await
    }

    async fn load(
        &self,
        session: LoadSession<'_>,
        target: &LoadTarget<'_>,
        batch: &RecordBatch,
        cancel: CancellationToken,
    ) -> Result<u64> {
        target.check_batch(batch)?;
        let mut sent = 0_u64;
        for rows in self.statement_ranges(batch) {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let count = u64::try_from(rows.len()).unwrap_or(u64::MAX);
            let statement = Self::insert_statement(target, batch, rows)?;
            session.run(&statement, cancel.clone()).await?;
            sent += count;
        }
        Ok(sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{ArrayRef, BinaryArray, Int64Array};
    use oxbase_dialect::bulk::LoadColumn;
    use oxbase_types::ObjectRef;

    #[test]
    fn decimal_padding() {
        assert_eq!(fraction_digits("-12.50"), Some(2));
        assert_eq!(fraction_digits("7"), Some(0));
        assert_eq!(fraction_digits("1e5"), None);
        assert_eq!(pad_scale("7", 2), "7.00");
        assert_eq!(pad_scale("-1.5", 4), "-1.5000");
        assert_eq!(pad_scale("1.12345", 2), "1.12345");
        assert_eq!(pad_scale("3", 0), "3");
        assert_eq!(pad_scale("1E3", 2), "1E3");
    }

    #[test]
    fn statements_convert_binary_and_pad_decimals() {
        let table = ObjectRef::table(Some("db"), Some("dbo"), "t");
        let columns = vec![
            LoadColumn::new("id", DataType::INT64),
            LoadColumn::new(
                "amount",
                DataType::Decimal {
                    precision: Some(12),
                    scale: Some(2),
                },
            ),
            LoadColumn::new("raw", DataType::VarBinary { length: None }),
        ];
        let version = ServerVersion::new(17, 0, 1000);
        let target = LoadTarget {
            table: &table,
            columns: &columns,
            version: &version,
        };
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![1, 2, 3])),
            Arc::new(StringArray::from(vec![Some("1.5"), Some("2.125"), None])),
            Arc::new(BinaryArray::from(vec![
                Some(&b"\x01"[..]),
                None,
                Some(&b""[..]),
            ])),
        ];
        let batch = RecordBatch::try_new(target.schema(), arrays).unwrap();
        let st = MsSqlBulkLoader::insert_statement(&target, &batch, 0..3).unwrap();
        assert_eq!(
            st.sql,
            "INSERT INTO [dbo].[t] ([id], [amount], [raw]) VALUES \
             (@P1, @P2, CONVERT(varbinary(max), @P3)), \
             (@P4, @P5, CONVERT(varbinary(max), @P6)), \
             (@P7, @P8, CONVERT(varbinary(max), @P9))"
        );
        assert_eq!(st.params[1], Value::Decimal("1.500".into()));
        assert_eq!(st.params[4], Value::Decimal("2.125".into()));
        assert_eq!(st.params[7], Value::Null);
        assert_eq!(st.params[5], Value::Null);
        // Only rows 0..1: the declared scale (2) is the widest.
        let st = MsSqlBulkLoader::insert_statement(&target, &batch, 0..1).unwrap();
        assert_eq!(st.params[1], Value::Decimal("1.50".into()));
        assert!(MsSqlBulkLoader::insert_statement(&target, &batch, 2..2).is_err());
    }

    #[test]
    fn ranges_stay_under_the_parameter_limit() {
        let loader = MsSqlBulkLoader::new();
        let version = ServerVersion::new(17, 0, 1000);
        assert_eq!(loader.method(&version), BulkMethod::MultiRowInsert);
        let limits = loader.limits(&version);
        assert_eq!(limits.rows_per_statement(14), 149);
        assert_eq!(limits.rows_per_statement(1), 1_000);
        let table = ObjectRef::table(None, Some("dbo"), "t");
        let columns = vec![LoadColumn::new("id", DataType::INT64)];
        let target = LoadTarget {
            table: &table,
            columns: &columns,
            version: &version,
        };
        let ids: ArrayRef = Arc::new(Int64Array::from_iter_values(0..2_500));
        let batch = RecordBatch::try_new(target.schema(), vec![ids]).unwrap();
        assert_eq!(
            loader.statement_ranges(&batch),
            vec![0..1_000, 1_000..2_000, 2_000..2_500]
        );
    }
}
