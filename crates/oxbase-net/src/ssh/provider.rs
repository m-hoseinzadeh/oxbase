//! [`RusshTunnelProvider`]: the production [`TunnelProvider`], built on `russh` (ring crypto).
//!
//! # Structure
//!
//! - One **SSH session** per (SSH endpoint, SSH user, host-key policy, credential), shared by
//!   every tunnel that goes through it, whatever the target. The credential is compared through
//!   a keyed hash ([`super::scope`]), so a request with another password or key authenticates
//!   for real instead of riding on a session opened with the right one. Sessions are reference counted: each tunnel holds
//!   an `Arc` to its session, the provider only a `Weak`, and the session disconnects when its
//!   last tunnel is dropped. A dead session (network loss, keepalive timeout, server disconnect)
//!   is never reused.
//! - One **local listener** per tunnel on `127.0.0.1:0`. Every accepted connection gets its own
//!   `direct-tcpip` channel to the target, so any number of connections (the driver session,
//!   PostgreSQL cancel requests, MySQL `KILL QUERY` side connections) share one tunnel.
//! - Each tunnel's `closed` token is a child of its session's `dead` token: when the session
//!   ends, every tunnel on it reports [`TunnelHandle::is_closed`] and stops accepting.
//!
//! Secrets (passwords, passphrases, inline keys) are only read with [`Secret::expose`] at the
//! point they are handed to russh, and never formatted into errors or logs.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use oxbase_types::{Endpoint, Error, KeySource, Result, Secret, SshAuth, SshOptions};
use russh::client::{self, AuthResult};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{self, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::known_hosts::HostKeyCheck;
use super::scope::{Scope, Scoper};
use super::{TunnelHandle, TunnelKey, TunnelProvider};

/// Deadline for TCP connect + SSH handshake + authentication when
/// [`SshOptions::connect_timeout`] is not set.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// SSH keepalive interval when [`SshOptions::keepalive_interval`] is not set.
pub const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// Unanswered keepalives after which the session is considered dead.
const KEEPALIVE_MAX: usize = 3;
/// Back-off after a failed `accept` (for example out of file descriptors).
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// The production [`TunnelProvider`], built on `russh`.
///
/// Cloning is cheap and clones share the session cache. See the module docs of
/// [`crate::ssh`] for the contract.
#[derive(Clone, Default)]
pub struct RusshTunnelProvider {
    sessions: Arc<Mutex<HashMap<SessionKey, SessionSlot>>>,
    /// Keys the credential part of [`SessionKey`]; shared by clones, like the cache.
    scoper: Arc<Scoper>,
    /// Agent socket (Unix) or named pipe (Windows) for [`SshAuth::Agent`]. `None`: the
    /// platform default (`SSH_AUTH_SOCK`; on Windows the OpenSSH agent pipe, then Pageant).
    agent_path: Option<PathBuf>,
}

impl RusshTunnelProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Uses this agent socket (Unix) or named pipe (Windows) for [`SshAuth::Agent`] instead of
    /// the platform default.
    #[must_use]
    pub fn with_agent_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.agent_path = Some(path.into());
        self
    }

    /// Number of SSH sessions currently alive (shared by all tunnels on them).
    pub fn session_count(&self) -> usize {
        let Ok(mut sessions) = self.sessions.lock() else {
            return 0;
        };
        let mut alive = 0;
        sessions.retain(|_, slot| match slot.try_lock() {
            Ok(weak) => {
                let live = weak.upgrade().is_some_and(|s| !s.is_dead());
                alive += usize::from(live);
                live
            }
            Err(_) => true,
        });
        alive
    }

    fn slot(&self, key: &SessionKey) -> Result<SessionSlot> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| Error::internal("SSH session cache lock poisoned"))?;
        // Forget sessions that are gone (no `open` holds or is about to lock their slot).
        sessions.retain(|_, slot| {
            Arc::strong_count(slot) > 1
                || !matches!(slot.try_lock(), Ok(w) if w.strong_count() == 0)
        });
        Ok(sessions.entry(key.clone()).or_default().clone())
    }

    /// The live session for `ssh`, connecting and authenticating a new one if needed.
    async fn session(
        &self,
        ssh: &SshOptions,
        cancel: &CancellationToken,
    ) -> Result<Arc<SshSession>> {
        let slot = self.slot(&SessionKey::new(ssh, &self.scoper))?;
        let mut current = tokio::select! {
            guard = slot.lock() => guard,
            () = cancel.cancelled() => return Err(Error::Cancelled),
        };
        if let Some(live) = current.upgrade().filter(|s| !s.is_dead()) {
            debug!(ssh = %live.endpoint, user = %ssh.user, "reusing SSH session");
            return Ok(live);
        }
        let deadline = ssh
            .connect_timeout
            .filter(|d| !d.is_zero())
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
        let session = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Error::Cancelled),
            r = tokio::time::timeout(deadline, connect(ssh, self.agent_path.as_deref())) => match r {
                Ok(r) => Arc::new(r?),
                Err(_) => {
                    return Err(Error::timeout(format!(
                        "SSH connection to {} did not complete within {} s",
                        ssh.endpoint(),
                        deadline.as_secs_f32()
                    )));
                }
            },
        };
        *current = Arc::downgrade(&session);
        Ok(session)
    }
}

