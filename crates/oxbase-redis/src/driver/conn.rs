//! Session state shared by [`RedisConnection`](super::RedisConnection) and
//! [`RedisHandle`](super::RedisHandle): the live link (one multiplexed connection, or a cluster
//! connection), connecting in each mode, reconnecting, side connections and cancel.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oxbase_driver::{ConnectParams, Notice};
use oxbase_types::{Endpoint, Error, Result, ServerVersion, TlsMode};
use redis::aio::{ConnectionLike, MultiplexedConnection};
use redis::cluster::ClusterClientBuilder;
use redis::cluster_async::ClusterConnection;
use redis::cluster_routing::{RoutingInfo, SingleNodeRoutingInfo};
use redis::{
    AsyncConnectionConfig, Cmd, ConnectionAddr, ProtocolVersion, RedisConnectionInfo, RedisFuture,
    Value,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::args::CancelKind;
use super::error::{from_redis, from_server, is_link_broken, is_resp3_refused};
use super::net::{Address, dial};
use super::reply::info_field;
use crate::options::{Protocol, RedisMode, RedisOptions, SentinelAuth};

/// Default bound of a connect (`ConnectParams::connect_timeout` overrides it).
pub(crate) const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a cancel waits for `CLIENT UNBLOCK` / `SCRIPT KILL` to end the command before it
/// replaces the connection instead (the whole cancel stays under 2 s).
const INTERRUPT_WAIT: Duration = Duration::from_millis(1500);
/// Replies of a cluster connection never time out on their own (`BLPOP 0` may wait forever);
/// cancel ends them.
const CLUSTER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30 * 24 * 3600);
/// The library name the session reports with `CLIENT SETINFO` (ignored by older servers).
const LIB_NAME: &str = "oxbase";

/// Aborts a spawned connection driver when dropped (or on [`Task::abort`]).
pub(crate) struct Task(tokio::task::AbortHandle);

impl Task {
    fn spawn(driver: impl Future<Output = ()> + Send + 'static) -> Self {
        Self(tokio::spawn(driver).abort_handle())
    }

    fn abort(&self) {
        self.0.abort();
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The connection commands run on.
#[derive(Clone)]
pub(crate) enum Link {
    /// One server (standalone, or the master a sentinel named).
    Single {
        conn: MultiplexedConnection,
        /// `CLIENT ID` of the connection (cancel of blocking commands).
        client_id: Option<i64>,
        task: Arc<Task>,
    },
    /// A Redis Cluster: commands are routed by slot.
    Cluster(ClusterConnection),
}

impl Link {
    /// Ends the link's socket(s) now, even while clones are still alive.
    fn abort(&self) {
        if let Self::Single { task, .. } = self {
            task.abort();
        }
    }

    fn client_id(&self) -> Option<i64> {
        match self {
            Self::Single { client_id, .. } => *client_id,
            Self::Cluster(_) => None,
        }
    }

    /// Sends `cmd` to `node` on a cluster; to the one server otherwise.
    async fn send_to(&mut self, node: Option<&Endpoint>, cmd: &Cmd) -> redis::RedisResult<Value> {
        match (self, node) {
            (Self::Cluster(c), Some(node)) => {
                let routing = RoutingInfo::SingleNode(SingleNodeRoutingInfo::ByAddress {
                    host: node.host.clone(),
                    port: node.port,
                });
                c.route_command(cmd.clone(), routing).await
            }
            (link, _) => link.req_packed_command(cmd).await,
        }
    }
}

impl ConnectionLike for Link {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        match self {
            Self::Single { conn, .. } => conn.req_packed_command(cmd),
            Self::Cluster(c) => c.req_packed_command(cmd),
        }
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipeline: &'a redis::Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        match self {
            Self::Single { conn, .. } => conn.req_packed_commands(pipeline, offset, count),
            Self::Cluster(c) => c.req_packed_commands(pipeline, offset, count),
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            Self::Single { conn, .. } => conn.get_db(),
            Self::Cluster(c) => c.get_db(),
        }
    }
}

/// What the server told about itself at connect time.
#[derive(Debug, Clone)]
pub(crate) struct Facts {
    pub version: ServerVersion,
    pub client_id: Option<i64>,
}

struct State {
    link: Option<Link>,
    /// Bumped by every (re)connect; a link is stale when its generation is at or below
    /// [`Session::stale`].
    generation: u64,
}

