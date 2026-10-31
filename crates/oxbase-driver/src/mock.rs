//! In-memory mock driver (feature `mock`).
//!
//! [`MockDriver`] needs no server. Every connection answers:
//!
//! - statements starting with `SELECT`, `WITH`, `VALUES` or `SHOW` with one result set of
//!   [`MockConfig::batches`] batches of [`MockConfig::rows_per_batch`] rows, schema
//!   `(id Int64 NOT NULL, name Utf8 NOT NULL)`, with ids counting up from 0 and names `row <id>`;
//! - statements starting with `FAIL` with an [`Error::Sql`] (code `MOCK1`);
//! - anything else with `RowsAffected(0)`.
//!
//! [`MockConfig::batch_delay`] is slept before each batch, which makes cancellation observable.
//! Connecting as user `denied` fails with an auth error. A mock refuses parameters for another
//! engine unless that engine was added with [`MockDriver::also_serves`] (the way the MySQL driver
//! serves MariaDB); its connections report the engine they were opened for. Every executed
//! statement (and
//! `BEGIN` / `COMMIT` / `ROLLBACK`) is appended to a log shared by the driver and its connections.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream;
use oxbase_types::{Engine, Error, Result, ServerVersion, Value};
use tokio_util::sync::CancellationToken;

use crate::{ConnectParams, Connection, Driver, ResultItem, ResultStream, Tx, from_arrow};

/// Shape and pacing of mock query results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockConfig {
    pub batches: usize,
    pub rows_per_batch: usize,
    /// Delay before each batch.
    pub batch_delay: Duration,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            batches: 3,
            rows_per_batch: 4,
            batch_delay: Duration::ZERO,
        }
    }
}

type Log = Arc<Mutex<Vec<String>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A driver that serves canned Arrow batches from memory.
#[derive(Debug, Clone)]
pub struct MockDriver {
    engine: Engine,
    /// Sibling engines this mock also accepts (see [`MockDriver::also_serves`]).
    also: Vec<Engine>,
    version: ServerVersion,
    config: MockConfig,
    log: Log,
}

impl MockDriver {
    /// A mock for `engine` reporting version `1.0.0` with the default [`MockConfig`].
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            also: Vec::new(),
            version: ServerVersion::new(1, 0, 0),
            config: MockConfig::default(),
            log: Log::default(),
        }
    }

    /// Also accepts connections for `engines` (siblings served by the same driver, such as
    /// MariaDB by the MySQL driver). Such connections report the requested engine.
    #[must_use]
    pub fn also_serves(mut self, engines: &[Engine]) -> Self {
        self.also.extend_from_slice(engines);
        self
    }

    /// `true` if this mock accepts connections for `engine`.
    pub fn serves(&self, engine: Engine) -> bool {
        engine == self.engine || self.also.contains(&engine)
    }

    #[must_use]
    pub fn with_version(mut self, version: ServerVersion) -> Self {
        self.version = version;
        self
    }

    #[must_use]
    pub fn with_config(mut self, config: MockConfig) -> Self {
        self.config = config;
        self
    }

    /// The schema of every mock result set.
    pub fn schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
        ]))
    }

    /// Statements executed so far on all connections of this driver, in order.
    pub fn log(&self) -> Vec<String> {
        lock(&self.log).clone()
    }
}

#[async_trait]
impl Driver for MockDriver {
    fn engine(&self) -> Engine {
        self.engine
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        if !self.serves(params.engine) {
            return Err(Error::invalid_input(format!(
                "mock driver for {} cannot connect to {}",
                self.engine, params.engine
            )));
        }
        if params.user.as_deref() == Some("denied") {
            return Err(Error::auth("mock: user 'denied' is always rejected"));
        }
        Ok(Box::new(MockConnection {
            inner: Arc::new(Inner {
                engine: params.engine,
                version: self.version.clone(),
                config: self.config.clone(),
                log: self.log.clone(),
                running: Mutex::new(None),
                closed: AtomicBool::new(false),
            }),
        }))
    }
}

struct Inner {
    engine: Engine,
    version: ServerVersion,
    config: MockConfig,
    log: Log,
    /// Token of the statement currently streaming, for `cancel_running`.
    running: Mutex<Option<CancellationToken>>,
    closed: AtomicBool,
}

