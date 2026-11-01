//! oxbase's own TLS for TDS (driver stream): the PRELOGIN exchange, the TLS handshake wrapped in
//! TDS PRELOGIN packets (TDS 7.4) or directly on TCP (TDS 8.0 strict, ALPN `tds/8.0`), and the
//! [`Transport`] handed to [`super::wire`]. Rules: the [`crate::driver`] module docs
//! (*Transport*) and `docs/m4-contracts.md` §2.2. Written from the public MS-TDS specification
//! (sections 2.2.6.5 PRELOGIN and 3.2.5.2 / 3.3.5.2 TLS negotiation); no code from other clients.
//!
//! Unit tests (no server): the `TlsMode` / server-answer table, and the packet wrapping of
//! handshake records over an in-memory duplex stream.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use oxbase_net::tls::rustls::ClientConfig;
use oxbase_net::tls::rustls::pki_types::ServerName;
use oxbase_types::{Error, Result, TlsMode};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use super::prelogin::{
    self, ClientPrelogin, Encryption, HEADER_LEN, Header, ServerPrelogin, TYPE_PRELOGIN,
    TYPE_TABULAR, invalid,
};

/// ALPN protocol of TDS 8.0 strict encryption.
pub(crate) const ALPN_TDS8: &[u8] = b"tds/8.0";

/// What the PRELOGIN `ENCRYPTION` option says for `mode` (TDS 7.4 negotiation).
pub(crate) fn client_encryption(mode: TlsMode) -> Encryption {
    match mode {
        TlsMode::Disable => Encryption::NotSupported,
        TlsMode::Prefer | TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull => {
            Encryption::On
        }
    }
}

/// Whether the session runs TLS, given the client's mode and the server's PRELOGIN answer
/// (the table in the [`crate::driver`] module docs).
pub(crate) fn use_tls(mode: TlsMode, server: Encryption) -> Result<bool> {
    let server_can = server != Encryption::NotSupported;
    match mode {
        TlsMode::Disable => {
            if matches!(server, Encryption::Required | Encryption::Strict) {
                Err(Error::connection(
                    "the server requires encryption; choose a TLS mode other than \"disable\"",
                ))
            } else {
                Ok(false)
            }
        }
        TlsMode::Prefer => Ok(server_can),
        TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull => {
            if server_can {
                Ok(true)
            } else {
                Err(Error::connection(
                    "the server does not support encryption (TLS), which the connection's TLS \
                     mode requires",
                ))
            }
        }
    }
}

/// How the transport is secured.
pub(crate) struct TlsPlan {
    pub mode: TlsMode,
    /// TDS 8.0 strict: TLS first, then PRELOGIN inside it.
    pub strict: bool,
    /// The rustls configuration (ALPN already set for strict); `None` only for `Disable`.
    pub config: Option<Arc<ClientConfig>>,
    /// Name verified and sent as SNI: the server name, never the dial address.
    pub server_name: ServerName<'static>,
    /// Named instance for `INSTOPT`.
    pub instance: Option<String>,
}

/// The socket after PRELOGIN (and TLS, if negotiated).
pub(crate) enum Transport {
    Plain(TcpStream),
    Tls(Box<TlsStream<HandshakeIo<TcpStream>>>),
}

/// The result of [`negotiate`].
pub(crate) struct Negotiated {
    pub transport: Transport,
    pub server: ServerPrelogin,
    pub tls: bool,
}

fn io_error(e: &io::Error) -> Error {
    if e.kind() == io::ErrorKind::TimedOut {
        Error::timeout(e.to_string())
    } else {
        Error::connection(format!("PRELOGIN: {e}"))
    }
}

fn tls_error(e: &io::Error) -> Error {
    Error::connection(format!("TLS handshake failed: {e}"))
}

fn check_instance(plan: &TlsPlan, server: &ServerPrelogin) -> Result<()> {
    match &plan.instance {
        Some(name) if !server.instance_ok => Err(Error::connection(format!(
            "the server is not instance '{name}' (PRELOGIN INSTOPT rejected it)"
        ))),
        _ => Ok(()),
    }
}