/// One session (see the driver module docs).
pub(crate) struct Session {
    pub params: ConnectParams,
    pub options: RedisOptions,
    /// The protocol negotiated at connect time (reconnects keep it).
    pub protocol: ProtocolVersion,
    pub tls: bool,
    pub version: ServerVersion,
    /// The logical database (`SELECT`s of the console move it).
    db: AtomicI64,
    /// The server a standalone or sentinel session talks to (sentinel: the current master).
    address: Mutex<Option<Address>>,
    state: tokio::sync::Mutex<State>,
    stale: AtomicU64,
    closed: AtomicBool,
    /// The token of the command running now, for `cancel_running`.
    running: Mutex<Option<(u64, CancellationToken)>>,
    next_run: AtomicU64,
}

/// An opened link and what came with it.
struct Opened {
    link: Link,
    facts: Facts,
    tls: bool,
    protocol: ProtocolVersion,
}

impl Session {
    /// Connects as the driver module docs describe, bounded by the connect timeout.
    pub(crate) async fn connect(params: &ConnectParams) -> Result<Self> {
        oxbase_net::tls::install_default_crypto_provider();
        params.validate()?;
        let options = RedisOptions::from_params(params)?;
        let timeout = params.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT);
        let server = params
            .server_endpoint()
            .map_or_else(|| "the server".to_owned(), |e| format!("{e}"));
        let (opened, address) = tokio::time::timeout(timeout, open(params, &options))
            .await
            .map_err(|_| {
                Error::timeout(format!(
                    "connecting to Redis at {server} took longer than {} s",
                    timeout.as_secs()
                ))
            })??;
        Ok(Self {
            params: params.clone(),
            protocol: opened.protocol,
            tls: opened.tls,
            version: opened.facts.version,
            db: AtomicI64::new(i64::from(options.database)),
            options,
            address: Mutex::new(address),
            state: tokio::sync::Mutex::new(State {
                link: Some(opened.link),
                generation: 1,
            }),
            stale: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            running: Mutex::new(None),
            next_run: AtomicU64::new(1),
        })
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub(crate) fn database(&self) -> i64 {
        self.db.load(Ordering::SeqCst)
    }

    pub(crate) fn set_database(&self, db: i64) {
        self.db.store(db, Ordering::SeqCst);
    }

    pub(crate) fn is_cluster(&self) -> bool {
        self.options.mode == RedisMode::Cluster
    }

    fn current_address(&self) -> Option<Address> {
        self.address.lock().ok().and_then(|a| Option::clone(&a))
    }

