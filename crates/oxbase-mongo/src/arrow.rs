//! BSON documents → Arrow (mongo stream), shared by find / aggregate requests and the shell's
//! cursor results. The layout rules are the contract in the `oxbase_dialect::document` module
//! docs (columns inferred from a sample, `_id` first, typed columns per single BSON type, JSON
//! text for nested and mixed values, the `$document` column last, NULL plus a counted notice
//! for values that do not fit their column).
//!
//! Field metadata on every column: `oxbase.bson_type` (and `oxbase.native_type`, same value),
//! `oxbase.bson_types` on `mixed` columns, `ARROW:extension:name = arrow.json` on JSON text
//! columns, `oxbase.role = document` on `$document`. Dates are UTC milliseconds; Decimal128 is
//! its exact text; ObjectId is 24 lowercase hex digits; binary keeps its bytes (subtype in
//! `oxbase.bson_subtype` when every value has the same one; UUIDs, subtype 4, too).

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use bson::Document;
use oxbase_types::Result;

/// The columns of one document result set.
#[derive(Debug, Clone)]
pub struct DocumentLayout {
    schema: SchemaRef,
}

impl DocumentLayout {
    /// Infers the columns from `sample` (the first documents of the result). `projection`, when
    /// the request has one with included fields, fixes the order of those fields.
    pub fn infer(sample: &[Document], projection: Option<&Document>) -> Self {
        let _ = (sample, projection);
        unimplemented!("m7: mongo")
    }

    /// The Arrow schema (ends with the `$document` column).
    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    /// One batch of `documents` in this layout, and how many values did not fit their column
    /// (NULL there; the document column keeps them).
    pub fn batch(&self, documents: &[Document]) -> Result<(RecordBatch, u64)> {
        let _ = documents;
        unimplemented!("m7: mongo")
    }
}
