//! TLS semantics of `oxbase_net::tls::client_config`, exercised with real handshakes against an
//! in-process rustls server over an in-memory duplex stream. Certificates come from rcgen.

use std::io;
use std::sync::Arc;

use oxbase_net::tls::{self, KeySource, PemSource, Secret, TlsMode, TlsOptions};
use oxbase_types::ErrorCategory;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// A test CA that can issue leaf certificates.
struct Ca {
    pem: String,
    issuer: Issuer<'static, KeyPair>,
}

impl Ca {
    fn new(name: &str) -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, name);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let pem = params.self_signed(&key).unwrap().pem();
        Self {
            pem,
            issuer: Issuer::new(params, key),
        }
    }

    /// Issues a leaf certificate; returns (cert PEM, key PEM).
    fn issue(&self, sans: &[&str], usage: ExtendedKeyUsagePurpose, expired: bool) -> Leaf {
        let key = KeyPair::generate().unwrap();
        let mut params =
            CertificateParams::new(sans.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
                .unwrap();
        params.distinguished_name.push(
            DnType::CommonName,
            sans.first().copied().unwrap_or("client"),
        );
        params.extended_key_usages = vec![usage];
        if expired {
            params.not_before = rcgen::date_time_ymd(2001, 1, 1);
            params.not_after = rcgen::date_time_ymd(2002, 1, 1);
        }
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        Leaf {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        }
    }
}

struct Leaf {
    cert_pem: String,
    key_pem: String,
}

/// SANs of the fixture server certificate (mirrors tests/fixtures.toml [tls]).
const SERVER_SANS: &[&str] = &["localhost", "127.0.0.1", "postgres", "mysql", "mariadb"];

fn server_config(leaf: &Leaf, client_ca: Option<&Ca>) -> Arc<ServerConfig> {
    let chain = CertificateDer::pem_slice_iter(leaf.cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_slice(leaf.key_pem.as_bytes()).unwrap();
    let builder = ServerConfig::builder_with_provider(tls::crypto_provider())
        .with_safe_default_protocol_versions()
        .unwrap();
    let builder = match client_ca {
        None => builder.with_no_client_auth(),
        Some(ca) => {
            let mut roots = RootCertStore::empty();
            for c in CertificateDer::pem_slice_iter(ca.pem.as_bytes()) {
                roots.add(c.unwrap()).unwrap();
            }
            let verifier = WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                tls::crypto_provider(),
            )
            .build()
            .unwrap();
            builder.with_client_cert_verifier(verifier)
        }
    };
    Arc::new(builder.with_single_cert(chain, key).unwrap())
}

/// Runs a full handshake plus a 2-byte application read; `Err` carries the client-side error.
async fn handshake(
    client: Arc<ClientConfig>,
    name: ServerName<'static>,
    server: Arc<ServerConfig>,
) -> Result<(), String> {
    let (c, s) = tokio::io::duplex(64 * 1024);
    let acceptor = TlsAcceptor::from(server);
    let server_task = tokio::spawn(async move {
        let mut tls = acceptor.accept(s).await?;
        tls.write_all(b"ok").await?;
        tls.shutdown().await?;
        Ok::<_, io::Error>(())
    });
    let client_result = async {
        let mut tls = TlsConnector::from(client).connect(name, c).await?;
        let mut buf = [0u8; 2];
        tls.read_exact(&mut buf).await?;
        assert_eq!(&buf, b"ok");
        Ok::<_, io::Error>(())
    }
    .await;
    let _server_result = server_task.await;
    client_result.map_err(|e| e.to_string())
}

fn name(opts: &TlsOptions, host: &str) -> ServerName<'static> {
    tls::server_name(opts, host).unwrap()
}

fn ca_file(dir: &tempfile::TempDir, ca: &Ca) -> PemSource {
    let path = dir.path().join("ca.pem");
    std::fs::write(&path, &ca.pem).unwrap();
    PemSource::File(path)
}

