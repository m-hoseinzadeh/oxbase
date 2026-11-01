//! The MySQL / MariaDB bulk loader (M5, `docs/m5-contracts.md` §4.3): batched multi-row
//! `INSERT`s with bound parameters.
//!
//! - Each call is split into `INSERT INTO t (c1, ...) VALUES (?, ...), (...)` statements of at
//!   most 65 535 placeholders (the protocol's 16-bit parameter count), 10 000 rows and about
//!   16 MiB of bound data (the server's default `max_allowed_packet` is 64 MiB), through the
//!   generic [`InsertLoader`] with [`BulkLimits::for_engine`]. Values are bound in the binary
//!   protocol (`crate::types::to_param`): decimals and JSON as text, `bool` as 0 / 1, UUIDs
//!   as text (MariaDB `uuid`), instants in UTC.
//! - [`BulkLoader::prepare`] sets the session time zone to UTC, so `TIMESTAMP` columns store
//!   the instants of `timestamp_tz` load columns exactly. The import session is closed after
//!   the import, so the setting does not outlive it.
//! - **No `LOAD DATA LOCAL INFILE`**: it needs `local_infile=ON` on the server (off by default
//!   since MySQL 8.0) and a client that lets the *server* name the file to send, which a
//!   malicious or compromised server can abuse to read client files.
//! - The token is checked before each statement; a running statement is cancelled by the
//!   driver (`KILL QUERY`). Rows of a failed or cancelled call roll back with the caller's
//!   transaction on transactional storage engines (InnoDB); non-transactional ones (MyISAM)
//!   keep the statements that completed.

use std::sync::Arc;

use arrow_array::RecordBatch;
use async_trait::async_trait;
use oxbase_dialect::bulk::{BulkLimits, BulkLoader, BulkMethod, LoadSession, LoadTarget};
use oxbase_dialect::{BacktickQuoting, InsertLoader};
use oxbase_driver::CancellationToken;
use oxbase_types::{BoundStatement, Engine, Result, ServerVersion};

/// The session setting [`BulkLoader::prepare`] runs.
pub const UTC_SESSION_SQL: &str = "SET time_zone = '+00:00'";

/// The MySQL / MariaDB [`BulkLoader`]: [`BulkMethod::MultiRowInsert`]. See the module docs.
pub struct MySqlBulkLoader {
    inner: InsertLoader,
    limits: BulkLimits,
}

impl MySqlBulkLoader {
    /// The loader of `engine` (MySQL or MariaDB; both have the same limits).
    pub fn new(engine: Engine) -> Self {
        let limits = BulkLimits::for_engine(engine);
        Self {
            inner: InsertLoader::new(Arc::new(BacktickQuoting), limits),
            limits,
        }
    }
}

#[async_trait]
impl BulkLoader for MySqlBulkLoader {
    fn method(&self, _version: &ServerVersion) -> BulkMethod {
        BulkMethod::MultiRowInsert
    }

    fn limits(&self, _version: &ServerVersion) -> BulkLimits {
        self.limits
    }

    async fn prepare(
        &self,
        session: LoadSession<'_>,
        _target: &LoadTarget<'_>,
        cancel: CancellationToken,
    ) -> Result<()> {
        session
            .run(&BoundStatement::new(UTC_SESSION_SQL, Vec::new()), cancel)
            .await
            .map(|_| ())
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
        let loader = MySqlBulkLoader::new(Engine::MariaDb);
        let version = ServerVersion::new(11, 8, 9);
        assert_eq!(loader.method(&version), BulkMethod::MultiRowInsert);
        let limits = loader.limits(&version);
        assert_eq!(limits.max_params, 65_535);
        // 14 columns: 4681 rows (65 534 placeholders) per statement.
        assert_eq!(limits.rows_per_statement(14), 4_681);
        assert_eq!(limits.max_statement_bytes, Some(16 * 1024 * 1024));
    }
}
