//! The MongoDB driver (mongo-driver stream): [`MongoDriver`] and [`MongoConnection`] on the
//! official `mongodb` crate (3.9, tokio, BSON 3). Contract: `docs/m7-contracts.md` §2.1, §4.2.
//! This file's public items are the Design contract; the stream fills the bodies and adds
//! private modules under `src/driver/` (the SOCKS5 dialer, TLS, cancel).
//!
//! # Addressing, TLS, SSH: the in-process SOCKS5 dialer
//!
//! The `mongodb` crate dials the host names it is given and verifies TLS against the same
//! name; it has no hook for another dial address, no chain-only verification, no platform
//! roots and no in-memory client key. Its one connection hook is `socks5-proxy`
//! (`ClientOptions::socks5_proxy`): it then asks a SOCKS5 proxy to `CONNECT` to each host
//! *by name* and speaks to whatever stream the proxy returns. So every client gets its own
//! **dialer**: a SOCKS5 server (RFC 1928, username / password auth RFC 1929) that oxbase runs
//! in-process.
//!
//! - It listens on `127.0.0.1:0` only, for the lifetime of the client, and accepts only the
//!   random 32-byte username / password generated for that client (so no other local process
//!   can use it). The driver's own TLS stays **off**; the dialer does TLS.
//! - On `CONNECT host:port` it dials: the SSH tunnel ([`ConnectParams::dial_endpoint`]) when
//!   `host:port` is the profile's server endpoint and the profile has SSH (any other host is
//!   refused: SSH profiles are direct connections to one server); otherwise `host:port`
//!   itself (replica-set members and SRV results as the server announces them).
//! - With TLS (`TlsMode` other than `Disable`) it runs the handshake with
//!   `oxbase_net::tls::client_config(&params.tls)` (ring provider; `VerifyCa`, platform roots,
//!   inline PEM and keychain client keys all as M1 §3) and verifies
//!   [`ConnectParams::tls_server_name`] for the profile's server endpoint, or the requested
//!   host for other members (`tls.server_name` overrides only the profile's own host). `Prefer`
//!   tries TLS and falls back to plain TCP on a handshake failure (MongoDB has no in-band
//!   negotiation); [`MongoConnection::tls_in_use`] reports what the first connection got.
//! - The bytes between the driver and the dialer cross the loopback interface unencrypted
//!   inside this process's own sockets; TLS covers everything from the dialer to the server.
//!
//! # Client options
//!
//! From [`crate::options::MongoOptions`]: hosts (or `mongodb+srv://` resolution through the
//! crate's hickory resolver; SRV hosts then pass the dialer like any member), `replicaSet`,
//! `directConnection`, `readPreference`, `appName` = `ConnectParams::application_name`,
//! `connectTimeout` / `serverSelectionTimeout` = `ConnectParams::connect_timeout` (default
//! 15 s), `maxPoolSize` 4 (an editor session is one logical session; the pool covers a cancel
//! and a cursor at the same time).
//!
//! # Auth
//!
//! `Credential { username, password, source: auth_source, mechanism }`: SCRAM-SHA-256 /
//! SCRAM-SHA-1 negotiated by default (the crate implements both; no password leaves the
//! client), `MONGODB-X509` (the client certificate of the dialer's TLS session, no password),
//! `PLAIN` (LDAP, only over verified TLS). Auth failures (code 18) are `Error::Auth`; the
//! password never appears in an error, a log or `Debug`.
//!
//! # Connect
//!
//! `Client::with_options`, then `hello` / `buildInfo` on the admin database for
//! [`MongoConnection::server_version`] (`version`, e.g. `7.0.45`), within the connect timeout.
//! Server selection errors become `Error::Connection` naming the server endpoint (never the
//! dialer's loopback address).
//!
//! # Statements (`Connection::execute`)
//!
//! `params` must be empty (`InvalidInput` otherwise). The statement text is one of:
//!
//! - a **request**: text whose first non-blank character is `{` is a JSON
//!   `oxbase_types::DocumentQuery` (the builder's form, as `document_find` /
//!   `document_aggregate` take it: `{"type": "find", "collection": {...}, "filter": ...}`),
//!   with the same rules as [`crate::request::RequestSource`] (Extended JSON values with the
//!   `$regex` operator exception, `batchSize` 1000, the operation comment), or a **command
//!   request** `{"type": "command", "database": "admin", "command": {"ping": 1}}` (the
//!   driver's own form: one `runCommand`, its reply as one row; cursor-returning commands
//!   belong in find / aggregate requests). The driver runs both itself (private
//!   `driver::{statement, run, convert}`), so they work without the shell;
//! - a **shell statement**: anything else, evaluated by the session's
//!   [`crate::shell::ShellSession`] (`db` = [`MongoConnection::current_database`]).
//!
//! Either way the stream yields Arrow result sets as described in `oxbase_dialect::document`
//! (documents: the inferred columns plus `$document`; scalars and command replies: one row),
//! `RowsAffected` for writes (`insertOne` 1, `updateMany` the modified count, ...) and notices
//! (the shell's `print`, write-concern and schema-sample warnings).
//!
//! `begin()` is `Unsupported` in M7 (multi-document transactions need a replica set and are
//! not offered; document edits apply one by one, `oxbase_types::DocumentEdit`).
//!
//! # Cancel (M1 §7)
//!
//! Every operation the connection sends carries `comment: "oxbase:<connection id>:<op
//! counter>"`. `cancel_running` and the token: stop reading (dropping the cursor sends
//! `killCursors`), then, on another pooled connection of the same client (through the
//! dialer, so through SSH), find the operation with `$currentOp {ownOps: true}` matching that
//! comment and `killOp` it (users may always kill their own operations). The stream ends with
//! one `Error::Cancelled` within 2 s; the session stays usable. A JavaScript loop of the shell
//! is interrupted by the shell's interrupt handler (see [`crate::shell`]).
//!
//! # Streaming
//!
//! Cursors read with `batchSize` 1000 (the first batch smaller when the result is small); each
//! server batch becomes Arrow batches as it arrives, so a 200 000-document find streams into
//! the result store without being held in memory; the run's `maxRows` / disk cap drop the
//! cursor (→ `killCursors`).

