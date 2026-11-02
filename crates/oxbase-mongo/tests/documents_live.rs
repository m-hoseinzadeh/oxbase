//! Live tests of the MongoDB document store and import loader (mongo stream,
//! docs/m7-contracts.md §11 *query* and *edit*) against the `mongodb` fixture with the M7 seed.
//!
//! All are `#[ignore]`d: the conformance CI job runs them with
//! `cargo test -p oxbase-mongo -- --include-ignored`. Edits only touch documents of `m7_edit`
//! whose `_id` is `"t<token>-<n>"` (never the seed's 1-3) and loads go to collections
//! `m7load_t<token>_*`; both are removed at the end, and leftovers older than an hour at
//! start.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_array::cast::AsArray;
use arrow_array::types::Int64Type;
use arrow_array::{Array, ArrayRef, Int64Array, RecordBatch, StringArray};
use arrow_schema::DataType as ArrowType;
use bson::{Bson, Document, doc};
use futures::StreamExt;
use oxbase_dialect::document::{DOCUMENT_COLUMN, META_BSON_TYPE, META_BSON_TYPES, META_ROLE};
use oxbase_dialect::{BulkLoader, DocumentStore, LoadColumn, LoadSession, LoadTarget};
use oxbase_driver::{CancellationToken, ResultItem, ResultSource, collect};
use oxbase_mongo::MongoConnection;
use oxbase_mongo::dialect::{MongoBulkLoader, MongoDocumentStore};
use oxbase_mongo::ejson::parse_document;
use oxbase_types::document::{FieldChange, FieldValue};
use oxbase_types::{
    AggregateRequest, DataType, DocumentEdit, DocumentEditStatus, DocumentQuery, FindRequest,
    ObjectRef,
};
use serde_json::json;

use common::{DATABASE, connect, epoch_secs, token};

const CANCEL_LIMIT: Duration = Duration::from_secs(2);

fn coll(name: &str) -> ObjectRef {
    ObjectRef::table(Some(DATABASE), None, name)
}

async fn query(c: &MongoConnection, request: DocumentQuery) -> oxbase_driver::Collected {
    let source = MongoDocumentStore.query(&request).expect("valid request");
    let stream = source
        .open(c, CancellationToken::new())
        .await
        .expect("open");
    collect(stream).await.expect("collect")
}

