//! SSH tunnels: local TCP port forwarding through an SSH server (`ssh -L` equivalent).
//!
//! The types, the [`TunnelProvider`] trait, [`TunnelHandle`] and [`TunnelPool`] are the M1
//! contract. [`RusshTunnelProvider`] implements it with `russh` (ring crypto): password, private
//! key (OpenSSH, PEM/PKCS#1, PKCS#8, PuTTY; optionally encrypted) and agent authentication;
//! host keys checked against `known_hosts` files (plain, wildcard and hashed entries,
//! `@revoked`) or a pinned fingerprint; one shared SSH session per (SSH endpoint, user,
//! host-key policy, credential), reference counted by the tunnels that use it.
//!
//! # Sharing
//!
//! A session or tunnel is reused only for a request with the same SSH endpoint and user **and**
//! the same host-key policy and credential (compared through keyed hashes, never stored; see
//! `scope.rs`). So a stricter policy or another pin is always checked against the server, and a
//! wrong password or key always fails, even while a correctly authenticated tunnel to the same
//! bastion is open.
//!
//! # Contract for [`TunnelProvider::open`]
//!
//! 1. Validate `ssh` ([`SshOptions::validate`]), connect TCP to `ssh.host:ssh.port`, run the SSH
//!    handshake and check the host key per [`KnownHostsPolicy`]:
//!    - `Strict`: the key must already be in the file; unknown or changed fails;
//!    - `AcceptNew`: unknown hosts are appended to the file; a **changed** key fails;
//!    - `Fingerprint`: the key's `SHA256:` fingerprint must equal the given one.
//!
//!    Host-key failures are `Error::Connection` whose message starts with `SSH host key` and
//!    includes the presented `SHA256:` fingerprint (so the UI can offer to trust it).
//! 2. Authenticate with [`SshAuth`]: password, private key (file or inline, optional passphrase)
//!    or the agent. Rejection is `Error::Auth`. No secret ever appears in errors or logs.
//! 3. Bind a listener on `127.0.0.1:0` (loopback only, never a wildcard address) and return a
//!    [`TunnelHandle`] whose [`TunnelHandle::local_addr`] is that listener's address.
//! 4. While the handle lives, accept **any number** of local connections and forward each one
//!    through its own `direct-tcpip` channel to `target` (resolved by the SSH server, so in-network
//!    names like `postgres:5432` work). This is what lets a PostgreSQL cancel request or a MySQL
//!    `KILL QUERY` side connection reach the same server as the session.
//! 5. `open` does not probe `target`: an unreachable target shows up as a connection error on
//!    the first forwarded connection (the driver's connect).
//! 6. `cancel` aborts a pending `open` with `Error::Cancelled`; it does not close a tunnel that
//!    was already returned. Connect/handshake deadline: [`SshOptions::connect_timeout`]
//!    (`Error::Timeout`); keepalives per [`SshOptions::keepalive_interval`].
//! 7. When the SSH session dies, the provider cancels the handle's `closed` token
//!    ([`TunnelHandle::is_closed`] becomes true) and stops accepting.
//!
//! # Lifetime
//!
//! Dropping the last [`TunnelHandle`] (the pool hands out `Arc`s) stops the listener, closes
//! every forwarded channel and disconnects the SSH session. The connection manager keeps the
//! handle alive for exactly as long as the driver connection that uses it, and closes the driver
//! connection **before** dropping the handle.

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub use oxbase_types::{Endpoint, KeySource, KnownHostsPolicy, Secret, SshAuth, SshOptions};
use oxbase_types::{Error, Result};

mod known_hosts;
mod provider;
#[cfg(test)]
mod provider_tests;
mod scope;

pub use provider::{DEFAULT_CONNECT_TIMEOUT, DEFAULT_KEEPALIVE_INTERVAL, RusshTunnelProvider};

/// Names a tunnel: SSH server, SSH user and target.
///
/// [`TunnelPool`] shares a tunnel between requests with the same key **and** the same host-key
/// policy and credential (see the module docs); the key alone does not decide sharing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TunnelKey {
    /// SSH server endpoint.
    pub ssh: Endpoint,
    /// SSH user name.
    pub user: String,
    /// Where the tunnel forwards to, as seen from the SSH server.
    pub target: Endpoint,
}

