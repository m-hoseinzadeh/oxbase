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
//!   kept, but as bytes), so the loader cannot prevent it; the import plan has to target a
//!   `TEXT` column for exact decimals, or warn when the target column has `NUMERIC` affinity.
//! - The token is checked before each statement; a running one is interrupted by the driver
//!   (`sqlite3_interrupt` and its progress handler), also while it waits for a lock held by
//!   another connection.

use std::sync::Arc;

use arrow_array::RecordBatch;
use async_trait::async_trait;
use oxbase_dialect::bulk::{BulkLimits, BulkLoader, BulkMethod, LoadSession, LoadTarget};
use oxbase_dialect::{AnsiQuoting, InsertLoader};
use oxbase_driver::CancellationToken;
use oxbase_types::{Engine, Result, ServerVersion};

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

#[cfg(test)]
mod tests {
    use super::*;

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
