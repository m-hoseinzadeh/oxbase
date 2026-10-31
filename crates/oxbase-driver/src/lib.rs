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
//! # Progress
//!
//! Long-running operations above the driver (catalog loads, import, transfer, backup) take a
//! [`ProgressSink`] next to their [`CancellationToken`] and report [`Progress`] events through
//! it. Reporting is lossy and never blocks the operation.
//!
//! # Feature `mock`
//!
//! Enables `mock::MockDriver`, an in-memory driver that streams fixed Arrow batches and honours
//! cancellation. Higher layers use it in their tests.

mod params;
mod progress;
mod registry;
mod result;

#[cfg(feature = "mock")]
pub mod mock;

use async_trait::async_trait;
use oxbase_types::{Engine, Result, ServerVersion, Value};

pub use oxbase_types::{Progress, ProgressUnit};
pub use params::{
    ConnectParams, Secret, SshAuth, SshOptions, TlsMode, TlsOptions, is_sensitive_option,
};
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

    /// Opens a connection. Tunnelling (SSH) and TLS are driven by `params`.
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
}
