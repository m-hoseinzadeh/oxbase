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
//! # Typed parameters
//!
//! On a session of this driver the loader declares the parameters itself, one type per load
//! column (through the driver's crate-private `ParamSession`, reached with `as_any`):
//!
//! | Load type | Declared as | Value |
//! |---|---|---|
//! | `Boolean` | `bit` | `Bit` |
//! | `Int64` | `bigint` | `I64` |
//! | `UInt64` | `decimal(20, 0)` | `Numeric` |
//! | `Float64` | `float` | `F64` (NaN and infinities: `InvalidInput`) |
//! | `Utf8` of a `decimal` column | `decimal(38, s)`, `s` = the declared scale or the widest value's | `Numeric` rescaled to `s` (exact) |
//! | other `Utf8` | `nvarchar(max)` | `String` |
//! | `Binary` | `varbinary(max)` | `Binary` |
//! | `Date32` | `date` | `Date` |
//! | `Time64(µs)` | `time(7)` | `Time` |
//! | `Timestamp(µs)` | `datetime2(7)` | `DateTime2` |
//! | `Timestamp(µs, "+00:00")` | `datetimeoffset(7)` | `DateTimeOffset` at `+00:00` |
//! | `FixedSizeBinary(16)` | `uniqueidentifier` | `Guid` |
//!
//! NULLs keep their column's declaration, so every full-size statement of a load has the same
//! text and `@params`, and SQL Server compiles it once instead of once per statement (a
//! 149-row, 2 086-parameter `INSERT` takes far longer to compile than to run). It also gives
//! each `VALUES` column one type, which SQL Server otherwise derives from all rows' parameter
//! types (data type precedence) before converting to the column types.
//!
//! On another session (no `as_any`), the loader falls back to `Value` parameters as
//! [`Connection::execute`](oxbase_driver::Connection::execute) declares them, and then has to
//! wrap binary parameters in `CONVERT(varbinary(max), @Pn)` (a NULL is declared `nvarchar(1)`,
//! which would make the column text, and text does not convert to `varbinary` implicitly) and
//! pad decimal texts to one scale per column (a `VALUES` column mixing `decimal(38, s)` scales
//! is typed with fewer decimal places than its widest value, which would round).
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

use std::borrow::Cow;
use std::ops::Range;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Float64Type, Int64Type, Time64MicrosecondType, TimestampMicrosecondType, UInt64Type,
};
use arrow_array::{
    Array, BinaryArray, BooleanArray, Date32Array, FixedSizeBinaryArray, Float64Array, Int64Array,
    RecordBatch, StringArray, Time64MicrosecondArray, TimestampMicrosecondArray, UInt64Array,
};
use arrow_schema::{DataType as ArrowType, TimeUnit};
use async_trait::async_trait;
use futures::StreamExt as _;
use oxbase_dialect::bulk::{
    BulkLimits, BulkLoader, BulkMethod, LoadSession, LoadTarget, bind_value,
};
use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{BracketQuoting, InsertLoader, Quoting};
use oxbase_driver::{CancellationToken, cell_value, collect};
use oxbase_types::{BoundStatement, DataType, Engine, Error, Result, ServerVersion, Value};
use tiberius::ColumnData;
use tiberius::numeric::Numeric;
use tiberius::time::{Date, DateTime2, DateTimeOffset, Time};

use crate::driver::ParamSession;
use crate::driver::types::{Param, parse_decimal};

/// Days from 0001-01-01 (day 0 of `date`) to 1970-01-01.
const DAYS_0001_TO_EPOCH: i64 = 719_162;
/// Days from 0001-01-01 to 9999-12-31.
const MAX_SQL_DAY: i64 = 3_652_058;
const MICROS_PER_DAY: i64 = 86_400_000_000;

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