mod convert;
mod dialer;
mod error;
mod run;
mod statement;

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bson::doc;
use mongodb::options::{
    AuthMechanism as MongoAuthMechanism, ClientOptions, Credential, ReadPreference as MongoRead,
    SelectionCriteria, ServerAddress, Socks5Proxy,
};
use oxbase_driver::{
    CancellationToken, ConnectParams, Connection, Driver, DriverRegistry, ResultStream, Tx,
};
use oxbase_types::{Engine, Error, Result, ServerVersion, Value};

use crate::options::{AuthMechanism, MongoOptions, ReadPreference};
use crate::shell::{ShellLimits, ShellSession};

use self::dialer::Dialer;
use self::error::{Redactor, map_error};

/// Connect timeout when the profile has none.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Pool size of one session's client (driver module docs, *Client options*).
const MAX_POOL_SIZE: u32 = 4;
/// The shell's database when the profile names none (as in mongosh).
const DEFAULT_DATABASE: &str = "test";

/// Opens [`MongoConnection`]s.
#[derive(Debug, Clone, Copy, Default)]
pub struct MongoDriver;

impl MongoDriver {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for MongoDriver {
    fn engine(&self) -> Engine {
        Engine::MongoDb
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        let connection = MongoConnection::connect(params).await?;
        Ok(Box::new(connection))
    }
}

/// Adds [`MongoDriver`] for [`Engine::MongoDb`] to `drivers` (called by
/// `oxbase_services::default_drivers` once the mongo-driver stream uncomments its line).
pub fn register(drivers: &mut DriverRegistry) {
    drivers.register(Arc::new(MongoDriver::new()));
}

/// One MongoDB session: a `mongodb::Client` behind its own dialer, the shell's JavaScript
/// session and the current database. See the module docs.
pub struct MongoConnection {
    params: ConnectParams,
    options: MongoOptions,
    version: ServerVersion,
    client: mongodb::Client,
    shell: ShellSession,
    shared: Arc<Shared>,
}

/// Session state shared with the statement tasks.
struct Shared {
    /// Random id in every operation comment (`oxbase:<id>:<n>`), so a cancel never matches
    /// another session's (or another process's) operations.
    id: String,
    counter: AtomicU64,
    current_database: Mutex<String>,
    closed: AtomicBool,
    /// Statements running on this connection, by run number.
    runs: Mutex<HashMap<u64, CancellationToken>>,
    next_run: AtomicU64,
    redactor: Redactor,
    /// Kept alive as long as any statement task runs; stops when the last holder drops.
    dialer: Dialer,
}

impl fmt::Debug for MongoConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // No credentials: the params' `Debug` redacts them, but only the facts are shown.
        f.debug_struct("MongoConnection")
            .field("server", &self.params.server_endpoint())
            .field("version", &self.version.raw())
            .field("tls_in_use", &self.shared.dialer.tls_in_use())
            .field("id", &self.shared.id)
            .finish_non_exhaustive()
    }
}