impl fmt::Debug for RusshTunnelProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RusshTunnelProvider")
            .field("sessions", &self.session_count())
            .field("agent_path", &self.agent_path)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl TunnelProvider for RusshTunnelProvider {
    async fn open(
        &self,
        ssh: &SshOptions,
        target: &Endpoint,
        cancel: CancellationToken,
    ) -> Result<TunnelHandle> {
        ssh.validate()?;
        if target.host.trim().is_empty() || target.port == 0 {
            return Err(Error::invalid_input(
                "SSH tunnel target needs a host and a non-zero port",
            ));
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let session = self.session(ssh, &cancel).await?;
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        // Loopback only: the forwarded port must never be reachable from other machines.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let local_addr = listener.local_addr()?;
        let closed = session.dead.child_token();
        let accept = tokio::spawn(accept_loop(
            listener,
            Arc::downgrade(&session),
            target.clone(),
            closed.clone(),
        ));
        debug!(
            ssh = %session.endpoint,
            target = %target,
            local = %local_addr,
            "SSH tunnel listening"
        );
        Ok(TunnelHandle::new(
            local_addr,
            TunnelKey::new(ssh, target),
            closed,
            TunnelResources {
                _accept: AbortOnDrop(accept),
                _session: session,
            },
        ))
    }
}

/// Sessions are shared by SSH endpoint and user **and** [`Scope`] (host-key policy and
/// credential), so a stricter policy is never satisfied by a session verified under a laxer
/// one, and a wrong credential never by a session authenticated with the right one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionKey {
    ssh: Endpoint,
    user: String,
    scope: Scope,
}

impl SessionKey {
    fn new(ssh: &SshOptions, scoper: &Scoper) -> Self {
        Self {
            ssh: ssh.endpoint(),
            user: ssh.user.clone(),
            scope: scoper.scope(ssh),
        }
    }
}

type SessionSlot = Arc<tokio::sync::Mutex<Weak<SshSession>>>;

/// Kept alive by the [`TunnelHandle`]; dropped with it. The accept loop is aborted first, then
/// the session reference is released (disconnecting if it was the last one).
struct TunnelResources {
    _accept: AbortOnDrop,
    _session: Arc<SshSession>,
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// An authenticated SSH session.
struct SshSession {
    endpoint: Endpoint,
    /// `None` only during `Drop`.
    handle: Option<client::Handle<ClientHandler>>,
    /// Cancelled when the russh session task ends (see [`ClientHandler`]).
    dead: CancellationToken,
}

impl SshSession {
    fn is_dead(&self) -> bool {
        self.dead.is_cancelled() || self.handle.as_ref().is_none_or(client::Handle::is_closed)
    }