    /// The live link, reconnecting first when it was replaced or broke.
    async fn link(&self, notices: &mut Vec<Notice>) -> Result<(Link, u64)> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(closed());
        }
        let mut state = self.state.lock().await;
        let stale = state.generation <= self.stale.load(Ordering::SeqCst);
        if state.link.is_none() || stale {
            if let Some(old) = state.link.take() {
                old.abort();
            }
            if self.closed.load(Ordering::SeqCst) {
                return Err(closed());
            }
            let timeout = self
                .params
                .connect_timeout
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
            let protocol = if self.protocol == ProtocolVersion::RESP3 {
                Protocol::Resp3
            } else {
                Protocol::Resp2
            };
            let mut options = self.options.clone();
            options.protocol = protocol;
            options.database = u16::try_from(self.database()).unwrap_or(options.database);
            let (opened, address) = tokio::time::timeout(timeout, open(&self.params, &options))
                .await
                .map_err(|_| Error::timeout("reconnecting to Redis took too long"))??;
            if let Ok(mut a) = self.address.lock() {
                *a = address;
            }
            state.generation += 1;
            state.link = Some(opened.link);
            notices.push(Notice::warning(
                "the session reconnected: MULTI / WATCH state, CLIENT settings and other \
                 connection state of the previous connection are gone (the database stays \
                 selected)",
            ));
        }
        let link = state.link.clone().ok_or_else(closed)?;
        Ok((link, state.generation))
    }

    fn mark_stale(&self, generation: u64) {
        self.stale.fetch_max(generation, Ordering::SeqCst);
    }

    /// Runs `cmd` (on `node` of a cluster when given) and returns its reply. An error reply is
    /// [`Error::Sql`]; cancel follows the driver module docs. Notices (a reconnect, a replaced
    /// connection) are returned next to the outcome.
    pub(crate) async fn run(
        self: &Arc<Self>,
        cmd: Cmd,
        node: Option<Endpoint>,
        kind: CancelKind,
        cancel: &CancellationToken,
    ) -> (Result<Value>, Vec<Notice>) {
        let mut notices = Vec::new();
        if cancel.is_cancelled() {
            return (Err(Error::Cancelled), notices);
        }
        let (link, generation) = match self.link(&mut notices).await {
            Ok(l) => l,
            Err(e) => return (Err(e), notices),
        };
        let token = cancel.child_token();
        let _running = Running::start(self, token.clone());
        let mut in_flight = InFlight {
            session: self,
            generation,
            armed: true,
        };
        let send = {
            let mut link = link.clone();
            async move { link.send_to(node.as_ref(), &cmd).await }
        };
        tokio::pin!(send);
        let reply = tokio::select! {
            biased;
            reply = &mut send => Some(reply),
            () = token.cancelled() => None,
        };
        if let Some(reply) = reply {
            in_flight.armed = false;
            return (self.finish(reply, generation), notices);
        }

        // Cancelled while the command runs.
        let deadline = Instant::now() + INTERRUPT_WAIT;
        let interrupted = match (kind, link.client_id()) {
            (CancelKind::Blocking, Some(id)) => {
                let mut c = redis::cmd("CLIENT");
                c.arg("UNBLOCK").arg(id).arg("ERROR");
                self.side(&c, deadline).await == Some(Value::Int(1))
            }
            (CancelKind::Script, Some(_)) => {
                let mut c = redis::cmd("SCRIPT");
                c.arg("KILL");
                matches!(self.side(&c, deadline).await, Some(Value::Okay))
            }
            (CancelKind::Function, Some(_)) => {
                let mut c = redis::cmd("FUNCTION");
                c.arg("KILL");
                matches!(self.side(&c, deadline).await, Some(Value::Okay))
            }
            _ => false,
        };
        if interrupted && tokio::time::timeout_at(deadline, &mut send).await.is_ok() {
            // The server ended the command; the connection is in step again.
            in_flight.armed = false;
            return (Err(Error::Cancelled), notices);
        }
        self.mark_stale(generation);
        link.abort();
        notices.push(Notice::warning(
            "the cancelled command could not be interrupted on the server: its reply is \
             dropped and the session reconnects before the next command",
        ));
        (Err(Error::Cancelled), notices)
    }

    fn finish(&self, reply: redis::RedisResult<Value>, generation: u64) -> Result<Value> {
        match reply {
            Ok(Value::ServerError(e)) => Err(from_server(&e)),
            Ok(value) => Ok(value),
            Err(e) => {
                if is_link_broken(&e) {
                    self.mark_stale(generation);
                }
                Err(from_redis(&e))
            }
        }
    }

    /// Runs `pipe` on the session's link (cancel replaces the connection).
    pub(crate) async fn pipeline(
        self: &Arc<Self>,
        pipe: &redis::Pipeline,
        cancel: &CancellationToken,
    ) -> Result<Vec<Value>> {
        let mut notices = Vec::new();
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let (link, generation) = self.link(&mut notices).await?;
        let token = cancel.child_token();
        let _running = Running::start(self, token.clone());
        let mut in_flight = InFlight {
            session: self,
            generation,
            armed: true,
        };
        let mut conn = link.clone();
        let reply = tokio::select! {
            biased;
            reply = pipe.query_async::<Vec<Value>>(&mut conn) => reply,
            () = token.cancelled() => {
                self.mark_stale(generation);
                link.abort();
                return Err(Error::Cancelled);
            }
        };
        in_flight.armed = false;
        match reply {
            Ok(values) => Ok(values),
            Err(e) => {
                if is_link_broken(&e) {
                    self.mark_stale(generation);
                }
                Err(from_redis(&e))
            }
        }
    }

    /// Cancels the command running now, if any.
    pub(crate) fn cancel_running(&self) {
        if let Ok(running) = self.running.lock()
            && let Some((_, token)) = running.as_ref()
        {
            token.cancel();
        }
    }

    pub(crate) async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.cancel_running();
        let mut state = self.state.lock().await;
        if let Some(link) = state.link.take() {
            link.abort();
        }
    }

    /// Runs one command on a fresh side connection to the session's server (same dial address,
    /// TLS and credentials) before `deadline`; `None` when it could not.
    async fn side(&self, cmd: &Cmd, deadline: Instant) -> Option<Value> {
        let address = self.current_address()?;
        let work = async {
            let config = connection_config();
            let (mut conn, task, _) = open_multiplexed(
                &self.params,
                &address,
                ProtocolVersion::RESP2,
                0,
                Credentials::Profile,
                config,
            )
            .await
            .ok()?;
            let reply = conn.req_packed_command(cmd).await.ok();
            drop(task);
            reply
        };
        tokio::time::timeout_at(deadline, work).await.ok().flatten()
    }

    /// The masters of a cluster (`CLUSTER SLOTS`); the one server otherwise.
    pub(crate) async fn masters(
        self: &Arc<Self>,
        cancel: &CancellationToken,
    ) -> Result<Vec<Endpoint>> {
        if !self.is_cluster() {
            let address = self
                .current_address()
                .ok_or_else(|| Error::internal("the session has no server address"))?;
            return Ok(vec![address.server]);
        }
        let mut slots = redis::cmd("CLUSTER");
        slots.arg("SLOTS");
        let (reply, _) = self.run(slots, None, CancelKind::Replace, cancel).await;
        let masters = cluster_masters(&reply?);
        if masters.is_empty() {
            return Err(Error::connection("CLUSTER SLOTS named no master"));
        }
        Ok(masters)
    }

    /// A new connection like the session's (a cluster: to its first master), for pub/sub and
    /// transactions that must not share the session's connection.
    pub(crate) async fn dedicated(
        self: &Arc<Self>,
        config: AsyncConnectionConfig,
        cancel: &CancellationToken,
    ) -> Result<(MultiplexedConnection, Task)> {
        let (address, db) = if self.is_cluster() {
            let first = self.masters(cancel).await?.remove(0);
            (Address::direct(first), 0)
        } else {
            (
                self.current_address()
                    .ok_or_else(|| Error::internal("the session has no server address"))?,
                self.database(),
            )
        };
        let open = open_multiplexed(
            &self.params,
            &address,
            self.protocol,
            db,
            Credentials::Profile,
            config,
        );
        let (conn, task, _) = tokio::select! {
            r = open => r?,
            () = cancel.cancelled() => return Err(Error::Cancelled),
        };
        Ok((conn, task))
    }

    /// A RESP2 pub/sub connection like the session's.
    pub(crate) async fn pubsub(
        self: &Arc<Self>,
        cancel: &CancellationToken,
    ) -> Result<redis::aio::PubSub> {
        let address = if self.is_cluster() {
            Address::direct(self.masters(cancel).await?.remove(0))
        } else {
            self.current_address()
                .ok_or_else(|| Error::internal("the session has no server address"))?
        };
        let open = async {
            let dialed = dial(&address, &self.params.tls).await?;
            let info = redis_info(
                &self.params,
                ProtocolVersion::RESP2,
                0,
                Credentials::Profile,
            );
            redis::aio::PubSub::new(&info, dialed.stream)
                .await
                .map_err(|e| from_redis(&e))
        };
        tokio::select! {
            r = open => r,
            () = cancel.cancelled() => Err(Error::Cancelled),
        }
    }
}