impl TunnelKey {
    pub fn new(ssh: &SshOptions, target: &Endpoint) -> Self {
        Self {
            ssh: ssh.endpoint(),
            user: ssh.user.clone(),
            target: target.clone(),
        }
    }
}

impl fmt::Display for TunnelKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{} -> {}", self.user, self.ssh, self.target)
    }
}

/// An open tunnel. Connect to [`TunnelHandle::local_addr`] to reach the target.
///
/// Dropping the handle closes the tunnel (see the module docs). It is `Send + Sync` and usually
/// shared as `Arc<TunnelHandle>`.
pub struct TunnelHandle {
    local_addr: SocketAddr,
    key: TunnelKey,
    closed: CancellationToken,
    /// Provider resources (SSH session, accept task guard) dropped with the handle.
    _resources: Box<dyn Any + Send + Sync>,
}

impl TunnelHandle {
    /// Used by providers. `closed` is cancelled by the provider when the SSH session ends and by
    /// the handle's `Drop`; the provider's accept loop and channels stop when it is cancelled.
    /// `resources` is dropped together with the handle.
    pub fn new(
        local_addr: SocketAddr,
        key: TunnelKey,
        closed: CancellationToken,
        resources: impl Any + Send + Sync,
    ) -> Self {
        Self {
            local_addr,
            key,
            closed,
            _resources: Box::new(resources),
        }
    }

    /// Loopback address of the local listener (`127.0.0.1:<ephemeral>`). Drivers dial this.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn key(&self) -> &TunnelKey {
        &self.key
    }

    /// `true` once the SSH session has ended or the tunnel was closed.
    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    /// Completes when the tunnel closes (SSH session lost or handle dropped elsewhere).
    pub async fn closed(&self) {
        self.closed.cancelled().await;
    }
}

impl Drop for TunnelHandle {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}

impl fmt::Debug for TunnelHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TunnelHandle")
            .field("local_addr", &self.local_addr)
            .field("key", &self.key)
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

/// Opens SSH tunnels. See the module docs for the full contract.
#[async_trait]
pub trait TunnelProvider: Send + Sync {
    /// Opens a tunnel through `ssh` to `target` (host and port as seen from the SSH server).
    async fn open(
        &self,
        ssh: &SshOptions,
        target: &Endpoint,
        cancel: CancellationToken,
    ) -> Result<TunnelHandle>;
}

type Slot = Arc<tokio::sync::Mutex<Weak<TunnelHandle>>>;

/// Shares tunnels between connections by [`TunnelKey`], host-key policy and credential.
///
/// [`TunnelPool::acquire`] returns the live tunnel for the request or opens a new one. A tunnel
/// is shared only with requests that have the same [`TunnelKey`], the same [`KnownHostsPolicy`]
/// and the same credential, so a request never skips its own host-key check or authentication
/// (see the module docs). The pool holds only weak references: a tunnel closes when the last
/// connection using it drops its `Arc`. Closed tunnels (SSH session lost) are never handed out
/// again. Concurrent `acquire`s for the same request wait for one `open` instead of racing;
/// different ones open in parallel.
pub struct TunnelPool {
    provider: Arc<dyn TunnelProvider>,
    scoper: scope::Scoper,
    slots: Mutex<HashMap<(TunnelKey, scope::Scope), Slot>>,
}