fn meta<'a>(field: &'a arrow_schema::Field, key: &str) -> Option<&'a str> {
    field.metadata().get(key).map(String::as_str)
}

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn find_gives_the_column_table_of_the_contract() {
    // A 10-document sample: `only_late` (document 12) is only in the document column.
    let c = connect(Some(10)).await;
    let out = query(&c, DocumentQuery::Find(FindRequest::new(coll("m7_docs")))).await;
    assert_eq!(out.result_sets.len(), 1);
    let set = &out.result_sets[0];
    assert_eq!(set.num_rows(), 12);
    let schema = &set.schema;
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names.first(), Some(&"_id"));
    assert_eq!(names.last(), Some(&DOCUMENT_COLUMN));
    assert!(!names.contains(&"only_late"), "{names:?}");
    let expect = [
        ("_id", ArrowType::Utf8, "objectId"),
        ("name", ArrowType::Utf8, "string"),
        ("n", ArrowType::Int64, "long"),
        ("price", ArrowType::Utf8, "decimal"),
        ("bin", ArrowType::Binary, "binData"),
        ("uuid", ArrowType::Binary, "binData"),
        ("tags", ArrowType::Utf8, "array"),
        ("meta", ArrowType::Utf8, "object"),
        ("mixed", ArrowType::Utf8, "mixed"),
        ("flag", ArrowType::Boolean, "bool"),
    ];
    for (name, arrow, bson_type) in expect {
        let field = schema.field_with_name(name).unwrap();
        assert_eq!(field.data_type(), &arrow, "{name}");
        assert_eq!(meta(field, META_BSON_TYPE), Some(bson_type), "{name}");
    }
    let at = schema.field_with_name("at").unwrap();
    assert!(matches!(at.data_type(), ArrowType::Timestamp(_, Some(tz)) if tz.as_ref() == "UTC"));
    assert_eq!(meta(at, META_BSON_TYPE), Some("date"));
    let mixed = schema.field_with_name("mixed").unwrap();
    let seen = meta(mixed, META_BSON_TYPES).unwrap_or_default();
    for t in ["string", "int", "double", "object", "array"] {
        assert!(seen.contains(t), "{t} not in {seen}");
    }
    let document = schema.field_with_name(DOCUMENT_COLUMN).unwrap();
    assert_eq!(meta(document, META_ROLE), Some("document"));

    // `n` holds int32 1-6 and int64 7-12 as one Int64 column.
    assert_eq!(set.batches.len(), 1, "12 documents fit one batch");
    let batch = &set.batches[0];
    let n = batch
        .column(schema.index_of("n").unwrap())
        .as_primitive::<Int64Type>();
    let values: Vec<i64> = (0..n.len()).map(|i| n.value(i)).collect();
    assert_eq!(values, (1..=12).collect::<Vec<i64>>());

    // The document column is canonical Extended JSON of the whole document (document_at).
    let docs = batch
        .column(schema.index_of(DOCUMENT_COLUMN).unwrap())
        .as_string::<i32>();
    let last = docs.value(11);
    assert!(
        last.contains(r#""$oid":"65a000000000000000000012""#),
        "{last}"
    );
    assert!(last.contains(r#""n":{"$numberLong":"12"}"#), "{last}");
    assert!(last.contains(r#""only_late":"late""#), "{last}");
    let parsed = parse_document(last).unwrap();
    assert_eq!(parsed.get("n"), Some(&Bson::Int64(12)));
    assert!(matches!(parsed.get("price"), Some(Bson::Decimal128(_))));
}

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn aggregates_group_and_views_read() {
    let c = connect(None).await;
    let request = AggregateRequest::new(
        coll("m7_big"),
        vec![json!({"$group": {"_id": null, "total": {"$sum": "$g"}, "n": {"$sum": 1}}})],
    );
    let out = query(&c, DocumentQuery::Aggregate(request)).await;
    let set = &out.result_sets[0];
    assert_eq!(set.num_rows(), 1);
    let batch = &set.batches[0];
    let total = batch.column(set.schema.index_of("total").unwrap());
    let total = match total.data_type() {
        ArrowType::Int64 => total.as_primitive::<Int64Type>().value(0),
        ArrowType::Int32 => i64::from(
            total
                .as_primitive::<arrow_array::types::Int32Type>()
                .value(0),
        ),
        other => panic!("total is {other}"),
    };
    assert_eq!(total, 9_900_000);

    let view = ObjectRef::new(oxbase_types::ObjectKind::View, "m7_view").in_catalog(Some(DATABASE));
    let out = query(&c, DocumentQuery::Find(FindRequest::new(view))).await;
    assert_eq!(
        out.result_sets[0].num_rows(),
        6,
        "flag: true in 6 documents"
    );
}

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn m7_big_streams_every_row_and_cancels_within_two_seconds() {
    let c = connect(None).await;
    let request = DocumentQuery::Find(FindRequest::new(coll("m7_big")));
    let source = MongoDocumentStore.query(&request).unwrap();

    let mut stream = source.open(&c, CancellationToken::new()).await.unwrap();
    let mut rows = 0usize;
    let mut largest = 0usize;
    while let Some(item) = stream.next().await {
        if let ResultItem::Batch(b) = item.unwrap() {
            rows += b.num_rows();
            largest = largest.max(b.num_rows());
        }
    }
    assert_eq!(rows, 200_000);
    assert!(largest <= 1000, "batches of at most 1000 rows: {largest}");

    // Cancel mid-stream: one `Cancelled`, then the end, within 2 s.
    let token = CancellationToken::new();
    let mut stream = source.open(&c, token.clone()).await.unwrap();
    let mut seen = 0usize;
    while seen < 5000 {
        match stream.next().await {
            Some(Ok(ResultItem::Batch(b))) => seen += b.num_rows(),
            Some(Ok(_)) => {}
            other => panic!("ended early: {other:?}"),
        }
    }
    let started = Instant::now();
    token.cancel();
    let mut cancelled = false;
    while let Some(item) = stream.next().await {
        if let Err(e) = item {
            assert!(e.is_cancelled(), "{e}");
            cancelled = true;
        }
    }
    assert!(cancelled, "the stream ends with Cancelled");
    assert!(started.elapsed() < CANCEL_LIMIT, "{:?}", started.elapsed());

    // The session still works.
    let out = query(
        &c,
        DocumentQuery::Find({
            let mut f = FindRequest::new(coll("m7_big"));
            f.filter = Some(json!({"g": 7}));
            f.sort = Some(json!({"_id": 1}));
            f.limit = Some(5);
            f
        }),
    )
    .await;
    assert_eq!(out.result_sets[0].num_rows(), 5);
}

/// Removes documents of `m7_edit` written by test runs older than an hour.
async fn drop_stale_edits(c: &MongoConnection) {
    let coll = c
        .client()
        .database(DATABASE)
        .collection::<Document>("m7_edit");
    let mut cursor = coll
        .find(doc! {"_id": {"$regex": "^t[0-9]+x[0-9]+-"}})
        .await
        .unwrap();
    let now = epoch_secs();
    while let Some(Ok(d)) = cursor.next().await {
        let Ok(id) = d.get_str("_id") else { continue };
        let born: u64 = id[1..]
            .split('x')
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        if now.saturating_sub(born) > 3600 {
            let _ = coll.delete_one(doc! {"_id": id}).await;
        }
    }
}

fn present(text: &str) -> FieldValue {
    FieldValue::Present {
        value: text.to_owned(),
    }
}

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn documents_insert_update_replace_and_delete_with_conflicts() {
    let c = connect(None).await;
    drop_stale_edits(&c).await;
    let t = token();
    let edit = coll("m7_edit");
    let id1 = format!(r#""t{t}-1""#);
    let id2 = format!(r#""t{t}-2""#);
    let store = MongoDocumentStore;
    let apply = |edits: Vec<DocumentEdit>| {
        let c = &c;
        let edit = edit.clone();
        async move {
            store
                .apply(c, &edit, &edits, CancellationToken::new())
                .await
                .expect("apply")
        }
    };

    // Insert two documents; the outcome carries the stored canonical text.
    let report = apply(vec![
        DocumentEdit::Insert {
            document: format!(r#"{{"_id": {id1}, "name": "a", "v": 1, "meta": {{"x": 1}}}}"#),
        },
        DocumentEdit::Insert {
            document: format!(r#"{{"_id": {id2}, "name": "b", "v": {{"$numberLong": "2"}}}}"#),
        },
    ])
    .await;
    assert_eq!(report.applied(), 2, "{report:?}");
    let stored1 = report.outcomes[0].document.clone().expect("read back");
    assert_eq!(
        stored1,
        format!(
            r#"{{"_id":{id1},"name":"a","v":{{"$numberInt":"1"}},"meta":{{"x":{{"$numberInt":"1"}}}}}}"#
        )
    );
    assert_eq!(report.outcomes[0].id.as_deref(), Some(id1.as_str()));

    // A duplicate _id fails, and the change after it does not run.
    let report = apply(vec![
        DocumentEdit::Insert {
            document: format!(r#"{{"_id": {id1}}}"#),
        },
        DocumentEdit::Delete {
            id: id2.clone(),
            previous: None,
        },
    ])
    .await;
    assert_eq!(report.outcomes[0].status, DocumentEditStatus::Failed);
    assert!(report.outcomes[0].error.is_some());
    assert_eq!(report.outcomes[1].status, DocumentEditStatus::NotRun);

    // Update: change a field, add a missing one, remove one.
    let report = apply(vec![DocumentEdit::Update {
        id: id1.clone(),
        changes: vec![
            FieldChange {
                path: "name".into(),
                previous: present(r#""a""#),
                value: present(r#""a2""#),
            },
            FieldChange {
                path: "meta.y".into(),
                previous: FieldValue::Missing,
                value: present(r#"{"$date": "2026-03-01T00:00:00Z"}"#),
            },
            FieldChange {
                path: "v".into(),
                previous: present("1"),
                value: FieldValue::Missing,
            },
        ],
    }])
    .await;
    assert_eq!(report.applied(), 1, "{report:?}");
    let stored1 = report.outcomes[0].document.clone().unwrap();
    let parsed = parse_document(&stored1).unwrap();
    assert_eq!(parsed.get_str("name").unwrap(), "a2");
    assert!(parsed.get("v").is_none());
    assert!(matches!(
        parsed.get_document("meta").unwrap().get("y"),
        Some(Bson::DateTime(_))
    ));

    // A stale update (the field is no longer "a"; "meta.y" is no longer missing) conflicts.
    for stale in [
        FieldChange {
            path: "name".into(),
            previous: present(r#""a""#),
            value: present(r#""zzz""#),
        },
        FieldChange {
            path: "meta.y".into(),
            previous: FieldValue::Missing,
            value: present("1"),
        },
    ] {
        let report = apply(vec![DocumentEdit::Update {
            id: id1.clone(),
            changes: vec![stale],
        }])
        .await;
        assert_eq!(report.outcomes[0].status, DocumentEditStatus::Conflict);
    }

    // Replace with the current text applies; with the old text it conflicts and writes nothing.
    let report = apply(vec![DocumentEdit::Replace {
        id: id1.clone(),
        previous: stored1.clone(),
        document: r#"{"name": "replaced", "w": [1, 2]}"#.into(),
    }])
    .await;
    assert_eq!(report.applied(), 1, "{report:?}");
    let replaced = report.outcomes[0].document.clone().unwrap();
    assert!(replaced.contains(r#""name":"replaced""#), "{replaced}");
    let report = apply(vec![
        DocumentEdit::Replace {
            id: id1.clone(),
            previous: stored1.clone(),
            document: r#"{"name": "lost"}"#.into(),
        },
        DocumentEdit::Delete {
            id: id2.clone(),
            previous: None,
        },
    ])
    .await;
    assert_eq!(report.outcomes[0].status, DocumentEditStatus::Conflict);
    assert_eq!(report.outcomes[1].status, DocumentEditStatus::NotRun);
    let now = store
        .get(&c, &edit, &id1, CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(now, replaced, "a conflict changes nothing");

    // Field order counts in the optimistic check.
    let reordered = {
        let mut d = parse_document(&replaced).unwrap();
        let name = d.remove("name").unwrap();
        d.insert("name", name);
        oxbase_mongo::ejson::to_canonical_text(&Bson::Document(d))
    };
    let report = apply(vec![DocumentEdit::Delete {
        id: id1.clone(),
        previous: Some(reordered),
    }])
    .await;
    assert_eq!(report.outcomes[0].status, DocumentEditStatus::Conflict);

    // Delete with the current text, then without a check.
    let report = apply(vec![
        DocumentEdit::Delete {
            id: id1.clone(),
            previous: Some(replaced),
        },
        DocumentEdit::Delete {
            id: id2.clone(),
            previous: None,
        },
    ])
    .await;
    assert_eq!(report.applied(), 2, "{report:?}");
    assert!(report.outcomes[0].document.is_none());
    for id in [&id1, &id2] {
        let gone = store
            .get(&c, &edit, id, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(gone, None);
    }
    // Deleting again is a conflict (the document went away).
    let report = apply(vec![DocumentEdit::Delete {
        id: id1.clone(),
        previous: None,
    }])
    .await;
    assert_eq!(report.outcomes[0].status, DocumentEditStatus::Conflict);

    // The seed documents are untouched.
    let seed = store
        .get(&c, &edit, "1", CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert!(seed.contains(r#""name":"seed-1""#), "{seed}");
}

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn get_reads_one_document_by_id() {
    let c = connect(None).await;
    let docs = coll("m7_docs");
    let found = MongoDocumentStore
        .get(
            &c,
            &docs,
            r#"{"$oid": "65a000000000000000000003"}"#,
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .expect("document 3");
    let parsed = parse_document(&found).unwrap();
    assert_eq!(parsed.get_str("name").unwrap(), "doc-03");
    assert_eq!(parsed.get("mixed"), Some(&Bson::Double(3.5)));
    let none = MongoDocumentStore
        .get(&c, &docs, r#""no such id""#, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(none, None);
}

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn the_loader_creates_a_collection_and_inserts_rows() {
    let c = connect(None).await;
    let db = c.client().database(DATABASE);
    let now = epoch_secs();
    for name in db.list_collection_names().await.unwrap() {
        let born: u64 = name
            .strip_prefix("m7load_t")
            .and_then(|r| r.split('x').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(u64::MAX);
        if born != u64::MAX && now.saturating_sub(born) > 3600 {
            let _ = db.collection::<Document>(&name).drop().await;
        }
    }
    let name = format!("m7load_t{}_rows", token());
    let table = coll(&name);
    let columns = vec![
        LoadColumn::new("k", DataType::INT64),
        LoadColumn::new("label", DataType::Text),
        LoadColumn::new("doc", DataType::Json { binary: true }),
    ];
    let version = c_version(&c);
    let target = LoadTarget {
        table: &table,
        columns: &columns,
        version: &version,
    };
    let loader = MongoBulkLoader;
    loader
        .create_target(
            LoadSession::new(&c, None),
            &target,
            CancellationToken::new(),
        )
        .await
        .expect("create_target is offered")
        .expect("create");
    let again = loader
        .create_target(
            LoadSession::new(&c, None),
            &target,
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .expect_err("exists");
    assert!(
        matches!(again, oxbase_types::Error::InvalidInput { .. }),
        "{again}"
    );

    let rows = 2500;
    let batch = RecordBatch::try_new(
        target.schema(),
        vec![
            Arc::new(Int64Array::from_iter_values(0..rows)) as ArrayRef,
            Arc::new(StringArray::from_iter(
                (0..rows).map(|i| (i % 10 != 0).then(|| format!("row-{i}"))),
            )) as ArrayRef,
            Arc::new(StringArray::from_iter_values((0..rows).map(|i| {
                format!(r#"{{"i": {i}, "id": {{"$oid": "65a0000000000000000000ff"}}}}"#)
            }))) as ArrayRef,
        ],
    )
    .unwrap();
    let written = loader
        .load(
            LoadSession::new(&c, None),
            &target,
            &batch,
            CancellationToken::new(),
        )
        .await
        .expect("load");
    assert_eq!(written, 2500);
    let coll = db.collection::<Document>(&name);
    assert_eq!(coll.count_documents(doc! {}).await.unwrap(), 2500);
    let d = coll.find_one(doc! {"k": 10_i64}).await.unwrap().unwrap();
    assert_eq!(d.get("label"), Some(&Bson::Null));
    let nested = d.get_document("doc").unwrap();
    assert!(matches!(nested.get("id"), Some(Bson::ObjectId(_))));
    let keys: Vec<&String> = d.keys().collect();
    assert_eq!(keys, ["_id", "k", "label", "doc"]);

    // A server error names the row of the batch.
    coll.create_index(
        mongodb::IndexModel::builder()
            .keys(doc! {"k": 1})
            .options(
                mongodb::options::IndexOptions::builder()
                    .unique(true)
                    .build(),
            )
            .build(),
    )
    .await
    .unwrap();
    let dup = RecordBatch::try_new(
        target.schema(),
        vec![
            Arc::new(Int64Array::from(vec![5000, 7])) as ArrayRef,
            Arc::new(StringArray::from(vec![Some("x"), Some("y")])) as ArrayRef,
            Arc::new(StringArray::from(vec![Some("{}"), Some("{}")])) as ArrayRef,
        ],
    )
    .unwrap();
    let err = loader
        .load(
            LoadSession::new(&c, None),
            &target,
            &dup,
            CancellationToken::new(),
        )
        .await
        .expect_err("duplicate key");
    assert!(err.to_string().contains("row 2"), "{err}");
    assert_eq!(coll.count_documents(doc! {}).await.unwrap(), 2501);
    coll.drop().await.unwrap();
}

fn c_version(c: &MongoConnection) -> oxbase_types::ServerVersion {
    use oxbase_driver::Connection;
    c.server_version().clone()
}