pub(crate) fn closed() -> Error {
    Error::connection("the Redis connection is closed")
}

/// Keeps the token of the running command for `cancel_running`.
struct Running<'a> {
    session: &'a Session,
    id: u64,
}

impl<'a> Running<'a> {
    fn start(session: &'a Session, token: CancellationToken) -> Self {
        let id = session.next_run.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut running) = session.running.lock() {
            *running = Some((id, token));
        }
        Self { session, id }
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        if let Ok(mut running) = self.session.running.lock()
            && running.as_ref().is_some_and(|(id, _)| *id == self.id)
        {
            *running = None;
        }
    }
}

/// While armed, dropping it (the caller dropped the run before the reply) marks the link stale:
/// the abandoned command may still hold the connection (a blocking command).
struct InFlight<'a> {
    session: &'a Session,
    generation: u64,
    armed: bool,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.session.mark_stale(self.generation);
        }
    }
}

/// Which credentials a connection sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Credentials {
    /// The profile's user and password.
    Profile,
    /// None (sentinels without `sentinel_auth = same`).
    None,
}

fn connection_config() -> AsyncConnectionConfig {
    AsyncConnectionConfig::new()
        .set_response_timeout(None)
        .set_connection_timeout(None)
}

fn redis_info(
    params: &ConnectParams,
    protocol: ProtocolVersion,
    db: i64,
    credentials: Credentials,
) -> RedisConnectionInfo {
    let mut info = RedisConnectionInfo::default()
        .set_protocol(protocol)
        .set_db(db)
        .set_lib_name(LIB_NAME, env!("CARGO_PKG_VERSION"));
    if credentials == Credentials::Profile {
        if let Some(user) = params
            .user
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        {
            info = info.set_username(user);
        }
        if let Some(password) = params.password.as_ref().filter(|p| !p.is_empty()) {
            info = info.set_password(password.expose());
        }
    }
    info
}

