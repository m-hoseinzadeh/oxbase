//! Document model (M7): find / aggregate requests from the query builder and document edits for
//! write-back, for document databases (MongoDB). Contract: `docs/m7-contracts.md` §3.1, §5.1.
//!
//! # Values
//!
//! Every document, filter, projection, sort, pipeline stage, field value and `_id` is in
//! **MongoDB Extended JSON v2**. Requests from the UI may use the relaxed or the canonical form
//! (`{"$oid": "..."}`, `{"$date": "..."}`, `{"$numberDecimal": "..."}`, `{"$binary": {...}}`,
//! ...); values the core sends back (documents read back after an edit, `document_get`) are
//! **canonical**, so a document survives a round trip through the UI without losing a type
//! (`1` stays an `int32`, `{"$numberLong": "1"}` an `int64`).
//!
//! Field order matters in MongoDB (sort specifications, whole-document comparisons). Queries
//! travel as JSON values (`serde_json` keeps insertion order in this workspace,
//! `preserve_order`). Documents, `_id`s and field values of **edits and their results travel as
//! Extended JSON text** (`String`): the UI keeps the exact text it received and sends it back,
//! so neither JavaScript (which moves integer-like keys first) nor a JSON parser can reorder the
//! fields of the `previous` version an optimistic check compares.
//!
//! One exception to Extended JSON parsing, in filters and pipelines only: an object whose keys
//! are exactly `$regex` (and optionally `$options`) is the query operator, not a legacy regular
//! expression value. Write a regular-expression *value* as `{"$regularExpression": {...}}`.
//!
//! This module only checks the *shape* of requests and edits ([`DocumentQuery::validate`],
//! [`DocumentEdit::validate`]); converting Extended JSON to BSON and running them is the
//! engine's `DocumentStore` cell (`oxbase_dialect::document`).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

use crate::{Error, ObjectKind, ObjectRef, Result};

/// Deepest JSON nesting a request or edit may have (MongoDB refuses documents nested deeper
/// than 100 levels; a request wraps them once more).
pub const MAX_JSON_DEPTH: usize = 100;
/// Most stages an aggregate request may have.
pub const MAX_PIPELINE_STAGES: usize = 1000;
/// Most edits one apply may carry.
pub const MAX_DOCUMENT_EDITS: usize = 10_000;

/// A query from the find / aggregate builder (`document_find`, `document_aggregate`). It runs as
/// an ordinary query run: events, pages, cancel and close as for `query_run`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
#[allow(
    clippy::large_enum_variant,
    reason = "a request is built once per run; boxing would only complicate the payload type"
)]
pub enum DocumentQuery {
    Find(FindRequest),
    Aggregate(AggregateRequest),
}

/// `find` on a collection or view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindRequest {
    /// The collection (`ObjectKind::Table`) or view (`ObjectKind::View`); `catalog` is the
    /// database.
    pub collection: ObjectRef,
    /// Query filter (an object); `None` matches every document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Json>,
    /// Projection (an object).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<Json>,
    /// Sort specification (an object: field → `1` / `-1` / `{"$meta": ...}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<Json>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<u64>,
    /// At most this many documents (`None`: all; the run's `maxRows` still applies).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// Index hint: an index name (string) or key pattern (object).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<Json>,
    /// Collation document (an object).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collation: Option<Json>,
    /// Server-side time limit (`maxTimeMS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_time_ms: Option<u64>,
}

/// `aggregate` on a collection or view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregateRequest {
    /// The collection (`ObjectKind::Table`) or view (`ObjectKind::View`); `catalog` is the
    /// database.
    pub collection: ObjectRef,
    /// The stages, each an object with exactly one `$`-prefixed key. `$out` and `$merge` write
    /// to the database: the request is then a write, and the UI asks first.
    #[serde(default)]
    pub pipeline: Vec<Json>,
    #[serde(default)]
    pub allow_disk_use: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collation: Option<Json>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_time_ms: Option<u64>,
}

