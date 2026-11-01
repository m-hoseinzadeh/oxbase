//! PostgreSQL driver, built on `tokio-postgres` with rustls (ring) TLS.
//!
//! Register it with [`register`]; everything else goes through `oxbase_driver::Connection`.
//!
//! # Addressing and TLS
//!
//! Sockets are opened at `ConnectParams::dial_endpoint()` (the SSH tunnel's local end when
//! tunnelled): an IP literal is passed to `tokio-postgres` as `hostaddr`, a DNS name as `host`.
//! TLS is configured by `oxbase_net::tls::client_config` and verifies
//! `ConnectParams::tls_server_name()`, never the dial address, so `VerifyFull` works through a
//! tunnel. `TlsMode::Prefer` falls back to plaintext when the server has no TLS;
//! `Connection::tls_in_use` reports what was negotiated. Cancel requests dial the same address
//! with the same TLS settings.
//!
//! # Options (`ConnectParams::options`)
//!
//! - `sslnegotiation`: `postgres` (default) or `direct` (PostgreSQL 17+, needs a TLS mode that
//!   requires TLS; sends the `postgresql` ALPN protocol).
//! - `options`: extra startup options (`-c search_path=app`).
//! - `channel_binding`: `disable`, `prefer` (default) or `require`.
//! - `target_session_attrs`: `any`, `read-write` or `read-only`.
//!
//! The driver starts sessions with `DateStyle=ISO` and `bytea_output=hex`, the formats its text
//! decoder reads. A session that changes them later gets NULLs (with a warning notice) for
//! values it cannot read. It also pins `standard_conforming_strings=on`, which the SQL lexer
//! assumes; generated DDL writes backslash literals as `E'...'`, so it means the same either way.
//!
//! # Results
//!
//! See [`types`] for the type mapping and `conn.rs` for how statements are sent. Results are
//! streamed in batches of at most [`BATCH_ROWS`] rows (fewer when rows are wide).
//!
//! # Dialect (M2)
//!
//! [`dialect::register`] adds the PostgreSQL catalog fetchers, object-tree layout, capabilities
//! and quoting to an `oxbase_dialect::DialectRegistry`.

mod conn;
mod decode;
pub mod dialect;
mod encode;
mod error;
mod split;
mod tls;
pub mod types;
mod values;

use std::sync::Arc;

use async_trait::async_trait;
use oxbase_driver::{ConnectParams, Connection, Driver, DriverRegistry};
use oxbase_types::{Engine, Result};

pub use conn::PgConnection;
pub use decode::BATCH_ROWS;

/// The PostgreSQL [`Driver`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PgDriver;

impl PgDriver {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for PgDriver {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        Ok(Box::new(PgConnection::connect(params).await?))
    }
}

/// Registers the PostgreSQL driver in `registry`.
pub fn register(registry: &mut DriverRegistry) {
    registry.register(Arc::new(PgDriver::new()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::Error;

    fn expect_err<T>(r: Result<T>) -> Error {
        match r {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        }
    }

    #[test]
    fn registers_for_postgres() {
        let mut registry = DriverRegistry::new();
        register(&mut registry);
        assert_eq!(registry.engines(), [Engine::Postgres]);
    }

    #[tokio::test]
    async fn rejects_bad_params_without_dialling() {
        let err = expect_err(
            PgDriver
                .connect(
                    &ConnectParams::network(Engine::Postgres, "db").with_password("pw-secret-1"),
                )
                .await,
        );
        assert!(err.to_string().contains("user name"), "{err}");
        assert!(!err.to_string().contains("pw-secret-1"));
        let err = expect_err(
            PgDriver
                .connect(&ConnectParams::network(Engine::MySql, "db").with_user("u"))
                .await,
        );
        assert!(err.to_string().contains("MySQL"), "{err}");
    }

    #[tokio::test]
    async fn unreachable_server_is_a_connection_error_without_secrets() {
        // Port 1 on loopback: nothing listens there.
        let params = ConnectParams::network(Engine::Postgres, "127.0.0.1")
            .with_port(1)
            .with_user("someone")
            .with_password("marker-pw-0451");
        let err = expect_err(PgDriver.connect(&params).await);
        let text = format!("{err} {err:?}");
        assert!(err.is_transient(), "{text}");
        assert!(text.contains("127.0.0.1:1"), "{text}");
        assert!(!text.contains("marker-pw-0451"), "{text}");
    }
}
