//! Bulk loading (M5): the per-engine [`BulkLoader`] role that import uses to insert Arrow
//! batches fast, its batch-size rules, and the generic multi-row `INSERT` loader every engine
//! can fall back to.
//!
//! # Contract (`docs/m5-contracts.md` §4)
//!
//! - **Input.** The import service maps and converts source columns first, so a loader only
//!   ever sees batches in the *load schema* of its [`LoadTarget`]: one column per
//!   [`LoadColumn`], in that order, with the Arrow type [`load_arrow_type`] gives for the
//!   column's logical type ([`LoadTarget::schema`], checked by [`LoadTarget::check_batch`]).
//!   Values are already range- and NULL-checked against the target column; what is left to fail
//!   is the server (constraints, triggers, a value the engine refuses).
//! - **One call per batch.** [`BulkLoader::load`] inserts every row of one batch, or fails. The
//!   service re-chunks the source stream to about [`BulkLimits::rows_per_call`] rows per call
//!   and always calls inside a transaction it opened ([`LoadSession::tx`]), so a failed call
//!   rolls back with the transaction (on transactional storage). A loader never commits and never
//!   begins a transaction; session settings it needs go in [`BulkLoader::prepare`] (undone in
//!   [`BulkLoader::finish`]), which the service calls once on its dedicated import session.
//! - **Names and values.** Identifiers go through the engine's [`Quoting`]; values are bound
//!   parameters or travel through the engine's bulk protocol (PostgreSQL `COPY ... FROM STDIN`,
//!   SQL Server TDS bulk load). Never SQL literals.
//! - **Limits.** A statement never binds more than [`BulkLimits::max_params`] parameters nor
//!   more than [`BulkLimits::max_rows_per_statement`] rows ([`BulkLimits::rows_per_statement`]),
//!   and, where the engine caps statement size, stays under
//!   [`BulkLimits::max_statement_bytes`] (estimated from the bound values).
//! - **Cancel.** `load` checks the token before each statement / protocol message and returns
//!   `Error::Cancelled` as soon as it is cancelled; the caller then rolls back.
//! - **Native access.** A loader that needs its client library's API reaches it with
//!   [`Connection::as_any`] / [`oxbase_driver::Tx::as_any`] on the session it is given (never
//!   outside its engine crate).
//!
//! # Per-engine methods (load stream)
//!
//! | Engine | Method | Notes |
//! |---|---|---|
//! | PostgreSQL | [`BulkMethod::CopyBinary`] (or `CopyText`) | `COPY t (cols) FROM STDIN` through tokio-postgres `copy_in` on the session's client |
//! | MySQL / MariaDB | [`BulkMethod::MultiRowInsert`] | [`InsertLoader`]: `INSERT INTO t (cols) VALUES (?, ...), (...)`; no `LOAD DATA LOCAL INFILE` (§4.3) |
//! | SQLite | [`BulkMethod::PreparedInsert`] or `MultiRowInsert` | one prepared statement per call inside the service's transaction |
//! | SQL Server | [`BulkMethod::TdsBulkLoad`] (else `MultiRowInsert`) | tiberius `bulk_insert` (`INSERT BULK`), else 1000-row `VALUES` lists under 2100 parameters |
//!
//! # Default registration
//!
//! [`DialectRegistry::bulk_loader`](crate::DialectRegistry::bulk_loader) returns the engine's
//! registered loader, or else an [`InsertLoader`] built from the engine's [`Quoting`] and
//! [`BulkLimits::for_engine`], so import works on every SQL engine before (and without) a
//! native loader. [`DialectRegistry::has_bulk_loader`](crate::DialectRegistry::has_bulk_loader)
//! tells the two apart (the load stream's tests require a registered one).

use std::ops::Range;
use std::sync::Arc;

use arrow_array::{Array, RecordBatch};
use arrow_schema::{DataType as ArrowType, Field, Schema, SchemaRef, TimeUnit};
use async_trait::async_trait;
use futures::StreamExt;
use oxbase_driver::{CancellationToken, Connection, ResultItem, ResultStream, Tx, cell_value};
use oxbase_types::{
    BoundStatement, DataType, Engine, Error, ObjectRef, Result, ServerVersion, Value,
};
use serde::{Deserialize, Serialize};

use crate::Quoting;
use crate::sql::SqlWriter;

