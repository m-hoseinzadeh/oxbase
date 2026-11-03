//! Long-running M8 operations by id (`docs/m8-contracts.md` §8.2): transfers, structure
//! compares and applies, data compares and applies, backups and restores register here with
//! the sessions they use, so `cancel` stops one by id and closing a connection
//! ([`RunRegistry::cancel_session`]) stops every operation that reads or writes through it.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{CancellationToken, SessionId};

/// Identifies one long-running operation (transfer, compare, apply, backup, restore) and, for
/// compares, the result the service keeps for scripts and applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(pub Uuid);

impl OperationId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for OperationId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

struct Entry {
    sessions: Vec<SessionId>,
    token: CancellationToken,
}

/// The running operations of one service. Cheap to clone (shared state).
#[derive(Clone, Default)]
pub struct RunRegistry {
    inner: Arc<Mutex<HashMap<OperationId, Entry>>>,
}

/// A registered operation. Dropping it unregisters the operation (the run task holds it until
/// its terminal event is sent).
pub struct RunTicket {
    id: OperationId,
    token: CancellationToken,
    registry: RunRegistry,
}

impl RunTicket {
    pub fn id(&self) -> OperationId {
        self.id
    }

    /// The operation's cancellation token.
    pub fn token(&self) -> &CancellationToken {
        &self.token
    }
}

impl Drop for RunTicket {
    fn drop(&mut self) {
        self.registry.lock().remove(&self.id);
    }
}

impl fmt::Debug for RunTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunTicket")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl RunRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<OperationId, Entry>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers a new operation on `sessions` with a fresh token.
    pub fn register(&self, sessions: &[SessionId]) -> RunTicket {
        self.register_with(sessions, CancellationToken::new())
    }

    /// Registers a new operation on `sessions` that `token` cancels (a request token from the
    /// app's `cancel_request`, for operations that run inside one command).
    pub fn register_with(&self, sessions: &[SessionId], token: CancellationToken) -> RunTicket {
        let id = OperationId::new();
        self.lock().insert(
            id,
            Entry {
                sessions: sessions.to_vec(),
                token: token.clone(),
            },
        );
        RunTicket {
            id,
            token,
            registry: self.clone(),
        }
    }

    /// Cancels operation `id`. `false` when it is unknown or already ended.
    pub fn cancel(&self, id: OperationId) -> bool {
        self.lock()
            .get(&id)
            .map(|e| e.token.clone())
            .inspect(CancellationToken::cancel)
            .is_some()
    }

    /// Cancels every operation that uses `session`; returns how many.
    pub fn cancel_session(&self, session: SessionId) -> usize {
        let tokens: Vec<CancellationToken> = self
            .lock()
            .values()
            .filter(|e| e.sessions.contains(&session))
            .map(|e| e.token.clone())
            .collect();
        for token in &tokens {
            token.cancel();
        }
        tokens.len()
    }

    /// `true` while operation `id` is registered.
    pub fn is_running(&self, id: OperationId) -> bool {
        self.lock().contains_key(&id)
    }

    /// Number of registered operations.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl fmt::Debug for RunRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunRegistry")
            .field("running", &self.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickets_cancel_by_id_and_by_session_and_unregister_on_drop() {
        let runs = RunRegistry::new();
        let a = SessionId::new();
        let b = SessionId::new();
        let c = SessionId::new();
        let first = runs.register(&[a, b]);
        let second = runs.register(&[b]);
        let third = runs.register_with(&[c], CancellationToken::new());
        assert_eq!(runs.len(), 3);
        assert!(runs.is_running(first.id()));

        assert_eq!(runs.cancel_session(b), 2);
        assert!(first.token().is_cancelled() && second.token().is_cancelled());
        assert!(!third.token().is_cancelled());

        assert!(runs.cancel(third.id()));
        assert!(third.token().is_cancelled());
        let gone = third.id();
        drop(third);
        assert!(!runs.cancel(gone));
        assert!(!runs.is_running(gone));
        drop(first);
        drop(second);
        assert_eq!(runs.len(), 0);
        assert_eq!(runs.cancel_session(a), 0);
        assert_eq!(
            serde_json::to_string(&gone).unwrap(),
            format!("\"{}\"", gone.0)
        );
    }
}
