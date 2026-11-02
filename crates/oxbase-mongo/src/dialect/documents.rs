//! The MongoDB document store (mongo stream): find / aggregate sources
//! ([`crate::request::RequestSource`]), their shell text, `document_get` and optimistic
//! document edits. Contract: `oxbase_dialect::document` and `oxbase_types::document` module
//! docs, `docs/m7-contracts.md` §5.1.
//!
//! Edits, each with the connection's operation comment and acknowledged write concern:
//!
//! | Edit | Command | Conflict when |
//! |---|---|---|
//! | `Replace` | `replaceOne({_id, $expr: {$eq: ["$$ROOT", {$literal: previous}]}}, document)` | `matchedCount` 0 |
//! | `Update` | `updateOne({_id, <per change: $expr equality with $literal, or {path: {$exists: false}}>}, {$set, $unset})` | `matchedCount` 0 |
//! | `Insert` | `insertOne(document)` | never (a duplicate `_id` is `failed`) |
//! | `Delete` | `deleteOne({_id[, $expr ... previous]})` | `deletedCount` 0 |
//!
//! After an applied replace, update or insert the document is read back by `_id` and returned
//! as canonical Extended JSON text in the outcome.

use std::sync::Arc;

use async_trait::async_trait;
use oxbase_dialect::DocumentStore;
use oxbase_driver::{CancellationToken, Connection, ResultSource};
use oxbase_types::{DocumentApplyReport, DocumentEdit, DocumentQuery, ObjectRef, Result};

/// The MongoDB document cell.
#[derive(Debug, Clone, Copy, Default)]
pub struct MongoDocumentStore;

#[async_trait]
impl DocumentStore for MongoDocumentStore {
    fn query(&self, request: &DocumentQuery) -> Result<Arc<dyn ResultSource>> {
        let _ = request;
        unimplemented!("m7: mongo")
    }

    fn request_text(&self, request: &DocumentQuery) -> String {
        let _ = request;
        unimplemented!("m7: mongo")
    }

    async fn get(
        &self,
        c: &dyn Connection,
        collection: &ObjectRef,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<Option<String>> {
        let _ = (c, collection, id, cancel);
        unimplemented!("m7: mongo")
    }

    async fn apply(
        &self,
        c: &dyn Connection,
        collection: &ObjectRef,
        edits: &[DocumentEdit],
        cancel: CancellationToken,
    ) -> Result<DocumentApplyReport> {
        let _ = (c, collection, edits, cancel);
        unimplemented!("m7: mongo")
    }
}
