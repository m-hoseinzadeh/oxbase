//! Extended JSON v2 ↔ BSON (mongo stream), used by requests, document edits, `document_get`
//! and the shell's `EJSON` helper. Field order is kept both ways (`serde_json` has
//! `preserve_order` in this workspace; documents travel as text, see
//! `oxbase_types::document`).

use bson::{Bson, Document};
use oxbase_types::Result;
use serde_json::Value as Json;

/// A document from Extended JSON text (relaxed or canonical). `InvalidInput` when the text is
/// not JSON, not an object, or holds a malformed wrapper (`{"$oid": "xyz"}`).
pub fn parse_document(text: &str) -> Result<Document> {
    let _ = text;
    unimplemented!("m7: mongo")
}

/// Any BSON value from Extended JSON text (an `_id`, a field value).
pub fn parse_value(text: &str) -> Result<Bson> {
    let _ = text;
    unimplemented!("m7: mongo")
}

/// A filter, projection, sort, hint, collation or pipeline stage from a JSON value: Extended
/// JSON, except that an object whose keys are exactly `$regex` (and `$options`) stays the
/// query operator. `what` names the part in errors.
pub fn query_document(value: &Json, what: &str) -> Result<Document> {
    let _ = (value, what);
    unimplemented!("m7: mongo")
}

/// `value` as canonical Extended JSON text (what the core sends to the UI).
pub fn to_canonical_text(value: &Bson) -> String {
    let _ = value;
    unimplemented!("m7: mongo")
}

/// `value` as relaxed Extended JSON (cells of JSON text columns, the shell's `printjson`).
pub fn to_relaxed(value: &Bson) -> Json {
    let _ = value;
    unimplemented!("m7: mongo")
}