impl TunnelPool {
    pub fn new(provider: Arc<dyn TunnelProvider>) -> Self {
        Self {
            provider,
            scoper: scope::Scoper::new(),
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// The live tunnel for `(ssh, target)`, opened through the provider if needed.
    pub async fn acquire(
        &self,
        ssh: &SshOptions,
        target: &Endpoint,
        cancel: CancellationToken,
    ) -> Result<Arc<TunnelHandle>> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let slot = self.slot((TunnelKey::new(ssh, target), self.scoper.scope(ssh)))?;
        let mut current = tokio::select! {
            guard = slot.lock() => guard,
            () = cancel.cancelled() => return Err(Error::Cancelled),
        };
        if let Some(live) = current.upgrade().filter(|t| !t.is_closed()) {
            return Ok(live);
        }
        let opened = Arc::new(self.provider.open(ssh, target, cancel).await?);
        *current = Arc::downgrade(&opened);
        Ok(opened)
    }

    /// Number of tunnels currently open (alive and not closed). Also forgets dead entries.
    pub fn open_count(&self) -> usize {
        let Ok(mut slots) = self.slots.lock() else {
            return 0;
        };
        let mut open = 0;
        slots.retain(|_, slot| match slot.try_lock() {
            Ok(weak) => {
                let alive = weak.upgrade().is_some_and(|t| !t.is_closed());
                open += usize::from(alive);
                alive
            }
            // An `open` is in progress for this key.
            Err(_) => true,
        });
        open
    }

    fn slot(&self, key: (TunnelKey, scope::Scope)) -> Result<Slot> {
        let mut slots = self
            .slots
            .lock()
            .map_err(|_| Error::internal("tunnel pool lock poisoned"))?;
        // Forget tunnels that are gone (no `acquire` holds or is about to lock their slot).
        slots.retain(|_, slot| {
            Arc::strong_count(slot) > 1
                || !matches!(slot.try_lock(), Ok(w) if w.strong_count() == 0)
        });
        Ok(slots.entry(key).or_default().clone())
    }
}

impl fmt::Debug for TunnelPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TunnelPool")
            .field("open", &self.open_count())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU16, Ordering};

    /// Fake provider: hands out distinct fake local addresses and counts opens.
    #[derive(Default)]
    struct FakeProvider {
        opened: AtomicU16,
    }

    #[async_trait]
    impl TunnelProvider for FakeProvider {
        async fn open(
            &self,
            ssh: &SshOptions,
            target: &Endpoint,
            cancel: CancellationToken,
        ) -> Result<TunnelHandle> {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            ssh.validate()?;
            let n = self.opened.fetch_add(1, Ordering::SeqCst) + 1;
            let addr = SocketAddr::from(([127, 0, 0, 1], 40000 + n));
            Ok(TunnelHandle::new(
                addr,
                TunnelKey::new(ssh, target),
                CancellationToken::new(),
                (),
            ))
        }
    }

    fn ssh(user: &str) -> SshOptions {
        SshOptions::new("bastion", user, SshAuth::Password(Secret::new("pw-xyz")))
    }

    #[tokio::test]
    async fn pool_shares_by_key_and_reopens_after_drop() {
        let provider = Arc::new(FakeProvider::default());
        let pool = TunnelPool::new(provider.clone());
        let pg = Endpoint::new("postgres", 5432);
        let my = Endpoint::new("mysql", 3306);

        let pg_1 = pool
            .acquire(&ssh("ops"), &pg, CancellationToken::new())
            .await
            .unwrap();
        let pg_2 = pool
            .acquire(&ssh("ops"), &pg, CancellationToken::new())
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&pg_1, &pg_2));
        let mysql_t = pool
            .acquire(&ssh("ops"), &my, CancellationToken::new())
            .await
            .unwrap();
        let other_user = pool
            .acquire(&ssh("other"), &pg, CancellationToken::new())
            .await
            .unwrap();
        assert_ne!(pg_1.local_addr(), mysql_t.local_addr());
        assert_ne!(pg_1.local_addr(), other_user.local_addr());
        assert_eq!(provider.opened.load(Ordering::SeqCst), 3);
        assert_eq!(pool.open_count(), 3);

        drop((pg_1, pg_2));
        assert_eq!(pool.open_count(), 2);
        let reopened = pool
            .acquire(&ssh("ops"), &pg, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(provider.opened.load(Ordering::SeqCst), 4);
        assert!(!reopened.is_closed());
        assert_eq!(
            reopened.key().to_string(),
            "ops@bastion:22 -> postgres:5432"
        );
        assert!(!format!("{pool:?} {reopened:?}").contains("pw-xyz"));
    }

    #[tokio::test]
    async fn closed_tunnels_are_not_reused_and_cancel_is_honoured() {
        let provider = Arc::new(FakeProvider::default());
        let pool = TunnelPool::new(provider.clone());
        let pg = Endpoint::new("postgres", 5432);
        let a = pool
            .acquire(&ssh("ops"), &pg, CancellationToken::new())
            .await
            .unwrap();
        // Simulate the SSH session dying: the provider cancels the handle's token.
        a.closed.cancel();
        a.closed().await;
        assert!(a.is_closed());
        let b = pool
            .acquire(&ssh("ops"), &pg, CancellationToken::new())
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&a, &b));

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let err = pool.acquire(&ssh("ops"), &pg, cancelled).await.unwrap_err();
        assert!(err.is_cancelled());
        let err = pool
            .acquire(&ssh(""), &pg, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);
    }

    #[tokio::test]
    async fn pool_never_shares_across_policies_or_credentials() {
        let provider = Arc::new(FakeProvider::default());
        let pool = TunnelPool::new(provider.clone());
        let pg = Endpoint::new("postgres", 5432);
        let tofu = ssh("ops").with_known_hosts(KnownHostsPolicy::AcceptNew { path: None });
        let first = pool
            .acquire(&tofu, &pg, CancellationToken::new())
            .await
            .unwrap();

        // Same key, stricter policy: its own open (so its own host-key check).
        let strict = ssh("ops").with_known_hosts(KnownHostsPolicy::Strict {
            path: Some("/nonexistent/known_hosts".into()),
        });
        let strict_t = pool
            .acquire(&strict, &pg, CancellationToken::new())
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &strict_t));
        assert_eq!(provider.opened.load(Ordering::SeqCst), 2);

        // Same key and policy, another password: its own open (so its own authentication).
        let mut other_pw = tofu.clone();
        other_pw.auth = SshAuth::Password(Secret::new("another-pw"));
        let other_t = pool
            .acquire(&other_pw, &pg, CancellationToken::new())
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &other_t));
        assert_eq!(provider.opened.load(Ordering::SeqCst), 3);

        // The same request again shares.
        let again = pool
            .acquire(&tofu, &pg, CancellationToken::new())
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(provider.opened.load(Ordering::SeqCst), 3);
        assert_eq!(pool.open_count(), 3);
        let text = format!("{pool:?} {first:?}");
        assert!(
            !text.contains("pw-xyz") && !text.contains("another-pw"),
            "{text}"
        );
    }

    /// Refuses every request whose host-key policy is `Strict` (as a provider would with an
    /// empty known_hosts file).
    struct StrictRefuses;

    #[async_trait]
    impl TunnelProvider for StrictRefuses {
        async fn open(
            &self,
            ssh: &SshOptions,
            target: &Endpoint,
            _cancel: CancellationToken,
        ) -> Result<TunnelHandle> {
            if matches!(ssh.known_hosts, KnownHostsPolicy::Strict { .. }) {
                return Err(Error::connection("SSH host key for bastion:22 is not in"));
            }
            Ok(TunnelHandle::new(
                SocketAddr::from(([127, 0, 0, 1], 40000)),
                TunnelKey::new(ssh, target),
                CancellationToken::new(),
                (),
            ))
        }
    }

    #[tokio::test]
    async fn strict_request_fails_while_an_accept_new_tunnel_is_live() {
        let pool = TunnelPool::new(Arc::new(StrictRefuses));
        let pg = Endpoint::new("postgres", 5432);
        let tofu = ssh("ops").with_known_hosts(KnownHostsPolicy::AcceptNew { path: None });
        let _live = pool
            .acquire(&tofu, &pg, CancellationToken::new())
            .await
            .unwrap();
        let strict = ssh("ops").with_known_hosts(KnownHostsPolicy::Strict { path: None });
        let err = pool
            .acquire(&strict, &pg, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::Connection);
    }

    #[test]
    fn handle_drop_closes() {
        let token = CancellationToken::new();
        let h = TunnelHandle::new(
            SocketAddr::from(([127, 0, 0, 1], 1)),
            TunnelKey::new(&ssh("u"), &Endpoint::new("t", 1)),
            token.clone(),
            (),
        );
        assert!(!token.is_cancelled());
        drop(h);
        assert!(token.is_cancelled());
    }
}
