//! Extended JSON v2 ↔ BSON (mongo stream), used by requests, document edits, `document_get`
//! and the shell's `EJSON` helper. Field order is kept both ways (`serde_json` has
//! `preserve_order` in this workspace; documents travel as text, see
//! `oxbase_types::document`).
//!
//! Parsing accepts the relaxed and the canonical form (`bson`'s own Extended JSON reader);
//! writing produces the canonical form for everything the core hands to the UI (so a document
//! survives a round trip without losing a type: `1` stays an `int32`, `{"$numberLong": "1"}` an
//! `int64`) and the relaxed form for display cells.

use bson::{Bson, Document};
use oxbase_types::{Error, Result};
use serde_json::Value as Json;

/// A document from Extended JSON text (relaxed or canonical). `InvalidInput` when the text is
/// not JSON, not an object, or holds a malformed wrapper (`{"$oid": "xyz"}`).
pub fn parse_document(text: &str) -> Result<Document> {
    let json: Json = serde_json::from_str(text.trim())
        .map_err(|e| Error::invalid_input(format!("the document is not valid JSON: {e}")))?;
    if !json.is_object() {
        return Err(Error::invalid_input("a document must be a JSON object"));
    }
    match Bson::try_from(json) {
        Ok(Bson::Document(doc)) => Ok(doc),
        Ok(other) => Err(Error::invalid_input(format!(
            "a document must be an object, not a {} value",
            crate::driver::convert::type_name(other.element_type())
        ))),
        Err(e) => Err(Error::invalid_input(format!(
            "the document holds an invalid Extended JSON value: {e}"
        ))),
    }
}

/// Any BSON value from Extended JSON text (an `_id`, a field value).
pub fn parse_value(text: &str) -> Result<Bson> {
    let json: Json = serde_json::from_str(text.trim())
        .map_err(|e| Error::invalid_input(format!("the value is not valid JSON: {e}")))?;
    Bson::try_from(json)
        .map_err(|e| Error::invalid_input(format!("invalid Extended JSON value: {e}")))
}

/// A filter, projection, sort, hint, collation or pipeline stage from a JSON value: Extended
/// JSON, except that an object whose keys are exactly `$regex` (and `$options`) stays the
/// query operator. `what` names the part in errors.
pub fn query_document(value: &Json, what: &str) -> Result<Document> {
    crate::driver::statement::query_document(value, what)
}

/// `value` as canonical Extended JSON text (what the core sends to the UI).
pub fn to_canonical_text(value: &Bson) -> String {
    value.clone().into_canonical_extjson().to_string()
}

/// `value` as relaxed Extended JSON (cells of JSON text columns, the shell's `printjson`).
pub fn to_relaxed(value: &Bson) -> Json {
    value.clone().into_relaxed_extjson()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::oid::ObjectId;
    use bson::{Decimal128, doc};
    use serde_json::json;

    #[test]
    fn documents_parse_from_both_forms_and_keep_their_field_order() {
        let doc = parse_document(
            r#"{"z": 1, "a": {"$numberLong": "2"}, "_id": {"$oid": "65a000000000000000000001"},
                "d": {"$numberDecimal": "12.50"}, "when": {"$date": "2026-01-02T00:00:00Z"}}"#,
        )
        .unwrap();
        let keys: Vec<&String> = doc.keys().collect();
        assert_eq!(keys, ["z", "a", "_id", "d", "when"]);
        assert_eq!(doc.get("z"), Some(&Bson::Int32(1)));
        assert_eq!(doc.get("a"), Some(&Bson::Int64(2)));
        assert_eq!(
            doc.get("_id"),
            Some(&Bson::ObjectId(
                ObjectId::parse_str("65a000000000000000000001").unwrap()
            ))
        );
        assert_eq!(
            doc.get("d"),
            Some(&Bson::Decimal128("12.50".parse::<Decimal128>().unwrap()))
        );
        assert!(matches!(doc.get("when"), Some(Bson::DateTime(_))));
    }

    #[test]
    fn canonical_text_round_trips_types() {
        let original = doc! {"b": 1_i32, "a": 1_i64, "f": 1.5, "s": "x", "n": null};
        let text = to_canonical_text(&Bson::Document(original.clone()));
        assert_eq!(
            text,
            r#"{"b":{"$numberInt":"1"},"a":{"$numberLong":"1"},"f":{"$numberDouble":"1.5"},"s":"x","n":null}"#
        );
        assert_eq!(parse_document(&text).unwrap(), original);
        assert_eq!(
            to_relaxed(&Bson::Document(original)),
            json!({"b": 1, "a": 1, "f": 1.5, "s": "x", "n": null})
        );
    }

    #[test]
    fn values_parse_and_bad_input_is_invalid_input() {
        assert_eq!(parse_value("7").unwrap(), Bson::Int32(7));
        assert_eq!(
            parse_value(r#""t1-1""#).unwrap(),
            Bson::String("t1-1".into())
        );
        assert_eq!(parse_value("null").unwrap(), Bson::Null);
        for bad in ["{", r#"{"$oid": "xyz"}"#, ""] {
            let err = parse_value(bad).unwrap_err();
            assert!(matches!(err, Error::InvalidInput { .. }), "{bad}: {err}");
        }
        for bad in ["[1]", "3", r#"{"$oid": "65a000000000000000000001"}"#, "{"] {
            let err = parse_document(bad).unwrap_err();
            assert!(matches!(err, Error::InvalidInput { .. }), "{bad}: {err}");
        }
    }

    #[test]
    fn query_documents_keep_the_regex_operator() {
        let filter = query_document(
            &json!({"name": {"$regex": "^a", "$options": "i"}, "n": {"$gt": {"$numberLong": "3"}}}),
            "filter",
        )
        .unwrap();
        assert_eq!(
            filter,
            doc! {"name": {"$regex": "^a", "$options": "i"}, "n": {"$gt": 3_i64}}
        );
        let err = query_document(&json!([1]), "sort").unwrap_err();
        assert!(err.to_string().contains("sort"), "{err}");
    }
}
