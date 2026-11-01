//! The SQLite bulk loader (M5, `docs/m5-contracts.md` §4.3): batched multi-row `INSERT`s with
//! bound parameters, inside the import's transaction.
//!
//! - Each call is split into `INSERT INTO "main"."t" ("c1", ...) VALUES (?1, ...), (...)`
//!   statements of at most 32 766 parameters (`SQLITE_MAX_VARIABLE_NUMBER` of the bundled
//!   SQLite) and 10 000 rows, through the generic [`InsertLoader`] with
//!   [`BulkLimits::for_engine`]. All statements of a call run on the connection's worker thread
//!   inside the caller's transaction (`BEGIN` by the import service), so SQLite writes the
//!   journal once per commit, not per statement; a multi-row statement costs one parse for
//!   thousands of rows, about what a reused single-row prepared statement saves.
//! - Values bind as the driver maps them: `bool` as 0 / 1, decimals as their exact text (the
//!   column's affinity decides what SQLite stores, see below), dates, times and timestamps as
//!   ISO text (`timestamp_tz` with `+00:00`), UUIDs as hyphenated text, JSON as compact text,
//!   binary as blobs.
//! - **Decimals are exact only in `TEXT`-affinity columns** (a deviation from §4.5, recorded in
//!   §13 of the contracts). SQLite applies `NUMERIC` affinity to a column declared `DECIMAL`,
//!   `NUMERIC`, `MONEY`, ... and turns bound text that looks like a number into an `INTEGER` or
//!   a `REAL` (a double): `'1.10'` is stored as `1.1`, and `'-9999999999999999.9999'` keeps
//!   about 15 significant digits. No error is raised and no bind type avoids it (a blob would be
//!   kept, but as bytes), so the loader cannot prevent it. Tables an import creates declare
//!   longer decimals `DECIMAL_TEXT(p,s)` (`TEXT` affinity, `DdlContext::exact_values`), and
//!   [`BulkLoader::plan_notes`] warns when an existing target column has `NUMERIC` affinity and
//!   the values may have more than 15 digits.
//! - The token is checked before each statement; a running one is interrupted by the driver
//!   (`sqlite3_interrupt` and its progress handler), also while it waits for a lock held by
//!   another connection.

use std::sync::Arc;

use arrow_array::RecordBatch;
use async_trait::async_trait;
use oxbase_dialect::bulk::{BulkLimits, BulkLoader, BulkMethod, LoadSession, LoadTarget};
use oxbase_dialect::{AnsiQuoting, InsertLoader};
use oxbase_driver::CancellationToken;
use oxbase_types::{DataType, Engine, Result, ServerVersion};

use super::ddl::{EXACT_DECIMAL, NUMERIC_EXACT_DIGITS};
use crate::convert::{Affinity, affinity};

/// The SQLite [`BulkLoader`]: [`BulkMethod::MultiRowInsert`]. See the module docs.
pub struct SqliteBulkLoader {
    inner: InsertLoader,
    limits: BulkLimits,
}

impl SqliteBulkLoader {
    pub fn new() -> Self {
        let limits = BulkLimits::for_engine(Engine::Sqlite);
        Self {
            inner: InsertLoader::new(Arc::new(AnsiQuoting), limits),
            limits,
        }
    }
}

impl Default for SqliteBulkLoader {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BulkLoader for SqliteBulkLoader {
    fn method(&self, _version: &ServerVersion) -> BulkMethod {
        BulkMethod::MultiRowInsert
    }

    fn limits(&self, _version: &ServerVersion) -> BulkLimits {
        self.limits
    }

    /// A warning per decimal target column of `NUMERIC` affinity whose values may have more
    /// than 15 significant digits (the column's precision and the source's allow it).
    fn plan_notes(&self, target: &LoadTarget<'_>, sources: &[DataType]) -> Vec<String> {
        target
            .columns
            .iter()
            .zip(sources)
            .filter(|(column, source)| {
                let DataType::Decimal { precision, .. } = column.data_type else {
                    return false;
                };
                affinity(Some(&column.native_type)) == Affinity::Numeric
                    && precision.is_none_or(|p| p > NUMERIC_EXACT_DIGITS)
                    && source_digits(source).is_none_or(|d| d > NUMERIC_EXACT_DIGITS)
            })
            .map(|(column, _)| {
                format!(
                    "column {}: SQLite stores the decimals of a {} column (NUMERIC affinity) as \
                     numbers with about {NUMERIC_EXACT_DIGITS} significant digits; longer values \
                     lose digits without an error (a {EXACT_DECIMAL} or TEXT column keeps them)",
                    column.name,
                    column.native_type.trim()
                )
            })
            .collect()
    }

    async fn load(
        &self,
        session: LoadSession<'_>,
        target: &LoadTarget<'_>,
        batch: &RecordBatch,
        cancel: CancellationToken,
    ) -> Result<u64> {
        self.inner.load(session, target, batch, cancel).await
    }
}

/// Most significant digits a source column of type `source` holds as a decimal, `None` when
/// unknown. Integers and floats count as 0: SQLite stores integers exactly, and floats are
/// doubles already.
fn source_digits(source: &DataType) -> Option<u32> {
    match source {
        DataType::Decimal { precision, .. } => *precision,
        DataType::Int { .. } | DataType::Float { .. } | DataType::Bool => Some(0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_dialect::bulk::LoadColumn;
    use oxbase_types::ObjectRef;

    #[test]
    fn numeric_affinity_decimals_get_a_plan_note() {
        let decimal = |precision| DataType::Decimal {
            precision,
            scale: Some(2),
        };
        let column = |name: &str, data_type, native: &str| {
            let mut c = LoadColumn::new(name, data_type);
            native.clone_into(&mut c.native_type);
            c
        };
        let columns = vec![
            column("wide", decimal(Some(38)), "DECIMAL(38,2)"),
            column("narrow", decimal(Some(12)), "DECIMAL(12,2)"),
            column("exact", decimal(Some(38)), "DECIMAL_TEXT(38,2)"),
            column("ints", decimal(None), "NUMERIC"),
            column("open", decimal(None), "NUMERIC"),
            column("new", decimal(Some(38)), ""),
        ];
        let sources = vec![
            decimal(Some(20)),
            decimal(Some(20)),
            decimal(Some(20)),
            DataType::INT64,
            DataType::Text,
            decimal(Some(38)),
        ];
        let table = ObjectRef::table(Some("main"), None, "t");
        let version = ServerVersion::new(3, 50, 4);
        let target = LoadTarget {
            table: &table,
            columns: &columns,
            version: &version,
        };
        let notes = SqliteBulkLoader::new().plan_notes(&target, &sources);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].starts_with("column wide: "), "{notes:?}");
        assert!(notes[1].starts_with("column open: "), "{notes:?}");
    }

    #[test]
    fn limits_and_method() {
        let loader = SqliteBulkLoader::new();
        let version = ServerVersion::new(3, 50, 4);
        assert_eq!(loader.method(&version), BulkMethod::MultiRowInsert);
        let limits = loader.limits(&version);
        assert_eq!(limits.max_params, 32_766);
        assert_eq!(limits.rows_per_statement(14), 2_340);
        assert_eq!(limits.max_statement_bytes, None);
    }
}
