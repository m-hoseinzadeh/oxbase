//! Connection commands: thin wrappers that forward to [`ConnectionManager`],
//! [`ProfileRepository`] and [`SecretStore`] and map errors to [`CommandError`].
//!
//! Every core call runs on its own task ([`isolated`]), so a panic in the core becomes an
//! `internal` error instead of a promise that never settles. Long calls (`test_connection`,
//! `open_connection`) accept an optional `request_id`; `cancel_request` cancels them through
//! their [`CancellationToken`].

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use oxbase_services::{
    CancellationToken, ConnectionInfo, ConnectionManager, SessionId, SessionSummary, profiles,
};
use oxbase_store::{ConnectionProfile, ProfileRepository, SecretStore};
use oxbase_types::{Error, Result, Secret};
use tauri::State;
use uuid::Uuid;

use crate::error::CommandError;
use crate::profiles::SecretInput;
use crate::workbench::WorkbenchState;

/// Everything the connection commands need, managed as Tauri state.
#[derive(Clone)]
pub struct ConnectionsState {
    manager: Arc<ConnectionManager>,
    profiles: Arc<dyn ProfileRepository>,
    pending: Arc<PendingRequests>,
}

impl ConnectionsState {
    pub fn new(manager: Arc<ConnectionManager>, profiles: Arc<dyn ProfileRepository>) -> Self {
        Self {
            manager,
            profiles,
            pending: Arc::default(),
        }
    }

    pub fn manager(&self) -> &Arc<ConnectionManager> {
        &self.manager
    }

    /// Registers a cancellable request (M2 workbench commands share `cancel_request`).
    pub(crate) fn start_request(&self, id: Option<String>) -> Result<PendingGuard> {
        self.pending.start(id)
    }

    fn secrets(&self) -> Arc<dyn SecretStore> {
        self.manager.secrets().clone()
    }
}

/// Cancellation tokens of in-flight requests, by the UI's request id.
#[derive(Default)]
struct PendingRequests {
    tokens: Mutex<HashMap<String, CancellationToken>>,
}

impl PendingRequests {
    /// Registers a token for `id` (if any); the guard unregisters it when the call ends.
    ///
    /// An id that is already in flight is rejected (`InvalidInput`): replacing its token would
    /// make the first call impossible to cancel, and the first call's guard would then
    /// unregister the second call's token.
    fn start(self: &Arc<Self>, id: Option<String>) -> Result<PendingGuard> {
        let token = CancellationToken::new();
        if let Some(id) = &id {
            let mut tokens = self
                .tokens
                .lock()
                .map_err(|_| Error::internal("request registry is unavailable"))?;
            if tokens.contains_key(id) {
                return Err(Error::invalid_input(format!(
                    "request id '{id}' is already in use"
                )));
            }
            tokens.insert(id.clone(), token.clone());
        }
        Ok(PendingGuard {
            pending: Arc::clone(self),
            id,
            token,
        })
    }

    fn cancel(&self, id: &str) -> bool {
        let token = self.tokens.lock().ok().and_then(|t| t.get(id).cloned());
        token.inspect(CancellationToken::cancel).is_some()
    }
}

pub(crate) struct PendingGuard {
    pending: Arc<PendingRequests>,
    id: Option<String>,
    token: CancellationToken,
}

impl PendingGuard {
    /// The request's cancellation token.
    pub(crate) fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(id) = &self.id
            && let Ok(mut tokens) = self.pending.tokens.lock()
        {
            tokens.remove(id);
        }
    }
}

/// Runs `fut` on its own task. Errors become [`CommandError`]s with `secrets` masked; a panic
/// becomes a generic internal error.
pub(crate) async fn isolated<T, F>(
    secrets: Vec<Secret>,
    fut: F,
) -> std::result::Result<T, CommandError>
where
    T: Send + 'static,
    F: Future<Output = Result<T>> + Send + 'static,
{
    match tauri::async_runtime::spawn(fut).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(CommandError::from(error).redact(&secrets)),
        Err(_) => Err(CommandError::aborted()),
    }
}

// --- core calls (plain async fns, testable without a Tauri runtime) --------------------------

pub(crate) async fn test_profile(
    state: ConnectionsState,
    mut profile: ConnectionProfile,
    input: SecretInput,
    cancel: CancellationToken,
) -> Result<ConnectionInfo> {
    // Stored passwords only go where the saved profile sends them. A test of an edited profile
    // with another server, user, TLS or SSH route (a typo, or a compromised UI) uses only values
    // typed for this call, so a saved password never reaches a new host without being retyped.
    // `save_profile` enforces the same rule for saved edits.
    profiles::restrict_to_saved_destination(state.profiles.as_ref(), &mut profile).await?;
    state
        .manager
        .test_connection(&profile, &input.provided(), cancel)
        .await
}

pub(crate) async fn open_profile(
    state: ConnectionsState,
    id: Uuid,
    input: SecretInput,
    cancel: CancellationToken,
) -> Result<SessionSummary> {
    let profile = state
        .profiles
        .get(id)
        .await?
        .ok_or_else(|| Error::not_found("this connection no longer exists"))?;
    let session = state
        .manager
        .open(&profile, &input.provided(), cancel)
        .await?;
    Ok(session.summary().clone())
}

