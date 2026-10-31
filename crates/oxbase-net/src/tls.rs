//! TLS client configuration for every driver: rustls with the *ring* provider.
//!
//! [`client_config`] turns [`TlsOptions`] into an `Arc<rustls::ClientConfig>` with these
//! semantics (the same as libpq's `sslmode`, without libpq's "a CA file upgrades `require` to
//! `verify-ca`" quirk):
//!
//! | Mode | Encrypts | Chain verified | Name verified |
//! |---|---|---|---|
//! | [`TlsMode::Disable`] | no (`client_config` returns an error) | - | - |
//! | [`TlsMode::Prefer`] | if the server supports it | **no** | **no** |
//! | [`TlsMode::Require`] | yes | **no** | **no** |
//! | [`TlsMode::VerifyCa`] | yes | yes | no |
//! | [`TlsMode::VerifyFull`] | yes | yes | yes |
//!
//! `Prefer` and `Require` use [`InsecureNoVerification`]: the handshake signatures are still
//! checked, but **any** certificate is accepted, so an active attacker can impersonate the server.
//! The plaintext fallback of `Prefer` is the driver's job (it only knows whether the server
//! offered TLS).
//!
//! Trust roots: [`TlsOptions::ca_cert`] when set (and then *only* those CAs), otherwise the
//! platform store (`rustls-native-certs`, loaded once per process).
//!
//! # Server name rule
//!
//! The name passed to the TLS connector ([`server_name`]) is [`TlsOptions::server_name`] if set,
//! otherwise the **server** host from the connection profile, never the dial/tunnel address.
//! With SSH the driver dials `127.0.0.1:<port>` but still verifies `db.example.com`.
//!
//! # Secrets
//!
//! Inline client keys are [`Secret`]s; key bytes read from files are zeroed after parsing (best
//! effort). Errors name files but never include key material.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{
    CryptoProvider, WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};

use oxbase_types::{Error, Result};
pub use oxbase_types::{KeySource, PemSource, Secret, TlsMode, TlsOptions};

/// Re-export of the rustls version this crate is built against, so drivers use the same one.
pub use rustls;

/// The process-wide crypto provider every oxbase TLS config is built with (ring).
pub fn crypto_provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::ring::default_provider()))
        .clone()
}

/// Installs ring as rustls' process-default provider, if no default is installed yet.
///
/// Needed by client libraries that call `rustls::ClientConfig::builder()` themselves (for
/// example `mysql_async`): that call panics when no default is installed and more than one
/// provider is compiled in. The app, the CLI and every driver's `connect` call this; it is
/// idempotent and cheap.
pub fn install_default_crypto_provider() {
    if CryptoProvider::get_default().is_none() {
        // Losing a race against another installer is fine: some provider is now the default.
        let _already_installed = rustls::crypto::ring::default_provider().install_default();
    }
}

/// Builds the rustls client configuration for `opts` (see the module docs for the semantics).
///
/// The result does not depend on the server name; pass [`server_name`] to the connector. Drivers
/// that need ALPN (PostgreSQL direct TLS: `postgresql`) clone the config and set
/// `alpn_protocols`.
///
/// Errors: [`TlsMode::Disable`] (`InvalidInput`), unreadable files (`Io`), malformed PEM or a
/// client certificate the key does not match (`InvalidInput`), no usable trust roots for a
/// verifying mode (`InvalidInput`).
pub fn client_config(opts: &TlsOptions) -> Result<Arc<ClientConfig>> {
    opts.validate()?;
    let provider = crypto_provider();
    let verifier: Arc<dyn ServerCertVerifier> = match opts.mode {
        TlsMode::Disable => {
            return Err(Error::invalid_input(
                "TLS is disabled for this connection (TlsMode::Disable)",
            ));
        }
        TlsMode::Prefer | TlsMode::Require => Arc::new(InsecureNoVerification::new(&provider)),
        TlsMode::VerifyCa => Arc::new(ChainOnlyVerification {
            inner: webpki_verifier(opts, &provider)?,
        }),
        TlsMode::VerifyFull => webpki_verifier(opts, &provider)?,
    };
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::internal(format!("TLS protocol setup: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    let config = match load_client_identity(opts)? {
        Some((chain, key)) => builder
            .with_client_auth_cert(chain, key)
            .map_err(|e| Error::invalid_input(format!("TLS client certificate/key: {e}")))?,
        None => builder.with_no_client_auth(),
    };
    Ok(Arc::new(config))
}

/// The rustls server name for a connection to `server_host` (the *server* endpoint host, never
/// the tunnel address), honouring [`TlsOptions::server_name`].
///
/// For [`TlsMode::VerifyFull`] an unusable name is an error. Other modes do not check the name,
/// so an unusable one falls back to an IP placeholder (which also means no SNI is sent).
pub fn server_name(opts: &TlsOptions, server_host: &str) -> Result<ServerName<'static>> {
    let name = opts
        .effective_server_name(server_host)
        .trim_start_matches('[')
        .trim_end_matches(']');
    match ServerName::try_from(name) {
        Ok(n) => Ok(n.to_owned()),
        Err(_) if !opts.mode.verifies_hostname() => Ok(ServerName::IpAddress(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED).into(),
        )),
        Err(e) => Err(Error::invalid_input(format!(
            "'{name}' is not a valid TLS server name: {e}"
        ))),
    }
}

/// Reads PEM text from `src` (for client libraries that take PEM bytes, such as `mysql_async`).
pub fn load_pem(src: &PemSource) -> Result<Vec<u8>> {
    match src {
        PemSource::File(path) => read_file(path, "PEM"),
        PemSource::Pem(text) => Ok(text.clone().into_bytes()),
    }
}