impl MongoConnection {
    /// Connects as described in the module docs. Bounded by `ConnectParams::connect_timeout`
    /// (default 15 s).
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        params.validate()?;
        oxbase_net::tls::install_default_crypto_provider();
        let options = MongoOptions::from_params(params)?;
        let timeout = params.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT);
        let dialer = Dialer::start(params, timeout).await?;
        let redactor = Redactor::new(params.password.as_ref());
        let client_options = client_options(params, &options, &dialer, timeout, &redactor).await?;
        // Subscribed before the client exists: its monitors dial at once.
        let failures = dialer.failures();
        let client =
            mongodb::Client::with_options(client_options).map_err(|e| map_error(&e, &redactor))?;
        let hello = handshake(&client, &dialer, failures, &options, timeout, &redactor);
        let version = match hello.await {
            Ok(version) => version,
            Err(e) => {
                let shutdown = client.clone().shutdown().immediate(true);
                let _ = tokio::time::timeout(Duration::from_secs(2), shutdown).await;
                return Err(connect_error(e, params, &dialer));
            }
        };
        let database = params
            .database
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .unwrap_or(DEFAULT_DATABASE)
            .to_owned();
        let shared = Arc::new(Shared {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_owned(),
            counter: AtomicU64::new(0),
            current_database: Mutex::new(database),
            closed: AtomicBool::new(false),
            runs: Mutex::new(HashMap::new()),
            next_run: AtomicU64::new(0),
            redactor,
            dialer,
        });
        Ok(Self {
            params: params.clone(),
            shell: ShellSession::new(ShellLimits::from_options(&options)),
            options,
            version,
            client,
            shared,
        })
    }

    /// The parameters this session was opened with.
    pub fn params(&self) -> &ConnectParams {
        &self.params
    }

    /// The parsed profile options.
    pub fn options(&self) -> &MongoOptions {
        &self.options
    }

    /// The client (behind the dialer). Dialect cells of this crate run their commands through
    /// it, always with this connection's operation comment ([`MongoConnection::next_comment`])
    /// so a cancel finds them.
    pub fn client(&self) -> &mongodb::Client {
        &self.client
    }

    /// The shell session of this connection (its JavaScript globals survive between
    /// statements of the editor).
    pub fn shell(&self) -> &ShellSession {
        &self.shell
    }

    /// A fresh `comment` for the next operation (`oxbase:<connection id>:<counter>`), which
    /// [`Connection::cancel_running`] matches in `$currentOp`.
    pub fn next_comment(&self) -> String {
        let n = self.shared.counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("oxbase:{}:{n}", self.shared.id)
    }

    /// `use <db>` in the shell: the database later statements run against.
    #[allow(dead_code, reason = "for the shell stream's `use <db>`")]
    pub(crate) fn set_current_database(&self, database: &str) {
        if let Ok(mut current) = self.shared.current_database.lock() {
            database.clone_into(&mut current);
        }
    }

    /// Kills the server operations started with `comment` (crate cells that run their own
    /// cursors call this when their token fires). Bounded by the cancel deadline.
    #[allow(dead_code, reason = "for the mongo and shell streams' own cursors")]
    pub(crate) async fn kill_operation(&self, comment: &str) -> Result<()> {
        let filter = bson::Bson::String(comment.to_owned());
        match tokio::time::timeout(run::KILL_TIMEOUT, run::kill_ops(&self.client, filter)).await {
            Ok(result) => result.map(|_| ()),
            Err(_) => Err(Error::timeout("the MongoDB cancel did not finish in time")),
        }
    }

    fn ensure_open(&self) -> Result<()> {
        if self.shared.closed.load(Ordering::Acquire) {
            Err(Error::connection("the MongoDB connection is closed"))
        } else {
            Ok(())
        }
    }

    /// Runs a JSON statement on its own task (see `run`).
    fn spawn_statement(
        &self,
        statement: statement::Statement,
        cancel: &CancellationToken,
    ) -> ResultStream {
        let token = cancel.child_token();
        let run_id = self.shared.next_run.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut runs) = self.shared.runs.lock() {
            runs.insert(run_id, token.clone());
        }
        let ctx = run::RunContext {
            client: self.client.clone(),
            redactor: self.shared.redactor.clone(),
            comment: self.next_comment(),
            sample_size: self.options.sample_size,
        };
        let shared = Arc::clone(&self.shared);
        run::spawn(ctx, statement, token, move || {
            if let Ok(mut runs) = shared.runs.lock() {
                runs.remove(&run_id);
            }
        })
    }
}

