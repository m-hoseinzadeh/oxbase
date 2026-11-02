//! Live tests of the Redis cells on a cluster and through a sentinel (docs/m7-contracts.md
//! §11, redis): the key-browser scan fans out over every master and returns the 100 seed keys,
//! the tree shows one database and the `m7:cluster:` namespace, values and edits are routed by
//! slot, pub/sub works on a cluster node; the sentinel's master is browsed and read.
//!
//! `#[ignore]`d. The cluster (`[m7.redis_cluster]`) and sentinel (`[m7.redis_sentinel]`)
//! fixtures are started by the conformance stream's compose services; when they are not up the
//! tests print "pending" and pass, unless `OXBASE_REDIS_REQUIRE_M7=1`.

mod support;

use std::collections::HashMap;
use std::time::Duration;

use oxbase_dialect::{Fetcher, KeyValueStore, PubSub, PubSubEvent, SubscribeRequest};
use oxbase_driver::{CancellationToken, ConnectParams, Connection};
use oxbase_redis::dialect::{RedisFetcher, RedisKeyValueStore, RedisPubSub};
use oxbase_redis::{RedisConnection, extras, options};
use oxbase_types::{
    BinaryText, Engine, KeyAddress, KeyApplyRequest, KeyEdit, KeyScanRequest, KeyType, ObjectKind,
    ObjectRef, TlsMode, TlsOptions, Value, props,
};
use support::{
    bytes, cleanup, connect, fixtures, int, lookup, port, read, row_count, s, scanned, token,
    up_or_pending, with_option,
};
use tokio::sync::mpsc;

fn cluster_params() -> Option<ConnectParams> {
    let f = fixtures();
    let ports: Vec<u16> = lookup(&f, &["m7", "redis_cluster", "ports"])
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|n| n.as_integer().and_then(|n| u16::try_from(n).ok()))
        .collect();
    let first = *ports.first().expect("[m7.redis_cluster] ports");
    if !up_or_pending("the Redis cluster", first) {
        return None;
    }
    let host = s(&f, &["m7", "redis_cluster", "host"]);
    let nodes = ports[1..]
        .iter()
        .map(|p| format!("{host}:{p}"))
        .collect::<Vec<_>>()
        .join(",");
    let p = ConnectParams::network(Engine::Redis, host)
        .with_port(first)
        .with_user(s(&f, &["m7", "redis_cluster", "user"]))
        .with_password(s(&f, &["m7", "redis_cluster", "password"]))
        .with_tls(TlsOptions::new(TlsMode::Disable));
    let p = with_option(p, options::MODE, "cluster");
    Some(with_option(p, options::NODES, &nodes))
}

