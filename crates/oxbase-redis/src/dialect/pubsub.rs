//! Redis publish / subscribe (redis stream). Contract: `oxbase_dialect::pubsub` module docs and
//! `docs/m7-contracts.md` §6.
//!
//! A subscription opens its own connection from the session's
//! [`RedisConnection::params`](crate::RedisConnection::params) (same resolver, TLS and
//! credentials, so it goes through the SSH tunnel too): on RESP3 a multiplexed connection with a
//! push sender, on RESP2 an `aio::PubSub`. Cluster: `SUBSCRIBE` / `PSUBSCRIBE` on one node
//! (classic pub/sub is broadcast cluster-wide); sharded `SSUBSCRIBE` is not offered in M7.
//!
//! # As built
//!
//! - RESP3: [`RedisConnection::dedicated`](crate::RedisConnection::dedicated) with a push sender
//!   that forwards straight from the connection's reader (never waiting: `try_send`), then
//!   `SUBSCRIBE` / `PSUBSCRIBE` on it. A `Disconnection` push ends the subscription with
//!   `closed {error}`.
//! - RESP2: [`RedisConnection::pubsub`](crate::RedisConnection::pubsub), then a task that reads
//!   its message stream; the stream ending means the connection was lost.
//! - Events go through one forwarder that serializes them: messages with `try_send` (a full
//!   sink counts drops; the next message that fits is preceded by `dropped {count}`), and
//!   exactly one `closed` last (sent by a task, so `close` never waits for the UI).
//! - `close`, dropping the subscription, or the subscribe call's `cancel` token firing all end
//!   it: the dedicated connection is closed (the server drops its subscriptions with it).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::StreamExt;
use oxbase_dialect::{PubSub, PubSubEvent, SubscribeRequest, Subscription};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{BinaryText, Error, Result};
use redis::{Msg, PushInfo, PushKind};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use super::keys::common::{check, int_of, native};
use crate::driver::DedicatedConnection;

/// The text of `closed {error}` when the server connection went away.
const LOST: &str = "the subscription's connection to the server was lost";

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
        check(&cancel)?;
        request.validate()?;
        let redis = native(c)?;
        let channels = names(&request.channels)?;
        let patterns = names(&request.patterns)?;
        let forward = Arc::new(Forward::new(sink));
        let stop = CancellationToken::new();
        // First in the sink, before any message can arrive; when subscribing fails the caller
        // gets the error and drops the receiver with it.
        forward.push(PubSubEvent::Subscribed {
            channels: request.channels.clone(),
            patterns: request.patterns.clone(),
        });
        let inner = if redis.protocol() == redis::ProtocolVersion::RESP3 {
            let config = redis::AsyncConnectionConfig::new()
                .set_response_timeout(None)
                .set_push_sender(Pushes(Arc::clone(&forward)));
            let mut dedicated = redis.dedicated(config, &cancel).await?;
            let conn = dedicated.connection_mut();
            let subscribed = async {
                if !channels.is_empty() {
                    conn.subscribe(channels.as_slice()).await?;
                }
                if !patterns.is_empty() {
                    conn.psubscribe(patterns.as_slice()).await?;
                }
                Ok::<_, redis::RedisError>(())
            };
            until(&cancel, subscribed).await?;
            Inner::Push(dedicated)
        } else {
            let mut pubsub = redis.pubsub(&cancel).await?;
            let subscribed = async {
                if !channels.is_empty() {
                    pubsub.subscribe(channels.as_slice()).await?;
                }
                if !patterns.is_empty() {
                    pubsub.psubscribe(patterns.as_slice()).await?;
                }
                Ok::<_, redis::RedisError>(())
            };
            until(&cancel, subscribed).await?;
            let messages = pubsub.into_on_message();
            let task = tokio::spawn(read_messages(messages, Arc::clone(&forward), stop.clone()));
            Inner::Reader(task)
        };
        let subscription = Arc::new(RedisSubscription {
            forward,
            stop,
            inner: Mutex::new(Some(inner)),
        });
        // The subscribe call's token ends the subscription too.
        let weak = Arc::downgrade(&subscription);
        let stopped = subscription.stop.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => {
                    if let Some(s) = weak.upgrade() {
                        s.shut(None).await;
                    }
                }
                () = stopped.cancelled() => {}
            }
        });
        Ok(Box::new(Handle(subscription)))
    }

    async fn publish(
        &self,
        c: &dyn Connection,
        channel: &BinaryText,
        payload: &BinaryText,
        cancel: CancellationToken,
    ) -> Result<u64> {
        check(&cancel)?;
        let redis = native(c)?;
        let channel = channel.to_bytes()?;
        if channel.is_empty() {
            return Err(Error::invalid_input("the channel name is empty"));
        }
        let mut cmd = redis::cmd("PUBLISH");
        cmd.arg(channel).arg(payload.to_bytes()?);
        let reply = redis.command(&cmd, &cancel).await?;
        Ok(int_of(&reply)
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(0))
    }
}