/// Reads private key text from `src` as a [`Secret`].
pub fn load_key_pem(src: &KeySource) -> Result<Secret> {
    match src {
        KeySource::Pem(secret) => Ok(secret.clone()),
        KeySource::File(path) => {
            let bytes = read_file(path, "private key")?;
            match String::from_utf8(bytes) {
                Ok(text) => Ok(Secret::new(text)),
                Err(e) => {
                    let mut bytes = e.into_bytes();
                    bytes.fill(0);
                    std::hint::black_box(&bytes);
                    Err(Error::invalid_input(format!(
                        "private key file {} is not PEM text",
                        path.display()
                    )))
                }
            }
        }
    }
}

/// Parses every certificate in `src` (PEM). Empty input is an error.
pub fn load_certificates(src: &PemSource) -> Result<Vec<CertificateDer<'static>>> {
    let pem = load_pem(src)?;
    let certs = CertificateDer::pem_slice_iter(&pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| {
            Error::invalid_input(format!("{}: invalid certificate PEM: {e}", label(src)))
        })?;
    if certs.is_empty() {
        return Err(Error::invalid_input(format!(
            "{}: no certificates found",
            label(src)
        )));
    }
    Ok(certs)
}

/// Parses an unencrypted private key (PKCS#8, PKCS#1 or SEC1 PEM) from `src`.
pub fn load_private_key(src: &KeySource) -> Result<PrivateKeyDer<'static>> {
    let pem = load_key_pem(src)?;
    // The pem error can quote the input, so it is not passed on.
    PrivateKeyDer::from_pem_slice(pem.expose().as_bytes()).map_err(|_| {
        Error::invalid_input(format!(
            "{}: no unencrypted PKCS#8, PKCS#1 or SEC1 private key found (encrypted keys are \
             not supported)",
            match src {
                KeySource::File(p) => format!("client key file {}", p.display()),
                KeySource::Pem(_) => "inline client key".to_owned(),
            }
        ))
    })
}

/// The client certificate chain and key, when mutual TLS is configured.
pub fn load_client_identity(
    opts: &TlsOptions,
) -> Result<Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>> {
    opts.validate()?;
    match (&opts.client_cert, &opts.client_key) {
        (Some(cert), Some(key)) => Ok(Some((load_certificates(cert)?, load_private_key(key)?))),
        _ => Ok(None),
    }
}

/// Trust roots for a verifying mode: the configured CA file, or the platform store.
pub fn root_store(opts: &TlsOptions) -> Result<Arc<RootCertStore>> {
    let Some(ca) = &opts.ca_cert else {
        return platform_roots();
    };
    let mut roots = RootCertStore::empty();
    for cert in load_certificates(ca)? {
        roots
            .add(cert)
            .map_err(|e| Error::invalid_input(format!("{}: unusable CA: {e}", label(ca))))?;
    }
    Ok(Arc::new(roots))
}

/// The platform trust store, loaded once per process.
fn platform_roots() -> Result<Arc<RootCertStore>> {
    static ROOTS: OnceLock<std::result::Result<Arc<RootCertStore>, String>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let loaded = rustls_native_certs::load_native_certs();
            for e in &loaded.errors {
                tracing::warn!(error = %e, "skipping part of the platform certificate store");
            }
            let mut roots = RootCertStore::empty();
            let (added, ignored) = roots.add_parsable_certificates(loaded.certs);
            tracing::debug!(added, ignored, "loaded platform trust roots");
            if roots.is_empty() {
                Err("no trusted CA certificates found in the platform store; set a CA file".into())
            } else {
                Ok(Arc::new(roots))
            }
        })
        .clone()
        .map_err(Error::invalid_input)
}

fn webpki_verifier(
    opts: &TlsOptions,
    provider: &Arc<CryptoProvider>,
) -> Result<Arc<WebPkiServerVerifier>> {
    WebPkiServerVerifier::builder_with_provider(root_store(opts)?, provider.clone())
        .build()
        .map_err(|e| Error::invalid_input(format!("TLS verifier: {e}")))
}

fn read_file(path: &Path, what: &str) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e: io::Error| Error::Io {
        kind: e.kind().to_string(),
        message: format!("cannot read {what} file {}: {e}", path.display()),
    })
}

fn label(src: &PemSource) -> String {
    match src {
        PemSource::File(p) => p.display().to_string(),
        PemSource::Pem(_) => "inline PEM".to_owned(),
    }
}

/// **Insecure** verifier for [`TlsMode::Prefer`] and [`TlsMode::Require`]: accepts any server
/// certificate (expired, self-signed, wrong name, unknown CA). It still checks that the
/// handshake is signed by the presented certificate's key, so the session is encrypted, but the
/// server is not authenticated. Use [`TlsMode::VerifyFull`] for anything remote.
#[derive(Debug)]
pub struct InsecureNoVerification {
    algorithms: WebPkiSupportedAlgorithms,
}

impl InsecureNoVerification {
    pub fn new(provider: &CryptoProvider) -> Self {
        Self {
            algorithms: provider.signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for InsecureNoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Verifier for [`TlsMode::VerifyCa`]: full WebPKI chain validation (trust anchor, validity
/// dates, key usage, revocation data if any), but a certificate that does not match the server
/// name is accepted.
///
/// rustls checks the name *after* the chain, so a name-mismatch error implies the chain passed.
pub struct ChainOnlyVerification {
    inner: Arc<WebPkiServerVerifier>,
}

impl fmt::Debug for ChainOnlyVerification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChainOnlyVerification")
            .finish_non_exhaustive()
    }
}

impl ServerCertVerifier for ChainOnlyVerification {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}
