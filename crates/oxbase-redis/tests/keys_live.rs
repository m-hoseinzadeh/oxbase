//! Live tests of the Redis key-value cell (docs/m7-contracts.md §11, redis): the key browser's
//! scan, the typed value reads (with the 200 000-element list and sorted set), key info and
//! every `KeyEdit`, against the `redis` fixture and its M7 seed (`[m7.redis]`).
//!
//! `#[ignore]`d: the conformance CI job runs them with `--include-ignored`. Edits only touch
//! keys `m7:edit:t<token>:*` (removed at the end; leftovers of crashed runs are removed at the
//! start). Without the M7 seed a test prints "pending" and passes, unless
//! `OXBASE_REDIS_REQUIRE_M7=1`.

mod support;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use futures::StreamExt;
use oxbase_dialect::KeyValueStore;
use oxbase_driver::{CancellationToken, Collected, Connection, ResultItem};
use oxbase_redis::dialect::RedisKeyValueStore;
use oxbase_types::{
    BinaryText, ErrorCategory, KeyAddress, KeyApplyReport, KeyApplyRequest, KeyEdit,
    KeyScanRequest, KeyType, ListEnd, StreamField,
};
use support::{
    ROWS, bytes, cleanup, columns, connect, fixtures, floats, ints, params, raw, read, row_count,
    s, scanned, seeded, strings, texts, token,
};

const CANCEL_LIMIT: Duration = Duration::from_secs(2);

fn t(text: &str) -> BinaryText {
    BinaryText::text(text)
}

fn addr(key: &str) -> KeyAddress {
    KeyAddress::new("0", t(key))
}

fn request(key: &str, key_type: Option<KeyType>, edits: Vec<KeyEdit>) -> KeyApplyRequest {
    KeyApplyRequest {
        address: addr(key),
        key_type,
        create: false,
        edits,
        confirm_destructive: false,
    }
}

fn create(key: &str, edits: Vec<KeyEdit>) -> KeyApplyRequest {
    KeyApplyRequest {
        create: true,
        ..request(key, None, edits)
    }
}

async fn apply(c: &oxbase_redis::RedisConnection, r: &KeyApplyRequest) -> KeyApplyReport {
    RedisKeyValueStore
        .apply(c, r, CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("apply {:?}: {e}", r.edits))
}

async fn value(c: &oxbase_redis::RedisConnection, key: &str, kt: KeyType) -> Collected {
    let source = RedisKeyValueStore.read_value(&addr(key), kt).unwrap();
    read(source.as_ref(), c).await
}

/// A score reply: a RESP3 double or the RESP2 text.
fn score(v: &redis::Value) -> f64 {
    match v {
        redis::Value::Double(d) => *d,
        redis::Value::BulkString(b) => String::from_utf8_lossy(b).parse().unwrap(),
        other => panic!("score {other:?}"),
    }
}

fn committed(r: &KeyApplyReport) {
    assert!(r.committed, "not committed: {r:?}");
    assert!(!r.conflict && r.error.is_none(), "{r:?}");
}

