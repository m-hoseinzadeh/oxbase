//! [`RusshTunnelProvider`] against an in-process russh SSH server on loopback.
//!
//! The server accepts one user with a password or any of the test keys in `testdata/`
//! (throwaway keys generated for these tests only), and serves `direct-tcpip` by dialling the
//! requested target. Targets are local echo servers.

use std::future::{Future, ready};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oxbase_types::ErrorCategory;
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{self, Auth, VerifiedPublicKey};
use russh::{Channel, ChannelOpenFailure};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::known_hosts::fingerprint;
use super::provider::load_private_key;
use super::*;

const USER: &str = "tester";
const PASSWORD: &str = "pw-marker-7f3a9c";
const KEY_PASSPHRASE: &str = "test-key-passphrase";

const HOST_A: &str = include_str!("testdata/host_ed25519_a");
const HOST_A_PUB: &str = include_str!("testdata/host_ed25519_a.pub");
const HOST_B_PUB: &str = include_str!("testdata/host_ed25519_b.pub");
const CLIENT_ED25519: &str = include_str!("testdata/client_ed25519");
const CLIENT_ED25519_PUB: &str = include_str!("testdata/client_ed25519.pub");
const CLIENT_ED25519_ENC: &str = include_str!("testdata/client_ed25519_enc");
const CLIENT_ED25519_ENC_PUB: &str = include_str!("testdata/client_ed25519_enc.pub");
const CLIENT_RSA_PEM_ENC: &str = include_str!("testdata/client_rsa_pem_enc");
const CLIENT_RSA_PEM_ENC_PUB: &str = include_str!("testdata/client_rsa_pem_enc.pub");
const CLIENT_ECDSA_PKCS8: &str = include_str!("testdata/client_ecdsa_pkcs8");
const CLIENT_ECDSA_PKCS8_PUB: &str = include_str!("testdata/client_ecdsa_pkcs8.pub");

/// Strings that must never appear in an error message.
const SECRETS: &[&str] = &[
    PASSWORD,
    KEY_PASSPHRASE,
    "b3BlbnNzaC1rZXktdjEA",
    "PRIVATE KEY",
];

fn assert_no_secret(text: &str) {
    for secret in SECRETS {
        assert!(
            !text.contains(secret),
            "secret {secret:?} leaked into: {text}"
        );
    }
}

fn public(text: &str) -> PublicKey {
    PublicKey::from_openssh(text.trim()).unwrap()
}

