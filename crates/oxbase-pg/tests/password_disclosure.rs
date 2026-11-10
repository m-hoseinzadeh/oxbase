//! Audit lead 5: the server picks the password exchange, so an unverified peer (an on-path
//! attacker answering `N` to the TLS request, or presenting any certificate) must not be able
//! to ask for cleartext or MD5 password authentication and recover the reusable password.
//!
//! An in-process fake server on `127.0.0.1:0` plays the attacker. The profile names a remote
//! host (`db.example`) and dials the fake server through `dial_addr`, as an SSH tunnel would.
//! No live database is needed.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use oxbase_driver::{ConnectParams, PemSource, TlsMode, TlsOptions};
use oxbase_net::tls::rustls::ServerConfig;
use oxbase_net::tls::rustls::pki_types::pem::PemObject;
use oxbase_net::tls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use oxbase_pg::PgConnection;
use oxbase_types::{Engine, Error};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// The dummy password; it must never reach the fake server unless the exchange is allowed.
const MARKER: &str = "MARKER-pw-1234";
/// The remote server name the profile uses (the fake server is reached through `dial_addr`).
const REMOTE: &str = "db.example";
/// The fake server's own address, used as the host for the local-server case.
const LOOPBACK: &str = "127.0.0.1";
/// `SSLRequest` code (PostgreSQL protocol).
const SSL_REQUEST: u32 = 80_877_103;

/// What the fake server asks for once the startup message is in.
#[derive(Debug, Clone, Copy)]
enum Ask {
    Cleartext,
    Md5,
    Scram,
}

/// A test CA and a server certificate for [`REMOTE`] it issued.
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

/// Serves one connection: answers the `SSLRequest` (`S` and a TLS handshake with `tls`, else
/// `N`), reads the startup message, sends the `ask` request and returns the next frontend
/// message (`None`: the client hung up without sending one).
async fn serve(listener: TcpListener, tls: Option<TlsAcceptor>, ask: Ask) -> Option<(u8, Vec<u8>)> {
    let (mut tcp, _) = listener.accept().await.unwrap();
    let mut head = [0u8; 8];
    tcp.read_exact(&mut head).await.unwrap();
    assert_eq!(
        u32::from_be_bytes([head[4], head[5], head[6], head[7]]),
        SSL_REQUEST,
        "expected an SSLRequest first"
    );
    if let Some(acceptor) = tls {
        tcp.write_all(b"S").await.unwrap();
        let stream = acceptor.accept(tcp).await.unwrap();
        authenticate(stream, ask).await
    } else {
        tcp.write_all(b"N").await.unwrap();
        authenticate(tcp, ask).await
    }
}

async fn authenticate<S: AsyncRead + AsyncWrite + Unpin>(
    mut s: S,
    ask: Ask,
) -> Option<(u8, Vec<u8>)> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await.unwrap();
    let mut startup = vec![0u8; usize::try_from(u32::from_be_bytes(len)).unwrap() - 4];
    s.read_exact(&mut startup).await.unwrap();
    let request: Vec<u8> = match ask {
        Ask::Cleartext => [&b"R"[..], &8u32.to_be_bytes()[..], &3u32.to_be_bytes()[..]].concat(),
        Ask::Md5 => [
            &b"R"[..],
            &12u32.to_be_bytes()[..],
            &5u32.to_be_bytes()[..],
            &[1u8, 2, 3, 4][..],
        ]
        .concat(),
        Ask::Scram => {
            let mechanisms = &b"SCRAM-SHA-256\0\0"[..];
            let len = u32::try_from(8 + mechanisms.len()).unwrap();
            [
                &b"R"[..],
                &len.to_be_bytes()[..],
                &10u32.to_be_bytes()[..],
                mechanisms,
            ]
            .concat()
        }
    };
    s.write_all(&request).await.unwrap();
    s.flush().await.unwrap();
    // The next frontend message, if any (EOF, a TLS error or silence: none).
    let read = async {
        let mut kind = [0u8; 1];
        s.read_exact(&mut kind).await.ok()?;
        s.read_exact(&mut len).await.ok()?;
        let mut body = vec![0u8; usize::try_from(u32::from_be_bytes(len)).ok()? - 4];
        s.read_exact(&mut body).await.ok()?;
        Some((kind[0], body))
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .ok()
        .flatten()
}

