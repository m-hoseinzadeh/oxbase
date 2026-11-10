//! The Redis driver (redis-driver stream): [`RedisDriver`] and [`RedisConnection`] on the
//! `redis` crate (1.8, tokio, multiplexed async connections). Contract:
//! `docs/m7-contracts.md` §2.2, §4.3. This file's public items are the Design contract; the
//! stream fills the bodies and adds private modules under `src/driver/`.
//!
//! # Addressing, TLS, SSH (M1 §2-3)
//!
//! - **Dial vs server name.** As built, standalone and sentinel sessions open their own
//!   sockets (`net`): TCP to [`ConnectParams::dial_endpoint`] (the SSH tunnel), then the TLS
//!   handshake with `oxbase_net::tls::client_config` verifying
//!   [`ConnectParams::tls_server_name`] (`tls.server_name`, else the server host), and hand the
//!   stream to `MultiplexedConnection::new_with_config`. That is the same split the design's
//!   `AsyncDNSResolver` mapping gives, with exactly the M1 semantics of every mode (chain-only
//!   `VerifyCa`, platform roots, inline / keychain client keys that never become plain byte
//!   buffers). Every connection of the session (the multiplexed one, the cancel side
//!   connection, dedicated pub/sub connections, sentinel queries) goes through it. Cluster and
//!   sentinel profiles have no SSH ([`crate::options::RedisOptions::from_params`] refuses it);
//!   their nodes are dialled as the servers announce them.
//! - **TLS**:
//!
//!   | `TlsMode` | Standalone / sentinel | Cluster (the crate's TLS) |
//!   |---|---|---|
//!   | `Disable` | TCP | `Tcp` |
//!   | `Prefer` | an inline `PING` on a fresh socket: a RESP line (`+PONG`, `-NOAUTH ...`) means a plaintext port (kept, `tls_in_use() = false`); anything else: TLS without verification | the same probe on the first seed, then `Insecure` or `Tcp` |
//!   | `Require` | TLS, any certificate | `Insecure` |
//!   | `VerifyCa` + CA file | TLS, chain checked, any name | `Secure` + `danger_accept_invalid_hostnames` |
//!   | `VerifyCa`, no CA file | as `VerifyFull` (platform roots: the name is checked too) | `Secure` |
//!   | `VerifyFull` | TLS, chain and name checked | `Secure` |
//!
//!   Roots: `ca_cert` when set (only those CAs), else the platform store. Client certificate
//!   and key: `client_cert` / `client_key`. A cluster verifies each node against the name it
//!   announces (`tls.server_name` is not used there).
//! - **Auth.** `ConnectParams::user` (ACL user; empty = `default`) and `password`, sent with
//!   `HELLO 3 AUTH` (RESP3) or `AUTH` (RESP2). `WRONGPASS` / `NOAUTH` are `Error::Auth`. The
//!   password never appears in errors, logs or `Debug`.
//! - **Protocol.** `auto`: RESP3 (`HELLO 3`); a server without `HELLO` (before 6.0) gets RESP2
//!   on a new connection. [`crate::options::Protocol`] forces one.
//! - **Database.** `SELECT n` in the handshake when n > 0
//!   ([`RedisConnection::current_database`] reports it); a console `SELECT` changes it and the
//!   connection tracks the new index (a reconnect selects it again).
//! - **Modes.** Standalone: one multiplexed connection. Cluster: `ClusterClient` (cluster-async)
//!   over the seeds; keys are routed by slot (read-only commands to a random replica of the
//!   slot when `read_from_replicas = true`), and [`RedisConnection::masters`] /
//!   [`RedisConnection::command_on`] let the dialect cells run `SCAN`-like commands per
//!   master. Sentinel: the sentinels are asked in order (`SENTINEL GET-MASTER-ADDR-BY-NAME`,
//!   with the profile's credentials only for `sentinel_auth = same`), then the master is
//!   connected to like standalone (a reconnect asks again, so a failover is followed).
//! - **Version.** `HELLO` without arguments (allowed for every ACL user) → `version` and the
//!   client `id`; when the server is not Redis (Valkey) or has no `HELLO`, `INFO server` →
//!   `redis_version`, and `CLIENT ID`. Unknown facts stay unknown (version 0.0.0).
//!
//! # Statements (`Connection::execute`)
//!
//! - `params` empty: `sql` is one console line, read with
//!   [`crate::command::parse_command_line`]; an empty line is `InvalidInput`.
//! - `params` not empty: `sql` holds the command words (`"HSET"`, `"CONFIG GET"`), `params`
//!   the remaining arguments in order (`Value::Bytes` as is, `Value::Text` as UTF-8, numbers in
//!   their decimal text, booleans `1` / `0`, JSON as text; `NULL` is refused); binary-safe.
//! - One command per call. The reply becomes one result set (rendering below), or
//!   `RowsAffected` for the integer reply of a write command (`DEL`, `UNLINK`, `HSET`, `HDEL`,
//!   `LPUSH`, `RPUSH`, `LREM`, `SADD`, `SREM`, `ZADD`, `ZREM`, `XDEL`, `EXPIRE`, `PERSIST`,
//!   the `*STORE` commands, ...) so the editor reports "n rows affected". An error reply is
//!   `Error::Sql { code: <first word, e.g. WRONGTYPE>, message }`.
//! - Refused with `InvalidInput` (they would take over or end the shared connection):
//!   `SUBSCRIBE` and the other pub/sub mode commands (use the pub/sub panel), `MONITOR`,
//!   `SYNC`, `PSYNC`, `QUIT`, `RESET`, `HELLO` with arguments, `CLIENT REPLY`.
//! - `MULTI` ... `EXEC` typed line by line work on the editor's own session (`EXEC` returns
//!   the replies as an array) for standalone and sentinel profiles. A cluster session refuses
//!   `MULTI`, `EXEC`, `DISCARD`, `WATCH` and `UNWATCH` with `InvalidInput`: the cluster routes
//!   each command to the node of its key, so the transaction would not be atomic (use a
//!   standalone profile on the node that holds the keys). `begin()` is `Unsupported`: grid-style transactions are not
//!   offered; key edits use `WATCH` / `MULTI` / `EXEC` inside the key-value cell (on a
//!   [`RedisConnection::dedicated`] connection when the session is shared).
//!
//! # Reply rendering (Arrow)
//!
//! Generic shapes by RESP type, then command-aware shapes for common read commands (the typed
//! value reads of `oxbase_dialect::keyvalue` use the same column names):
//!
//! | Reply | Columns |
//! |---|---|
//! | simple string, bulk string, verbatim string, integer, double, boolean, big number, null | one row: `type` (`Utf8`: `string` (bulk), `status` (simple), `verbatim`, `integer`, `double`, `boolean`, `bignumber`, `null`), `value` (`Utf8`: UTF-8 text as is, other bytes escaped like `redis-cli` (`\xff`); NULL for null), `raw` (`Binary`, the bytes of a bulk string, else NULL) |
//! | array / set / push | a row per element: `#` (`Int64`, 1-based), `type`, `value`, `raw` |
//! | map (RESP3), or the flat pairs of `CONFIG GET` / `HELLO` in RESP2 | a row per entry: `key` (`Utf8`), `key_raw` (`Binary`), `type`, `value`, `raw` |
//! | nested aggregates (`SCAN`'s `[cursor, keys]`, `CLUSTER SLOTS`, `EXEC`, ...) | as above, with nested elements in `value` as JSON text (byte strings as JSON strings when UTF-8, else `{"hex": "..."}`; maps with text keys as objects, else `[key, value]` pairs; NaN / infinities and big numbers as strings; errors as `{"error": "CODE message"}`); `value` carries `arrow.json` when every row is such an aggregate; an error element is a row of type `error` |
//! | `ZRANGE` / `ZREVRANGE` / `ZRANGEBYSCORE` / `ZREVRANGEBYSCORE` / `ZUNION` / `ZINTER` / `ZDIFF` / `ZRANDMEMBER` `... WITHSCORES`, `ZSCAN` | `member` (`Binary`), `score` (`Float64`) |
//! | `HGETALL`, `HSCAN` | `field` (`Binary`), `value` (`Binary`) |
//! | `LRANGE` | `index` (`Int64`, from the start argument; negative positions from the tail when the stop is negative), `value` (`Binary`) |
//! | `SMEMBERS`, `SINTER`, `SUNION`, `SDIFF`, `SSCAN` | `member` (`Binary`) |
//! | `XRANGE`, `XREVRANGE` | `id` (`Utf8`), `fields` (`Utf8`, `arrow.json` object; non-UTF-8 values as `{"hex": "..."}`) |
//! | `INFO` | `section`, `key`, `value` (`Utf8`) |
//!
//! The cursor commands (`HSCAN`, `SSCAN`, `ZSCAN`) put the next cursor in an info notice. A
//! reply that does not have the shape its command promises (an unexpected form) falls back to
//! the generic shape. `Binary` columns carry `oxbase.encoding = binary`; every column carries
//! `oxbase.native_type` (the RESP type of its values, `mixed` when they differ).
//!
//! # Cancel (M1 §7)
//!
//! `cancel_running` / the token: when the running command is blocking (`BLPOP`, `BRPOP`,
//! `BLMOVE`, `BRPOPLPUSH`, `BLMPOP`, `BZPOPMIN` / `MAX`, `BZMPOP`, `XREAD` / `XREADGROUP ...
//! BLOCK`, `WAIT`, `WAITAOF`) a side connection (same dial address, TLS and credentials) sends
//! `CLIENT UNBLOCK <id> ERROR`; a busy script gets `SCRIPT KILL` (`EVAL*`) or `FUNCTION KILL`
//! (`FCALL*`), which only end read-only scripts. When the server ends the command within 1.5 s
//! the session keeps its connection. Otherwise (any other command, an ACL user without
//! `CLIENT UNBLOCK`, a cluster) the connection is closed at once (a cluster: every node socket
//! of the session), so the server ends the command and it cannot pop or move data whose reply
//! nobody reads; the next command reconnects (and re-`SELECT`s), with a warning notice that
//! `MULTI` state and connection settings are gone. Commands running concurrently on the same
//! session fail with `Error::Connection`. The stream ends with `Error::Cancelled` within 2 s
//! in every case, also while the session is reconnecting (the reconnect is abandoned).
//! `CLIENT KILL` is never used (it ends the session for everyone on it). A run dropped before
//! its reply closes the connection the same way. `cancel_running` cancels every command
//! running on the session.
//! A broken connection (server restart, idle timeout) fails its command with
//! `Error::Connection` and is replaced before the next one.
//!
//! # Streaming
//!
//! A single command's reply arrives whole (RESP has no partial replies); it is turned into
//! Arrow batches of at most 4096 rows. Large values are read in chunks by the key-value cell
//! (cursor commands), never by one huge reply.
//!
//! # Native access for the dialect cells (same crate)
//!
//! Cells reach the session with `Connection::as_any` → [`RedisConnection`]:
//! [`RedisConnection::command`] / [`RedisConnection::pipeline`] (raw `redis::Value` replies,
//! error replies as `Error::Sql`, cancel as above), [`RedisConnection::masters`] and
//! [`RedisConnection::command_on`] (cluster fan-out), [`RedisConnection::handle`] (an owned
//! `'static` [`RedisHandle`] for result streams), [`RedisConnection::dedicated`] (a new
//! connection like the session's, for `WATCH` / `MULTI` / `EXEC` and RESP3 pub/sub with a push
//! sender) and [`RedisConnection::pubsub`] (a RESP2 `redis::aio::PubSub`).