fn testdata(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/ssh/testdata")
        .join(name)
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Counts live server-side sessions (incremented on connect, decremented when the session's
/// handler is dropped, which happens when the session ends).
struct LiveGuard(Arc<AtomicUsize>);

impl LiveGuard {
    fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter.clone())
    }
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct ServerHandler {
    authorized: Arc<Vec<PublicKey>>,
    _live: LiveGuard,
}

impl server::Handler for ServerHandler {
    type Error = russh::Error;

    fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> impl Future<Output = Result<Auth, Self::Error>> + Send {
        ready(Ok(if user == USER && password == PASSWORD {
            Auth::Accept
        } else {
            Auth::reject()
        }))
    }

    fn auth_publickey(
        &mut self,
        user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> impl Future<Output = Result<Auth, Self::Error>> + Send {
        let known = self
            .authorized
            .iter()
            .any(|k| k.key_data() == key.public_key.key_data());
        ready(Ok(if user == USER && known {
            Auth::Accept
        } else {
            Auth::reject()
        }))
    }

    fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<server::Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let target = format!("{host_to_connect}:{port_to_connect}");
        tokio::spawn(async move {
            match TcpStream::connect(target).await {
                Ok(mut tcp) => {
                    reply.accept().await;
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
                }
                Err(_) => reply.reject(ChannelOpenFailure::ConnectFailed).await,
            }
        });
        ready(Ok(()))
    }
}

struct TestServer {
    addr: SocketAddr,
    /// Sessions currently alive on the server.
    live: Arc<AtomicUsize>,
    /// Sessions ever accepted.
    total: Arc<AtomicUsize>,
    handles: Arc<Mutex<Vec<server::Handle>>>,
    _task: AbortOnDrop,
}

impl TestServer {
    async fn start() -> Self {
        let host_key = PrivateKey::from_openssh(HOST_A).unwrap();
        let config = Arc::new(server::Config {
            keys: vec![host_key],
            auth_rejection_time: Duration::from_millis(5),
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..server::Config::default()
        });
        let authorized = Arc::new(
            [
                CLIENT_ED25519_PUB,
                CLIENT_ED25519_ENC_PUB,
                CLIENT_RSA_PEM_ENC_PUB,
                CLIENT_ECDSA_PKCS8_PUB,
            ]
            .map(public)
            .to_vec(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let live = Arc::new(AtomicUsize::new(0));
        let total = Arc::new(AtomicUsize::new(0));
        let handles = Arc::new(Mutex::new(Vec::new()));
        let task = {
            let (live, total, handles) = (live.clone(), total.clone(), handles.clone());
            tokio::spawn(async move {
                let mut sessions = tokio::task::JoinSet::new();
                while let Ok((tcp, _)) = listener.accept().await {
                    total.fetch_add(1, Ordering::SeqCst);
                    let handler = ServerHandler {
                        authorized: authorized.clone(),
                        _live: LiveGuard::new(&live),
                    };
                    let (config, handles) = (config.clone(), handles.clone());
                    sessions.spawn(async move {
                        if let Ok(running) = server::run_stream(config, tcp, handler).await {
                            handles.lock().unwrap().push(running.handle());
                            let _ = running.await;
                        }
                    });
                }
            })
        };
        Self {
            addr,
            live,
            total,
            handles,
            _task: AbortOnDrop(task),
        }
    }

    /// Disconnects every session from the server side.
    async fn kill_all(&self) {
        let handles: Vec<_> = self.handles.lock().unwrap().drain(..).collect();
        for h in handles {
            let _ = h
                .disconnect(russh::Disconnect::ByApplication, "bye".into(), "en".into())
                .await;
        }
    }

    fn ssh(&self, auth: SshAuth) -> SshOptions {
        SshOptions::new("127.0.0.1", USER, auth)
            .with_port(self.addr.port())
            .with_known_hosts(pinned())
    }
}

fn pinned() -> KnownHostsPolicy {
    KnownHostsPolicy::Fingerprint {
        fingerprint: fingerprint(&public(HOST_A_PUB)),
    }
}

fn password() -> SshAuth {
    SshAuth::Password(Secret::new(PASSWORD))
}

/// An echo server; returns its address.
async fn echo_server() -> (Endpoint, AbortOnDrop) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = tcp.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    (Endpoint::from(addr), AbortOnDrop(task))
}

async fn round_trip(addr: SocketAddr, payload: &[u8]) {
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    tcp.write_all(payload).await.unwrap();
    let mut back = vec![0; payload.len()];
    tokio::time::timeout(Duration::from_secs(10), tcp.read_exact(&mut back))
        .await
        .expect("echo timed out")
        .unwrap();
    assert_eq!(back, payload);
}

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn open(
    provider: &RusshTunnelProvider,
    ssh: &SshOptions,
    target: &Endpoint,
) -> Result<TunnelHandle> {
    provider.open(ssh, target, CancellationToken::new()).await
}

#[tokio::test]
async fn password_tunnel_forwards_concurrent_connections_and_closes_on_drop() {
    let server = TestServer::start().await;
    let (echo, _echo) = echo_server().await;
    let provider = RusshTunnelProvider::new();
    let tunnel = open(&provider, &server.ssh(password()), &echo)
        .await
        .unwrap();
    let local = tunnel.local_addr();
    assert!(local.ip().is_loopback() && local.is_ipv4(), "{local}");
    assert_ne!(local.port(), 0);
    assert!(!tunnel.is_closed());
    assert_eq!(tunnel.key().target, echo);
    assert_no_secret(&format!("{tunnel:?} {provider:?}"));

    // Many concurrent connections, each with its own channel, plus a large payload.
    let tasks: Vec<_> = (0..8u8)
        .map(|i| tokio::spawn(async move { round_trip(local, &[i; 1000]).await }))
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    let big: Vec<u8> = (0..300_000u32)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    round_trip(local, &big).await;

    // A connection that is open while the handle drops gets closed.
    let mut held = TcpStream::connect(local).await.unwrap();
    held.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    held.read_exact(&mut buf).await.unwrap();
    assert_eq!(server.live.load(Ordering::SeqCst), 1);
    drop(tunnel);
    assert_eq!(
        provider.session_count(),
        0,
        "last tunnel releases the session"
    );
    let n = tokio::time::timeout(Duration::from_secs(5), held.read(&mut buf))
        .await
        .expect("held connection not closed");
    assert!(matches!(n, Ok(0) | Err(_)), "{n:?}");
    let live = server.live.clone();
    eventually("server session to end", || live.load(Ordering::SeqCst) == 0).await;
    // The listener is gone.
    let mut refused = false;
    for _ in 0..100 {
        if TcpStream::connect(local).await.is_err() {
            refused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(refused, "listener still accepting after drop");
}

#[tokio::test]
async fn private_keys_in_every_format() {
    let server = TestServer::start().await;
    let (echo, _echo) = echo_server().await;
    let cases = [
        (
            "openssh file",
            KeySource::File(testdata("client_ed25519")),
            None,
        ),
        (
            "openssh encrypted inline",
            KeySource::Pem(Secret::new(CLIENT_ED25519_ENC)),
            Some(KEY_PASSPHRASE),
        ),
        (
            "pem rsa encrypted, indented and CRLF",
            KeySource::Pem(Secret::new(
                CLIENT_RSA_PEM_ENC
                    .lines()
                    .map(|l| ["  ", l, "\r\n"].concat())
                    .collect::<String>(),
            )),
            Some(KEY_PASSPHRASE),
        ),
        (
            "pkcs8 ecdsa",
            KeySource::Pem(Secret::new(CLIENT_ECDSA_PKCS8)),
            None,
        ),
    ];
    for (name, key, passphrase) in cases {
        let auth = SshAuth::PrivateKey {
            key,
            passphrase: passphrase.map(Secret::new),
        };
        // A fresh provider per case, so each case really authenticates (no shared session).
        let provider = RusshTunnelProvider::new();
        let tunnel = open(&provider, &server.ssh(auth), &echo)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        round_trip(tunnel.local_addr(), name.as_bytes()).await;
    }
}

#[tokio::test]
async fn auth_failures_are_typed_and_leak_nothing() {
    let server = TestServer::start().await;
    let (echo, _echo) = echo_server().await;
    let provider = RusshTunnelProvider::new();
    let endpoint = format!("127.0.0.1:{}", server.addr.port());

    let mut wrong = server.ssh(SshAuth::Password(Secret::new("not-the-password")));
    let err = open(&provider, &wrong, &echo).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Auth, "{err}");
    let msg = err.to_string();
    assert!(msg.contains(&endpoint) && msg.contains(USER), "{msg}");
    assert!(!msg.contains("not-the-password"), "{msg}");

    let enc = |passphrase: Option<&str>| SshAuth::PrivateKey {
        key: KeySource::Pem(Secret::new(CLIENT_ED25519_ENC)),
        passphrase: passphrase.map(Secret::new),
    };
    let mut errors = vec![msg];
    for (auth, category, needle) in [
        (enc(None), ErrorCategory::Auth, "needs a passphrase"),
        (
            enc(Some("wrong-pass")),
            ErrorCategory::Auth,
            "passphrase may be wrong",
        ),
        (
            SshAuth::PrivateKey {
                key: KeySource::Pem(Secret::new(CLIENT_RSA_PEM_ENC)),
                passphrase: None,
            },
            ErrorCategory::Auth,
            "needs a passphrase",
        ),
        (
            SshAuth::PrivateKey {
                key: KeySource::Pem(Secret::new("-----BEGIN PRIVATE KEY-----\nnot a key\n")),
                passphrase: None,
            },
            ErrorCategory::InvalidInput,
            "could not parse",
        ),
        (
            SshAuth::PrivateKey {
                key: KeySource::File(testdata("does-not-exist")),
                passphrase: None,
            },
            ErrorCategory::Io,
            "does-not-exist",
        ),
        (
            // A valid key the server does not know.
            SshAuth::PrivateKey {
                key: KeySource::File(testdata("host_ed25519_b")),
                passphrase: None,
            },
            ErrorCategory::Auth,
            "rejected public key authentication",
        ),
    ] {
        wrong.auth = auth;
        let err = open(&provider, &wrong, &echo).await.unwrap_err();
        let msg = err.to_string();
        assert_eq!(err.category(), category, "{msg}");
        assert!(msg.contains(needle), "{msg}");
        errors.push(msg);
        errors.push(format!("{err:?}"));
    }
    for e in &errors {
        assert_no_secret(e);
        assert!(!e.contains("wrong-pass"), "{e}");
    }
    assert_eq!(provider.session_count(), 0);
    let live = server.live.clone();
    eventually("failed sessions to close", || {
        live.load(Ordering::SeqCst) == 0
    })
    .await;
}

#[tokio::test]
async fn host_key_policies() {
    let server = TestServer::start().await;
    let (echo, _echo) = echo_server().await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("known_hosts");
    let presented = fingerprint(&public(HOST_A_PUB));
    let with = |policy: KnownHostsPolicy| server.ssh(password()).with_known_hosts(policy);
    let strict = || {
        with(KnownHostsPolicy::Strict {
            path: Some(file.clone()),
        })
    };

    // Strict, unknown host (file missing).
    let err = open(&RusshTunnelProvider::new(), &strict(), &echo)
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert_eq!(err.category(), ErrorCategory::Connection, "{msg}");
    assert!(
        msg.starts_with("connection error: SSH host key for 127.0.0.1:"),
        "{msg}"
    );
    assert!(msg.contains(&presented), "{msg}");

    // AcceptNew learns it; Strict then accepts it.
    let tofu = with(KnownHostsPolicy::AcceptNew {
        path: Some(file.clone()),
    });
    drop(
        open(&RusshTunnelProvider::new(), &tofu, &echo)
            .await
            .unwrap(),
    );
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        text.starts_with(&format!("[127.0.0.1]:{} ssh-ed25519 ", server.addr.port())),
        "{text}"
    );
    let ok = open(&RusshTunnelProvider::new(), &strict(), &echo)
        .await
        .unwrap();
    round_trip(ok.local_addr(), b"strict").await;

    // A changed key fails both Strict and AcceptNew.
    let b_fields: Vec<&str> = HOST_B_PUB.split_whitespace().take(2).collect();
    std::fs::write(
        &file,
        format!(
            "[127.0.0.1]:{} {}\n",
            server.addr.port(),
            b_fields.join(" ")
        ),
    )
    .unwrap();
    for ssh in [strict(), tofu.clone()] {
        let err = open(&RusshTunnelProvider::new(), &ssh, &echo)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert_eq!(err.category(), ErrorCategory::Connection, "{msg}");
        assert!(
            msg.contains("SSH host key") && msg.contains("does not match"),
            "{msg}"
        );
        assert!(msg.contains(&presented), "{msg}");
    }

    // Fingerprint pin: right and wrong.
    let right = open(&RusshTunnelProvider::new(), &with(pinned()), &echo).await;
    assert!(right.is_ok());
    let wrong_pin = with(KnownHostsPolicy::Fingerprint {
        fingerprint: fingerprint(&public(HOST_B_PUB)),
    });
    let err = open(&RusshTunnelProvider::new(), &wrong_pin, &echo)
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("pinned fingerprint") && msg.contains(&presented),
        "{msg}"
    );
    assert_no_secret(&msg);
}

#[tokio::test]
async fn sessions_are_shared_and_reference_counted() {
    let server = TestServer::start().await;
    let (echo_1, _e1) = echo_server().await;
    let (echo_2, _e2) = echo_server().await;
    let provider = RusshTunnelProvider::new();
    let pool = TunnelPool::new(Arc::new(provider.clone()));
    let ssh = server.ssh(password());

    let a = pool
        .acquire(&ssh, &echo_1, CancellationToken::new())
        .await
        .unwrap();
    let a2 = pool
        .acquire(&ssh, &echo_1, CancellationToken::new())
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&a, &a2), "same key, same tunnel");
    let b = pool
        .acquire(&ssh, &echo_2, CancellationToken::new())
        .await
        .unwrap();
    assert_ne!(a.local_addr(), b.local_addr());
    assert_eq!(pool.open_count(), 2);
    assert_eq!(
        provider.session_count(),
        1,
        "one SSH session for both targets"
    );
    assert_eq!(server.total.load(Ordering::SeqCst), 1);
    round_trip(a.local_addr(), b"one").await;
    round_trip(b.local_addr(), b"two").await;

    // A different host-key policy shares neither the tunnel nor the session: it gets its own
    // host-key check (here learning the key into a new file).
    let dir = tempfile::tempdir().unwrap();
    let other_policy = ssh.clone().with_known_hosts(KnownHostsPolicy::AcceptNew {
        path: Some(dir.path().join("kh")),
    });
    let c = pool
        .acquire(&other_policy, &echo_2, CancellationToken::new())
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&b, &c));
    assert_eq!(provider.session_count(), 2);
    assert_eq!(server.total.load(Ordering::SeqCst), 2);
    assert!(dir.path().join("kh").exists(), "the new policy was checked");
    drop(c);
    assert_eq!(provider.session_count(), 1);

    drop((a, a2));
    assert_eq!(provider.session_count(), 1, "b still uses the session");
    round_trip(b.local_addr(), b"still up").await;
    drop(b);
    assert_eq!(provider.session_count(), 0);
    assert_eq!(pool.open_count(), 0);
    let live = server.live.clone();
    eventually("server sessions to end", || {
        live.load(Ordering::SeqCst) == 0
    })
    .await;
}

