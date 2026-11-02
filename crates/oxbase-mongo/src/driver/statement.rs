//! JSON statements the driver runs itself (driver module docs, *Statements*): a find /
//! aggregate request (`oxbase_types::DocumentQuery`, the builder's form) or a database command
//! (`{"type": "command", "database": "admin", "command": {"ping": 1}}`), and the Extended
//! JSON → BSON conversion of their parts.
//!
//! Values are MongoDB Extended JSON v2 (relaxed or canonical; key order kept), with one
//! exception in filters, projections, sorts and pipeline stages: an object whose keys are
//! exactly `$regex` (and optionally `$options`) stays the query operator instead of becoming a
//! legacy regular-expression value.

use std::time::Duration;

use bson::{Bson, Document};
use mongodb::options::{AggregateOptions, Collation, FindOptions, Hint};
use oxbase_types::{AggregateRequest, DocumentQuery, Error, FindRequest, Result};
use serde_json::{Map, Value as Json};

/// Documents per Arrow batch and per server batch (`batchSize`).
pub(crate) const BATCH_ROWS: usize = 1000;

/// Keys that make an object an Extended JSON value wrapper rather than a document.
const WRAPPER_KEYS: [&str; 17] = [
    "$oid",
    "$symbol",
    "$numberInt",
    "$numberLong",
    "$numberDouble",
    "$numberDecimal",
    "$binary",
    "$uuid",
    "$code",
    "$timestamp",
    "$regularExpression",
    "$dbPointer",
    "$date",
    "$minKey",
    "$maxKey",
    "$undefined",
    "$scope",
];

/// A parsed JSON statement.
#[derive(Debug, Clone)]
pub(crate) enum Statement {
    Query(Box<DocumentQuery>),
    Command { database: String, command: Document },
}

/// Parses a statement whose first non-blank character is `{`.
pub(crate) fn parse(text: &str) -> Result<Statement> {
    let value: Json = serde_json::from_str(text.trim())
        .map_err(|e| Error::invalid_input(format!("the request is not valid JSON: {e}")))?;
    let Json::Object(object) = &value else {
        return Err(Error::invalid_input("a request must be a JSON object"));
    };
    let kind = object.get("type").and_then(Json::as_str).map(str::to_owned);
    match kind.as_deref() {
        Some("command") => parse_command(object),
        Some("find" | "aggregate") => {
            let query: DocumentQuery = serde_json::from_value(value)
                .map_err(|e| Error::invalid_input(format!("invalid request: {e}")))?;
            query.validate()?;
            // Check the Extended JSON parts before anything reaches the server.
            match &query {
                DocumentQuery::Find(f) => {
                    find_parts(f)?;
                }
                DocumentQuery::Aggregate(a) => {
                    aggregate_parts(a)?;
                }
            }
            Ok(Statement::Query(Box::new(query)))
        }
        _ => Err(Error::invalid_input(
            "a JSON request needs \"type\": \"find\", \"aggregate\" or \"command\"",
        )),
    }
}

fn parse_command(object: &Map<String, Json>) -> Result<Statement> {
    if let Some(key) = object
        .keys()
        .find(|k| !matches!(k.as_str(), "type" | "database" | "command"))
    {
        return Err(Error::invalid_input(format!(
            "unknown key '{key}' in a command request"
        )));
    }
    let database = object
        .get("database")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .ok_or_else(|| Error::invalid_input("a command request needs a \"database\""))?
        .to_owned();
    let command = match object.get("command") {
        Some(c @ Json::Object(o)) if !o.is_empty() => query_document(c, "command")?,
        _ => {
            return Err(Error::invalid_input(
                "a command request needs a non-empty \"command\" object",
            ));
        }
    };
    Ok(Statement::Command { database, command })
}

