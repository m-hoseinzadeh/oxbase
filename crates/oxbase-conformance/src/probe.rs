//! Reachability probes: a TCP connect plus a minimal, unauthenticated protocol handshake.
//!
//! The handshake matters because Docker's port proxy accepts TCP connections on a published
//! port even when nothing listens inside the container. Each probe sends the smallest request
//! the engine answers before authentication and checks the reply shape:
//!
//! | Engine | Request | Accepted reply |
//! |---|---|---|
//! | PostgreSQL | `SSLRequest` | `S`, `N` or an `E` error message |
//! | MySQL / MariaDB | none (server speaks first) | protocol-10 greeting or an error packet |
//! | SQL Server | TDS `PRELOGIN` | a TDS tabular-result packet |
//! | MongoDB | `OP_MSG {buildInfo: 1}` | an `OP_MSG` reply |
//! | Redis | inline `PING` | a RESP simple string or error (`+PONG`, `-NOAUTH ...`) |
//!
//! SQLite has no server: the probe creates the fixture file (an empty file is a valid empty
//! database) or checks the header of an existing one.

use std::fmt::{self, Write as _};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::adapter::Engine;

/// Default per-engine connect and handshake timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// Largest reply the probes will read, so a misbehaving peer cannot make them allocate much.
const MAX_REPLY: usize = 1 << 20;

