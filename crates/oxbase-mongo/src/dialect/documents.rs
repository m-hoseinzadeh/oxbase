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
//!
//! # Update checks
//!
//! The per-field checks of an update are one `$expr` (`$and` of one condition per change) that
//! reads each changed path from `$$ROOT` segment by segment: `$getField` (with a `$literal`
//! name, so names with `$` work) on documents, `$arrayElemAt` for a numeric segment on an
//! array (as `$set: {"tags.0": ...}` addresses it), and *missing* for anything else. A present
//! previous value must equal the stored one (`$eq` with `{$literal: value}`: field order of
//! embedded documents counts, numbers compare by value); a missing one must still be missing
//! (`$type` is `"missing"`, the `$expr` form of `{path: {$exists: false}}`, which also works
//! through array positions).
//!
//! # Order, failures, cancel
//!
//! Every edit's Extended JSON is parsed before the first write, so a malformed edit fails the
//! whole apply with `InvalidInput` and nothing written. Then edits run in order, each on its
//! own; the first conflict or failure stops the rest (`notRun`). The token is checked before
//! each edit (a write already sent is never abandoned halfway: its outcome is reported); a
//! cancel before the first edit is `Err(Cancelled)`, later ones leave the rest `notRun`.

use std::sync::Arc;

use async_trait::async_trait;
use bson::{Bson, Document, doc};
use mongodb::Collection;
use oxbase_dialect::DocumentStore;
use oxbase_driver::{CancellationToken, Connection, ResultSource};
use oxbase_types::document::{FieldChange, FieldValue};
use oxbase_types::{
    DocumentApplyReport, DocumentEdit, DocumentEditOutcome, DocumentEditStatus, DocumentQuery,
    Error, ObjectKind, ObjectRef, Result,
};

use super::fetch::{check, database_of, guarded, map, mongo};
use crate::MongoConnection;
use crate::ejson::{parse_document, parse_value, to_canonical_text};
use crate::request::{RequestSource, request_text};

/// Why a conflicting edit did not apply.
const CONFLICT: &str = "the document was changed or deleted since it was read; reload it";

/// The MongoDB document cell.
#[derive(Debug, Clone, Copy, Default)]
pub struct MongoDocumentStore;

#[async_trait]
impl DocumentStore for MongoDocumentStore {
    fn query(&self, request: &DocumentQuery) -> Result<Arc<dyn ResultSource>> {
        Ok(Arc::new(RequestSource::new(request.clone())?))
    }

    fn request_text(&self, request: &DocumentQuery) -> String {
        request_text(request)
    }