mod args;
mod conn;
mod error;
mod net;
mod reply;

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream;
use oxbase_driver::{
    CancellationToken, ConnectParams, Connection, Driver, DriverRegistry, ResultItem, ResultStream,
    Tx,
};
use oxbase_types::{Endpoint, Engine, Error, Result, ServerVersion, Value};

use crate::options::{RedisMode, RedisOptions};
use args::{CancelKind, CommandInfo, command_args};
use conn::Session;

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
    session: Arc<Session>,
}

impl fmt::Debug for RedisConnection {
    /// The server, mode, protocol and database; never credentials.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedisConnection")
            .field("server", &self.params.server_endpoint())
            .field("dial", &self.params.dial_endpoint())
            .field("mode", &self.options.mode)
            .field("protocol", &self.session.protocol)
            .field("tls", &self.session.tls)
            .field("database", &self.session.database())
            .field("version", &self.version.raw())
            .finish_non_exhaustive()
    }
}

impl RedisConnection {
    /// Connects as described in the module docs: options parsed with
    /// [`RedisOptions::from_params`], TLS and SSH mapping, auth, protocol, `SELECT`, version.
    /// Bounded by `ConnectParams::connect_timeout` (default 15 s).
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let session = Session::connect(params).await?;
        Ok(Self {
            params: session.params.clone(),
            options: session.options.clone(),
            version: session.version.clone(),
            session: Arc::new(session),
        })
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

