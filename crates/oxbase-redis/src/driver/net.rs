//! Sockets of a session: TCP to the dial address, then TLS with `oxbase_net::tls` verifying the
//! server name (M1 §2-3). Every connection of a session (the multiplexed one, cancel side
//! connections, dedicated pub/sub connections, sentinel queries) is opened here.

use std::time::Duration;

use oxbase_types::{Endpoint, Error, Result, TlsMode, TlsOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_util::either::Either;

/// A session socket: plain TCP or TLS over TCP.
pub(crate) type NetStream = Either<TcpStream, TlsStream<TcpStream>>;

/// How long the `Prefer` probe waits for the server's first bytes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Where one server is reached and what its TLS certificate must say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Address {
    /// Where the socket is opened (the SSH tunnel's local end, or the server itself).
    pub dial: Endpoint,
    /// The server endpoint as configured (messages; the TLS name unless `tls.server_name`).
    pub server: Endpoint,
}

impl Address {
    /// A server dialled directly (cluster nodes, sentinel masters, sentinels).
    pub(crate) fn direct(server: Endpoint) -> Self {
        Self {
            dial: server.clone(),
            server,
        }
    }

    fn describe(&self) -> String {
        if self.dial == self.server {
            self.server.to_string()
        } else {
            format!("{} (through {})", self.server, self.dial)
        }
    }
}

/// An open socket and whether it is encrypted.
pub(crate) struct Dialed {
    pub stream: NetStream,
    pub tls: bool,
}

/// Opens a socket to `addr` honouring `tls` (the mode table of the driver module docs).
pub(crate) async fn dial(addr: &Address, tls: &TlsOptions) -> Result<Dialed> {
    match tls.mode {
        TlsMode::Disable => Ok(Dialed {
            stream: Either::Left(tcp(addr).await?),
            tls: false,
        }),
        TlsMode::Prefer => {
            // Redis has no STARTTLS: a port speaks either RESP or TLS. An inline PING tells them
            // apart without a TLS handshake that a plaintext server would never answer.
            let mut probe = tcp(addr).await?;
            if speaks_resp(&mut probe).await {
                return Ok(Dialed {
                    stream: Either::Left(probe),
                    tls: false,
                });
            }
            drop(probe);
            handshake(addr, tls).await
        }
        TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull => handshake(addr, tls).await,
    }
}

/// `true` when the server answers an inline `PING` with a RESP status or error line (a
/// plaintext Redis port: `+PONG` or `-NOAUTH ...`). The whole reply line is read, so the
/// socket is ready for the session's handshake.
pub(crate) async fn speaks_resp(stream: &mut TcpStream) -> bool {
    let probe = async {
        stream.write_all(b"PING\r\n").await.ok()?;
        let mut line = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            let n = stream.read(&mut buf).await.ok()?;
            if n == 0 {
                return None;
            }
            line.extend_from_slice(&buf[..n]);
            if !matches!(line.first(), Some(b'+' | b'-')) {
                return Some(false);
            }
            if line.ends_with(b"\r\n") {
                return Some(true);
            }
            if line.len() > 4096 {
                return Some(false);
            }
        }
    };
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, probe).await,
        Ok(Some(true))
    )
}

/// A TCP connection to `addr.dial` (`TCP_NODELAY` on).
pub(crate) async fn tcp(addr: &Address) -> Result<TcpStream> {
    let stream = TcpStream::connect((addr.dial.host.as_str(), addr.dial.port))
        .await
        .map_err(|e| Error::connection(format!("cannot connect to {}: {e}", addr.describe())))?;
    stream
        .set_nodelay(true)
        .map_err(|e| Error::connection(format!("cannot configure the socket: {e}")))?;
    Ok(stream)
}

/// TCP, then a TLS handshake verifying `tls.server_name`, else the server host.
async fn handshake(addr: &Address, tls: &TlsOptions) -> Result<Dialed> {
    let config = oxbase_net::tls::client_config(tls)?;
    let name = oxbase_net::tls::server_name(tls, &addr.server.host)?;
    let stream = tcp(addr).await?;
    let stream = TlsConnector::from(config)
        .connect(name, stream)
        .await
        .map_err(|e| {
            Error::connection(format!(
                "TLS handshake with {} failed: {e}",
                addr.describe()
            ))
        })?;
    Ok(Dialed {
        stream: Either::Right(stream),
        tls: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn server(reply: &'static [u8]) -> Endpoint {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 6];
            let _ = s.read_exact(&mut buf).await;
            let _ = s.write_all(reply).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        Endpoint::new("127.0.0.1", port)
    }

    #[tokio::test]
    async fn the_prefer_probe_tells_resp_from_tls() {
        for (reply, resp) in [
            (&b"+PONG\r\n"[..], true),
            (&b"-NOAUTH Authentication required.\r\n"[..], true),
            // A TLS alert record.
            (&b"\x15\x03\x01\x00\x02\x02\x46"[..], false),
            (&b""[..], false),
        ] {
            let addr = Address::direct(server(reply).await);
            let mut s = tcp(&addr).await.unwrap();
            assert_eq!(speaks_resp(&mut s).await, resp, "{reply:?}");
        }
    }

    #[tokio::test]
    async fn refused_connections_name_the_server_and_the_tunnel() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let addr = Address {
            dial: Endpoint::new("127.0.0.1", port),
            server: Endpoint::new("redis.internal", 6379),
        };
        let err = tcp(&addr).await.unwrap_err();
        assert!(matches!(err, Error::Connection { .. }), "{err}");
        let text = err.to_string();
        assert!(
            text.contains("redis.internal:6379") && text.contains(&port.to_string()),
            "{text}"
        );
    }
}
