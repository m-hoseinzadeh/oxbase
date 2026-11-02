//! The in-process SOCKS5 dialer (see the [`crate::driver`] module docs, *Addressing, TLS, SSH*).
//!
//! One [`Dialer`] per client: a SOCKS5 server (RFC 1928, `CONNECT` only) with username /
//! password authentication (RFC 1929) on `127.0.0.1:0`. The `mongodb` crate is configured with
//! it as its `socks5_proxy` and its own TLS off; every connection the client opens (monitoring,
//! pool, cancel) comes here, and the dialer:
//!
//! 1. checks the per-client random credentials (anything else is refused);
//! 2. maps the requested `host:port` to a socket address: the profile's server endpoint goes to
//!    [`ConnectParams::dial_endpoint`] (the SSH tunnel when there is one), other hosts
//!    (replica-set members, SRV results) are dialled as named, and are refused when the
//!    profile is tunnelled (a tunnel reaches one server);
//! 3. runs the TLS handshake with `oxbase_net::tls::client_config` when the profile asks for
//!    TLS, verifying [`ConnectParams::tls_server_name`] for the profile's own endpoint and the
//!    requested host for other members (`Prefer` falls back to plain TCP when the handshake
//!    fails);
//! 4. answers the SOCKS request and copies bytes both ways until either side closes or the
//!    dialer stops.
//!
//! Failures are recorded ([`Dialer::last_error`]) so the driver can report the real cause (a
//! certificate that does not match, a refused tunnel) instead of the client's generic server
//! selection timeout. Neither the credentials nor the client's traffic are ever logged.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use oxbase_driver::ConnectParams;
use oxbase_net::tls::rustls::ClientConfig;
use oxbase_net::tls::rustls::pki_types::ServerName;
use oxbase_types::{Endpoint, Error, Result, TlsMode, TlsOptions};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_util::sync::CancellationToken;

/// SOCKS protocol version 5.
const SOCKS_VERSION: u8 = 5;
/// RFC 1929 sub-negotiation version.
const AUTH_VERSION: u8 = 1;
/// Method: username / password.
const METHOD_PASSWORD: u8 = 2;
/// Method answer: no acceptable method.
const METHOD_NONE_ACCEPTABLE: u8 = 0xff;
const CMD_CONNECT: u8 = 1;
const ATYP_V4: u8 = 1;
const ATYP_DOMAIN: u8 = 3;
const ATYP_V6: u8 = 4;
/// Reply codes (RFC 1928 §6).
const REP_SUCCEEDED: u8 = 0;
const REP_GENERAL_FAILURE: u8 = 1;
const REP_NOT_ALLOWED: u8 = 2;
const REP_HOST_UNREACHABLE: u8 = 4;
const REP_REFUSED: u8 = 5;
const REP_COMMAND_NOT_SUPPORTED: u8 = 7;
const REP_ADDRESS_NOT_SUPPORTED: u8 = 8;

/// Longest the SOCKS greeting and request of the client may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// What the first finished connection to a server used (for `tls_in_use`).
const TLS_UNKNOWN: u8 = 0;
const TLS_YES: u8 = 1;
const TLS_NO: u8 = 2;

/// The TLS side of the dialer, built once per client.
struct TlsSetup {
    mode: TlsMode,
    config: Arc<ClientConfig>,
    /// The profile's TLS options (its `server_name` applies to the profile's own host only).
    options: TlsOptions,
}

/// State shared by the accept loop and every proxied connection.
struct Shared {
    /// The profile's server endpoint (`host`, effective port), compared case-insensitively.
    server: Option<Endpoint>,
    /// Where the profile's server endpoint is dialled (the tunnel, else the server itself).
    dial: Option<Endpoint>,
    /// `true` when sockets go to a tunnel: no other host may be dialled.
    tunnelled: bool,
    /// The name TLS verifies for the profile's own endpoint.
    server_tls_name: Option<String>,
    tls: Option<TlsSetup>,
    connect_timeout: Duration,
    username: String,
    password: String,
    tls_state: AtomicU8,
    /// `Prefer` only: the server answered the TLS handshake with garbage once; later
    /// connections go plain straight away.
    prefer_plain: AtomicBool,
    last_error: Mutex<Option<String>>,
    /// Counts dial failures, so the connect step can stop waiting at the first one.
    failures: watch::Sender<u64>,
}

/// A running dialer. Stops (closing every proxied connection) when dropped.
pub(crate) struct Dialer {
    addr: SocketAddr,
    shared: Arc<Shared>,
    stop: CancellationToken,
}