    async fn open_direct_tcpip(
        &self,
        target: &Endpoint,
        origin: SocketAddr,
    ) -> std::result::Result<russh::Channel<client::Msg>, russh::Error> {
        let handle = self.handle.as_ref().ok_or(russh::Error::Disconnect)?;
        // The SSH server resolves the target, so in-network names work; IPv6 without brackets.
        let host = target
            .ip()
            .map_or_else(|| target.host.clone(), |ip| ip.to_string());
        handle
            .channel_open_direct_tcpip(
                host,
                u32::from(target.port),
                origin.ip().to_string(),
                u32::from(origin.port()),
            )
            .await
    }
}

impl Drop for SshSession {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        // Forwarded channels hold senders into the session loop, so dropping the handle alone
        // does not end it: send an explicit disconnect.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let endpoint = self.endpoint.clone();
            rt.spawn(async move {
                let _ = handle
                    .disconnect(russh::Disconnect::ByApplication, "", "en")
                    .await;
                debug!(ssh = %endpoint, "SSH session closed");
            });
        }
    }
}

/// russh callbacks for one session: host-key verification and session-end detection.
struct ClientHandler {
    check: HostKeyCheck,
    /// The host-key failure, read by `connect` to replace russh's generic "unknown key" error.
    failure: Arc<Mutex<Option<Error>>>,
    dead: CancellationToken,
}

impl ClientHandler {
    fn record(&self, error: Error) {
        if let Ok(mut slot) = self.failure.lock() {
            *slot = Some(error);
        }
    }
}

impl Drop for ClientHandler {
    /// The handler lives inside the russh session task, so this runs when the session ends for
    /// any reason.
    fn drop(&mut self) {
        self.dead.cancel();
    }
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let key = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => {
                // Not negotiated (no certificate host-key algorithms are offered); refuse.
                let key = keys::PublicKey::from(cert.public_key().clone());
                self.record(Error::connection(format!(
                    "SSH host key for {} is a certificate, which is not supported (key {})",
                    self.check_server(),
                    super::known_hosts::fingerprint(&key),
                )));
                return Ok(false);
            }
        };
        let check = self.check.clone();
        match tokio::task::spawn_blocking(move || check.verify(&key)).await {
            Ok(Ok(())) => Ok(true),
            Ok(Err(e)) => {
                self.record(e);
                Ok(false)
            }
            Err(e) => {
                self.record(Error::internal(format!("SSH host key check failed: {e}")));
                Ok(false)
            }
        }
    }

    fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> impl Future<Output = std::result::Result<(), Self::Error>> + Send {
        self.dead.cancel();
        let result = match reason {
            client::DisconnectReason::ReceivedDisconnect(info) => {
                debug!(ssh = %self.check_server(), reason = ?info, "SSH server disconnected");
                Ok(())
            }
            client::DisconnectReason::Error(e) => Err(e),
        };
        std::future::ready(result)
    }
}

impl ClientHandler {
    fn check_server(&self) -> String {
        self.check.server().to_string()
    }
}