/// Where an engine fixture lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Target {
    Tcp { host: String, port: u16 },
    File { path: PathBuf },
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp { host, port } if host.contains(':') => write!(f, "[{host}]:{port}"),
            Self::Tcp { host, port } => write!(f, "{host}:{port}"),
            Self::File { path } => write!(f, "{}", path.display()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeOptions {
    pub timeout: Duration,
    /// When `false`, a successful TCP connect is enough.
    pub handshake: bool,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            handshake: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Handshake {
    NotAttempted,
    Verified { detail: String },
    Failed { detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeOutcome {
    pub reachable: bool,
    pub latency_ms: u64,
    pub handshake: Handshake,
    /// Version the server reported during the handshake, when the protocol exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    /// Whether `server_version` matches the fixture's `version_prefix`. Informational only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_matches: Option<bool>,
    /// The fixture's `version_prefix`, when it has one. If this is set but `server_version` is
    /// not, the probe could not check the version: the PostgreSQL SSLRequest and the
    /// unauthenticated Redis PING never report one (PostgreSQL only sends `server_version`
    /// after authentication). Version checks for those engines wait for the M1 drivers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ProbeOutcome {
    fn unreachable(error: String, start: Instant) -> Self {
        Self {
            reachable: false,
            latency_ms: elapsed_ms(start),
            handshake: Handshake::NotAttempted,
            server_version: None,
            version_matches: None,
            expected_version: None,
            error: Some(error),
        }
    }

    /// True when the fixture names a version but the probe could not compare it.
    pub fn version_not_checked(&self) -> bool {
        self.reachable && self.expected_version.is_some() && self.version_matches.is_none()
    }

    /// One-line human summary for the report table.
    pub fn summary(&self) -> String {
        let mut s = match (&self.error, &self.handshake) {
            (Some(err), _) => err.clone(),
            (None, Handshake::Verified { detail } | Handshake::Failed { detail }) => detail.clone(),
            (None, Handshake::NotAttempted) => "TCP connect ok (handshake skipped)".to_owned(),
        };
        if self.version_matches == Some(false) {
            s.push_str(" [version differs from fixtures.toml]");
        } else if self.version_not_checked() {
            s.push_str(" [version not checked]");
        }
        if self.reachable {
            let _ = write!(s, " ({} ms)", self.latency_ms);
        }
        s
    }
}

/// Probes one engine fixture. Never panics; every failure is reported in the outcome.
pub fn probe(
    engine: Engine,
    target: &Target,
    version_prefix: Option<&str>,
    opts: ProbeOptions,
) -> ProbeOutcome {
    let mut outcome = match target {
        Target::Tcp { host, port } => probe_tcp(engine, host, *port, opts),
        Target::File { path } => probe_file(path),
    };
    if let (Some(version), Some(expected)) = (&outcome.server_version, version_prefix) {
        outcome.version_matches = version_matches(engine, version, expected);
    }
    outcome.expected_version = version_prefix.map(str::to_owned);
    outcome
}

fn probe_tcp(engine: Engine, host: &str, port: u16, opts: ProbeOptions) -> ProbeOutcome {
    let start = Instant::now();
    let endpoint = Target::Tcp {
        host: host.to_owned(),
        port,
    };
    let mut stream = match connect(host, port, opts.timeout) {
        Ok(s) => s,
        Err(e) => {
            return ProbeOutcome::unreachable(
                format!("TCP connect to {endpoint} failed: {e}"),
                start,
            );
        }
    };
    if !opts.handshake {
        return ProbeOutcome {
            reachable: true,
            latency_ms: elapsed_ms(start),
            handshake: Handshake::NotAttempted,
            server_version: None,
            version_matches: None,
            expected_version: None,
            error: None,
        };
    }
    let result = stream
        .set_read_timeout(Some(opts.timeout))
        .and_then(|()| stream.set_write_timeout(Some(opts.timeout)))
        .map_err(|e| e.to_string())
        .and_then(|()| handshake(engine, &mut stream));
    match result {
        Ok(Reply { detail, version }) => ProbeOutcome {
            reachable: true,
            latency_ms: elapsed_ms(start),
            handshake: Handshake::Verified { detail },
            server_version: version,
            version_matches: None,
            expected_version: None,
            error: None,
        },
        Err(detail) => ProbeOutcome {
            reachable: false,
            latency_ms: elapsed_ms(start),
            error: Some(format!(
                "port {endpoint} accepts TCP but the {engine} handshake failed: {detail}"
            )),
            handshake: Handshake::Failed { detail },
            server_version: None,
            version_matches: None,
            expected_version: None,
        },
    }
}

fn connect(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let mut last = None;
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address for host")))
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// What a successful handshake learned.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reply {
    detail: String,
    version: Option<String>,
}

impl Reply {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            version: None,
        }
    }
}

type HsResult = Result<Reply, String>;

fn handshake(engine: Engine, s: &mut TcpStream) -> HsResult {
    match engine {
        Engine::Postgres => postgres(s),
        Engine::MySql | Engine::MariaDb => mysql(s),
        Engine::MsSql => mssql(s),
        Engine::MongoDb => mongodb(s),
        Engine::Redis => redis(s),
        Engine::Sqlite => Err("sqlite is file-based and has no network handshake".to_owned()),
    }
}

fn io_err(what: &str) -> impl Fn(io::Error) -> String + '_ {
    move |e| match e.kind() {
        io::ErrorKind::UnexpectedEof => {
            format!("{what}: connection closed by the peer (engine still starting?)")
        }
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
            format!("{what}: timed out waiting for a reply")
        }
        _ => format!("{what}: {e}"),
    }
}

fn read_vec(s: &mut impl Read, len: usize, what: &str) -> Result<Vec<u8>, String> {
    if len > MAX_REPLY {
        return Err(format!("{what}: reply of {len} bytes is implausibly large"));
    }
    let mut buf = vec![0; len];
    s.read_exact(&mut buf).map_err(io_err(what))?;
    Ok(buf)
}

// ---------------------------------------------------------------- PostgreSQL

/// Length 8, request code 80877103.
const PG_SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 0x04, 0xD2, 0x16, 0x2F];

fn postgres(s: &mut (impl Read + Write)) -> HsResult {
    s.write_all(&PG_SSL_REQUEST)
        .map_err(io_err("sending SSLRequest"))?;
    let mut b = [0u8; 1];
    s.read_exact(&mut b)
        .map_err(io_err("reading SSLRequest reply"))?;
    parse_pg_ssl_reply(b[0])
}

