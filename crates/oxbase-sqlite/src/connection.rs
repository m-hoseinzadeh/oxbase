//! [`SqliteConnection`] and [`SqliteTx`]: the async side of the worker.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc as std_mpsc};

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream;
use oxbase_driver::{CancellationToken, Connection, ResultItem, ResultStream, Tx};
use oxbase_types::{Engine, Error, Result, ServerVersion, Value};
use tokio::sync::{mpsc, oneshot};

use crate::options::SqliteOptions;
use crate::worker::{self, Command, Job, RunState};

/// Result items buffered between the worker and the consumer. Small on purpose: batches are
/// large, and a slow consumer should hold the worker back rather than let rows pile up.
const CHANNEL_CAPACITY: usize = 4;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn closed() -> Error {
    Error::connection("the SQLite connection is closed")
}

/// Shared by a connection and its transactions.
struct Shared {
    /// `None` once closed.
    commands: Mutex<Option<std_mpsc::Sender<Command>>>,
    state: Arc<RunState>,
    next_id: AtomicU64,
}

impl Shared {
    fn send(&self, cmd: Command) -> Result<()> {
        match lock(&self.commands).as_ref() {
            Some(tx) => tx.send(cmd).map_err(|_| closed()),
            None => Err(closed()),
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: &CancellationToken,
        tx: Option<u64>,
    ) -> Result<ResultStream> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let id = self.next_id();
        let token = cancel.child_token();
        let done = CancellationToken::new();
        let (out, rx) = mpsc::channel(CHANNEL_CAPACITY);
        self.send(Command::Execute(Job {
            id,
            sql: sql.to_owned(),
            params: params.to_vec(),
            tx,
            token: token.clone(),
            out,
            done: done.clone(),
        }))?;
        // Interrupts SQLite as soon as the token is cancelled, even while nobody polls the
        // stream. (Without a runtime the stream and the progress handler still cancel.)
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let (state, token, done) = (self.state.clone(), token.clone(), done.clone());
            rt.spawn(async move {
                tokio::select! {
                    () = token.cancelled() => state.interrupt_if_running(id),
                    () = done.cancelled() => {}
                }
            });
        }
        Ok(result_stream(
            rx,
            ExecGuard {
                token,
                state: self.state.clone(),
                id,
            },
            done,
        ))
    }

    async fn request(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<()>>) -> Command,
    ) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.send(make(reply))?;
        rx.await.map_err(|_| closed())?
    }
}

/// Cancels its execution when the stream is dropped (the worker stops at the next row, or at
/// once through the interrupt).
struct ExecGuard {
    token: CancellationToken,
    state: Arc<RunState>,
    id: u64,
}

impl Drop for ExecGuard {
    fn drop(&mut self) {
        self.token.cancel();
        self.state.interrupt_if_running(self.id);
    }
}

struct StreamState {
    rx: mpsc::Receiver<Result<ResultItem>>,
    guard: ExecGuard,
    /// Cancelled by the worker once the execution is over, before it drops its sender. A channel
    /// that ends without it means the worker stopped (a panic, or its thread exited) while the
    /// statement was queued or running.
    job_done: CancellationToken,
    done: bool,
}

/// Yields the worker's items; the first error (or a cancel of the token) ends the stream.
///
/// Once the stream has ended with an error it closes its receiver, so a worker still trying to
/// send to it gives up even while the caller keeps the stream around.
fn result_stream(
    rx: mpsc::Receiver<Result<ResultItem>>,
    guard: ExecGuard,
    job_done: CancellationToken,
) -> ResultStream {
    let st = StreamState {
        rx,
        guard,
        job_done,
        done: false,
    };
    stream::unfold(st, |mut st| async move {
        if st.done {
            return None;
        }
        let next = tokio::select! {
            biased;
            () = st.guard.token.cancelled() => None,
            item = st.rx.recv() => Some(item),
        };
        match next {
            None => {
                st.done = true;
                st.rx.close();
                st.guard.state.interrupt_if_running(st.guard.id);
                Some((Err(Error::Cancelled), st))
            }
            Some(None) if !st.job_done.is_cancelled() => {
                st.done = true;
                Some((
                    Err(Error::connection(
                        "the SQLite worker stopped before the statement finished",
                    )),
                    st,
                ))
            }
            Some(None) => None,
            Some(Some(Ok(item))) => Some((Ok(item), st)),
            Some(Some(Err(e))) => {
                st.done = true;
                st.rx.close();
                Some((Err(e), st))
            }
        }
    })
    .boxed()
}