/// Bound parameters per statement: PostgreSQL (the protocol's 16-bit parameter count).
pub const POSTGRES_MAX_PARAMS: usize = 65_535;
/// Bound parameters per prepared statement: MySQL / MariaDB (16-bit count).
pub const MYSQL_MAX_PARAMS: usize = 65_535;
/// Bound parameters per statement: SQLite (`SQLITE_MAX_VARIABLE_NUMBER`, 32766 since 3.32; the
/// bundled SQLite is newer).
pub const SQLITE_MAX_PARAMS: usize = 32_766;
/// Parameters per RPC request: SQL Server. Parameterized statements go through
/// `sp_executesql`, whose own `@stmt` and `@params` count too, so a statement may bind
/// `MSSQL_MAX_PARAMS - 2`.
pub const MSSQL_MAX_PARAMS: usize = 2_100;
/// Rows per `INSERT ... VALUES` list: SQL Server's row-constructor limit.
pub const MSSQL_MAX_VALUES_ROWS: usize = 1_000;

/// How an engine's loader moves rows (reported in the import report and shown by the UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkMethod {
    /// PostgreSQL `COPY ... FROM STDIN` in text format.
    CopyText,
    /// PostgreSQL `COPY ... FROM STDIN (FORMAT binary)`.
    CopyBinary,
    /// `INSERT INTO t (...) VALUES (...), (...)` with bound parameters ([`InsertLoader`]).
    MultiRowInsert,
    /// One prepared single-row `INSERT` executed per row (SQLite).
    PreparedInsert,
    /// SQL Server TDS bulk load (`INSERT BULK`).
    TdsBulkLoad,
}

/// Batch-size rules of one engine. See the module docs, *Limits*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BulkLimits {
    /// Most bound parameters one statement may carry.
    pub max_params: usize,
    /// Most rows one multi-row statement may carry (whatever the parameter count allows).
    pub max_rows_per_statement: usize,
    /// Approximate cap on one statement's bound data, when the engine has one (MySQL
    /// `max_allowed_packet`).
    pub max_statement_bytes: Option<usize>,
    /// About how many rows the service hands to one [`BulkLoader::load`] call.
    pub rows_per_call: usize,
}

impl BulkLimits {
    /// Rows per call the service aims for when the engine names nothing else.
    pub const DEFAULT_ROWS_PER_CALL: usize = 50_000;

    /// The default limits of `engine` (what [`InsertLoader`] uses when the engine registered no
    /// loader of its own):
    ///
    /// | Engine | `max_params` | `max_rows_per_statement` | `max_statement_bytes` |
    /// |---|---|---|---|
    /// | PostgreSQL | 65535 | 10000 | - |
    /// | MySQL, MariaDB | 65535 | 10000 | 16 MiB (default `max_allowed_packet` is 64 MiB) |
    /// | SQLite | 32766 | 10000 | - |
    /// | SQL Server | 2098 | 1000 | - |
    /// | others | 999 | 1000 | - |
    pub fn for_engine(engine: Engine) -> Self {
        let (max_params, max_rows_per_statement, max_statement_bytes) = match engine {
            Engine::Postgres => (POSTGRES_MAX_PARAMS, 10_000, None),
            Engine::MySql | Engine::MariaDb => (MYSQL_MAX_PARAMS, 10_000, Some(16 * 1024 * 1024)),
            Engine::Sqlite => (SQLITE_MAX_PARAMS, 10_000, None),
            Engine::MsSql => (MSSQL_MAX_PARAMS - 2, MSSQL_MAX_VALUES_ROWS, None),
            _ => (999, 1_000, None),
        };
        Self {
            max_params,
            max_rows_per_statement,
            max_statement_bytes,
            rows_per_call: Self::DEFAULT_ROWS_PER_CALL,
        }
    }

    /// Rows one multi-row statement may carry for a table of `columns` columns: at least 1, at
    /// most [`BulkLimits::max_rows_per_statement`], and never more than
    /// `max_params / columns`.
    pub fn rows_per_statement(&self, columns: usize) -> usize {
        let by_params = self.max_params / columns.max(1);
        by_params.min(self.max_rows_per_statement).max(1)
    }
}