// --- Tauri commands -------------------------------------------------------------------------

/// Every saved profile, sorted by name.
#[tauri::command]
pub async fn list_profiles(
    state: State<'_, ConnectionsState>,
) -> std::result::Result<Vec<ConnectionProfile>, CommandError> {
    let repo = Arc::clone(&state.profiles);
    isolated(Vec::new(), async move { repo.list().await }).await
}

/// Saves a profile; typed secrets go to the keychain when the profile asks for it. Returns the
/// saved profile (with its id), never the secrets.
#[tauri::command]
pub async fn save_profile(
    state: State<'_, ConnectionsState>,
    profile: ConnectionProfile,
    secrets: Option<SecretInput>,
) -> std::result::Result<ConnectionProfile, CommandError> {
    let input = secrets.unwrap_or_default();
    let (repo, store) = (Arc::clone(&state.profiles), state.secrets());
    isolated(input.secrets(), async move {
        profiles::save_profile(repo.as_ref(), store.as_ref(), profile, &input.provided()).await
    })
    .await
}

/// Deletes a profile and its keychain secrets. Open sessions of it stay open.
#[tauri::command]
pub async fn delete_profile(
    state: State<'_, ConnectionsState>,
    id: Uuid,
) -> std::result::Result<bool, CommandError> {
    let (repo, store) = (Arc::clone(&state.profiles), state.secrets());
    isolated(Vec::new(), async move {
        profiles::delete_profile(repo.as_ref(), store.as_ref(), id).await
    })
    .await
}

/// Connects with a (possibly unsaved) profile, reports server version, TLS, SSH and latency,
/// and disconnects again.
#[tauri::command]
pub async fn test_connection(
    state: State<'_, ConnectionsState>,
    profile: ConnectionProfile,
    secrets: Option<SecretInput>,
    request_id: Option<String>,
) -> std::result::Result<ConnectionInfo, CommandError> {
    let input = secrets.unwrap_or_default();
    let guard = state.pending.start(request_id)?;
    let state = ConnectionsState::clone(&*state);
    let result = isolated(
        input.secrets(),
        test_profile(state, profile, input, guard.token.clone()),
    )
    .await;
    drop(guard);
    result
}

/// Opens a session for the saved profile `id`. `secrets` answers "ask every time" fields.
#[tauri::command]
pub async fn open_connection(
    state: State<'_, ConnectionsState>,
    id: Uuid,
    secrets: Option<SecretInput>,
    request_id: Option<String>,
) -> std::result::Result<SessionSummary, CommandError> {
    let input = secrets.unwrap_or_default();
    let guard = state.pending.start(request_id)?;
    let state = ConnectionsState::clone(&*state);
    let result = isolated(
        input.secrets(),
        open_profile(state, id, input, guard.token.clone()),
    )
    .await;
    drop(guard);
    result
}

/// Cancels an in-flight `test_connection` / `open_connection`. Returns whether it was found.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn cancel_request(state: State<'_, ConnectionsState>, request_id: &str) -> bool {
    state.pending.cancel(request_id)
}

/// Closes an open session (with its child sessions) and drops the object tree's cached
/// metadata sessions of it.
#[tauri::command]
pub async fn close_connection(
    state: State<'_, ConnectionsState>,
    workbench: State<'_, WorkbenchState>,
    session_id: SessionId,
) -> std::result::Result<(), CommandError> {
    let manager = Arc::clone(&state.manager);
    let tree = Arc::clone(workbench.tree());
    isolated(Vec::new(), async move {
        let closed = manager.close(session_id).await;
        // The children are closed with the base; this only forgets them. Its error never hides
        // the close result.
        let _ = tree.forget(session_id).await;
        closed
    })
    .await
}