impl FindRequest {
    /// A `find` of every document of `collection`.
    pub fn new(collection: ObjectRef) -> Self {
        Self {
            collection,
            filter: None,
            projection: None,
            sort: None,
            skip: None,
            limit: None,
            hint: None,
            collation: None,
            max_time_ms: None,
        }
    }
}

impl AggregateRequest {
    /// An `aggregate` of `collection` through `pipeline`.
    pub fn new(collection: ObjectRef, pipeline: Vec<Json>) -> Self {
        Self {
            collection,
            pipeline,
            allow_disk_use: false,
            collation: None,
            max_time_ms: None,
        }
    }
}

impl DocumentQuery {
    /// The collection or view the request reads.
    pub fn collection(&self) -> &ObjectRef {
        match self {
            Self::Find(f) => &f.collection,
            Self::Aggregate(a) => &a.collection,
        }
    }

    /// `true` for an aggregate whose pipeline writes (`$out`, `$merge`).
    pub fn writes(&self) -> bool {
        match self {
            Self::Find(_) => false,
            Self::Aggregate(a) => a.pipeline.iter().any(|stage| {
                stage
                    .as_object()
                    .is_some_and(|o| o.contains_key("$out") || o.contains_key("$merge"))
            }),
        }
    }

    /// Checks the shape: a named collection or view with a database; filter, projection,
    /// sort, collation (and an object hint) are objects; every pipeline stage is an object with
    /// exactly one `$`-prefixed key; at most [`MAX_PIPELINE_STAGES`] stages; nesting at most
    /// [`MAX_JSON_DEPTH`]. Extended JSON itself is checked by the engine.
    pub fn validate(&self) -> Result<()> {
        check_collection(self.collection())?;
        match self {
            Self::Find(f) => {
                check_object("filter", f.filter.as_ref())?;
                check_object("projection", f.projection.as_ref())?;
                check_object("sort", f.sort.as_ref())?;
                check_object("collation", f.collation.as_ref())?;
                match &f.hint {
                    None | Some(Json::String(_) | Json::Object(_)) => {}
                    Some(_) => {
                        return Err(Error::invalid_input(
                            "hint must be an index name or a key pattern object",
                        ));
                    }
                }
            }
            Self::Aggregate(a) => {
                check_object("collation", a.collation.as_ref())?;
                if a.pipeline.len() > MAX_PIPELINE_STAGES {
                    return Err(Error::invalid_input(format!(
                        "a pipeline may have at most {MAX_PIPELINE_STAGES} stages"
                    )));
                }
                for (i, stage) in a.pipeline.iter().enumerate() {
                    let ok = stage
                        .as_object()
                        .is_some_and(|o| o.len() == 1 && o.keys().all(|k| k.starts_with('$')));
                    if !ok {
                        return Err(Error::invalid_input(format!(
                            "pipeline stage {} must be an object with exactly one $-prefixed \
                             key, such as {{\"$match\": {{...}}}}",
                            i + 1
                        )));
                    }
                    check_depth(stage)?;
                }
            }
        }
        Ok(())
    }
}

/// A field's value before or after a change: present with a value (Extended JSON text), or
/// missing from the document. `"null"` is a present BSON null, not a missing field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum FieldValue {
    Present { value: String },
    Missing,
}

/// One field of an [`DocumentEdit::Update`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldChange {
    /// Dotted path (`address.city`, `tags.0`). No empty segment, no segment starting with `$`.
    pub path: String,
    /// What the editor read (the optimistic check): the update applies only if the field still
    /// has this value (or is still missing).
    pub previous: FieldValue,
    /// The new value (`$set`), or `Missing` to remove the field (`$unset`).
    pub value: FieldValue,
}

