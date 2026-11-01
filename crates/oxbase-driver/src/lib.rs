//! Driver abstraction: the [`Driver`] / [`Connection`] / [`Tx`] traits, streaming results as
//! Arrow `RecordBatch`es, and cooperative cancellation via [`CancellationToken`].
//!
//! Engine crates (`oxbase-pg`, `oxbase-mysql`, ...) implement these traits; everything above them
//! (dialects, services, the CLI and the app) only sees `dyn Connection`.
//!
//! # Cancellation contract
//!
//! - [`Connection::execute`] takes a [`CancellationToken`]. When the token is cancelled the driver
//!   stops producing items, asks the server to cancel the statement if it can, yields one
//!   `Err(Error::Cancelled)` and then ends the stream.
//! - [`Connection::cancel_running`] cancels whatever statement is currently running on the
//!   connection, with the same effect on its stream, without the caller needing the token.
//! - A token that is already cancelled makes `execute` return `Err(Error::Cancelled)` without
//!   sending anything to the server.
//!
//! Out-of-band cancel requests (PostgreSQL `CancelRequest`, the MySQL `KILL QUERY` side
//! connection) dial [`ConnectParams::dial_endpoint`], the same address as the session itself, so
//! cancel works through an SSH tunnel. A cancel that reaches the server after the statement
//! finished is harmless; drivers must make sure it cannot hit a *later* statement (MySQL: kill
//! by the connection id only while the statement it targets is still running).
//!
//! # Addressing, TLS and SSH
//!
//! See [`ConnectParams`]: drivers dial [`ConnectParams::dial_endpoint`], verify TLS against
//! [`ConnectParams::tls_server_name`], build their rustls configuration with
//! `oxbase_net::tls::client_config` (or, when the client library builds its own, map
//! [`TlsOptions`] onto it with the same semantics and load PEM material through `oxbase_net::tls`),
//! and never open SSH tunnels themselves.
//!
//! # Progress
//!
//! Long-running operations above the driver (catalog loads, import, transfer, backup) take a
//! [`ProgressSink`] next to their [`CancellationToken`] and report [`Progress`] events through
//! it. Reporting is lossy and never blocks the operation.
//!
//! # Native access (M5)
//!
//! [`Connection::as_any`] and [`Tx::as_any`] hand a dialect cell of the *same engine crate* the
//! concrete connection type, for the client library's own API where plain statements are not
//! enough (PostgreSQL `COPY ... FROM STDIN`, SQL Server bulk load; `docs/m5-contracts.md` §4).
//! Both default to `None`; nothing outside the engine crate may downcast.
//!
//! [`cell_value`] reads one Arrow cell as a [`Value`] (the inverse of what drivers produce), for
//! bulk loaders and the SQL dump writer.
//!
//! # Feature `mock`
//!
//! Enables `mock::MockDriver`, an in-memory driver that streams fixed Arrow batches and honours
//! cancellation. Higher layers use it in their tests.

mod cell;
mod params;
mod progress;
mod registry;
mod result;

#[cfg(feature = "mock")]
pub mod mock;

use async_trait::async_trait;
use oxbase_types::{Engine, Result, ServerVersion, Value};

pub use cell::cell_value;
pub use oxbase_types::{
    Endpoint, KeySource, KnownHostsPolicy, PemSource, Secret, SshAuth, SshOptions, TlsMode,
    TlsOptions,
};
pub use oxbase_types::{Progress, ProgressUnit};
pub use params::{ConnectParams, is_sensitive_option};
pub use progress::ProgressSink;
pub use registry::DriverRegistry;
pub use result::{
    Collected, Notice, NoticeSeverity, ResultItem, ResultSet, ResultStream, collect, from_arrow,
};
pub use tokio_util::sync::CancellationToken;

/// Re-exports of the Arrow types that appear in this crate's API.
pub mod arrow {
    pub use arrow_array::RecordBatch;
    pub use arrow_schema::{ArrowError, DataType, Field, Schema, SchemaRef};
}

