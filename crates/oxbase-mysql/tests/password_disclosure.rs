//! Audit lead 5: `caching_sha2_password` full authentication sends the password itself (in
//! clear over TLS, else encrypted to an RSA key the server sends), so an unverified peer (an
//! on-path attacker that leaves TLS out of its handshake, or presents any certificate) must not
//! be able to ask for it.
//!
//! An in-process fake server on `127.0.0.1:0` plays the attacker. The profile names a remote
//! host (`db.example`) and dials the fake server through `dial_addr`, as an SSH tunnel would.
//! No live database is needed.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use oxbase_driver::{ConnectParams, PemSource, TlsMode, TlsOptions};
use oxbase_mysql::MySqlDriver;
use oxbase_net::tls::rustls::ServerConfig;
use oxbase_net::tls::rustls::pki_types::pem::PemObject;
use oxbase_net::tls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use oxbase_types::{Engine, Error};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// The dummy password; it must never reach the fake server unless full auth is allowed.
const MARKER: &str = "MARKER-pw-1234";
/// The remote server name the profile uses (the fake server is reached through `dial_addr`).
const REMOTE: &str = "db.example";
/// The fake server's own address, used as the host for the local-server case.
const LOOPBACK: &str = "127.0.0.1";
/// The handshake scramble (20 bytes).
const NONCE: &[u8; 20] = b"0123456789abcdefghij";

/// Lower and upper capability halves: `LONG_PASSWORD | FOUND_ROWS | LONG_FLAG |
/// CONNECT_WITH_DB | PROTOCOL_41 | TRANSACTIONS | SECURE_CONNECTION`, and `MULTI_STATEMENTS |
/// MULTI_RESULTS | PLUGIN_AUTH | CONNECT_ATTRS | PLUGIN_AUTH_LENENC_CLIENT_DATA | DEPRECATE_EOF`.
const CAPS_LOW: u16 = 0x0001 | 0x0002 | 0x0004 | 0x0008 | 0x0200 | 0x2000 | 0x8000;
const CAPS_HIGH: u16 = 0x0001 | 0x0002 | 0x0008 | 0x0010 | 0x0020 | 0x0100;
/// `CLIENT_SSL`.
const CAPS_SSL: u16 = 0x0800;

/// How the fake server answers the client's login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// `01 04`: perform full authentication (send the password).
    FullAuth,
    /// `01 03` and an OK packet: fast authentication succeeded.
    FastAuth,
}

/// A TLS acceptor for [`REMOTE`] and the PEM of the test CA that issued its certificate.
struct Pki {
    ca_pem: String,
    acceptor: TlsAcceptor,
}

