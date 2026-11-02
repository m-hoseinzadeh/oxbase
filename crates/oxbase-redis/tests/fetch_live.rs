//! Live tests of the Redis tree fetchers (docs/m7-contracts.md §11, redis, *browse*): the
//! logical databases, the key-namespace tree under `m7:` with key types and TTLs, database 1,
//! binary key names, truncated listings and ACL users, against the `redis` fixture and its M7
//! seed. `#[ignore]`d (the conformance job runs them); without the M7 seed they are pending
//! unless `OXBASE_REDIS_REQUIRE_M7=1`.

mod support;

use std::collections::HashMap;

use oxbase_dialect::Fetcher;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_redis::dialect::RedisFetcher;
use oxbase_redis::{extras, options};
use oxbase_types::{ObjectDef, ObjectKind, ObjectRef, ObjectSummary, Value, props};
use support::{
    cleanup, connect, fixtures, int, params, params_on, raw, s, seeded, token, with_option,
};

async fn list(
    c: &oxbase_redis::RedisConnection,
    kind: ObjectKind,
    parent: &ObjectRef,
) -> Vec<ObjectSummary> {
    RedisFetcher::new(kind)
        .list(c, parent, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("list {kind} under {parent}: {e}"))
}

async fn load(c: &oxbase_redis::RedisConnection, kind: ObjectKind, obj: &ObjectRef) -> ObjectDef {
    RedisFetcher::new(kind)
        .load(c, obj, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("load {obj}: {e}"))
}

fn by_name(rows: Vec<ObjectSummary>) -> HashMap<String, ObjectSummary> {
    rows.into_iter().map(|r| (r.obj.name.clone(), r)).collect()
}

fn int_prop(row: &ObjectSummary, prop: &oxbase_types::PropId) -> Option<i64> {
    match row.extras.get(prop) {
        Some(Value::Int(n)) => Some(*n),
        _ => None,
    }
}