/// One target column of a load, in load order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadColumn {
    pub name: String,
    /// The column's logical type (from the loaded `TableDef`, or the type the import created it
    /// with).
    pub data_type: DataType,
    /// The engine's own type text (`character varying(20)`), empty when unknown. Loaders that
    /// must name a type (PostgreSQL binary `COPY`, SQL Server bulk metadata) use it.
    pub native_type: String,
    pub nullable: bool,
}

impl LoadColumn {
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
            native_type: String::new(),
            nullable: true,
        }
    }
}

/// What a [`BulkLoader`] writes to.
#[derive(Debug, Clone, Copy)]
pub struct LoadTarget<'a> {
    /// The table (a `Table` ref as the object tree lists it).
    pub table: &'a ObjectRef,
    /// The columns the batches fill, in batch column order. Columns of the table that are not
    /// listed get their defaults.
    pub columns: &'a [LoadColumn],
    pub version: &'a ServerVersion,
}

impl LoadTarget<'_> {
    /// The load schema: one field per column, typed with [`load_arrow_type`], nullable as the
    /// column is.
    pub fn schema(&self) -> SchemaRef {
        Arc::new(Schema::new(
            self.columns
                .iter()
                .map(|c| Field::new(&c.name, load_arrow_type(&c.data_type), c.nullable))
                .collect::<Vec<_>>(),
        ))
    }

    /// `Internal` unless `batch` has exactly the load schema's column count and Arrow types
    /// (names and nullability are not compared).
    pub fn check_batch(&self, batch: &RecordBatch) -> Result<()> {
        if batch.num_columns() != self.columns.len() {
            return Err(Error::internal(format!(
                "load batch has {} columns, the target {}",
                batch.num_columns(),
                self.columns.len()
            )));
        }
        for (array, column) in batch.columns().iter().zip(self.columns) {
            let expected = load_arrow_type(&column.data_type);
            if array.data_type() != &expected {
                return Err(Error::internal(format!(
                    "load column '{}' is {}, expected {expected}",
                    column.name,
                    array.data_type()
                )));
            }
        }
        Ok(())
    }
}

/// The Arrow type a load batch carries for a column of logical type `data_type`:
///
/// | Logical type | Arrow type |
/// |---|---|
/// | `bool` | `Boolean` |
/// | `int` (any width, signed or unsigned below 64 bits) | `Int64` |
/// | `int` unsigned 64-bit | `UInt64` |
/// | `float` | `Float64` |
/// | `decimal` | `Utf8` (exact decimal text, `-123.45`) |
/// | `char`, `var_char`, `text`, `enum`, `interval`, `array`, `other` | `Utf8` (engine text form) |
/// | `json` | `Utf8` (JSON text) |
/// | `binary`, `var_binary` | `Binary` |
/// | `date` | `Date32` |
/// | `time` | `Time64(Microsecond)` |
/// | `timestamp` | `Timestamp(Microsecond, None)` |
/// | `timestamp_tz` | `Timestamp(Microsecond, Some("+00:00"))` (the instant in UTC) |
/// | `uuid` | `FixedSizeBinary(16)` |
pub fn load_arrow_type(data_type: &DataType) -> ArrowType {
    match data_type {
        DataType::Bool => ArrowType::Boolean,
        DataType::Int {
            bits: 64,
            unsigned: true,
        } => ArrowType::UInt64,
        DataType::Int { .. } => ArrowType::Int64,
        DataType::Float { .. } => ArrowType::Float64,
        DataType::Binary { .. } | DataType::VarBinary { .. } => ArrowType::Binary,
        DataType::Date => ArrowType::Date32,
        DataType::Time { .. } => ArrowType::Time64(TimeUnit::Microsecond),
        DataType::Timestamp { .. } => ArrowType::Timestamp(TimeUnit::Microsecond, None),
        DataType::TimestampTz { .. } => {
            ArrowType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into()))
        }
        DataType::Uuid => ArrowType::FixedSizeBinary(16),
        DataType::Decimal { .. }
        | DataType::Char { .. }
        | DataType::VarChar { .. }
        | DataType::Text
        | DataType::Interval
        | DataType::Json { .. }
        | DataType::Array { .. }
        | DataType::Enum { .. }
        | DataType::Other { .. } => ArrowType::Utf8,
    }
}