/// TCP (+ TLS) to `address`, then the `redis` handshake (`HELLO` / `AUTH`, `SELECT`), and the
/// connection's driver task.
async fn open_multiplexed(
    params: &ConnectParams,
    address: &Address,
    protocol: ProtocolVersion,
    db: i64,
    credentials: Credentials,
    config: AsyncConnectionConfig,
) -> Result<(MultiplexedConnection, Task, bool)> {
    let dialed = dial(address, &params.tls).await?;
    let info = redis_info(params, protocol, db, credentials);
    let (conn, driver) = MultiplexedConnection::new_with_config(&info, dialed.stream, config)
        .await
        .map_err(|e| handshake_error(&e, address))?;
    Ok((conn, Task::spawn(driver), dialed.tls))
}

fn handshake_error(e: &redis::RedisError, address: &Address) -> Error {
    match from_redis(e) {
        Error::Connection { message } => {
            Error::connection(format!("Redis at {}: {message}", address.server))
        }
        other => other,
    }
}

/// Opens one server with the protocol of `wanted` (auto: RESP3, else RESP2).
async fn open_single(
    params: &ConnectParams,
    address: &Address,
    wanted: Protocol,
    db: i64,
) -> Result<Opened> {
    let attempt = |protocol| {
        open_multiplexed(
            params,
            address,
            protocol,
            db,
            Credentials::Profile,
            connection_config(),
        )
    };
    let (conn, task, tls, protocol) = match wanted {
        Protocol::Resp2 => {
            let (c, t, tls) = attempt(ProtocolVersion::RESP2).await?;
            (c, t, tls, ProtocolVersion::RESP2)
        }
        Protocol::Resp3 => {
            let (c, t, tls) = attempt(ProtocolVersion::RESP3).await?;
            (c, t, tls, ProtocolVersion::RESP3)
        }
        Protocol::Auto => {
            let dialed = dial(address, &params.tls).await?;
            let info = redis_info(params, ProtocolVersion::RESP3, db, Credentials::Profile);
            match MultiplexedConnection::new_with_config(&info, dialed.stream, connection_config())
                .await
            {
                Ok((c, driver)) => (c, Task::spawn(driver), dialed.tls, ProtocolVersion::RESP3),
                Err(e) if is_resp3_refused(&e) => {
                    let (c, t, tls) = attempt(ProtocolVersion::RESP2).await?;
                    (c, t, tls, ProtocolVersion::RESP2)
                }
                Err(e) => return Err(handshake_error(&e, address)),
            }
        }
    };
    let mut link = Link::Single {
        conn,
        client_id: None,
        task: Arc::new(task),
    };
    let facts = read_facts(&mut link).await;
    if let Link::Single { client_id, .. } = &mut link {
        *client_id = facts.client_id;
    }
    Ok(Opened {
        link,
        facts,
        tls,
        protocol,
    })
}

/// Connects in the profile's mode. Returns the opened link and, for standalone and sentinel,
/// the server address side connections use.
async fn open(params: &ConnectParams, options: &RedisOptions) -> Result<(Opened, Option<Address>)> {
    let db = i64::from(options.database);
    match &options.mode {
        RedisMode::Standalone => {
            let (Some(dial_at), Some(server)) = (params.dial_endpoint(), params.server_endpoint())
            else {
                return Err(Error::invalid_input("the Redis profile has no host"));
            };
            let address = Address {
                dial: dial_at,
                server,
            };
            let opened = open_single(params, &address, options.protocol, db).await?;
            Ok((opened, Some(address)))
        }
        RedisMode::Sentinel { master_name, auth } => {
            let master = resolve_master(params, options, master_name, *auth).await?;
            let address = Address::direct(master);
            let opened = open_single(params, &address, options.protocol, db).await?;
            Ok((opened, Some(address)))
        }
        RedisMode::Cluster => Ok((open_cluster(params, options).await?, None)),
    }
}