impl fmt::Debug for Dialer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the credentials.
        f.debug_struct("Dialer")
            .field("addr", &self.addr)
            .field("server", &self.shared.server)
            .field("tunnelled", &self.shared.tunnelled)
            .finish_non_exhaustive()
    }
}

impl Drop for Dialer {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Dialer {
    /// Starts a dialer for `params` on `127.0.0.1:0`.
    pub(crate) async fn start(params: &ConnectParams, connect_timeout: Duration) -> Result<Self> {
        let tls = match params.tls.mode {
            TlsMode::Disable => None,
            mode => {
                let config = oxbase_net::tls::client_config(&params.tls)?;
                Some(TlsSetup {
                    mode,
                    config,
                    options: params.tls.clone(),
                })
            }
        };
        if let (Some(setup), Some(host)) = (&tls, params.host.as_deref()) {
            // Fail early on a name VerifyFull cannot use (an empty or malformed name).
            oxbase_net::tls::server_name(&setup.options, host.trim())?;
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| Error::connection(format!("cannot start the MongoDB dialer: {e}")))?;
        let addr = listener
            .local_addr()
            .map_err(|e| Error::connection(format!("cannot start the MongoDB dialer: {e}")))?;
        let (failures, _) = watch::channel(0);
        let shared = Arc::new(Shared {
            server: params.server_endpoint(),
            dial: params.dial_endpoint(),
            tunnelled: params.is_tunnelled() || params.ssh.is_some(),
            server_tls_name: params.tls_server_name().map(str::to_owned),
            tls,
            connect_timeout,
            username: random_token(),
            password: random_token(),
            tls_state: AtomicU8::new(TLS_UNKNOWN),
            prefer_plain: AtomicBool::new(false),
            last_error: Mutex::new(None),
            failures,
        });
        let stop = CancellationToken::new();
        tokio::spawn(accept_loop(listener, Arc::clone(&shared), stop.clone()));
        Ok(Self { addr, shared, stop })
    }

    /// The loopback port the client must use as its SOCKS5 proxy.
    pub(crate) fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The per-client proxy credentials (username, password). Random; never logged.
    pub(crate) fn credentials(&self) -> (String, String) {
        (self.shared.username.clone(), self.shared.password.clone())
    }

    /// Whether the first finished connection used TLS.
    pub(crate) fn tls_in_use(&self) -> bool {
        self.shared.tls_state.load(Ordering::Acquire) == TLS_YES
    }