impl Inner {
    fn check_open(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(Error::connection("mock connection is closed"))
        } else {
            Ok(())
        }
    }

    fn record(&self, sql: &str) {
        lock(&self.log).push(sql.to_owned());
    }

    fn execute(&self, sql: &str, cancel: &CancellationToken) -> Result<ResultStream> {
        self.check_open()?;
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        self.record(sql);
        let head = sql
            .trim_start()
            .split(|c: char| !c.is_ascii_alphabetic())
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        match head.as_str() {
            "SELECT" | "WITH" | "VALUES" | "SHOW" => {
                // A child token: cancelling it (cancel_running) does not cancel the caller's token.
                let token = cancel.child_token();
                *lock(&self.running) = Some(token.clone());
                Ok(rows_stream(token, self.config.clone()))
            }
            "FAIL" => Err(Error::sql(
                Some("MOCK1"),
                format!("mock failure for: {sql}"),
            )),
            _ => Ok(stream::iter([Ok(ResultItem::RowsAffected(0))]).boxed()),
        }
    }
}

struct RowsState {
    token: CancellationToken,
    config: MockConfig,
    schema: SchemaRef,
    /// 0 = schema not yet sent; n = batch n-1 is next.
    next: usize,
    done: bool,
}

fn rows_stream(token: CancellationToken, config: MockConfig) -> ResultStream {
    let state = RowsState {
        token,
        config,
        schema: MockDriver::schema(),
        next: 0,
        done: false,
    };
    stream::unfold(state, |mut st| async move {
        if st.done || st.next > st.config.batches {
            return None;
        }
        let cancelled = if st.config.batch_delay.is_zero() {
            st.token.is_cancelled()
        } else {
            tokio::select! {
                biased;
                () = st.token.cancelled() => true,
                () = tokio::time::sleep(st.config.batch_delay) => st.token.is_cancelled(),
            }
        };
        if cancelled {
            st.done = true;
            return Some((Err(Error::Cancelled), st));
        }
        if st.next == 0 {
            st.next = 1;
            let item = ResultItem::Schema(st.schema.clone());
            return Some((Ok(item), st));
        }
        let item = make_batch(&st.schema, st.next - 1, st.config.rows_per_batch);
        st.next += 1;
        if item.is_err() {
            st.done = true;
        }
        Some((item.map(ResultItem::Batch), st))
    })
    .boxed()
}

fn make_batch(schema: &SchemaRef, index: usize, rows: usize) -> Result<RecordBatch> {
    let start = index * rows;
    let ids: Vec<i64> = (start..start + rows)
        .map(|i| i64::try_from(i).map_err(|_| Error::internal("mock row id overflow")))
        .collect::<Result<_>>()?;
    let names: Vec<String> = ids.iter().map(|i| format!("row {i}")).collect();
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(ids)),
        Arc::new(StringArray::from(names)),
    ];
    RecordBatch::try_new(schema.clone(), columns).map_err(from_arrow)
}

/// A connection opened by [`MockDriver`].
pub struct MockConnection {
    inner: Arc<Inner>,
}

#[async_trait]
impl Connection for MockConnection {
    fn engine(&self) -> Engine {
        self.inner.engine
    }

    fn server_version(&self) -> &ServerVersion {
        &self.inner.version
    }

    async fn execute(
        &self,
        sql: &str,
        _params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        self.inner.execute(sql, &cancel)
    }

    async fn cancel_running(&self) -> Result<()> {
        if let Some(token) = lock(&self.inner.running).take() {
            token.cancel();
        }
        Ok(())
    }

    async fn begin(&self) -> Result<Box<dyn Tx>> {
        self.inner.check_open()?;
        self.inner.record("BEGIN");
        Ok(Box::new(MockTx {
            inner: self.inner.clone(),
            finished: false,
        }))
    }

    async fn close(&self) -> Result<()> {
        if let Some(token) = lock(&self.inner.running).take() {
            token.cancel();
        }
        self.inner.closed.store(true, Ordering::Release);
        Ok(())
    }
}

/// A transaction on a [`MockConnection`]. Dropping it unfinished logs `ROLLBACK`.
pub struct MockTx {
    inner: Arc<Inner>,
    finished: bool,
}

#[async_trait]
impl Tx for MockTx {
    async fn execute(
        &self,
        sql: &str,
        _params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        self.inner.execute(sql, &cancel)
    }