/// `buildInfo` on `admin` (which also authenticates the first pooled connection), stopped
/// early when a direct connection's dial fails (a TLS name mismatch, a refused tunnel).
async fn handshake(
    client: &mongodb::Client,
    dialer: &Dialer,
    mut failures: tokio::sync::watch::Receiver<u64>,
    options: &MongoOptions,
    timeout: Duration,
    redactor: &Redactor,
) -> Result<ServerVersion> {
    let direct = options.direct_connection && !options.srv;
    let admin = client.database("admin");
    let build_info = admin.run_command(doc! {"buildInfo": 1_i32});
    let reply = tokio::select! {
        reply = build_info => reply.map_err(|e| map_error(&e, redactor))?,
        _ = failures.changed(), if direct => {
            return Err(Error::connection(
                dialer.last_error().unwrap_or_else(|| "the connection failed".to_owned()),
            ));
        }
        () = tokio::time::sleep(timeout + Duration::from_secs(1)) => {
            return Err(Error::timeout(format!(
                "no answer from MongoDB within {} s",
                timeout.as_secs()
            )));
        }
    };
    let version = reply
        .get_str("version")
        .map_err(|_| Error::internal("buildInfo returned no version"))?;
    ServerVersion::parse(version)
}

/// Prefers the dialer's own failure (it names the real cause) over the client's generic
/// server-selection message.
fn connect_error(e: Error, params: &ConnectParams, dialer: &Dialer) -> Error {
    match e {
        Error::Connection { message } => {
            let server = params
                .server_endpoint()
                .map_or_else(|| "the MongoDB server".to_owned(), |s| s.to_string());
            match dialer.last_error() {
                Some(cause) => Error::connection(cause),
                None => Error::connection(format!("cannot reach {server}: {message}")),
            }
        }
        other => other,
    }
}

async fn client_options(
    params: &ConnectParams,
    options: &MongoOptions,
    dialer: &Dialer,
    timeout: Duration,
    redactor: &Redactor,
) -> Result<ClientOptions> {
    let mut o = if options.srv {
        let name = options
            .hosts
            .first()
            .map(|h| h.host.clone())
            .ok_or_else(|| Error::invalid_input("the MongoDB profile has no host"))?;
        ClientOptions::parse(format!("mongodb+srv://{name}"))
            .await
            .map_err(|e| map_error(&e, redactor))?
    } else {
        let mut o = ClientOptions::default();
        o.hosts = options
            .hosts
            .iter()
            .map(|h| ServerAddress::Tcp {
                host: h
                    .host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_owned(),
                port: Some(h.port),
            })
            .collect();
        o
    };
    // The dialer does TLS (module docs); the client speaks plain bytes to it.
    o.tls = None;
    o.app_name.clone_from(&params.application_name);
    o.connect_timeout = Some(timeout);
    o.server_selection_timeout = Some(timeout);
    o.direct_connection = Some(options.direct_connection);
    if options.replica_set.is_some() {
        o.repl_set_name.clone_from(&options.replica_set);
    }
    o.selection_criteria = Some(SelectionCriteria::ReadPreference(read_preference(
        options.read_preference,
    )));
    o.max_pool_size = Some(MAX_POOL_SIZE);
    o.credential = credential(params, options);
    let (username, password) = dialer.credentials();
    o.socks5_proxy = Some(
        Socks5Proxy::builder()
            .host("127.0.0.1".to_owned())
            .port(dialer.port())
            .authentication((username, password))
            .build(),
    );
    Ok(o)
}

