//! Service orchestration (sessions, query runs, jobs, documents) shared by the Tauri app and
//! `oxbase-cli`. Never depends on Tauri.
//!
//! # Connection manager (M1)
//!
//! [`ConnectionManager`] turns a saved [`ConnectionProfile`] into an open session:
//!
//! 1. **Resolve secrets.** Every [`SecretRef::Keychain`] / [`KeyRef::Keychain`] in the profile is
//!    read from the [`SecretStore`]; [`SecretRef::Prompt`] values come from [`ProvidedSecrets`]
//!    (a missing one is `Error::Auth` naming the field, never the value). Values in
//!    [`ProvidedSecrets`] win over stored ones. A profile may only read its own keychain entries
//!    (`connection/<profile id>/...`); a reference to another key is `Error::InvalidInput`, so a
//!    hand-edited or imported profile cannot send another connection's secret to its server.
//! 2. **Open the tunnel** if the profile has SSH. The target is the profile's *server* endpoint
//!    (host + effective port). See *Tunnel sharing* below.
//! 3. **Build [`ConnectParams`]**: `host`/`port` = the server endpoint, `dial_addr` = the
//!    tunnel's `local_addr()` (or `None`), `tls` from the profile (`TlsOptions`; drivers build
//!    rustls with `oxbase_net::tls::client_config` and verify the *server* host or
//!    `tls.server_name`), `ssh` = the resolved `SshOptions`.
//! 4. **Connect** through the [`DriverRegistry`].
//! 5. **Register** the session: the [`SessionHandle`] owns the connection and the tunnel `Arc`;
//!    on close the connection is closed first, then the tunnel reference is dropped.
//!
//! Every step honours the [`CancellationToken`] (`Error::Cancelled`), and a failure after the
//! tunnel opened drops it again. Secrets live only in the resolved `ConnectParams`/`SshOptions`
//! of an attempt and are never logged.
//!
//! # Deadline
//!
//! When the profile sets `connect_timeout_secs`, the network part of a connect (opening the
//! tunnel *and* the driver connect) is bounded by the manager as a whole: the profile's timeout,
//! plus the SSH connect timeout when there is a tunnel (its own setting or the provider default),
//! plus a grace period. So neither a slow bastion nor a driver that ignores its deadline can hang
//! the caller. Secret resolution (step 1) is not under that deadline because it may wait for the
//! user at an OS keychain unlock prompt; the [`CancellationToken`] still interrupts it.
//!
//! # Tunnel sharing
//!
//! Sessions opened with [`ConnectionManager::open`] share an SSH tunnel only when they go to the
//! same SSH server as the same SSH user to the same target (the `TunnelPool` key) **and** use
//! the same host-key policy and the same SSH credential references (auth method, key file or
//! keychain entry). A profile with a stricter host-key policy or other credentials therefore
//! never rides on a tunnel that was checked or authenticated differently. Two cases always open
//! a dedicated tunnel that authenticates for real: [`ConnectionManager::test_connection`], and
//! a connect whose SSH password or key passphrase is supplied in [`ProvidedSecrets`] (it may
//! differ from what an existing tunnel used).
//!
//! [`default_drivers`] builds the registry the app and the CLI use (PostgreSQL, MySQL, MariaDB
//! and SQLite).
//!
//! # Profiles
//!
//! [`profiles`] saves, tests and deletes profiles together with their keychain secrets, for both
//! front ends. A stored database or SSH password only goes to the destination it was saved for:
//! an edit that changes the server, user, TLS or SSH route must retype it.
//!
//! # M2: tree, query runs, history
//!
//! Contracts in `docs/m2-contracts.md`:
//!
//! - [`default_dialects`] builds the [`DialectRegistry`] (fetchers, tree layouts) of every
//!   engine, like [`default_drivers`] for drivers.
//! - [`tree::TreeService`] lists the children of an object-tree node and loads object
//!   definitions through the registry.
//! - [`query::QueryService`] runs a script statement by statement on an editor session and keeps
//!   every result set in a [`results::ResultStore`] (memory up to a budget, then Arrow IPC spill
//!   files) that the grid pages through, even while rows still arrive.
//! - [`history::HistoryService`] records executed statements (redacted) in a
//!   [`HistoryRepository`](oxbase_store::HistoryRepository).
//!
//! # M3: table data, grid editing, table designer
//!
//! Contracts in `docs/m3-contracts.md`:
//!
//! - [`edit::EditService`] opens a table's rows in the grid with a typed filter and sort
//!   (`SELECT * ... WHERE ... ORDER BY` with bound values, run through the M2 query path),
//!   reports whether they can be edited (primary key, else a NOT NULL unique key; otherwise a
//!   read-only reason), applies the UI's pending changes in one transaction with optimistic
//!   concurrency (every update and delete must hit exactly one row) and reads single rows for
//!   the form / JSON view. [`convert`] turns Arrow cells and edit text into typed values.
//! - [`designer::DesignerService`] loads a table into an `oxbase_designer::TableDesign`,
//!   previews the DDL of an edited design (diff, script, atomic or not, destructive or not) and
//!   applies it on a dedicated session; [`designer::DesignerService::capabilities`] tells the UI
//!   what the engine offers.
//!
//! **Sessions.** A session opened with [`ConnectionManager::open`] is a *base* session: the
//! object tree reads metadata through it. Query editors never run on a base session; each
//! editor gets a *child* session ([`ConnectionManager::open_child`]) with the same resolved
//! parameters and tunnel, so a long query never blocks the tree and every editor keeps its own
//! transaction state. On PostgreSQL ([`DatabaseScope::Session`](oxbase_dialect::DatabaseScope))
//! the tree also opens a child per other database it browses. Children are not listed by
//! [`ConnectionManager::list_open`] and close with their base. Every user of
//! [`SessionHandle::connection`] holds [`SessionHandle::exclusive`] while it has a statement
//! open, because a connection runs one statement at a time.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use oxbase_driver::{CancellationToken, ConnectParams, Connection, DriverRegistry};
use oxbase_driver::{Endpoint, KeySource, PemSource, SshAuth, SshOptions, TlsOptions};
use oxbase_net::ssh::{DEFAULT_CONNECT_TIMEOUT, TunnelHandle, TunnelPool, TunnelProvider};
pub use oxbase_store::{
    ConnectionProfile, KeyRef, ProfileRepository, SecretKey, SecretRef, SecretStore,
};
use oxbase_store::{PemRef, ProfileAddress, SshAuthSettings, SshSettings, TlsSettings};
use oxbase_types::{Engine, Error, KnownHostsPolicy, Result, Secret, ServerVersion};