    /// The protocol the session speaks (`RESP3` on Redis 6+ unless the profile forces RESP2).
    pub fn protocol(&self) -> redis::ProtocolVersion {
        self.session.protocol
    }

    /// `true` for a cluster session (one keyspace, database 0; scans per master).
    pub fn is_cluster(&self) -> bool {
        self.options.mode == RedisMode::Cluster
    }

    /// An owned handle on this session, for result streams that outlive the borrow of the
    /// connection (`ResultSource::open` returns `'static` streams).
    pub fn handle(&self) -> RedisHandle {
        RedisHandle {
            session: self.session.clone(),
        }
    }

    /// Runs one command and returns its reply (see [`RedisHandle::command`]).
    pub async fn command(
        &self,
        cmd: &redis::Cmd,
        cancel: &CancellationToken,
    ) -> Result<redis::Value> {
        self.handle().command(cmd, cancel).await
    }

    /// Runs a pipeline (see [`RedisHandle::pipeline`]).
    pub async fn pipeline(
        &self,
        pipe: &redis::Pipeline,
        cancel: &CancellationToken,
    ) -> Result<Vec<redis::Value>> {
        self.handle().pipeline(pipe, cancel).await
    }

    /// The masters of a cluster; the one server of other sessions (see [`RedisHandle::masters`]).
    pub async fn masters(&self, cancel: &CancellationToken) -> Result<Vec<Endpoint>> {
        self.handle().masters(cancel).await
    }