#[tokio::test]
async fn live_session_never_covers_another_credential_or_policy() {
    let server = TestServer::start().await;
    let (echo, _echo) = echo_server().await;
    let provider = RusshTunnelProvider::new();
    let pool = TunnelPool::new(Arc::new(provider.clone()));
    let ssh = server.ssh(password());
    let live = pool
        .acquire(&ssh, &echo, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(server.total.load(Ordering::SeqCst), 1);

    // A wrong password fails while a session with the right one is open, through the provider
    // (a dedicated "test connection" tunnel) and through the pool.
    let mut wrong = ssh.clone();
    wrong.auth = SshAuth::Password(Secret::new("not-the-password"));
    let err = open(&provider, &wrong, &echo).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Auth, "{err}");
    let err = pool
        .acquire(&wrong, &echo, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Auth, "{err}");
    assert_no_secret(&err.to_string());

    // A Strict policy with an empty known_hosts file and a wrong pin fail the same way.
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("known_hosts");
    std::fs::write(&empty, "").unwrap();
    for policy in [
        KnownHostsPolicy::Strict { path: Some(empty) },
        KnownHostsPolicy::Fingerprint {
            fingerprint: fingerprint(&public(HOST_B_PUB)),
        },
    ] {
        let err = pool
            .acquire(
                &ssh.clone().with_known_hosts(policy),
                &echo,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.category(), ErrorCategory::Connection, "{err}");
        assert!(err.to_string().contains("SSH host key"), "{err}");
    }

    // The same credential still shares the live session (no new server session).
    let before = server.total.load(Ordering::SeqCst);
    let again = open(&provider, &ssh, &echo).await.unwrap();
    assert_eq!(server.total.load(Ordering::SeqCst), before);
    assert_eq!(provider.session_count(), 1);
    round_trip(again.local_addr(), b"same").await;
    drop((live, again));
    assert_eq!(provider.session_count(), 0);
}