/// Row `row` of a load-schema column as the [`Value`] to bind for `column`: [`cell_value`],
/// with decimal text as `Value::Decimal` and JSON text as `Value::Json` (`InvalidInput` when it
/// is not JSON). Everything else is `cell_value` unchanged.
pub fn bind_value(array: &dyn Array, row: usize, column: &LoadColumn) -> Result<Value> {
    let value = cell_value(array, row)?;
    Ok(match (value, &column.data_type) {
        (Value::Text(text), DataType::Decimal { .. }) => Value::Decimal(text),
        (Value::Text(text), DataType::Json { .. }) => {
            Value::Json(serde_json::from_str(&text).map_err(|e| {
                Error::invalid_input(format!("column '{}': not JSON: {e}", column.name))
            })?)
        }
        (value, _) => value,
    })
}

/// Where a load runs: the session, and the transaction the service opened on it.
#[derive(Clone, Copy)]
pub struct LoadSession<'a> {
    pub connection: &'a dyn Connection,
    /// The open transaction; statements run through it when set.
    pub tx: Option<&'a dyn Tx>,
}

impl<'a> LoadSession<'a> {
    pub fn new(connection: &'a dyn Connection, tx: Option<&'a dyn Tx>) -> Self {
        Self { connection, tx }
    }

    /// Runs `sql` through the transaction when there is one, else on the connection.
    pub async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        match self.tx {
            Some(tx) => tx.execute(sql, params, cancel).await,
            None => self.connection.execute(sql, params, cancel).await,
        }
    }

    /// Runs `statement` to its end and returns the sum of its `RowsAffected` items (rows of a
    /// result set are read and dropped). Fails with the first error the stream yields.
    pub async fn run(&self, statement: &BoundStatement, cancel: CancellationToken) -> Result<u64> {
        let mut stream = self
            .execute(&statement.sql, &statement.params, cancel)
            .await?;
        let mut affected = 0_u64;
        while let Some(item) = stream.next().await {
            if let ResultItem::RowsAffected(n) = item? {
                affected = affected.saturating_add(n);
            }
        }
        Ok(affected)
    }
}

/// Inserts load batches into a table, fast. One implementation per engine, registered with
/// [`DialectRegistry::register_bulk_loader`](crate::DialectRegistry::register_bulk_loader).
/// See the module docs for the full contract.
#[async_trait]
pub trait BulkLoader: Send + Sync {
    /// The method this loader uses on server version `version`.
    fn method(&self, version: &ServerVersion) -> BulkMethod;

    /// The batch-size rules on server version `version`.
    fn limits(&self, version: &ServerVersion) -> BulkLimits;

    /// Called once by the import service on its dedicated session before the first
    /// [`BulkLoader::load`], outside any transaction. For session settings the loads need
    /// (MySQL `SET time_zone = '+00:00'` so `TIMESTAMP` columns take UTC instants; SQL Server
    /// `SET IDENTITY_INSERT <table> ON` when an identity column is loaded). The session is
    /// closed after the import, so nothing here outlives it. Default: nothing.
    async fn prepare(
        &self,
        session: LoadSession<'_>,
        target: &LoadTarget<'_>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let _ = (session, target, cancel);
        Ok(())
    }

    /// Called once after the last [`BulkLoader::load`] (also after a failure or a cancel, best
    /// effort, errors ignored there), outside any transaction, to undo [`BulkLoader::prepare`]
    /// where that matters (SQL Server allows `IDENTITY_INSERT` on one table per session).
    /// Default: nothing.
    async fn finish(
        &self,
        session: LoadSession<'_>,
        target: &LoadTarget<'_>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let _ = (session, target, cancel);
        Ok(())
    }

    /// Inserts every row of `batch` (in the load schema of `target`) into `target.table` on
    /// `session` and returns the number of rows sent. Fails as a whole; the caller rolls the
    /// transaction back. `Error::Cancelled` once `cancel` fires.
    async fn load(
        &self,
        session: LoadSession<'_>,
        target: &LoadTarget<'_>,
        batch: &RecordBatch,
        cancel: CancellationToken,
    ) -> Result<u64>;
}

/// The generic loader: multi-row `INSERT INTO t (c1, ...) VALUES (?, ...), (...)` with every
/// value bound ([`bind_value`]), split by [`BulkLimits::rows_per_statement`] and
/// [`BulkLimits::max_statement_bytes`]. Works on every SQL engine; MySQL / MariaDB use it as
/// their loader, the others as the fallback of
/// [`DialectRegistry::bulk_loader`](crate::DialectRegistry::bulk_loader).
pub struct InsertLoader {
    quoting: Arc<dyn Quoting>,
    limits: BulkLimits,
}