#[tokio::test]
async fn verify_full_checks_chain_and_name() {
    let dir = tempfile::tempdir().unwrap();
    let ca = Ca::new("Oxbase Test CA");
    let server = server_config(
        &ca.issue(SERVER_SANS, ExtendedKeyUsagePurpose::ServerAuth, false),
        None,
    );
    let opts = TlsOptions::new(TlsMode::VerifyFull).with_ca_cert(ca_file(&dir, &ca));
    let cfg = tls::client_config(&opts).unwrap();

    for host in ["localhost", "127.0.0.1", "postgres"] {
        handshake(cfg.clone(), name(&opts, host), server.clone())
            .await
            .unwrap_or_else(|e| panic!("{host}: {e}"));
    }
    let err = handshake(cfg.clone(), name(&opts, "db.example.com"), server.clone())
        .await
        .unwrap_err();
    assert!(err.to_lowercase().contains("name"), "{err}");

    // A different CA is not trusted.
    let other = TlsOptions::new(TlsMode::VerifyFull)
        .with_ca_cert(PemSource::Pem(Ca::new("Other CA").pem.clone()));
    let err = handshake(
        tls::client_config(&other).unwrap(),
        name(&other, "localhost"),
        server,
    )
    .await
    .unwrap_err();
    assert!(err.to_lowercase().contains("issuer"), "{err}");
}

#[tokio::test]
async fn verify_full_through_tunnel_uses_server_host_not_dial_address() {
    // Through SSH the driver dials 127.0.0.1:<tunnel port>; the certificate only names the
    // server host. Verifying the server host (or an explicit server_name) must succeed and
    // verifying the dial address must fail.
    let ca = Ca::new("Oxbase Test CA");
    let server = server_config(
        &ca.issue(&["db.internal"], ExtendedKeyUsagePurpose::ServerAuth, false),
        None,
    );
    let opts = TlsOptions::new(TlsMode::VerifyFull).with_ca_cert(PemSource::Pem(ca.pem.clone()));
    let cfg = tls::client_config(&opts).unwrap();
    handshake(cfg.clone(), name(&opts, "db.internal"), server.clone())
        .await
        .unwrap();
    assert!(
        handshake(cfg.clone(), name(&opts, "127.0.0.1"), server.clone())
            .await
            .is_err()
    );
    // Server reached by IP but the certificate names a host: use the server_name override.
    let by_ip = opts.clone().with_server_name("db.internal");
    handshake(cfg, name(&by_ip, "10.1.2.3"), server)
        .await
        .unwrap();
}

#[tokio::test]
async fn verify_ca_checks_chain_but_not_name() {
    let ca = Ca::new("Oxbase Test CA");
    let good = server_config(
        &ca.issue(&["localhost"], ExtendedKeyUsagePurpose::ServerAuth, false),
        None,
    );
    let opts = TlsOptions::new(TlsMode::VerifyCa).with_ca_cert(PemSource::Pem(ca.pem.clone()));
    let cfg = tls::client_config(&opts).unwrap();
    handshake(cfg.clone(), name(&opts, "anything.example"), good.clone())
        .await
        .unwrap();

    // Unknown CA: rejected.
    let stranger = Ca::new("Stranger CA");
    let foreign = server_config(
        &stranger.issue(&["localhost"], ExtendedKeyUsagePurpose::ServerAuth, false),
        None,
    );
    assert!(
        handshake(cfg.clone(), name(&opts, "localhost"), foreign)
            .await
            .is_err()
    );

    // Expired leaf from the trusted CA: rejected (dates are part of the chain check).
    let expired = server_config(
        &ca.issue(&["localhost"], ExtendedKeyUsagePurpose::ServerAuth, true),
        None,
    );
    let err = handshake(cfg, name(&opts, "localhost"), expired)
        .await
        .unwrap_err();
    assert!(err.to_lowercase().contains("expired"), "{err}");
}

#[tokio::test]
async fn require_and_prefer_encrypt_without_verifying() {
    // Self-signed by an unknown CA, expired, wrong name: still accepted (insecure modes).
    let stranger = Ca::new("Stranger CA");
    let server = server_config(
        &stranger.issue(&["elsewhere"], ExtendedKeyUsagePurpose::ServerAuth, true),
        None,
    );
    for mode in [TlsMode::Require, TlsMode::Prefer] {
        // A CA file does not turn Require into VerifyCa.
        let opts = TlsOptions::new(mode).with_ca_cert(PemSource::Pem(Ca::new("Unrelated").pem));
        handshake(
            tls::client_config(&opts).unwrap(),
            name(&opts, "localhost"),
            server.clone(),
        )
        .await
        .unwrap_or_else(|e| panic!("{mode}: {e}"));
    }
}

