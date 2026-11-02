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
//!   with arguments in shell syntax ([`crate::ejson::to_shell_text`]: plain numbers for int32,
//!   `NumberLong("..")`, `ObjectId("..")`, `ISODate("..")`, `/re/flags`, ...), so the history
//!   entry can be pasted into the shell editor or mongosh and means the same request.
//!
//! # How a request runs
//!
//! [`RequestSource::open`] hands the request to the connection as a JSON request statement
//! (the form the driver runs itself, see the `driver` module docs): the driver's statement
//! task streams the cursor in server batches through the same layout, tags every operation
//! with the connection's comment, kills the operation and drops the cursor on cancel (one
//! `Error::Cancelled` within 2 s) and when the consumer drops the stream (`maxRows`, the disk
//! cap). A builder request, a request typed in the editor and the history entry pasted back
//! therefore behave the same.

use std::fmt::Write as _;

use async_trait::async_trait;
use bson::{Bson, Document};
use oxbase_driver::{CancellationToken, Connection, ResultSource, ResultStream};
use oxbase_types::{AggregateRequest, DocumentQuery, Error, FindRequest, Result};
use serde_json::Value as Json;

use crate::driver::statement::{aggregate_parts, find_parts};
use crate::ejson::{query_document, to_shell_text};

/// A validated request, ready to run on a [`crate::MongoConnection`].
#[derive(Debug, Clone)]
pub struct RequestSource {
    request: DocumentQuery,
}

impl RequestSource {
    /// Checks the request (`DocumentQuery::validate`) and its Extended JSON parts.
    pub fn new(request: DocumentQuery) -> Result<Self> {
        request.validate()?;
        match &request {
            DocumentQuery::Find(f) => {
                find_parts(f)?;
            }
            DocumentQuery::Aggregate(a) => {
                aggregate_parts(a)?;
            }
        }
        Ok(Self { request })
    }

    /// The request this source runs.
    pub fn request(&self) -> &DocumentQuery {
        &self.request
    }

    /// The JSON request statement the driver runs.
    fn statement(&self) -> Result<String> {
        serde_json::to_string(&self.request)
            .map_err(|e| Error::internal(format!("writing the request failed: {e}")))
    }
}

#[async_trait]
impl ResultSource for RequestSource {
    async fn open(&self, c: &dyn Connection, cancel: CancellationToken) -> Result<ResultStream> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let statement = self.statement()?;
        c.execute(&statement, &[], cancel).await
    }
}

/// `request` as shell code (see the module docs).
pub fn request_text(request: &DocumentQuery) -> String {
    let collection = request.collection();
    let mut out = format!(
        "db.getSiblingDB({}).getCollection({})",
        js_string(collection.database_name().unwrap_or_default()),
        js_string(&collection.name)
    );
    match request {
        DocumentQuery::Find(f) => find_text(&mut out, f),
        DocumentQuery::Aggregate(a) => aggregate_text(&mut out, a),
    }
    out
}

fn find_text(out: &mut String, f: &FindRequest) {
    let filter = f
        .filter
        .as_ref()
        .map_or_else(|| "{}".to_owned(), |v| json_arg(v, "filter"));
    match &f.projection {
        Some(projection) => {
            push(
                out,
                format_args!(".find({filter}, {})", json_arg(projection, "projection")),
            );
        }
        None => push(out, format_args!(".find({filter})")),
    }
    if let Some(sort) = &f.sort {
        push(out, format_args!(".sort({})", json_arg(sort, "sort")));
    }
    if let Some(hint) = &f.hint {
        let hint = match hint {
            Json::String(name) => js_string(name),
            keys => json_arg(keys, "hint"),
        };
        push(out, format_args!(".hint({hint})"));
    }
    if let Some(collation) = &f.collation {
        push(
            out,
            format_args!(".collation({})", json_arg(collation, "collation")),
        );
    }
    if let Some(skip) = f.skip {
        push(out, format_args!(".skip({skip})"));
    }
    if let Some(limit) = f.limit {
        push(out, format_args!(".limit({limit})"));
    }
    if let Some(ms) = f.max_time_ms {
        push(out, format_args!(".maxTimeMS({ms})"));
    }
}