pub mod convert;
pub mod designer;
mod dialects;
mod drivers;
pub mod edit;
pub mod history;
pub mod profiles;
pub mod query;
pub mod results;
pub mod tree;

pub use dialects::default_dialects;
pub use drivers::default_drivers;
pub use oxbase_dialect::DialectRegistry;

/// Sent to servers that report a client name (PostgreSQL `application_name`, ...).
pub const APPLICATION_NAME: &str = "Oxbase";

/// Extra time the manager allows on top of a profile's `connect_timeout_secs` (and the SSH
/// connect timeout) before it gives up on a step that does not honour its own deadline. Short in
/// unit tests so deadline tests run fast.
const CONNECT_GRACE: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(5)
};

/// Identifies an open session inside one [`ConnectionManager`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What a successful connect learned (returned by `test_connection`, kept on each session).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub server_version: ServerVersion,
    /// The transport is TLS-encrypted (`Connection::tls_in_use`).
    pub tls_in_use: bool,
    /// The connection goes through an SSH tunnel.
    pub via_ssh: bool,
    /// Wall time of the whole connect (secrets, tunnel, TCP, TLS, auth), in milliseconds.
    pub latency_ms: u64,
}

/// Secrets supplied by the caller for this connect only: answers to [`SecretRef::Prompt`]
/// fields, or overrides of stored ones. Never persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvidedSecrets {
    pub password: Option<Secret>,
    pub ssh_password: Option<Secret>,
    pub ssh_key_passphrase: Option<Secret>,
}

impl ProvidedSecrets {
    /// No secrets: everything must come from the [`SecretStore`].
    pub fn none() -> Self {
        Self::default()
    }
}

/// A profile resolved for one connect attempt: driver parameters (with secrets) and the tunnel
/// they dial, if any. Keep `tunnel` alive for as long as a connection made from `params` lives.
pub struct ResolvedConnection {
    pub params: ConnectParams,
    pub tunnel: Option<Arc<TunnelHandle>>,
}

impl fmt::Debug for ResolvedConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedConnection")
            .field("params", &self.params)
            .field("tunnel", &self.tunnel)
            .finish()
    }
}

/// Serializable view of an open session (for the UI and the CLI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub profile_id: Uuid,
    pub profile_name: String,
    pub engine: Engine,
    pub info: ConnectionInfo,
    pub opened_at: DateTime<Utc>,
    /// M2: the database this session was opened on when it was asked for one (the profile's
    /// database for a base session, the requested one for a child). `None`: the server default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// M2: for a child session ([`ConnectionManager::open_child`]), the base session it belongs
    /// to. `None` for base sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionId>,
}

/// An open session. Cheap to clone; all clones share the connection.
///
/// The connection is dropped before the tunnel (field order), so the driver never outlives the
/// tunnel it dials.
#[derive(Clone)]
pub struct SessionHandle {
    inner: Arc<SessionInner>,
}

struct SessionInner {
    summary: SessionSummary,
    /// Held while a statement is open on `connection` (see [`SessionHandle::exclusive`]).
    exec: Arc<tokio::sync::Mutex<()>>,
    // Field order matters: `connection` drops before `tunnel`.
    connection: Arc<dyn Connection>,
    tunnel: Option<Arc<TunnelHandle>>,
}

/// Exclusive use of a session's connection; see [`SessionHandle::exclusive`].
pub type SessionGuard = tokio::sync::OwnedMutexGuard<()>;