/// One document edit (the document grid's pending changes and the tree / JSON views).
///
/// Every edit names the document by its `_id` (Extended JSON text, as the core sent it).
/// Edits are **optimistic**: `Replace` and `Delete` with a `previous` apply only when the
/// stored document is still exactly `previous`, field order included
/// (`{_id: id, $expr: {$eq: ["$$ROOT", {$literal: previous}]}}`); `Update` applies only when
/// every changed field still has its `previous` value (`$expr` equality, or `$exists: false`
/// for `Missing`). A document that changed or went away meanwhile is a `conflict`, never
/// overwritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DocumentEdit {
    /// Replace the whole document (`replaceOne`). `document` may omit `_id` (it is kept); if it
    /// has one it must equal `id`.
    Replace {
        id: String,
        previous: String,
        document: String,
    },
    /// Change fields (`updateOne` with `$set` / `$unset`).
    Update {
        id: String,
        changes: Vec<FieldChange>,
    },
    /// Insert a new document (`insertOne`); without `_id` the server assigns an ObjectId.
    Insert { document: String },
    /// Delete the document (`deleteOne`); with `previous`, only if it is still that document.
    Delete {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        previous: Option<String>,
    },
}

impl DocumentEdit {
    /// Checks the shape: documents are objects nested at most [`MAX_JSON_DEPTH`] deep; a
    /// replacement keeps `_id`; an update has at least one change, valid paths, no path twice
    /// and no path inside another changed path.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Replace {
                id,
                previous,
                document,
            } => {
                let id = check_id(id)?;
                check_document("previous", previous)?;
                let document = check_document("document", document)?;
                if let Some(new_id) = document.get("_id")
                    && new_id != &id
                {
                    return Err(Error::invalid_input(
                        "a replacement cannot change _id; insert a new document instead",
                    ));
                }
            }
            Self::Update { id, changes } => {
                check_id(id)?;
                if changes.is_empty() {
                    return Err(Error::invalid_input("an update needs at least one change"));
                }
                for (i, change) in changes.iter().enumerate() {
                    check_path(&change.path)?;
                    if change.path == "_id" || change.path.starts_with("_id.") {
                        return Err(Error::invalid_input("_id cannot be changed by an update"));
                    }
                    for value in [&change.previous, &change.value] {
                        if let FieldValue::Present { value } = value {
                            check_depth(&parse("a field value", value)?)?;
                        }
                    }
                    for other in &changes[..i] {
                        if paths_overlap(&other.path, &change.path) {
                            return Err(Error::invalid_input(format!(
                                "the changes of `{}` and `{}` overlap",
                                other.path, change.path
                            )));
                        }
                    }
                }
            }
            Self::Insert { document } => {
                check_document("document", document)?;
            }
            Self::Delete { id, previous } => {
                check_id(id)?;
                if let Some(previous) = previous {
                    check_document("previous", previous)?;
                }
            }
        }
        Ok(())
    }

    /// The `_id` text the edit targets (`None` for an insert).
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Replace { id, .. } | Self::Update { id, .. } | Self::Delete { id, .. } => {
                Some(id.as_str())
            }
            Self::Insert { .. } => None,
        }
    }
}

/// What happened to one edit of a [`DocumentApplyReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DocumentEditStatus {
    /// Written (and kept: document edits are not transactional in M7).
    Applied,
    /// The document changed or went away since it was read; nothing written.
    Conflict,
    /// The server refused it (validator, duplicate key, ...); nothing written.
    Failed,
    /// Not tried because an earlier edit stopped the apply.
    NotRun,
}

/// One edit's outcome, in the order of the request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentEditOutcome {
    pub index: usize,
    pub status: DocumentEditStatus,
    /// The `_id` of the edited or inserted document (canonical Extended JSON text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The stored document after an applied replace, update or insert (canonical Extended
    /// JSON text), read back in the same call; `None` after a delete or when it could not be
    /// read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<String>,
    /// Why a `failed` or `conflict` edit did not apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The result of `document_apply`: edits run in order, each on its own (no transaction); the
/// first conflict or failure stops the rest (`notRun`). Applied edits stay applied.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentApplyReport {
    pub outcomes: Vec<DocumentEditOutcome>,
}