fn parse_pg_ssl_reply(b: u8) -> HsResult {
    match b {
        b'S' => Ok(Reply::new("SSLRequest answered 'S' (TLS offered)")),
        b'N' => Ok(Reply::new("SSLRequest answered 'N' (TLS not offered)")),
        b'E' => Ok(Reply::new("SSLRequest answered with an error message")),
        other => Err(format!("unexpected SSLRequest reply byte 0x{other:02x}")),
    }
}

// ---------------------------------------------------------------- MySQL / MariaDB

fn mysql(s: &mut impl Read) -> HsResult {
    let mut header = [0u8; 4];
    s.read_exact(&mut header)
        .map_err(io_err("reading server greeting"))?;
    let len = usize::from(header[0]) | usize::from(header[1]) << 8 | usize::from(header[2]) << 16;
    let payload = read_vec(s, len, "reading server greeting")?;
    parse_mysql_greeting(&payload)
}

fn parse_mysql_greeting(payload: &[u8]) -> HsResult {
    match payload.split_first() {
        Some((10, rest)) => {
            let end = rest
                .iter()
                .position(|&b| b == 0)
                .ok_or("greeting has an unterminated server version")?;
            let raw = String::from_utf8_lossy(&rest[..end]);
            // MariaDB may prefix "5.5.5-" for old replication clients.
            let version = raw.strip_prefix("5.5.5-").unwrap_or(&raw).to_owned();
            Ok(Reply {
                detail: format!("protocol 10 greeting from server {version}"),
                version: Some(version),
            })
        }
        Some((0xFF, rest)) if rest.len() >= 2 => {
            let code = u16::from_le_bytes([rest[0], rest[1]]);
            let mut msg = &rest[2..];
            if msg.first() == Some(&b'#') && msg.len() >= 6 {
                msg = &msg[6..];
            }
            Ok(Reply::new(format!(
                "server answered with error {code}: {}",
                String::from_utf8_lossy(msg)
            )))
        }
        Some((p, _)) => Err(format!("unexpected greeting byte 0x{p:02x}")),
        None => Err("empty greeting packet".to_owned()),
    }
}

// ---------------------------------------------------------------- SQL Server (TDS)

const TDS_PRELOGIN: u8 = 0x12;
const TDS_TABULAR_RESULT: u8 = 0x04;

/// A PRELOGIN packet with only the VERSION and ENCRYPTION options.
fn tds_prelogin_packet() -> Vec<u8> {
    // Option table: VERSION at 11 (6 bytes), ENCRYPTION at 17 (1 byte), terminator.
    let payload: [u8; 18] = [
        0x00, 0x00, 11, 0x00, 6, // VERSION
        0x01, 0x00, 17, 0x00, 1,    // ENCRYPTION
        0xFF, // terminator
        0, 0, 0, 0, 0, 0,    // client version (unspecified)
        0x00, // ENCRYPT_OFF
    ];
    let total = u16::try_from(8 + payload.len()).unwrap_or(u16::MAX);
    let mut packet = vec![TDS_PRELOGIN, 0x01];
    packet.extend_from_slice(&total.to_be_bytes());
    packet.extend_from_slice(&[0, 0, 1, 0]); // SPID, packet id, window
    packet.extend_from_slice(&payload);
    packet
}

fn mssql(s: &mut (impl Read + Write)) -> HsResult {
    s.write_all(&tds_prelogin_packet())
        .map_err(io_err("sending PRELOGIN"))?;
    let mut header = [0u8; 8];
    s.read_exact(&mut header)
        .map_err(io_err("reading PRELOGIN reply"))?;
    if header[0] != TDS_TABULAR_RESULT {
        return Err(format!(
            "PRELOGIN reply has packet type 0x{:02x}, expected 0x{TDS_TABULAR_RESULT:02x}",
            header[0]
        ));
    }
    let total = usize::from(u16::from_be_bytes([header[2], header[3]]));
    let payload = read_vec(
        s,
        total.saturating_sub(header.len()),
        "reading PRELOGIN reply",
    )?;
    Ok(parse_tds_prelogin_reply(&payload))
}