/// Runs PRELOGIN (and the TLS handshake when negotiated or strict) on `tcp`.
pub(crate) async fn negotiate(tcp: TcpStream, plan: &TlsPlan) -> Result<Negotiated> {
    let thread_id = std::process::id();
    if plan.strict {
        let config = plan
            .config
            .clone()
            .ok_or_else(|| Error::internal("strict encryption without a TLS configuration"))?;
        let mut tls = TlsConnector::from(config)
            .connect(plan.server_name.clone(), HandshakeIo::passthrough(tcp))
            .await
            .map_err(|e| tls_error(&e))?;
        let msg = ClientPrelogin {
            encryption: Encryption::Required,
            instance: plan.instance.clone(),
            thread_id,
        };
        let server = prelogin::exchange(&mut tls, &msg)
            .await
            .map_err(|e| io_error(&e))?;
        check_instance(plan, &server)?;
        return Ok(Negotiated {
            transport: Transport::Tls(Box::new(tls)),
            server,
            tls: true,
        });
    }

    let mut tcp = tcp;
    let msg = ClientPrelogin {
        encryption: client_encryption(plan.mode),
        instance: plan.instance.clone(),
        thread_id,
    };
    let server = prelogin::exchange(&mut tcp, &msg)
        .await
        .map_err(|e| io_error(&e))?;
    check_instance(plan, &server)?;
    if !use_tls(plan.mode, server.encryption)? {
        return Ok(Negotiated {
            transport: Transport::Plain(tcp),
            server,
            tls: false,
        });
    }
    let config = plan
        .config
        .clone()
        .ok_or_else(|| Error::internal("TLS negotiated without a TLS configuration"))?;
    let mut tls = TlsConnector::from(config)
        .connect(plan.server_name.clone(), HandshakeIo::new(tcp))
        .await
        .map_err(|e| tls_error(&e))?;
    tls.get_mut()
        .0
        .finish_handshake()
        .map_err(|e| tls_error(&e))?;
    Ok(Negotiated {
        transport: Transport::Tls(Box::new(tls)),
        server,
        tls: true,
    })
}

/// The I/O under the TLS stream. While `handshaking`, every flush of TLS records is sent as one
/// PRELOGIN message (packets of type `0x12`) and the server's packets are unwrapped; afterwards
/// (and from the start in strict mode) bytes pass through unchanged.
pub(crate) struct HandshakeIo<S> {
    inner: S,
    handshaking: bool,
    rhdr: [u8; HEADER_LEN],
    rhdr_len: usize,
    rremaining: usize,
    wpending: Vec<u8>,
    wout: Vec<u8>,
    wpos: usize,
    scratch: Vec<u8>,
}

impl<S> HandshakeIo<S> {
    /// Wraps the handshake in PRELOGIN packets (TDS 7.4).
    pub(crate) fn new(inner: S) -> Self {
        Self {
            inner,
            handshaking: true,
            rhdr: [0; HEADER_LEN],
            rhdr_len: 0,
            rremaining: 0,
            wpending: Vec::new(),
            wout: Vec::new(),
            wpos: 0,
            scratch: vec![0; 16 * 1024],
        }
    }

    /// Plain pass-through (TDS 8.0 strict: TLS directly on TCP).
    pub(crate) fn passthrough(inner: S) -> Self {
        let mut io = Self::new(inner);
        io.handshaking = false;
        io.scratch = Vec::new();
        io
    }