#[tokio::test]
async fn verify_full_without_ca_uses_platform_roots() {
    // Our private CA is not in the platform store, so the handshake must fail. (On a host with
    // an empty store, building the config already fails; both are a rejection.)
    let ca = Ca::new("Oxbase Test CA");
    let server = server_config(
        &ca.issue(&["localhost"], ExtendedKeyUsagePurpose::ServerAuth, false),
        None,
    );
    let opts = TlsOptions::new(TlsMode::VerifyFull);
    match tls::client_config(&opts) {
        Ok(cfg) => assert!(
            handshake(cfg, name(&opts, "localhost"), server)
                .await
                .is_err()
        ),
        Err(e) => assert_eq!(e.category(), ErrorCategory::InvalidInput),
    }
}

#[tokio::test]
async fn mutual_tls_with_inline_and_file_keys() {
    let dir = tempfile::tempdir().unwrap();
    let ca = Ca::new("Oxbase Test CA");
    let server = server_config(
        &ca.issue(&["localhost"], ExtendedKeyUsagePurpose::ServerAuth, false),
        Some(&ca),
    );
    let client = ca.issue(&["oxbase_tls"], ExtendedKeyUsagePurpose::ClientAuth, false);
    let key_path = dir.path().join("client-key.pem");
    std::fs::write(&key_path, &client.key_pem).unwrap();

    let base = TlsOptions::new(TlsMode::VerifyFull).with_ca_cert(PemSource::Pem(ca.pem.clone()));
    for key in [
        KeySource::Pem(Secret::new(client.key_pem.clone())),
        KeySource::File(key_path),
    ] {
        let opts = base
            .clone()
            .with_client_identity(PemSource::Pem(client.cert_pem.clone()), key);
        handshake(
            tls::client_config(&opts).unwrap(),
            name(&opts, "localhost"),
            server.clone(),
        )
        .await
        .unwrap();
    }
    // Without a client certificate the server rejects us.
    assert!(
        handshake(
            tls::client_config(&base).unwrap(),
            name(&base, "localhost"),
            server
        )
        .await
        .is_err()
    );
}

#[test]
fn configuration_errors_are_typed_and_leak_nothing() {
    let disabled = tls::client_config(&TlsOptions::new(TlsMode::Disable)).unwrap_err();
    assert_eq!(disabled.category(), ErrorCategory::InvalidInput);

    let mut half = TlsOptions::new(TlsMode::Require);
    half.client_cert = Some(PemSource::Pem("x".into()));
    assert_eq!(
        tls::client_config(&half).unwrap_err().category(),
        ErrorCategory::InvalidInput
    );

    let missing = TlsOptions::new(TlsMode::VerifyCa)
        .with_ca_cert(PemSource::File("/nonexistent/oxbase/ca.pem".into()));
    let err = tls::client_config(&missing).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Io);
    assert!(
        err.to_string().contains("/nonexistent/oxbase/ca.pem"),
        "{err}"
    );

    let not_pem = TlsOptions::new(TlsMode::VerifyCa).with_ca_cert(PemSource::Pem("garbage".into()));
    assert_eq!(
        tls::client_config(&not_pem).unwrap_err().category(),
        ErrorCategory::InvalidInput
    );

    let ca = Ca::new("CA");
    let client = ca.issue(&["c"], ExtendedKeyUsagePurpose::ClientAuth, false);
    let secret_text =
        "-----BEGIN PRIVATE KEY-----\nTOP-SECRET-KEY-MATERIAL\n-----END PRIVATE KEY-----\n";
    let bad_key = TlsOptions::new(TlsMode::Require).with_client_identity(
        PemSource::Pem(client.cert_pem),
        KeySource::Pem(Secret::new(secret_text)),
    );
    let err = tls::client_config(&bad_key).unwrap_err();
    let shown = format!("{err} {err:?} {bad_key:?}");
    assert!(!shown.contains("TOP-SECRET"), "{shown}");

    // VerifyFull needs a usable name; other modes fall back to a placeholder.
    let full = TlsOptions::new(TlsMode::VerifyFull);
    assert!(tls::server_name(&full, "bad name!").is_err());
    assert!(tls::server_name(&TlsOptions::new(TlsMode::Require), "bad name!").is_ok());
    assert!(tls::server_name(&full, "[::1]").is_ok());
}

#[test]
fn default_provider_install_is_idempotent() {
    tls::install_default_crypto_provider();
    tls::install_default_crypto_provider();
    // A library calling the provider-less builder must not panic.
    let _cfg = ClientConfig::builder()
        .with_root_certificates(RootCertStore::empty())
        .with_no_client_auth();
}