impl InsertLoader {
    pub fn new(quoting: Arc<dyn Quoting>, limits: BulkLimits) -> Self {
        Self { quoting, limits }
    }

    /// The `INSERT` for rows `rows` of `batch` (load schema of `target`). `InvalidInput` for an
    /// empty range or a target without columns.
    pub fn insert_statement(
        &self,
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
        let mut w = SqlWriter::new(self.quoting.as_ref());
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
                w.param(bind_value(array.as_ref(), row, column)?);
            }
            w.push(")");
        }
        Ok(w.finish())
    }

    /// Row ranges of `batch`, one per statement, within the limits.
    pub fn statement_ranges(&self, batch: &RecordBatch) -> Vec<Range<usize>> {
        let per_statement = self.limits.rows_per_statement(batch.num_columns());
        let mut out = Vec::new();
        let mut start = 0;
        let mut bytes = 0_usize;
        for row in 0..batch.num_rows() {
            let row_bytes = if self.limits.max_statement_bytes.is_some() {
                estimated_row_bytes(batch, row)
            } else {
                0
            };
            let full_by_rows = row - start >= per_statement;
            let full_by_bytes = self
                .limits
                .max_statement_bytes
                .is_some_and(|cap| row > start && bytes + row_bytes > cap);
            if full_by_rows || full_by_bytes {
                out.push(start..row);
                start = row;
                bytes = 0;
            }
            bytes += row_bytes;
        }
        if start < batch.num_rows() {
            out.push(start..batch.num_rows());
        }
        out
    }
}

/// Rough wire size of one row's bound values (data bytes plus a small per-value overhead).
fn estimated_row_bytes(batch: &RecordBatch, row: usize) -> usize {
    batch
        .columns()
        .iter()
        .map(|array| {
            let data = if array.is_null(row) {
                0
            } else {
                match array.data_type() {
                    ArrowType::Utf8 => array
                        .as_any()
                        .downcast_ref::<arrow_array::StringArray>()
                        .map_or(0, |a| a.value(row).len()),
                    ArrowType::Binary => array
                        .as_any()
                        .downcast_ref::<arrow_array::BinaryArray>()
                        .map_or(0, |a| a.value(row).len()),
                    _ => 16,
                }
            };
            data + 8
        })
        .sum()
}

#[async_trait]
impl BulkLoader for InsertLoader {
    fn method(&self, _version: &ServerVersion) -> BulkMethod {
        BulkMethod::MultiRowInsert
    }

    fn limits(&self, _version: &ServerVersion) -> BulkLimits {
        self.limits
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
            let statement = self.insert_statement(target, batch, rows)?;
            session.run(&statement, cancel.clone()).await?;
            sent += count;
        }
        Ok(sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BracketQuoting, PgQuoting};
    use arrow_array::{ArrayRef, Int64Array, StringArray};
    use pretty_assertions::assert_eq;

    fn columns() -> Vec<LoadColumn> {
        vec![
            LoadColumn::new("id", DataType::INT64),
            LoadColumn::new(
                "amount",
                DataType::Decimal {
                    precision: Some(12),
                    scale: Some(2),
                },
            ),
            LoadColumn::new("doc", DataType::Json { binary: true }),
        ]
    }

    fn batch(target: &LoadTarget<'_>, rows: usize) -> RecordBatch {
        let ids: ArrayRef = Arc::new(Int64Array::from_iter_values(
            0..i64::try_from(rows).unwrap(),
        ));
        let amounts: ArrayRef = Arc::new(StringArray::from_iter_values(
            (0..rows).map(|i| format!("{i}.50")),
        ));
        let docs: ArrayRef = Arc::new(StringArray::from_iter(
            (0..rows).map(|i| (i % 2 == 0).then(|| format!("{{\"n\":{i}}}"))),
        ));
        RecordBatch::try_new(target.schema(), vec![ids, amounts, docs]).unwrap()
    }

