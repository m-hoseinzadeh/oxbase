//! TDS packet framing and PRELOGIN messages, written from the public MS-TDS specification
//! (2.2.3.1 packet header, 2.2.6.5 PRELOGIN).
//!
//! oxbase runs the PRELOGIN exchange itself (the TLS negotiation lives there, see [`super::tls`]);
//! tiberius later sends its own PRELOGIN, which [`super::wire`] answers with
//! [`ServerPrelogin::replay_payload`].

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Length of a TDS packet header.
pub(crate) const HEADER_LEN: usize = 8;
/// Packet type of server responses (tabular result).
pub(crate) const TYPE_TABULAR: u8 = 0x04;
/// Packet type of an attention signal (cancel).
pub(crate) const TYPE_ATTENTION: u8 = 0x06;
/// Packet type of PRELOGIN messages and of the TLS handshake records wrapped in them.
pub(crate) const TYPE_PRELOGIN: u8 = 0x12;
/// Status bit: the last packet of a message.
pub(crate) const STATUS_EOM: u8 = 0x01;
/// Largest packet oxbase writes itself (the TDS default packet size).
pub(crate) const PACKET_SIZE: usize = 4096;
/// Largest packet a server may send (MS-TDS caps the negotiated size at 32767 bytes).
pub(crate) const MAX_PACKET: usize = 32_767;
/// Largest PRELOGIN response oxbase accepts (it is a few dozen bytes in practice).
const MAX_PRELOGIN_RESPONSE: usize = 64 * 1024;

/// The attention packet: header only, `EOM`.
pub(crate) const ATTENTION_PACKET: [u8; HEADER_LEN] = [
    TYPE_ATTENTION,
    STATUS_EOM,
    0x00,
    0x08,
    0x00,
    0x00,
    0x01,
    0x00,
];

// PRELOGIN option tokens.
const OPT_VERSION: u8 = 0x00;
const OPT_ENCRYPTION: u8 = 0x01;
const OPT_INSTOPT: u8 = 0x02;
const OPT_THREADID: u8 = 0x03;
const OPT_MARS: u8 = 0x04;
const OPT_TERMINATOR: u8 = 0xFF;

/// A packet header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    pub ty: u8,
    pub status: u8,
    /// Whole packet length, header included.
    pub len: usize,
}

impl Header {
    /// Parses the first [`HEADER_LEN`] bytes of `b`.
    pub(crate) fn parse(b: &[u8]) -> io::Result<Self> {
        if b.len() < HEADER_LEN {
            return Err(invalid("truncated TDS packet header"));
        }
        let len = usize::from(u16::from_be_bytes([b[2], b[3]]));
        if len < HEADER_LEN {
            return Err(invalid("TDS packet shorter than its header"));
        }
        Ok(Self {
            ty: b[0],
            status: b[1],
            len,
        })
    }

    pub(crate) fn is_eom(self) -> bool {
        self.status & STATUS_EOM != 0
    }

    /// Payload bytes after the header.
    pub(crate) fn payload_len(self) -> usize {
        self.len - HEADER_LEN
    }
}

pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

/// Frames `payload` as one message of `ty` packets of at most [`PACKET_SIZE`] bytes; the last
/// packet carries `EOM`. An empty payload is one header-only packet.
pub(crate) fn frame(ty: u8, payload: &[u8]) -> Vec<u8> {
    let chunk = PACKET_SIZE - HEADER_LEN;
    let mut out = Vec::with_capacity(payload.len() + HEADER_LEN * (payload.len() / chunk + 1));
    if payload.is_empty() {
        push_header(&mut out, ty, STATUS_EOM, 0, 1);
        return out;
    }
    let count = payload.len().div_ceil(chunk);
    for (i, part) in payload.chunks(chunk).enumerate() {
        let status = if i + 1 == count { STATUS_EOM } else { 0 };
        #[allow(
            clippy::cast_possible_truncation,
            reason = "packet ids wrap at 256 by definition"
        )]
        let id = (i + 1) as u8;
        push_header(&mut out, ty, status, part.len(), id);
        out.extend_from_slice(part);
    }
    out
}

fn push_header(out: &mut Vec<u8>, ty: u8, status: u8, payload_len: usize, id: u8) {
    // Bounded by PACKET_SIZE, so it fits in u16.
    let len = u16::try_from(payload_len + HEADER_LEN).unwrap_or(u16::MAX);
    out.push(ty);
    out.push(status);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&[0, 0, id, 0]);
}

