//! Import into collections (mongo stream): the MongoDB [`BulkLoader`]. Contract:
//! `docs/m7-contracts.md` §5.3.
//!
//! - Each row of a load batch becomes one document: one field per load column, in column
//!   order; JSON-typed columns (`DataType::Json`) are parsed and stored as embedded documents
//!   or arrays; NULL cells are stored as BSON null (an empty column is kept, not dropped).
//! - `load` sends `insertMany` (`ordered: true`) with at most [`BulkLimits::rows_per_statement`]
//!   documents and about 16 MiB per command; a failure names the row.
//! - Not transactional ([`BulkLoader::transactional`] is `false`): the import service loads
//!   without a transaction and reports the batches written before a failure or cancel.
//! - Mode *create*: [`BulkLoader::create_target`] runs `create` for the collection (an
//!   existing collection is `InvalidInput`, like a table that exists). *Append* inserts into
//!   an existing collection. *Truncate*, *delete all* and *replace* need statements MongoDB
//!   does not have; the import service refuses them for a loader without a transaction in M7
//!   (`Unsupported` in the plan), so a collection is never emptied by an import.

use arrow_array::RecordBatch;
use async_trait::async_trait;
use oxbase_dialect::{BulkLimits, BulkLoader, BulkMethod, LoadSession, LoadTarget};
use oxbase_driver::CancellationToken;
use oxbase_types::{Result, ServerVersion};

/// Documents per `insertMany`.
pub const DOCUMENTS_PER_INSERT: usize = 1000;

/// The MongoDB import loader.
#[derive(Debug, Clone, Copy, Default)]
pub struct MongoBulkLoader;

#[async_trait]
impl BulkLoader for MongoBulkLoader {
    fn method(&self, version: &ServerVersion) -> BulkMethod {
        let _ = version;
        BulkMethod::DocumentInsert
    }

    fn limits(&self, version: &ServerVersion) -> BulkLimits {
        let _ = version;
        BulkLimits {
            max_params: usize::MAX,
            max_rows_per_statement: DOCUMENTS_PER_INSERT,
            max_statement_bytes: Some(16 * 1024 * 1024),
            rows_per_call: DOCUMENTS_PER_INSERT,
        }
    }

    fn transactional(&self) -> bool {
        false
    }

    async fn create_target(
        &self,
        session: LoadSession<'_>,
        target: &LoadTarget<'_>,
        cancel: CancellationToken,
    ) -> Option<Result<()>> {
        let _ = (session, target, cancel);
        unimplemented!("m7: mongo")
    }

    async fn load(
        &self,
        session: LoadSession<'_>,
        target: &LoadTarget<'_>,
        batch: &RecordBatch,
        cancel: CancellationToken,
    ) -> Result<u64> {
        let _ = (session, target, batch, cancel);
        unimplemented!("m7: mongo")
    }
}