/// TCP connect, handshake, host-key check and authentication. Runs under the caller's deadline.
async fn connect(ssh: &SshOptions, agent_path: Option<&Path>) -> Result<SshSession> {
    let endpoint = ssh.endpoint();
    let check = HostKeyCheck::new(&ssh.known_hosts, &ssh.host, ssh.port)?;

    let mut config = client::Config {
        keepalive_interval: Some(ssh.keepalive_interval.unwrap_or(DEFAULT_KEEPALIVE_INTERVAL))
            .filter(|d| !d.is_zero()),
        keepalive_max: KEEPALIVE_MAX,
        nodelay: true,
        ..client::Config::default()
    };
    let preferred = {
        let check = check.clone();
        let defaults = config.preferred.key.to_vec();
        tokio::task::spawn_blocking(move || check.preferred_algorithms(&defaults))
            .await
            .ok()
            .flatten()
    };
    if let Some(algorithms) = preferred {
        config.preferred.key = Cow::Owned(algorithms);
    }

    let tcp = match endpoint.socket_addr() {
        Some(addr) => TcpStream::connect(addr).await,
        None => TcpStream::connect((ssh.host.trim(), ssh.port)).await,
    }
    .map_err(|e| Error::connection(format!("could not reach SSH server {endpoint}: {e}")))?;
    let _ = tcp.set_nodelay(true);

    let failure = Arc::new(Mutex::new(None));
    let dead = CancellationToken::new();
    let handler = ClientHandler {
        check,
        failure: failure.clone(),
        dead: dead.clone(),
    };
    let take_failure = || failure.lock().ok().and_then(|mut f| f.take());
    let handle = match client::connect_stream(Arc::new(config), tcp, handler).await {
        Ok(h) => h,
        Err(e) => {
            return Err(take_failure()
                .unwrap_or_else(|| russh_error(&endpoint, "SSH handshake failed", &e)));
        }
    };
    // From here on, dropping `session` (any error below) disconnects.
    let mut session = SshSession {
        endpoint: endpoint.clone(),
        handle: Some(handle),
        dead,
    };
    let handle = session
        .handle
        .as_mut()
        .ok_or_else(|| Error::internal("SSH session handle missing"))?;
    match authenticate(handle, ssh, agent_path).await {
        Ok(()) => {}
        Err(e) => return Err(take_failure().unwrap_or(e)),
    }
    debug!(ssh = %endpoint, user = %ssh.user, "SSH session established");
    Ok(session)
}

/// How an authentication attempt ended (when the server answered at all).
enum Outcome {
    Success,
    Rejected,
    /// Accepted, but the server wants another method too (multi-factor).
    Partial,
}

impl From<AuthResult> for Outcome {
    fn from(r: AuthResult) -> Self {
        match r {
            AuthResult::Success => Self::Success,
            AuthResult::Failure {
                partial_success: true,
                ..
            } => Self::Partial,
            AuthResult::Failure { .. } => Self::Rejected,
        }
    }
}

async fn authenticate(
    handle: &mut client::Handle<ClientHandler>,
    ssh: &SshOptions,
    agent_path: Option<&Path>,
) -> Result<()> {
    let endpoint = ssh.endpoint();
    let user = ssh.user.as_str();
    let protocol = |e: russh::Error| russh_error(&endpoint, "SSH authentication failed", &e);
    let (method, outcome) = match &ssh.auth {
        SshAuth::Password(password) => {
            let r = handle
                .authenticate_password(user, password.expose())
                .await
                .map_err(protocol)?;
            ("password", Outcome::from(r))
        }
        SshAuth::PrivateKey { key, passphrase } => {
            let private = load_private_key(key, passphrase.as_ref()).await?;
            let hash = rsa_hash(handle, &private.algorithm()).await;
            let r = handle
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(private), hash))
                .await
                .map_err(protocol)?;
            ("public key", Outcome::from(r))
        }
        SshAuth::Agent => (
            "SSH agent",
            authenticate_agent(handle, ssh, agent_path).await?,
        ),
    };
    match outcome {
        Outcome::Success => Ok(()),
        Outcome::Rejected => Err(Error::auth(format!(
            "SSH server {endpoint} rejected {method} authentication for user {user}"
        ))),
        Outcome::Partial => Err(Error::auth(format!(
            "SSH server {endpoint} accepted {method} authentication for user {user} but requires \
             a further method (multi-factor), which is not supported"
        ))),
    }
}

/// The RSA signature hash to use with an RSA key (the best one the server advertises), or
/// `None` for other key types.
async fn rsa_hash(
    handle: &client::Handle<ClientHandler>,
    algorithm: &keys::Algorithm,
) -> Option<HashAlg> {
    if !algorithm.clone().is_rsa() {
        return None;
    }
    handle
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten()
}