impl MsSqlBulkLoader {
    /// The `INSERT` for rows `rows` of `batch` with typed parameters (see the module docs): the
    /// SQL text and the declared parameters `@P1 ... @Pn`.
    pub(crate) fn typed_statement(
        target: &LoadTarget<'_>,
        batch: &RecordBatch,
        rows: Range<usize>,
    ) -> Result<(String, Vec<Param>)> {
        if target.columns.is_empty() || rows.is_empty() || rows.end > batch.num_rows() {
            return Err(Error::invalid_input(format!(
                "cannot insert rows {rows:?} of a {}-row batch into {} columns",
                batch.num_rows(),
                target.columns.len()
            )));
        }
        let columns = batch
            .columns()
            .iter()
            .zip(target.columns)
            .map(|(array, column)| Typed::new(array.as_ref(), &column.data_type, &rows))
            .collect::<Result<Vec<_>>>()?;
        let q = BracketQuoting;
        let mut sql = String::with_capacity(64 + rows.len() * columns.len() * 8);
        sql.push_str("INSERT INTO ");
        sql.push_str(&q.qualify(target.table));
        sql.push_str(" (");
        for (pos, column) in target.columns.iter().enumerate() {
            if pos > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&q.quote_ident(&column.name));
        }
        sql.push_str(") VALUES ");
        let mut params = Vec::with_capacity(rows.len() * columns.len());
        for (n, row) in rows.enumerate() {
            sql.push_str(if n == 0 { "(" } else { ", (" });
            for (pos, column) in columns.iter().enumerate() {
                if pos > 0 {
                    sql.push_str(", ");
                }
                params.push(Param {
                    decl: column.decl(),
                    data: column.value(row)?,
                });
                sql.push_str(&q.placeholder(params.len()));
            }
            sql.push(')');
        }
        Ok((sql, params))
    }
}