fn pki() -> Pki {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(DnType::CommonName, "Oxbase Test CA");
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_pem = ca.self_signed(&ca_key).unwrap().pem();
    let issuer = Issuer::new(ca, ca_key);

    let key = KeyPair::generate().unwrap();
    let mut leaf = CertificateParams::new(vec![REMOTE.to_owned()]).unwrap();
    leaf.distinguished_name.push(DnType::CommonName, REMOTE);
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let cert = leaf.signed_by(&key, &issuer).unwrap();
    let chain = vec![CertificateDer::from_pem_slice(cert.pem().as_bytes()).unwrap()];
    let key = PrivateKeyDer::from_pem_slice(key.serialize_pem().as_bytes()).unwrap();
    let config = ServerConfig::builder_with_provider(oxbase_net::tls::crypto_provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
    Pki {
        ca_pem,
        acceptor: TlsAcceptor::from(Arc::new(config)),
    }
}

async fn read_packet<S: AsyncRead + Unpin>(s: &mut S) -> Option<(u8, Vec<u8>)> {
    let mut header = [0u8; 4];
    s.read_exact(&mut header).await.ok()?;
    let len =
        usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
    let mut payload = vec![0u8; len];
    s.read_exact(&mut payload).await.ok()?;
    Some((header[3], payload))
}

async fn write_packet<S: AsyncWrite + Unpin>(s: &mut S, seq: u8, payload: &[u8]) {
    let len = u32::try_from(payload.len()).unwrap().to_le_bytes();
    s.write_all(&[len[0], len[1], len[2], seq]).await.unwrap();
    s.write_all(payload).await.unwrap();
    s.flush().await.unwrap();
}

/// A protocol-10 handshake announcing `caching_sha2_password` (and TLS when `tls`).
fn handshake(tls: bool) -> Vec<u8> {
    let low = if tls { CAPS_LOW | CAPS_SSL } else { CAPS_LOW };
    let mut p = vec![10];
    p.extend_from_slice(b"8.4.0\0");
    p.extend_from_slice(&7u32.to_le_bytes()); // connection id
    p.extend_from_slice(&NONCE[..8]);
    p.push(0);
    p.extend_from_slice(&low.to_le_bytes());
    p.push(255); // utf8mb4_0900_ai_ci
    p.extend_from_slice(&2u16.to_le_bytes()); // SERVER_STATUS_AUTOCOMMIT
    p.extend_from_slice(&CAPS_HIGH.to_le_bytes());
    p.push(21);
    p.extend_from_slice(&[0; 10]);
    p.extend_from_slice(&NONCE[8..]);
    p.push(0);
    p.extend_from_slice(b"caching_sha2_password\0");
    p
}

/// Every packet the client sent on one connection, and what it sent after the server's
/// authentication answer.
#[derive(Debug, Default)]
struct Seen {
    all: Vec<Vec<u8>>,
    after_answer: Vec<Vec<u8>>,
}

impl Seen {
    fn carries_marker(&self) -> bool {
        self.all
            .iter()
            .any(|p| p.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()))
    }
}

/// Serves connections until one gets past the handshake (a `Prefer` client that wanted TLS
/// hangs up on a server without it and comes back in plaintext), then answers its login.
async fn serve(listener: TcpListener, tls: Option<TlsAcceptor>, answer: Answer) -> Seen {
    loop {
        let (mut tcp, _) = listener.accept().await.unwrap();
        write_packet(&mut tcp, 0, &handshake(tls.is_some())).await;
        let Some((seq, first)) = read_packet(&mut tcp).await else {
            continue;
        };
        // A hang-up (COM_QUIT) instead of a login: wait for the next connection.
        if first == [0x01] {
            continue;
        }
        return if let Some(acceptor) = tls {
            assert_eq!(first.len(), 32, "expected an SSLRequest");
            let mut stream = acceptor.accept(tcp).await.unwrap();
            let (seq, response) = read_packet(&mut stream).await.unwrap();
            login(&mut stream, seq, vec![first, response], answer).await
        } else {
            login(&mut tcp, seq, vec![first], answer).await
        };
    }
}

async fn login<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    seq: u8,
    all: Vec<Vec<u8>>,
    answer: Answer,
) -> Seen {
    match answer {
        Answer::FullAuth => write_packet(s, seq + 1, &[0x01, 0x04]).await,
        Answer::FastAuth => {
            write_packet(s, seq + 1, &[0x01, 0x03]).await;
            write_packet(s, seq + 2, &[0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00]).await;
        }
    }
    let mut seen = Seen {
        all,
        after_answer: Vec::new(),
    };
    // Whatever follows, until the client hangs up or goes quiet.
    while let Ok(Some((_, packet))) =
        tokio::time::timeout(Duration::from_secs(3), read_packet(s)).await
    {
        seen.all.push(packet.clone());
        seen.after_answer.push(packet);
        if seen.after_answer.len() == 1 && answer == Answer::FastAuth {
            break;
        }
    }
    seen
}

fn params(addr: SocketAddr, host: &str, tls: TlsOptions) -> ConnectParams {
    let mut p = ConnectParams::network(Engine::MySql, host)
        .with_port(addr.port())
        .with_user("oxbase")
        .with_password(MARKER)
        .with_tls(tls);
    // A remote name is reached through `dial_addr`, like a tunnel; the loopback case dials it.
    if host != LOOPBACK {
        p = p.with_dial_addr(addr);
    }
    p.connect_timeout = Some(Duration::from_secs(10));
    p
}