fn names(list: &[BinaryText]) -> Result<Vec<Vec<u8>>> {
    list.iter().map(BinaryText::to_bytes).collect()
}

/// Runs `work` unless `cancel` fires first.
async fn until<F>(cancel: &CancellationToken, work: F) -> Result<()>
where
    F: Future<Output = std::result::Result<(), redis::RedisError>>,
{
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Error::Cancelled),
        outcome = work => outcome.map_err(|e| subscribe_error(&e)),
    }
}

fn subscribe_error(e: &redis::RedisError) -> Error {
    match e.code() {
        Some(code) => Error::sql(
            Some(code.to_owned()),
            e.detail().map_or_else(|| e.to_string(), str::to_owned),
        ),
        None => Error::connection(format!("Redis subscribe: {e}")),
    }
}

/// Unix milliseconds now.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// A published message as an event.
fn message_event(msg: &Msg) -> PubSubEvent {
    let channel: Vec<u8> = msg.get_channel().unwrap_or_default();
    let pattern: Option<Vec<u8>> = msg.get_pattern().unwrap_or_default();
    PubSubEvent::Message {
        channel: BinaryText::from_bytes(&channel),
        pattern: pattern.map(|p| BinaryText::from_bytes(&p)),
        payload: BinaryText::from_bytes(msg.get_payload_bytes()),
        received_at_ms: now_ms(),
    }
}

// ---- event forwarding ---------------------------------------------------------------------------

#[derive(Default)]
struct ForwardState {
    dropped: u64,
    closed: bool,
}

/// Puts events into the bounded sink in order (see the module docs).
pub(crate) struct Forward {
    sink: mpsc::Sender<PubSubEvent>,
    state: Mutex<ForwardState>,
}

impl Forward {
    pub(crate) fn new(sink: mpsc::Sender<PubSubEvent>) -> Self {
        Self {
            sink,
            state: Mutex::new(ForwardState::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ForwardState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sends `event` now if it fits (after a pending `dropped`), else counts it as dropped.
    pub(crate) fn push(&self, event: PubSubEvent) {
        let mut state = self.lock();
        if state.closed {
            return;
        }
        if state.dropped > 0 {
            match self.sink.try_send(PubSubEvent::Dropped {
                count: state.dropped,
            }) {
                Ok(()) => state.dropped = 0,
                Err(TrySendError::Full(_)) => {
                    state.dropped += 1;
                    return;
                }
                Err(TrySendError::Closed(_)) => return,
            }
        }
        if let Err(TrySendError::Full(_)) = self.sink.try_send(event) {
            state.dropped += 1;
        }
    }

    /// Ends the event stream: a pending `dropped`, then `closed {error}`, sent by a task (the
    /// UI may be slow); nothing is pushed after it. Only the first call counts.
    pub(crate) fn finish(&self, error: Option<String>) {
        let dropped = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            std::mem::take(&mut state.dropped)
        };
        let sink = self.sink.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // Outside a runtime (a drop at shutdown): whatever fits.
            if dropped > 0 {
                let _ = sink.try_send(PubSubEvent::Dropped { count: dropped });
            }
            let _ = sink.try_send(PubSubEvent::Closed { error });
            return;
        };
        runtime.spawn(async move {
            if dropped > 0 {
                let _ = sink.send(PubSubEvent::Dropped { count: dropped }).await;
            }
            let _ = sink.send(PubSubEvent::Closed { error }).await;
        });
    }
}

/// The RESP3 push sender: forwards messages, ends on disconnection.
struct Pushes(Arc<Forward>);

impl redis::aio::AsyncPushSender for Pushes {
    fn send(&self, info: PushInfo) -> std::result::Result<(), redis::aio::SendError> {
        match info.kind {
            PushKind::Disconnection => self.0.finish(Some(LOST.to_owned())),
            PushKind::Message | PushKind::PMessage => {
                if let Some(msg) = Msg::from_push_info(info) {
                    self.0.push(message_event(&msg));
                }
            }
            _ => {}
        }
        // Never "closed": the connection must keep reading replies.
        Ok(())
    }
}

/// The RESP2 reader task.
#[allow(
    clippy::needless_pass_by_value,
    reason = "a spawned task owns its arguments"
)]
async fn read_messages(
    mut messages: redis::aio::PubSubStream,
    forward: Arc<Forward>,
    stop: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            msg = messages.next() => {
                let Some(msg) = msg else {
                    forward.finish(Some(LOST.to_owned()));
                    return;
                };
                forward.push(message_event(&msg));
            }
        }
    }
}

