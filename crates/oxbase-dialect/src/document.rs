//! The document role (M7): read and write documents of a document database (MongoDB).
//! Contract: `docs/m7-contracts.md` §3.1, §5.1.
//!
//! One [`DocumentStore`] per engine, registered with
//! [`DialectRegistry::register_document_store`](crate::DialectRegistry::register_document_store).
//! `oxbase_services::documents::DocumentService` resolves it from the session's engine; nothing
//! above this trait knows MongoDB.
//!
//! # Arrow pages of documents
//!
//! [`DocumentStore::query`] returns a [`ResultSource`] that `QueryService::run_source` runs like a
//! statement. Its result set is one Arrow batch stream with:
//!
//! - a column per top-level field found in the first [`SCHEMA_SAMPLE_DOCUMENTS`] documents (or
//!   the projection), `_id` always first, then in order of first appearance;
//! - each column typed from the BSON types seen (field metadata [`META_BSON_TYPE`]): one type
//!   maps to its Arrow type (`objectId` → `Utf8` hex, `string` → `Utf8`, `int` → `Int32`,
//!   `long` → `Int64`, `int` + `long` → `Int64`, `double` → `Float64`, `decimal` → `Utf8`
//!   exact text, `bool` → `Boolean`, `date` → `Timestamp(Millisecond, "UTC")`, `binData` →
//!   `Binary`); `object`, `array`, every other BSON type and every mix of types are `Utf8`
//!   holding relaxed Extended JSON with `ARROW:extension:name = arrow.json`, and
//!   [`META_BSON_TYPE`] = `mixed` plus [`META_BSON_TYPES`] listing what was seen;
//! - a value of a later document that does not fit its column's type is NULL in that column
//!   (the row's document column below still holds it), and the result gets one warning notice
//!   counting such cells; fields first seen after the sample appear only in the document column;
//! - last, the **document column** [`DOCUMENT_COLUMN`]: `Utf8` canonical Extended JSON of the
//!   whole document, field metadata [`META_ROLE`] = [`ROLE_DOCUMENT`] and `arrow.json`. The grid
//!   hides it; the tree and JSON views and the editors read it (so they show every field and
//!   type exactly, and edits start from the canonical document).
//!
//! # Edits
//!
//! [`DocumentStore::apply`] runs [`DocumentEdit`]s in order, each on its own (M7 has no
//! multi-document transactions: the fixture and most deployments are standalone servers), with
//! the optimistic checks documented on [`DocumentEdit`]; the first conflict or failure stops the
//! rest (`notRun`). Writes use acknowledged write concern (`w: 1` or the server default).

use async_trait::async_trait;
use oxbase_driver::{CancellationToken, Connection, ResultSource};
use oxbase_types::{DocumentApplyReport, DocumentEdit, DocumentQuery, ObjectRef, Result};
use std::sync::Arc;

/// Documents read to infer the column layout of a document result.
pub const SCHEMA_SAMPLE_DOCUMENTS: usize = 1000;
/// Name of the last column of a document result: the whole document (see the module docs).
pub const DOCUMENT_COLUMN: &str = "$document";
/// Arrow field metadata key: the column's role. Only [`ROLE_DOCUMENT`] is defined.
pub const META_ROLE: &str = "oxbase.role";
/// Value of [`META_ROLE`] for the document column.
pub const ROLE_DOCUMENT: &str = "document";
/// Arrow field metadata key: the BSON type of a column (`objectId`, `string`, `int`, `long`,
/// `double`, `decimal`, `bool`, `date`, `binData`, `object`, `array`, `mixed`, ...; the names
/// MongoDB's `$type` uses). Also used as `oxbase.native_type`.
pub const META_BSON_TYPE: &str = "oxbase.bson_type";
/// Arrow field metadata key of a `mixed` column: the BSON types seen, comma-separated.
pub const META_BSON_TYPES: &str = "oxbase.bson_types";

/// Reads and writes documents of one engine. See the module docs.
#[async_trait]
pub trait DocumentStore: Send + Sync {
    /// A source that runs `request` (already validated with `DocumentQuery::validate`) and
    /// streams the documents as described in the module docs. Errors that need no server
    /// (Extended JSON that does not parse) fail here with `InvalidInput`.
    fn query(&self, request: &DocumentQuery) -> Result<Arc<dyn ResultSource>>;

    /// The request as the engine's shell would write it
    /// (`db.getSiblingDB("d").getCollection("c").find({...}).limit(10)`), for the run's
    /// statement text and history.
    fn request_text(&self, request: &DocumentQuery) -> String;

    /// The document of `collection` whose `_id` is `id` (Extended JSON text), as canonical
    /// Extended JSON text; `None` when there is none.
    async fn get(
        &self,
        c: &dyn Connection,
        collection: &ObjectRef,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<Option<String>>;

    /// Applies `edits` (already validated with `validate_document_edits`) to `collection` in
    /// order; see the module docs. Never fails as a whole once the first edit was tried: each
    /// edit's outcome is in the report. `Err` only before anything was written (cancelled
    /// before the first edit, unusable connection).
    async fn apply(
        &self,
        c: &dyn Connection,
        collection: &ObjectRef,
        edits: &[DocumentEdit],
        cancel: CancellationToken,
    ) -> Result<DocumentApplyReport>;
}