/// Reads one whole message (packets up to `EOM`) and returns its payload.
pub(crate) async fn read_message<S: AsyncRead + Unpin>(
    io: &mut S,
    limit: usize,
) -> io::Result<Vec<u8>> {
    let mut payload = Vec::new();
    loop {
        let mut hdr = [0u8; HEADER_LEN];
        io.read_exact(&mut hdr).await?;
        let header = Header::parse(&hdr)?;
        let start = payload.len();
        if start + header.payload_len() > limit {
            return Err(invalid("the server's PRELOGIN response is too large"));
        }
        payload.resize(start + header.payload_len(), 0);
        io.read_exact(&mut payload[start..]).await?;
        if header.is_eom() {
            return Ok(payload);
        }
    }
}

/// PRELOGIN `ENCRYPTION` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encryption {
    /// `ENCRYPT_OFF`: login packet only (oxbase never sends it).
    Off,
    /// `ENCRYPT_ON`.
    On,
    /// `ENCRYPT_NOT_SUP`.
    NotSupported,
    /// `ENCRYPT_REQ`.
    Required,
    /// `ENCRYPT_STRICT` (TDS 8.0 servers may echo it).
    Strict,
}

impl Encryption {
    pub(crate) fn byte(self) -> u8 {
        match self {
            Self::Off => 0x00,
            Self::On => 0x01,
            Self::NotSupported => 0x02,
            Self::Required => 0x03,
            Self::Strict => 0x04,
        }
    }

    /// Decodes a server value. The high bit (`ENCRYPT_CLIENT_CERT`) is ignored.
    pub(crate) fn from_byte(b: u8) -> Option<Self> {
        match b & 0x7F {
            0x00 => Some(Self::Off),
            0x01 => Some(Self::On),
            0x02 => Some(Self::NotSupported),
            0x03 => Some(Self::Required),
            0x04 => Some(Self::Strict),
            _ => None,
        }
    }
}

/// The PRELOGIN oxbase sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientPrelogin {
    pub encryption: Encryption,
    /// Named instance to validate (`INSTOPT`); `None` sends an empty name.
    pub instance: Option<String>,
    pub thread_id: u32,
}

impl ClientPrelogin {
    /// The PRELOGIN payload: `VERSION`, `ENCRYPTION`, `INSTOPT`, `THREADID`, `MARS = 0`.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut instance = self.instance.clone().unwrap_or_default().into_bytes();
        instance.push(0);
        let options: [(u8, Vec<u8>); 5] = [
            (OPT_VERSION, client_version().to_vec()),
            (OPT_ENCRYPTION, vec![self.encryption.byte()]),
            (OPT_INSTOPT, instance),
            (OPT_THREADID, self.thread_id.to_be_bytes().to_vec()),
            (OPT_MARS, vec![0]),
        ];
        encode_options(&options)
    }
}

/// `VERSION` sent by the client: oxbase's own version, then a zero sub-build.
fn client_version() -> [u8; 6] {
    let mut parts = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|p| p.parse::<u8>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    [major, minor, 0, patch, 0, 0]
}

fn encode_options(options: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let table = options.len() * 5 + 1;
    let mut out = Vec::with_capacity(table + options.iter().map(|(_, d)| d.len()).sum::<usize>());
    let mut offset = table;
    for (token, data) in options {
        out.push(*token);
        out.extend_from_slice(&u16::try_from(offset).unwrap_or(u16::MAX).to_be_bytes());
        out.extend_from_slice(&u16::try_from(data.len()).unwrap_or(u16::MAX).to_be_bytes());
        offset += data.len();
    }
    out.push(OPT_TERMINATOR);
    for (_, data) in options {
        out.extend_from_slice(data);
    }
    out
}

/// The server's PRELOGIN answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerPrelogin {
    /// `VERSION` as sent (major, minor, build big-endian, then the sub-build).
    pub version: [u8; 6],
    pub encryption: Encryption,
    /// `false` when the server answered `INSTOPT` with a non-zero byte: not the requested
    /// instance.
    pub instance_ok: bool,
}

