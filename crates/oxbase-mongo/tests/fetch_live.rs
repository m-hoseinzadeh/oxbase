//! Live tests of the MongoDB catalog fetchers (mongo stream, docs/m7-contracts.md §11 *browse*)
//! against the `mongodb` fixture with the M7 seed (`tests/docker/init/mongodb/02-m7.js`).
//!
//! All are `#[ignore]`d: the conformance CI job runs them with
//! `cargo test -p oxbase-mongo -- --include-ignored`. They only read.

mod common;

use common::{DATABASE, connect, fixtures, list};
use oxbase_dialect::Fetcher;
use oxbase_driver::CancellationToken;
use oxbase_mongo::dialect::MongoFetcher;
use oxbase_mongo::extras;
use oxbase_types::{
    DataType, Error, ObjectDef, ObjectKind, ObjectRef, ObjectSummary, Value, props,
};

async fn list_of(
    c: &oxbase_mongo::MongoConnection,
    kind: ObjectKind,
    parent: &ObjectRef,
) -> Vec<ObjectSummary> {
    MongoFetcher::new(kind)
        .list(c, parent, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("list {kind:?} of {parent}: {e}"))
}

async fn load(c: &oxbase_mongo::MongoConnection, kind: ObjectKind, obj: &ObjectRef) -> ObjectDef {
    MongoFetcher::new(kind)
        .load(c, obj, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("load {obj}: {e}"))
}

fn names(rows: &[ObjectSummary]) -> Vec<&str> {
    rows.iter().map(|r| r.obj.name.as_str()).collect()
}

fn find<'a>(rows: &'a [ObjectSummary], name: &str) -> &'a ObjectSummary {
    rows.iter()
        .find(|r| r.obj.name == name)
        .unwrap_or_else(|| panic!("{name} is not listed: {:?}", names(rows)))
}