impl SessionHandle {
    /// Used by the manager once a connection is up.
    pub fn new(
        summary: SessionSummary,
        connection: Arc<dyn Connection>,
        tunnel: Option<Arc<TunnelHandle>>,
    ) -> Self {
        Self {
            inner: Arc::new(SessionInner {
                summary,
                exec: Arc::new(tokio::sync::Mutex::new(())),
                connection,
                tunnel,
            }),
        }
    }

    pub fn id(&self) -> SessionId {
        self.inner.summary.id
    }

    pub fn summary(&self) -> &SessionSummary {
        &self.inner.summary
    }

    pub fn info(&self) -> &ConnectionInfo {
        &self.inner.summary.info
    }

    pub fn connection(&self) -> &Arc<dyn Connection> {
        &self.inner.connection
    }

    /// The tunnel this session dials, if it goes through SSH.
    pub fn tunnel(&self) -> Option<&Arc<TunnelHandle>> {
        self.inner.tunnel.as_ref()
    }

    /// Waits for exclusive use of the connection and returns a guard that keeps it until
    /// dropped (M2).
    ///
    /// A connection runs one statement at a time, so every caller of
    /// [`SessionHandle::connection`] (tree fetchers, query runs) holds this guard from
    /// `execute` until the result stream is drained or dropped. Waiting is fair (FIFO). Wrap
    /// the wait in the caller's cancellation token.
    pub async fn exclusive(&self) -> SessionGuard {
        Arc::clone(&self.inner.exec).lock_owned().await
    }

    /// The session's current database as the driver reports it
    /// ([`Connection::current_database`]).
    pub fn current_database(&self) -> Option<String> {
        self.inner.connection.current_database()
    }
}

impl fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionHandle")
            .field("summary", &self.inner.summary)
            .field("tunnel", &self.inner.tunnel)
            .finish_non_exhaustive()
    }
}

/// Which tunnels may be shared beyond the `TunnelPool` key (SSH endpoint, user, target): the
/// host-key policy and the credential *references* (never values). See the crate docs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TunnelScope {
    known_hosts: KnownHostsPolicy,
    auth: SshAuthSettings,
}

/// Whether a connect may reuse a pooled tunnel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sharing {
    /// Reuse a live tunnel of the same scope, or open one and pool it.
    Pooled,
    /// Always open (and authenticate) a new tunnel that nothing else uses.
    Dedicated,
}

/// Opens, tracks and closes connections for both front ends. See the crate docs for the flow.
pub struct ConnectionManager {
    drivers: DriverRegistry,
    secrets: Arc<dyn SecretStore>,
    provider: Arc<dyn TunnelProvider>,
    /// One pool per sharing scope.
    pools: Mutex<HashMap<TunnelScope, Arc<TunnelPool>>>,
    /// Tunnels opened outside the pools (tests, provided SSH secrets), for `open_tunnel_count`.
    dedicated: Mutex<Vec<Weak<TunnelHandle>>>,
    sessions: Mutex<HashMap<SessionId, SessionHandle>>,
    /// M2: the resolved parameters (secrets included) of every base session opened by
    /// [`ConnectionManager::open`], for [`ConnectionManager::open_child`]. Dropped when the base
    /// closes; never serialized or logged.
    bases: Mutex<HashMap<SessionId, ConnectParams>>,
}

impl ConnectionManager {
    pub fn new(
        drivers: DriverRegistry,
        secrets: Arc<dyn SecretStore>,
        tunnels: Arc<dyn TunnelProvider>,
    ) -> Self {
        Self {
            drivers,
            secrets,
            provider: tunnels,
            pools: Mutex::new(HashMap::new()),
            dedicated: Mutex::new(Vec::new()),
            sessions: Mutex::new(HashMap::new()),
            bases: Mutex::new(HashMap::new()),
        }
    }

    pub fn drivers(&self) -> &DriverRegistry {
        &self.drivers
    }

    pub fn secrets(&self) -> &Arc<dyn SecretStore> {
        &self.secrets
    }

    /// Number of SSH tunnels currently open (pooled and dedicated, alive and not closed).
    pub fn open_tunnel_count(&self) -> usize {
        let mut open = 0;
        if let Ok(pools) = self.pools.lock() {
            for pool in pools.values() {
                open += pool.open_count();
            }
        }
        if let Ok(mut dedicated) = self.dedicated.lock() {
            dedicated.retain(|t| t.upgrade().is_some_and(|t| !t.is_closed()));
            open += dedicated.len();
        }
        open
    }

    /// Steps 1-3 of the crate docs: resolves secrets, opens (or reuses, see *Tunnel sharing*)
    /// the SSH tunnel and builds the driver parameters. Validates the profile first.
    ///
    /// The returned [`ResolvedConnection::tunnel`] keeps the tunnel open; drop it once the
    /// connection made from the parameters is closed.
    pub async fn resolve(
        &self,
        profile: &ConnectionProfile,
        provided: &ProvidedSecrets,
        cancel: CancellationToken,
    ) -> Result<ResolvedConnection> {
        let (params, ssh) = self.prepare(profile, provided, &cancel).await?;
        self.attach_tunnel(profile, provided, params, ssh, Sharing::Pooled, &cancel)
            .await
    }