fn read_preference(r: ReadPreference) -> MongoRead {
    match r {
        ReadPreference::Primary => MongoRead::Primary,
        ReadPreference::PrimaryPreferred => MongoRead::PrimaryPreferred { options: None },
        ReadPreference::Secondary => MongoRead::Secondary { options: None },
        ReadPreference::SecondaryPreferred => MongoRead::SecondaryPreferred { options: None },
        ReadPreference::Nearest => MongoRead::Nearest { options: None },
    }
}

/// The client credential: none without a user (except X.509, whose user is the certificate).
fn credential(params: &ConnectParams, options: &MongoOptions) -> Option<Credential> {
    let user = params
        .user
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty());
    let mechanism = options.auth_mechanism.map(|m| match m {
        AuthMechanism::ScramSha256 => MongoAuthMechanism::ScramSha256,
        AuthMechanism::ScramSha1 => MongoAuthMechanism::ScramSha1,
        AuthMechanism::X509 => MongoAuthMechanism::MongoDbX509,
        AuthMechanism::Plain => MongoAuthMechanism::Plain,
    });
    let x509 = options.auth_mechanism == Some(AuthMechanism::X509);
    if user.is_none() && !x509 {
        return None;
    }
    let mut c = Credential::default();
    c.username = user.map(str::to_owned);
    c.password = if x509 {
        None
    } else {
        params.password.as_ref().map(|p| p.expose().to_owned())
    };
    c.source = Some(options.auth_source.clone());
    c.mechanism = mechanism;
    Some(c)
}

#[async_trait]
impl Connection for MongoConnection {
    fn engine(&self) -> Engine {
        Engine::MongoDb
    }

    fn server_version(&self) -> &ServerVersion {
        &self.version
    }

    fn tls_in_use(&self) -> bool {
        self.shared.dialer.tls_in_use()
    }