    #[test]
    fn limits_per_engine() {
        let pg = BulkLimits::for_engine(Engine::Postgres);
        assert_eq!(pg.rows_per_statement(3), 10_000);
        assert_eq!(pg.rows_per_statement(10), 6_553);
        let mssql = BulkLimits::for_engine(Engine::MsSql);
        assert_eq!(mssql.max_params, 2_098);
        assert_eq!(mssql.rows_per_statement(1), 1_000);
        assert_eq!(mssql.rows_per_statement(9), 233);
        assert_eq!(mssql.rows_per_statement(5_000), 1);
        assert_eq!(
            BulkLimits::for_engine(Engine::Sqlite).rows_per_statement(4),
            8_191
        );
        assert!(
            BulkLimits::for_engine(Engine::MySql)
                .max_statement_bytes
                .is_some()
        );
    }

    #[test]
    fn load_types() {
        assert_eq!(load_arrow_type(&DataType::INT32), ArrowType::Int64);
        assert_eq!(
            load_arrow_type(&DataType::Int {
                bits: 64,
                unsigned: true
            }),
            ArrowType::UInt64
        );
        assert_eq!(
            load_arrow_type(&DataType::TimestampTz { precision: None }),
            ArrowType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into()))
        );
        assert_eq!(
            load_arrow_type(&DataType::Uuid),
            ArrowType::FixedSizeBinary(16)
        );
        assert_eq!(load_arrow_type(&DataType::Text), ArrowType::Utf8);
    }

    #[test]
    fn insert_statements_bind_every_value() {
        let table = ObjectRef::table(None, Some("s"), "t");
        let cols = columns();
        let version = ServerVersion::new(18, 0, 0);
        let target = LoadTarget {
            table: &table,
            columns: &cols,
            version: &version,
        };
        let b = batch(&target, 2);
        target.check_batch(&b).unwrap();
        let loader = InsertLoader::new(
            Arc::new(PgQuoting),
            BulkLimits::for_engine(Engine::Postgres),
        );
        let st = loader.insert_statement(&target, &b, 0..2).unwrap();
        assert_eq!(
            st.sql,
            "INSERT INTO \"s\".\"t\" (\"id\", \"amount\", \"doc\") VALUES ($1, $2, $3), ($4, $5, $6)"
        );
        assert_eq!(st.params[1], Value::Decimal("0.50".into()));
        assert_eq!(st.params[2], Value::Json(serde_json::json!({"n": 0})));
        assert_eq!(st.params[5], Value::Null);
        assert!(loader.insert_statement(&target, &b, 1..1).is_err());

        let bad = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("id", ArrowType::Int64, true)])),
            vec![Arc::new(Int64Array::from(vec![1])) as ArrayRef],
        )
        .unwrap();
        assert!(target.check_batch(&bad).is_err());
    }

    #[test]
    fn statements_split_by_parameter_limit() {
        let table = ObjectRef::table(None, Some("dbo"), "t");
        let cols = columns();
        let version = ServerVersion::new(17, 0, 5035);
        let target = LoadTarget {
            table: &table,
            columns: &cols,
            version: &version,
        };
        let b = batch(&target, 1_500);
        let loader = InsertLoader::new(
            Arc::new(BracketQuoting),
            BulkLimits::for_engine(Engine::MsSql),
        );
        // 2098 / 3 = 699 rows per statement.
        assert_eq!(
            loader.statement_ranges(&b),
            vec![0..699, 699..1_398, 1_398..1_500]
        );
        let st = loader.insert_statement(&target, &b, 0..699).unwrap();
        assert_eq!(st.params.len(), 2_097);
        assert!(st.sql.ends_with("@P2095, @P2096, @P2097)"));
    }

    #[test]
    fn statements_split_by_bytes() {
        let table = ObjectRef::table(Some("db"), None, "t");
        let cols = vec![LoadColumn::new("note", DataType::Text)];
        let version = ServerVersion::new(8, 4, 13);
        let target = LoadTarget {
            table: &table,
            columns: &cols,
            version: &version,
        };
        let text: ArrayRef = Arc::new(StringArray::from_iter_values(
            (0..10).map(|_| "x".repeat(92)),
        ));
        let b = RecordBatch::try_new(target.schema(), vec![text]).unwrap();
        let mut limits = BulkLimits::for_engine(Engine::MySql);
        limits.max_statement_bytes = Some(300);
        let loader = InsertLoader::new(Arc::new(crate::BacktickQuoting), limits);
        // 100 estimated bytes per row: 3 rows per statement.
        assert_eq!(loader.statement_ranges(&b), vec![0..3, 3..6, 6..9, 9..10]);
    }
}