    /// Step 1 and step 3 without the tunnel: validated parameters (`dial_addr`, `ssh` unset) and
    /// the resolved SSH options, if the profile has SSH.
    async fn prepare(
        &self,
        profile: &ConnectionProfile,
        provided: &ProvidedSecrets,
        cancel: &CancellationToken,
    ) -> Result<(ConnectParams, Option<SshOptions>)> {
        check_cancelled(cancel)?;
        profile.validate()?;
        // Fail on a missing driver before touching the keychain or opening a tunnel.
        self.drivers.require(profile.engine)?;

        let secrets = until_cancelled(cancel, self.resolve_secrets(profile, provided)).await?;
        let params = build_params(profile, &secrets)?;
        // Everything except the tunnel is checked before the tunnel opens.
        params.validate()?;
        Ok((params, secrets.ssh))
    }

    /// Step 2 and the rest of step 3: opens or reuses the tunnel and points `params` at it.
    async fn attach_tunnel(
        &self,
        profile: &ConnectionProfile,
        provided: &ProvidedSecrets,
        mut params: ConnectParams,
        ssh: Option<SshOptions>,
        sharing: Sharing,
        cancel: &CancellationToken,
    ) -> Result<ResolvedConnection> {
        let Some(ssh) = ssh else {
            return Ok(ResolvedConnection {
                params,
                tunnel: None,
            });
        };
        let target = params.server_endpoint().ok_or_else(|| {
            Error::invalid_input(format!(
                "connection '{}' has no server address for the SSH tunnel",
                profile.name
            ))
        })?;
        let scope = match (&profile.ssh, sharing) {
            (Some(settings), Sharing::Pooled) if !uses_provided_ssh_secret(settings, provided) => {
                Some(TunnelScope {
                    known_hosts: settings.known_hosts.clone(),
                    auth: settings.auth.clone(),
                })
            }
            _ => None,
        };
        let tunnel = self.tunnel(scope, &ssh, &target, cancel).await?;
        params.dial_addr = Some(tunnel.local_addr());
        params.ssh = Some(ssh);
        params.validate()?;
        Ok(ResolvedConnection {
            params,
            tunnel: Some(tunnel),
        })
    }

    /// A tunnel from the pool of `scope`, or a dedicated one when `scope` is `None`.
    async fn tunnel(
        &self,
        scope: Option<TunnelScope>,
        ssh: &SshOptions,
        target: &Endpoint,
        cancel: &CancellationToken,
    ) -> Result<Arc<TunnelHandle>> {
        if let Some(scope) = scope {
            let pool = self.pool(scope)?;
            return until_cancelled(cancel, pool.acquire(ssh, target, cancel.clone())).await;
        }
        let tunnel = Arc::new(
            until_cancelled(cancel, self.provider.open(ssh, target, cancel.clone())).await?,
        );
        let mut dedicated = self
            .dedicated
            .lock()
            .map_err(|_| Error::internal("tunnel list lock poisoned"))?;
        dedicated.retain(|t| t.strong_count() > 0);
        dedicated.push(Arc::downgrade(&tunnel));
        Ok(tunnel)
    }

    fn pool(&self, scope: TunnelScope) -> Result<Arc<TunnelPool>> {
        let mut pools = self
            .pools
            .lock()
            .map_err(|_| Error::internal("tunnel pool table lock poisoned"))?;
        Ok(pools
            .entry(scope)
            .or_insert_with(|| Arc::new(TunnelPool::new(self.provider.clone())))
            .clone())
    }

    /// Connects with `profile`, measures it, closes the connection again and reports what it
    /// found. Does not register a session. A tunnel opened only for the test is closed.
    pub async fn test_connection(
        &self,
        profile: &ConnectionProfile,
        provided: &ProvidedSecrets,
        cancel: CancellationToken,
    ) -> Result<ConnectionInfo> {
        // A test always authenticates for real: never reuse a pooled tunnel.
        let (connection, tunnel, info, params) = self
            .connect(profile, provided, cancel, Sharing::Dedicated)
            .await?;
        drop(params);
        if let Err(e) = connection.close().await {
            tracing::debug!(profile = %profile.id, error = %e, "closing test connection failed");
        }
        // The connection goes before the tunnel it dials.
        drop(connection);
        drop(tunnel);
        Ok(info)
    }

    /// Opens a session for `profile` and registers it (see [`ConnectionManager::list_open`]).
    pub async fn open(
        &self,
        profile: &ConnectionProfile,
        provided: &ProvidedSecrets,
        cancel: CancellationToken,
    ) -> Result<SessionHandle> {
        let (connection, tunnel, info, params) = self
            .connect(profile, provided, cancel, Sharing::Pooled)
            .await?;
        let summary = SessionSummary {
            id: SessionId::new(),
            profile_id: profile.id,
            profile_name: profile.name.clone(),
            engine: profile.engine,
            info,
            opened_at: Utc::now(),
            database: profile.database.clone(),
            parent: None,
        };
        let handle = SessionHandle::new(summary, Arc::from(connection), tunnel);
        if let Err(e) = self.register(handle.clone()) {
            let _ = handle.connection().close().await;
            return Err(e);
        }
        let stored = self
            .bases
            .lock()
            .map(|mut bases| bases.insert(handle.id(), params))
            .is_ok();
        if !stored {
            let _ = self.close(handle.id()).await;
            return Err(Error::internal("session parameter table lock poisoned"));
        }
        tracing::debug!(
            session = %handle.id(),
            profile = %profile.id,
            engine = %profile.engine,
            via_ssh = handle.info().via_ssh,
            tls = handle.info().tls_in_use,
            "session opened"
        );
        Ok(handle)
    }