#[tokio::test]
async fn unsupported_and_encrypted_key_formats_are_reported_precisely() {
    async fn load(text: &str, passphrase: Option<&str>) -> Result<PrivateKey> {
        let passphrase = passphrase.map(Secret::new);
        load_private_key(&KeySource::Pem(Secret::new(text)), passphrase.as_ref()).await
    }
    let pem = |label: &str, dek: &str| {
        format!(
            "-----BEGIN {label}-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: {dek},00112233445566778899AABBCCDDEEFF\n\nAAAABBBBCCCC\n-----END {label}-----\n"
        )
    };
    let rsa = "RSA PRIVATE KEY";
    for (text, needle) in [
        (pem(rsa, "AES-256-CBC"), "AES-256-CBC is not supported"),
        (pem(rsa, "DES-EDE3-CBC"), "DES-EDE3-CBC is not supported"),
        (pem("EC PRIVATE KEY", "AES-128-CBC"), "only for RSA keys"),
    ] {
        for passphrase in [None, Some(KEY_PASSPHRASE)] {
            let err = load(&text, passphrase).await.unwrap_err();
            let msg = err.to_string();
            assert_eq!(err.category(), ErrorCategory::InvalidInput, "{msg}");
            assert!(
                msg.contains(needle) && msg.contains("ssh-keygen -p"),
                "{msg}"
            );
            assert_no_secret(&msg);
        }
    }
    // The supported legacy cipher still reaches the decoder (and fails there as a bad key).
    let err = load(&pem(rsa, "AES-128-CBC"), Some("x")).await.unwrap_err();
    assert!(!err.to_string().contains("not supported"), "{err}");

    // PuTTY keys: an encrypted one needs a passphrase, and a wrong one is reported as such.
    let ppk = |encryption: &str| {
        format!(
            "PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: {encryption}\nComment: test\nPublic-Lines: 1\nAAAA\nPrivate-Lines: 1\nAAAA\nPrivate-MAC: 00\n"
        )
    };
    let err = load(&ppk("aes256-cbc"), None).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Auth, "{err}");
    assert!(err.to_string().contains("needs a passphrase"), "{err}");
    let err = load(&ppk("aes256-cbc"), Some("wrong")).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Auth, "{err}");
    assert!(err.to_string().contains("passphrase may be wrong"), "{err}");
    let err = load(&ppk("none"), None).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput, "{err}");
}

