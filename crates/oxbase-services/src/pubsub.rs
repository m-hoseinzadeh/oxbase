//! Publish / subscribe sessions (M7, Redis) through the engine's
//! [`PubSub`](oxbase_dialect::PubSub) role. Contract: `docs/m7-contracts.md` §6.
//!
//! - [`PubSubService::subscribe`] opens a subscription on a dedicated connection the cell
//!   derives from the session (never the session itself) and returns its id with the event
//!   receiver: `subscribed`, `message`s (lossy: a full buffer of
//!   [`SINK_CAPACITY`](oxbase_dialect::pubsub::SINK_CAPACITY) drops messages and reports
//!   `dropped {count}`), last `closed`. The app forwards the events to a Tauri `Channel`; when
//!   that channel is gone (the tab closed) it calls [`PubSubService::unsubscribe`].
//! - [`PubSubService::unsubscribe`] closes one subscription; [`PubSubService::close_session`]
//!   every subscription of a session (call it when the session closes).
//! - [`PubSubService::publish`] runs `PUBLISH` on the base session (holding
//!   `SessionHandle::exclusive`).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use oxbase_dialect::pubsub::SINK_CAPACITY;
use oxbase_dialect::{DialectRegistry, Subscription};
/// The request and event payloads (defined with the role in `oxbase_dialect::pubsub`).
pub use oxbase_dialect::{PubSubEvent, SubscribeRequest};
use oxbase_types::{BinaryText, Error, Result};

use crate::{CancellationToken, ConnectionManager, SessionHandle, SessionId, check_cancelled};

/// Identifies one subscription of a [`PubSubService`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubscriptionId(pub Uuid);

impl SubscriptionId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for SubscriptionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SubscriptionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A started subscription: its id and its events.
#[derive(Debug)]
pub struct SubscriptionHandle {
    pub id: SubscriptionId,
    pub events: mpsc::Receiver<PubSubEvent>,
}

struct Entry {
    session: SessionId,
    subscription: Arc<dyn Subscription>,
}

/// See the module docs.
pub struct PubSubService {
    manager: Arc<ConnectionManager>,
    dialects: Arc<DialectRegistry>,
    subscriptions: Mutex<HashMap<SubscriptionId, Entry>>,
}

impl PubSubService {
    pub fn new(manager: Arc<ConnectionManager>, dialects: Arc<DialectRegistry>) -> Self {
        Self {
            manager,
            dialects,
            subscriptions: Mutex::new(HashMap::new()),
        }
    }

    /// Subscribes to `request` for base session `session` (see the module docs).
    pub async fn subscribe(
        &self,
        session: SessionId,
        request: &SubscribeRequest,
        cancel: CancellationToken,
    ) -> Result<SubscriptionHandle> {
        check_cancelled(&cancel)?;
        request.validate()?;
        let handle = self.base(session)?;
        let role = self.dialects.pubsub(handle.summary().engine)?;
        let (sink, events) = mpsc::channel(SINK_CAPACITY);
        let subscription = role
            .subscribe(handle.connection().as_ref(), request, sink, cancel)
            .await?;
        let id = SubscriptionId::new();
        self.lock().insert(
            id,
            Entry {
                session,
                subscription: Arc::from(subscription),
            },
        );
        Ok(SubscriptionHandle { id, events })
    }

    /// Closes subscription `id`. Returns whether it was open.
    pub async fn unsubscribe(&self, id: SubscriptionId) -> Result<bool> {
        let entry = self.lock().remove(&id);
        match entry {
            Some(entry) => entry.subscription.close().await.map(|()| true),
            None => Ok(false),
        }
    }

    /// Closes every subscription of `session`; returns how many there were. Close errors are
    /// only logged.
    pub async fn close_session(&self, session: SessionId) -> usize {
        let closing: Vec<Arc<dyn Subscription>> = {
            let mut subscriptions = self.lock();
            let ids: Vec<SubscriptionId> = subscriptions
                .iter()
                .filter(|(_, e)| e.session == session)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| subscriptions.remove(&id))
                .map(|e| e.subscription)
                .collect()
        };
        for subscription in &closing {
            if let Err(e) = subscription.close().await {
                tracing::debug!(category = %e.category(), "closing a subscription failed");
            }
        }
        closing.len()
    }

    /// Publishes `payload` on `channel` through base session `session`; returns the number of
    /// receivers the server reports.
    pub async fn publish(
        &self,
        session: SessionId,
        channel: &BinaryText,
        payload: &BinaryText,
        cancel: CancellationToken,
    ) -> Result<u64> {
        check_cancelled(&cancel)?;
        if channel.to_bytes()?.is_empty() {
            return Err(Error::invalid_input("the channel name is empty"));
        }
        payload.to_bytes()?;
        let handle = self.base(session)?;
        let role = self.dialects.pubsub(handle.summary().engine)?;
        let _guard = handle.exclusive().await;
        role.publish(handle.connection().as_ref(), channel, payload, cancel)
            .await
    }

    /// Open subscriptions (all sessions).
    pub fn open_count(&self) -> usize {
        self.lock().len()
    }

    fn base(&self, id: SessionId) -> Result<SessionHandle> {
        self.manager
            .get(id)
            .ok_or_else(|| Error::not_found(format!("no open session {id}")))
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SubscriptionId, Entry>> {
        self.subscriptions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl fmt::Debug for PubSubService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PubSubService")
            .field("subscriptions", &self.open_count())
            .finish_non_exhaustive()
    }
}
