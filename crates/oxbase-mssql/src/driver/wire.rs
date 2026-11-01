//! The stream tiberius talks to, shared with the connection so it can cancel in-band.
//!
//! [`SharedWire`] wraps the negotiated [`super::tls::Transport`] (or any byte stream in tests)
//! behind a `std::sync::Mutex` that is only held inside one `poll_*` call. It does three things:
//!
//! 1. **PRELOGIN replay.** tiberius always starts with its own PRELOGIN. The wire swallows that
//!    message and answers it with the server's real answer re-encoded with
//!    `ENCRYPTION = ENCRYPT_NOT_SUP` ([`super::prelogin::ServerPrelogin::replay_payload`]):
//!    oxbase already did the PRELOGIN (and TLS) itself, so tiberius goes straight to LOGIN7.
//! 2. **Message accounting.** It follows the TDS packet headers in both directions and counts
//!    client request messages and server response messages (one response per request).
//! 3. **Attention.** [`SharedWire::send_attention`] writes an ATTENTION packet onto the socket
//!    while tiberius is reading the response of the request in flight; tiberius keeps reading
//!    and the response ends at the server's acknowledgement (a `DONE` with the attention bit),
//!    so the reader never drops a half-read token. When the request had already finished
//!    before the server saw the attention, the server sends the acknowledgement as a message
//!    of its own; the wire recognises and swallows it, so it can never be taken for the answer
//!    to a later request.
//!
//! [`SharedWire::kill`] drops the socket; every later read or write fails.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, ready};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::prelogin::{
    ATTENTION_PACKET, HEADER_LEN, Header, MAX_PACKET, TYPE_ATTENTION, TYPE_PRELOGIN, TYPE_TABULAR,
    frame, invalid,
};

/// Length of a `DONE` / `DONEPROC` / `DONEINPROC` token in TDS 7.2+.
const DONE_LEN: usize = 13;
/// `DONE_ATTN` status bit.
const DONE_ATTN: u16 = 0x0020;

fn closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "the connection was closed after a cancel",
    )
}

/// `true` when `token` is a `DONE`-family token with the attention bit.
fn is_attention_done(token: &[u8]) -> bool {
    token.len() == DONE_LEN
        && matches!(token[0], 0xFD..=0xFF)
        && u16::from_le_bytes([token[1], token[2]]) & DONE_ATTN != 0
}

/// `true` when `packet` is a whole message consisting only of an attention acknowledgement.
fn is_attention_ack_packet(packet: &[u8]) -> bool {
    match Header::parse(packet) {
        Ok(h) => {
            h.ty == TYPE_TABULAR
                && h.is_eom()
                && h.len == HEADER_LEN + DONE_LEN
                && is_attention_done(&packet[HEADER_LEN..])
        }
        Err(_) => false,
    }
}

enum Replay {
    /// Collecting tiberius' PRELOGIN; then serve `answer`.
    Swallow {
        written: Vec<u8>,
        answer: Vec<u8>,
    },
    /// Serving the framed answer to tiberius' reads.
    Serve {
        bytes: Vec<u8>,
        pos: usize,
    },
    Done,
}

