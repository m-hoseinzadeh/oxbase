//! The persisted form of a [`CatalogSnapshot`] (the app database's `schema_cache_snapshots`
//! table, `oxbase_store::SchemaCacheRepository`).
//!
//! Format 1 is the snapshot's serde JSON as UTF-8 bytes. A snapshot of another
//! [`CatalogSnapshot::FORMAT`] (an older or newer build) does not decode; the caller drops it
//! and reads the catalog again.

use oxbase_types::{CatalogSnapshot, Error, Result};

/// Snapshots larger than this are not persisted (the cache still works in memory).
pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

/// The bytes to persist for `snapshot`.
pub fn encode(snapshot: &CatalogSnapshot) -> Vec<u8> {
    // Plain data with string keys: serialization cannot fail.
    serde_json::to_vec(snapshot).unwrap_or_default()
}

/// The snapshot persisted as `bytes`. `Error::InvalidInput` when they are not a snapshot of
/// this build's format.
pub fn decode(bytes: &[u8]) -> Result<CatalogSnapshot> {
    let snapshot: CatalogSnapshot = serde_json::from_slice(bytes)
        .map_err(|e| Error::invalid_input(format!("unreadable schema cache snapshot: {e}")))?;
    if snapshot.format != CatalogSnapshot::FORMAT {
        return Err(Error::invalid_input(format!(
            "schema cache snapshot format {} (this build reads {})",
            snapshot.format,
            CatalogSnapshot::FORMAT
        )));
    }
    Ok(snapshot)
}