fn params(addr: SocketAddr, host: &str, tls: TlsOptions) -> ConnectParams {
    let mut p = ConnectParams::network(Engine::Postgres, host)
        .with_port(addr.port())
        .with_user("oxbase")
        .with_password(MARKER)
        .with_database("postgres")
        .with_tls(tls);
    // A remote name is reached through `dial_addr`, like a tunnel; the loopback case dials it.
    if host != LOOPBACK {
        p = p.with_dial_addr(addr);
    }
    p.connect_timeout = Some(Duration::from_secs(10));
    p
}

/// Runs the fake server and a connect against it; returns the connect error (if any) and the
/// message the server received after its request.
async fn exchange(host: &str, tls: TlsOptions, acceptor: Option<TlsAcceptor>, ask: Ask) -> Outcome {
    let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(serve(listener, acceptor, ask));
    let client = PgConnection::connect(&params(addr, host, tls)).await;
    let received = server.await.unwrap();
    (client.err(), received)
}

type Outcome = (Option<Error>, Option<(u8, Vec<u8>)>);

fn assert_refused(outcome: Outcome, case: &str) {
    let (err, received) = outcome;
    assert_eq!(received, None, "{case}: the client answered the request");
    let err = err.unwrap_or_else(|| panic!("{case}: connect succeeded"));
    let Error::Auth { message } = &err else {
        panic!("{case}: expected an authentication error, got {err}");
    };
    assert!(message.contains("verify_full"), "{case}: {message}");
    assert!(!message.contains(MARKER), "{case}: {message}");
}

#[tokio::test]
async fn cleartext_and_md5_are_refused_without_tls_under_prefer() {
    for ask in [Ask::Cleartext, Ask::Md5] {
        let outcome = exchange(REMOTE, TlsOptions::new(TlsMode::Prefer), None, ask).await;
        assert_refused(outcome, &format!("prefer, no TLS, {ask:?}"));
    }
}

#[tokio::test]
async fn cleartext_and_md5_are_refused_over_unverified_tls() {
    let pki = pki();
    for mode in [TlsMode::Prefer, TlsMode::Require] {
        for ask in [Ask::Cleartext, Ask::Md5] {
            let outcome = exchange(
                REMOTE,
                TlsOptions::new(mode),
                Some(pki.acceptor.clone()),
                ask,
            )
            .await;
            assert_refused(outcome, &format!("{mode} over TLS, {ask:?}"));
        }
    }
}

#[tokio::test]
async fn scram_still_answers_without_tls() {
    // SCRAM never sends the password: the client goes on with its first message.
    let (_, received) = exchange(REMOTE, TlsOptions::new(TlsMode::Prefer), None, Ask::Scram).await;
    let (kind, body) = received.expect("the client answered the SCRAM request");
    assert_eq!(kind, b'p');
    assert!(body.starts_with(b"SCRAM-SHA-256\0"), "{body:?}");
    assert!(
        !body.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()),
        "the SCRAM message carries the password"
    );
}

#[tokio::test]
async fn cleartext_is_answered_for_a_verified_or_local_server() {
    // Verified certificate: the peer is the server, so the request is answered as before.
    let pki = pki();
    let verified = TlsOptions::new(TlsMode::VerifyFull).with_ca_cert(PemSource::Pem(pki.ca_pem));
    let (_, received) = exchange(REMOTE, verified, Some(pki.acceptor), Ask::Cleartext).await;
    assert_eq!(
        received,
        Some((b'p', format!("{MARKER}\0").into_bytes())),
        "verify_full"
    );

    // A loopback server is on this machine: no network to attack.
    let (_, received) = exchange(
        LOOPBACK,
        TlsOptions::new(TlsMode::Prefer),
        None,
        Ask::Cleartext,
    )
    .await;
    assert_eq!(
        received,
        Some((b'p', format!("{MARKER}\0").into_bytes())),
        "loopback"
    );
}

#[tokio::test]
async fn cleartext_is_answered_when_the_user_allows_an_unverified_server() {
    // The user's explicit choice in the profile's TLS settings.
    let allowed = TlsOptions::new(TlsMode::Prefer).with_unverified_password_allowed(true);
    let (_, received) = exchange(REMOTE, allowed, None, Ask::Cleartext).await;
    assert_eq!(
        received,
        Some((b'p', format!("{MARKER}\0").into_bytes())),
        "allowed by the user"
    );
}