struct Wire<T> {
    io: Option<T>,
    replay: Replay,
    // Read side.
    rin: Vec<u8>,
    out: Vec<u8>,
    out_pos: usize,
    scratch: Vec<u8>,
    msg_start: bool,
    tail: Vec<u8>,
    server_msgs: u64,
    // Write side.
    whdr: [u8; HEADER_LEN],
    whdr_len: usize,
    wcur: Option<Header>,
    wremaining: usize,
    client_msgs: u64,
    // Attention.
    attn_pos: Option<usize>,
    acks_outstanding: u32,
    attn_target: u64,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Wire<T> {
    fn new(io: T, replay_answer: Option<Vec<u8>>) -> Self {
        Self {
            io: Some(io),
            replay: match replay_answer {
                Some(answer) => Replay::Swallow {
                    written: Vec::new(),
                    answer,
                },
                None => Replay::Done,
            },
            rin: Vec::new(),
            out: Vec::new(),
            out_pos: 0,
            scratch: vec![0; 16 * 1024],
            msg_start: true,
            tail: Vec::with_capacity(DONE_LEN),
            server_msgs: 0,
            whdr: [0; HEADER_LEN],
            whdr_len: 0,
            wcur: None,
            wremaining: 0,
            client_msgs: 0,
            attn_pos: None,
            acks_outstanding: 0,
            attn_target: 0,
        }
    }

    fn io(&mut self) -> io::Result<Pin<&mut T>> {
        self.io.as_mut().map(Pin::new).ok_or_else(closed)
    }

    /// Reads once from the socket and frames what arrived into `out` (see the module docs).
    /// `Ok(0)` is end of stream.
    fn poll_fill(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let n = {
            let io = self.io.as_mut().ok_or_else(closed)?;
            let mut rb = ReadBuf::new(&mut self.scratch);
            ready!(Pin::new(io).poll_read(cx, &mut rb))?;
            rb.filled().len()
        };
        if n == 0 {
            return Poll::Ready(Ok(0));
        }
        self.rin.extend_from_slice(&self.scratch[..n]);
        self.frame_incoming()?;
        Poll::Ready(Ok(n))
    }

    fn frame_incoming(&mut self) -> io::Result<()> {
        let mut start = 0;
        while self.rin.len() - start >= HEADER_LEN {
            let header = Header::parse(&self.rin[start..])?;
            if header.len > MAX_PACKET {
                return Err(invalid("TDS packet larger than the protocol allows"));
            }
            if self.rin.len() - start < header.len {
                break;
            }
            let packet = &self.rin[start..start + header.len];
            if self.msg_start
                && self.acks_outstanding > 0
                && self.server_msgs >= self.attn_target
                && is_attention_ack_packet(packet)
            {
                // A late acknowledgement of an attention whose request had already finished.
                self.acks_outstanding -= 1;
            } else {
                self.out.extend_from_slice(packet);
                let payload = &packet[HEADER_LEN..];
                if payload.len() >= DONE_LEN {
                    self.tail.clear();
                    self.tail
                        .extend_from_slice(&payload[payload.len() - DONE_LEN..]);
                } else {
                    self.tail.extend_from_slice(payload);
                    if self.tail.len() > DONE_LEN {
                        let excess = self.tail.len() - DONE_LEN;
                        self.tail.drain(..excess);
                    }
                }
                if header.is_eom() {
                    self.server_msgs += 1;
                    if is_attention_done(&self.tail) {
                        self.acks_outstanding = self.acks_outstanding.saturating_sub(1);
                    }
                    self.tail.clear();
                    self.msg_start = true;
                } else {
                    self.msg_start = false;
                }
            }
            start += header.len;
        }
        self.rin.drain(..start);
        Ok(())
    }

    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if self.io.is_none() {
            return Poll::Ready(Err(closed()));
        }
        match &mut self.replay {
            Replay::Swallow { .. } => {
                return Poll::Ready(Err(invalid("tiberius read before sending its PRELOGIN")));
            }
            Replay::Serve { bytes, pos } => {
                let n = (bytes.len() - *pos).min(buf.remaining());
                buf.put_slice(&bytes[*pos..*pos + n]);
                *pos += n;
                if *pos == bytes.len() {
                    self.replay = Replay::Done;
                }
                return Poll::Ready(Ok(()));
            }
            Replay::Done => {}
        }
        loop {
            if self.out_pos < self.out.len() {
                let n = (self.out.len() - self.out_pos).min(buf.remaining());
                buf.put_slice(&self.out[self.out_pos..self.out_pos + n]);
                self.out_pos += n;
                if self.out_pos == self.out.len() {
                    self.out.clear();
                    self.out_pos = 0;
                }
                return Poll::Ready(Ok(()));
            }
            if ready!(self.poll_fill(cx))? == 0 {
                return Poll::Ready(Ok(()));
            }
        }
    }

