//! The publish / subscribe role (M7, Redis). Contract: `docs/m7-contracts.md` §6.
//!
//! One [`PubSub`] per engine, registered with
//! [`DialectRegistry::register_pubsub`](crate::DialectRegistry::register_pubsub).
//! `oxbase_services::pubsub::PubSubService` owns the subscriptions and forwards their events to
//! the UI (a Tauri `Channel`).
//!
//! # Rules
//!
//! - A subscription runs on a **dedicated connection** the cell opens from the session's
//!   parameters (same server endpoint, dial address, TLS and credentials; through the SSH
//!   tunnel when the session uses one), never on the session itself, so the console and the
//!   tree keep working. RESP3 servers use a push-capable connection; RESP2 servers a dedicated
//!   pub/sub connection.
//! - Events go into the bounded `sink` with `try_send`: a full sink drops the message and
//!   counts it; the next event that fits is preceded by one [`PubSubEvent::Dropped`] with the
//!   count. Reading from the server never waits for the UI.
//! - [`Subscription::close`] (or dropping the subscription, or `cancel`) unsubscribes and
//!   closes the dedicated connection; the last event is [`PubSubEvent::Closed`]. A lost
//!   connection ends the subscription with `Closed { error }` (no silent reconnect: the UI
//!   shows it and may subscribe again).
//! - Payloads are bytes ([`BinaryText`]); channel names too.

use async_trait::async_trait;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{BinaryText, Error, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Capacity of a subscription's event sink (the UI's buffer between the server and the
/// channel).
pub const SINK_CAPACITY: usize = 1024;

/// What to subscribe to. At least one channel or pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeRequest {
    /// `SUBSCRIBE` channels.
    #[serde(default)]
    pub channels: Vec<BinaryText>,
    /// `PSUBSCRIBE` glob patterns.
    #[serde(default)]
    pub patterns: Vec<BinaryText>,
}

impl SubscribeRequest {
    /// `InvalidInput` when there is nothing to subscribe to, or a name is empty or bad hex.
    pub fn validate(&self) -> Result<()> {
        if self.channels.is_empty() && self.patterns.is_empty() {
            return Err(Error::invalid_input(
                "subscribe to at least one channel or pattern",
            ));
        }
        for name in self.channels.iter().chain(&self.patterns) {
            if name.to_bytes()?.is_empty() {
                return Err(Error::invalid_input("a channel name or pattern is empty"));
            }
        }
        Ok(())
    }
}

/// One event of a subscription, in order: `subscribed`, then `message`s (with `dropped`
/// between them when the sink was full), last `closed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PubSubEvent {
    /// The server confirmed every channel and pattern of the request.
    Subscribed {
        channels: Vec<BinaryText>,
        patterns: Vec<BinaryText>,
    },
    /// One published message.
    Message {
        channel: BinaryText,
        /// The pattern that matched, for `PSUBSCRIBE` deliveries.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<BinaryText>,
        payload: BinaryText,
        /// When the message arrived, Unix milliseconds (client clock).
        received_at_ms: i64,
    },
    /// This many messages were dropped because the sink was full.
    Dropped { count: u64 },
    /// The subscription ended: closed by the caller (`error` absent) or lost.
    Closed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// A running subscription (see the module docs).
#[async_trait]
pub trait Subscription: Send + Sync {
    /// Unsubscribes, closes the dedicated connection and sends [`PubSubEvent::Closed`]. Closing
    /// twice is `Ok`.
    async fn close(&self) -> Result<()>;
}

/// Publish / subscribe of one engine.
#[async_trait]
pub trait PubSub: Send + Sync {
    /// Opens a dedicated connection like `c` (see the module docs), subscribes to `request`
    /// (already validated), sends [`PubSubEvent::Subscribed`] and keeps forwarding messages
    /// into `sink` until closed. Fails before anything was subscribed (`Unsupported` on a
    /// server without pub/sub, connection errors, `Cancelled`).
    async fn subscribe(
        &self,
        c: &dyn Connection,
        request: &SubscribeRequest,
        sink: mpsc::Sender<PubSubEvent>,
        cancel: CancellationToken,
    ) -> Result<Box<dyn Subscription>>;

    /// `PUBLISH channel payload` on `c`; returns how many clients received it (on a cluster,
    /// the receivers of the node that served it).
    async fn publish(
        &self,
        c: &dyn Connection,
        channel: &BinaryText,
        payload: &BinaryText,
        cancel: CancellationToken,
    ) -> Result<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn events_serialize_in_camel_case() {
        let event = PubSubEvent::Message {
            channel: BinaryText::text("news"),
            pattern: None,
            payload: BinaryText::from_bytes(&[0xff]),
            received_at_ms: 5,
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({
                "type": "message",
                "channel": {"encoding": "utf8", "text": "news"},
                "payload": {"encoding": "hex", "hex": "ff"},
                "receivedAtMs": 5
            })
        );
        assert_eq!(
            serde_json::to_value(PubSubEvent::Dropped { count: 3 }).unwrap(),
            json!({"type": "dropped", "count": 3})
        );
    }

    #[test]
    fn requests_need_a_name() {
        let mut r = SubscribeRequest {
            channels: Vec::new(),
            patterns: Vec::new(),
        };
        assert!(r.validate().is_err());
        r.patterns.push(BinaryText::text("m7:*"));
        assert!(r.validate().is_ok());
        r.channels.push(BinaryText::text(""));
        assert!(r.validate().is_err());
    }
}
