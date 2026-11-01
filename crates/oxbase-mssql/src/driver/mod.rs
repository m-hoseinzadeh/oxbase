//! The SQL Server [`Driver`] (M4, **driver** stream). Contract: `docs/m4-contracts.md` §2; the
//! summary below is what the stream codes against.
//!
//! # Transport
//!
//! - TCP to [`ConnectParams::dial_endpoint`] (the SSH tunnel's local end when tunnelled), with
//!   `set_nodelay(true)`; adapted to futures-io with `tokio_util::compat` for `tiberius`.
//! - `tiberius::Client::connect(config, stream)` runs PRELOGIN + LOGIN7 (TDS 7.4) over the
//!   stream oxbase hands it. tiberius is built **without a TLS backend**, so it always believes
//!   the connection is unencrypted (`EncryptionLevel::NotSupported`) and never touches TLS.
//! - `tls.rs` runs the encryption itself, with `oxbase_net::tls::client_config` (rustls + ring,
//!   verification per `TlsMode`, server name = [`ConnectParams::tls_server_name`], never the
//!   dial address):
//!   - **TDS 7.4 (default)**: oxbase sends its own PRELOGIN (`VERSION`, `ENCRYPTION`,
//!     `INSTOPT`, `THREADID`, `MARS = 0`) and reads the server's. If both sides agree on
//!     encryption, it performs the TLS handshake with each TLS record wrapped in TDS packets
//!     of type `0x12` (PRELOGIN) until the handshake completes; from then on TLS records go
//!     straight over TCP and every TDS packet travels inside TLS (full encryption: oxbase never
//!     sends `ENCRYPT_OFF`, so there is no "login-only" encryption). The stream given to
//!     tiberius then swallows tiberius' own PRELOGIN packet and answers it with the server's
//!     saved PRELOGIN response (its `ENCRYPTION` byte set to `ENCRYPT_NOT_SUP`), so tiberius
//!     continues with LOGIN7 inside the TLS session.
//!   - **TDS 8.0 strict** (`options::ENCRYPT = strict`, SQL Server 2022+): TLS first, directly
//!     on TCP, ALPN `tds/8.0`; then the same PRELOGIN exchange and replay inside TLS.
//!   - **No encryption**: PRELOGIN with `ENCRYPT_NOT_SUP`; the stream is passed through.
//! - `TlsMode` → PRELOGIN `ENCRYPTION` and outcome:
//!
//!   | `TlsMode` | Client sends | Server `NOT_SUP` | Server `OFF` / `ON` / `REQ` |
//!   |---|---|---|---|
//!   | `Disable` | `NOT_SUP` | plaintext | `REQ`: fail ("the server requires encryption"); `OFF`/`ON`: plaintext |
//!   | `Prefer` | `ON` | plaintext (`tls_in_use() == false`) | TLS, certificate not checked |
//!   | `Require` | `ON` | fail | TLS, certificate not checked |
//!   | `VerifyCa` | `ON` | fail | TLS, chain checked, name not checked |
//!   | `VerifyFull` | `ON` | fail | TLS, chain and name checked |
//!
//!   Strict: `Require` / `VerifyCa` / `VerifyFull` only (others are `InvalidInput`).
//!   `Connection::tls_in_use` reports what was negotiated. Client certificates
//!   (`TlsOptions::client_cert`) are refused with `Unsupported` (SQL Server has no certificate
//!   login over TDS 7.4; strict-mode client certificates are not in M4).
//! - Authentication: SQL Server logins only (`AuthMethod::sql_server`). Windows (NTLM /
//!   Kerberos) and Microsoft Entra ID are not in M4 (`ConnectParams::user` is required).
//! - Named instances: [`crate::options::INSTANCE`]; a port is required (no SQL Browser).
//! - [`Connection::server_version`]: `SERVERPROPERTY('ProductVersion')` (`17.0.4005.7` →
//!   17.0.4005.7) read once after login, together with `DB_NAME()`
//!   ([`Connection::current_database`]) and `@@SPID`; `raw` keeps `@@VERSION`'s first line.
//!
//! # Statements
//!
//! - `execute(sql, [])`: a SQL batch (`Client::simple_query`). The batch runs in the session's
//!   own scope, so `USE`, `SET`, `#temp` tables and transactions persist (the editor sends one
//!   `GO` batch per call).
//! - `execute(sql, params)` with parameters: `sp_executesql` through an RPC request
//!   (`tiberius::Command::new("sp_executesql")` with `@stmt`, `@params` = the declarations
//!   `@P1 <type>, ...` derived from each `Value`, and `@P1 ... @Pn`), which yields rows *and*
//!   per-statement row counts. Placeholders are `@P1..@Pn` (`BracketQuoting::placeholder`).
//!   Session-scoped effects inside it (`SET`, `#temp` created there) end with it.
//! - Result items: `Schema` + `Batch`es per result set (multiple result sets in order),
//!   `RowsAffected(n)` for every DONE with a count that does not close a result set (RPC path
//!   only: tiberius' batch stream does not report counts, see *Known gaps*), errors as
//!   `Error::Sql { code: Some(<error number>), message }` (message includes procedure and line
//!   when the server sends them). Login failures (18456) are `Error::Auth`.
//! - Batches of at most [`types::BATCH_ROWS`] rows (option [`crate::options::BATCH_ROWS`]).
//!
//! # Cancellation
//!
//! In-band TDS **attention** (works through an SSH tunnel; no side connection, and never
//! `KILL`, which would end the whole session). `execute` runs the statement in a task that owns
//! the client; on the token or [`Connection::cancel_running`] it stops reading, sends the
//! attention (`Client::cancel_query`) and drains to the `DONE` with the attention bit. The
//! stream ends with one `Err(Error::Cancelled)` **within 2 s**. If the drain fails or takes
//! longer than 1.5 s, the driver closes the socket and the connection reports
//! `Error::Connection` from then on (the M2 query service then opens a new session, as for
//! MySQL). A cancel never reaches a later statement: the attention is sent only while the
//! targeted request is still in flight on this connection.
//!
//! # Transactions
//!
//! [`Connection::begin`]: `BEGIN TRANSACTION`; [`oxbase_driver::Tx::commit`] /
//! [`oxbase_driver::Tx::rollback`]: `COMMIT` / `ROLLBACK` (`IF @@TRANCOUNT > 0`). A dropped
//! `Tx` is rolled back before the connection's next statement. DDL is transactional on SQL
//! Server, so designer scripts run atomically.
//!
//! # Known gaps (tiberius 0.13.0, accepted for M4)
//!
//! - `INFO` messages (`PRINT`, `RAISERROR` with severity 0-10, `DBCC` output) are not exposed by
//!   tiberius: the driver yields no `Notice` for them.
//! - SQL batches report no per-statement row counts (`RowsAffected`); parameterized statements
//!   do.
//! - Result metadata carries only the column name and a coarse type (no length, precision,
//!   scale or nullability), which shapes the Arrow mapping ([`types`]).
//!
//! # Secrets
//!
//! The password goes to tiberius' `AuthMethod` (held in `secrecy::SecretString`); it never
//! appears in `Debug` of [`MsSqlConnection`], in errors, logs or panics. Do not `Debug`-print a
//! `tiberius::Config`.

mod conn;
mod error;
mod tls;
pub mod types;

use std::sync::Arc;

use async_trait::async_trait;
use oxbase_driver::{ConnectParams, Connection, Driver, DriverRegistry};
use oxbase_types::{Engine, Result};

pub use conn::MsSqlConnection;

/// The SQL Server [`Driver`].
#[derive(Debug, Clone, Copy, Default)]
pub struct MsSqlDriver;

impl MsSqlDriver {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for MsSqlDriver {
    fn engine(&self) -> Engine {
        Engine::MsSql
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        let _ = params;
        unimplemented!("m4: driver")
    }
}

/// Registers the SQL Server driver in `registry`.
pub fn register(registry: &mut DriverRegistry) {
    registry.register(Arc::new(MsSqlDriver::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_for_mssql() {
        let mut registry = DriverRegistry::new();
        register(&mut registry);
        assert_eq!(registry.engines(), [Engine::MsSql]);
        assert_eq!(MsSqlDriver::new().engine(), Engine::MsSql);
    }
}