/// A JSON value as BSON (Extended JSON with the `$regex` operator exception). `what` names
/// the part in errors.
pub(crate) fn to_bson(value: &Json, what: &str) -> Result<Bson> {
    match value {
        Json::Object(object) => {
            if is_regex_operator(object) {
                let mut out = Document::new();
                for (key, v) in object {
                    out.insert(key.clone(), to_bson(v, what)?);
                }
                return Ok(Bson::Document(out));
            }
            if object.keys().any(|k| WRAPPER_KEYS.contains(&k.as_str())) {
                return Bson::try_from(value.clone()).map_err(|e| {
                    Error::invalid_input(format!("{what}: invalid Extended JSON value: {e}"))
                });
            }
            let mut out = Document::new();
            for (key, v) in object {
                out.insert(key.clone(), to_bson(v, what)?);
            }
            Ok(Bson::Document(out))
        }
        Json::Array(items) => items
            .iter()
            .map(|v| to_bson(v, what))
            .collect::<Result<Vec<_>>>()
            .map(Bson::Array),
        scalar => {
            Bson::try_from(scalar.clone()).map_err(|e| Error::invalid_input(format!("{what}: {e}")))
        }
    }
}

/// `{"$regex": ...}` or `{"$regex": ..., "$options": ...}`.
fn is_regex_operator(object: &Map<String, Json>) -> bool {
    object.contains_key("$regex") && object.keys().all(|k| k == "$regex" || k == "$options")
}

/// A JSON object as a BSON document.
pub(crate) fn query_document(value: &Json, what: &str) -> Result<Document> {
    match to_bson(value, what)? {
        Bson::Document(d) => Ok(d),
        _ => Err(Error::invalid_input(format!("{what} must be an object"))),
    }
}

fn optional_document(value: Option<&Json>, what: &str) -> Result<Option<Document>> {
    value.map(|v| query_document(v, what)).transpose()
}

fn collation(value: Option<&Json>) -> Result<Option<Collation>> {
    optional_document(value, "collation")?
        .map(|doc| {
            bson::deserialize_from_document::<Collation>(doc)
                .map_err(|e| Error::invalid_input(format!("collation: {e}")))
        })
        .transpose()
}

/// The filter and options of a find request (comment and batch size set by the caller).
pub(crate) fn find_parts(f: &FindRequest) -> Result<(Document, FindOptions)> {
    let filter = optional_document(f.filter.as_ref(), "filter")?.unwrap_or_default();
    let mut options = FindOptions::default();
    options.projection = optional_document(f.projection.as_ref(), "projection")?;
    options.sort = optional_document(f.sort.as_ref(), "sort")?;
    options.skip = f.skip;
    options.limit = f
        .limit
        .map(|n| i64::try_from(n).map_err(|_| Error::invalid_input("limit is too large")))
        .transpose()?;
    options.hint = match &f.hint {
        None => None,
        Some(Json::String(name)) => Some(Hint::Name(name.clone())),
        Some(keys) => Some(Hint::Keys(query_document(keys, "hint")?)),
    };
    options.collation = collation(f.collation.as_ref())?;
    options.max_time = f.max_time_ms.map(Duration::from_millis);
    Ok((filter, options))
}