async fn authenticate_agent(
    handle: &mut client::Handle<ClientHandler>,
    ssh: &SshOptions,
    agent_path: Option<&Path>,
) -> Result<Outcome> {
    let mut agent = connect_agent(agent_path).await?;
    let identities = agent.request_identities().await.map_err(|e| {
        Error::connection(format!(
            "could not list the keys held by the SSH agent: {e}"
        ))
    })?;
    if identities.is_empty() {
        return Err(Error::auth(format!(
            "the SSH agent holds no keys to authenticate user {} at {}",
            ssh.user,
            ssh.endpoint()
        )));
    }
    let mut outcome = Outcome::Rejected;
    for identity in identities {
        let hash = rsa_hash(handle, &identity.public_key().algorithm()).await;
        let attempt = match &identity {
            AgentIdentity::PublicKey { key, .. } => {
                handle
                    .authenticate_publickey_with(ssh.user.as_str(), key.clone(), hash, &mut agent)
                    .await
            }
            AgentIdentity::Certificate { certificate, .. } => {
                handle
                    .authenticate_certificate_with(
                        ssh.user.as_str(),
                        certificate.clone(),
                        hash,
                        &mut agent,
                    )
                    .await
            }
        };
        match attempt.map(Outcome::from) {
            Ok(Outcome::Success) => return Ok(Outcome::Success),
            Ok(Outcome::Partial) => outcome = Outcome::Partial,
            Ok(Outcome::Rejected) => {}
            // The agent refused or failed to sign with this key; try the next one.
            Err(e) => debug!(ssh = %ssh.endpoint(), error = %e, "SSH agent could not sign"),
        }
        if handle.is_closed() {
            return Err(Error::connection(format!(
                "SSH server {} closed the connection during authentication",
                ssh.endpoint()
            )));
        }
    }
    Ok(outcome)
}

type DynAgent = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

#[cfg(unix)]
async fn connect_agent(path: Option<&Path>) -> Result<DynAgent> {
    let path = match path {
        Some(p) => p.to_owned(),
        None => std::env::var_os("SSH_AUTH_SOCK")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                Error::connection(
                    "could not connect to the SSH agent: SSH_AUTH_SOCK is not set (no agent is \
                     running in this session)",
                )
            })?,
    };
    let agent = AgentClient::connect_uds(&path).await.map_err(|e| {
        Error::connection(format!(
            "could not connect to the SSH agent at {}: {e}",
            path.display()
        ))
    })?;
    Ok(agent.dynamic())
}