/// Every open session, oldest first.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn list_open_connections(state: State<'_, ConnectionsState>) -> Vec<SessionSummary> {
    state.manager.list_open()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_driver::mock::MockDriver;
    use oxbase_net::ssh::RusshTunnelProvider;
    use oxbase_services::DriverRegistry;
    use oxbase_store::{Engine, MemoryProfileRepository, MemorySecretStore};
    use oxbase_types::ErrorCategory;

    fn state() -> ConnectionsState {
        let mut drivers = DriverRegistry::new();
        drivers.register(Arc::new(MockDriver::new(Engine::Postgres)));
        let manager = ConnectionManager::new(
            drivers,
            Arc::new(MemorySecretStore::new()),
            Arc::new(RusshTunnelProvider::new()),
        );
        ConnectionsState::new(Arc::new(manager), Arc::new(MemoryProfileRepository::new()))
    }

    #[tokio::test]
    async fn isolated_maps_errors_and_masks_secrets() {
        let err = isolated::<(), _>(vec![Secret::new("marker-secret")], async {
            Err(Error::auth("rejected marker-secret"))
        })
        .await
        .unwrap_err();
        assert_eq!(err.category, ErrorCategory::Auth);
        assert_eq!(err.message, "rejected ***");
    }

    #[tokio::test]
    async fn isolated_turns_a_panic_into_an_internal_error() {
        let err = isolated::<(), _>(Vec::new(), async {
            panic!("boom with marker-secret");
        })
        .await
        .unwrap_err();
        assert_eq!(err, CommandError::aborted());
        assert!(!err.message.contains("marker-secret"));
    }

    #[tokio::test]
    async fn open_unknown_profile_is_not_found() {
        let err = open_profile(
            state(),
            Uuid::now_v7(),
            SecretInput::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        let err = CommandError::from(err);
        assert_eq!(err.category, ErrorCategory::NotFound);
        assert_eq!(err.message, "this connection no longer exists");
    }

    #[test]
    fn pending_requests_cancel_and_unregister() {
        let pending = Arc::new(PendingRequests::default());
        let guard = pending.start(Some("r1".into())).unwrap();
        assert!(!guard.token.is_cancelled());
        assert!(pending.cancel("r1"));
        assert!(guard.token.is_cancelled());
        drop(guard);
        assert!(!pending.cancel("r1"), "finished requests are forgotten");
        let anonymous = pending.start(None).unwrap();
        let another = pending.start(None).unwrap();
        assert!(!pending.cancel(""));
        drop((anonymous, another));
    }

    #[test]
    fn pending_requests_reject_an_id_in_flight() {
        let pending = Arc::new(PendingRequests::default());
        let first = pending.start(Some("dup".into())).unwrap();
        let err = CommandError::from(pending.start(Some("dup".into())).err().unwrap());
        assert_eq!(err.category, ErrorCategory::InvalidInput);
        // The first call is still cancellable, and its id is reusable once it finished.
        assert!(pending.cancel("dup"));
        assert!(first.token.is_cancelled());
        drop(first);
        let again = pending.start(Some("dup".into())).unwrap();
        assert!(!again.token.is_cancelled());
    }

    #[tokio::test]
    async fn test_uses_stored_password_only_for_the_saved_destination() {
        let state = state();
        let mut saved = ConnectionProfile::new(
            "pg",
            Engine::Postgres,
            oxbase_store::ProfileAddress::Network {
                host: "db".into(),
                port: None,
            },
        );
        saved.user = Some("app".into());
        saved.password = Some(oxbase_store::SecretRef::Keychain {
            key: saved.secret_key(oxbase_store::SecretSlot::Password),
        });
        state.profiles.save(&saved).await.unwrap();
        state
            .secrets()
            .set(
                &saved.secret_key(oxbase_store::SecretSlot::Password),
                &Secret::new("stored-marker"),
            )
            .await
            .unwrap();

        let mut moved = saved.clone();
        moved.address = oxbase_store::ProfileAddress::Network {
            host: "attacker.example".into(),
            port: None,
        };
        let err = test_profile(
            state.clone(),
            moved,
            SecretInput::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.category(), ErrorCategory::Auth, "{err}");
        assert!(!err.to_string().contains("stored-marker"));
    }

    /// The same rule through the saved-profile path: an edit that re-points a saved profile at
    /// another host without retyping its password is refused, so `open_connection` keeps going
    /// to the saved destination and the stored password never reaches the new one.
    #[tokio::test]
    async fn save_then_open_never_sends_a_stored_password_to_a_new_destination() {
        let state = state();
        let mut profile = ConnectionProfile::new(
            "pg",
            Engine::Postgres,
            oxbase_store::ProfileAddress::Network {
                host: "db".into(),
                port: None,
            },
        );
        profile.id = Uuid::nil();
        profile.user = Some("app".into());
        profile.password = Some(oxbase_store::SecretRef::Prompt);
        let mut keep = profile.clone();
        keep.password = Some(oxbase_store::SecretRef::Keychain {
            key: oxbase_store::SecretKey::new("pending").unwrap(),
        });
        let typed = SecretInput {
            password: Some(crate::profiles::SecretArg(Secret::new("stored-marker"))),
            ..SecretInput::default()
        };
        let secrets = state.secrets();
        let saved = profiles::save_profile(
            state.profiles.as_ref(),
            secrets.as_ref(),
            keep,
            &typed.provided(),
        )
        .await
        .unwrap();

        let mut moved = saved.clone();
        moved.address = oxbase_store::ProfileAddress::Network {
            host: "attacker.example".into(),
            port: None,
        };
        let err = profiles::save_profile(
            state.profiles.as_ref(),
            secrets.as_ref(),
            moved,
            &SecretInput::default().provided(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.category(), ErrorCategory::InvalidInput, "{err}");
        assert!(err.to_string().contains("retype"), "{err}");
        assert!(!err.to_string().contains("stored-marker"));

        // The saved profile still points at the original host, and opening it uses that.
        let current = state.profiles.get(saved.id).await.unwrap().unwrap();
        assert_eq!(current, saved);
        let session = open_profile(
            state.clone(),
            saved.id,
            SecretInput::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(session.profile_id, saved.id);
        state.manager.close(session.id).await.unwrap();
    }

    #[test]
    fn list_open_starts_empty() {
        assert_eq!(state().manager().list_open(), Vec::<SessionSummary>::new());
    }
}