fn parse_tds_prelogin_reply(payload: &[u8]) -> Reply {
    let version = tds_option(payload, 0x00).and_then(|v| {
        let [major, minor, b0, b1, ..] = *v else {
            return None;
        };
        let build = u16::from_be_bytes([b0, b1]);
        let product = mssql_product(major).map(|p| format!(" (SQL Server {p})"));
        Some(format!(
            "{major}.{minor}.{build}{}",
            product.unwrap_or_default()
        ))
    });
    let detail = match &version {
        Some(v) => format!("PRELOGIN answered, server {v}"),
        None => "PRELOGIN answered".to_owned(),
    };
    Reply { detail, version }
}

/// Finds one option's data in a PRELOGIN option table.
fn tds_option(payload: &[u8], wanted: u8) -> Option<&[u8]> {
    let mut i = 0;
    loop {
        let token = *payload.get(i)?;
        if token == 0xFF {
            return None;
        }
        let entry = payload.get(i + 1..i + 5)?;
        let offset = usize::from(u16::from_be_bytes([entry[0], entry[1]]));
        let len = usize::from(u16::from_be_bytes([entry[2], entry[3]]));
        if token == wanted {
            return payload.get(offset..offset + len);
        }
        i += 5;
    }
}

fn mssql_product(major: u8) -> Option<&'static str> {
    Some(match major {
        13 => "2016",
        14 => "2017",
        15 => "2019",
        16 => "2022",
        17 => "2025",
        _ => return None,
    })
}

// ---------------------------------------------------------------- MongoDB

const OP_MSG: u32 = 2013;

fn bson_document(elements: &[u8]) -> Vec<u8> {
    let len = u32::try_from(4 + elements.len() + 1).unwrap_or(u32::MAX);
    let mut doc = len.to_le_bytes().to_vec();
    doc.extend_from_slice(elements);
    doc.push(0);
    doc
}

/// `OP_MSG` carrying `{buildInfo: 1, $db: "admin"}`. `buildInfo` needs no authentication.
fn mongo_build_info_msg() -> Vec<u8> {
    let mut el = vec![0x10];
    el.extend_from_slice(b"buildInfo\0");
    el.extend_from_slice(&1i32.to_le_bytes());
    el.push(0x02);
    el.extend_from_slice(b"$db\0");
    el.extend_from_slice(&6u32.to_le_bytes());
    el.extend_from_slice(b"admin\0");
    let doc = bson_document(&el);

    let total = u32::try_from(16 + 4 + 1 + doc.len()).unwrap_or(u32::MAX);
    let mut msg = Vec::with_capacity(total as usize);
    msg.extend_from_slice(&total.to_le_bytes());
    msg.extend_from_slice(&1u32.to_le_bytes()); // requestID
    msg.extend_from_slice(&0u32.to_le_bytes()); // responseTo
    msg.extend_from_slice(&OP_MSG.to_le_bytes());
    msg.extend_from_slice(&0u32.to_le_bytes()); // flagBits
    msg.push(0); // section kind 0: body
    msg.extend_from_slice(&doc);
    msg
}

fn mongodb(s: &mut (impl Read + Write)) -> HsResult {
    s.write_all(&mongo_build_info_msg())
        .map_err(io_err("sending buildInfo"))?;
    let mut header = [0u8; 16];
    s.read_exact(&mut header)
        .map_err(io_err("reading buildInfo reply"))?;
    let total = le_u32(&header, 0).unwrap_or(0);
    let op = le_u32(&header, 12).unwrap_or(0);
    if op != OP_MSG {
        return Err(format!("reply has opCode {op}, expected {OP_MSG} (OP_MSG)"));
    }
    let body_len = usize::try_from(total)
        .ok()
        .and_then(|t| t.checked_sub(16))
        .ok_or_else(|| format!("reply has invalid length {total}"))?;
    let body = read_vec(s, body_len, "reading buildInfo reply")?;
    parse_mongo_reply(&body)
}