    /// Runs one command on `node` of a cluster (see [`RedisHandle::command_on`]).
    pub async fn command_on(
        &self,
        node: &Endpoint,
        cmd: &redis::Cmd,
        cancel: &CancellationToken,
    ) -> Result<redis::Value> {
        self.handle().command_on(node, cmd, cancel).await
    }

    /// A new connection like the session's (see [`RedisHandle::dedicated`]).
    pub async fn dedicated(
        &self,
        config: redis::AsyncConnectionConfig,
        cancel: &CancellationToken,
    ) -> Result<DedicatedConnection> {
        self.handle().dedicated(config, cancel).await
    }

    /// A RESP2 pub/sub connection like the session's (see [`RedisHandle::pubsub`]).
    pub async fn pubsub(&self, cancel: &CancellationToken) -> Result<redis::aio::PubSub> {
        self.handle().pubsub(cancel).await
    }
}

/// An owned, cloneable handle on a [`RedisConnection`]'s session (`'static`, `Send`, `Sync`).
/// It keeps the session alive but does not keep it open: after `close()` every call fails with
/// `Error::Connection`.
#[derive(Clone)]
pub struct RedisHandle {
    session: Arc<Session>,
}

impl fmt::Debug for RedisHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedisHandle")
            .field("server", &self.session.params.server_endpoint())
            .field("database", &self.session.database())
            .finish_non_exhaustive()
    }
}