impl DocumentApplyReport {
    /// Edits that were written.
    pub fn applied(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| o.status == DocumentEditStatus::Applied)
            .count()
    }
}

/// Validates a whole apply request: at most [`MAX_DOCUMENT_EDITS`] edits, each valid.
pub fn validate_document_edits(edits: &[DocumentEdit]) -> Result<()> {
    if edits.is_empty() {
        return Err(Error::invalid_input("there are no changes to apply"));
    }
    if edits.len() > MAX_DOCUMENT_EDITS {
        return Err(Error::invalid_input(format!(
            "at most {MAX_DOCUMENT_EDITS} document changes can be applied at once"
        )));
    }
    for (i, edit) in edits.iter().enumerate() {
        match edit.validate() {
            Ok(()) => {}
            Err(Error::InvalidInput { message }) => {
                return Err(Error::invalid_input(format!("change {}: {message}", i + 1)));
            }
            Err(other) => return Err(other),
        }
    }
    Ok(())
}

fn check_collection(collection: &ObjectRef) -> Result<()> {
    if !matches!(collection.kind, ObjectKind::Table | ObjectKind::View) {
        return Err(Error::invalid_input(format!(
            "a document query reads a collection or a view, not a {}",
            collection.kind.display_name()
        )));
    }
    if collection.name.is_empty() {
        return Err(Error::invalid_input("the collection has no name"));
    }
    if collection.catalog.as_deref().is_none_or(str::is_empty) {
        return Err(Error::invalid_input("the collection has no database"));
    }
    Ok(())
}

fn check_object(what: &str, value: Option<&Json>) -> Result<()> {
    match value {
        None => Ok(()),
        Some(v @ Json::Object(_)) => check_depth(v),
        Some(_) => Err(Error::invalid_input(format!(
            "{what} must be a JSON object"
        ))),
    }
}

/// `text` parsed as JSON (`InvalidInput` naming `what` when it is not).
fn parse(what: &str, text: &str) -> Result<Json> {
    serde_json::from_str(text)
        .map_err(|e| Error::invalid_input(format!("{what} is not valid JSON: {e}")))
}

/// `text` parsed as a JSON object nested at most [`MAX_JSON_DEPTH`] deep.
fn check_document(what: &str, text: &str) -> Result<Json> {
    let value = parse(what, text)?;
    if !value.is_object() {
        return Err(Error::invalid_input(format!(
            "{what} must be a JSON object"
        )));
    }
    check_depth(&value)?;
    Ok(value)
}

/// `text` parsed as an `_id`: anything but null and arrays.
fn check_id(text: &str) -> Result<Json> {
    match parse("_id", text)? {
        Json::Null => Err(Error::invalid_input("the document has no _id")),
        Json::Array(_) => Err(Error::invalid_input("_id cannot be an array")),
        other => {
            check_depth(&other)?;
            Ok(other)
        }
    }
}

/// Fails when `value` nests deeper than [`MAX_JSON_DEPTH`] (iterative: no recursion limit).
fn check_depth(value: &Json) -> Result<()> {
    let mut stack: Vec<(&Json, usize)> = vec![(value, 1)];
    while let Some((v, depth)) = stack.pop() {
        if depth > MAX_JSON_DEPTH {
            return Err(Error::invalid_input(format!(
                "the document is nested deeper than {MAX_JSON_DEPTH} levels"
            )));
        }
        match v {
            Json::Array(items) => stack.extend(items.iter().map(|i| (i, depth + 1))),
            Json::Object(map) => stack.extend(map.values().map(|i| (i, depth + 1))),
            _ => {}
        }
    }
    Ok(())
}