/// Runs the fake server and a connect against it.
async fn exchange(
    host: &str,
    tls: TlsOptions,
    acceptor: Option<TlsAcceptor>,
    answer: Answer,
) -> (Option<Error>, Seen) {
    let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(serve(listener, acceptor, answer));
    let client = MySqlDriver::new()
        .connect_mysql(&params(addr, host, tls))
        .await;
    let err = client.err();
    let seen = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the fake server finished")
        .unwrap();
    (err, seen)
}

/// The client hung up after `01 04` without the password: nothing but an optional `COM_QUIT`.
fn assert_refused(outcome: (Option<Error>, Seen), case: &str) {
    let (err, seen) = outcome;
    assert!(
        !seen.carries_marker(),
        "{case}: the password reached the server"
    );
    assert!(
        seen.after_answer.iter().all(|p| *p == [0x01]),
        "{case}: the client went on with full authentication: {:?}",
        seen.after_answer
    );
    let err = err.unwrap_or_else(|| panic!("{case}: connect succeeded"));
    let Error::Auth { message } = &err else {
        panic!("{case}: expected an authentication error, got {err}");
    };
    assert!(message.contains("verify_full"), "{case}: {message}");
    assert!(!message.contains(MARKER), "{case}: {message}");
}

#[tokio::test]
async fn full_auth_is_refused_without_tls_under_prefer() {
    // Plaintext: the client would ask for the server's RSA key (0x02) and encrypt the password
    // to it.
    let outcome = exchange(
        REMOTE,
        TlsOptions::new(TlsMode::Prefer),
        None,
        Answer::FullAuth,
    )
    .await;
    assert_refused(outcome, "prefer, no TLS");
}

#[tokio::test]
async fn full_auth_is_refused_over_unverified_tls() {
    // TLS with any certificate: the client would send the password in clear inside it.
    let pki = pki();
    for mode in [TlsMode::Prefer, TlsMode::Require] {
        let outcome = exchange(
            REMOTE,
            TlsOptions::new(mode),
            Some(pki.acceptor.clone()),
            Answer::FullAuth,
        )
        .await;
        assert_refused(outcome, &format!("{mode} over TLS"));
    }
}

#[tokio::test]
async fn fast_auth_still_works_unverified() {
    // A cached login needs only the scramble: the client goes on to its first query.
    let (_, seen) = exchange(
        REMOTE,
        TlsOptions::new(TlsMode::Prefer),
        None,
        Answer::FastAuth,
    )
    .await;
    assert!(!seen.carries_marker(), "the password reached the server");
    assert_eq!(
        seen.after_answer.first().map(|p| p[0]),
        Some(0x03),
        "expected a COM_QUERY after fast auth: {:?}",
        seen.after_answer
    );
}

#[tokio::test]
async fn full_auth_proceeds_for_a_verified_or_local_server() {
    // Verified certificate: the peer is the server, so the password goes inside TLS as before.
    let pki = pki();
    let verified = TlsOptions::new(TlsMode::VerifyFull).with_ca_cert(PemSource::Pem(pki.ca_pem));
    let (_, seen) = exchange(REMOTE, verified, Some(pki.acceptor), Answer::FullAuth).await;
    let password = format!("{MARKER}\0");
    assert_eq!(
        seen.after_answer.first().map(Vec::as_slice),
        Some(password.as_bytes()),
        "verify_full"
    );

    // A loopback server is on this machine: no network to attack, so the plaintext RSA path
    // starts (the public-key request, 0x02).
    let (_, seen) = exchange(
        LOOPBACK,
        TlsOptions::new(TlsMode::Prefer),
        None,
        Answer::FullAuth,
    )
    .await;
    assert_eq!(
        seen.after_answer.first().map(Vec::as_slice),
        Some(&[0x02][..]),
        "loopback"
    );
}