/// One load column, typed for parameters (see the module docs).
enum Typed<'a> {
    Bool(&'a BooleanArray),
    Int(&'a Int64Array),
    UInt(&'a UInt64Array),
    Float(&'a Float64Array),
    Decimal(&'a StringArray, u8),
    Text(&'a StringArray),
    Bytes(&'a BinaryArray),
    Date(&'a Date32Array),
    Time(&'a Time64MicrosecondArray),
    Timestamp(&'a TimestampMicrosecondArray),
    TimestampTz(&'a TimestampMicrosecondArray),
    Uuid(&'a FixedSizeBinaryArray),
}

impl<'a> Typed<'a> {
    fn new(array: &'a dyn Array, data_type: &DataType, rows: &Range<usize>) -> Result<Self> {
        let typed = match array.data_type() {
            ArrowType::Boolean => array.as_boolean_opt().map(Typed::Bool),
            ArrowType::Int64 => array.as_primitive_opt::<Int64Type>().map(Typed::Int),
            ArrowType::UInt64 => array.as_primitive_opt::<UInt64Type>().map(Typed::UInt),
            ArrowType::Float64 => array.as_primitive_opt::<Float64Type>().map(Typed::Float),
            ArrowType::Utf8 => match decimal_scale(array, data_type, rows) {
                Some(scale) => {
                    let scale = u8::try_from(scale).ok().filter(|s| *s <= 38).ok_or_else(|| {
                        Error::invalid_input(format!(
                            "decimal values with {scale} decimal places do not fit SQL Server's \
                             38 digits"
                        ))
                    })?;
                    array
                        .as_string_opt::<i32>()
                        .map(|a| Typed::Decimal(a, scale))
                }
                None => array.as_string_opt::<i32>().map(Typed::Text),
            },
            ArrowType::Binary => array.as_binary_opt::<i32>().map(Typed::Bytes),
            ArrowType::Date32 => array.as_primitive_opt::<Date32Type>().map(Typed::Date),
            ArrowType::Time64(TimeUnit::Microsecond) => array
                .as_primitive_opt::<Time64MicrosecondType>()
                .map(Typed::Time),
            ArrowType::Timestamp(TimeUnit::Microsecond, None) => array
                .as_primitive_opt::<TimestampMicrosecondType>()
                .map(Typed::Timestamp),
            ArrowType::Timestamp(TimeUnit::Microsecond, Some(_)) => array
                .as_primitive_opt::<TimestampMicrosecondType>()
                .map(Typed::TimestampTz),
            ArrowType::FixedSizeBinary(16) => array.as_fixed_size_binary_opt().map(Typed::Uuid),
            _ => None,
        };
        typed.ok_or_else(|| {
            Error::internal(format!(
                "SQL Server load cannot bind a {} column",
                array.data_type()
            ))
        })
    }

    fn decl(&self) -> String {
        match self {
            Self::Bool(_) => "bit".to_owned(),
            Self::Int(_) => "bigint".to_owned(),
            Self::UInt(_) => "decimal(20, 0)".to_owned(),
            Self::Float(_) => "float".to_owned(),
            Self::Decimal(_, scale) => format!("decimal(38, {scale})"),
            Self::Text(_) => "nvarchar(max)".to_owned(),
            Self::Bytes(_) => "varbinary(max)".to_owned(),
            Self::Date(_) => "date".to_owned(),
            Self::Time(_) => "time(7)".to_owned(),
            Self::Timestamp(_) => "datetime2(7)".to_owned(),
            Self::TimestampTz(_) => "datetimeoffset(7)".to_owned(),
            Self::Uuid(_) => "uniqueidentifier".to_owned(),
        }
    }

    fn value(&self, row: usize) -> Result<ColumnData<'static>> {
        Ok(match self {
            Self::Bool(a) => ColumnData::Bit(a.is_valid(row).then(|| a.value(row))),
            Self::Int(a) => ColumnData::I64(a.is_valid(row).then(|| a.value(row))),
            Self::UInt(a) => ColumnData::Numeric(
                a.is_valid(row)
                    .then(|| Numeric::new_with_scale(i128::from(a.value(row)), 0)),
            ),
            Self::Float(a) => ColumnData::F64(match a.is_valid(row).then(|| a.value(row)) {
                Some(v) if !v.is_finite() => {
                    return Err(Error::invalid_input(format!(
                        "SQL Server has no {v} float value"
                    )));
                }
                v => v,
            }),
            Self::Decimal(a, scale) => ColumnData::Numeric(if a.is_valid(row) {
                Some(rescaled(a.value(row), *scale)?)
            } else {
                None
            }),
            Self::Text(a) => {
                ColumnData::String(a.is_valid(row).then(|| Cow::Owned(a.value(row).to_owned())))
            }
            Self::Bytes(a) => {
                ColumnData::Binary(a.is_valid(row).then(|| Cow::Owned(a.value(row).to_vec())))
            }
            Self::Date(a) => ColumnData::Date(if a.is_valid(row) {
                Some(sql_date(i64::from(a.value(row)))?)
            } else {
                None
            }),
            Self::Time(a) => ColumnData::Time(if a.is_valid(row) {
                Some(sql_time(a.value(row))?)
            } else {
                None
            }),
            Self::Timestamp(a) => ColumnData::DateTime2(if a.is_valid(row) {
                Some(sql_datetime2(a.value(row))?)
            } else {
                None
            }),
            Self::TimestampTz(a) => ColumnData::DateTimeOffset(if a.is_valid(row) {
                Some(DateTimeOffset::new(sql_datetime2(a.value(row))?, 0))
            } else {
                None
            }),
            Self::Uuid(a) => ColumnData::Guid(if a.is_valid(row) {
                Some(
                    tiberius::Uuid::from_slice(a.value(row))
                        .map_err(|e| Error::internal(format!("bad UUID bytes: {e}")))?,
                )
            } else {
                None
            }),
        })
    }
}

/// Decimal `text` as a `Numeric` of exactly `scale` decimal places (never rounds: `scale` is at
/// least the text's own).
fn rescaled(text: &str, scale: u8) -> Result<Numeric> {
    let (value, have) = parse_decimal(text)?;
    let too_big = || {
        Error::invalid_input(format!(
            "'{text}' does not fit SQL Server's decimal(38, {scale})"
        ))
    };
    if have > scale {
        return Err(too_big());
    }
    let value = 10_i128
        .checked_pow(u32::from(scale - have))
        .and_then(|factor| value.checked_mul(factor))
        .ok_or_else(too_big)?;
    Ok(Numeric::new_with_scale(value, scale))
}

fn sql_date(days_from_epoch: i64) -> Result<Date> {
    let days = days_from_epoch + DAYS_0001_TO_EPOCH;
    if !(0..=MAX_SQL_DAY).contains(&days) {
        return Err(Error::invalid_input(format!(
            "SQL Server dates are within 0001-01-01 .. 9999-12-31 ({days_from_epoch} days from \
             1970 is not)"
        )));
    }
    Ok(Date::new(u32::try_from(days).unwrap_or_default()))
}

fn sql_time(micros: i64) -> Result<Time> {
    if !(0..MICROS_PER_DAY).contains(&micros) {
        return Err(Error::invalid_input(format!(
            "time of day out of range ({micros} µs)"
        )));
    }
    Ok(Time::new(u64::try_from(micros).unwrap_or_default() * 10, 7))
}

fn sql_datetime2(micros: i64) -> Result<DateTime2> {
    Ok(DateTime2::new(
        sql_date(micros.div_euclid(MICROS_PER_DAY))?,
        sql_time(micros.rem_euclid(MICROS_PER_DAY))?,
    ))
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
        let any = match session.tx {
            Some(tx) => tx.as_any(),
            None => session.connection.as_any(),
        };
        let typed = any.and_then(ParamSession::from_any);
        let mut sent = 0_u64;
        for rows in self.statement_ranges(batch) {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let count = u64::try_from(rows.len()).unwrap_or(u64::MAX);
            if let Some(params_session) = &typed {
                let (sql, params) = Self::typed_statement(target, batch, rows)?;
                let mut stream = params_session.execute(&sql, params, cancel.clone()).await?;
                while let Some(item) = stream.next().await {
                    item?;
                }
            } else {
                let statement = Self::insert_statement(target, batch, rows)?;
                session.run(&statement, cancel.clone()).await?;
            }
            sent += count;
        }
        Ok(sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::ArrayRef;
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
    fn typed_statements_declare_one_type_per_column() {
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
            Arc::new(Int64Array::from(vec![Some(1), None])),
            Arc::new(StringArray::from(vec![Some("1.5"), Some("-2.125")])),
            Arc::new(BinaryArray::from(vec![None, Some(&b"\x01"[..])])),
        ];
        let batch = RecordBatch::try_new(target.schema(), arrays).unwrap();
        let (sql, params) = MsSqlBulkLoader::typed_statement(&target, &batch, 0..2).unwrap();
        assert_eq!(
            sql,
            "INSERT INTO [dbo].[t] ([id], [amount], [raw]) VALUES (@P1, @P2, @P3), (@P4, @P5, @P6)"
        );
        let decls: Vec<&str> = params.iter().map(|p| p.decl.as_str()).collect();
        assert_eq!(
            decls,
            [
                "bigint",
                "decimal(38, 3)",
                "varbinary(max)",
                "bigint",
                "decimal(38, 3)",
                "varbinary(max)"
            ]
        );
        assert_eq!(
            format!("{:?}", params[1].data),
            format!(
                "{:?}",
                ColumnData::Numeric(Some(Numeric::new_with_scale(1_500, 3)))
            )
        );
        assert_eq!(
            format!("{:?}", params[3].data),
            format!("{:?}", ColumnData::I64(None))
        );
        assert!(rescaled("1.2345", 2).is_err());
        assert!(sql_date(-719_163).is_err());
        assert!(sql_time(MICROS_PER_DAY).is_err());
        assert_eq!(
            format!("{:?}", sql_datetime2(-1).unwrap()),
            format!(
                "{:?}",
                DateTime2::new(Date::new(719_161), Time::new(863_999_999_990, 7))
            )
        );
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