/// The pipeline and options of an aggregate request.
pub(crate) fn aggregate_parts(a: &AggregateRequest) -> Result<(Vec<Document>, AggregateOptions)> {
    let pipeline = a
        .pipeline
        .iter()
        .enumerate()
        .map(|(i, stage)| query_document(stage, &format!("pipeline stage {}", i + 1)))
        .collect::<Result<Vec<_>>>()?;
    let mut options = AggregateOptions::default();
    options.allow_disk_use = a.allow_disk_use.then_some(true);
    options.collation = collation(a.collation.as_ref())?;
    options.max_time = a.max_time_ms.map(Duration::from_millis);
    Ok((pipeline, options))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;
    use bson::oid::ObjectId;
    use serde_json::json;

    #[test]
    fn find_requests_parse_with_typed_values_and_the_regex_operator() {
        let text = r#"
            {"type": "find",
             "collection": {"kind": "table", "catalog": "oxbase_test", "name": "m7_docs"},
             "filter": {"_id": {"$oid": "65a000000000000000000001"},
                        "name": {"$regex": "^a", "$options": "i"},
                        "n": {"$gte": {"$numberLong": "5"}}},
             "projection": {"name": 1, "n": 1},
             "sort": {"n": -1, "name": 1},
             "skip": 2, "limit": 10, "hint": "m7_docs_name_idx", "maxTimeMs": 500}
        "#;
        let Statement::Query(query) = parse(text).unwrap() else {
            panic!("a query");
        };
        let DocumentQuery::Find(f) = query.as_ref() else {
            panic!("find");
        };
        let (filter, options) = find_parts(f).unwrap();
        assert_eq!(
            filter.get("_id"),
            Some(&Bson::ObjectId(
                ObjectId::parse_str("65a000000000000000000001").unwrap()
            ))
        );
        assert_eq!(
            filter.get_document("name").unwrap(),
            &doc! {"$regex": "^a", "$options": "i"},
            "the $regex operator is not a regular-expression value"
        );
        assert_eq!(filter.get_document("n").unwrap(), &doc! {"$gte": 5_i64});
        let sort = options.sort.unwrap();
        let keys: Vec<&String> = sort.keys().collect();
        assert_eq!(keys, ["n", "name"], "sort keeps its key order");
        assert_eq!(options.skip, Some(2));
        assert_eq!(options.limit, Some(10));
        assert!(matches!(options.hint, Some(Hint::Name(ref n)) if n == "m7_docs_name_idx"));
        assert_eq!(options.max_time, Some(Duration::from_millis(500)));
    }

    #[test]
    fn aggregate_and_command_requests_parse() {
        let text = r#"{"type": "aggregate",
            "collection": {"kind": "table", "catalog": "oxbase_test", "name": "m7_big"},
            "pipeline": [{"$match": {"g": 7}}, {"$group": {"_id": null, "n": {"$sum": 1}}}],
            "allowDiskUse": true}"#;
        let Statement::Query(query) = parse(text).unwrap() else {
            panic!("a query");
        };
        let DocumentQuery::Aggregate(a) = query.as_ref() else {
            panic!("aggregate");
        };
        let (pipeline, options) = aggregate_parts(a).unwrap();
        assert_eq!(pipeline.len(), 2);
        assert_eq!(pipeline[0], doc! {"$match": {"g": 7}});
        assert_eq!(options.allow_disk_use, Some(true));

        let Statement::Command { database, command } =
            parse(r#" {"type": "command", "database": "admin", "command": {"ping": 1}} "#).unwrap()
        else {
            panic!("a command");
        };
        assert_eq!(database, "admin");
        assert_eq!(command, doc! {"ping": 1});
    }

    #[test]
    fn bad_requests_are_invalid_input() {
        for text in [
            "{",
            "[1]",
            r#"{"type": "drop"}"#,
            r#"{"type": "command", "command": {"ping": 1}}"#,
            r#"{"type": "command", "database": "admin", "command": {}}"#,
            r#"{"type": "command", "database": "admin", "command": {"ping": 1}, "x": 1}"#,
            r#"{"type": "find", "collection": {"kind": "table", "name": "c"}}"#,
            r#"{"type": "find", "collection": {"kind": "table", "catalog": "d", "name": "c"},
                "filter": {"_id": {"$oid": "xyz"}}}"#,
            r#"{"type": "aggregate", "collection": {"kind": "table", "catalog": "d", "name": "c"},
                "pipeline": [{"match": {}}]}"#,
        ] {
            let err = parse(text).expect_err(text);
            assert!(matches!(err, Error::InvalidInput { .. }), "{text}: {err}");
        }
        let err = to_bson(&json!({"$date": "not a date"}), "filter").unwrap_err();
        assert!(err.to_string().contains("filter"), "{err}");
    }
}