/// Asks the sentinels, in order, for the address of `master_name`.
async fn resolve_master(
    params: &ConnectParams,
    options: &RedisOptions,
    master_name: &str,
    auth: SentinelAuth,
) -> Result<Endpoint> {
    let credentials = match auth {
        SentinelAuth::Same => Credentials::Profile,
        SentinelAuth::None => Credentials::None,
    };
    let mut problems = Vec::new();
    for sentinel in &options.seeds {
        let address = Address::direct(sentinel.clone());
        let ask = async {
            let (mut conn, task, _) = open_multiplexed(
                params,
                &address,
                ProtocolVersion::RESP2,
                0,
                credentials,
                connection_config(),
            )
            .await?;
            let mut cmd = redis::cmd("SENTINEL");
            cmd.arg("GET-MASTER-ADDR-BY-NAME").arg(master_name);
            let reply = conn
                .req_packed_command(&cmd)
                .await
                .map_err(|e| from_redis(&e));
            drop(task);
            match reply? {
                Value::ServerError(e) => Err(from_server(&e)),
                value => Ok(master_address(&value)),
            }
        };
        match ask.await {
            Ok(Some(master)) => return Ok(master),
            Ok(None) => problems.push(format!("{sentinel}: no master named '{master_name}'")),
            Err(e) => problems.push(format!("{sentinel}: {e}")),
        }
    }
    Err(Error::connection(format!(
        "no sentinel gave the address of master '{master_name}': {}",
        problems.join("; ")
    )))
}

/// `[host, port]` of `SENTINEL GET-MASTER-ADDR-BY-NAME` (`nil` when the name is unknown).
pub(crate) fn master_address(value: &Value) -> Option<Endpoint> {
    let Value::Array(items) = value else {
        return None;
    };
    let text = |v: &Value| match v {
        Value::BulkString(b) => String::from_utf8(b.clone()).ok(),
        Value::SimpleString(s) => Some(s.clone()),
        Value::Int(i) => Some(i.to_string()),
        _ => None,
    };
    let host = text(items.first()?)?;
    let port = text(items.get(1)?)?.parse::<u16>().ok()?;
    Some(Endpoint::new(host, port))
}

/// The distinct master addresses of a `CLUSTER SLOTS` reply, in slot order.
pub(crate) fn cluster_masters(value: &Value) -> Vec<Endpoint> {
    let mut masters: Vec<Endpoint> = Vec::new();
    let Value::Array(ranges) = value else {
        return masters;
    };
    for range in ranges {
        let Value::Array(parts) = range else { continue };
        if let Some(Value::Array(node)) = parts.get(2)
            && let Some(master) =
                master_address(&Value::Array(node.iter().take(2).cloned().collect()))
            && !masters.contains(&master)
        {
            masters.push(master);
        }
    }
    masters
}

/// Opens a cluster connection over the seeds (the crate's own TLS: cluster nodes are dialled
/// as they announce themselves, never through SSH).
async fn open_cluster(params: &ConnectParams, options: &RedisOptions) -> Result<Opened> {
    let timeout = params.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT);
    let first = options
        .seeds
        .first()
        .cloned()
        .ok_or_else(|| Error::invalid_input("the Redis profile has no host"))?;
    let tls = &params.tls;
    let tls_mode = match tls.mode {
        TlsMode::Disable => None,
        TlsMode::Prefer => {
            let mut probe = super::net::tcp(&Address::direct(first)).await?;
            if super::net::speaks_resp(&mut probe).await {
                None
            } else {
                Some(redis::TlsMode::Insecure)
            }
        }
        TlsMode::Require => Some(redis::TlsMode::Insecure),
        TlsMode::VerifyCa | TlsMode::VerifyFull => Some(redis::TlsMode::Secure),
    };
    let certificates = if tls_mode.is_some() {
        Some(cluster_certificates(tls)?)
    } else {
        None
    };
    let setup = ClusterSetup {
        params,
        options,
        timeout,
        tls_mode,
        certificates,
    };
    let (conn, protocol) = match options.protocol {
        Protocol::Resp2 => (
            setup.connect(ProtocolVersion::RESP2).await?,
            ProtocolVersion::RESP2,
        ),
        Protocol::Resp3 => (
            setup.connect(ProtocolVersion::RESP3).await?,
            ProtocolVersion::RESP3,
        ),
        Protocol::Auto => match setup.connect(ProtocolVersion::RESP3).await {
            Ok(c) => (c, ProtocolVersion::RESP3),
            Err(Error::Unsupported { .. }) => (
                setup.connect(ProtocolVersion::RESP2).await?,
                ProtocolVersion::RESP2,
            ),
            Err(e) => return Err(e),
        },
    };
    let mut link = Link::Cluster(conn);
    let facts = read_facts(&mut link).await;
    Ok(Opened {
        link,
        facts,
        tls: tls_mode.is_some(),
        protocol,
    })
}

