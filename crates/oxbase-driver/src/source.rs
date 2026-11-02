//! Result sources (M7): a stream of result items produced by engine code instead of by one
//! statement text, run through the same query-run machinery (events, result store, paging,
//! cancel) as `Connection::execute`.

use async_trait::async_trait;
use oxbase_types::Result;

use crate::{CancellationToken, Connection, ResultStream};

/// Opens a [`ResultStream`] on a connection: a find / aggregate request, a key-browser `SCAN`
/// over every key, a typed value read in chunks. Dialect cells build them (the
/// `oxbase_dialect::DocumentStore` / `KeyValueStore` roles) and `oxbase-services` runs them
/// with `QueryService::run_source`, on an editor session like any query.
///
/// The stream follows the [`Connection::execute`] contract: `Schema` then `Batch`es per result
/// set, `RowsAffected`, `Notice`s anywhere; one `Err(Error::Cancelled)` and the end once
/// `cancel` fires (stop the server-side work: drop the cursor, stop issuing `SCAN`s); a token
/// that is already cancelled fails `open` with `Cancelled` without touching the server. The
/// stream is `'static` and owns whatever it needs from `c` (a client handle, a cursor).
///
/// A source is called with a connection of its own engine; it may reach the concrete
/// connection with [`Connection::as_any`] (same engine crate only).
#[async_trait]
pub trait ResultSource: Send + Sync {
    async fn open(&self, c: &dyn Connection, cancel: CancellationToken) -> Result<ResultStream>;
}