    /// The shell's current database (`ConnectParams::database` at first, `test` without one;
    /// `use <db>` changes it).
    fn current_database(&self) -> Option<String> {
        self.shared
            .current_database
            .lock()
            .ok()
            .map(|current| current.clone())
    }

    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        self.ensure_open()?;
        if !params.is_empty() {
            return Err(Error::invalid_input(
                "MongoDB statements take no bound parameters",
            ));
        }
        if sql.trim_start().starts_with('{') {
            let statement = statement::parse(sql)?;
            return Ok(self.spawn_statement(statement, &cancel));
        }
        self.shell.run(self, sql, cancel).await
    }

    async fn cancel_running(&self) -> Result<()> {
        let tokens: Vec<CancellationToken> = self
            .shared
            .runs
            .lock()
            .map(|runs| runs.values().cloned().collect())
            .unwrap_or_default();
        for token in &tokens {
            token.cancel();
        }
        if self.shared.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        // Operations other cells and the shell started with this connection's comments.
        let filter = run::connection_comments(&self.shared.id);
        match tokio::time::timeout(run::KILL_TIMEOUT, run::kill_ops(&self.client, filter)).await {
            Ok(Err(e)) if tokens.is_empty() => Err(e),
            // Done, or the runs' own cancel already killed their operations.
            _ => Ok(()),
        }
    }

    /// Always `Unsupported` in M7 (see the module docs).
    async fn begin(&self) -> Result<Box<dyn Tx>> {
        Err(Error::unsupported(
            "MongoDB transactions are not offered: document edits apply one by one",
        ))
    }

    async fn close(&self) -> Result<()> {
        if self.shared.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Ok(runs) = self.shared.runs.lock() {
            for token in runs.values() {
                token.cancel();
            }
        }
        let shutdown = self.client.clone().shutdown().immediate(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), shutdown).await;
        self.shared.dialer.stop();
        Ok(())
    }

    fn as_any(&self) -> Option<&(dyn std::any::Any + Send + Sync)> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{Secret, TlsMode, TlsOptions};

    fn params() -> ConnectParams {
        ConnectParams::network(Engine::MongoDb, "127.0.0.1")
            .with_port(1)
            .with_tls(TlsOptions::new(TlsMode::Disable))
    }

    #[test]
    fn credentials_follow_the_profile() {
        let p = params();
        let o = MongoOptions::from_params(&p).unwrap();
        assert!(credential(&p, &o).is_none(), "no user: no authentication");

        let p = params().with_user("oxbase").with_password("pw-secret-1");
        let o = MongoOptions::from_params(&p).unwrap();
        let c = credential(&p, &o).unwrap();
        assert_eq!(c.username.as_deref(), Some("oxbase"));
        assert_eq!(c.password.as_deref(), Some("pw-secret-1"));
        assert_eq!(c.source.as_deref(), Some("admin"));
        assert_eq!(c.mechanism, None, "negotiated");
        assert!(!format!("{c:?}").contains("pw-secret"));

        let mut p = params().with_user("oxbase").with_password("pw");
        p.options
            .insert(crate::options::AUTH_MECHANISM.into(), "SCRAM-SHA-1".into());
        p.options
            .insert(crate::options::AUTH_SOURCE.into(), "oxbase_test".into());
        let o = MongoOptions::from_params(&p).unwrap();
        let c = credential(&p, &o).unwrap();
        assert_eq!(c.mechanism, Some(MongoAuthMechanism::ScramSha1));
        assert_eq!(c.source.as_deref(), Some("oxbase_test"));

        let mut p = params().with_password(Secret::new("ignored"));
        p.options
            .insert(crate::options::AUTH_MECHANISM.into(), "MONGODB-X509".into());
        let o = MongoOptions::from_params(&p).unwrap();
        let c = credential(&p, &o).unwrap();
        assert_eq!(c.mechanism, Some(MongoAuthMechanism::MongoDbX509));
        assert_eq!(c.password, None);
        assert_eq!(c.source.as_deref(), Some("$external"));
    }

    #[tokio::test]
    async fn client_options_route_everything_through_the_dialer() {
        let mut p = params().with_user("u").with_password("secret-pw");
        p.application_name = Some("oxbase-test".into());
        p.options
            .insert(crate::options::READ_PREFERENCE.into(), "nearest".into());
        let o = MongoOptions::from_params(&p).unwrap();
        let dialer = Dialer::start(&p, Duration::from_secs(3)).await.unwrap();
        let redactor = Redactor::new(p.password.as_ref());
        let co = client_options(&p, &o, &dialer, Duration::from_secs(3), &redactor)
            .await
            .unwrap();
        assert!(co.tls.is_none(), "the dialer does TLS");
        assert_eq!(co.direct_connection, Some(true));
        assert_eq!(co.max_pool_size, Some(MAX_POOL_SIZE));
        assert_eq!(co.app_name.as_deref(), Some("oxbase-test"));
        assert_eq!(
            co.hosts,
            [ServerAddress::Tcp {
                host: "127.0.0.1".into(),
                port: Some(1)
            }]
        );
        let proxy = co.socks5_proxy.clone().unwrap();
        assert_eq!(proxy.host, "127.0.0.1");
        assert_eq!(proxy.port, Some(dialer.port()));
        assert!(matches!(
            co.selection_criteria,
            Some(SelectionCriteria::ReadPreference(MongoRead::Nearest { .. }))
        ));
    }

    #[tokio::test]
    async fn an_unreachable_server_fails_fast_with_a_connection_error() {
        // Nothing listens on port 1.
        let mut p = params();
        p.connect_timeout = Some(Duration::from_secs(5));
        let started = std::time::Instant::now();
        let err = MongoConnection::connect(&p).await.err().expect("refused");
        assert!(matches!(err, Error::Connection { .. }), "{err}");
        assert!(err.to_string().contains("127.0.0.1:1"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the dialer's failure ends the wait: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn unknown_options_are_refused_before_connecting() {
        let mut p = params();
        p.options.insert("bogus".into(), "1".into());
        let err = MongoConnection::connect(&p).await.err().expect("refused");
        assert!(matches!(err, Error::InvalidInput { .. }), "{err}");
    }
}