    async fn get(
        &self,
        c: &dyn Connection,
        collection: &ObjectRef,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<Option<String>> {
        check(&cancel)?;
        let conn = mongo(c)?;
        let id = parse_value(id)?;
        let coll = collection_of(conn, collection, &[ObjectKind::Table, ObjectKind::View])?;
        let comment = conn.next_comment();
        let tag = comment.clone();
        let found = guarded(conn, &comment, &cancel, async move {
            coll.find_one(doc! {"_id": id})
                .comment(tag)
                .await
                .map_err(|e| map(&e))
        })
        .await?;
        Ok(found.map(|d| to_canonical_text(&Bson::Document(d))))
    }

    async fn apply(
        &self,
        c: &dyn Connection,
        collection: &ObjectRef,
        edits: &[DocumentEdit],
        cancel: CancellationToken,
    ) -> Result<DocumentApplyReport> {
        check(&cancel)?;
        let conn = mongo(c)?;
        let coll = collection_of(conn, collection, &[ObjectKind::Table])?;
        let prepared = edits
            .iter()
            .enumerate()
            .map(|(i, edit)| {
                Prepared::new(edit).map_err(|e| match e {
                    Error::InvalidInput { message } => {
                        Error::invalid_input(format!("change {}: {message}", i + 1))
                    }
                    other => other,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut report = DocumentApplyReport::default();
        let mut stopped = false;
        for (index, edit) in prepared.into_iter().enumerate() {
            let id = edit.id().map(to_canonical_text);
            if stopped {
                report
                    .outcomes
                    .push(outcome(index, DocumentEditStatus::NotRun, id));
                continue;
            }
            if cancel.is_cancelled() {
                if index == 0 {
                    return Err(Error::Cancelled);
                }
                let mut not_run = outcome(index, DocumentEditStatus::NotRun, id);
                not_run.error = Some("cancelled before this change was sent".to_owned());
                report.outcomes.push(not_run);
                stopped = true;
                continue;
            }
            let done = run(conn, &coll, index, edit).await;
            stopped = done.status != DocumentEditStatus::Applied;
            report.outcomes.push(done);
        }
        Ok(report)
    }
}

/// The collection `obj` names, when it is one of `kinds`.
fn collection_of(
    conn: &MongoConnection,
    obj: &ObjectRef,
    kinds: &[ObjectKind],
) -> Result<Collection<Document>> {
    if !kinds.contains(&obj.kind) || obj.name.is_empty() {
        return Err(Error::invalid_input(format!(
            "{} is not a collection",
            obj.display_name()
        )));
    }
    let db = database_of(conn, obj)?;
    Ok(conn.client().database(&db).collection(&obj.name))
}

fn outcome(index: usize, status: DocumentEditStatus, id: Option<String>) -> DocumentEditOutcome {
    DocumentEditOutcome {
        index,
        status,
        id,
        document: None,
        error: None,
    }
}

/// An edit with its Extended JSON parsed.
#[derive(Debug, Clone)]
enum Prepared {
    Replace {
        id: Bson,
        previous: Document,
        document: Document,
    },
    Update {
        id: Bson,
        check: Document,
        update: Document,
    },
    Insert {
        document: Document,
    },
    Delete {
        id: Bson,
        previous: Option<Document>,
    },
}

impl Prepared {
    fn new(edit: &DocumentEdit) -> Result<Self> {
        Ok(match edit {
            DocumentEdit::Replace {
                id,
                previous,
                document,
            } => {
                let id = parse_value(id)?;
                let document = parse_document(document)?;
                if document.get("_id").is_some_and(|new| new != &id) {
                    return Err(Error::invalid_input(
                        "a replacement cannot change _id; insert a new document instead",
                    ));
                }
                Self::Replace {
                    id,
                    previous: parse_document(previous)?,
                    document,
                }
            }
            DocumentEdit::Update { id, changes } => Self::Update {
                id: parse_value(id)?,
                check: update_check(changes)?,
                update: update_document(changes)?,
            },
            DocumentEdit::Insert { document } => Self::Insert {
                document: parse_document(document)?,
            },
            DocumentEdit::Delete { id, previous } => Self::Delete {
                id: parse_value(id)?,
                previous: previous.as_deref().map(parse_document).transpose()?,
            },
        })
    }

    fn id(&self) -> Option<&Bson> {
        match self {
            Self::Replace { id, .. } | Self::Update { id, .. } | Self::Delete { id, .. } => {
                Some(id)
            }
            Self::Insert { document } => document.get("_id"),
        }
    }
}

/// `{$eq: ["$$ROOT", {$literal: previous}]}`: the stored document is exactly `previous`.
fn unchanged(previous: Document) -> Document {
    doc! {"$eq": ["$$ROOT", {"$literal": previous}]}
}

/// The `$expr` condition of an update: every changed path still has its previous value (or
/// is still missing). See the module docs.
fn update_check(changes: &[FieldChange]) -> Result<Document> {
    let mut conditions = Vec::with_capacity(changes.len());
    for change in changes {
        let value = path_expression(&change.path);
        conditions.push(Bson::Document(match &change.previous {
            FieldValue::Present { value: previous } => {
                doc! {"$eq": [value, {"$literal": parse_value(previous)?}]}
            }
            FieldValue::Missing => doc! {"$eq": [{"$type": value}, "missing"]},
        }));
    }
    Ok(doc! {"$and": conditions})
}

/// `{$set: {...}, $unset: {...}}` of an update (only the operators it needs).
fn update_document(changes: &[FieldChange]) -> Result<Document> {
    let mut set = Document::new();
    let mut unset = Document::new();
    for change in changes {
        match &change.value {
            FieldValue::Present { value } => {
                set.insert(change.path.clone(), parse_value(value)?);
            }
            FieldValue::Missing => {
                unset.insert(change.path.clone(), "");
            }
        }
    }
    let mut update = Document::new();
    if !set.is_empty() {
        update.insert("$set", set);
    }
    if !unset.is_empty() {
        update.insert("$unset", unset);
    }
    Ok(update)
}

/// An aggregation expression that reads dotted `path` from `$$ROOT` (missing when any
/// segment is missing or not a document / array), linear in the path's length.
fn path_expression(path: &str) -> Bson {
    let mut current = Bson::String("$$ROOT".to_owned());
    for segment in path.split('.') {
        let field = doc! {
            "$cond": [
                {"$eq": [{"$type": "$$v"}, "object"]},
                {"$getField": {"field": {"$literal": segment}, "input": "$$v"}},
                "$$REMOVE",
            ]
        };
        let step = match segment.parse::<i32>() {
            Ok(n) if !segment.starts_with('+') && !segment.starts_with('-') => doc! {
                "$cond": [{"$isArray": "$$v"}, {"$arrayElemAt": ["$$v", n]}, field]
            },
            _ => field,
        };
        current = Bson::Document(doc! {"$let": {"vars": {"v": current}, "in": step}});
    }
    current
}

/// Runs one prepared edit; never fails as a whole (see the module docs).
async fn run(
    conn: &MongoConnection,
    coll: &Collection<Document>,
    index: usize,
    edit: Prepared,
) -> DocumentEditOutcome {
    let id = edit.id().map(to_canonical_text);
    let mut out = outcome(index, DocumentEditStatus::Applied, id);
    let written = write(conn, coll, edit).await;
    match written {
        Ok(Some(id)) => {
            out.id = Some(to_canonical_text(&id));
            out.document = read_back(conn, coll, id).await;
        }
        Ok(None) => {}
        Err(Written::Conflict) => {
            out.status = DocumentEditStatus::Conflict;
            out.error = Some(CONFLICT.to_owned());
        }
        Err(Written::Failed(e)) => {
            out.status = DocumentEditStatus::Failed;
            out.error = Some(e.to_string());
        }
    }
    out
}

/// Why a write did not apply.
enum Written {
    Conflict,
    Failed(Error),
}

/// Sends one edit. `Ok(Some(id))`: applied, read the document back; `Ok(None)`: applied
/// delete.
async fn write(
    conn: &MongoConnection,
    coll: &Collection<Document>,
    edit: Prepared,
) -> std::result::Result<Option<Bson>, Written> {
    let comment = conn.next_comment();
    let failed = |e: mongodb::error::Error| Written::Failed(map(&e));
    match edit {
        Prepared::Replace {
            id,
            previous,
            document,
        } => {
            let filter = doc! {"_id": id.clone(), "$expr": unchanged(previous)};
            let result = coll
                .replace_one(filter, document)
                .comment(comment)
                .await
                .map_err(failed)?;
            if result.matched_count == 0 {
                return Err(Written::Conflict);
            }
            Ok(Some(id))
        }
        Prepared::Update { id, check, update } => {
            let filter = doc! {"_id": id.clone(), "$expr": check};
            let result = coll
                .update_one(filter, update)
                .comment(comment)
                .await
                .map_err(failed)?;
            if result.matched_count == 0 {
                return Err(Written::Conflict);
            }
            Ok(Some(id))
        }
        Prepared::Insert { document } => {
            let result = coll
                .insert_one(document)
                .comment(comment)
                .await
                .map_err(failed)?;
            Ok(Some(result.inserted_id))
        }
        Prepared::Delete { id, previous } => {
            let mut filter = doc! {"_id": id};
            if let Some(previous) = previous {
                filter.insert("$expr", unchanged(previous));
            }
            let result = coll
                .delete_one(filter)
                .comment(comment)
                .await
                .map_err(failed)?;
            if result.deleted_count == 0 {
                return Err(Written::Conflict);
            }
            Ok(None)
        }
    }
}

/// The stored document with `_id` `id`, as canonical Extended JSON text (`None` when it cannot
/// be read: the edit is still applied).
async fn read_back(
    conn: &MongoConnection,
    coll: &Collection<Document>,
    id: Bson,
) -> Option<String> {
    match coll
        .find_one(doc! {"_id": id})
        .comment(conn.next_comment())
        .await
    {
        Ok(found) => found.map(|d| to_canonical_text(&Bson::Document(d))),
        Err(e) => {
            tracing::debug!(error = %map(&e), "MongoDB: reading an edited document back failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present(text: &str) -> FieldValue {
        FieldValue::Present {
            value: text.to_owned(),
        }
    }

    #[test]
    fn replace_and_delete_compare_the_whole_document() {
        let edit = DocumentEdit::Replace {
            id: r#"{"$oid": "65a000000000000000000001"}"#.into(),
            previous: r#"{"_id": {"$oid": "65a000000000000000000001"}, "b": 1, "a": 2}"#.into(),
            document: r#"{"b": 3}"#.into(),
        };
        let Prepared::Replace { previous, .. } = Prepared::new(&edit).unwrap() else {
            panic!("replace");
        };
        let check = unchanged(previous);
        let literal = check.get_array("$eq").unwrap()[1]
            .as_document()
            .unwrap()
            .get_document("$literal")
            .unwrap();
        let keys: Vec<&String> = literal.keys().collect();
        assert_eq!(
            keys,
            ["_id", "b", "a"],
            "the previous document keeps its order"
        );

        let changed_id = DocumentEdit::Replace {
            id: "1".into(),
            previous: r#"{"_id": 1}"#.into(),
            document: r#"{"_id": 2}"#.into(),
        };
        assert!(matches!(
            Prepared::new(&changed_id),
            Err(Error::InvalidInput { .. })
        ));
        let delete = DocumentEdit::Delete {
            id: r#""t1-1""#.into(),
            previous: None,
        };
        assert!(matches!(
            Prepared::new(&delete).unwrap(),
            Prepared::Delete { previous: None, .. }
        ));
    }

    #[test]
    fn updates_check_each_changed_field() {
        let changes = vec![
            FieldChange {
                path: "name".into(),
                previous: present(r#""old""#),
                value: present(r#""new""#),
            },
            FieldChange {
                path: "meta.extra".into(),
                previous: FieldValue::Missing,
                value: present(r#"{"$numberLong": "5"}"#),
            },
            FieldChange {
                path: "gone".into(),
                previous: present("1"),
                value: FieldValue::Missing,
            },
        ];
        let update = update_document(&changes).unwrap();
        assert_eq!(
            update,
            doc! {"$set": {"name": "new", "meta.extra": 5_i64}, "$unset": {"gone": ""}}
        );
        let check = update_check(&changes).unwrap();
        let conditions = check.get_array("$and").unwrap();
        assert_eq!(conditions.len(), 3);
        let missing = conditions[1]
            .as_document()
            .unwrap()
            .get_array("$eq")
            .unwrap();
        assert_eq!(missing[1], Bson::String("missing".into()));

        let only_set = update_document(&changes[..1]).unwrap();
        assert!(!only_set.contains_key("$unset"));
    }

    #[test]
    fn path_expressions_nest_one_let_per_segment() {
        let one = path_expression("a");
        let text = to_canonical_text(&one);
        assert!(
            text.starts_with(r#"{"$let":{"vars":{"v":"$$ROOT"}"#),
            "{text}"
        );
        assert!(!text.contains("$arrayElemAt"));
        let indexed = to_canonical_text(&path_expression("tags.0"));
        assert!(indexed.contains("$arrayElemAt"), "{indexed}");
        assert_eq!(indexed.matches("$let").count(), 2);
        // A long path stays linear in size.
        let long = to_canonical_text(&path_expression(&vec!["x"; 50].join(".")));
        assert!(long.len() < 50 * 300, "{}", long.len());
    }

    #[test]
    fn malformed_edits_name_the_part() {
        let edit = DocumentEdit::Insert {
            document: r#"{"_id": {"$oid": "zz"}}"#.into(),
        };
        assert!(matches!(
            Prepared::new(&edit),
            Err(Error::InvalidInput { .. })
        ));
        let edit = DocumentEdit::Insert {
            document: r#"{"a": 1}"#.into(),
        };
        assert!(Prepared::new(&edit).unwrap().id().is_none());
    }
}