fn parse_mongo_reply(body: &[u8]) -> HsResult {
    // flagBits (4), then section kind 0 and the body document.
    let Some([0, doc @ ..]) = body.get(4..) else {
        return Err("reply has no body section".to_owned());
    };
    let elements = bson_elements(doc).ok_or("reply body is not a valid BSON document")?;
    let find = |key: &str| elements.iter().find(|e| e.name == key.as_bytes());
    if let Some(version) = find("version").and_then(BsonElement::as_str) {
        return Ok(Reply {
            detail: format!("buildInfo answered, server {version}"),
            version: Some(version),
        });
    }
    let errmsg = find("errmsg").and_then(BsonElement::as_str);
    Ok(Reply::new(match errmsg {
        Some(m) => format!("OP_MSG answered with error: {m}"),
        None => "OP_MSG answered".to_owned(),
    }))
}

fn le_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

struct BsonElement<'a> {
    ty: u8,
    name: &'a [u8],
    value: &'a [u8],
}

impl BsonElement<'_> {
    fn as_str(&self) -> Option<String> {
        if self.ty != 0x02 {
            return None;
        }
        let s = self.value.get(4..)?.strip_suffix(&[0])?;
        Some(String::from_utf8_lossy(s).into_owned())
    }
}

/// Top-level elements of a BSON document. `None` on malformed input or an unknown type.
fn bson_elements(doc: &[u8]) -> Option<Vec<BsonElement<'_>>> {
    let total = usize::try_from(le_u32(doc, 0)?).ok()?;
    let doc = doc.get(..total)?;
    let mut out = Vec::new();
    let mut i = 4;
    loop {
        let ty = *doc.get(i)?;
        i += 1;
        if ty == 0 {
            return Some(out);
        }
        let name_len = doc.get(i..)?.iter().position(|&b| b == 0)?;
        let name = &doc[i..i + name_len];
        i += name_len + 1;
        let prefixed = |extra: usize| -> Option<usize> {
            usize::try_from(le_u32(doc, i)?).ok()?.checked_add(extra)
        };
        let size = match ty {
            0x01 | 0x09 | 0x11 | 0x12 => 8, // double, datetime, timestamp, int64
            0x02 | 0x0D | 0x0E => prefixed(4)?, // string, js code, symbol
            0x03 | 0x04 | 0x0F => prefixed(0)?, // document, array, code_w_scope
            0x05 => prefixed(5)?,           // binary
            0x07 => 12,                     // ObjectId
            0x08 => 1,                      // bool
            0x06 | 0x0A | 0x7F | 0xFF => 0, // undefined, null, max/min key
            0x10 => 4,                      // int32
            0x13 => 16,                     // decimal128
            _ => return None,
        };
        let value = doc.get(i..i.checked_add(size)?)?;
        out.push(BsonElement { ty, name, value });
        i += size;
    }
}

// ---------------------------------------------------------------- Redis

fn redis(s: &mut TcpStream) -> HsResult {
    s.write_all(b"PING\r\n").map_err(io_err("sending PING"))?;
    let mut line = Vec::new();
    BufReader::new(s)
        .take(512)
        .read_until(b'\n', &mut line)
        .map_err(io_err("reading PING reply"))?;
    parse_redis_reply(&line)
}

fn parse_redis_reply(line: &[u8]) -> HsResult {
    let text = String::from_utf8_lossy(line);
    let text = text.trim_end();
    match text.as_bytes().first() {
        Some(b'+') => Ok(Reply::new(format!("PING answered {text}"))),
        Some(b'-') => Ok(Reply::new(format!("PING answered with error {text}"))),
        Some(_) => Err(format!("unexpected PING reply {text:?}")),
        None => Err("connection closed without a PING reply".to_owned()),
    }
}

// ---------------------------------------------------------------- SQLite

const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