#[cfg(windows)]
async fn connect_agent(path: Option<&Path>) -> Result<DynAgent> {
    /// The pipe of the Windows OpenSSH agent service (`ssh-agent`).
    const OPENSSH_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
    // An explicitly configured pipe is used as is: failing to reach it is an error.
    if let Some(pipe) = path {
        let agent = AgentClient::connect_named_pipe(pipe).await.map_err(|e| {
            Error::connection(format!(
                "could not connect to the SSH agent pipe {}: {e}",
                pipe.display()
            ))
        })?;
        return Ok(agent.dynamic());
    }
    // SSH_AUTH_SOCK only when it names a named pipe: Git for Windows (MSYS) sets it to a
    // Unix-style path that native code cannot open. If it fails, fall through to the defaults.
    if let Some(pipe) =
        std::env::var_os("SSH_AUTH_SOCK").filter(|v| v.to_string_lossy().starts_with(r"\\.\pipe\"))
    {
        match AgentClient::connect_named_pipe(&pipe).await {
            Ok(agent) => return Ok(agent.dynamic()),
            Err(e) => debug!(
                pipe = %pipe.to_string_lossy(),
                error = %e,
                "SSH_AUTH_SOCK agent pipe not reachable; trying the default agents"
            ),
        }
    }
    if let Ok(agent) = AgentClient::connect_named_pipe(OPENSSH_AGENT_PIPE).await {
        return Ok(agent.dynamic());
    }
    let agent = AgentClient::connect_pageant().await.map_err(|e| {
        Error::connection(format!(
            "no SSH agent found: the OpenSSH agent service is not running and Pageant is not \
             available ({e})"
        ))
    })?;
    Ok(agent.dynamic())
}

#[cfg(not(any(unix, windows)))]
async fn connect_agent(_path: Option<&Path>) -> Result<DynAgent> {
    Err(Error::unsupported(
        "SSH agent authentication is not supported on this platform",
    ))
}

/// Loads an OpenSSH, PEM (PKCS#1, optionally encrypted), PKCS#8 or PuTTY private key.
///
/// Decoding (bcrypt / PBKDF2 for encrypted keys) is CPU-bound, so it runs on the blocking pool.
/// Errors never include key text or parser details that could echo it.
pub(super) async fn load_private_key(
    source: &KeySource,
    passphrase: Option<&Secret>,
) -> Result<PrivateKey> {
    let text = match source {
        KeySource::File(path) => {
            let text = tokio::fs::read_to_string(path)
                .await
                .map_err(|e| Error::Io {
                    kind: e.kind().to_string(),
                    message: format!(
                        "could not read SSH private key file {}: {e}",
                        path.display()
                    ),
                })?;
            Secret::new(text)
        }
        KeySource::Pem(pem) => pem.clone(),
    };
    let passphrase = passphrase.filter(|p| !p.is_empty()).cloned();
    let decoded = tokio::task::spawn_blocking(move || {
        // Tolerate indentation and CRLF from copy-pasted keys.
        let normalized = Secret::new(
            text.expose()
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        if let Some(unsupported) = unsupported_key_format(normalized.expose()) {
            return Err(unsupported);
        }
        let result =
            keys::decode_secret_key(normalized.expose(), passphrase.as_ref().map(Secret::expose));
        result.map_err(|e| classify_key_error(&e, normalized.expose(), passphrase.is_some()))
    })
    .await;
    match decoded {
        Ok(result) => result,
        // A panic inside the key parser (unsupported cipher): report it as a bad key.
        Err(_) => Err(Error::invalid_input(
            "could not load the SSH private key: unsupported key format or cipher",
        )),
    }
}

/// Legacy PEM encryption (`Proc-Type: 4,ENCRYPTED`) is decoded only for PKCS#1 RSA keys with
/// `DEK-Info: AES-128-CBC`. Anything else would fail later as a misleading "wrong passphrase",
/// so it is reported up front with how to convert the key. `text` is normalized (trimmed lines).
fn unsupported_key_format(text: &str) -> Option<Error> {
    if !text
        .lines()
        .any(|l| l.starts_with("Proc-Type:") && l.contains("ENCRYPTED"))
    {
        return None;
    }
    let cipher = text
        .lines()
        .find_map(|l| l.strip_prefix("DEK-Info:"))
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .unwrap_or_default();
    let rsa = text.lines().any(|l| l == "-----BEGIN RSA PRIVATE KEY-----");
    if rsa && cipher.eq_ignore_ascii_case("AES-128-CBC") {
        return None;
    }
    // The cipher name is a short token from the header, never key material; cap it anyway.
    let cipher: String = cipher
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(32)
        .collect();
    let what = if rsa {
        format!(
            "PEM encryption {} is not supported (only AES-128-CBC)",
            if cipher.is_empty() {
                "?"
            } else {
                cipher.as_str()
            }
        )
    } else {
        String::from("legacy PEM encryption is supported only for RSA keys")
    };
    Some(Error::invalid_input(format!(
        "could not load the SSH private key: {what}; convert the key to the OpenSSH format with \
         `ssh-keygen -p -f <key file>` (it keeps the passphrase you enter)"
    )))
}

fn classify_key_error(error: &keys::Error, text: &str, had_passphrase: bool) -> Error {
    let encrypted = matches!(
        error,
        keys::Error::KeyIsEncrypted | keys::Error::SshKey(keys::ssh_key::Error::Encrypted)
    ) || text.contains("ENCRYPTED")
        || putty_encrypted(text)
        || PrivateKey::from_openssh(text).is_ok_and(|k| k.is_encrypted());
    match (encrypted, had_passphrase) {
        (true, false) => Error::auth("the SSH private key is encrypted and needs a passphrase"),
        (true, true) => {
            Error::auth("could not decrypt the SSH private key: the passphrase may be wrong")
        }
        (false, _) => Error::invalid_input(
            "could not parse the SSH private key (supported: OpenSSH, PEM/PKCS#1 RSA, PKCS#8 and \
             PuTTY formats)",
        ),
    }
}

/// Whether `text` is a PuTTY (`.ppk`) key with an `Encryption:` header other than `none`.
fn putty_encrypted(text: &str) -> bool {
    text.trim_start().starts_with("PuTTY-User-Key-File-")
        && text
            .lines()
            .find_map(|l| l.strip_prefix("Encryption:"))
            .is_some_and(|v| !v.trim().eq_ignore_ascii_case("none"))
}

/// Maps a russh protocol error. Its text describes protocol state, never credentials.
fn russh_error(endpoint: &Endpoint, context: &str, error: &russh::Error) -> Error {
    match error {
        russh::Error::ConnectionTimeout
        | russh::Error::KeepaliveTimeout
        | russh::Error::InactivityTimeout
        | russh::Error::Elapsed(_) => Error::timeout(format!("{context} with {endpoint}: {error}")),
        russh::Error::IO(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            Error::timeout(format!("{context} with {endpoint}: {error}"))
        }
        _ => Error::connection(format!("{context} with {endpoint}: {error}")),
    }
}

/// Accepts local connections until `closed` is cancelled, forwarding each through its own
/// `direct-tcpip` channel. Dropping (or aborting) this future aborts every forwarded connection.
async fn accept_loop(
    listener: TcpListener,
    session: Weak<SshSession>,
    target: Endpoint,
    closed: CancellationToken,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            () = closed.cancelled() => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => match accepted {
                Ok((tcp, peer)) => {
                    if session.strong_count() == 0 {
                        break;
                    }
                    connections.spawn(forward(
                        tcp,
                        peer,
                        session.clone(),
                        target.clone(),
                        closed.clone(),
                    ));
                }
                Err(e) => {
                    warn!(target = %target, error = %e, "SSH tunnel could not accept a connection");
                    tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                }
            },
        }
    }
    debug!(target = %target, "SSH tunnel stopped accepting");
}

async fn forward(
    mut tcp: TcpStream,
    peer: SocketAddr,
    session: Weak<SshSession>,
    target: Endpoint,
    closed: CancellationToken,
) {
    let _ = tcp.set_nodelay(true);
    let opened = {
        // Hold the session only while opening: the channel keeps working on its own, and the
        // tunnel handle decides when the session goes away.
        let Some(session) = session.upgrade() else {
            return;
        };
        let endpoint = session.endpoint.clone();
        tokio::select! {
            () = closed.cancelled() => return,
            r = session.open_direct_tcpip(&target, peer) => r.map_err(|e| (endpoint, e)),
        }
    };
    let channel = match opened {
        Ok(channel) => channel,
        Err((ssh, e)) => {
            // The local client sees the connection close, like `ssh -L` does.
            warn!(ssh = %ssh, target = %target, error = %e, "SSH server could not open the forwarded connection");
            return;
        }
    };
    let mut remote = channel.into_stream();
    tokio::select! {
        () = closed.cancelled() => {}
        r = tokio::io::copy_bidirectional(&mut tcp, &mut remote) => {
            if let Err(e) = r {
                debug!(target = %target, error = %e, "forwarded connection ended with an error");
            }
        }
    }
}