#[tokio::test]
async fn session_loss_closes_tunnels_and_the_next_open_reconnects() {
    let server = TestServer::start().await;
    let (echo, _echo) = echo_server().await;
    let provider = RusshTunnelProvider::new();
    let pool = TunnelPool::new(Arc::new(provider.clone()));
    let ssh = server.ssh(password());
    let first = pool
        .acquire(&ssh, &echo, CancellationToken::new())
        .await
        .unwrap();
    round_trip(first.local_addr(), b"before").await;

    server.kill_all().await;
    tokio::time::timeout(Duration::from_secs(10), first.closed())
        .await
        .expect("tunnel not closed after the session died");
    assert!(first.is_closed());
    assert_eq!(provider.session_count(), 0);

    let second = pool
        .acquire(&ssh, &echo, CancellationToken::new())
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    assert!(!second.is_closed());
    round_trip(second.local_addr(), b"after").await;
    assert_eq!(server.total.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn unreachable_target_fails_per_connection_only() {
    let server = TestServer::start().await;
    let (echo, _echo) = echo_server().await;
    // A port that was just free: nothing listens there.
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        Endpoint::from(l.local_addr().unwrap())
    };
    let provider = RusshTunnelProvider::new();
    let ssh = server.ssh(password());
    let tunnel = open(&provider, &ssh, &dead)
        .await
        .expect("open must not probe the target");
    let mut tcp = TcpStream::connect(tunnel.local_addr()).await.unwrap();
    let mut buf = [0u8; 1];
    let r = tokio::time::timeout(Duration::from_secs(10), tcp.read(&mut buf))
        .await
        .expect("connection to an unreachable target was not closed");
    assert!(matches!(r, Ok(0) | Err(_)), "{r:?}");
    assert!(!tunnel.is_closed());
    // The session is still fine for other tunnels.
    let ok = open(&provider, &ssh, &echo).await.unwrap();
    round_trip(ok.local_addr(), b"fine").await;
    assert_eq!(provider.session_count(), 1);
}

/// Accepts TCP connections and never says anything.
async fn black_hole() -> (SocketAddr, AbortOnDrop) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((tcp, _)) = listener.accept().await {
            held.push(tcp);
        }
    });
    (addr, AbortOnDrop(task))
}