    async fn commit(mut self: Box<Self>) -> Result<()> {
        self.inner.check_open()?;
        self.finished = true;
        self.inner.record("COMMIT");
        Ok(())
    }

    async fn rollback(mut self: Box<Self>) -> Result<()> {
        self.inner.check_open()?;
        self.finished = true;
        self.inner.record("ROLLBACK");
        Ok(())
    }
}

impl Drop for MockTx {
    fn drop(&mut self) {
        if !self.finished {
            self.inner.record("ROLLBACK");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DriverRegistry, collect};

    fn slow(batches: usize) -> MockDriver {
        MockDriver::new(Engine::Postgres).with_config(MockConfig {
            batches,
            rows_per_batch: 10,
            batch_delay: Duration::from_millis(5),
        })
    }

    async fn connect(d: &MockDriver) -> Box<dyn Connection> {
        d.connect(&ConnectParams::network(d.engine(), "mock"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn streams_fixed_batches() {
        let d = MockDriver::new(Engine::Sqlite).with_version(ServerVersion::new(3, 45, 1));
        let c = d.connect(&ConnectParams::sqlite(":memory:")).await.unwrap();
        assert_eq!(c.engine(), Engine::Sqlite);
        assert!(c.server_version().at_least(3, 45));
        let s = c
            .execute("select * from t", &[], CancellationToken::new())
            .await
            .unwrap();
        let got = collect(s).await.unwrap();
        assert_eq!(got.result_sets.len(), 1);
        let set = &got.result_sets[0];
        assert_eq!(set.schema, MockDriver::schema());
        assert_eq!(set.batches.len(), 3);
        assert_eq!(set.num_rows(), 12);
        let last = set.batches[2]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(last.value(3), 11);

        let s = c
            .execute("UPDATE t SET x = 1", &[], CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(collect(s).await.unwrap().rows_affected, [0]);
        let err = c
            .execute("FAIL please", &[], CancellationToken::new())
            .await
            .err()
            .unwrap();
        assert_eq!(err.sql_code(), Some("MOCK1"));
        assert_eq!(
            d.log(),
            ["select * from t", "UPDATE t SET x = 1", "FAIL please"]
        );
    }

    /// Cancelling the caller's token mid-stream yields `Cancelled` and then ends the stream long
    /// before all batches are produced.
    #[tokio::test]
    async fn token_cancellation_stops_the_stream() {
        let d = slow(10_000);
        let c = connect(&d).await;
        let token = CancellationToken::new();
        let mut s = c.execute("SELECT 1", &[], token.clone()).await.unwrap();
        assert!(matches!(s.next().await, Some(Ok(ResultItem::Schema(_)))));
        assert!(matches!(s.next().await, Some(Ok(ResultItem::Batch(_)))));
        assert!(matches!(s.next().await, Some(Ok(ResultItem::Batch(_)))));

        token.cancel();
        let rest = tokio::time::timeout(Duration::from_secs(5), s.collect::<Vec<_>>())
            .await
            .expect("stream must end promptly after cancel");
        assert_eq!(rest.len(), 1, "exactly one Cancelled item, then end");
        assert!(matches!(&rest[0], Err(e) if e.is_cancelled()));
    }

    /// Cancelling while the stream is waiting for the next batch wakes it immediately.
    #[tokio::test]
    async fn cancel_from_another_task_interrupts_a_waiting_stream() {
        let d = MockDriver::new(Engine::Postgres).with_config(MockConfig {
            batches: 5,
            rows_per_batch: 1,
            batch_delay: Duration::from_secs(3600),
        });
        let c = connect(&d).await;
        let token = CancellationToken::new();
        // The delay applies before the schema too, so the stream is waiting from the start.
        let s = c.execute("SELECT 1", &[], token.clone()).await.unwrap();
        let canceller = tokio::spawn({
            let token = token.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                token.cancel();
            }
        });
        let rest = tokio::time::timeout(Duration::from_secs(5), s.collect::<Vec<_>>())
            .await
            .expect("must not wait for the hour-long batch delay");
        assert_eq!(rest.len(), 1, "exactly one Cancelled item, then end");
        assert!(matches!(&rest[0], Err(e) if e.is_cancelled()));
        canceller.await.unwrap();
    }

    #[tokio::test]
    async fn cancel_running_stops_the_current_statement_only() {
        let d = slow(10_000);
        let c = connect(&d).await;
        let token = CancellationToken::new();
        let mut s = c.execute("SELECT 1", &[], token.clone()).await.unwrap();
        assert!(matches!(s.next().await, Some(Ok(ResultItem::Schema(_)))));
        c.cancel_running().await.unwrap();
        let rest = tokio::time::timeout(Duration::from_secs(5), s.collect::<Vec<_>>())
            .await
            .unwrap();
        assert_eq!(rest.len(), 1);
        assert!(matches!(&rest[0], Err(e) if e.is_cancelled()));
        // The caller's token is untouched, so the connection is usable again.
        assert!(!token.is_cancelled());
        let s = c.execute("SELECT 2", &[], token).await.unwrap();
        // Nothing running now: cancel_running is a no-op.
        drop(s);
        c.cancel_running().await.unwrap();
    }

    #[tokio::test]
    async fn pre_cancelled_token_runs_nothing() {
        let d = MockDriver::new(Engine::MySql);
        let c = connect(&d).await;
        let token = CancellationToken::new();
        token.cancel();
        let err = c.execute("SELECT 1", &[], token).await.err().unwrap();
        assert!(err.is_cancelled());
        assert_eq!(d.log().len(), 0);
    }

    #[tokio::test]
    async fn transactions_and_close() {
        let d = MockDriver::new(Engine::MsSql);
        let c = connect(&d).await;
        let tx = c.begin().await.unwrap();
        let s = tx
            .execute(
                "INSERT INTO t VALUES (1)",
                &[1.into()],
                CancellationToken::new(),
            )
            .await
            .unwrap();
        collect(s).await.unwrap();
        tx.commit().await.unwrap();
        let tx = c.begin().await.unwrap();
        tx.rollback().await.unwrap();
        drop(c.begin().await.unwrap());
        assert_eq!(
            d.log(),
            [
                "BEGIN",
                "INSERT INTO t VALUES (1)",
                "COMMIT",
                "BEGIN",
                "ROLLBACK",
                "BEGIN",
                "ROLLBACK"
            ]
        );
        c.close().await.unwrap();
        c.close().await.unwrap();
        let err = c
            .execute("SELECT 1", &[], CancellationToken::new())
            .await
            .err()
            .unwrap();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::Connection);
    }

    #[tokio::test]
    async fn registry_dispatch() {
        let mut reg = DriverRegistry::new();
        let mysql = Arc::new(MockDriver::new(Engine::MySql).also_serves(&[Engine::MariaDb]));
        assert!(reg.register(mysql.clone()).is_none());
        reg.register(Arc::new(MockDriver::new(Engine::Postgres)));
        assert_eq!(reg.engines(), [Engine::Postgres, Engine::MySql]);
        assert!(reg.contains(Engine::MySql));

        let c = reg
            .connect(&ConnectParams::network(Engine::MySql, "h"))
            .await
            .unwrap();
        assert_eq!(c.engine(), Engine::MySql);

        let err = reg
            .connect(&ConnectParams::network(Engine::Redis, "h"))
            .await
            .err()
            .unwrap();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::Unsupported);

        // Invalid params are rejected before reaching the driver.
        let err = reg
            .connect(&ConnectParams::new(Engine::MySql))
            .await
            .err()
            .unwrap();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);

        let denied = ConnectParams::network(Engine::Postgres, "h").with_user("denied");
        let err = reg.connect(&denied).await.err().unwrap();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::Auth);

        // The MySQL driver can be registered for MariaDB too, and serves MariaDB connections.
        reg.register_as(Engine::MariaDb, mysql.clone());
        assert!(reg.contains(Engine::MariaDb));
        assert!(format!("{reg:?}").contains("MariaDb"));
        let c = reg
            .connect(&ConnectParams::network(Engine::MariaDb, "h"))
            .await
            .unwrap();
        assert_eq!(c.engine(), Engine::MariaDb);
        assert_eq!(mysql.engine(), Engine::MySql);

        // A driver registered for an engine it does not serve still refuses it.
        reg.register_as(Engine::MsSql, mysql);
        let err = reg
            .connect(&ConnectParams::network(Engine::MsSql, "h"))
            .await
            .err()
            .unwrap();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);
    }
}