fn aggregate_text(out: &mut String, a: &AggregateRequest) {
    let stages: Vec<String> = a
        .pipeline
        .iter()
        .enumerate()
        .map(|(i, stage)| json_arg(stage, &format!("pipeline stage {}", i + 1)))
        .collect();
    let mut options = Document::new();
    if a.allow_disk_use {
        options.insert("allowDiskUse", true);
    }
    if let Some(collation) = &a.collation {
        match query_document(collation, "collation") {
            Ok(doc) => {
                options.insert("collation", doc);
            }
            Err(_) => {
                // Not reachable for a validated request; keep the text readable anyway.
                options.insert("collation", Bson::String(collation.to_string()));
            }
        }
    }
    if let Some(ms) = a.max_time_ms {
        let ms = i32::try_from(ms).map_or_else(
            |_| Bson::Int64(i64::try_from(ms).unwrap_or(i64::MAX)),
            Bson::Int32,
        );
        options.insert("maxTimeMS", ms);
    }
    push(out, format_args!(".aggregate([{}]", stages.join(", ")));
    if !options.is_empty() {
        out.push_str(", ");
        out.push_str(&to_shell_text(&Bson::Document(options)));
    }
    out.push(')');
}

/// Appends formatted text to `out`.
fn push(out: &mut String, args: std::fmt::Arguments<'_>) {
    let _ = out.write_fmt(args);
}

/// A JSON part as shell code (its JSON text when it does not convert, which a validated
/// request never has).
fn json_arg(value: &Json, what: &str) -> String {
    match query_document(value, what) {
        Ok(doc) => to_shell_text(&Bson::Document(doc)),
        Err(_) => value.to_string(),
    }
}

/// `text` as a JavaScript string literal (JSON string syntax is valid JavaScript).
fn js_string(text: &str) -> String {
    Json::String(text.to_owned()).to_string()
}

