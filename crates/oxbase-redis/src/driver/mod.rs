//! The Redis driver (redis-driver stream): [`RedisDriver`] and [`RedisConnection`] on the
//! `redis` crate (1.8, tokio, multiplexed async connections). Contract:
//! `docs/m7-contracts.md` §2.2, §4.3. This file's public items are the Design contract; the
//! stream fills the bodies and adds private modules under `src/driver/`.
//!
//! # Addressing, TLS, SSH (M1 §2-3)
//!
//! - **Dial vs server name.** The client is built for the *server* host
//!   ([`ConnectParams::tls_server_name`], else the host) and port, so TLS verifies and sends SNI
//!   for that name. Sockets are mapped onto [`ConnectParams::dial_endpoint`] (the SSH tunnel)
//!   by an `AsyncDNSResolver` the connection config carries
//!   (`AsyncConnectionConfig::set_dns_resolver`): it answers the server host and port with the
//!   dial address and resolves every other name normally. Every connection of the session
//!   (the multiplexed one, the cancel side connection, pub/sub connections) goes through it.
//!   Cluster and sentinel profiles have no SSH ([`crate::options::RedisOptions::from_params`]
//!   refuses it); their nodes are dialled as the server announces them.
//! - **TLS** (`ConnectionAddr::TcpTls` + `TlsCertificates`, crypto provider = ring, installed
//!   with `oxbase_net::tls::install_default_crypto_provider` before the first connect):
//!
//!   | `TlsMode` | Redis connection |
//!   |---|---|
//!   | `Disable` | `Tcp` |
//!   | `Prefer` | `TcpTls { insecure: true }`; when the TLS handshake fails, `Tcp` (`tls_in_use() = false`) |
//!   | `Require` | `TcpTls { insecure: true }` |
//!   | `VerifyCa` | `TcpTls { insecure: false }` + `danger_accept_invalid_hostnames` (chain checked, any name) |
//!   | `VerifyFull` | `TcpTls { insecure: false }` |
//!
//!   Roots: `TlsCertificates::root_cert` = the CA PEM bytes when `ca_cert` is set (only those
//!   CAs), else the platform store (`rustls-native-certs`, the crate's default). Client
//!   certificate and key: `TlsCertificates::client_tls` from `client_cert` / `client_key`
//!   (loaded with `oxbase_net::tls::load_pem` / `load_key_pem`; the key bytes live only for the
//!   call). `tls.server_name` overrides the name (it becomes the client's host).
//! - **Auth.** `ConnectParams::user` (ACL user; empty = `default`) and `password`, sent with
//!   `HELLO 3 AUTH` (RESP3) or `AUTH` (RESP2). `WRONGPASS` / `NOAUTH` are `Error::Auth`. The
//!   password never appears in errors, logs or `Debug`.
//! - **Protocol.** `auto`: RESP3 when the server's version is 6.0 or newer (`HELLO 3`),
//!   RESP2 otherwise; [`crate::options::Protocol`] forces one.
//! - **Database.** `SELECT n` after connect when n > 0 ([`RedisConnection::current_database`]
//!   reports it); a console `SELECT` changes it and the connection tracks the new index.
//! - **Modes.** Standalone: one multiplexed connection. Cluster: `ClusterClient` (cluster-async)
//!   over the seeds; keys are routed by slot, `SCAN`-like commands are run per master by the
//!   dialect cells (`RedisConnection` exposes the masters). Sentinel: `SentinelClient` asks the
//!   sentinels for `master_name`'s address, then connects to it like standalone.
//! - **Version.** `INFO server` → `redis_version` (Valkey reports `valkey_version` too; keep
//!   `redis_version`), parsed into `ServerVersion`.
//!
//! # Statements (`Connection::execute`)
//!
//! - `params` empty: `sql` is one console line, read with
//!   [`crate::command::parse_command_line`]; an empty line is `InvalidInput`.
//! - `params` not empty: `sql` holds the command words (`"HSET"`, `"CONFIG GET"`), `params`
//!   the remaining arguments in order (`Value::Bytes` as is, `Value::Text` as UTF-8, numbers in
//!   their decimal text); binary-safe.
//! - One command per call. The reply becomes one result set (rendering below), or
//!   `RowsAffected` for the integer reply of a write command (`DEL`, `HSET`, ... the docs'
//!   write-command list) so the editor reports "n rows affected". An error reply is
//!   `Error::Sql { code: <first word, e.g. WRONGTYPE>, message }`.
//! - `MULTI` ... `EXEC` typed line by line work on the editor's own session (`EXEC` returns
//!   the replies as an array). `begin()` is `Unsupported`: grid-style transactions are not
//!   offered; key edits use `WATCH` / `MULTI` / `EXEC` inside the key-value cell.
//!
//! # Reply rendering (Arrow)
//!
//! Generic shapes by RESP type, then command-aware shapes for common read commands (the typed
//! value reads of `oxbase_dialect::keyvalue` use the same column names):
//!
//! | Reply | Columns |
//! |---|---|
//! | simple string, bulk string, verbatim string, integer, double, boolean, big number, null | one row: `type` (`Utf8`: `string`, `integer`, `double`, `boolean`, `bignumber`, `null`, ...), `value` (`Utf8`, the text redis-cli prints, bytes escaped), `raw` (`Binary`, the bytes of a bulk string, else NULL) |
//! | array / set / push of scalars | a row per element: `#` (`Int64`, 1-based), `type`, `value`, `raw` |
//! | map (RESP3), or the flat pairs of `HGETALL` / `CONFIG GET` in RESP2 | a row per entry: `key` (`Utf8`), `key_raw` (`Binary`), `type`, `value`, `raw` |
//! | nested aggregates (`XRANGE`, `SCAN`'s `[cursor, keys]`, `CLUSTER SLOTS`, ...) | as above, with nested elements rendered in `value` as JSON text (`arrow.json`; byte strings as JSON strings when UTF-8, else `{"hex": "..."}`) |
//! | `ZRANGE ... WITHSCORES`, `ZSCAN` | `member` (`Binary`), `score` (`Float64`) |
//! | `HGETALL`, `HSCAN` | `field` (`Binary`), `value` (`Binary`) |
//! | `LRANGE` | `index` (`Int64`, from the start argument), `value` (`Binary`) |
//! | `SMEMBERS`, `SSCAN` | `member` (`Binary`) |
//! | `XRANGE`, `XREVRANGE` | `id` (`Utf8`), `fields` (`Utf8`, `arrow.json`) |
//! | `INFO` | `section`, `key`, `value` (`Utf8`) |
//!
//! `Binary` columns carry `oxbase.encoding = binary`; every column carries
//! `oxbase.native_type` = the RESP type name.
//!
//! # Cancel (M1 §7)
//!
//! `cancel_running` / the token: when the running command is blocking (`BLPOP`, `BLMOVE`,
//! `XREAD ... BLOCK`, `WAIT`, ...) a side connection (same dial address, TLS and credentials)
//! sends `CLIENT UNBLOCK <id> ERROR`; a busy script gets `SCRIPT KILL` / `FUNCTION KILL`
//! (only kills read-only scripts; others keep running and the stream still ends); any other
//! command's reply is dropped and the multiplexed connection is replaced (reconnected and
//! re-`SELECT`ed) before the next command, with a warning notice that `MULTI` state and
//! session settings are gone. The stream ends with `Error::Cancelled` within 2 s in every case.
//! `CLIENT KILL` is never used (it ends the session for everyone on it).
//!
//! # Streaming
//!
//! A single command's reply arrives whole (RESP has no partial replies); it is turned into
//! Arrow batches of at most 4096 rows. Large values are read in chunks by the key-value cell
//! (cursor commands), never by one huge reply.