/// Everything a cluster client is built from.
struct ClusterSetup<'a> {
    params: &'a ConnectParams,
    options: &'a RedisOptions,
    timeout: Duration,
    tls_mode: Option<redis::TlsMode>,
    certificates: Option<redis::TlsCertificates>,
}

impl ClusterSetup<'_> {
    /// Connects with `protocol`; a server without RESP3 is `Unsupported`.
    async fn connect(&self, protocol: ProtocolVersion) -> Result<ClusterConnection> {
        let seeds = self
            .options
            .seeds
            .iter()
            .map(|e| ConnectionAddr::Tcp(e.host.clone(), e.port));
        let mut builder = ClusterClientBuilder::new(seeds)
            .connection_timeout(self.timeout)
            .response_timeout(CLUSTER_RESPONSE_TIMEOUT)
            .use_protocol(protocol);
        if let Some(user) = self
            .params
            .user
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        {
            builder = builder.username(user);
        }
        if let Some(password) = self.params.password.as_ref().filter(|p| !p.is_empty()) {
            builder = builder.password(password.expose());
        }
        if let Some(mode) = self.tls_mode {
            builder = builder.tls(mode);
            if self.params.tls.mode == TlsMode::VerifyCa {
                builder = builder.danger_accept_invalid_hostnames(true);
            }
            if let Some(certs) = self.certificates.clone() {
                builder = builder.certs(certs);
            }
        }
        let client = builder.build().map_err(|e| from_redis(&e))?;
        client.get_async_connection().await.map_err(|e| {
            if is_resp3_refused(&e) {
                Error::unsupported(e.to_string())
            } else {
                from_redis(&e)
            }
        })
    }
}

/// The `redis` crate's certificate bundle for a cluster: the CA (else the platform store) and
/// the client identity.
fn cluster_certificates(tls: &oxbase_types::TlsOptions) -> Result<redis::TlsCertificates> {
    let root_cert = tls
        .ca_cert
        .as_ref()
        .map(oxbase_net::tls::load_pem)
        .transpose()?;
    let client_tls = match (&tls.client_cert, &tls.client_key) {
        (Some(cert), Some(key)) => Some(redis::ClientTlsConfig {
            client_cert: oxbase_net::tls::load_pem(cert)?,
            client_key: oxbase_net::tls::load_key_pem(key)?
                .expose()
                .as_bytes()
                .to_vec(),
        }),
        _ => None,
    };
    Ok(redis::TlsCertificates {
        client_tls,
        root_cert,
    })
}

/// `HELLO` (no arguments: the connection facts without switching anything; allowed for every
/// ACL user), else `INFO server` and `CLIENT ID`. Missing facts stay unknown.
async fn read_facts(link: &mut Link) -> Facts {
    let mut version = None;
    let mut server = None;
    let mut client_id = None;
    if let Some(hello) = fact(link, command("HELLO", None)).await {
        for (key, value) in hello_pairs(hello) {
            match key.as_str() {
                "version" => version = scalar_text(&value),
                "server" => server = scalar_text(&value),
                "id" => client_id = scalar_text(&value).and_then(|t| t.parse().ok()),
                _ => {}
            }
        }
    }
    if (version.is_none() || server.as_deref().is_some_and(|s| s != "redis"))
        && let Some(info) = fact(link, command("INFO", Some("server"))).await
        && let Some(text) = scalar_text(&info)
        && let Some(v) = info_field(&text, "redis_version")
    {
        version = Some(v);
    }
    if client_id.is_none() && matches!(link, Link::Single { .. }) {
        let mut id = redis::cmd("CLIENT");
        id.arg("ID");
        if let Some(Value::Int(i)) = fact(link, id).await {
            client_id = Some(i);
        }
    }
    let version = version
        .and_then(|v| ServerVersion::parse(&v).ok())
        .unwrap_or_else(|| ServerVersion::new(0, 0, 0));
    Facts { version, client_id }
}

fn command(name: &str, arg: Option<&str>) -> Cmd {
    let mut c = redis::cmd(name);
    if let Some(arg) = arg {
        c.arg(arg);
    }
    c
}