    /// The most recent dial failure, as text naming the server endpoint.
    pub(crate) fn last_error(&self) -> Option<String> {
        self.shared
            .last_error
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Resolves when a dial fails after this call.
    pub(crate) fn failures(&self) -> watch::Receiver<u64> {
        self.shared.failures.subscribe()
    }

    /// Stops accepting and closes every proxied connection.
    pub(crate) fn stop(&self) {
        self.stop.cancel();
    }
}

/// 32 random bytes as hex (two v4 UUIDs: 244 random bits from the OS generator).
fn random_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

async fn accept_loop(listener: TcpListener, shared: Arc<Shared>, stop: CancellationToken) {
    loop {
        let accepted = tokio::select! {
            () = stop.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((socket, _)) => {
                let shared = Arc::clone(&shared);
                let stop = stop.child_token();
                tokio::spawn(async move {
                    tokio::select! {
                        () = stop.cancelled() => {}
                        () = serve(socket, &shared) => {}
                    }
                });
            }
            Err(e) => {
                tracing::debug!(error = %e, "MongoDB dialer: accept failed");
                // Avoid a hot loop when the process is out of descriptors.
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// One proxied connection: SOCKS handshake, upstream dial (+ TLS), then the byte pump.
async fn serve(mut client: TcpStream, shared: &Shared) {
    let _ = client.set_nodelay(true);
    let target = match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(&mut client, shared)).await
    {
        Ok(Ok(target)) => target,
        Ok(Err(reply)) => {
            if let Some(code) = reply {
                let _ = send_reply(&mut client, code).await;
            }
            return;
        }
        Err(_) => return,
    };
    let mut upstream = match connect_upstream(shared, &target).await {
        Ok(stream) => stream,
        Err((code, message)) => {
            shared.record_failure(message);
            let _ = send_reply(&mut client, code).await;
            return;
        }
    };
    if send_reply(&mut client, REP_SUCCEEDED).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

impl Shared {
    fn record_failure(&self, message: String) {
        tracing::debug!(%message, "MongoDB dialer: connection failed");
        if let Ok(mut last) = self.last_error.lock() {
            *last = Some(message);
        }
        self.failures.send_modify(|n| *n += 1);
    }

    /// Whether `host:port` is the profile's own server endpoint.
    fn is_server(&self, target: &Endpoint) -> bool {
        self.server.as_ref().is_some_and(|server| {
            server.port == target.port && same_host(&server.host, &target.host)
        })
    }
}

fn same_host(a: &str, b: &str) -> bool {
    let trim = |h: &str| h.trim().trim_start_matches('[').trim_end_matches(']');
    let (a, b) = (trim(a), trim(b));
    match (a.parse::<IpAddr>(), b.parse::<IpAddr>()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a.eq_ignore_ascii_case(b),
    }
}

/// The SOCKS5 greeting, authentication and request. `Err(Some(code))`: answer the request
/// with `code` and close; `Err(None)`: just close (the protocol already answered).
async fn handshake(
    client: &mut TcpStream,
    shared: &Shared,
) -> std::result::Result<Endpoint, Option<u8>> {
    // Greeting: VER NMETHODS METHODS...
    let mut head = [0u8; 2];
    client.read_exact(&mut head).await.map_err(|_| None)?;
    if head[0] != SOCKS_VERSION {
        return Err(None);
    }
    let mut methods = vec![0u8; usize::from(head[1])];
    client.read_exact(&mut methods).await.map_err(|_| None)?;
    if !methods.contains(&METHOD_PASSWORD) {
        let _ = client
            .write_all(&[SOCKS_VERSION, METHOD_NONE_ACCEPTABLE])
            .await;
        return Err(None);
    }
    client
        .write_all(&[SOCKS_VERSION, METHOD_PASSWORD])
        .await
        .map_err(|_| None)?;

    // RFC 1929: VER ULEN UNAME PLEN PASSWD
    let mut version = [0u8; 1];
    client.read_exact(&mut version).await.map_err(|_| None)?;
    if version[0] != AUTH_VERSION {
        return Err(None);
    }
    let username = read_short(client).await.map_err(|_| None)?;
    let password = read_short(client).await.map_err(|_| None)?;
    let ok = constant_time_eq(&username, shared.username.as_bytes())
        & constant_time_eq(&password, shared.password.as_bytes());
    client
        .write_all(&[AUTH_VERSION, u8::from(!ok)])
        .await
        .map_err(|_| None)?;
    if !ok {
        return Err(None);
    }

    // Request: VER CMD RSV ATYP DST.ADDR DST.PORT
    let mut request = [0u8; 4];
    client.read_exact(&mut request).await.map_err(|_| None)?;
    if request[0] != SOCKS_VERSION {
        return Err(None);
    }
    let host = match request[3] {
        ATYP_V4 => {
            let mut ip = [0u8; 4];
            client.read_exact(&mut ip).await.map_err(|_| None)?;
            Ipv4Addr::from(ip).to_string()
        }
        ATYP_V6 => {
            let mut ip = [0u8; 16];
            client.read_exact(&mut ip).await.map_err(|_| None)?;
            Ipv6Addr::from(ip).to_string()
        }
        ATYP_DOMAIN => {
            let name = read_short(client).await.map_err(|_| None)?;
            String::from_utf8(name).map_err(|_| Some(REP_ADDRESS_NOT_SUPPORTED))?
        }
        _ => return Err(Some(REP_ADDRESS_NOT_SUPPORTED)),
    };
    let mut port = [0u8; 2];
    client.read_exact(&mut port).await.map_err(|_| None)?;
    if request[1] != CMD_CONNECT {
        return Err(Some(REP_COMMAND_NOT_SUPPORTED));
    }
    Ok(Endpoint::new(host, u16::from_be_bytes(port)))
}

/// Reads a one-byte length and that many bytes.
async fn read_short(client: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 1];
    client.read_exact(&mut len).await?;
    let mut out = vec![0u8; usize::from(len[0])];
    client.read_exact(&mut out).await?;
    Ok(out)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn send_reply(client: &mut TcpStream, code: u8) -> io::Result<()> {
    // BND.ADDR 0.0.0.0:0: the client does not use it.
    client
        .write_all(&[SOCKS_VERSION, code, 0, ATYP_V4, 0, 0, 0, 0, 0, 0])
        .await?;
    client.flush().await
}

/// Dials `target` (see the module docs) and runs TLS when the profile asks for it. On failure:
/// the SOCKS reply code and a message naming the server endpoint.
async fn connect_upstream(
    shared: &Shared,
    target: &Endpoint,
) -> std::result::Result<Upstream, (u8, String)> {
    let own = shared.is_server(target);
    let dial = if own {
        shared.dial.clone().unwrap_or_else(|| target.clone())
    } else if shared.tunnelled {
        return Err((
            REP_NOT_ALLOWED,
            format!(
                "the server announced {target}, which the SSH tunnel cannot reach (an SSH \
                 profile connects to one server)"
            ),
        ));
    } else {
        target.clone()
    };
    let Some(setup) = &shared.tls else {
        let tcp = dial_tcp(shared, target, &dial).await?;
        shared.note_tls(false);
        return Ok(Upstream::Plain(tcp));
    };
    if setup.mode == TlsMode::Prefer && shared.prefer_plain.load(Ordering::Acquire) {
        let tcp = dial_tcp(shared, target, &dial).await?;
        return Ok(Upstream::Plain(tcp));
    }
    let name = if own {
        let host = shared
            .server_tls_name
            .clone()
            .unwrap_or_else(|| target.host.clone());
        // `server_tls_name` already applied the override.
        oxbase_net::tls::server_name(&TlsOptions::new(setup.mode), &host)
    } else {
        // `tls.server_name` overrides only the profile's own host.
        let mut options = setup.options.clone();
        options.server_name = None;
        oxbase_net::tls::server_name(&options, &target.host)
    }
    .map_err(|e| (REP_GENERAL_FAILURE, format!("TLS for {target}: {e}")))?;
    let tcp = dial_tcp(shared, target, &dial).await?;
    let connector = TlsConnector::from(Arc::clone(&setup.config));
    let handshake = tokio::time::timeout(shared.connect_timeout, connector.connect(name, tcp));
    match handshake.await {
        Ok(Ok(tls)) => {
            shared.note_tls(true);
            Ok(Upstream::Tls(Box::new(tls)))
        }
        Ok(Err(e)) if setup.mode == TlsMode::Prefer => {
            tracing::debug!(error = %e, "MongoDB dialer: TLS refused, falling back to plain TCP");
            shared.prefer_plain.store(true, Ordering::Release);
            let tcp = dial_tcp(shared, target, &dial).await?;
            shared.note_tls(false);
            Ok(Upstream::Plain(tcp))
        }
        Ok(Err(e)) => Err((
            REP_GENERAL_FAILURE,
            format!("TLS handshake with {target} failed: {e}"),
        )),
        Err(_) => Err((
            REP_GENERAL_FAILURE,
            format!(
                "TLS handshake with {target} did not finish within {} s",
                shared.connect_timeout.as_secs()
            ),
        )),
    }
}

impl Shared {
    fn note_tls(&self, tls: bool) {
        let state = if tls { TLS_YES } else { TLS_NO };
        let _ = self.tls_state.compare_exchange(
            TLS_UNKNOWN,
            state,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

async fn dial_tcp(
    shared: &Shared,
    target: &Endpoint,
    dial: &Endpoint,
) -> std::result::Result<TcpStream, (u8, String)> {
    let connect = TcpStream::connect((
        dial.host.trim_start_matches('[').trim_end_matches(']'),
        dial.port,
    ));
    match tokio::time::timeout(shared.connect_timeout, connect).await {
        Ok(Ok(tcp)) => {
            let _ = tcp.set_nodelay(true);
            Ok(tcp)
        }
        Ok(Err(e)) => {
            let code = if e.kind() == io::ErrorKind::ConnectionRefused {
                REP_REFUSED
            } else {
                REP_HOST_UNREACHABLE
            };
            Err((code, format!("cannot connect to {target}: {e}")))
        }
        Err(_) => Err((
            REP_HOST_UNREACHABLE,
            format!(
                "cannot connect to {target}: no answer within {} s",
                shared.connect_timeout.as_secs()
            ),
        )),
    }
}

/// The server side of a proxied connection.
enum Upstream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Upstream {
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

impl AsyncWrite for Upstream {
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
    use oxbase_types::Engine;

    /// A SOCKS5 client conversation, by hand: greeting, auth, CONNECT to `host:port`.
    async fn socks_connect(
        proxy: u16,
        user: &str,
        pass: &str,
        host: &str,
        port: u16,
    ) -> io::Result<(TcpStream, u8)> {
        let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy)).await?;
        s.write_all(&[5, 2, 0, 2]).await?;
        let mut choice = [0u8; 2];
        s.read_exact(&mut choice).await?;
        assert_eq!(choice, [5, 2], "the dialer insists on username / password");
        let mut auth = vec![1, u8::try_from(user.len()).unwrap()];
        auth.extend_from_slice(user.as_bytes());
        auth.push(u8::try_from(pass.len()).unwrap());
        auth.extend_from_slice(pass.as_bytes());
        s.write_all(&auth).await?;
        let mut status = [0u8; 2];
        s.read_exact(&mut status).await?;
        if status[1] != 0 {
            return Ok((s, 0xff));
        }
        let mut req = vec![5, 1, 0, 3, u8::try_from(host.len()).unwrap()];
        req.extend_from_slice(host.as_bytes());
        req.extend_from_slice(&port.to_be_bytes());
        s.write_all(&req).await?;
        let mut reply = [0u8; 10];
        s.read_exact(&mut reply).await?;
        Ok((s, reply[1]))
    }

    async fn echo_server() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        port
    }

    fn params(host: &str, port: u16) -> ConnectParams {
        ConnectParams::network(Engine::MongoDb, host)
            .with_port(port)
            .with_tls(TlsOptions::new(TlsMode::Disable))
    }

    #[tokio::test]
    async fn proxies_the_server_endpoint_and_checks_credentials() {
        let echo = echo_server().await;
        let dialer = Dialer::start(&params("localhost", echo), Duration::from_secs(5))
            .await
            .unwrap();
        let (user, pass) = dialer.credentials();
        assert_eq!(user.len(), 64);
        assert_ne!(user, pass);
        assert!(!format!("{dialer:?}").contains(&pass));

        let (mut s, code) = socks_connect(dialer.port(), &user, &pass, "LOCALHOST", echo)
            .await
            .unwrap();
        assert_eq!(code, REP_SUCCEEDED);
        s.write_all(b"ping").await.unwrap();
        let mut back = [0u8; 4];
        s.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"ping");
        assert!(!dialer.tls_in_use());

        let (_, code) = socks_connect(dialer.port(), &user, "wrong", "localhost", echo)
            .await
            .unwrap();
        assert_eq!(code, 0xff, "a wrong password is refused");
    }

    #[tokio::test]
    async fn the_tunnel_endpoint_is_dialled_and_other_hosts_are_refused() {
        let echo = echo_server().await;
        // The profile names a host nobody can resolve; the "tunnel" is the echo server.
        let p = params("mongodb.oxbase.invalid", 27017)
            .with_dial_addr(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), echo));
        let dialer = Dialer::start(&p, Duration::from_secs(5)).await.unwrap();
        let (user, pass) = dialer.credentials();
        let (mut s, code) =
            socks_connect(dialer.port(), &user, &pass, "mongodb.oxbase.invalid", 27017)
                .await
                .unwrap();
        assert_eq!(code, REP_SUCCEEDED);
        s.write_all(b"x").await.unwrap();
        let mut back = [0u8; 1];
        s.read_exact(&mut back).await.unwrap();

        let mut failures = dialer.failures();
        let (_, code) = socks_connect(dialer.port(), &user, &pass, "other.example", 27017)
            .await
            .unwrap();
        assert_eq!(code, REP_NOT_ALLOWED);
        failures.changed().await.unwrap();
        let why = dialer.last_error().unwrap();
        assert!(why.contains("other.example:27017"), "{why}");
    }

    #[tokio::test]
    async fn a_refused_dial_is_reported_and_stop_closes_the_listener() {
        // Bind and drop: nothing listens on that port any more.
        let port = {
            let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            l.local_addr().unwrap().port()
        };
        let dialer = Dialer::start(&params("127.0.0.1", port), Duration::from_secs(5))
            .await
            .unwrap();
        let (user, pass) = dialer.credentials();
        let (_, code) = socks_connect(dialer.port(), &user, &pass, "127.0.0.1", port)
            .await
            .unwrap();
        assert_ne!(code, REP_SUCCEEDED);
        assert!(dialer.last_error().unwrap().contains("cannot connect to"));
        dialer.stop();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            socks_connect(dialer.port(), &user, &pass, "127.0.0.1", port)
                .await
                .is_err()
        );
    }

    #[test]
    fn host_comparison() {
        assert!(same_host("LocalHost", "localhost"));
        assert!(same_host("[::1]", "::1"));
        assert!(same_host("127.0.0.1", "127.0.0.1"));
        assert!(!same_host("127.0.0.1", "localhost"));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