/// A dotted field path: non-empty segments, none starting with `$`, no NUL.
fn check_path(path: &str) -> Result<()> {
    if path.is_empty() || path.contains('\0') {
        return Err(Error::invalid_input("a field path cannot be empty"));
    }
    for segment in path.split('.') {
        if segment.is_empty() {
            return Err(Error::invalid_input(format!(
                "the field path `{path}` has an empty segment"
            )));
        }
        if segment.starts_with('$') {
            return Err(Error::invalid_input(format!(
                "the field path `{path}` has a segment starting with $"
            )));
        }
    }
    Ok(())
}

/// `true` when one path equals the other or lies inside it (`a` and `a.b`).
fn paths_overlap(a: &str, b: &str) -> bool {
    let inside = |outer: &str, inner: &str| {
        inner.len() > outer.len()
            && inner.starts_with(outer)
            && inner.as_bytes()[outer.len()] == b'.'
    };
    a == b || inside(a, b) || inside(b, a)
}

/// A JSON object from `(key, value)` pairs (test and builder convenience).
pub fn json_object<I, K>(pairs: I) -> Json
where
    I: IntoIterator<Item = (K, Json)>,
    K: Into<String>,
{
    Json::Object(
        pairs
            .into_iter()
            .map(|(k, v)| (k.into(), v))
            .collect::<Map<_, _>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn coll() -> ObjectRef {
        ObjectRef::table(Some("oxbase_test"), None, "m7_docs")
    }

    #[test]
    fn find_requests_round_trip_in_camel_case() {
        let q = DocumentQuery::Find(FindRequest {
            filter: Some(json!({"n": {"$gt": 1}})),
            limit: Some(10),
            max_time_ms: Some(500),
            ..FindRequest::new(coll())
        });
        let text = serde_json::to_value(&q).unwrap();
        assert_eq!(text["type"], "find");
        assert_eq!(text["maxTimeMs"], 500);
        assert!(text.get("projection").is_none());
        let back: DocumentQuery = serde_json::from_value(text).unwrap();
        assert_eq!(back, q);
        assert!(q.validate().is_ok());
        assert!(!q.writes());
    }

    #[test]
    fn request_shapes_are_checked() {
        let mut f = FindRequest {
            filter: Some(json!([1])),
            ..FindRequest::new(coll())
        };
        assert!(DocumentQuery::Find(f.clone()).validate().is_err());
        f.filter = None;
        f.hint = Some(json!(3));
        assert!(DocumentQuery::Find(f.clone()).validate().is_err());
        f.hint = Some(json!("m7_docs_name_idx"));
        assert!(DocumentQuery::Find(f.clone()).validate().is_ok());
        f.collection.catalog = None;
        assert!(DocumentQuery::Find(f.clone()).validate().is_err());
        f.collection = ObjectRef::new(ObjectKind::Key, "k").in_catalog(Some("0"));
        assert!(DocumentQuery::Find(f).validate().is_err());

        let a =
            |pipeline: Vec<Json>| DocumentQuery::Aggregate(AggregateRequest::new(coll(), pipeline));
        assert!(a(vec![json!({"$match": {}})]).validate().is_ok());
        assert!(
            a(vec![json!({"$match": {}, "$limit": 1})])
                .validate()
                .is_err()
        );
        assert!(a(vec![json!({"match": {}})]).validate().is_err());
        assert!(a(vec![json!("$match")]).validate().is_err());
        assert!(a(vec![json!({"$out": "x"})]).writes());
        assert!(a(vec![json!({"$merge": {"into": "x"}})]).writes());
    }

    #[test]
    fn depth_is_limited_without_recursion() {
        let mut deep = json!(1);
        for _ in 0..=MAX_JSON_DEPTH {
            deep = json!({ "a": deep });
        }
        let q = DocumentQuery::Find(FindRequest {
            filter: Some(deep),
            ..FindRequest::new(coll())
        });
        assert!(q.validate().is_err());
    }

    fn present(text: &str) -> FieldValue {
        FieldValue::Present {
            value: text.to_owned(),
        }
    }

    #[test]
    fn edits_round_trip_and_validate() {
        let id = r#"{"$oid": "65a000000000000000000001"}"#.to_owned();
        let update = DocumentEdit::Update {
            id: id.clone(),
            changes: vec![
                FieldChange {
                    path: "meta.city".to_owned(),
                    previous: present(r#""Oslo""#),
                    value: present(r#""Bergen""#),
                },
                FieldChange {
                    path: "note".to_owned(),
                    previous: FieldValue::Missing,
                    value: FieldValue::Missing,
                },
            ],
        };
        let text = serde_json::to_value(&update).unwrap();
        assert_eq!(text["type"], "update");
        assert_eq!(text["changes"][0]["previous"]["state"], "present");
        assert_eq!(text["changes"][1]["value"]["state"], "missing");
        let back: DocumentEdit = serde_json::from_value(text).unwrap();
        assert_eq!(back, update);
        assert!(update.validate().is_ok());
        assert_eq!(update.id(), Some(id.as_str()));

        let bad_path = |path: &str| DocumentEdit::Update {
            id: id.clone(),
            changes: vec![FieldChange {
                path: path.to_owned(),
                previous: FieldValue::Missing,
                value: present("1"),
            }],
        };
        for path in ["", "a..b", "$set", "a.$b", "_id", "_id.x"] {
            assert!(bad_path(path).validate().is_err(), "{path}");
        }
        let overlapping = DocumentEdit::Update {
            id: id.clone(),
            changes: vec![
                FieldChange {
                    path: "a".to_owned(),
                    previous: FieldValue::Missing,
                    value: FieldValue::Missing,
                },
                FieldChange {
                    path: "a.b".to_owned(),
                    previous: FieldValue::Missing,
                    value: FieldValue::Missing,
                },
            ],
        };
        assert!(overlapping.validate().is_err());
        assert!(!paths_overlap("ab", "a.b"));
        assert!(!paths_overlap("a", "ab"));

        let replace = DocumentEdit::Replace {
            id: id.clone(),
            previous: format!(r#"{{"_id": {id}, "a": 1}}"#),
            document: r#"{"_id": {"$oid": "65a000000000000000000002"}, "a": 2}"#.to_owned(),
        };
        assert!(replace.validate().is_err());
        let keeps_id = DocumentEdit::Replace {
            id: id.clone(),
            previous: format!(r#"{{"_id": {id}, "a": 1}}"#),
            document: r#"{"a": 2}"#.to_owned(),
        };
        assert!(keeps_id.validate().is_ok());
        let insert = |document: &str| DocumentEdit::Insert {
            document: document.to_owned(),
        };
        assert!(insert("[1]").validate().is_err());
        assert!(insert("{").validate().is_err());
        assert!(insert(r#"{"a": 1}"#).validate().is_ok());
        let delete = |id: &str| DocumentEdit::Delete {
            id: id.to_owned(),
            previous: None,
        };
        assert!(delete("null").validate().is_err());
        assert!(delete("[1]").validate().is_err());
        assert!(delete("7").validate().is_ok());
        assert!(validate_document_edits(&[]).is_err());
        assert!(validate_document_edits(&[update]).is_ok());
    }

    #[test]
    fn reports_count_applied_edits() {
        let report = DocumentApplyReport {
            outcomes: vec![
                DocumentEditOutcome {
                    index: 0,
                    status: DocumentEditStatus::Applied,
                    id: Some("1".to_owned()),
                    document: None,
                    error: None,
                },
                DocumentEditOutcome {
                    index: 1,
                    status: DocumentEditStatus::Conflict,
                    id: Some("2".to_owned()),
                    document: None,
                    error: Some("changed".to_owned()),
                },
            ],
        };
        assert_eq!(report.applied(), 1);
        let text = serde_json::to_value(&report).unwrap();
        assert_eq!(text["outcomes"][1]["status"], "conflict");
        assert_eq!(
            json_object([("a", json!(1))]),
            json!({"a": 1}),
            "json_object builds objects"
        );
    }
}