fn docs_ref() -> ObjectRef {
    ObjectRef::table(Some(DATABASE), None, "m7_docs")
}

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn the_tree_lists_databases_collections_views_users_and_roles() {
    let f = fixtures();
    let c = connect(None).await;

    let databases = list_of(&c, ObjectKind::Database, &ObjectRef::root()).await;
    let test = find(&databases, DATABASE);
    assert!(!test.system);
    assert_eq!(test.obj, ObjectRef::database(DATABASE));
    assert!(find(&databases, "admin").system);

    let db = ObjectRef::database(DATABASE);
    let collections = list_of(&c, ObjectKind::Table, &db).await;
    for name in list(&f, &["m7", "mongodb", "collections"]) {
        let row = find(&collections, &name);
        assert_eq!(row.obj.kind, ObjectKind::Table);
        assert_eq!(row.obj.catalog.as_deref(), Some(DATABASE));
    }
    let big = find(&collections, "m7_big");
    assert_eq!(big.row_estimate, Some(200_000));
    assert_eq!(
        big.extras.get(&props::DOCUMENT_COUNT),
        Some(&Value::UInt(200_000))
    );
    assert!(
        !names(&collections).contains(&"m7_view"),
        "views are not collections"
    );

    let views = list_of(&c, ObjectKind::View, &db).await;
    let view = find(&views, "m7_view");
    assert_eq!(
        view.extras.get(&extras::VIEW_ON),
        Some(&Value::Text("m7_docs".into()))
    );

    let users = list_of(&c, ObjectKind::User, &ObjectRef::database("admin")).await;
    let reader = find(&users, "m7_reader");
    let roles = format!("{:?}", reader.extras.get(&extras::ROLES));
    assert!(roles.contains("oxbase_test"), "{roles}");
    // Users of oxbase_test itself (none are defined there): the listing still works.
    list_of(&c, ObjectKind::User, &db).await;

    let roles = list_of(&c, ObjectKind::Role, &db).await;
    assert!(!find(&roles, "m7_role").system);
    assert!(find(&roles, "read").system, "built-in roles are system");
}

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn m7_docs_has_its_five_indexes() {
    let f = fixtures();
    let c = connect(None).await;
    let indexes = list_of(&c, ObjectKind::Index, &docs_ref()).await;
    let mut expected: Vec<String> = list(&f, &["m7", "mongodb", "indexes"])
        .into_iter()
        .filter(|n| n.starts_with("m7_docs"))
        .collect();
    expected.push("_id_".to_owned());
    expected.sort();
    assert_eq!(names(&indexes), expected);
    for row in &indexes {
        assert_eq!(row.obj.kind, ObjectKind::Index);
        assert_eq!(row.obj.parent.as_deref(), Some("m7_docs"));
        assert_eq!(row.obj.catalog.as_deref(), Some(DATABASE));
    }
    let id = find(&indexes, "_id_");
    assert_eq!(id.extras.get(&props::PRIMARY_KEY), Some(&Value::Bool(true)));
    let ttl = find(&indexes, "m7_docs_at_ttl");
    assert!(matches!(
        ttl.extras.get(&extras::EXPIRE_AFTER_SECONDS),
        Some(Value::Int(n)) if *n > 0
    ));

    let unique = load(
        &c,
        ObjectKind::Index,
        &docs_ref().child(ObjectKind::Index, "m7_docs_meta_city_uq"),
    )
    .await;
    let ObjectDef::Index { table, index } = unique else {
        panic!("an index def");
    };
    assert_eq!(table, docs_ref());
    assert!(index.unique);
    assert_eq!(index.columns.len(), 1);
    assert_eq!(index.columns[0].name, "meta.city");
    let predicate = index.predicate.unwrap_or_default();
    assert!(predicate.contains("$exists"), "{predicate}");

    let big = ObjectRef::table(Some(DATABASE), None, "m7_big");
    let big_indexes = list_of(&c, ObjectKind::Index, &big).await;
    assert_eq!(names(&big_indexes), ["_id_", "m7_big_g_idx"]);
}

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn loading_a_collection_infers_typed_columns() {
    let c = connect(None).await;
    let ObjectDef::Table(def) = load(&c, ObjectKind::Table, &docs_ref()).await else {
        panic!("a table def");
    };
    assert_eq!(def.obj, docs_ref());
    assert_eq!(
        def.primary_key.as_ref().map(|k| k.columns.clone()),
        Some(vec!["_id".to_owned()])
    );
    let column = |name: &str| {
        def.column(name)
            .unwrap_or_else(|| panic!("no column {name}: {:?}", def.columns))
    };
    assert_eq!(def.columns[0].name, "_id");
    assert_eq!(column("_id").native_type, "objectId");
    assert!(!column("_id").nullable);
    assert_eq!(column("name").data_type, DataType::Text);
    assert_eq!(column("n").data_type, DataType::INT64);
    assert_eq!(column("n").native_type, "long");
    assert!(matches!(
        column("price").data_type,
        DataType::Decimal { .. }
    ));
    assert!(matches!(
        column("at").data_type,
        DataType::TimestampTz { .. }
    ));
    assert_eq!(column("uuid").data_type, DataType::Uuid);
    assert!(matches!(
        column("bin").data_type,
        DataType::VarBinary { .. }
    ));
    assert_eq!(column("flag").data_type, DataType::Bool);
    for json in ["tags", "meta", "mixed"] {
        assert!(
            matches!(column(json).data_type, DataType::Json { .. }),
            "{json}: {:?}",
            column(json).data_type
        );
    }
    assert_eq!(column("mixed").native_type, "mixed");
    assert!(column("mixed").nullable);
    // The default sample (1000) covers all 12 documents.
    assert!(def.column("only_late").is_some());
    let index_names: Vec<&str> = def.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(index_names.len(), 5, "{index_names:?}");
    assert_eq!(
        def.extras.get(&props::DOCUMENT_COUNT),
        Some(&Value::UInt(12))
    );
    assert_eq!(def.extras.get(&extras::CAPPED), Some(&Value::Bool(false)));
    assert!(def.extras.get(&extras::OPTIONS).is_some());
    assert!(matches!(
        def.extras.get(&props::SIZE_BYTES),
        Some(Value::Int(n)) if *n > 0
    ));

    let missing = MongoFetcher::new(ObjectKind::Table)
        .load(
            &c,
            &ObjectRef::table(Some(DATABASE), None, "m7_no_such_collection"),
            CancellationToken::new(),
        )
        .await
        .expect_err("missing collection");
    assert!(matches!(missing, Error::NotFound { .. }), "{missing}");
}

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn views_databases_users_and_roles_load() {
    let c = connect(None).await;
    let view = ObjectRef::new(ObjectKind::View, "m7_view").in_catalog(Some(DATABASE));
    let ObjectDef::View(def) = load(&c, ObjectKind::View, &view).await else {
        panic!("a view def");
    };
    assert!(def.definition.starts_with('['), "{}", def.definition);
    assert!(def.definition.contains("$match"), "{}", def.definition);
    assert_eq!(
        def.extras.get(&extras::VIEW_ON),
        Some(&Value::Text("m7_docs".into()))
    );

    let ObjectDef::Schema(db) =
        load(&c, ObjectKind::Database, &ObjectRef::database(DATABASE)).await
    else {
        panic!("a database def");
    };
    assert!(matches!(db.extras.get(&props::SIZE_BYTES), Some(Value::Int(n)) if *n > 0));

    let user = ObjectRef::new(ObjectKind::User, "m7_reader").in_catalog(Some("admin"));
    let def = load(&c, ObjectKind::User, &user).await;
    let ObjectDef::Generic(g) = &def else {
        panic!("a generic def");
    };
    assert!(g.extras.get(&extras::ROLES).is_some());
    let text = serde_json::to_string(&def).unwrap();
    for secret in [
        "credentials",
        "SCRAM-SHA-256\":{",
        "storedKey",
        "Oxbase_Fixture_Pw",
    ] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }

    let role = ObjectRef::new(ObjectKind::Role, "m7_role").in_catalog(Some(DATABASE));
    let ObjectDef::Generic(g) = load(&c, ObjectKind::Role, &role).await else {
        panic!("a generic def");
    };
    let privileges = format!("{:?}", g.extras.get(&extras::PRIVILEGES));
    assert!(privileges.contains("find"), "{privileges}");
    assert!(privileges.contains("m7_docs"), "{privileges}");
}

#[tokio::test]
#[ignore = "needs the mongodb fixture with the M7 seed"]
async fn a_cancelled_listing_returns_cancelled() {
    let c = connect(None).await;
    let token = CancellationToken::new();
    token.cancel();
    let err = MongoFetcher::new(ObjectKind::Table)
        .list(&c, &ObjectRef::database(DATABASE), token)
        .await
        .expect_err("cancelled");
    assert!(err.is_cancelled(), "{err}");
}