/// A JSON request statement from the editor (`{"type": "find", "collection": {...}, ...}`),
/// parsed and validated. `InvalidInput` when it is not a request.
pub fn parse_request_statement(text: &str) -> Result<DocumentQuery> {
    let value: Json = serde_json::from_str(text.trim())
        .map_err(|e| Error::invalid_input(format!("the request is not valid JSON: {e}")))?;
    let kind = value
        .as_object()
        .and_then(|o| o.get("type"))
        .and_then(Json::as_str);
    if !matches!(kind, Some("find" | "aggregate")) {
        return Err(Error::invalid_input(
            "not a find or aggregate request: it needs \"type\": \"find\" or \"aggregate\"",
        ));
    }
    let query: DocumentQuery = serde_json::from_value(value)
        .map_err(|e| Error::invalid_input(format!("invalid request: {e}")))?;
    Ok(RequestSource::new(query)?.request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{ObjectKind, ObjectRef};
    use serde_json::json;

    fn docs() -> ObjectRef {
        ObjectRef::table(Some("oxbase_test"), None, "m7_docs")
    }

    #[test]
    fn find_text_is_shell_code_with_shell_arguments() {
        let mut f = FindRequest::new(docs());
        f.filter = Some(json!({"n": {"$gte": 5}, "name": {"$regex": "^a"}}));
        f.projection = Some(json!({"name": 1, "n": 1}));
        f.sort = Some(json!({"n": -1, "name": 1}));
        f.skip = Some(5);
        f.limit = Some(10);
        f.hint = Some(json!("m7_docs_name_idx"));
        f.max_time_ms = Some(500);
        assert_eq!(
            request_text(&DocumentQuery::Find(f)),
            concat!(
                r#"db.getSiblingDB("oxbase_test").getCollection("m7_docs")"#,
                r#".find({"n":{"$gte":5},"name":{"$regex":"^a"}}, "#,
                r#"{"name":1,"n":1})"#,
                r#".sort({"n":-1,"name":1})"#,
                r#".hint("m7_docs_name_idx").skip(5).limit(10).maxTimeMS(500)"#
            )
        );
        // Typed values use the shell's helpers, never wrapper objects.
        let mut f = FindRequest::new(docs());
        f.filter = Some(json!({
            "_id": {"$oid": "65a000000000000000000001"},
            "big": {"$numberLong": "9007199254740993"},
            "at": {"$gte": {"$date": "2026-01-02T00:00:00Z"}},
            "price": {"$numberDecimal": "12.50"},
            "re": {"$regularExpression": {"pattern": "^d", "options": "i"}},
        }));
        assert_eq!(
            request_text(&DocumentQuery::Find(f)),
            concat!(
                r#"db.getSiblingDB("oxbase_test").getCollection("m7_docs").find({"#,
                r#""_id":ObjectId("65a000000000000000000001"),"#,
                r#""big":NumberLong("9007199254740993"),"#,
                r#""at":{"$gte":ISODate("2026-01-02T00:00:00Z")},"#,
                r#""price":NumberDecimal("12.50"),"re":/^d/i})"#
            )
        );
        let plain = FindRequest::new(ObjectRef::table(Some("d\"b"), None, "c"));
        assert_eq!(
            request_text(&DocumentQuery::Find(plain)),
            r#"db.getSiblingDB("d\"b").getCollection("c").find({})"#
        );
    }

    #[test]
    fn aggregate_text_lists_stages_and_options() {
        let mut a = AggregateRequest::new(
            docs(),
            vec![
                json!({"$match": {"flag": true}}),
                json!({"$group": {"_id": null, "n": {"$sum": 1}}}),
            ],
        );
        assert_eq!(
            request_text(&DocumentQuery::Aggregate(a.clone())),
            concat!(
                r#"db.getSiblingDB("oxbase_test").getCollection("m7_docs").aggregate(["#,
                r#"{"$match":{"flag":true}}, "#,
                r#"{"$group":{"_id":null,"n":{"$sum":1}}}])"#
            )
        );
        a.allow_disk_use = true;
        a.max_time_ms = Some(9);
        assert!(
            request_text(&DocumentQuery::Aggregate(a))
                .ends_with(r#"}}], {"allowDiskUse":true,"maxTimeMS":9})"#)
        );
    }

    #[test]
    fn sources_validate_their_extended_json() {
        let mut f = FindRequest::new(docs());
        f.filter = Some(json!({"_id": {"$oid": "nope"}}));
        let err = RequestSource::new(DocumentQuery::Find(f)).unwrap_err();
        assert!(matches!(err, Error::InvalidInput { .. }), "{err}");
        assert!(err.to_string().contains("filter"), "{err}");

        let a = AggregateRequest::new(docs(), vec![json!({"$match": {"d": {"$date": "x"}}})]);
        let err = RequestSource::new(DocumentQuery::Aggregate(a)).unwrap_err();
        assert!(err.to_string().contains("pipeline stage 1"), "{err}");

        let view = ObjectRef::new(ObjectKind::View, "m7_view").in_catalog(Some("oxbase_test"));
        let source = RequestSource::new(DocumentQuery::Find(FindRequest::new(view))).unwrap();
        let statement = source.statement().unwrap();
        assert!(statement.starts_with(r#"{"type":"find","collection":{"kind":"view""#));
    }

    #[test]
    fn request_statements_parse() {
        let query = parse_request_statement(
            r#" {"type": "aggregate",
                 "collection": {"kind": "table", "catalog": "oxbase_test", "name": "m7_big"},
                 "pipeline": [{"$group": {"_id": null, "t": {"$sum": "$g"}}}]} "#,
        )
        .unwrap();
        assert!(matches!(query, DocumentQuery::Aggregate(_)));
        for bad in [
            "{",
            "[]",
            r#"{"type": "command", "database": "admin", "command": {"ping": 1}}"#,
            r#"{"type": "find", "collection": {"kind": "table", "name": "c"}}"#,
        ] {
            let err = parse_request_statement(bad).unwrap_err();
            assert!(matches!(err, Error::InvalidInput { .. }), "{bad}: {err}");
        }
    }
}