impl ServerPrelogin {
    /// Parses a PRELOGIN response payload. Unknown options are skipped.
    pub(crate) fn parse(payload: &[u8]) -> io::Result<Self> {
        let mut version = [0u8; 6];
        let mut encryption = None;
        let mut instance_ok = true;
        let mut pos = 0;
        loop {
            let token = *payload
                .get(pos)
                .ok_or_else(|| invalid("PRELOGIN response without terminator"))?;
            if token == OPT_TERMINATOR {
                break;
            }
            let entry = payload
                .get(pos + 1..pos + 5)
                .ok_or_else(|| invalid("truncated PRELOGIN option table"))?;
            let offset = usize::from(u16::from_be_bytes([entry[0], entry[1]]));
            let len = usize::from(u16::from_be_bytes([entry[2], entry[3]]));
            let data = payload
                .get(offset..offset + len)
                .ok_or_else(|| invalid("PRELOGIN option points outside the message"))?;
            match token {
                OPT_VERSION if len >= 6 => version.copy_from_slice(&data[..6]),
                OPT_ENCRYPTION if len >= 1 => {
                    encryption = Some(
                        Encryption::from_byte(data[0])
                            .ok_or_else(|| invalid("unknown PRELOGIN ENCRYPTION value"))?,
                    );
                }
                OPT_INSTOPT if len >= 1 => instance_ok = data[0] == 0,
                _ => {}
            }
            pos += 5;
        }
        Ok(Self {
            version,
            encryption: encryption
                .ok_or_else(|| invalid("PRELOGIN response without ENCRYPTION"))?,
            instance_ok,
        })
    }

    /// The answer handed to tiberius in place of the real one: the server's `VERSION` with
    /// `ENCRYPTION = ENCRYPT_NOT_SUP` (encryption, if any, is already in place underneath),
    /// `INSTOPT` valid, empty `THREADID`, `MARS` off. Re-encoded rather than echoed so options
    /// tiberius does not know never reach it.
    pub(crate) fn replay_payload(&self) -> Vec<u8> {
        encode_options(&[
            (OPT_VERSION, self.version.to_vec()),
            (OPT_ENCRYPTION, vec![Encryption::NotSupported.byte()]),
            (OPT_INSTOPT, vec![0]),
            (OPT_THREADID, Vec::new()),
            (OPT_MARS, vec![0]),
        ])
    }
}