use std::sync::Arc;

use async_trait::async_trait;
use oxbase_driver::{
    CancellationToken, ConnectParams, Connection, Driver, DriverRegistry, ResultStream, Tx,
};
use oxbase_types::{Engine, Error, Result, ServerVersion, Value};

use crate::options::RedisOptions;

/// Opens [`RedisConnection`]s.
#[derive(Debug, Clone, Copy, Default)]
pub struct RedisDriver;

impl RedisDriver {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for RedisDriver {
    fn engine(&self) -> Engine {
        Engine::Redis
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        let connection = RedisConnection::connect(params).await?;
        Ok(Box::new(connection))
    }
}

/// Adds [`RedisDriver`] for [`Engine::Redis`] to `drivers` (called by
/// `oxbase_services::default_drivers` once the redis-driver stream uncomments its line).
pub fn register(drivers: &mut DriverRegistry) {
    drivers.register(Arc::new(RedisDriver::new()));
}

/// One Redis session (standalone, cluster or sentinel). See the module docs.
pub struct RedisConnection {
    params: ConnectParams,
    options: RedisOptions,
    version: ServerVersion,
}

impl RedisConnection {
    /// Connects as described in the module docs: options parsed with
    /// [`RedisOptions::from_params`], TLS and SSH mapping, auth, protocol, `SELECT`, version.
    /// Bounded by `ConnectParams::connect_timeout` (default 15 s).
    #[allow(
        clippy::unused_async,
        reason = "m7 skeleton: the redis-driver stream fills the body"
    )]
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let _ = params;
        unimplemented!("m7: redis-driver")
    }

    /// The parameters this session was opened with (dialect cells open dedicated pub/sub
    /// connections and cancel side connections from them).
    pub fn params(&self) -> &ConnectParams {
        &self.params
    }

    /// The parsed profile options (mode, namespace separator, tree scan limit, ...).
    pub fn options(&self) -> &RedisOptions {
        &self.options
    }
}

#[async_trait]
impl Connection for RedisConnection {
    fn engine(&self) -> Engine {
        Engine::Redis
    }

    fn server_version(&self) -> &ServerVersion {
        &self.version
    }

    fn tls_in_use(&self) -> bool {
        unimplemented!("m7: redis-driver")
    }

    /// The logical database index the session is on (`"0"`, ...), tracking console `SELECT`s.
    fn current_database(&self) -> Option<String> {
        unimplemented!("m7: redis-driver")
    }

    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        let _ = (sql, params, cancel);
        unimplemented!("m7: redis-driver")
    }

    async fn cancel_running(&self) -> Result<()> {
        unimplemented!("m7: redis-driver")
    }

    /// Always `Unsupported` (see the module docs: `MULTI` / `EXEC` in the console, `WATCH` +
    /// `MULTI` / `EXEC` in the key-value cell).
    async fn begin(&self) -> Result<Box<dyn Tx>> {
        Err(Error::unsupported(
            "Redis has no transactions of this kind: use MULTI / EXEC in the console",
        ))
    }

    async fn close(&self) -> Result<()> {
        unimplemented!("m7: redis-driver")
    }

    fn as_any(&self) -> Option<&(dyn std::any::Any + Send + Sync)> {
        Some(self)
    }
}
