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
//!
//! One implementation serves the whole crate: [`DocumentLayout`] is the public face of the
//! driver's own layout (`driver::convert`), so a find typed in the editor, a builder request
//! and a shell cursor give the same columns for the same documents.

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use bson::Document;
use oxbase_types::Result;

use crate::driver::convert::Layout;

/// The columns of one document result set.
#[derive(Debug, Clone)]
pub struct DocumentLayout {
    schema: SchemaRef,
    layout: Layout,
}

impl DocumentLayout {
    /// Infers the columns from `sample` (the first documents of the result). `projection`, when
    /// the request has one with included fields, fixes the order of those fields.
    pub fn infer(sample: &[Document], projection: Option<&Document>) -> Self {
        let layout = Layout::infer(sample, projection);
        Self {
            schema: layout.schema(),
            layout,
        }
    }

    /// The Arrow schema (ends with the `$document` column).
    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    /// One batch of `documents` in this layout, and how many values did not fit their column
    /// (NULL there; the document column keeps them).
    pub fn batch(&self, documents: &[Document]) -> Result<(RecordBatch, u64)> {
        self.layout.batch(documents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Array;
    use arrow_array::cast::AsArray;
    use arrow_array::types::Int64Type;
    use arrow_schema::DataType;
    use bson::doc;
    use oxbase_dialect::document::{DOCUMENT_COLUMN, META_BSON_TYPE, META_ROLE, ROLE_DOCUMENT};

    #[test]
    fn int_and_long_share_one_int64_column_and_the_document_column_is_last() {
        let docs = vec![
            doc! {"_id": 1_i32, "n": 1_i32, "s": "a"},
            doc! {"_id": 2_i32, "n": 2_i64, "late": true},
        ];
        let layout = DocumentLayout::infer(&docs[..1], None);
        let schema = layout.schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, ["_id", "n", "s", DOCUMENT_COLUMN]);
        let last = schema.field(names.len() - 1);
        assert_eq!(
            last.metadata().get(META_ROLE).map(String::as_str),
            Some(ROLE_DOCUMENT)
        );

        let layout = DocumentLayout::infer(&docs, None);
        let schema = layout.schema();
        let n = schema.field_with_name("n").unwrap();
        assert_eq!(n.data_type(), &DataType::Int64);
        assert_eq!(
            n.metadata().get(META_BSON_TYPE).map(String::as_str),
            Some("long")
        );
        let (batch, misfits) = layout.batch(&docs).unwrap();
        assert_eq!(misfits, 0);
        assert_eq!(batch.num_rows(), 2);
        let values = batch.column(1).as_primitive::<Int64Type>();
        assert_eq!(values.value(0), 1);
        assert_eq!(values.value(1), 2);
        let documents = batch.column(batch.num_columns() - 1).as_string::<i32>();
        assert!(!documents.is_null(1));
        assert!(documents.value(1).contains("\"late\":true"));
    }
}
