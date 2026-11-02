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
//!   `document_aggregate` take it), executed by [`crate::request::RequestSource`];
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

use std::sync::Arc;

use async_trait::async_trait;
use oxbase_driver::{
    CancellationToken, ConnectParams, Connection, Driver, DriverRegistry, ResultStream, Tx,
};
use oxbase_types::{Engine, Error, Result, ServerVersion, Value};

use crate::options::MongoOptions;
use crate::shell::ShellSession;

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
}

impl MongoConnection {
    /// Connects as described in the module docs. Bounded by `ConnectParams::connect_timeout`
    /// (default 15 s).
    #[allow(
        clippy::unused_async,
        reason = "m7 skeleton: the mongo-driver stream fills the body"
    )]
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let _ = params;
        unimplemented!("m7: mongo-driver")
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
        let _ = self;
        unimplemented!("m7: mongo-driver")
    }
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
        unimplemented!("m7: mongo-driver")
    }

    /// The shell's current database (`ConnectParams::database` at first, `test` without one;
    /// `use <db>` changes it).
    fn current_database(&self) -> Option<String> {
        unimplemented!("m7: mongo-driver")
    }

    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        let _ = (sql, params, cancel);
        unimplemented!("m7: mongo-driver")
    }

    async fn cancel_running(&self) -> Result<()> {
        unimplemented!("m7: mongo-driver")
    }

    /// Always `Unsupported` in M7 (see the module docs).
    async fn begin(&self) -> Result<Box<dyn Tx>> {
        Err(Error::unsupported(
            "MongoDB transactions are not offered: document edits apply one by one",
        ))
    }

    async fn close(&self) -> Result<()> {
        unimplemented!("m7: mongo-driver")
    }

    fn as_any(&self) -> Option<&(dyn std::any::Any + Send + Sync)> {
        Some(self)
    }
}
