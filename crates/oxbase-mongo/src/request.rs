//! Find / aggregate requests (mongo stream): the builder's [`DocumentQuery`] run as a
//! [`ResultSource`], its shell text, and JSON request statements typed in the editor.
//! Contract: `oxbase_dialect::document` module docs and `docs/m7-contracts.md` §4.1, §5.1.
//!
//! - Filter, projection, sort, hint, collation and pipeline stages convert from Extended JSON
//!   with [`crate::ejson::query_document`] (the `$regex` operator exception); a value that does
//!   not parse is `InvalidInput` naming the part (`filter`, `pipeline stage 2`).
//! - `find`: `Collection::find` with the options, `batchSize` 1000, `comment` from
//!   `MongoConnection::next_comment`; `aggregate`: `Collection::aggregate` with
//!   `allowDiskUse`, on a view the same calls (MongoDB runs the view's pipeline).
//! - Documents are turned into Arrow by [`crate::arrow::DocumentLayout`] (inferred from the
//!   first batch, up to the profile's `sample_size` documents).
//! - [`request_text`] writes the request the way the shell would:
//!   `db.getSiblingDB("oxbase_test").getCollection("m7_docs").find({...}, {...}).sort({...}).skip(5).limit(10)`
//!   with canonical Extended JSON arguments, so the history entry can be pasted into the
//!   shell editor.

use async_trait::async_trait;
use oxbase_driver::{CancellationToken, Connection, ResultSource, ResultStream};
use oxbase_types::{DocumentQuery, Result};

/// A validated request, ready to run on a [`crate::MongoConnection`].
#[derive(Debug, Clone)]
pub struct RequestSource {
    request: DocumentQuery,
}

impl RequestSource {
    /// Checks the request (`DocumentQuery::validate`) and its Extended JSON parts.
    pub fn new(request: DocumentQuery) -> Result<Self> {
        drop(request);
        unimplemented!("m7: mongo")
    }

    /// The request this source runs.
    pub fn request(&self) -> &DocumentQuery {
        &self.request
    }
}

#[async_trait]
impl ResultSource for RequestSource {
    async fn open(&self, c: &dyn Connection, cancel: CancellationToken) -> Result<ResultStream> {
        let _ = (c, cancel);
        unimplemented!("m7: mongo")
    }
}

/// `request` as shell code (see the module docs).
pub fn request_text(request: &DocumentQuery) -> String {
    let _ = request;
    unimplemented!("m7: mongo")
}

/// A JSON request statement from the editor (`{"type": "find", "collection": {...}, ...}`),
/// parsed and validated. `InvalidInput` when it is not a request.
pub fn parse_request_statement(text: &str) -> Result<DocumentQuery> {
    let _ = text;
    unimplemented!("m7: mongo")
}
