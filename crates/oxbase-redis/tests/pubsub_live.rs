//! Live tests of Redis publish / subscribe (docs/m7-contracts.md §11, redis, *pub/sub*):
//! subscribe to `m7:news` and `m7:*`, publish, receive both deliveries, unsubscribe, and the
//! `dropped` count of a full sink, on RESP2 and RESP3 sessions of the `redis` fixture.
//! `#[ignore]`d: the conformance CI job runs them with `--include-ignored`.

mod support;

use std::time::Duration;

use oxbase_dialect::{PubSub, PubSubEvent, SubscribeRequest};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_redis::dialect::RedisPubSub;
use oxbase_redis::options;
use oxbase_types::BinaryText;
use support::{connect, fixtures, params, s, token, with_option};
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(5);

async fn next(events: &mut mpsc::Receiver<PubSubEvent>) -> PubSubEvent {
    tokio::time::timeout(WAIT, events.recv())
        .await
        .expect("an event within 5 s")
        .expect("the event channel is open")
}

async fn publish(c: &oxbase_redis::RedisConnection, channel: &str, payload: &[u8]) -> u64 {
    RedisPubSub
        .publish(
            c,
            &BinaryText::text(channel),
            &BinaryText::from_bytes(payload),
            CancellationToken::new(),
        )
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn channel_and_pattern_subscriptions_receive_publishes_on_both_protocols() {
    let f = fixtures();
    let channel = s(&f, &["m7", "redis", "pubsub_channel"]);
    let pattern = s(&f, &["m7", "redis", "pubsub_pattern"]);
    for protocol in ["resp3", "resp2"] {
        let c = connect(&with_option(params(), options::PROTOCOL, protocol)).await;
        let (sink, mut events) = mpsc::channel(64);
        let request = SubscribeRequest {
            channels: vec![BinaryText::text(&channel)],
            patterns: vec![BinaryText::text(&pattern)],
        };
        let subscription = RedisPubSub
            .subscribe(&c, &request, sink, CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{protocol}: subscribe: {e}"));
        assert_eq!(
            next(&mut events).await,
            PubSubEvent::Subscribed {
                channels: request.channels.clone(),
                patterns: request.patterns.clone(),
            }
        );
        // The session itself stays usable for commands.
        let receivers = publish(&c, &channel, &[b'h', b'i', 0xff]).await;
        assert!(receivers >= 2, "{protocol}: {receivers} receivers");
        let mut seen = Vec::new();
        for _ in 0..2 {
            match next(&mut events).await {
                PubSubEvent::Message {
                    channel: ch,
                    pattern: p,
                    payload,
                    received_at_ms,
                } => {
                    assert_eq!(ch, BinaryText::text(&channel));
                    assert_eq!(payload, BinaryText::from_bytes(&[b'h', b'i', 0xff]));
                    assert!(received_at_ms > 0);
                    seen.push(p);
                }
                other => panic!("{protocol}: {other:?}"),
            }
        }
        seen.sort();
        assert_eq!(seen, [None, Some(BinaryText::text(&pattern))], "{protocol}");
        // Only the pattern matches another m7 channel.
        let other = format!("m7:other:{}", token());
        publish(&c, &other, b"x").await;
        match next(&mut events).await {
            PubSubEvent::Message {
                channel: ch,
                pattern: p,
                ..
            } => {
                assert_eq!(ch, BinaryText::text(&other));
                assert_eq!(p, Some(BinaryText::text(&pattern)));
            }
            other => panic!("{protocol}: {other:?}"),
        }
        subscription.close().await.unwrap();
        assert_eq!(next(&mut events).await, PubSubEvent::Closed { error: None });
        subscription.close().await.unwrap();
        // Unsubscribed: nothing arrives any more, and nobody listens on our own channel.
        let mine = format!("m7:mine:{}", token());
        assert_eq!(publish(&c, &mine, b"late").await, 0);
        match tokio::time::timeout(Duration::from_millis(300), events.recv()).await {
            Err(_) | Ok(None) => {}
            Ok(Some(event)) => panic!("{protocol}: an event after closed: {event:?}"),
        }
        c.close().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn a_full_sink_drops_messages_and_counts_them() {
    for protocol in ["resp3", "resp2"] {
        let c = connect(&with_option(params(), options::PROTOCOL, protocol)).await;
        let channel = format!("m7:drops:{}", token());
        let (sink, mut events) = mpsc::channel(4);
        let subscription = RedisPubSub
            .subscribe(
                &c,
                &SubscribeRequest {
                    channels: vec![BinaryText::text(&channel)],
                    patterns: Vec::new(),
                },
                sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        // Nobody reads: `subscribed` and three messages fit, seven are dropped.
        for i in 0..10_u8 {
            assert_eq!(publish(&c, &channel, &[i]).await, 1);
        }
        // Let the server deliver everything before reading.
        let deadline = tokio::time::Instant::now() + WAIT;
        while tokio::time::Instant::now() < deadline && events.capacity() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(matches!(
            next(&mut events).await,
            PubSubEvent::Subscribed { .. }
        ));
        for i in 0..3_u8 {
            assert!(
                matches!(next(&mut events).await, PubSubEvent::Message { payload, .. } if payload == BinaryText::from_bytes(&[i])),
                "{protocol}"
            );
        }
        // The next message that fits is preceded by the count.
        publish(&c, &channel, &[42]).await;
        assert_eq!(
            next(&mut events).await,
            PubSubEvent::Dropped { count: 7 },
            "{protocol}"
        );
        assert!(
            matches!(next(&mut events).await, PubSubEvent::Message { payload, .. } if payload == BinaryText::from_bytes(&[42]))
        );
        subscription.close().await.unwrap();
        assert_eq!(next(&mut events).await, PubSubEvent::Closed { error: None });
        c.close().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "needs the redis fixture"]
async fn cancelling_the_subscribe_token_or_dropping_ends_the_subscription() {
    let c = connect(&params()).await;
    let channel = format!("m7:cancel:{}", token());
    let request = SubscribeRequest {
        channels: vec![BinaryText::text(&channel)],
        patterns: Vec::new(),
    };
    let cancel = CancellationToken::new();
    let (sink, mut events) = mpsc::channel(16);
    let subscription = RedisPubSub
        .subscribe(&c, &request, sink, cancel.clone())
        .await
        .unwrap();
    assert!(matches!(
        next(&mut events).await,
        PubSubEvent::Subscribed { .. }
    ));
    cancel.cancel();
    assert_eq!(next(&mut events).await, PubSubEvent::Closed { error: None });
    subscription.close().await.unwrap();

    let (sink, mut events) = mpsc::channel(16);
    let subscription = RedisPubSub
        .subscribe(&c, &request, sink, CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(
        next(&mut events).await,
        PubSubEvent::Subscribed { .. }
    ));
    drop(subscription);
    assert_eq!(next(&mut events).await, PubSubEvent::Closed { error: None });
    // The server forgets the subscriber.
    let deadline = tokio::time::Instant::now() + WAIT;
    while publish(&c, &channel, b"x").await > 0 {
        assert!(tokio::time::Instant::now() < deadline, "still subscribed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // An already cancelled token subscribes to nothing.
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let (sink, _events) = mpsc::channel(16);
    let err = RedisPubSub
        .subscribe(&c, &request, sink, cancelled)
        .await
        .err()
        .unwrap();
    assert!(err.is_cancelled());
    c.close().await.unwrap();
}