/// One fact query (a cluster: on a random master); `None` on any error.
async fn fact(link: &mut Link, cmd: Cmd) -> Option<Value> {
    let reply = match link {
        Link::Cluster(c) => {
            c.route_command(
                cmd,
                RoutingInfo::SingleNode(SingleNodeRoutingInfo::RandomPrimary),
            )
            .await
        }
        single => single.req_packed_command(&cmd).await,
    };
    match reply {
        Ok(Value::ServerError(_)) | Err(_) => None,
        Ok(v) => Some(v),
    }
}

/// The key / value pairs of a `HELLO` reply (RESP3 map or RESP2 flat array).
pub(crate) fn hello_pairs(value: Value) -> Vec<(String, Value)> {
    let pairs = match value {
        Value::Map(entries) => entries,
        Value::Array(items) => {
            let mut out = Vec::new();
            let mut it = items.into_iter();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                out.push((k, v));
            }
            out
        }
        _ => Vec::new(),
    };
    pairs
        .into_iter()
        .filter_map(|(k, v)| scalar_text(&k).map(|k| (k, v)))
        .collect()
}

pub(crate) fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(b) => String::from_utf8(b.clone()).ok(),
        Value::SimpleString(s) => Some(s.clone()),
        Value::VerbatimString { text, .. } => Some(text.clone()),
        Value::Int(i) => Some(i.to_string()),
        Value::Okay => Some("OK".to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    #[test]
    fn reads_sentinel_and_cluster_addresses() {
        assert_eq!(
            master_address(&Value::Array(vec![bulk("127.0.0.1"), bulk("15511")])),
            Some(Endpoint::new("127.0.0.1", 15511))
        );
        assert_eq!(master_address(&Value::Nil), None);
        assert_eq!(
            master_address(&Value::Array(vec![bulk("h"), bulk("x")])),
            None
        );

        let range = |start: i64, end: i64, port: &str| {
            Value::Array(vec![
                Value::Int(start),
                Value::Int(end),
                Value::Array(vec![
                    bulk("127.0.0.1"),
                    Value::Int(port.parse().unwrap()),
                    bulk("id"),
                ]),
                Value::Array(vec![bulk("127.0.0.1"), Value::Int(16000), bulk("replica")]),
            ])
        };
        let slots = Value::Array(vec![
            range(0, 5460, "15501"),
            range(5461, 10922, "15502"),
            range(10923, 16000, "15503"),
            range(16001, 16383, "15501"),
        ]);
        assert_eq!(
            cluster_masters(&slots),
            [15501, 15502, 15503].map(|p| Endpoint::new("127.0.0.1", p))
        );
        assert!(cluster_masters(&Value::Nil).is_empty());
    }

    #[test]
    fn reads_hello_replies_of_both_protocols() {
        let resp3 = Value::Map(vec![
            (bulk("server"), bulk("redis")),
            (bulk("version"), bulk("8.10.3")),
            (bulk("proto"), Value::Int(3)),
            (bulk("id"), Value::Int(42)),
        ]);
        let resp2 = Value::Array(vec![
            bulk("server"),
            bulk("redis"),
            bulk("id"),
            Value::Int(7),
        ]);
        let pairs = hello_pairs(resp3);
        assert_eq!(pairs.len(), 4);
        assert_eq!(scalar_text(&pairs[1].1).as_deref(), Some("8.10.3"));
        let pairs = hello_pairs(resp2);
        assert_eq!(pairs[1].0, "id");
        assert_eq!(scalar_text(&pairs[1].1).as_deref(), Some("7"));
        assert!(hello_pairs(Value::Nil).is_empty());
    }

    #[test]
    fn credentials_go_to_the_handshake_and_never_to_debug() {
        let params = ConnectParams::network(oxbase_types::Engine::Redis, "h")
            .with_user("m7_reader")
            .with_password("Secret_Marker_123");
        let info = redis_info(&params, ProtocolVersion::RESP3, 2, Credentials::Profile);
        assert_eq!(info.username(), Some("m7_reader"));
        assert_eq!(info.password(), Some("Secret_Marker_123"));
        assert_eq!(info.db(), 2);
        assert!(!format!("{info:?}").contains("Secret_Marker_123"));
        let none = redis_info(&params, ProtocolVersion::RESP2, 0, Credentials::None);
        assert_eq!(none.username(), None);
        assert_eq!(none.password(), None);
        // An empty user means the default user (no user name sent).
        let empty = ConnectParams::network(oxbase_types::Engine::Redis, "h").with_user(" ");
        assert_eq!(
            redis_info(&empty, ProtocolVersion::RESP2, 0, Credentials::Profile).username(),
            None
        );
    }
}