#[tokio::test]
async fn cancel_and_timeout() {
    let (addr, _hole) = black_hole().await;
    let target = Endpoint::new("db", 5432);
    let provider = RusshTunnelProvider::new();
    let ssh = SshOptions::new("127.0.0.1", USER, password())
        .with_port(addr.port())
        .with_known_hosts(pinned());

    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        trigger.cancel();
    });
    let started = std::time::Instant::now();
    let err = provider.open(&ssh, &target, cancel).await.unwrap_err();
    assert!(err.is_cancelled(), "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));

    let already = CancellationToken::new();
    already.cancel();
    assert!(
        provider
            .open(&ssh, &target, already)
            .await
            .unwrap_err()
            .is_cancelled()
    );

    let mut quick = ssh.clone();
    quick.connect_timeout = Some(Duration::from_millis(300));
    let err = open(&provider, &quick, &target).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Timeout, "{err}");
    assert!(err.to_string().contains(&addr.to_string()), "{err}");

    // Nothing listening at all: a connection error naming the SSH server.
    let closed_port = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let err = open(&provider, &ssh.clone().with_port(closed_port), &target)
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Connection, "{err}");
    assert!(
        err.to_string()
            .contains(&format!("127.0.0.1:{closed_port}")),
        "{err}"
    );
    assert_no_secret(&err.to_string());

    // Invalid options are rejected before any I/O.
    let err = open(&provider, &ssh, &Endpoint::new("", 5432))
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
    let err = open(&provider, &ssh.clone().with_port(0), &target)
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::InvalidInput);
}

