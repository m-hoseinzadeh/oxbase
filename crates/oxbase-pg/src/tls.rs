//! `tokio-postgres` TLS connector over `tokio-postgres-rustls`, with the oxbase rules:
//!
//! - the rustls configuration comes from `oxbase_net::tls::client_config` (ring, the mode's
//!   verifier, CA / client identity);
//! - the name verified and sent as SNI is [`ConnectParams::tls_server_name`] (the server host or
//!   `tls.server_name`), never the address `tokio-postgres` dials, so `VerifyFull` works through
//!   an SSH tunnel; the `domain` tokio-postgres passes in is ignored;
//! - a flag records whether a handshake actually happened (`Prefer` may fall back to plaintext).

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use oxbase_driver::{ConnectParams, TlsMode};
use oxbase_types::{Error, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_postgres::config::{SslMode, SslNegotiation};
use tokio_postgres::tls::{MakeTlsConnect, TlsConnect};
use tokio_postgres_rustls::MakeRustlsConnect;

/// ALPN protocol PostgreSQL 17+ requires for direct TLS (`sslnegotiation=direct`).
const ALPN_POSTGRESQL: &[u8] = b"postgresql";

/// The TLS connector for one connection (and its cancel requests).
#[derive(Clone)]
pub(crate) struct PgTls {
    /// `None` for [`TlsMode::Disable`]; tokio-postgres then never calls `connect`.
    inner: Option<MakeRustlsConnect>,
    server_name: String,
    used: Option<Arc<AtomicBool>>,
}

impl PgTls {
    /// Builds the connector for `params`. `direct` adds the ALPN protocol for direct TLS.
    pub(crate) fn new(params: &ConnectParams, direct: bool) -> Result<Self> {
        if params.tls.mode == TlsMode::Disable {
            return Ok(Self {
                inner: None,
                server_name: String::new(),
                used: None,
            });
        }
        let host = params
            .server_endpoint()
            .map(|e| e.host)
            .ok_or_else(|| Error::invalid_input("PostgreSQL needs a host"))?;
        let name = oxbase_net::tls::server_name(&params.tls, &host)?;
        let mut config = (*oxbase_net::tls::client_config(&params.tls)?).clone();
        if direct {
            config.alpn_protocols = vec![ALPN_POSTGRESQL.to_vec()];
        }
        Ok(Self {
            inner: Some(MakeRustlsConnect::new(config)),
            server_name: name.to_str().into_owned(),
            used: None,
        })
    }

    /// A copy that records a successful handshake in the returned flag.
    pub(crate) fn tracked(&self) -> (Self, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        let mut this = self.clone();
        this.used = Some(flag.clone());
        (this, flag)
    }
}

/// The `tokio-postgres` SSL mode for an oxbase [`TlsMode`]. The certificate checks of the
/// verifying modes are done by the rustls configuration.
pub(crate) fn ssl_mode(mode: TlsMode) -> SslMode {
    match mode {
        TlsMode::Disable => SslMode::Disable,
        TlsMode::Prefer => SslMode::Prefer,
        TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull => SslMode::Require,
    }
}

/// Parses the `sslnegotiation` option (`postgres`, the default, or `direct`).
pub(crate) fn ssl_negotiation(value: Option<&str>, mode: TlsMode) -> Result<SslNegotiation> {
    match value.map(str::trim) {
        None | Some("" | "postgres") => Ok(SslNegotiation::Postgres),
        Some("direct") if mode.requires_tls() => Ok(SslNegotiation::Direct),
        Some("direct") => Err(Error::invalid_input(
            "sslnegotiation=direct needs a TLS mode that requires TLS (require, verify_ca or \
             verify_full)",
        )),
        Some(other) => Err(Error::invalid_input(format!(
            "unknown sslnegotiation '{other}' (expected postgres or direct)"
        ))),
    }
}

type InnerConnect<S> = <MakeRustlsConnect as MakeTlsConnect<S>>::TlsConnect;
type InnerStream<S> = <MakeRustlsConnect as MakeTlsConnect<S>>::Stream;

impl<S> MakeTlsConnect<S> for PgTls
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = InnerStream<S>;
    type TlsConnect = PgTlsConnect<S>;
    type Error = io::Error;

    fn make_tls_connect(&mut self, _domain: &str) -> io::Result<PgTlsConnect<S>> {
        let inner = match &mut self.inner {
            Some(make) => Some(
                <MakeRustlsConnect as MakeTlsConnect<S>>::make_tls_connect(make, &self.server_name)
                    .map_err(|never: std::convert::Infallible| -> io::Error { match never {} })?,
            ),
            None => None,
        };
        Ok(PgTlsConnect {
            inner,
            used: self.used.clone(),
        })
    }
}

pub(crate) struct PgTlsConnect<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    inner: Option<InnerConnect<S>>,
    used: Option<Arc<AtomicBool>>,
}

impl<S> TlsConnect<S> for PgTlsConnect<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = InnerStream<S>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<InnerStream<S>>> + Send>>;

    fn connect(self, stream: S) -> Self::Future {
        let Some(inner) = self.inner else {
            return Box::pin(std::future::ready(Err(io::Error::other(
                "TLS is disabled for this connection",
            ))));
        };
        let handshake = inner.connect(stream);
        let used = self.used;
        Box::pin(async move {
            let stream = handshake.await?;
            if let Some(flag) = used {
                flag.store(true, Ordering::Release);
            }
            Ok(stream)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_driver::TlsOptions;
    use oxbase_types::Engine;

    #[test]
    fn modes_and_negotiation() {
        assert_eq!(ssl_mode(TlsMode::Disable), SslMode::Disable);
        assert_eq!(ssl_mode(TlsMode::Prefer), SslMode::Prefer);
        assert_eq!(ssl_mode(TlsMode::VerifyFull), SslMode::Require);
        assert_eq!(
            ssl_negotiation(None, TlsMode::Prefer).unwrap(),
            SslNegotiation::Postgres
        );
        assert_eq!(
            ssl_negotiation(Some("direct"), TlsMode::Require).unwrap(),
            SslNegotiation::Direct
        );
        assert!(ssl_negotiation(Some("direct"), TlsMode::Prefer).is_err());
        assert!(ssl_negotiation(Some("bogus"), TlsMode::Require).is_err());
    }

    #[test]
    fn verifies_the_server_name_not_the_dial_address() {
        let params = ConnectParams::network(Engine::Postgres, "db.internal")
            .with_tls(TlsOptions::new(TlsMode::Require))
            .with_dial_addr("127.0.0.1:40000".parse().unwrap());
        let tls = PgTls::new(&params, false).unwrap();
        assert_eq!(tls.server_name, "db.internal");
        let params =
            params.with_tls(TlsOptions::new(TlsMode::Require).with_server_name("pg.example"));
        assert_eq!(PgTls::new(&params, true).unwrap().server_name, "pg.example");
        let off = params.with_tls(TlsOptions::new(TlsMode::Disable));
        assert!(PgTls::new(&off, false).unwrap().inner.is_none());
    }
}
