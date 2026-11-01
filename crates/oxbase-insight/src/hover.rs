//! Hover information (`docs/m6-contracts.md` §4.4). Owner of [`hover`]: the **diagnose**
//! stream.

use std::ops::Range;

use oxbase_schema_cache::SchemaCache;
use serde::{Deserialize, Serialize};

use crate::Dialect;

/// What the editor shows over an identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hover {
    /// Byte range of the identifier (or keyword / function name) hovered.
    pub range: Range<usize>,
    /// Markdown: the object's kind and qualified name, then for a relation its columns
    /// (name, type, nullability, primary and foreign keys) and comment; for a column its type,
    /// nullability, keys and relation; for a routine its signature; for a built-in function
    /// its signature and summary; for a keyword nothing (no hover).
    pub markdown: String,
}

/// The hover for the identifier at byte `offset` of `text`, resolved through the statement's
/// aliases and CTEs and then `cache`. `None` when there is nothing to say (whitespace, a
/// literal, a name the cache does not know and that is not built in). Never panics.
pub fn hover(
    dialect: &Dialect,
    text: &str,
    offset: usize,
    cache: Option<&SchemaCache>,
) -> Option<Hover> {
    let _ = (dialect, text, offset, cache);
    unimplemented!("m6: diagnose")
}