/// The keys under `prefix` (every master), once the cluster seed is loaded (the fixture's
/// one-shot seeding may still be running); `None` when it is not (pending).
async fn cluster_seed(c: &RedisConnection, prefix: &str, wanted: usize) -> Option<Vec<String>> {
    let mut request = KeyScanRequest::all("0");
    request.pattern = Some(format!("{prefix}*"));
    let source = RedisKeyValueStore.scan(&request).unwrap();
    for _ in 0..50 {
        let found: Vec<String> = scanned(&read(source.as_ref(), c).await)
            .into_keys()
            .collect();
        if found.len() >= wanted {
            return Some(found);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        !support::required(),
        "the cluster seed ({prefix}*) is missing"
    );
    eprintln!("pending: the cluster seed ({prefix}*) is not loaded");
    None
}

#[tokio::test]
#[ignore = "needs the redis cluster fixture (M7)"]
async fn a_cluster_scan_fans_out_over_every_master() {
    let Some(p) = cluster_params() else {
        return;
    };
    let f = fixtures();
    let prefix = s(&f, &["m7", "redis_cluster", "key_prefix"]);
    let seed_keys = usize::try_from(int(&f, &["m7", "redis_cluster", "seed_keys"])).unwrap();
    let c = connect(&p).await;
    let Some(found) = cluster_seed(&c, &prefix, seed_keys).await else {
        return;
    };
    assert!(found.iter().all(|k| k.starts_with(&prefix)));
    // The keys live on more than one master.
    let masters = c.masters(&CancellationToken::new()).await.unwrap();
    assert!(masters.len() >= 2, "{masters:?}");
    let mut per_master = Vec::new();
    for node in &masters {
        let reply = c
            .command_on(node, &redis::cmd("DBSIZE"), &CancellationToken::new())
            .await
            .unwrap();
        per_master.push(reply);
    }
    assert!(
        per_master
            .iter()
            .filter(|r| **r != redis::Value::Int(0))
            .count()
            >= 2,
        "{per_master:?}"
    );

    // The tree: one database, the namespace with every seed key, keys with types.
    let dbs = RedisFetcher::new(ObjectKind::Database)
        .list(&c, &ObjectRef::root(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(dbs.len(), 1);
    assert_eq!(dbs[0].obj.name, "0");
    assert_eq!(
        dbs[0].extras.get(&extras::MODE),
        Some(&Value::Text("cluster".into()))
    );
    let namespaces = RedisFetcher::new(ObjectKind::Schema)
        .list(
            &c,
            &ObjectRef::schema(Some("0"), "m7:"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let ns = namespaces
        .iter()
        .find(|r| r.obj.name == prefix)
        .unwrap_or_else(|| panic!("no {prefix} namespace"));
    assert!(
        matches!(ns.extras.get(&props::KEY_COUNT), Some(Value::Int(n)) if usize::try_from(*n).unwrap() >= seed_keys)
    );
    let keys = RedisFetcher::new(ObjectKind::Key)
        .list(&c, &ns.obj, CancellationToken::new())
        .await
        .unwrap();
    assert!(keys.len() >= seed_keys);
    let types: HashMap<String, Option<&Value>> = keys
        .iter()
        .map(|r| (r.obj.name.clone(), r.extras.get(&props::KEY_TYPE)))
        .collect();
    assert!(types.values().all(Option::is_some), "{types:?}");
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis cluster fixture (M7)"]
async fn cluster_values_and_edits_are_routed_by_slot() {
    let Some(p) = cluster_params() else {
        return;
    };
    let c = connect(&p).await;
    let tok = token();
    let store = RedisKeyValueStore;
    let pattern = format!("m7:edit:t{tok}:*");
    // Keys in many slots: every one is created, read and changed on its own master.
    for i in 0..12 {
        let key = format!("m7:edit:t{tok}:{i}");
        let create = KeyApplyRequest {
            address: KeyAddress::new("0", BinaryText::text(key.as_str())),
            key_type: None,
            create: true,
            edits: vec![
                KeyEdit::HashSet {
                    field: BinaryText::text("f"),
                    value: BinaryText::text(i.to_string()),
                    only_new: false,
                },
                KeyEdit::Expire {
                    ttl_ms: Some(600_000),
                },
            ],
            confirm_destructive: false,
        };
        let report = store
            .apply(&c, &create, CancellationToken::new())
            .await
            .unwrap();
        assert!(report.committed, "{key}: {report:?}");
        let info = report.info.unwrap();
        assert_eq!(info.key_type, KeyType::Hash);
        assert_eq!(info.length, Some(1));
        let out = read(
            store
                .read_value(
                    &KeyAddress::new("0", BinaryText::text(key.as_str())),
                    KeyType::Hash,
                )
                .unwrap()
                .as_ref(),
            &c,
        )
        .await;
        assert_eq!(bytes(&out, "value"), [i.to_string().into_bytes()]);
        // A stale type is a conflict on every node too.
        let stale = KeyApplyRequest {
            key_type: Some(KeyType::Set),
            create: false,
            edits: vec![KeyEdit::SetAdd {
                members: vec![BinaryText::text("x")],
            }],
            ..create
        };
        let report = store
            .apply(&c, &stale, CancellationToken::new())
            .await
            .unwrap();
        assert!(!report.committed && report.conflict, "{report:?}");
    }
    let mut request = KeyScanRequest::all("0");
    request.pattern = Some(pattern.clone());
    request.key_type = Some(KeyType::Hash);
    assert_eq!(
        row_count(&read(store.scan(&request).unwrap().as_ref(), &c).await),
        12
    );

    // Pub/sub on a cluster node; PUBLISH reaches it from any node.
    let channel = format!("m7:cluster:news:{tok}");
    let (sink, mut events) = mpsc::channel(16);
    let subscription = RedisPubSub
        .subscribe(
            &c,
            &SubscribeRequest {
                channels: vec![BinaryText::text(channel.as_str())],
                patterns: Vec::new(),
            },
            sink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(first, PubSubEvent::Subscribed { .. }));
    RedisPubSub
        .publish(
            &c,
            &BinaryText::text(channel.as_str()),
            &BinaryText::text("hello"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let message = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&message, PubSubEvent::Message { payload, .. } if *payload == BinaryText::text("hello")),
        "{message:?}"
    );
    subscription.close().await.unwrap();

    for i in 0..12 {
        support::raw(&c, &[b"UNLINK", format!("m7:edit:t{tok}:{i}").as_bytes()]).await;
    }
    c.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the redis sentinel fixture (M7)"]
async fn the_sentinel_master_is_browsed_and_read() {
    let f = fixtures();
    let sentinel_port = port(&f, &["m7", "redis_sentinel", "sentinel_port"]);
    if !up_or_pending("the Redis sentinel", sentinel_port) {
        return;
    }
    let p = ConnectParams::network(Engine::Redis, s(&f, &["m7", "redis_sentinel", "host"]))
        .with_port(sentinel_port)
        .with_user(s(&f, &["m7", "redis_sentinel", "user"]))
        .with_password(s(&f, &["m7", "redis_sentinel", "password"]))
        .with_tls(TlsOptions::new(TlsMode::Disable));
    let p = with_option(p, options::MODE, "sentinel");
    let p = with_option(
        p,
        options::MASTER_NAME,
        &s(&f, &["m7", "redis_sentinel", "master_name"]),
    );
    let c = connect(&p).await;
    let seed = s(&f, &["m7", "redis_sentinel", "seed_key"]);
    if support::raw(&c, &[b"EXISTS", seed.as_bytes()]).await != redis::Value::Int(1) {
        assert!(
            !support::required(),
            "the sentinel seed ({seed}) is missing"
        );
        eprintln!("pending: the sentinel seed ({seed}) is not loaded");
        return;
    }
    let store = RedisKeyValueStore;
    let address = KeyAddress::new("0", BinaryText::text(seed.as_str()));
    let info = store
        .info(&c, &address, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(info.key_type, KeyType::String);
    let out = read(
        store
            .read_value(&address, KeyType::String)
            .unwrap()
            .as_ref(),
        &c,
    )
    .await;
    assert_eq!(bytes(&out, "value"), [b"master".to_vec()]);
    let dbs = RedisFetcher::new(ObjectKind::Database)
        .list(&c, &ObjectRef::root(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(dbs.first().map(|d| d.obj.name.as_str()), Some("0"));
    assert_eq!(
        dbs[0].extras.get(&extras::MODE),
        Some(&Value::Text("sentinel".into()))
    );
    // An edit on the master through the sentinel profile.
    let tok = token();
    let key = format!("m7:edit:t{tok}:sentinel");
    let report = store
        .apply(
            &c,
            &KeyApplyRequest {
                address: KeyAddress::new("0", BinaryText::text(key.as_str())),
                key_type: None,
                create: true,
                edits: vec![KeyEdit::SetString {
                    value: BinaryText::text("v"),
                    keep_ttl: false,
                }],
                confirm_destructive: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(report.committed, "{report:?}");
    cleanup(&c, &format!("m7:edit:t{tok}:*")).await;
    c.close().await.unwrap();
}