    /// Switches to pass-through once the TLS handshake is complete. Fails when part of a
    /// PRELOGIN packet is still unread or unsent.
    pub(crate) fn finish_handshake(&mut self) -> io::Result<()> {
        if self.rremaining != 0
            || self.rhdr_len != 0
            || !self.wpending.is_empty()
            || self.wpos < self.wout.len()
        {
            return Err(invalid("the TLS handshake ended inside a PRELOGIN packet"));
        }
        self.handshaking = false;
        self.scratch = Vec::new();
        Ok(())
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for HandshakeIo<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.handshaking {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        }
        loop {
            if this.rremaining == 0 {
                while this.rhdr_len < HEADER_LEN {
                    let mut rb = ReadBuf::new(&mut this.rhdr[this.rhdr_len..]);
                    ready!(Pin::new(&mut this.inner).poll_read(cx, &mut rb))?;
                    let n = rb.filled().len();
                    if n == 0 {
                        // EOF between packets: rustls reports the failed handshake.
                        return Poll::Ready(Ok(()));
                    }
                    this.rhdr_len += n;
                }
                let header = Header::parse(&this.rhdr)?;
                this.rhdr_len = 0;
                if header.ty != TYPE_PRELOGIN && header.ty != TYPE_TABULAR {
                    return Poll::Ready(Err(invalid(
                        "unexpected TDS packet during the TLS handshake",
                    )));
                }
                this.rremaining = header.payload_len();
                continue;
            }
            let want = this.rremaining.min(buf.remaining()).min(this.scratch.len());
            if want == 0 {
                return Poll::Ready(Ok(()));
            }
            let mut rb = ReadBuf::new(&mut this.scratch[..want]);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut rb))?;
            let n = rb.filled().len();
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
            }
            buf.put_slice(&this.scratch[..n]);
            this.rremaining -= n;
            return Poll::Ready(Ok(()));
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for HandshakeIo<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.handshaking {
            this.wpending.extend_from_slice(buf);
            return Poll::Ready(Ok(buf.len()));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.wpos >= this.wout.len() && !this.wpending.is_empty() {
            this.wout = prelogin::frame(TYPE_PRELOGIN, &this.wpending);
            this.wpending.clear();
            this.wpos = 0;
        }
        while this.wpos < this.wout.len() {
            let n = ready!(Pin::new(&mut this.inner).poll_write(cx, &this.wout[this.wpos..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            this.wpos += n;
        }
        this.wout.clear();
        this.wpos = 0;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl AsyncRead for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn mode_table() {
        use Encryption::{NotSupported, Off, On, Required};
        assert_eq!(client_encryption(TlsMode::Disable), NotSupported);
        for mode in [
            TlsMode::Prefer,
            TlsMode::Require,
            TlsMode::VerifyCa,
            TlsMode::VerifyFull,
        ] {
            assert_eq!(client_encryption(mode), On);
        }

        // Disable: plaintext unless the server requires encryption.
        assert!(!use_tls(TlsMode::Disable, NotSupported).unwrap());
        assert!(!use_tls(TlsMode::Disable, Off).unwrap());
        assert!(!use_tls(TlsMode::Disable, On).unwrap());
        let err = use_tls(TlsMode::Disable, Required).unwrap_err();
        assert!(err.to_string().contains("requires encryption"), "{err}");

        // Prefer: falls back to plaintext only when the server cannot encrypt.
        assert!(!use_tls(TlsMode::Prefer, NotSupported).unwrap());
        for server in [Off, On, Required] {
            assert!(use_tls(TlsMode::Prefer, server).unwrap());
        }

        // Require / VerifyCa / VerifyFull: TLS or nothing.
        for mode in [TlsMode::Require, TlsMode::VerifyCa, TlsMode::VerifyFull] {
            assert!(matches!(
                use_tls(mode, NotSupported),
                Err(Error::Connection { .. })
            ));
            for server in [Off, On, Required] {
                assert!(use_tls(mode, server).unwrap());
            }
        }
    }

    #[tokio::test]
    async fn handshake_records_are_wrapped_in_prelogin_packets() {
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        let mut io = HandshakeIo::new(client);

        // Two writes then one flush: one PRELOGIN message.
        io.write_all(b"hello ").await.unwrap();
        io.write_all(b"tls").await.unwrap();
        io.flush().await.unwrap();
        let mut hdr = [0u8; HEADER_LEN];
        server.read_exact(&mut hdr).await.unwrap();
        let h = Header::parse(&hdr).unwrap();
        assert_eq!(h.ty, TYPE_PRELOGIN);
        assert!(h.is_eom());
        let mut body = vec![0u8; h.payload_len()];
        server.read_exact(&mut body).await.unwrap();
        assert_eq!(body, b"hello tls");

        // The server's records arrive in packets; the TLS layer sees only payload bytes.
        let mut reply = prelogin::frame(TYPE_PRELOGIN, b"server-hello");
        reply.extend(prelogin::frame(TYPE_TABULAR, b"-finished"));
        server.write_all(&reply).await.unwrap();
        let mut got = vec![0u8; "server-hello-finished".len()];
        io.read_exact(&mut got).await.unwrap();
        assert_eq!(got, b"server-hello-finished");

        // After the handshake: pass-through both ways.
        io.finish_handshake().unwrap();
        io.write_all(b"raw").await.unwrap();
        io.flush().await.unwrap();
        let mut raw = [0u8; 3];
        server.read_exact(&mut raw).await.unwrap();
        assert_eq!(&raw, b"raw");
        server.write_all(b"back").await.unwrap();
        let mut back = [0u8; 4];
        io.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"back");
    }

    #[tokio::test]
    async fn finishing_inside_a_packet_is_an_error() {
        let (client, mut server) = tokio::io::duplex(1024);
        let mut io = HandshakeIo::new(client);
        server
            .write_all(&prelogin::frame(TYPE_PRELOGIN, b"abcdef"))
            .await
            .unwrap();
        let mut two = [0u8; 2];
        io.read_exact(&mut two).await.unwrap();
        assert!(io.finish_handshake().is_err());
        let mut rest = [0u8; 4];
        io.read_exact(&mut rest).await.unwrap();
        io.finish_handshake().unwrap();

        // Another packet type during the handshake is refused.
        let (client, mut server) = tokio::io::duplex(1024);
        let mut io = HandshakeIo::new(client);
        server
            .write_all(&prelogin::frame(0x01, b"x"))
            .await
            .unwrap();
        let mut one = [0u8; 1];
        assert!(io.read_exact(&mut one).await.is_err());
    }
}