fn probe_file(path: &Path) -> ProbeOutcome {
    let start = Instant::now();
    match check_sqlite_file(path) {
        Ok(detail) => ProbeOutcome {
            reachable: true,
            latency_ms: elapsed_ms(start),
            handshake: Handshake::Verified { detail },
            server_version: None,
            version_matches: None,
            expected_version: None,
            error: None,
        },
        Err(e) => ProbeOutcome::unreachable(format!("{}: {e}", path.display()), start),
    }
}

fn check_sqlite_file(path: &Path) -> Result<String, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("cannot create directory: {e}"))?;
    }
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(_) => return Ok("created empty database file".to_owned()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("cannot create file: {e}")),
    }
    let mut header = Vec::with_capacity(16);
    fs::File::open(path)
        .and_then(|f| f.take(16).read_to_end(&mut header))
        .map_err(|e| format!("cannot read file: {e}"))?;
    if header.is_empty() {
        Ok("empty database file".to_owned())
    } else if header == SQLITE_HEADER {
        Ok("existing database file with a valid SQLite header".to_owned())
    } else {
        Err("file exists but is not a SQLite database".to_owned())
    }
}

// ---------------------------------------------------------------- versions

/// Compares a handshake version with the fixture's `version_prefix`. `None` when the two are
/// not comparable.
fn version_matches(engine: Engine, version: &str, expected: &str) -> Option<bool> {
    match engine {
        // The prefix is the `@@VERSION` banner ("Microsoft SQL Server 2025"); the handshake
        // only gives the numeric version, so compare the product year.
        Engine::MsSql => {
            let year = version.split("SQL Server ").nth(1)?.trim_end_matches(')');
            Some(expected.contains(year))
        }
        _ => Some(version.starts_with(expected)),
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    fn opts() -> ProbeOptions {
        ProbeOptions {
            timeout: Duration::from_secs(2),
            handshake: true,
        }
    }

    /// Serves one connection: reads `expect_in` bytes, then writes `reply`.
    fn fake_server(expect_in: usize, reply: Vec<u8>) -> (u16, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut got = vec![0; expect_in];
            s.read_exact(&mut got).unwrap();
            s.write_all(&reply).unwrap();
            got
        });
        (port, handle)
    }

    fn tcp(port: u16) -> Target {
        Target::Tcp {
            host: "127.0.0.1".to_owned(),
            port,
        }
    }

    #[test]
    fn closed_port_is_unreachable_with_endpoint_in_message() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let out = probe(Engine::Postgres, &tcp(port), None, opts());
        assert!(!out.reachable);
        let err = out.error.unwrap();
        assert!(err.contains(&format!("127.0.0.1:{port}")), "{err}");
    }

    #[test]
    fn postgres_ssl_request_against_fake_server() {
        let (port, h) = fake_server(8, b"N".to_vec());
        let out = probe(Engine::Postgres, &tcp(port), None, opts());
        assert_eq!(h.join().unwrap(), PG_SSL_REQUEST);
        assert!(out.reachable, "{out:?}");
        assert!(matches!(out.handshake, Handshake::Verified { .. }));
    }

    #[test]
    fn postgres_version_is_reported_as_not_checked() {
        // The SSLRequest reply carries no version, so a version_prefix cannot be compared.
        let (port, h) = fake_server(8, b"N".to_vec());
        let out = probe(Engine::Postgres, &tcp(port), Some("PostgreSQL 18"), opts());
        h.join().unwrap();
        assert!(out.reachable, "{out:?}");
        assert_eq!(out.server_version, None);
        assert_eq!(out.version_matches, None);
        assert_eq!(out.expected_version.as_deref(), Some("PostgreSQL 18"));
        assert!(out.version_not_checked());
        assert!(
            out.summary().contains("[version not checked]"),
            "{}",
            out.summary()
        );
    }

    #[test]
    fn wrong_protocol_on_port_is_unreachable() {
        // Something answers, but not like PostgreSQL.
        let (port, h) = fake_server(8, b"HTTP/1.1 400\r\n".to_vec());
        let out = probe(Engine::Postgres, &tcp(port), None, opts());
        h.join().unwrap();
        assert!(!out.reachable);
        assert!(matches!(out.handshake, Handshake::Failed { .. }));
        assert!(out.error.unwrap().contains("handshake failed"));
    }

    #[test]
    fn tcp_only_mode_skips_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let out = probe(
            Engine::Redis,
            &tcp(port),
            None,
            ProbeOptions {
                handshake: false,
                ..opts()
            },
        );
        assert!(out.reachable);
        assert_eq!(out.handshake, Handshake::NotAttempted);
    }

    #[test]
    fn redis_against_fake_server() {
        let (port, h) = fake_server(6, b"-NOAUTH Authentication required.\r\n".to_vec());
        let out = probe(Engine::Redis, &tcp(port), None, opts());
        assert_eq!(h.join().unwrap(), b"PING\r\n");
        assert!(out.reachable, "{out:?}");
        assert!(parse_redis_reply(b"+PONG\r\n").is_ok());
        assert!(parse_redis_reply(b"").is_err());
        assert!(parse_redis_reply(b"SSH-2.0-x\r\n").is_err());
    }

    #[test]
    fn mysql_greeting_parsing() {
        let mut greeting = vec![10];
        greeting.extend_from_slice(b"5.5.5-11.8.9-MariaDB-ubu2404\0rest");
        let reply = parse_mysql_greeting(&greeting).unwrap();
        assert_eq!(reply.version.as_deref(), Some("11.8.9-MariaDB-ubu2404"));
        assert_eq!(
            version_matches(Engine::MariaDb, "11.8.9-MariaDB-ubu2404", "11.8.9-MariaDB"),
            Some(true)
        );

        let mut err = vec![0xFF, 0x6A, 0x04, b'#'];
        err.extend_from_slice(b"HY000Host not allowed");
        let reply = parse_mysql_greeting(&err).unwrap();
        assert!(reply.detail.contains("1130: Host not allowed"), "{reply:?}");

        assert!(parse_mysql_greeting(&[9, 0]).is_err());
        assert!(parse_mysql_greeting(&[10, b'8']).is_err());
    }

    #[test]
    fn mysql_against_fake_server() {
        let mut payload = vec![10];
        payload.extend_from_slice(b"8.4.13\0");
        let len = u8::try_from(payload.len()).unwrap();
        let mut packet = vec![len, 0, 0, 0];
        packet.extend_from_slice(&payload);
        let (port, h) = fake_server(0, packet);
        let out = probe(Engine::MySql, &tcp(port), Some("8.4.13"), opts());
        h.join().unwrap();
        assert!(out.reachable, "{out:?}");
        assert_eq!(out.server_version.as_deref(), Some("8.4.13"));
        assert_eq!(out.version_matches, Some(true));
    }

    #[test]
    fn tds_prelogin_round_trip() {
        let packet = tds_prelogin_packet();
        assert_eq!(packet[0], TDS_PRELOGIN);
        assert_eq!(
            usize::from(u16::from_be_bytes([packet[2], packet[3]])),
            packet.len()
        );
        // Reply payload: VERSION option at offset 6 = 17.0.1000.
        let payload = [0x00, 0x00, 6, 0x00, 6, 0xFF, 17, 0, 0x03, 0xE8, 0, 0];
        let mut reply = vec![TDS_TABULAR_RESULT, 1];
        reply.extend_from_slice(&u16::try_from(8 + payload.len()).unwrap().to_be_bytes());
        reply.extend_from_slice(&[0, 0, 1, 0]);
        reply.extend_from_slice(&payload);
        let (port, h) = fake_server(packet.len(), reply);
        let out = probe(
            Engine::MsSql,
            &tcp(port),
            Some("Microsoft SQL Server 2025"),
            opts(),
        );
        h.join().unwrap();
        assert!(out.reachable, "{out:?}");
        assert_eq!(
            out.server_version.as_deref(),
            Some("17.0.1000 (SQL Server 2025)")
        );
        assert_eq!(out.version_matches, Some(true));
    }

    #[test]
    fn mongo_build_info_round_trip() {
        let msg = mongo_build_info_msg();
        assert_eq!(le_u32(&msg, 0).map(|n| n as usize), Some(msg.len()));
        assert_eq!(le_u32(&msg, 12), Some(OP_MSG));
        let doc = &msg[21..];
        let els = bson_elements(doc).unwrap();
        assert_eq!(els.len(), 2);
        assert_eq!(els[1].as_str().as_deref(), Some("admin"));

        // Reply body: {ok: 1.0, nested: {a: 1}, version: "8.0.34"}
        let mut el = vec![0x01];
        el.extend_from_slice(b"ok\0");
        el.extend_from_slice(&1f64.to_le_bytes());
        let mut inner = vec![0x10, b'a', 0];
        inner.extend_from_slice(&1i32.to_le_bytes());
        el.push(0x03);
        el.extend_from_slice(b"nested\0");
        el.extend_from_slice(&bson_document(&inner));
        el.push(0x02);
        el.extend_from_slice(b"version\0");
        el.extend_from_slice(&7u32.to_le_bytes());
        el.extend_from_slice(b"8.0.34\0");
        let doc = bson_document(&el);
        let mut body = vec![0, 0, 0, 0, 0];
        body.extend_from_slice(&doc);
        let mut reply = u32::try_from(16 + body.len())
            .unwrap()
            .to_le_bytes()
            .to_vec();
        reply.extend_from_slice(&[0; 8]);
        reply.extend_from_slice(&OP_MSG.to_le_bytes());
        reply.extend_from_slice(&body);

        let (port, h) = fake_server(msg.len(), reply);
        let out = probe(Engine::MongoDb, &tcp(port), Some("8.0.34"), opts());
        h.join().unwrap();
        assert!(out.reachable, "{out:?}");
        assert_eq!(out.server_version.as_deref(), Some("8.0.34"));
        assert_eq!(out.version_matches, Some(true));
        assert!(parse_mongo_reply(&[0, 0, 0, 0, 1]).is_err());
        assert!(bson_elements(&[0xFF, 0, 0, 0]).is_none());
    }

    #[test]
    fn sqlite_file_probe() {
        let dir =
            std::env::temp_dir().join(format!("oxbase-conformance-probe-{}", std::process::id()));
        let path = dir.join("nested").join("t.sqlite");
        let _ = fs::remove_dir_all(&dir);

        let out = probe(
            Engine::Sqlite,
            &Target::File { path: path.clone() },
            None,
            opts(),
        );
        assert!(out.reachable, "{out:?}");
        assert!(path.is_file());
        // Second probe sees the empty file.
        assert!(
            probe(
                Engine::Sqlite,
                &Target::File { path: path.clone() },
                None,
                opts()
            )
            .reachable
        );

        fs::write(&path, b"SQLite format 3\0more").unwrap();
        assert!(
            probe(
                Engine::Sqlite,
                &Target::File { path: path.clone() },
                None,
                opts()
            )
            .reachable
        );

        fs::write(&path, b"not a database").unwrap();
        let out = probe(
            Engine::Sqlite,
            &Target::File { path: path.clone() },
            None,
            opts(),
        );
        assert!(!out.reachable);
        assert!(out.error.unwrap().contains("not a SQLite database"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn early_close_has_a_readable_error() {
        let (port, h) = fake_server(0, Vec::new());
        let out = probe(Engine::MySql, &tcp(port), None, opts());
        h.join().unwrap();
        assert!(!out.reachable);
        let err = out.error.unwrap();
        assert!(err.contains("connection closed by the peer"), "{err}");
    }

    #[test]
    fn target_display() {
        assert_eq!(tcp(5).to_string(), "127.0.0.1:5");
        let v6 = Target::Tcp {
            host: "::1".to_owned(),
            port: 5,
        };
        assert_eq!(v6.to_string(), "[::1]:5");
    }
}
