//! Live SSH tunnel test against the fixtures bastion (`[ssh]` in `tests/fixtures.toml`).
//!
//! Ignored by default: it needs the fixtures stack (`scripts/fixtures-up.sh`) with the SSH
//! bastion and its generated keys under `target/fixtures/ssh`. Run with
//! `cargo test -p oxbase-net --test ssh_live -- --ignored`.
//!
//! For each auth method (password, key, encrypted key) and host-key policy (Strict with the
//! fixture `known_hosts`, Fingerprint pin) it forwards to the PostgreSQL fixture through the
//! bastion and reads PostgreSQL's answer to an `SSLRequest` (one byte, `S` or `N`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use oxbase_net::ssh::{
    KeySource, KnownHostsPolicy, RusshTunnelProvider, Secret, SshAuth, SshOptions, TunnelPool,
    TunnelProvider,
};
use oxbase_net::{CancellationToken, Endpoint};
use oxbase_types::ErrorCategory;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Deserialize)]
struct Fixtures {
    ssh: SshFixture,
}

#[derive(Deserialize)]
struct SshFixture {
    host: String,
    port: u16,
    user: String,
    password: String,
    key_dir: PathBuf,
    client_key: String,
    client_key_encrypted: String,
    client_key_passphrase: String,
    known_hosts: String,
    host_key_fingerprint: String,
    targets: BTreeMap<String, Target>,
}

#[derive(Deserialize)]
struct Target {
    host: String,
    port: u16,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixtures() -> SshFixture {
    let path = std::env::var_os("OXBASE_FIXTURES").map_or_else(
        || workspace_root().join("tests/fixtures.toml"),
        PathBuf::from,
    );
    let path = if path.is_relative() {
        workspace_root().join(path)
    } else {
        path
    };
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let parsed: Fixtures =
        toml::from_str(&text).unwrap_or_else(|e| panic!("bad {}: {e}", path.display()));
    let mut ssh = parsed.ssh;
    if ssh.key_dir.is_relative() {
        ssh.key_dir = workspace_root().join(&ssh.key_dir);
    }
    assert!(
        ssh.key_dir.join(&ssh.known_hosts).exists(),
        "fixture SSH keys missing under {}: start the fixtures (scripts/fixtures-up.sh)",
        ssh.key_dir.display()
    );
    ssh
}

/// Sends a PostgreSQL `SSLRequest` through the tunnel and returns the server's one-byte answer.
async fn postgres_ssl_answer(local: std::net::SocketAddr) -> u8 {
    let mut tcp = TcpStream::connect(local).await.unwrap();
    // Int32 length 8, Int32 code 80877103.
    tcp.write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f])
        .await
        .unwrap();
    let mut answer = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(10), tcp.read_exact(&mut answer))
        .await
        .expect("no answer from postgres through the tunnel")
        .unwrap();
    answer[0]
}

#[tokio::test]
#[ignore = "needs the fixtures stack with the SSH bastion (scripts/fixtures-up.sh)"]
async fn bastion_forwards_to_postgres() {
    let fx = fixtures();
    let target = fx.targets.get("postgres").expect("[ssh.targets] postgres");
    let target = Endpoint::new(target.host.clone(), target.port);
    let known_hosts = fx.key_dir.join(&fx.known_hosts);
    let fingerprint = std::fs::read_to_string(fx.key_dir.join(&fx.host_key_fingerprint))
        .unwrap()
        .trim()
        .to_owned();
    let strict = KnownHostsPolicy::Strict {
        path: Some(known_hosts),
    };
    let pinned = KnownHostsPolicy::Fingerprint { fingerprint };

    let auths = [
        (
            "password",
            SshAuth::Password(Secret::new(fx.password.clone())),
        ),
        (
            "key",
            SshAuth::PrivateKey {
                key: KeySource::File(fx.key_dir.join(&fx.client_key)),
                passphrase: None,
            },
        ),
        (
            "encrypted key",
            SshAuth::PrivateKey {
                key: KeySource::File(fx.key_dir.join(&fx.client_key_encrypted)),
                passphrase: Some(Secret::new(fx.client_key_passphrase.clone())),
            },
        ),
    ];
    for (name, auth) in auths {
        for policy in [strict.clone(), pinned.clone()] {
            let ssh = SshOptions::new(fx.host.clone(), fx.user.clone(), auth.clone())
                .with_port(fx.port)
                .with_known_hosts(policy.clone());
            // A fresh provider per case so each one really authenticates.
            let provider = RusshTunnelProvider::new();
            let tunnel = provider
                .open(&ssh, &target, CancellationToken::new())
                .await
                .unwrap_or_else(|e| panic!("{name} / {policy:?}: {e}"));
            assert!(tunnel.local_addr().ip().is_loopback());
            let answer = postgres_ssl_answer(tunnel.local_addr()).await;
            assert!(
                answer == b'S' || answer == b'N',
                "{name}: unexpected answer {answer}"
            );
            // A second connection on the same tunnel (like a cancel request) works too.
            let again = postgres_ssl_answer(tunnel.local_addr()).await;
            assert_eq!(again, answer);
        }
    }

    // Through the pool: two acquires share one tunnel.
    let ssh = SshOptions::new(
        fx.host.clone(),
        fx.user.clone(),
        SshAuth::Password(Secret::new(fx.password.clone())),
    )
    .with_port(fx.port)
    .with_known_hosts(strict.clone());
    let provider = RusshTunnelProvider::new();
    let pool = TunnelPool::new(std::sync::Arc::new(provider.clone()));
    let a = pool
        .acquire(&ssh, &target, CancellationToken::new())
        .await
        .unwrap();
    let b = pool
        .acquire(&ssh, &target, CancellationToken::new())
        .await
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(&a, &b));
    assert_eq!(provider.session_count(), 1);

    // Strict with an empty known_hosts file and a wrong pin both fail with a host-key error,
    // even through the provider and pool that hold a live, verified session to the bastion.
    let empty = tempfile::NamedTempFile::new().unwrap();
    for policy in [
        KnownHostsPolicy::Strict {
            path: Some(empty.path().to_owned()),
        },
        KnownHostsPolicy::Fingerprint {
            fingerprint: "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        },
    ] {
        let err = pool
            .acquire(
                &ssh.clone().with_known_hosts(policy),
                &target,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.category(), ErrorCategory::Connection, "{err}");
        assert!(err.to_string().contains("SSH host key"), "{err}");
        assert!(!err.to_string().contains(&fx.password), "{err}");
    }

    // A wrong password is an auth error that names the host and leaks nothing, even while a
    // session authenticated with the right one is open on the same provider.
    let mut wrong = ssh.clone();
    wrong.auth = SshAuth::Password(Secret::new("definitely-wrong-pw"));
    let err = provider
        .open(&wrong, &target, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Auth, "{err}");
    let msg = err.to_string();
    assert!(msg.contains(&fx.host), "{msg}");
    assert!(!msg.contains("definitely-wrong-pw") && !msg.contains(&fx.password));
    assert_eq!(
        provider.session_count(),
        1,
        "failed attempts leave no session behind"
    );
    drop((a, b));
    assert_eq!(provider.session_count(), 0);
}