#[cfg(unix)]
#[tokio::test]
async fn agent_authentication() {
    use russh::keys::agent::client::AgentClient;
    use russh::keys::agent::server::Agent;

    #[derive(Clone)]
    struct AllowAll;
    impl Agent for AllowAll {}

    let server = TestServer::start().await;
    let (echo, _echo) = echo_server().await;
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("agent.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let incoming = futures::stream::unfold(listener, |l| async move {
        let next = l.accept().await.map(|(s, _)| s);
        Some((next, l))
    });
    let _agent = AbortOnDrop(tokio::spawn(async move {
        let _ = russh::keys::agent::server::serve(Box::pin(incoming), AllowAll).await;
    }));

    let ssh = server.ssh(SshAuth::Agent);
    let provider = RusshTunnelProvider::new().with_agent_path(&sock);

    // An empty agent: typed auth error.
    let err = open(&provider, &ssh, &echo).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Auth, "{err}");
    assert!(err.to_string().contains("holds no keys"), "{err}");

    // Load an unknown key first and the authorized one second: the provider tries each.
    let mut client = AgentClient::connect_uds(&sock).await.unwrap();
    let unknown = PrivateKey::from_openssh(include_str!("testdata/host_ed25519_b")).unwrap();
    let known = PrivateKey::from_openssh(CLIENT_ED25519).unwrap();
    client.add_identity(&unknown, &[]).await.unwrap();
    client.add_identity(&known, &[]).await.unwrap();
    let tunnel = open(&provider, &ssh, &echo).await.unwrap();
    round_trip(tunnel.local_addr(), b"agent").await;

    // No agent socket at all.
    let missing = RusshTunnelProvider::new().with_agent_path(dir.path().join("nope.sock"));
    let mut fresh_user = ssh.clone();
    fresh_user.user = "someone-else".into();
    let err = open(&missing, &fresh_user, &echo).await.unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Connection, "{err}");
    assert!(err.to_string().contains("SSH agent"), "{err}");
}