/// Sends `msg` as a PRELOGIN message on `io` and reads the server's answer.
pub(crate) async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut S,
    msg: &ClientPrelogin,
) -> io::Result<ServerPrelogin> {
    io.write_all(&frame(TYPE_PRELOGIN, &msg.encode())).await?;
    io.flush().await?;
    let payload = read_message(io, MAX_PRELOGIN_RESPONSE).await?;
    ServerPrelogin::parse(&payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server answer as SQL Server writes it: VERSION, ENCRYPTION, INSTOPT, THREADID (empty),
    /// MARS, FEDAUTHREQUIRED (skipped), terminator.
    fn server_answer(encryption: u8, instopt: u8) -> Vec<u8> {
        encode_options(&[
            (OPT_VERSION, vec![17, 0, 0x0F, 0xA5, 0, 0]),
            (OPT_ENCRYPTION, vec![encryption]),
            (OPT_INSTOPT, vec![instopt]),
            (OPT_THREADID, Vec::new()),
            (OPT_MARS, vec![0]),
            (0x06, vec![0]),
        ])
    }

    #[test]
    fn client_prelogin_layout() {
        let msg = ClientPrelogin {
            encryption: Encryption::On,
            instance: Some("SQLEXPRESS".into()),
            thread_id: 0x0102_0304,
        };
        let p = msg.encode();
        // 5 options * 5 bytes + terminator = 26 bytes of table.
        assert_eq!(p[25], OPT_TERMINATOR);
        assert_eq!(&p[0..5], &[OPT_VERSION, 0, 26, 0, 6]);
        assert_eq!(&p[5..10], &[OPT_ENCRYPTION, 0, 32, 0, 1]);
        assert_eq!(&p[10..15], &[OPT_INSTOPT, 0, 33, 0, 11]);
        assert_eq!(&p[15..20], &[OPT_THREADID, 0, 44, 0, 4]);
        assert_eq!(&p[20..25], &[OPT_MARS, 0, 48, 0, 1]);
        assert_eq!(p[32], 0x01);
        assert_eq!(&p[33..44], b"SQLEXPRESS\0");
        assert_eq!(&p[44..48], &[1, 2, 3, 4]);
        assert_eq!(p[48], 0);
        assert_eq!(p.len(), 49);

        let plain = ClientPrelogin {
            encryption: Encryption::NotSupported,
            instance: None,
            thread_id: 0,
        }
        .encode();
        assert_eq!(&plain[10..15], &[OPT_INSTOPT, 0, 33, 0, 1]);
        assert_eq!(plain[33], 0);
        assert_eq!(plain[32], 0x02);
    }

    #[test]
    fn server_prelogin_parses_and_replays() {
        let s = ServerPrelogin::parse(&server_answer(0x01, 0)).unwrap();
        assert_eq!(s.encryption, Encryption::On);
        assert!(s.instance_ok);
        assert_eq!(s.version[0], 17);

        let replay = s.replay_payload();
        let back = ServerPrelogin::parse(&replay).unwrap();
        assert_eq!(back.encryption, Encryption::NotSupported);
        assert_eq!(back.version, s.version);
        assert!(back.instance_ok);

        let wrong = ServerPrelogin::parse(&server_answer(0x03, 1)).unwrap();
        assert_eq!(wrong.encryption, Encryption::Required);
        assert!(!wrong.instance_ok);
        // ENCRYPT_CLIENT_CERT is a flag on top of the level.
        let cert = ServerPrelogin::parse(&server_answer(0x81, 0)).unwrap();
        assert_eq!(cert.encryption, Encryption::On);
    }

    #[test]
    fn malformed_server_prelogin_is_an_error() {
        assert!(ServerPrelogin::parse(&[]).is_err());
        assert!(ServerPrelogin::parse(&[OPT_ENCRYPTION, 0, 50, 0, 1, OPT_TERMINATOR]).is_err());
        assert!(
            ServerPrelogin::parse(&[OPT_TERMINATOR]).is_err(),
            "no ENCRYPTION"
        );
        let bad = encode_options(&[(OPT_ENCRYPTION, vec![0x09])]);
        assert!(ServerPrelogin::parse(&bad).is_err());
    }

    #[test]
    fn framing_splits_and_marks_the_last_packet() {
        let one = frame(TYPE_PRELOGIN, &[1, 2, 3]);
        assert_eq!(one, [0x12, 0x01, 0, 11, 0, 0, 1, 0, 1, 2, 3]);
        let empty = frame(TYPE_PRELOGIN, &[]);
        assert_eq!(empty, [0x12, 0x01, 0, 8, 0, 0, 1, 0]);

        let big = vec![7u8; PACKET_SIZE * 2];
        let framed = frame(TYPE_PRELOGIN, &big);
        let first = Header::parse(&framed).unwrap();
        assert_eq!(first.len, PACKET_SIZE);
        assert!(!first.is_eom());
        let second = Header::parse(&framed[PACKET_SIZE..]).unwrap();
        assert!(!second.is_eom());
        let third = Header::parse(&framed[PACKET_SIZE * 2..]).unwrap();
        assert!(third.is_eom());
        assert_eq!(
            first.payload_len() + second.payload_len() + third.payload_len(),
            big.len()
        );
        assert!(Header::parse(&[0x04, 0x01, 0, 4, 0, 0, 0, 0]).is_err());
        assert_eq!(Header::parse(&ATTENTION_PACKET).unwrap().ty, TYPE_ATTENTION);
    }

    #[tokio::test]
    async fn exchange_over_a_duplex() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let srv = tokio::spawn(async move {
            let request = read_message(&mut server, 4096).await.unwrap();
            let parsed = ServerPrelogin::parse(&request).unwrap();
            assert_eq!(parsed.encryption, Encryption::On);
            // Answer in two packets to exercise reassembly.
            let answer = server_answer(0x00, 0);
            let (a, b) = answer.split_at(7);
            let mut bytes = Vec::new();
            push_header(&mut bytes, TYPE_TABULAR, 0, a.len(), 1);
            bytes.extend_from_slice(a);
            push_header(&mut bytes, TYPE_TABULAR, STATUS_EOM, b.len(), 2);
            bytes.extend_from_slice(b);
            server.write_all(&bytes).await.unwrap();
        });
        let answer = exchange(
            &mut client,
            &ClientPrelogin {
                encryption: Encryption::On,
                instance: None,
                thread_id: 7,
            },
        )
        .await
        .unwrap();
        assert_eq!(answer.encryption, Encryption::Off);
        srv.await.unwrap();
    }
}