    /// Follows the client's packet headers in the bytes just written.
    fn tap_written(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            if self.wremaining == 0 && self.wcur.is_none() {
                let take = (HEADER_LEN - self.whdr_len).min(bytes.len());
                self.whdr[self.whdr_len..self.whdr_len + take].copy_from_slice(&bytes[..take]);
                self.whdr_len += take;
                bytes = &bytes[take..];
                if self.whdr_len == HEADER_LEN {
                    let header = Header::parse(&self.whdr)?;
                    self.whdr_len = 0;
                    self.wremaining = header.payload_len();
                    self.wcur = Some(header);
                }
            } else {
                let take = self.wremaining.min(bytes.len());
                self.wremaining -= take;
                bytes = &bytes[take..];
            }
            if self.wremaining == 0
                && let Some(header) = self.wcur.take()
                && header.is_eom()
                && header.ty != TYPE_ATTENTION
            {
                self.client_msgs += 1;
            }
        }
        Ok(())
    }

    fn at_write_boundary(&self) -> bool {
        self.whdr_len == 0 && self.wremaining == 0 && self.wcur.is_none()
    }

    fn swallow(written: &mut Vec<u8>, data: &[u8]) -> io::Result<bool> {
        written.extend_from_slice(data);
        let mut pos = 0;
        while written.len() - pos >= HEADER_LEN {
            let header = Header::parse(&written[pos..])?;
            if header.ty != TYPE_PRELOGIN {
                return Err(invalid("tiberius did not start with PRELOGIN"));
            }
            if written.len() - pos < header.len {
                return Ok(false);
            }
            pos += header.len;
            if header.is_eom() {
                if pos != written.len() {
                    return Err(invalid("tiberius wrote past its PRELOGIN"));
                }
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn poll_attention(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(mut pos) = self.attn_pos else {
            return Poll::Ready(Ok(()));
        };
        while pos < ATTENTION_PACKET.len() {
            let n = ready!(self.io()?.poll_write(cx, &ATTENTION_PACKET[pos..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            pos += n;
            self.attn_pos = Some(pos);
        }
        ready!(self.io()?.poll_flush(cx))?;
        self.attn_pos = None;
        Poll::Ready(Ok(()))
    }

    fn poll_write(&mut self, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        if self.io.is_none() {
            return Poll::Ready(Err(closed()));
        }
        if let Replay::Swallow { written, answer } = &mut self.replay {
            if Self::swallow(written, data)? {
                let bytes = frame(TYPE_TABULAR, answer);
                self.replay = Replay::Serve { bytes, pos: 0 };
            }
            return Poll::Ready(Ok(data.len()));
        }
        ready!(self.poll_attention(cx))?;
        let n = ready!(self.io()?.poll_write(cx, data))?;
        self.tap_written(&data[..n])?;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if matches!(self.replay, Replay::Swallow { .. }) {
            return Poll::Ready(Ok(()));
        }
        ready!(self.poll_attention(cx))?;
        self.io()?.poll_flush(cx)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The stream handed to tiberius (through `tokio_util::compat`) and kept by the connection.
pub(crate) struct SharedWire<T>(Arc<Mutex<Wire<T>>>);

impl<T> Clone for SharedWire<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> SharedWire<T> {
    /// Wraps `io`. With `replay_answer`, tiberius' PRELOGIN is swallowed and answered with
    /// it (framed as one tabular message).
    pub(crate) fn new(io: T, replay_answer: Option<Vec<u8>>) -> Self {
        Self(Arc::new(Mutex::new(Wire::new(io, replay_answer))))
    }

    /// Request messages written since the PRELOGIN replay.
    pub(crate) fn client_messages(&self) -> u64 {
        lock(&self.0).client_msgs
    }

    /// `true` when the request written after `baseline` is completely on the wire (its last
    /// packet handed to the socket) and nothing else is half written.
    pub(crate) fn request_sent(&self, baseline: u64) -> bool {
        let w = lock(&self.0);
        w.client_msgs > baseline && w.at_write_boundary()
    }

    /// `true` while a request's response has not completely arrived.
    pub(crate) fn response_pending(&self) -> bool {
        let w = lock(&self.0);
        w.client_msgs > w.server_msgs
    }

    /// Attention acknowledgements not seen yet.
    pub(crate) fn acks_outstanding(&self) -> u32 {
        lock(&self.0).acks_outstanding
    }

    pub(crate) fn is_dead(&self) -> bool {
        lock(&self.0).io.is_none()
    }

    /// Drops the socket. Every later read or write fails with "the connection was closed
    /// after a cancel".
    pub(crate) fn kill(&self) {
        let io = lock(&self.0).io.take();
        drop(io);
    }

    /// Writes an ATTENTION packet (call only while a request is in flight, at a write
    /// boundary; see [`SharedWire::request_sent`]).
    pub(crate) async fn send_attention(&self) -> io::Result<()> {
        {
            let mut w = lock(&self.0);
            if w.io.is_none() {
                return Err(closed());
            }
            if !w.at_write_boundary() || matches!(w.replay, Replay::Swallow { .. }) {
                return Err(invalid("cannot send an attention inside a request"));
            }
            if w.attn_pos.is_none() {
                w.attn_pos = Some(0);
                w.acks_outstanding += 1;
                w.attn_target = w.client_msgs;
            }
        }
        std::future::poll_fn(|cx| lock(&self.0).poll_attention(cx)).await
    }

    /// Reads until every attention is acknowledged. Other bytes that arrive are kept for
    /// tiberius.
    pub(crate) async fn wait_acks(&self) -> io::Result<()> {
        std::future::poll_fn(|cx| {
            let mut w = lock(&self.0);
            loop {
                if w.acks_outstanding == 0 {
                    return Poll::Ready(Ok(()));
                }
                if ready!(w.poll_fill(cx))? == 0 {
                    return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
                }
            }
        })
        .await
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> AsyncRead for SharedWire<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        lock(&self.0).poll_read(cx, buf)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> AsyncWrite for SharedWire<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        lock(&self.0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        lock(&self.0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut w = lock(&self.0);
        match w.io.as_mut() {
            Some(io) => Pin::new(io).poll_shutdown(cx),
            None => Poll::Ready(Ok(())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::prelogin::{ClientPrelogin, Encryption, STATUS_EOM, ServerPrelogin};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    fn done(status: u16) -> Vec<u8> {
        let mut t = vec![0xFD];
        t.extend_from_slice(&status.to_le_bytes());
        t.extend_from_slice(&[0, 0]);
        t.extend_from_slice(&0u64.to_le_bytes());
        t
    }

    fn message(payload: &[u8]) -> Vec<u8> {
        frame(TYPE_TABULAR, payload)
    }

    fn request(payload: &[u8]) -> Vec<u8> {
        frame(0x01, payload)
    }

    async fn read_n(w: &mut SharedWire<DuplexStream>, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        w.read_exact(&mut buf).await.unwrap();
        buf
    }

    #[tokio::test]
    async fn prelogin_is_swallowed_and_replayed() {
        let (io, mut server) = tokio::io::duplex(64 * 1024);
        let answer = ServerPrelogin {
            version: [17, 0, 0, 1, 0, 0],
            encryption: Encryption::On,
            instance_ok: true,
        }
        .replay_payload();
        let mut w = SharedWire::new(io, Some(answer.clone()));

        // tiberius' PRELOGIN never reaches the server...
        let theirs = ClientPrelogin {
            encryption: Encryption::NotSupported,
            instance: None,
            thread_id: 0,
        }
        .encode();
        let framed = frame(TYPE_PRELOGIN, &theirs);
        let (a, b) = framed.split_at(5);
        w.write_all(a).await.unwrap();
        w.write_all(b).await.unwrap();
        w.flush().await.unwrap();

        // ...and it reads the replayed answer, framed as one tabular message.
        let hdr = read_n(&mut w, HEADER_LEN).await;
        let h = Header::parse(&hdr).unwrap();
        assert_eq!(h.ty, TYPE_TABULAR);
        assert!(h.is_eom());
        let body = read_n(&mut w, h.payload_len()).await;
        assert_eq!(body, answer);
        let parsed = ServerPrelogin::parse(&body).unwrap();
        assert_eq!(parsed.encryption, Encryption::NotSupported);

        // Then everything passes through: LOGIN7 out, the login response in.
        let login = frame(0x10, b"login7");
        w.write_all(&login).await.unwrap();
        w.flush().await.unwrap();
        let mut got = vec![0u8; login.len()];
        server.read_exact(&mut got).await.unwrap();
        assert_eq!(got, login);
        let reply = message(&done(0));
        server.write_all(&reply).await.unwrap();
        assert_eq!(read_n(&mut w, reply.len()).await, reply);
        assert_eq!(w.client_messages(), 1);
        assert!(!w.response_pending());
    }

    #[tokio::test]
    async fn non_prelogin_first_write_is_refused() {
        let (io, _server) = tokio::io::duplex(1024);
        let mut w = SharedWire::new(io, Some(vec![0xFF]));
        assert!(w.write_all(&request(b"select")).await.is_err());
    }

    #[tokio::test]
    async fn in_band_acknowledgement_reaches_tiberius() {
        let (io, mut server) = tokio::io::duplex(64 * 1024);
        let mut w = SharedWire::new(io, None);
        w.write_all(&request(b"WAITFOR")).await.unwrap();
        w.flush().await.unwrap();
        assert!(w.request_sent(0));
        assert!(w.response_pending());

        w.send_attention().await.unwrap();
        assert_eq!(w.acks_outstanding(), 1);
        let mut got = vec![0u8; request(b"WAITFOR").len() + HEADER_LEN];
        server.read_exact(&mut got).await.unwrap();
        assert_eq!(&got[got.len() - HEADER_LEN..], &ATTENTION_PACKET);

        // The cancelled request's response is the acknowledgement itself: passed through.
        let ack = message(&done(DONE_ATTN));
        server.write_all(&ack).await.unwrap();
        assert_eq!(read_n(&mut w, ack.len()).await, ack);
        assert_eq!(w.acks_outstanding(), 0);
        assert!(!w.response_pending());
    }

    #[tokio::test]
    async fn late_acknowledgement_is_swallowed() {
        let (io, mut server) = tokio::io::duplex(64 * 1024);
        let mut w = SharedWire::new(io, None);
        w.write_all(&request(b"SELECT 1")).await.unwrap();
        w.flush().await.unwrap();

        // The response is already on its way when the attention is sent.
        let mut rows = vec![0x81, 1, 2, 3];
        rows.extend(done(0x0010));
        let response = message(&rows);
        server.write_all(&response).await.unwrap();
        w.send_attention().await.unwrap();
        assert_eq!(read_n(&mut w, response.len()).await, response);
        assert_eq!(w.acks_outstanding(), 1);

        // The separate acknowledgement and the next response arrive together.
        let mut tail = message(&done(DONE_ATTN));
        let next = message(&done(0x0010));
        tail.extend_from_slice(&next);
        server.write_all(&tail).await.unwrap();
        w.wait_acks().await.unwrap();
        assert_eq!(w.acks_outstanding(), 0);

        w.write_all(&request(b"SELECT 2")).await.unwrap();
        w.flush().await.unwrap();
        // Only the real response reaches tiberius.
        assert_eq!(read_n(&mut w, next.len()).await, next);
        assert_eq!(w.client_messages(), 2);
        assert!(!w.response_pending());
    }

    #[tokio::test]
    async fn acknowledgement_split_across_packets_is_seen() {
        let (io, mut server) = tokio::io::duplex(64 * 1024);
        let mut w = SharedWire::new(io, None);
        w.write_all(&request(b"SELECT big")).await.unwrap();
        w.flush().await.unwrap();
        w.send_attention().await.unwrap();

        let mut payload = vec![0xD1; 20];
        payload.extend(done(DONE_ATTN | 0x0001));
        let (a, b) = payload.split_at(payload.len() - 5);
        let mut bytes = Vec::new();
        let mut first = frame(TYPE_TABULAR, a);
        first[1] = 0; // not EOM
        bytes.extend(first);
        bytes.extend(frame(TYPE_TABULAR, b));
        server.write_all(&bytes).await.unwrap();
        assert_eq!(read_n(&mut w, bytes.len()).await, bytes);
        assert_eq!(w.acks_outstanding(), 0);
    }

    #[tokio::test]
    async fn killed_wire_fails_fast() {
        let (io, _server) = tokio::io::duplex(1024);
        let mut w = SharedWire::new(io, None);
        w.kill();
        assert!(w.is_dead());
        let mut buf = [0u8; 1];
        let err = w.read(&mut buf).await.unwrap_err();
        assert!(err.to_string().contains("closed after a cancel"), "{err}");
        assert!(w.write_all(b"x").await.is_err());
        assert!(w.send_attention().await.is_err());
    }

    #[test]
    fn attention_tokens() {
        assert!(is_attention_done(&done(DONE_ATTN)));
        assert!(!is_attention_done(&done(0x0010)));
        assert!(is_attention_ack_packet(&message(&done(DONE_ATTN))));
        let mut not_eom = message(&done(DONE_ATTN));
        not_eom[1] &= !STATUS_EOM;
        assert!(!is_attention_ack_packet(&not_eom));
    }
}
