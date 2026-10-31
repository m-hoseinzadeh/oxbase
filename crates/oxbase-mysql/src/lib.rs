//! MySQL and MariaDB driver and dialect cells, built on `mysql_async`. MariaDB differences are
//! version-flagged rather than split into a separate crate.
//!
//! One [`MySqlDriver`] serves [`Engine::MySql`](oxbase_types::Engine::MySql) and
//! [`Engine::MariaDb`](oxbase_types::Engine::MariaDb); [`register`] adds it to a
//! [`DriverRegistry`](oxbase_driver::DriverRegistry) for both. A connection reports the flavour
//! its server announces in `VERSION()` ([`Flavor`]).
//!
//! # Results
//!
//! - Rows stream as Arrow batches of at most [`MySqlOptions::batch_rows`] rows or about
//!   [`MySqlOptions::batch_bytes`] bytes, read with `query_iter` (text protocol, statements
//!   without parameters, multi-statement scripts allowed) or `exec_iter` (binary protocol,
//!   positional `?` parameters, one statement). See [`types`] for the type mapping.
//! - Each statement yields `Schema` + `Batch`es, or `RowsAffected`. A statement that generated an
//!   `AUTO_INCREMENT` id also yields an info notice with code [`LAST_INSERT_ID_CODE`]
//!   (`ResultItem` has no slot for it), and [`MySqlConnection::last_insert_id`] returns it.
//! - Warnings: when the last statement reports warnings, `SHOW WARNINGS` runs after it and each
//!   row becomes a [`Notice`](oxbase_driver::Notice) (code = MySQL error number). Earlier
//!   statements of a script only get a count, since the server keeps details for the last one.
//!
//! # Time zone
//!
//! By default the session runs `SET time_zone = '+00:00'` after connecting
//! ([`MySqlOptions::utc_session`]), so `TIMESTAMP` columns come back as UTC instants
//! (`Timestamp(µs, "UTC")`) and `Value::TimestampTz` parameters are stored exactly. `DATETIME`
//! stays a zone-less wall-clock value. Session functions such as `NOW()` report UTC.
//!
//! # Cancellation
//!
//! `KILL QUERY <connection id>` over a short-lived second connection with the session's own
//! options: the same dial address (so it works through an SSH tunnel), credentials and TLS. See
//! the `conn` module docs for how a kill is kept from reaching a later statement. If the
//! statement has not stopped about [`CANCEL_DEADLINE`] after the request (the side connection
//! cannot be opened, or the outcome of a `KILL` is unknown), the session is closed instead: the
//! stream still ends with `Error::Cancelled`, and later calls report a closed connection.
//!
//! # TLS
//!
//! rustls with the ring provider (no OpenSSL). Sockets go to
//! [`ConnectParams::dial_endpoint`](oxbase_driver::ConnectParams::dial_endpoint); the certificate
//! is checked against [`ConnectParams::tls_server_name`](oxbase_driver::ConnectParams::tls_server_name)
//! (also sent as SNI), so `VerifyFull` works through a tunnel. `Prefer` falls back to plaintext
//! only when the server does not offer TLS. Known limits:
//!
//! - Without a CA file, `VerifyCa` / `VerifyFull` trust `mysql_async`'s bundled Mozilla roots
//!   (`webpki-roots`), not the platform store.
//! - `VerifyCa` is refused with `Error::Unsupported`: `mysql_async` 0.37.1 detects a name
//!   mismatch by looking for `NotValidForName` in the rustls error text, which rustls 0.23.46 no
//!   longer prints, so it could only behave like `VerifyFull`. Use `VerifyFull` (with
//!   `TlsOptions::server_name` when connecting by IP or alias) or `Require`.
//! - `mysql_async` keeps the password and inline client key as plain `String`/bytes in its
//!   options for the life of the connection (needed for cancel side connections); they are
//!   never logged or formatted by this crate.

mod conn;
mod driver;
mod error;
pub mod types;

pub use conn::{CANCEL_DEADLINE, KILL_RETRY, LAST_INSERT_ID_CODE, MySqlConnection};
pub use driver::{
    DEFAULT_CONNECT_TIMEOUT, Flavor, MySqlDriver, MySqlOptions, OPT_BATCH_BYTES, OPT_BATCH_ROWS,
    OPT_TINYINT1_AS_BOOL, OPT_UTC_SESSION, register,
};
pub use types::TypeOptions;