fn text_prop(extras: &oxbase_types::Extras, prop: &oxbase_types::PropId) -> Option<String> {
    match extras.get(prop) {
        Some(Value::Text(t)) => Some(t.clone()),
        _ => None,
    }
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn databases_list_with_key_counts() {
    let c = connect(&params()).await;
    if !seeded(&c).await {
        return;
    }
    let f = fixtures();
    let rows = list(&c, ObjectKind::Database, &ObjectRef::root()).await;
    let expected = usize::try_from(int(&f, &["m7", "redis", "databases"])).unwrap();
    assert_eq!(rows.len(), expected);
    let names: Vec<&str> = rows.iter().map(|r| r.obj.name.as_str()).collect();
    assert_eq!(names[0], "0");
    assert_eq!(names[15], "15");
    assert!(rows.iter().all(|r| r.obj.kind == ObjectKind::Database));
    assert!(
        rows.iter()
            .all(|r| text_prop(&r.extras, &extras::MODE).as_deref() == Some("standalone"))
    );
    assert!(int_prop(&rows[0], &props::KEY_COUNT).unwrap() > 10);
    assert!(int_prop(&rows[1], &props::KEY_COUNT).unwrap() >= 1);
    let ObjectDef::Schema(def) = load(&c, ObjectKind::Database, &ObjectRef::database("0")).await
    else {
        panic!("a database loads as a schema definition");
    };
    assert!(matches!(def.extras.get(&props::KEY_COUNT), Some(Value::Int(n)) if *n > 10));
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn the_namespace_tree_under_m7_lists_keys_with_types_and_ttls() {
    let c = connect(&params()).await;
    if !seeded(&c).await {
        return;
    }
    let f = fixtures();
    let db = ObjectRef::database("0");
    let top = by_name(list(&c, ObjectKind::Schema, &db).await);
    let root = s(&f, &["m7", "redis", "namespace_root"]);
    let m7 = top
        .get(&root)
        .unwrap_or_else(|| panic!("no {root} in {:?}", top.keys()));
    assert_eq!(m7.obj.kind, ObjectKind::Schema);
    assert_eq!(m7.obj.catalog.as_deref(), Some("0"));
    assert!(int_prop(m7, &props::KEY_COUNT).unwrap() >= 15);

    let under_m7 = by_name(list(&c, ObjectKind::Schema, &m7.obj).await);
    let ns = &under_m7["m7:ns:"];
    assert_eq!(ns.obj.schema.as_deref(), Some("m7:"));
    assert_eq!(int_prop(ns, &props::KEY_COUNT), Some(4));
    assert!(under_m7.contains_key("m7:big:"));
    let keys = by_name(list(&c, ObjectKind::Key, &m7.obj).await);
    for (key, kind) in [
        ("m7:string", "string"),
        ("m7:binary", "string"),
        ("m7:hash", "hash"),
        ("m7:list", "list"),
        ("m7:set", "set"),
        ("m7:zset", "zset"),
        ("m7:stream", "stream"),
    ] {
        let row = keys.get(key).unwrap_or_else(|| panic!("no key {key}"));
        assert_eq!(
            text_prop(&row.extras, &props::KEY_TYPE).as_deref(),
            Some(kind)
        );
        assert_eq!(row.obj.kind, ObjectKind::Key);
        assert_eq!(row.obj.schema.as_deref(), Some("m7:"));
    }
    assert!(
        !keys.contains_key("m7:ns:leaf"),
        "deeper keys stay in their namespace"
    );
    let ttl_key = s(&f, &["m7", "redis", "ttl_key"]);
    let ttl = int_prop(&keys[&ttl_key], &props::TTL_MS).unwrap();
    assert!(ttl > 0 && ttl <= 86_400_000);
    assert_eq!(int_prop(&keys["m7:string"], &props::TTL_MS), None);
    assert!(keys.values().all(|r| !r.extras.contains(&props::TRUNCATED)));

    let ns_children = by_name(list(&c, ObjectKind::Schema, &ns.obj).await);
    let mut names: Vec<&String> = ns_children.keys().collect();
    names.sort();
    assert_eq!(names, ["m7:ns:a:", "m7:ns:b:"]);
    assert_eq!(
        int_prop(&ns_children["m7:ns:a:"], &props::KEY_COUNT),
        Some(2)
    );
    assert_eq!(
        int_prop(&ns_children["m7:ns:b:"], &props::KEY_COUNT),
        Some(1)
    );
    let leaves = by_name(list(&c, ObjectKind::Key, &ns.obj).await);
    assert_eq!(leaves.keys().collect::<Vec<_>>(), ["m7:ns:leaf"]);
    let a_keys = by_name(list(&c, ObjectKind::Key, &ns_children["m7:ns:a:"].obj).await);
    assert_eq!(a_keys.len(), 2);

    // load: a key, a namespace.
    let ObjectDef::Generic(def) = load(&c, ObjectKind::Key, &keys["m7:list"].obj).await else {
        panic!("a key loads as a generic definition");
    };
    assert_eq!(
        text_prop(&def.extras, &props::KEY_TYPE).as_deref(),
        Some("list")
    );
    assert_eq!(def.extras.get(&extras::LENGTH), Some(&Value::Int(3)));
    assert!(def.extras.contains(&extras::ENCODING));
    assert!(def.extras.contains(&extras::MEMORY_BYTES));
    let ObjectDef::Schema(def) = load(&c, ObjectKind::Schema, &ns.obj).await else {
        panic!("a namespace loads as a schema definition");
    };
    assert_eq!(def.extras.get(&props::KEY_COUNT), Some(&Value::Int(4)));
    let missing = RedisFetcher::new(ObjectKind::Key)
        .load(
            &c,
            &m7.obj
                .child(ObjectKind::Key, format!("m7:missing:{}", token())),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(missing.category(), oxbase_types::ErrorCategory::NotFound);
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn database_one_is_read_on_its_own_session() {
    let c = connect(&params_on("1")).await;
    let f = fixtures();
    let key = s(&f, &["m7", "redis", "db1_key"]);
    if raw(&c, &[b"EXISTS", key.as_bytes()]).await != redis::Value::Int(1) {
        assert!(!support::required(), "{key} is missing in database 1");
        eprintln!("pending: the M7 Redis seed is not loaded");
        return;
    }
    let db = ObjectRef::database("1");
    let m7 = by_name(list(&c, ObjectKind::Schema, &db).await);
    let ns = &m7["m7:"];
    assert_eq!(ns.obj.catalog.as_deref(), Some("1"));
    let under = by_name(list(&c, ObjectKind::Schema, &ns.obj).await);
    let db1 = &under["m7:db1:"];
    let keys = by_name(list(&c, ObjectKind::Key, &db1.obj).await);
    assert_eq!(
        text_prop(&keys[&key].extras, &props::KEY_TYPE).as_deref(),
        Some("string")
    );
    // Database 1 holds none of database 0's seed keys.
    assert!(!under.contains_key("m7:ns:"));
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn binary_names_flat_listings_and_truncation() {
    let c = connect(&params()).await;
    if !seeded(&c).await {
        return;
    }
    let tok = token();
    let prefix = format!("m7:edit:t{tok}:");
    cleanup(&c, &format!("{prefix}*")).await;
    let mut binary = prefix.as_bytes().to_vec();
    binary.extend_from_slice(&[0xff, 0x00, b'k']);
    raw(&c, &[b"SET", &binary, b"v"]).await;
    // A key whose next separator follows non-UTF-8 bytes stays a key at this level.
    let mut deep = prefix.as_bytes().to_vec();
    deep.extend_from_slice(&[0xfe, b':', b'x']);
    raw(&c, &[b"SET", &deep, b"v"]).await;
    for i in 0..12 {
        raw(&c, &[b"SET", format!("{prefix}n{i}").as_bytes(), b"v"]).await;
    }
    let parent = ObjectRef::schema(Some("0"), prefix.clone());
    let keys = list(&c, ObjectKind::Key, &parent).await;
    assert_eq!(keys.len(), 14);
    assert_eq!(
        list(&c, ObjectKind::Schema, &parent).await,
        Vec::<ObjectSummary>::new()
    );
    let bin = keys
        .iter()
        .find(|r| r.extras.contains(&props::KEY_HEX))
        .expect("a row with KEY_HEX");
    assert!(bin.obj.name.contains("\\x"), "{}", bin.obj.name);
    let hex = text_prop(&bin.extras, &props::KEY_HEX).unwrap();
    assert!(
        hex == oxbase_types::keyvalue::to_hex(&binary)
            || hex == oxbase_types::keyvalue::to_hex(&deep)
    );
    // The escaped name loads the key behind it.
    let ObjectDef::Generic(def) = load(&c, ObjectKind::Key, &bin.obj).await else {
        panic!("generic");
    };
    assert_eq!(
        def.extras.get(&props::KEY_HEX),
        bin.extras.get(&props::KEY_HEX)
    );

    // A tiny scan limit marks the listing truncated.
    let small = connect(&with_option(params(), options::TREE_SCAN_LIMIT, "5")).await;
    let cut = list(&small, ObjectKind::Key, &parent).await;
    assert!(cut.len() <= 5);
    assert!(
        cut.iter()
            .all(|r| r.extras.get(&props::TRUNCATED) == Some(&Value::Bool(true)))
    );
    // No separator: every key directly under its parent.
    let flat = connect(&with_option(params(), options::NAMESPACE_SEPARATOR, "")).await;
    assert_eq!(
        list(&flat, ObjectKind::Schema, &ObjectRef::database("0")).await,
        Vec::<ObjectSummary>::new()
    );
    let flat_keys = by_name(list(&flat, ObjectKind::Key, &parent).await);
    assert_eq!(flat_keys.len(), 14);
    small.close().await.unwrap();
    flat.close().await.unwrap();
    cleanup(&c, &format!("{prefix}*")).await;
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn acl_users_list_and_load_without_passwords() {
    let c = connect(&params()).await;
    if !seeded(&c).await {
        return;
    }
    let f = fixtures();
    let reader = s(&f, &["m7", "redis", "acl_user"]);
    let password = s(&f, &["m7", "redis", "acl_password"]);
    let users = by_name(list(&c, ObjectKind::User, &ObjectRef::root()).await);
    assert!(users.contains_key("default"));
    let row = users
        .get(&reader)
        .unwrap_or_else(|| panic!("no ACL user {reader} in {:?}", users.keys()));
    let ObjectDef::Generic(def) = load(&c, ObjectKind::User, &row.obj).await else {
        panic!("a user loads as a generic definition");
    };
    assert_eq!(
        text_prop(&def.extras, &extras::ACL_KEYS).as_deref(),
        Some("~m7:*")
    );
    assert!(
        text_prop(&def.extras, &extras::ACL_FLAGS)
            .unwrap()
            .contains("on")
    );
    assert!(
        text_prop(&def.extras, &extras::ACL_COMMANDS)
            .unwrap()
            .contains("+@read")
    );
    let text = def.definition.unwrap();
    assert!(text.starts_with(&format!("user {reader} on")), "{text}");
    assert!(!text.contains(&password) && !text.contains('#') && !text.contains('>'));
    c.close().await.unwrap();
}