/// An open SQLite database.
///
/// Statements run one at a time on the connection's own worker thread, in the order they were
/// submitted: an `execute` issued while an earlier stream is still being read waits for it (so
/// drain or drop each stream). `execute` accepts scripts of several statements.
///
/// Statements run through [`Connection::execute`] while a [`SqliteTx`] is open run inside that
/// transaction (there is only one SQLite connection).
pub struct SqliteConnection {
    shared: Arc<Shared>,
    version: ServerVersion,
}

impl fmt::Debug for SqliteConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteConnection")
            .field("version", &self.version)
            .field("open", &lock(&self.shared.commands).is_some())
            .finish_non_exhaustive()
    }
}

impl SqliteConnection {
    /// Opens the database described by `opts` on a new worker thread.
    pub async fn open(opts: SqliteOptions) -> Result<Self> {
        let opened = worker::spawn(opts).await?;
        Ok(Self {
            shared: Arc::new(Shared {
                commands: Mutex::new(Some(opened.commands)),
                state: opened.state,
                next_id: AtomicU64::new(1),
            }),
            version: opened.version,
        })
    }
}

#[async_trait]
impl Connection for SqliteConnection {
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }

    /// The SQLite library version (`sqlite_version()`).
    fn server_version(&self) -> &ServerVersion {
        &self.version
    }

    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        self.shared.execute(sql, params, &cancel, None)
    }

    async fn cancel_running(&self) -> Result<()> {
        self.shared.state.cancel_running();
        Ok(())
    }

    /// `BEGIN` (deferred). Fails if a transaction is already open on this connection.
    async fn begin(&self) -> Result<Box<dyn Tx>> {
        let tx = self.shared.next_id();
        self.shared
            .request(|reply| Command::Begin { tx, reply })
            .await?;
        Ok(Box::new(SqliteTx {
            shared: self.shared.clone(),
            id: tx,
            finished: false,
        }))
    }

    /// Cancels the running statement, fails statements still queued, then closes the database
    /// (rolling back an open transaction). Returns once the file is closed.
    async fn close(&self) -> Result<()> {
        let Some(sender) = lock(&self.shared.commands).take() else {
            return Ok(());
        };
        self.shared.state.set_closing();
        self.shared.state.cancel_running();
        let (reply, rx) = oneshot::channel();
        if sender.send(Command::Close { reply }).is_ok() {
            drop(sender);
            let _ = rx.await;
        }
        Ok(())
    }
}

/// A transaction on a [`SqliteConnection`].
///
/// Dropping it without [`Tx::commit`] or [`Tx::rollback`] rolls it back. A commit that fails
/// (for example with `SQLITE_BUSY`) leaves the transaction to be rolled back on drop. If a
/// statement ends the transaction (`COMMIT` / `ROLLBACK` in SQL) or SQLite rolls it back by
/// itself (a cancelled write), later calls on the `SqliteTx` fail instead of running outside it.
pub struct SqliteTx {
    shared: Arc<Shared>,
    id: u64,
    finished: bool,
}

impl fmt::Debug for SqliteTx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteTx")
            .field("id", &self.id)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Tx for SqliteTx {
    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        self.shared.execute(sql, params, &cancel, Some(self.id))
    }

    async fn commit(mut self: Box<Self>) -> Result<()> {
        let tx = self.id;
        self.shared
            .request(|reply| Command::End {
                tx,
                commit: true,
                reply,
            })
            .await?;
        self.finished = true;
        Ok(())
    }

    async fn rollback(mut self: Box<Self>) -> Result<()> {
        let tx = self.id;
        self.shared
            .request(|reply| Command::End {
                tx,
                commit: false,
                reply,
            })
            .await?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for SqliteTx {
    fn drop(&mut self) {
        if !self.finished {
            // Fails only when the connection is closed, which rolls back anyway.
            let _ = self.shared.send(Command::DropTx { tx: self.id });
        }
    }
}