    /// Steps 1-4: resolve, connect through the driver, measure. On any failure (or cancel) the
    /// connection is closed and the tunnel reference is dropped before returning. The tunnel and
    /// the driver connect run under the deadline described in the crate docs.
    async fn connect(
        &self,
        profile: &ConnectionProfile,
        provided: &ProvidedSecrets,
        cancel: CancellationToken,
        sharing: Sharing,
    ) -> Result<(
        Box<dyn Connection>,
        Option<Arc<TunnelHandle>>,
        ConnectionInfo,
        ConnectParams,
    )> {
        let started = Instant::now();
        let (params, ssh) = self.prepare(profile, provided, &cancel).await?;
        let deadline = params.connect_timeout.map(|limit| {
            let tunnel_budget = ssh.as_ref().map_or(Duration::ZERO, |s| {
                s.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT)
            });
            limit
                .saturating_add(tunnel_budget)
                .saturating_add(CONNECT_GRACE)
        });

        // On an error inside, `tunnel` (and the parameters with their secrets) drop here.
        let network = async {
            let ResolvedConnection { params, tunnel } = self
                .attach_tunnel(profile, provided, params, ssh, sharing, &cancel)
                .await?;
            let connection = self.drivers.connect(&params).await?;
            Ok::<_, Error>((connection, tunnel, params))
        };
        let bounded = async {
            match deadline {
                Some(deadline) => tokio::time::timeout(deadline, network)
                    .await
                    .unwrap_or_else(|_| {
                        Err(Error::timeout(format!(
                            "connecting to '{}' took longer than {} s",
                            profile.name,
                            deadline.as_secs()
                        )))
                    }),
                None => network.await,
            }
        };
        let (connection, tunnel, params) = until_cancelled(&cancel, bounded).await?;
        if cancel.is_cancelled() {
            // Connected in the same instant the caller gave up.
            let _ = connection.close().await;
            drop(connection);
            drop(params);
            drop(tunnel);
            return Err(Error::Cancelled);
        }
        let info = ConnectionInfo {
            server_version: connection.server_version().clone(),
            tls_in_use: connection.tls_in_use(),
            via_ssh: tunnel.is_some(),
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        Ok((connection, tunnel, info, params))
    }

    /// Step 1: every secret the profile needs, from the caller or the store.
    async fn resolve_secrets(
        &self,
        profile: &ConnectionProfile,
        provided: &ProvidedSecrets,
    ) -> Result<ResolvedSecrets> {
        let password = self
            .secret(
                profile,
                "password",
                profile.password.as_ref(),
                provided.password.as_ref(),
            )
            .await?;
        let tls = self.tls_options(profile, &profile.tls).await?;
        let ssh = match &profile.ssh {
            Some(ssh) => Some(self.ssh_options(profile, ssh, provided).await?),
            None => None,
        };
        Ok(ResolvedSecrets { password, tls, ssh })
    }

    /// Resolves one secret: `provided` wins; otherwise the reference decides. `field` names the
    /// secret in errors (never its value).
    async fn secret(
        &self,
        profile: &ConnectionProfile,
        field: &str,
        reference: Option<&SecretRef>,
        provided: Option<&Secret>,
    ) -> Result<Option<Secret>> {
        if let Some(value) = provided {
            return Ok(Some(value.clone()));
        }
        match reference {
            None => Ok(None),
            Some(SecretRef::Prompt) => Err(Error::auth(format!(
                "connection '{}' needs the {field}: it is not saved, enter it to connect",
                profile.name
            ))),
            Some(SecretRef::Keychain { key }) => Ok(Some(self.stored(profile, field, key).await?)),
        }
    }

    /// Reads `key` from the secret store; missing is an `Auth` error naming the field and key.
    async fn stored(
        &self,
        profile: &ConnectionProfile,
        field: &str,
        key: &SecretKey,
    ) -> Result<Secret> {
        let own = format!("connection/{}/", profile.id);
        if !key.as_str().starts_with(&own) {
            return Err(Error::invalid_input(format!(
                "connection '{}': the {field} refers to the keychain entry {key}, which belongs \
                 to another connection; enter the {field} again",
                profile.name
            )));
        }
        self.secrets.get(key).await?.ok_or_else(|| {
            Error::auth(format!(
                "connection '{}': the saved {field} is missing from the keychain ({key}); \
                 enter it again",
                profile.name
            ))
        })
    }

