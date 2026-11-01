//! [`MsSqlConnection`]: sessions, statement execution, cancellation and transactions (driver
//! stream). See the [`crate::driver`] module docs for the contract.
//!
//! Suggested shape (the stream may choose another that keeps the contract): the
//! `tiberius::Client` lives behind an `Arc<tokio::sync::Mutex<..>>`; every `execute` spawns a
//! task that takes the lock (owned guard), runs the request and feeds a bounded channel that is
//! the returned [`ResultStream`], so the stream is `'static` and nothing is collected in memory.
//! The task watches the statement's token and a per-connection cancel generation (bumped by
//! `cancel_running`) and performs the attention drain itself, since `Client::cancel_query`
//! needs `&mut Client`.

use async_trait::async_trait;
use oxbase_driver::{CancellationToken, Connection, ResultStream, Tx};
use oxbase_types::{Engine, Result, ServerVersion, Value};

/// One SQL Server session. `Debug` never shows the password.
#[derive(Debug)]
pub struct MsSqlConnection {
    _private: (),
}

#[async_trait]
impl Connection for MsSqlConnection {
    fn engine(&self) -> Engine {
        Engine::MsSql
    }

    fn server_version(&self) -> &ServerVersion {
        unimplemented!("m4: driver")
    }

    fn tls_in_use(&self) -> bool {
        unimplemented!("m4: driver")
    }

    fn current_database(&self) -> Option<String> {
        unimplemented!("m4: driver")
    }

    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        let _ = (sql, params, cancel);
        unimplemented!("m4: driver")
    }

    async fn cancel_running(&self) -> Result<()> {
        unimplemented!("m4: driver")
    }

    async fn begin(&self) -> Result<Box<dyn Tx>> {
        unimplemented!("m4: driver")
    }

    async fn close(&self) -> Result<()> {
        unimplemented!("m4: driver")
    }
}