// ---- subscriptions ----------------------------------------------------------------------------

enum Inner {
    /// RESP3: the dedicated connection (closing it ends the subscription).
    Push(DedicatedConnection),
    /// RESP2: the task reading the pub/sub connection (it owns the connection).
    Reader(tokio::task::JoinHandle<()>),
}

struct RedisSubscription {
    forward: Arc<Forward>,
    stop: CancellationToken,
    inner: Mutex<Option<Inner>>,
}

impl RedisSubscription {
    fn take(&self) -> Option<Inner> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Closes the connection and sends `closed {error}` (once).
    async fn shut(&self, error: Option<String>) {
        self.stop.cancel();
        // `closed` is queued before the connection goes, so a `Disconnection` push of our own
        // close is not reported as a lost connection.
        self.forward.finish(error);
        match self.take() {
            Some(Inner::Push(dedicated)) => drop(dedicated),
            Some(Inner::Reader(task)) => {
                let _ = task.await;
            }
            None => {}
        }
    }
}

impl Drop for RedisSubscription {
    fn drop(&mut self) {
        self.stop.cancel();
        self.forward.finish(None);
        if let Some(Inner::Reader(task)) = self.take() {
            task.abort();
        }
    }
}

/// What `subscribe` returns: closing it (or dropping it) ends the subscription.
struct Handle(Arc<RedisSubscription>);

#[async_trait]
impl Subscription for Handle {
    /// Closing twice is `Ok` (the second call finds nothing to close).
    async fn close(&self) -> Result<()> {
        self.0.shut(None).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(n: u8) -> PubSubEvent {
        PubSubEvent::Message {
            channel: BinaryText::text("c"),
            pattern: None,
            payload: BinaryText::from_bytes(&[n]),
            received_at_ms: 0,
        }
    }

    #[tokio::test]
    async fn a_full_sink_counts_dropped_messages() {
        let (sink, mut events) = mpsc::channel(2);
        let forward = Forward::new(sink);
        for n in 0..5 {
            forward.push(message(n));
        }
        assert_eq!(events.recv().await, Some(message(0)));
        assert_eq!(events.recv().await, Some(message(1)));
        // Room for two again: the drop count, then the next message.
        forward.push(message(5));
        assert_eq!(events.recv().await, Some(PubSubEvent::Dropped { count: 3 }));
        assert_eq!(events.recv().await, Some(message(5)));
        forward.finish(None);
        forward.push(message(6));
        forward.finish(Some("again".into()));
        assert_eq!(
            events.recv().await,
            Some(PubSubEvent::Closed { error: None })
        );
        drop(forward);
        assert_eq!(events.recv().await, None);
    }

    #[tokio::test]
    async fn closing_reports_pending_drops_first() {
        let (sink, mut events) = mpsc::channel(1);
        let forward = Forward::new(sink);
        forward.push(message(0));
        forward.push(message(1));
        forward.finish(Some(LOST.to_owned()));
        assert_eq!(events.recv().await, Some(message(0)));
        assert_eq!(events.recv().await, Some(PubSubEvent::Dropped { count: 1 }));
        assert_eq!(
            events.recv().await,
            Some(PubSubEvent::Closed {
                error: Some(LOST.to_owned())
            })
        );
    }

    #[test]
    fn push_messages_become_events() {
        let info = PushInfo {
            kind: PushKind::PMessage,
            data: vec![
                redis::Value::BulkString(b"m7:*".to_vec()),
                redis::Value::BulkString(b"m7:news".to_vec()),
                redis::Value::BulkString(vec![0xff]),
            ],
        };
        let msg = Msg::from_push_info(info).unwrap();
        match message_event(&msg) {
            PubSubEvent::Message {
                channel,
                pattern,
                payload,
                received_at_ms,
            } => {
                assert_eq!(channel, BinaryText::text("m7:news"));
                assert_eq!(pattern, Some(BinaryText::text("m7:*")));
                assert_eq!(payload, BinaryText::from_bytes(&[0xff]));
                assert!(received_at_ms > 0);
            }
            other => panic!("{other:?}"),
        }
    }
}