    async fn key_source(
        &self,
        profile: &ConnectionProfile,
        field: &str,
        key: &KeyRef,
    ) -> Result<KeySource> {
        Ok(match key {
            KeyRef::File { path } => KeySource::File(path.clone()),
            KeyRef::Keychain { key } => KeySource::Pem(self.stored(profile, field, key).await?),
        })
    }

    async fn tls_options(
        &self,
        profile: &ConnectionProfile,
        tls: &TlsSettings,
    ) -> Result<TlsOptions> {
        let client_key = match &tls.client_key {
            Some(key) => Some(self.key_source(profile, "TLS client key", key).await?),
            None => None,
        };
        Ok(TlsOptions {
            mode: tls.mode,
            ca_cert: tls.ca_cert.as_ref().map(pem_source),
            client_cert: tls.client_cert.as_ref().map(pem_source),
            client_key,
            server_name: tls
                .server_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        })
    }

    async fn ssh_options(
        &self,
        profile: &ConnectionProfile,
        ssh: &SshSettings,
        provided: &ProvidedSecrets,
    ) -> Result<SshOptions> {
        let auth = match &ssh.auth {
            SshAuthSettings::Password { password } => {
                let secret = self
                    .secret(
                        profile,
                        "SSH password",
                        Some(password),
                        provided.ssh_password.as_ref(),
                    )
                    .await?
                    .unwrap_or_default();
                SshAuth::Password(secret)
            }
            SshAuthSettings::PrivateKey { key, passphrase } => SshAuth::PrivateKey {
                key: self.key_source(profile, "SSH private key", key).await?,
                passphrase: self
                    .secret(
                        profile,
                        "SSH key passphrase",
                        passphrase.as_ref(),
                        provided.ssh_key_passphrase.as_ref(),
                    )
                    .await?,
            },
            SshAuthSettings::Agent => SshAuth::Agent,
        };
        Ok(SshOptions {
            host: ssh.host.trim().to_owned(),
            port: ssh.port,
            user: ssh.user.clone(),
            auth,
            known_hosts: ssh.known_hosts.clone(),
            connect_timeout: ssh.connect_timeout_secs.map(secs),
            keepalive_interval: ssh.keepalive_secs.map(secs),
        })
    }

