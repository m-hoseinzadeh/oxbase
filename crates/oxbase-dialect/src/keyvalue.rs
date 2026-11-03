//! The key-value role (M7): browse keys and read and write typed values of a key-value store
//! (Redis). Contract: `docs/m7-contracts.md` §3.2, §5.2.
//!
//! One [`KeyValueStore`] per engine, registered with
//! [`DialectRegistry::register_key_value_store`](crate::DialectRegistry::register_key_value_store).
//! `oxbase_services::keys::KeyService` resolves it from the session's engine.
//!
//! # Arrow pages
//!
//! Both sources run through `QueryService::run_source` (events, result store, `query_page`,
//! cancel), so a database of millions of keys or a list of millions of elements streams in
//! bounded memory and the grid pulls only the rows it shows.
//!
//! - [`KeyValueStore::scan`]: `SCAN cursor MATCH p COUNT n TYPE t` until the cursor is 0 (on a
//!   cluster: on every master, one after the other), never `KEYS`. Per page of keys, `TYPE`
//!   and `PTTL` in one pipeline. Columns: `key` (`Binary`), `key_text` (`Utf8`, the key as
//!   `BinaryText::display` shows it), `type` (`Utf8`, `KeyType::as_str`), `ttl_ms` (`Int64`,
//!   NULL = no expiry). A key that vanished between `SCAN` and `TYPE` is left out. Keys may
//!   repeat (a `SCAN` guarantee only); the source drops repeats it has seen in this run while
//!   it holds at most [`SCAN_DEDUP_LIMIT`] keys in memory.
//! - [`KeyValueStore::read_value`]: the whole value of one key, read in chunks of
//!   [`VALUE_CHUNK`] (`LRANGE` by index, `HSCAN` / `SSCAN` / `ZSCAN` by cursor, `XRANGE` by id,
//!   `GETRANGE` for strings over [`VALUE_CHUNK`] bytes). Columns per type:
//!
//!   | Type | Columns |
//!   |---|---|
//!   | string | `value` (`Binary`) |
//!   | list | `index` (`Int64`), `value` (`Binary`) |
//!   | hash | `field` (`Binary`), `value` (`Binary`) |
//!   | set | `member` (`Binary`) |
//!   | zset | `member` (`Binary`), `score` (`Float64`), in score order |
//!   | stream | `id` (`Utf8`), `fields` (`Utf8`, `arrow.json` object; values that are not UTF-8 as `{"hex": "..."}`) |
//!   | json | `value` (`Utf8`, `arrow.json`: `JSON.GET key $`) |
//!
//!   `Binary` columns carry field metadata `oxbase.encoding = binary`; the grid shows UTF-8
//!   text when the bytes are valid UTF-8 and escaped bytes otherwise.
//!
//! # Edits
//!
//! [`KeyValueStore::apply`] applies one `KeyApplyRequest` atomically (`WATCH` + `MULTI` /
//! `EXEC`, see its docs) and reads the key's info back.

use std::sync::Arc;

use async_trait::async_trait;
use oxbase_driver::{CancellationToken, Connection, ResultSource};
use oxbase_types::{
    KeyAddress, KeyApplyReport, KeyApplyRequest, KeyInfo, KeyScanRequest, KeyType, Result,
};

/// Elements per round trip of a value read (and `COUNT` of its cursor commands).
pub const VALUE_CHUNK: usize = 1000;
/// Keys a scan remembers to drop repeats; past it, repeats may reach the grid.
pub const SCAN_DEDUP_LIMIT: usize = 1_000_000;
/// Arrow field metadata key marking `Binary` columns of user bytes.
pub const META_ENCODING: &str = "oxbase.encoding";

/// Browses keys and reads and writes typed values of one engine. See the module docs.
#[async_trait]
pub trait KeyValueStore: Send + Sync {
    /// A source that streams every key `request` (already validated) matches.
    fn scan(&self, request: &KeyScanRequest) -> Result<Arc<dyn ResultSource>>;

    /// The scan as a console line (`SCAN 0 MATCH m7:* COUNT 1000 TYPE hash`, repeated), for the
    /// run's statement text.
    fn scan_text(&self, request: &KeyScanRequest) -> String;

    /// A source that streams the whole value of `key`, whose type the caller read with
    /// [`KeyValueStore::info`]. `Unsupported` for [`KeyType::Other`].
    fn read_value(&self, key: &KeyAddress, key_type: KeyType) -> Result<Arc<dyn ResultSource>>;

    /// The read as console commands (`HSCAN m7:hash 0 COUNT 1000`, repeated), for the run's
    /// statement text.
    fn read_text(&self, key: &KeyAddress, key_type: KeyType) -> String;

    /// Type, TTL, length, encoding and memory of `key`; `NotFound` when it does not exist.
    /// `c` is a session on `key.database`.
    async fn info(
        &self,
        c: &dyn Connection,
        key: &KeyAddress,
        cancel: CancellationToken,
    ) -> Result<KeyInfo>;

    /// Applies `request` (already validated) atomically on `c`, a session on
    /// `request.address.database`. Conflicts and server errors are reported in the
    /// [`KeyApplyReport`] (`committed = false`), not as `Err`.
    async fn apply(
        &self,
        c: &dyn Connection,
        request: &KeyApplyRequest,
        cancel: CancellationToken,
    ) -> Result<KeyApplyReport>;
}