impl RedisHandle {
    /// Runs one command on the session's connection (a cluster routes it by its key). An error
    /// reply is `Error::Sql { code }`; other replies come back as they are (errors inside an
    /// `EXEC` reply stay `redis::Value::ServerError` elements). `cancel` interrupts a blocking
    /// command or a script, else replaces the connection (module docs, "Cancel").
    pub async fn command(
        &self,
        cmd: &redis::Cmd,
        cancel: &CancellationToken,
    ) -> Result<redis::Value> {
        let kind = cancel_kind(cmd);
        self.session.run(cmd.clone(), None, kind, cancel).await.0
    }

    /// Runs `pipe` (atomic or not) on the session's connection: one reply per command (for an
    /// atomic pipeline: the `EXEC` replies). Without `ignore_errors` the first error reply
    /// fails the call. An atomic pipeline whose `EXEC` returns nil (a key `WATCH`ed on this
    /// connection changed, nothing was written) fails with
    /// `Error::Sql { code: Some("EXECABORT") }`, never with an empty reply list. Cancel closes
    /// the connection (the next command reconnects).
    pub async fn pipeline(
        &self,
        pipe: &redis::Pipeline,
        cancel: &CancellationToken,
    ) -> Result<Vec<redis::Value>> {
        self.session.pipeline(pipe, cancel).await
    }

    /// The master nodes of a cluster (`CLUSTER SLOTS`, in slot order, read again on every
    /// call so a resharding is followed); for standalone and sentinel sessions the one server.
    pub async fn masters(&self, cancel: &CancellationToken) -> Result<Vec<Endpoint>> {
        self.session.masters(cancel).await
    }

    /// Runs one command on `node` (one of [`RedisHandle::masters`]) of a cluster, whatever its
    /// keys; on other sessions on the one server (`node` is not checked).
    pub async fn command_on(
        &self,
        node: &Endpoint,
        cmd: &redis::Cmd,
        cancel: &CancellationToken,
    ) -> Result<redis::Value> {
        let node = self.session.is_cluster().then(|| node.clone());
        let kind = cancel_kind(cmd);
        self.session.run(cmd.clone(), node, kind, cancel).await.0
    }

    /// Opens a new connection like the session's: same dial address (through SSH), TLS,
    /// credentials, protocol and database; a cluster: to its first master, database 0.
    /// `config` may carry a push sender (RESP3 pub/sub); response and connection timeouts are
    /// the caller's (`redis::AsyncConnectionConfig::new()` has 500 ms / 1 s defaults: set
    /// `None` for blocking use). The connection closes when the returned value is dropped.
    pub async fn dedicated(
        &self,
        config: redis::AsyncConnectionConfig,
        cancel: &CancellationToken,
    ) -> Result<DedicatedConnection> {
        let (conn, task) = self.session.dedicated(config, cancel).await?;
        Ok(DedicatedConnection { conn, _task: task })
    }

    /// Opens a RESP2 `redis::aio::PubSub` connection like the session's (database 0; channels
    /// are not per database).
    pub async fn pubsub(&self, cancel: &CancellationToken) -> Result<redis::aio::PubSub> {
        self.session.pubsub(cancel).await
    }
}

/// A connection opened by [`RedisHandle::dedicated`]. Dropping it closes the socket, even if
/// clones of its [`MultiplexedConnection`](redis::aio::MultiplexedConnection) are still alive.
pub struct DedicatedConnection {
    conn: redis::aio::MultiplexedConnection,
    _task: conn::Task,
}

impl DedicatedConnection {
    /// The connection (clone it to share; clones stop working once this value is dropped).
    pub fn connection(&self) -> &redis::aio::MultiplexedConnection {
        &self.conn
    }

    /// The connection, mutably (for `redis::aio::ConnectionLike` calls).
    pub fn connection_mut(&mut self) -> &mut redis::aio::MultiplexedConnection {
        &mut self.conn
    }
}

impl fmt::Debug for DedicatedConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DedicatedConnection")
            .finish_non_exhaustive()
    }
}