    /// Opens a child session of the open base session `base` (M2): a new connection with the
    /// base's resolved parameters (its secrets are not asked for again) and the same SSH tunnel,
    /// on `database` when given (else the base's database).
    ///
    /// The child is tracked under its base: [`ConnectionManager::get`] finds it,
    /// [`ConnectionManager::list_open`] does not list it, [`ConnectionManager::close`] on the
    /// child closes only the child, and closing the base closes its children first. Its
    /// [`SessionSummary`] has `parent = Some(base)` and `database` set. Opening a child of a
    /// child is `InvalidInput`; an unknown base is `NotFound`. Honours `cancel` like
    /// [`ConnectionManager::open`].
    ///
    /// To do this the manager keeps each base session's resolved [`ConnectParams`] (secrets
    /// included, redacted in `Debug`) in memory for as long as the base is open, and drops them
    /// when it closes. They are never serialized or logged.
    pub async fn open_child(
        &self,
        base: SessionId,
        database: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<SessionHandle> {
        check_cancelled(&cancel)?;
        let base_handle = self
            .get(base)
            .ok_or_else(|| Error::not_found(format!("no open session {base}")))?;
        if base_handle.summary().parent.is_some() {
            return Err(Error::invalid_input(
                "a session can only be opened from a base session, not from another child",
            ));
        }
        let mut params = self
            .bases
            .lock()
            .map_err(|_| Error::internal("session parameter table lock poisoned"))?
            .get(&base)
            .cloned()
            .ok_or_else(|| {
                Error::invalid_input(format!(
                    "session {base} was not opened by the connection manager; it has no \
                     parameters to open another session with"
                ))
            })?;
        if let Some(db) = database {
            params.database = Some(db.to_owned());
        }
        let base_summary = base_handle.summary();
        let started = Instant::now();
        // The tunnel is already open: only the driver connect is bounded.
        let deadline = params
            .connect_timeout
            .map(|limit| limit.saturating_add(CONNECT_GRACE));
        let connect = self.drivers.connect(&params);
        let bounded = async {
            match deadline {
                Some(deadline) => tokio::time::timeout(deadline, connect)
                    .await
                    .unwrap_or_else(|_| {
                        Err(Error::timeout(format!(
                            "connecting to '{}' took longer than {} s",
                            base_summary.profile_name,
                            deadline.as_secs()
                        )))
                    }),
                None => connect.await,
            }
        };
        let connection = until_cancelled(&cancel, bounded).await?;
        let opened_on = params.database.clone();
        drop(params);
        if cancel.is_cancelled() {
            let _ = connection.close().await;
            return Err(Error::Cancelled);
        }
        let summary = SessionSummary {
            id: SessionId::new(),
            profile_id: base_summary.profile_id,
            profile_name: base_summary.profile_name.clone(),
            engine: base_summary.engine,
            info: ConnectionInfo {
                server_version: connection.server_version().clone(),
                tls_in_use: connection.tls_in_use(),
                via_ssh: base_handle.tunnel().is_some(),
                latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            },
            opened_at: Utc::now(),
            database: opened_on,
            parent: Some(base),
        };
        let handle = SessionHandle::new(
            summary,
            Arc::from(connection),
            base_handle.tunnel().cloned(),
        );
        let registered = {
            let mut sessions = self.lock()?;
            // The base may have closed while the child was connecting.
            if sessions.contains_key(&base) {
                sessions.insert(handle.id(), handle.clone());
                true
            } else {
                false
            }
        };
        if !registered {
            let _ = handle.connection().close().await;
            return Err(Error::not_found(format!("session {base} was closed")));
        }
        tracing::debug!(session = %handle.id(), base = %base, "child session opened");
        Ok(handle)
    }

    /// `false` when a child of the open base session `base` would not see the base's data:
    /// an in-memory database private to one connection (`:memory:`, an empty file name, or a
    /// `file:` URI with `mode=memory` or `file::memory:`, without `cache=shared`), whatever
    /// the engine. Such a
    /// base must be used directly instead of through children. `true` for anything else,
    /// including unknown sessions (opening the child then reports them).
    pub(crate) fn children_share_data(&self, base: SessionId) -> bool {
        let Ok(bases) = self.bases.lock() else {
            return true;
        };
        let Some(file) = bases.get(&base).and_then(|p| p.file.as_ref()) else {
            return true;
        };
        let file = file.to_string_lossy();
        let file = file.trim();
        let lower = file.to_ascii_lowercase();
        let private_memory = file.is_empty()
            || file == ":memory:"
            || (lower.starts_with("file:")
                && (lower.contains("mode=memory") || lower.starts_with("file::memory:"))
                && !lower.contains("cache=shared"));
        !private_memory
    }

    /// Unregisters and closes the session: the driver connection is closed, then the manager's
    /// tunnel reference is dropped (the tunnel closes when no other session shares it). Clones
    /// of the handle held elsewhere see a closed connection. Unknown ids are `NotFound`.
    ///
    /// Closing a base session closes its child sessions first.
    pub async fn close(&self, id: SessionId) -> Result<()> {
        let (handle, children) = {
            let mut sessions = self.lock()?;
            let handle = sessions
                .remove(&id)
                .ok_or_else(|| Error::not_found(format!("no open session {id}")))?;
            let child_ids: Vec<SessionId> = sessions
                .values()
                .filter(|s| s.summary().parent == Some(id))
                .map(SessionHandle::id)
                .collect();
            let children: Vec<SessionHandle> = child_ids
                .iter()
                .filter_map(|c| sessions.remove(c))
                .collect();
            (handle, children)
        };
        if let Ok(mut bases) = self.bases.lock() {
            bases.remove(&id);
        }
        for child in children {
            if let Err(e) = child.connection().close().await {
                tracing::debug!(session = %child.id(), error = %e, "closing child session failed");
            }
        }
        handle.connection().close().await
    }

    /// Closes every open session; returns the first error after trying all of them.
    pub async fn close_all(&self) -> Result<()> {
        // Bases first: closing one also closes its children.
        let mut ids: Vec<(bool, SessionId)> = self
            .lock()?
            .values()
            .map(|s| (s.summary().parent.is_some(), s.id()))
            .collect();
        ids.sort();
        let mut first_error = None;
        for (_, id) in ids {
            match self.close(id).await {
                Ok(()) => {}
                Err(e) if e.category() == oxbase_types::ErrorCategory::NotFound => {}
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// The open session `id`, if any.
    pub fn get(&self, id: SessionId) -> Option<SessionHandle> {
        self.lock().ok()?.get(&id).cloned()
    }

    /// Every open base session, oldest first (child sessions are not listed).
    pub fn list_open(&self) -> Vec<SessionSummary> {
        let Ok(sessions) = self.lock() else {
            return Vec::new();
        };
        let mut all: Vec<_> = sessions
            .values()
            .filter(|s| s.summary().parent.is_none())
            .map(|s| s.summary().clone())
            .collect();
        all.sort_by_key(|s| (s.opened_at, s.id));
        all
    }

    /// Registers an opened session (used by `open`).
    pub fn register(&self, session: SessionHandle) -> Result<()> {
        self.lock()?.insert(session.id(), session);
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<SessionId, SessionHandle>>> {
        self.sessions
            .lock()
            .map_err(|_| Error::internal("session table lock poisoned"))
    }
}

/// The secrets of one connect attempt, already placed into runtime option types.
struct ResolvedSecrets {
    password: Option<Secret>,
    tls: TlsOptions,
    ssh: Option<SshOptions>,
}

/// Step 3 without the tunnel: driver parameters for `profile` (`dial_addr` and `ssh` unset).
fn build_params(profile: &ConnectionProfile, secrets: &ResolvedSecrets) -> Result<ConnectParams> {
    let mut params = ConnectParams::new(profile.engine);
    match &profile.address {
        ProfileAddress::Network { host, port } => {
            params.host = Some(host.trim().to_owned());
            params.port = *port;
        }
        ProfileAddress::File { path } => params.file = Some(path.clone()),
    }
    params.user.clone_from(&profile.user);
    params.password.clone_from(&secrets.password);
    params.database.clone_from(&profile.database);
    params.tls = secrets.tls.clone();
    params.connect_timeout = profile.connect_timeout_secs.map(secs);
    params.application_name = Some(APPLICATION_NAME.to_owned());
    params.options.clone_from(&profile.options);
    if params.engine.is_networked() && params.server_endpoint().is_none() {
        return Err(Error::invalid_input(format!(
            "connection '{}' has no port and {} has no default port",
            profile.name,
            profile.engine.display_name()
        )));
    }
    Ok(params)
}

/// The caller supplied the SSH password or key passphrase for this connect, so it may differ
/// from what a pooled tunnel authenticated with.
fn uses_provided_ssh_secret(ssh: &SshSettings, provided: &ProvidedSecrets) -> bool {
    match &ssh.auth {
        SshAuthSettings::Password { .. } => provided.ssh_password.is_some(),
        SshAuthSettings::PrivateKey { .. } => provided.ssh_key_passphrase.is_some(),
        SshAuthSettings::Agent => false,
    }
}

fn pem_source(pem: &PemRef) -> PemSource {
    match pem {
        PemRef::File { path } => PemSource::File(path.clone()),
        PemRef::Inline { pem } => PemSource::Pem(pem.clone()),
    }
}

fn secs(s: u32) -> Duration {
    Duration::from_secs(u64::from(s))
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// Runs `fut` unless `cancel` fires first (then the future is dropped and the result is
/// `Error::Cancelled`).
async fn until_cancelled<T>(
    cancel: &CancellationToken,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Error::Cancelled),
        r = fut => r,
    }
}

impl fmt::Debug for ConnectionManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionManager")
            .field("drivers", &self.drivers)
            .field("secrets", &self.secrets)
            .field("open_tunnels", &self.open_tunnel_count())
            .field("open_sessions", &self.list_open().len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod manager_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_driver::mock::MockDriver;
    use oxbase_net::ssh::RusshTunnelProvider;
    use oxbase_store::MemorySecretStore;

    #[tokio::test]
    async fn register_list_and_close() {
        let mut drivers = DriverRegistry::new();
        drivers.register(Arc::new(MockDriver::new(Engine::Postgres)));
        let mgr = ConnectionManager::new(
            drivers.clone(),
            Arc::new(MemorySecretStore::new()),
            Arc::new(RusshTunnelProvider::new()),
        );
        assert_eq!(mgr.list_open(), Vec::<SessionSummary>::new());

        let conn: Arc<dyn Connection> = Arc::from(
            drivers
                .connect(&ConnectParams::network(Engine::Postgres, "mock"))
                .await
                .unwrap(),
        );
        let summary = SessionSummary {
            id: SessionId::new(),
            profile_id: Uuid::nil(),
            profile_name: "mock".into(),
            engine: Engine::Postgres,
            info: ConnectionInfo {
                server_version: conn.server_version().clone(),
                tls_in_use: conn.tls_in_use(),
                via_ssh: false,
                latency_ms: 1,
            },
            opened_at: Utc::now(),
            database: None,
            parent: None,
        };
        let handle = SessionHandle::new(summary.clone(), conn, None);
        mgr.register(handle.clone()).unwrap();
        assert_eq!(mgr.list_open(), vec![summary.clone()]);
        assert!(mgr.get(summary.id).is_some());
        assert!(handle.tunnel().is_none());

        mgr.close(summary.id).await.unwrap();
        assert_eq!(mgr.list_open(), Vec::<SessionSummary>::new());
        let err = mgr.close(summary.id).await.unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::NotFound);
        mgr.close_all().await.unwrap();
        let json = serde_json::to_value(&summary.info).unwrap();
        assert_eq!(json["via_ssh"], false);
        // M2 fields are omitted when unset, so older readers see the M1 shape.
        let json = serde_json::to_value(&summary).unwrap();
        assert!(json.get("database").is_none() && json.get("parent").is_none());
    }

    #[tokio::test]
    async fn exclusive_serialises_users_of_a_session() {
        use oxbase_driver::Driver;
        let conn: Arc<dyn Connection> = Arc::from(
            MockDriver::new(Engine::Postgres)
                .connect(&ConnectParams::network(Engine::Postgres, "mock"))
                .await
                .unwrap(),
        );
        let summary = SessionSummary {
            id: SessionId::new(),
            profile_id: Uuid::nil(),
            profile_name: "mock".into(),
            engine: Engine::Postgres,
            info: ConnectionInfo {
                server_version: conn.server_version().clone(),
                tls_in_use: false,
                via_ssh: false,
                latency_ms: 1,
            },
            opened_at: Utc::now(),
            database: Some("db".into()),
            parent: None,
        };
        let handle = SessionHandle::new(summary, conn, None);
        assert_eq!(handle.current_database(), None);
        let guard = handle.exclusive().await;
        let other = handle.clone();
        let waiter = tokio::spawn(async move {
            let _g = other.exclusive().await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished(), "second user waits for the first");
        drop(guard);
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("second user proceeds")
            .unwrap();
    }
}