/// Opens connections to one engine.
#[async_trait]
pub trait Driver: Send + Sync {
    /// The engine this driver serves. A driver may also be registered for a sibling engine
    /// (the MySQL driver serves MariaDB) with [`DriverRegistry::register_as`].
    fn engine(&self) -> Engine;

    /// Opens a connection described by `params` (see [`ConnectParams`] for the addressing and
    /// TLS rules). Any SSH tunnel is already open: dial [`ConnectParams::dial_endpoint`].
    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>>;
}

/// One open session with a server.
///
/// A connection runs one statement at a time: drain or drop the previous [`ResultStream`] before
/// calling [`Connection::execute`] again. Result streams are `'static` and do not borrow the
/// connection, so they can be handed to another task (for example the channel feeding the UI).
#[async_trait]
pub trait Connection: Send + Sync {
    fn engine(&self) -> Engine;

    /// Version reported by the server at connect time.
    fn server_version(&self) -> &ServerVersion;

    /// `true` when the session's transport is encrypted with TLS (with `TlsMode::Prefer` this
    /// says whether the server accepted TLS). Drivers that can negotiate TLS must override it;
    /// the default suits embedded engines.
    fn tls_in_use(&self) -> bool {
        false
    }

    /// The database (catalog) this session was opened on, when the driver knows it without a
    /// round trip (M2). Used by `oxbase-services` to decide whether a tree node or a query in
    /// database `X` can use this session or needs another one (see
    /// `oxbase_dialect::DatabaseScope`).
    ///
    /// - PostgreSQL: the connected database (`ConnectParams::database`, or the user name, which
    ///   is the server's default).
    /// - MySQL / MariaDB: the default database given at connect time, if any (a later `USE`
    ///   in the session is not tracked).
    /// - SQLite: `main`.
    ///
    /// `None` means unknown; callers then assume the session is on no particular database.
    fn current_database(&self) -> Option<String> {
        None
    }

    /// Runs `sql` (one or more statements, engine permitting) with positional `params`.
    ///
    /// The stream yields, per statement, either `Schema` followed by zero or more `Batch`es, or
    /// `RowsAffected`; `Notice`s may appear anywhere. See the crate docs for cancellation.
    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream>;

    /// Cancels the statement currently running on this connection, if any. Returns `Ok` when
    /// nothing is running.
    async fn cancel_running(&self) -> Result<()>;

    /// Starts a transaction. Statements run through the returned [`Tx`] until it is committed or
    /// rolled back.
    async fn begin(&self) -> Result<Box<dyn Tx>>;

    /// Closes the connection. Later calls fail with a connection error. Closing twice is `Ok`.
    async fn close(&self) -> Result<()>;

    /// M5: the concrete connection, for dialect cells of the same engine crate that need the
    /// client library's own API (PostgreSQL `COPY FROM STDIN`, SQL Server bulk load). `None` by
    /// default; an engine returns `Some(self)` when one of its cells downcasts. Code outside the
    /// engine crate must not downcast (PLAN §1, rule 3: no engine special cases elsewhere).
    fn as_any(&self) -> Option<&(dyn std::any::Any + Send + Sync)> {
        None
    }
}

/// An open transaction.
///
/// Dropping a `Tx` without calling [`Tx::commit`] rolls it back (drivers do this on drop or on the
/// next use of the connection).
#[async_trait]
pub trait Tx: Send + Sync {
    /// Same contract as [`Connection::execute`], inside the transaction.
    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream>;

    async fn commit(self: Box<Self>) -> Result<()>;

    async fn rollback(self: Box<Self>) -> Result<()>;

    /// M5: the concrete transaction, like [`Connection::as_any`]. `None` by default. A native
    /// load inside a transaction runs on the transaction's session (PostgreSQL and SQL Server
    /// transactions are session state, so the engine reaches its client through this).
    fn as_any(&self) -> Option<&(dyn std::any::Any + Send + Sync)> {
        None
    }
}
