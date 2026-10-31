//! SQLite driver, built on `rusqlite` with a bundled SQLite.
//!
//! - **Threading.** Every [`SqliteConnection`] owns a dedicated worker thread that holds the
//!   `rusqlite::Connection`; async callers only exchange messages with it, so they never block
//!   the runtime.
//! - **Opening.** [`ConnectParams::file`](oxbase_driver::ConnectParams::file) is a path,
//!   `:memory:` or a `file:` URI. The options [`OPT_READ_ONLY`], [`OPT_CREATE_IF_MISSING`] and
//!   [`OPT_BUSY_TIMEOUT_MS`] (in `ConnectParams::options`) set the open mode; see
//!   [`SqliteOptions`]. A missing file opened without create fails with `NotFound`.
//! - **SSH and TLS do not apply** to an embedded file. A profile that sets SSH, a dial address,
//!   a TLS mode that requires TLS, or TLS material fails with `Unsupported`.
//! - **Results** stream as Arrow batches over a bounded channel (backpressure on the worker).
//!   Scripts of several statements run in order, split by SQLite's own parser. Each statement
//!   yields `Schema` + `Batch`es, or `RowsAffected`. Column types follow the declared type's
//!   affinity with a per-value fallback (see "Type mapping"); mixed-type columns become `Utf8`. The
//!   declared type travels in the field metadata under [`DECL_TYPE_METADATA_KEY`].
//! - **Parameters** are positional [`Value`](oxbase_types::Value)s bound by index; every
//!   statement of a script binds from the same list. When parameters are given, a count that
//!   does not match what the statements use fails before anything runs, as long as every
//!   statement can be prepared up front. A script whose later statements depend on its own
//!   earlier DDL is checked as it runs instead: a statement short of parameters fails before it
//!   runs, but unused extra parameters are reported only after the statements have run (and,
//!   outside a transaction, committed).
//! - **Cancellation**: the execution token, [`Connection::cancel_running`]
//!   and dropping the stream all interrupt SQLite (`sqlite3_interrupt` through rusqlite's
//!   `InterruptHandle`, plus a progress handler that closes the race between statements). A
//!   statement waiting for a lock held by another connection stops waiting within about 10 ms
//!   of a cancel (the driver's own busy handler replaces SQLite's; running
//!   `PRAGMA busy_timeout` as SQL puts SQLite's back, and a cancel then waits out the timeout).
//!   A worker waiting for the consumer to read (stream held but not polled) is freed by a cancel
//!   or by `close` too. The stream ends with `Err(Cancelled)` and the connection stays usable.
//! - **Version**: `server_version()` is `sqlite_version()`.
//! - **Transactions**: [`Connection::begin`] runs `BEGIN`; a
//!   dropped [`SqliteTx`] rolls back.
//!
//! # Type mapping
//!
//! SQLite is dynamically typed: a column's declared type only gives it an *affinity*, and any
//! value may still hold any storage class. A result set needs one Arrow type per column, fixed
//! when its schema is sent, so the driver decides it from the declared type **and** the values
//! of the first batch. The first batch holds up to 2048 rows. It is sent earlier only when it
//! reaches 8 MiB, when it has at least 256 rows and has been filling for 250 ms since its first
//! row, or when it has been filling for 2 s since its first row (a query that yields rows
//! slowly), so a query that is slow to produce its first row still samples many rows:
//!
//! | Values seen in the first batch | Arrow type |
//! |---|---|
//! | integers only | `Int64`; `Float64` in a `REAL` column, and in a `NUMERIC`-affinity column (`NUMERIC`, `DECIMAL(p,s)`, ...) other than `BOOLEAN`, because NUMERIC affinity stores whole values such as `10.00` as integers and later rows may hold `9.75` (there `Utf8` if an integer is beyond ±2^53) |
//! | reals only, or integers and reals | `Float64` (`Utf8` if an integer is beyond ±2^53, which `Float64` cannot hold exactly) |
//! | text only | `Utf8` |
//! | blobs only | `Binary` |
//! | any other mix (mixed-type column) | `Utf8` (numbers as text, blobs as `0x…` hex) |
//! | none (no rows, or all `NULL`) | from the declared type's affinity: `INTEGER` → `Int64`, `REAL` → `Float64`, `TEXT` → `Utf8`, `BLOB` → `Binary`, `NUMERIC` → `Int64` for booleans, `Utf8` for dates and times, `Float64` otherwise; no declared type (an expression) → `Utf8` |
//!
//! A value in a later batch that does not fit the chosen type is converted when that is exact
//! (an integer into `Float64`, an integral real into `Int64`, numeric text into a number, anything
//! into `Utf8`, text into `Binary`). Otherwise it is returned as `NULL` and the result set is
//! followed by a warning [`Notice`](oxbase_driver::Notice) naming the column and the count.
//!
//! Every field is nullable. A field whose column has a declared type carries it in the field
//! metadata under [`DECL_TYPE_METADATA_KEY`].

mod connection;
mod convert;
mod error;
mod options;
mod worker;

use async_trait::async_trait;
use oxbase_driver::{ConnectParams, Connection, Driver};
use oxbase_types::{Engine, Result};

pub use connection::{SqliteConnection, SqliteTx};
pub use convert::DECL_TYPE_METADATA_KEY;
pub use options::{OPT_BUSY_TIMEOUT_MS, OPT_CREATE_IF_MISSING, OPT_READ_ONLY, SqliteOptions};

/// The SQLite [`Driver`]. Register it with `DriverRegistry::register(Arc::new(SqliteDriver))`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteDriver;

#[async_trait]
impl Driver for SqliteDriver {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        let opts = SqliteOptions::from_params(params)?;
        Ok(Box::new(SqliteConnection::open(opts).await?))
    }
}