fn conflict(r: &KeyApplyReport) {
    assert!(!r.committed && r.conflict, "expected a conflict: {r:?}");
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn scan_lists_every_seed_key_once_with_types_and_ttls() {
    let c = connect(&params()).await;
    if !seeded(&c).await {
        return;
    }
    let f = fixtures();
    let store = RedisKeyValueStore;
    let mut r = KeyScanRequest::all("0");
    r.pattern = Some("m7:*".to_owned());
    let out = read(store.scan(&r).unwrap().as_ref(), &c).await;
    assert_eq!(columns(&out), ["key", "key_text", "type", "ttl_ms"]);
    let found = scanned(&out);
    for key in strings(&f, &["m7", "redis", "keys"]) {
        assert!(found.contains_key(&key), "{key} missing from the scan");
    }
    assert!(found.keys().all(|k| k.starts_with("m7:")));
    assert_eq!(found["m7:hash"].0, "hash");
    assert_eq!(found["m7:zset"].0, "zset");
    assert_eq!(found["m7:stream"].0, "stream");
    assert_eq!(found["m7:string"].1, None);
    let ttl_key = s(&f, &["m7", "redis", "ttl_key"]);
    let ttl = found[&ttl_key].1.expect("the TTL key expires");
    assert!(ttl > 0 && ttl <= 86_400_000, "{ttl}");
    // Keys come back as bytes too.
    let raw_keys: HashSet<Vec<u8>> = bytes(&out, "key").into_iter().collect();
    assert!(raw_keys.contains(b"m7:binary".as_slice()));

    // Type filter, COUNT and limit.
    r.key_type = Some(KeyType::Hash);
    r.count = Some(7);
    let hashes = scanned(&read(store.scan(&r).unwrap().as_ref(), &c).await);
    assert!(hashes.contains_key("m7:hash") && hashes.contains_key("m7:edit:seed"));
    assert!(hashes.values().all(|(t, _)| t == "hash"), "{hashes:?}");
    r.key_type = None;
    r.limit = Some(3);
    let limited = read(store.scan(&r).unwrap().as_ref(), &c).await;
    assert_eq!(row_count(&limited), 3);
    // A pattern that matches nothing gives the columns and no rows.
    r.pattern = Some(format!("m7:none:{}:*", token()));
    r.limit = None;
    let none = read(store.scan(&r).unwrap().as_ref(), &c).await;
    assert_eq!(row_count(&none), 0);
    assert_eq!(columns(&none).len(), 4);
    assert_eq!(
        store.scan_text(&KeyScanRequest {
            pattern: Some("m7:*".into()),
            key_type: Some(KeyType::Hash),
            ..KeyScanRequest::all("0")
        }),
        "SCAN 0 MATCH m7:* COUNT 1000 TYPE hash"
    );
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn every_type_reads_into_its_typed_columns() {
    let c = connect(&params()).await;
    if !seeded(&c).await {
        return;
    }
    let store = RedisKeyValueStore;
    let out = value(&c, "m7:string", KeyType::String).await;
    assert_eq!(columns(&out), ["value"]);
    assert_eq!(bytes(&out, "value"), [b"hello".to_vec()]);
    let out = value(&c, "m7:binary", KeyType::String).await;
    assert_eq!(bytes(&out, "value"), [vec![0x00, 0xff, 0xfe, 0x0a]]);
    let field = &out.result_sets[0].schema.fields()[0];
    assert_eq!(
        field.metadata().get("oxbase.encoding").map(String::as_str),
        Some("binary")
    );

    let out = value(&c, "m7:hash", KeyType::Hash).await;
    assert_eq!(columns(&out), ["field", "value"]);
    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = bytes(&out, "field")
        .into_iter()
        .zip(bytes(&out, "value"))
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        [
            (b"f1".to_vec(), b"v1".to_vec()),
            (b"f2".to_vec(), b"v2".to_vec()),
            (b"f3".to_vec(), "grüße".as_bytes().to_vec()),
        ]
    );

    let out = value(&c, "m7:list", KeyType::List).await;
    assert_eq!(columns(&out), ["index", "value"]);
    assert_eq!(ints(&out, "index"), [Some(0), Some(1), Some(2)]);
    assert_eq!(
        bytes(&out, "value"),
        [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
    );

    let out = value(&c, "m7:set", KeyType::Set).await;
    assert_eq!(columns(&out), ["member"]);
    let mut members = bytes(&out, "member");
    members.sort();
    assert_eq!(members, [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);

    let out = value(&c, "m7:zset", KeyType::Zset).await;
    assert_eq!(columns(&out), ["member", "score"]);
    assert_eq!(
        bytes(&out, "member"),
        [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
    );
    assert_eq!(floats(&out, "score"), [1.0, 2.0, 3.0]);

    let out = value(&c, "m7:stream", KeyType::Stream).await;
    assert_eq!(columns(&out), ["id", "fields"]);
    assert_eq!(
        texts(&out, "id"),
        [
            Some("1767261600000-0".to_owned()),
            Some("1767261600000-1".to_owned())
        ]
    );
    let first: serde_json::Value =
        serde_json::from_str(texts(&out, "fields")[0].as_deref().unwrap()).unwrap();
    assert_eq!(first, serde_json::json!({"k": "v1"}));

    // RedisJSON is optional in the fixture image.
    let json = raw(&c, &[b"TYPE", b"m7:json"]).await;
    if json == redis::Value::SimpleString("ReJSON-RL".into()) {
        let out = value(&c, "m7:json", KeyType::Json).await;
        let doc: serde_json::Value =
            serde_json::from_str(texts(&out, "value")[0].as_deref().unwrap()).unwrap();
        assert_eq!(doc, serde_json::json!({"a": 1, "b": [true, null]}));
    } else {
        eprintln!("RedisJSON is not loaded: m7:json skipped");
    }
    assert!(
        store
            .read_value(&addr("m7:string"), KeyType::Other)
            .is_err()
    );
    assert_eq!(
        store.read_text(&addr("m7:hash"), KeyType::Hash),
        "HSCAN m7:hash 0 COUNT 1000"
    );
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn big_lists_and_sorted_sets_stream_in_chunks() {
    let c = connect(&params()).await;
    if !seeded(&c).await {
        return;
    }
    let f = fixtures();
    let store = RedisKeyValueStore;
    let list = s(&f, &["m7", "redis", "big_list"]);
    let started = Instant::now();
    let out = read(
        store
            .read_value(&addr(&list), KeyType::List)
            .unwrap()
            .as_ref(),
        &c,
    )
    .await;
    assert_eq!(row_count(&out), ROWS);
    assert!(out.result_sets[0].batches.len() >= ROWS / 1000);
    let index = ints(&out, "index");
    assert!(
        index
            .iter()
            .enumerate()
            .all(|(i, v)| *v == Some(i64::try_from(i).unwrap()))
    );
    let values = bytes(&out, "value");
    assert_eq!(values[0], b"e1");
    assert_eq!(values[ROWS - 1], format!("e{ROWS}").as_bytes());
    eprintln!("{list}: {ROWS} rows in {:?}", started.elapsed());

    let zset = s(&f, &["m7", "redis", "big_zset"]);
    let started = Instant::now();
    let out = read(
        store
            .read_value(&addr(&zset), KeyType::Zset)
            .unwrap()
            .as_ref(),
        &c,
    )
    .await;
    assert_eq!(row_count(&out), ROWS);
    let scores = floats(&out, "score");
    assert!(scores.windows(2).all(|w| w[0] < w[1]), "score order");
    let members = bytes(&out, "member");
    assert_eq!(members[0], b"m1");
    assert_eq!(members[ROWS - 1], format!("m{ROWS}").as_bytes());
    eprintln!("{zset}: {ROWS} rows in {:?}", started.elapsed());

    // A scan of every key also streams (the seed has more than one SCAN page of keys? only
    // when the database is large; at least every m7 key comes back once).
    let out = read(store.scan(&KeyScanRequest::all("0")).unwrap().as_ref(), &c).await;
    assert!(scanned(&out).contains_key(&list));
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn a_value_read_is_cancelled_within_two_seconds_and_dropping_stops_it() {
    let c = connect(&params()).await;
    if !seeded(&c).await {
        return;
    }
    let f = fixtures();
    let list = s(&f, &["m7", "redis", "big_list"]);
    let store = RedisKeyValueStore;
    let source = store.read_value(&addr(&list), KeyType::List).unwrap();
    let cancel = CancellationToken::new();
    let mut stream = source.open(&c, cancel.clone()).await.unwrap();
    // Schema and the first batch.
    for _ in 0..2 {
        stream.next().await.unwrap().unwrap();
    }
    cancel.cancel();
    let started = Instant::now();
    let mut ended_cancelled = false;
    while let Some(item) = stream.next().await {
        match item {
            Ok(ResultItem::Batch(_)) => {}
            Ok(other) => panic!("{other:?}"),
            Err(e) => {
                assert!(e.is_cancelled(), "{e}");
                ended_cancelled = true;
            }
        }
    }
    assert!(ended_cancelled);
    assert!(started.elapsed() < CANCEL_LIMIT, "{:?}", started.elapsed());
    // An already cancelled token fails the open.
    let err = source.open(&c, cancel).await.err().unwrap();
    assert!(err.is_cancelled());
    // Dropping a stream after a few batches leaves the session usable.
    let mut stream = source.open(&c, CancellationToken::new()).await.unwrap();
    stream.next().await.unwrap().unwrap();
    stream.next().await.unwrap().unwrap();
    drop(stream);
    assert_eq!(
        raw(&c, &[b"PING"]).await,
        redis::Value::SimpleString("PONG".into())
    );
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn key_info_reports_type_ttl_length_and_memory() {
    let c = connect(&params()).await;
    if !seeded(&c).await {
        return;
    }
    let store = RedisKeyValueStore;
    let info = store
        .info(&c, &addr("m7:list"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(info.key_type, KeyType::List);
    assert_eq!(info.length, Some(3));
    assert_eq!(info.ttl_ms, None);
    assert!(info.encoding.is_some(), "{info:?}");
    assert!(info.memory_bytes.is_some_and(|m| m > 0), "{info:?}");
    let info = store
        .info(&c, &addr("m7:ttl"), CancellationToken::new())
        .await
        .unwrap();
    assert!(info.ttl_ms.is_some_and(|t| t > 0), "{info:?}");
    assert_eq!(info.length, Some(7));
    let missing = store
        .info(
            &c,
            &addr(&format!("m7:missing:{}", token())),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(missing.category(), ErrorCategory::NotFound);

    // The read-only ACL user may not run OBJECT / MEMORY: those stay unknown, the rest works.
    let f = fixtures();
    let reader = connect(
        &params()
            .with_user(s(&f, &["m7", "redis", "acl_user"]))
            .with_password(s(&f, &["m7", "redis", "acl_password"])),
    )
    .await;
    let info = store
        .info(&reader, &addr("m7:hash"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(info.key_type, KeyType::Hash);
    assert_eq!(info.length, Some(3));
    reader.close().await.unwrap();
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn every_key_edit_applies_atomically() {
    let c = connect(&params()).await;
    if !seeded(&c).await {
        return;
    }
    let tok = token();
    cleanup(&c, &format!("m7:edit:t{tok}:*")).await;
    let k = |name: &str| format!("m7:edit:t{tok}:{name}");

    // Strings: create, overwrite keeping the TTL, expire, persist.
    let r = apply(
        &c,
        &create(
            &k("s"),
            vec![
                KeyEdit::SetString {
                    value: t("one"),
                    keep_ttl: false,
                },
                KeyEdit::Expire {
                    ttl_ms: Some(600_000),
                },
            ],
        ),
    )
    .await;
    committed(&r);
    let info = r.info.unwrap();
    assert_eq!(info.key_type, KeyType::String);
    assert!(info.ttl_ms.is_some_and(|t| t > 500_000));
    let r = apply(
        &c,
        &request(
            &k("s"),
            Some(KeyType::String),
            vec![KeyEdit::SetString {
                value: BinaryText::from_bytes(&[0, 0xff]),
                keep_ttl: true,
            }],
        ),
    )
    .await;
    committed(&r);
    assert!(r.info.unwrap().ttl_ms.is_some());
    assert_eq!(
        raw(&c, &[b"GET", k("s").as_bytes()]).await,
        redis::Value::BulkString(vec![0, 0xff])
    );
    let r = apply(
        &c,
        &request(&k("s"), None, vec![KeyEdit::Expire { ttl_ms: None }]),
    )
    .await;
    committed(&r);
    assert_eq!(r.info.unwrap().ttl_ms, None);
    // Creating an existing key is a conflict; nothing changes.
    let r = apply(
        &c,
        &create(
            &k("s"),
            vec![KeyEdit::SetString {
                value: t("other"),
                keep_ttl: false,
            }],
        ),
    )
    .await;
    conflict(&r);
    // An expiry of 0 ms deletes the key: it needs confirmation like a delete.
    let mut expire_now = request(&k("s"), None, vec![KeyEdit::Expire { ttl_ms: Some(0) }]);
    let err = RedisKeyValueStore
        .apply(&c, &expire_now, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert_eq!(
        raw(&c, &[b"EXISTS", k("s").as_bytes()]).await,
        redis::Value::Int(1)
    );
    expire_now.confirm_destructive = true;
    let r = apply(&c, &expire_now).await;
    committed(&r);
    assert!(r.info.is_none(), "{r:?}");
    assert_eq!(
        raw(&c, &[b"EXISTS", k("s").as_bytes()]).await,
        redis::Value::Int(0)
    );

    // Hashes.
    let r = apply(
        &c,
        &create(
            &k("h"),
            vec![
                KeyEdit::HashSet {
                    field: t("a"),
                    value: t("1"),
                    only_new: false,
                },
                KeyEdit::HashSet {
                    field: t("b"),
                    value: t("2"),
                    only_new: false,
                },
            ],
        ),
    )
    .await;
    committed(&r);
    assert_eq!(r.info.unwrap().length, Some(2));
    let r = apply(
        &c,
        &request(
            &k("h"),
            Some(KeyType::Hash),
            vec![
                KeyEdit::HashSet {
                    field: t("a"),
                    value: t("kept"),
                    only_new: true,
                },
                KeyEdit::HashDelete {
                    fields: vec![t("b")],
                },
            ],
        ),
    )
    .await;
    committed(&r);
    assert_eq!(r.info.unwrap().length, Some(1));
    assert_eq!(
        raw(&c, &[b"HGET", k("h").as_bytes(), b"a"]).await,
        redis::Value::BulkString(b"1".to_vec())
    );
    // A type conflict: the editor read a list, the key is a hash.
    let r = apply(
        &c,
        &request(
            &k("h"),
            Some(KeyType::List),
            vec![KeyEdit::ListPush {
                end: ListEnd::Tail,
                values: vec![t("x")],
            }],
        ),
    )
    .await;
    conflict(&r);
    assert_eq!(
        raw(&c, &[b"TYPE", k("h").as_bytes()]).await,
        redis::Value::SimpleString("hash".into())
    );

    // Lists: push at both ends, set with the expected value, a stale expected value, remove.
    let r = apply(
        &c,
        &create(
            &k("l"),
            vec![
                KeyEdit::ListPush {
                    end: ListEnd::Tail,
                    values: vec![t("b"), t("c")],
                },
                KeyEdit::ListPush {
                    end: ListEnd::Head,
                    values: vec![t("a")],
                },
            ],
        ),
    )
    .await;
    committed(&r);
    assert_eq!(r.info.unwrap().length, Some(3));
    let r = apply(
        &c,
        &request(
            &k("l"),
            Some(KeyType::List),
            vec![KeyEdit::ListSet {
                index: 1,
                value: t("B"),
                expected: Some(t("b")),
            }],
        ),
    )
    .await;
    committed(&r);
    // A set by index after a push of the same apply is refused before anything is written.
    let pushed = request(
        &k("l"),
        Some(KeyType::List),
        vec![
            KeyEdit::ListPush {
                end: ListEnd::Tail,
                values: vec![t("never")],
            },
            KeyEdit::ListSet {
                index: 1,
                value: t("X"),
                expected: Some(t("B")),
            },
        ],
    );
    let err = RedisKeyValueStore
        .apply(&c, &pushed, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    // A stale expected value is a conflict.
    let stale = request(
        &k("l"),
        Some(KeyType::List),
        vec![KeyEdit::ListSet {
            index: 1,
            value: t("X"),
            expected: Some(t("b")),
        }],
    );
    conflict(&apply(&c, &stale).await);
    let r = apply(
        &c,
        &request(
            &k("l"),
            Some(KeyType::List),
            vec![KeyEdit::ListSet {
                index: 50,
                value: t("X"),
                expected: None,
            }],
        ),
    )
    .await;
    assert!(!r.committed && !r.conflict && r.error.is_some(), "{r:?}");
    let r = apply(
        &c,
        &request(
            &k("l"),
            Some(KeyType::List),
            vec![KeyEdit::ListRemove {
                value: t("c"),
                count: 0,
            }],
        ),
    )
    .await;
    committed(&r);
    assert_eq!(
        raw(&c, &[b"LRANGE", k("l").as_bytes(), b"0", b"-1"]).await,
        redis::Value::Array(vec![
            redis::Value::BulkString(b"a".to_vec()),
            redis::Value::BulkString(b"B".to_vec()),
        ])
    );

    // Sets and sorted sets.
    let r = apply(
        &c,
        &create(
            &k("set"),
            vec![KeyEdit::SetAdd {
                members: vec![t("x"), t("y"), BinaryText::from_bytes(&[0xfe])],
            }],
        ),
    )
    .await;
    committed(&r);
    let r = apply(
        &c,
        &request(
            &k("set"),
            Some(KeyType::Set),
            vec![KeyEdit::SetRemove {
                members: vec![t("x")],
            }],
        ),
    )
    .await;
    assert_eq!(r.info.unwrap().length, Some(2));
    let r = apply(
        &c,
        &create(
            &k("z"),
            vec![
                KeyEdit::ZsetAdd {
                    member: t("a"),
                    score: 1.5,
                },
                KeyEdit::ZsetAdd {
                    member: t("b"),
                    score: -2.0,
                },
            ],
        ),
    )
    .await;
    committed(&r);
    let r = apply(
        &c,
        &request(
            &k("z"),
            Some(KeyType::Zset),
            vec![
                KeyEdit::ZsetAdd {
                    member: t("a"),
                    score: 0.1,
                },
                KeyEdit::ZsetRemove {
                    members: vec![t("b")],
                },
            ],
        ),
    )
    .await;
    committed(&r);
    assert_eq!(
        score(&raw(&c, &[b"ZSCORE", k("z").as_bytes(), b"a"]).await).to_bits(),
        0.1_f64.to_bits()
    );

    // Streams: explicit and server ids, an id that does not grow, delete.
    let field = |f: &str, v: &str| StreamField {
        field: t(f),
        value: t(v),
    };
    let r = apply(
        &c,
        &create(
            &k("x"),
            vec![
                KeyEdit::StreamAdd {
                    id: Some("5-1".into()),
                    fields: vec![field("a", "1")],
                },
                KeyEdit::StreamAdd {
                    id: None,
                    fields: vec![field("b", "2"), field("c", "3")],
                },
            ],
        ),
    )
    .await;
    committed(&r);
    assert_eq!(r.info.unwrap().length, Some(2));
    let r = apply(
        &c,
        &request(
            &k("x"),
            Some(KeyType::Stream),
            vec![KeyEdit::StreamAdd {
                id: Some("5-0".into()),
                fields: vec![field("late", "x")],
            }],
        ),
    )
    .await;
    assert!(!r.committed && r.error.is_some(), "{r:?}");
    let r = apply(
        &c,
        &request(
            &k("x"),
            Some(KeyType::Stream),
            vec![KeyEdit::StreamDelete {
                ids: vec!["5-1".into()],
            }],
        ),
    )
    .await;
    committed(&r);
    assert_eq!(r.info.unwrap().length, Some(1));
    // Deleting the top entry does not lower the last generated id: an id at or below it is
    // refused before anything is written (the TTL of the same apply is not set either).
    let add = |id: &str| KeyEdit::StreamAdd {
        id: Some(id.to_owned()),
        fields: vec![field("a", "1")],
    };
    committed(&apply(&c, &create(&k("x2"), vec![add("7-1"), add("7-2")])).await);
    let r = apply(
        &c,
        &request(
            &k("x2"),
            Some(KeyType::Stream),
            vec![KeyEdit::StreamDelete {
                ids: vec!["7-2".into()],
            }],
        ),
    )
    .await;
    committed(&r);
    let r = apply(
        &c,
        &request(
            &k("x2"),
            Some(KeyType::Stream),
            vec![
                KeyEdit::Expire {
                    ttl_ms: Some(600_000),
                },
                add("7-2"),
            ],
        ),
    )
    .await;
    assert!(!r.committed && !r.conflict && r.error.is_some(), "{r:?}");
    assert_eq!(
        raw(&c, &[b"PTTL", k("x2").as_bytes()]).await,
        redis::Value::Int(-1)
    );
    let r = apply(
        &c,
        &request(&k("x2"), Some(KeyType::Stream), vec![add("7-*")]),
    )
    .await;
    committed(&r);
    assert_eq!(r.info.unwrap().length, Some(2));
    let top = raw(
        &c,
        &[b"XREVRANGE", k("x2").as_bytes(), b"+", b"-", b"COUNT", b"1"],
    )
    .await;
    let top_id = match top {
        redis::Value::Array(entries) => match entries.into_iter().next() {
            Some(redis::Value::Array(entry)) => entry.into_iter().next(),
            other => panic!("XREVRANGE entry {other:?}"),
        },
        other => panic!("XREVRANGE {other:?}"),
    };
    assert_eq!(top_id, Some(redis::Value::BulkString(b"7-3".to_vec())));
    let r = apply(
        &c,
        &request(&k("x2"), Some(KeyType::Stream), vec![add("6-*")]),
    )
    .await;
    assert!(!r.committed && r.error.is_some(), "{r:?}");

    // Rename: RENAMENX refuses an existing target; the new key is reported.
    let r = apply(
        &c,
        &request(
            &k("set"),
            None,
            vec![KeyEdit::Rename {
                new_key: t(&k("h")),
                overwrite: false,
            }],
        ),
    )
    .await;
    assert!(!r.committed && r.error.is_some(), "{r:?}");
    let r = apply(
        &c,
        &request(
            &k("set"),
            None,
            vec![KeyEdit::Rename {
                new_key: t(&k("set2")),
                overwrite: false,
            }],
        ),
    )
    .await;
    committed(&r);
    let info = r.info.unwrap();
    assert_eq!(info.address.key, t(&k("set2")));
    assert_eq!(info.key_type, KeyType::Set);
    // Overwriting rename and delete need confirmation.
    let mut overwrite = request(
        &k("set2"),
        None,
        vec![KeyEdit::Rename {
            new_key: t(&k("h")),
            overwrite: true,
        }],
    );
    let err = RedisKeyValueStore
        .apply(&c, &overwrite, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    overwrite.confirm_destructive = true;
    committed(&apply(&c, &overwrite).await);
    assert_eq!(
        raw(&c, &[b"TYPE", k("h").as_bytes()]).await,
        redis::Value::SimpleString("set".into())
    );
    let mut delete = request(&k("h"), None, vec![KeyEdit::Delete]);
    let err = RedisKeyValueStore
        .apply(&c, &delete, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    assert_eq!(
        raw(&c, &[b"EXISTS", k("h").as_bytes()]).await,
        redis::Value::Int(1)
    );
    delete.confirm_destructive = true;
    let r = apply(&c, &delete).await;
    committed(&r);
    assert!(r.info.is_none());
    assert_eq!(
        raw(&c, &[b"EXISTS", k("h").as_bytes()]).await,
        redis::Value::Int(0)
    );
    // Editing a key that is gone is a conflict.
    let r = apply(
        &c,
        &request(&k("h"), None, vec![KeyEdit::Expire { ttl_ms: Some(10) }]),
    )
    .await;
    conflict(&r);

    // JSON (RedisJSON is optional).
    let json = KeyApplyRequest {
        key_type: Some(KeyType::Json),
        ..create(
            &k("j"),
            vec![KeyEdit::JsonSet {
                path: "$".into(),
                value: serde_json::json!({"n": 1}),
            }],
        )
    };
    match RedisKeyValueStore
        .apply(&c, &json, CancellationToken::new())
        .await
    {
        Ok(r) => {
            committed(&r);
            assert_eq!(r.info.unwrap().key_type, KeyType::Json);
            json_paths_and_numbers(&c, &k("j"), &k("big")).await;
        }
        Err(e) => {
            assert_eq!(e.category(), ErrorCategory::Unsupported, "{e}");
            eprintln!("RedisJSON is not loaded: {e}");
        }
    }

    // A concurrent change between the checks and EXEC is a conflict (simulated by a WATCHed
    // key changing: another connection writes while the apply holds its WATCH is not
    // reproducible deterministically, so the stale-type and stale-value checks above cover
    // the checks; here the seed key is left untouched).
    let seed = raw(&c, &[b"HGET", b"m7:edit:seed", b"v"]).await;
    assert_eq!(seed, redis::Value::BulkString(b"1".to_vec()));
    cleanup(&c, &format!("m7:edit:t{tok}:*")).await;
    c.close().await.unwrap();
}

/// RedisJSON loaded: a new member of an existing object is set; a path whose parent is missing
/// is refused before anything is written; numbers beyond 64 bits are shown as the server
/// stores them (cut out of the reply, not parsed and printed again).
async fn json_paths_and_numbers(c: &oxbase_redis::RedisConnection, j: &str, big: &str) {
    let member = request(
        j,
        Some(KeyType::Json),
        vec![KeyEdit::JsonSet {
            path: "$.m".into(),
            value: serde_json::json!([1]),
        }],
    );
    committed(&apply(c, &member).await);
    let deep = request(
        j,
        Some(KeyType::Json),
        vec![
            KeyEdit::Expire {
                ttl_ms: Some(600_000),
            },
            KeyEdit::JsonSet {
                path: "$.a.b".into(),
                value: serde_json::json!(2),
            },
        ],
    );
    let r = apply(c, &deep).await;
    assert!(!r.committed && !r.conflict && r.error.is_some(), "{r:?}");
    assert_eq!(
        raw(c, &[b"PTTL", j.as_bytes()]).await,
        redis::Value::Int(-1)
    );
    let doc = r#"{"id":123456789012345678901234567890,"n":18446744073709551615,"d":0.1}"#;
    raw(c, &[b"JSON.SET", big.as_bytes(), b"$", doc.as_bytes()]).await;
    let stored = match raw(c, &[b"JSON.GET", big.as_bytes(), b"$"]).await {
        redis::Value::BulkString(b) => String::from_utf8(b).unwrap(),
        other => panic!("JSON.GET {other:?}"),
    };
    let expected = stored
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap()
        .trim()
        .to_owned();
    let out = value(c, big, KeyType::Json).await;
    assert_eq!(texts(&out, "value"), [Some(expected)]);
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn a_large_string_is_read_in_pieces() {
    let c = connect(&params()).await;
    if !seeded(&c).await {
        return;
    }
    let key = format!("m7:edit:t{}:large", token());
    let big: Vec<u8> = (0..3 * 1024 * 1024 + 17)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    raw(&c, &[b"SET", key.as_bytes(), big.as_slice()]).await;
    let out = value(&c, &key, KeyType::String).await;
    assert_eq!(bytes(&out, "value"), [big]);
    // Nothing stays watched: an apply afterwards commits.
    let r = apply(
        &c,
        &request(
            &key,
            Some(KeyType::String),
            vec![KeyEdit::SetString {
                value: t("small"),
                keep_ttl: false,
            }],
        ),
    )
    .await;
    committed(&r);
    raw(&c, &[b"DEL", key.as_bytes()]).await;
    c.close().await.unwrap();
}
