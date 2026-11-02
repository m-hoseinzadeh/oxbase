//! Redis publish / subscribe (redis stream). Contract: `oxbase_dialect::pubsub` module docs and
//! `docs/m7-contracts.md` §6.
//!
//! A subscription opens its own connection from the session's
//! [`RedisConnection::params`](crate::RedisConnection::params) (same resolver, TLS and
//! credentials, so it goes through the SSH tunnel too): on RESP3 a multiplexed connection with a
//! push sender, on RESP2 an `aio::PubSub`. Cluster: `SUBSCRIBE` / `PSUBSCRIBE` on one node
//! (classic pub/sub is broadcast cluster-wide); sharded `SSUBSCRIBE` is not offered in M7.

use async_trait::async_trait;
use oxbase_dialect::{PubSub, PubSubEvent, SubscribeRequest, Subscription};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{BinaryText, Result};
use tokio::sync::mpsc;

/// The Redis pub/sub cell.
#[derive(Debug, Clone, Copy, Default)]
pub struct RedisPubSub;

#[async_trait]
impl PubSub for RedisPubSub {
    async fn subscribe(
        &self,
        c: &dyn Connection,
        request: &SubscribeRequest,
        sink: mpsc::Sender<PubSubEvent>,
        cancel: CancellationToken,
    ) -> Result<Box<dyn Subscription>> {
        let _ = (c, request, sink, cancel);
        unimplemented!("m7: redis")
    }

    async fn publish(
        &self,
        c: &dyn Connection,
        channel: &BinaryText,
        payload: &BinaryText,
        cancel: CancellationToken,
    ) -> Result<u64> {
        let _ = (c, channel, payload, cancel);
        unimplemented!("m7: redis")
    }
}