fn cancel_kind(cmd: &redis::Cmd) -> CancelKind {
    let args: Vec<Vec<u8>> = cmd
        .args_iter()
        .map(|a| match a {
            redis::Arg::Simple(b) => b.to_vec(),
            _ => b"0".to_vec(),
        })
        .collect();
    if args.is_empty() {
        CancelKind::Replace
    } else {
        CommandInfo::of(&args).cancel
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
        self.session.tls
    }

    /// The logical database index the session is on (`"0"`, ...), tracking console `SELECT`s.
    fn current_database(&self) -> Option<String> {
        Some(self.session.database().to_string())
    }

    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        if self.session.is_closed() {
            return Err(conn::closed());
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let args = command_args(sql, params)?;
        let info = CommandInfo::of(&args);
        info.check_allowed(&args)?;
        if self.session.is_cluster() {
            info.check_cluster()?;
        }
        let session = self.session.clone();
        let run = async move {
            let mut cmd = redis::Cmd::new();
            for arg in &args {
                cmd.arg(arg.as_slice());
            }
            let (reply, notices) = session.run(cmd, None, info.cancel, &cancel).await;
            let mut items: Vec<Result<ResultItem>> = notices
                .into_iter()
                .map(|n| Ok(ResultItem::Notice(n)))
                .collect();
            match reply.and_then(|value| {
                if info.is_select()
                    && matches!(value, redis::Value::Okay)
                    && let Some(db) = args
                        .get(1)
                        .and_then(|a| std::str::from_utf8(a).ok())
                        .and_then(|a| a.trim().parse::<i64>().ok())
                {
                    session.set_database(db);
                }
                reply::render(&info, &args, value)
            }) {
                Ok(rendered) => items.extend(rendered.into_iter().map(Ok)),
                Err(e) => items.push(Err(e)),
            }
            stream::iter(items)
        };
        Ok(stream::once(run).flatten().boxed())
    }

    async fn cancel_running(&self) -> Result<()> {
        self.session.cancel_running();
        Ok(())
    }

    /// Always `Unsupported` (see the module docs: `MULTI` / `EXEC` in the console, `WATCH` +
    /// `MULTI` / `EXEC` in the key-value cell).
    async fn begin(&self) -> Result<Box<dyn Tx>> {
        Err(Error::unsupported(
            "Redis has no transactions of this kind: use MULTI / EXEC in the console",
        ))
    }

    async fn close(&self) -> Result<()> {
        self.session.close().await;
        Ok(())
    }

    fn as_any(&self) -> Option<&(dyn std::any::Any + Send + Sync)> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_for_redis() {
        let mut drivers = DriverRegistry::new();
        register(&mut drivers);
        assert_eq!(drivers.engines(), [Engine::Redis]);
        assert_eq!(RedisDriver::new().engine(), Engine::Redis);
    }

    #[test]
    fn raw_commands_are_classified_for_cancel() {
        let mut blpop = redis::cmd("BLPOP");
        blpop.arg("q").arg(0);
        assert_eq!(cancel_kind(&blpop), CancelKind::Blocking);
        let mut eval = redis::cmd("eval");
        eval.arg("return 1").arg(0);
        assert_eq!(cancel_kind(&eval), CancelKind::Script);
        assert_eq!(cancel_kind(&redis::cmd("GET")), CancelKind::Replace);
        assert_eq!(cancel_kind(&redis::Cmd::new()), CancelKind::Replace);
    }

    #[tokio::test]
    async fn unreachable_servers_and_bad_options_fail_cleanly() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut params = ConnectParams::network(Engine::Redis, "127.0.0.1")
            .with_port(port)
            .with_password("Never_Shown_Pw_1")
            .with_tls(oxbase_types::TlsOptions::new(
                oxbase_types::TlsMode::Disable,
            ));
        let err = RedisConnection::connect(&params).await.unwrap_err();
        assert!(matches!(err, Error::Connection { .. }), "{err}");
        assert!(!err.to_string().contains("Never_Shown_Pw_1"), "{err}");

        params.options.insert("mode".into(), "ring".into());
        let err = RedisConnection::connect(&params).await.unwrap_err();
        assert!(matches!(err, Error::InvalidInput { .. }), "{err}");
    }
}
