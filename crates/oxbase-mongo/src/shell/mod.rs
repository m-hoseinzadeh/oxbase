//! The mongo-shell subset (shell stream): the editor language ([`MongoShell`], registered as
//! MongoDB's `ScriptLanguage`) and the sandboxed JavaScript session that evaluates statements
//! ([`ShellSession`], one per [`MongoConnection`]). Contract: `docs/m7-contracts.md` §4.2.
//!
//! # The subset
//!
//! Statements are JavaScript (QuickJS through `rquickjs`) with these globals, and nothing else
//! of a host environment:
//!
//! | Global | Supported |
//! |---|---|
//! | `db` | the current database: `db.<coll>` / `db.getCollection(name)`, `db.getName()`, `db.getSiblingDB(name)`, `db.getCollectionNames()`, `db.getCollectionInfos(filter?)`, `db.runCommand(doc)`, `db.adminCommand(doc)`, `db.createCollection(name, options?)`, `db.createView(name, source, pipeline)`, `db.dropDatabase()`, `db.stats()`, `db.version()` |
//! | collection | `find(filter?, projection?)` → cursor, `findOne(filter?, projection?)`, `aggregate(pipeline, options?)` → cursor, `insertOne`, `insertMany`, `updateOne`, `updateMany`, `replaceOne`, `deleteOne`, `deleteMany`, `countDocuments(filter?)`, `estimatedDocumentCount()`, `distinct(field, filter?)`, `createIndex(keys, options?)`, `getIndexes()`, `dropIndex(name)`, `drop()`, `renameCollection(name)` |
//! | cursor | `sort`, `limit`, `skip`, `project`, `hint`, `batchSize`, `maxTimeMS`, `toArray()`, `count()` (= `countDocuments` of its filter), `next()`, `hasNext()`, `forEach(fn)`, `map(fn)` |
//! | helpers | `ObjectId(hex?)`, `ISODate(text?)` / `new Date(...)`, `NumberInt(n)`, `NumberLong(n or text)`, `NumberDecimal(text)`, `UUID(text?)`, `BinData(subtype, base64)`, `Timestamp(t, i)`, `MinKey()`, `MaxKey()`, `RegExp` literals; `print(...)` / `printjson(x)` / `console.log(...)` (notices); `EJSON.stringify` / `EJSON.parse`; `sleep(ms)` (at most 10 s, waiting time) |
//! | commands | `use <db>`, `show dbs` / `show databases`, `show collections` / `show tables`, `show users`, `show roles` (`it` is not offered: results are pageable instead) |
//!
//! Anything else (`load`, `require`, `fetch`, `process`, `fs`, `quit`, `sleep` beyond 10 s,
//! `Mongo()`, `connect()`, sessions, change streams, `$where` helpers) is a `ReferenceError` or
//! an `InvalidInput` naming the unsupported helper. The docs page lists exactly this subset.
//!
//! # Values
//!
//! JavaScript values become BSON with the helpers' types (`ObjectId(...)` is an ObjectId,
//! `NumberLong` an int64, integers that fit an int32 are int32 like mongosh, other numbers
//! doubles, `Date` a date); BSON returned by the server becomes the same helper objects, so
//! `db.c.findOne()._id` can be passed back. Plain objects whose only key is an Extended JSON
//! wrapper (`{$oid: ...}`, `{$date: ...}`, `{$numberDecimal: ...}`) are **not** converted:
//! in the shell the helpers are the way to write typed values (as in mongosh).
//!
//! # Results
//!
//! The value of the last expression of a statement is its result:
//!
//! - a cursor (`find`, `aggregate`) that the script has not read streams as a document result
//!   set (columns inferred, plus `$document`; `oxbase_dialect::document`) without being
//!   materialized in JavaScript: it becomes a find / aggregate request that the driver runs
//!   (the statement's run then cancels, pages and limits it like any request). A cursor the
//!   script already read from, or one a request cannot express (`db.aggregate(...)`, an
//!   aggregate `hint`), is read to its end into a document result;
//! - a document or an array of documents: the same shape, from the values;
//! - write results (`insertOne`, `updateMany`, ...): `RowsAffected` (inserted / modified /
//!   deleted count) plus a one-row result with the acknowledged fields (`insertedId`,
//!   `matchedCount`, `modifiedCount`, `upsertedId`, `deletedCount`);
//! - `show ...`: one result set (`name` column, and `sizeOnDisk` / `empty` for `show dbs`);
//! - any other value: one row, column `value` (relaxed Extended JSON text, `arrow.json`);
//! - `undefined` (declarations, assignments, loops): no result set;
//! - `use x`: a notice `switched to db x`.
//!
//! `print` output comes first, as info notices, in order. A failing statement still reports
//! what it printed before the error.
//!
//! # Sandbox
//!
//! - **No host access**: the runtime is a bare QuickJS context (no `std` / `os` modules, no
//!   module loader, no `eval` of files); the only bridge to Rust is the `db` API above.
//! - **Limits** ([`ShellLimits`]): `Runtime::set_memory_limit` (default 64 MiB),
//!   `set_max_stack_size` (1 MiB), and an interrupt handler that stops the script when the
//!   statement's cancellation token fires or when it has run longer than the CPU time limit
//!   (default 10 s; time spent waiting for the server does not count). An interrupted script
//!   ends the statement with `Error::Cancelled` (token) or `Error::Timeout` (limit); the
//!   server operation it was waiting for is cancelled as in the driver docs.
//! - **Iteration limit**: a script reads at most 100 000 documents into JavaScript per
//!   statement (`toArray`, `forEach`, `next`, ...); a cursor *returned* as the statement's
//!   value streams every document without that limit. `print` output is kept up to 1000 lines
//!   or 1 MiB per statement.
//! - **Errors** are deterministic: a JavaScript error ends the statement with
//!   `Error::Sql { code: <error name, or the server's codeName>, message: "<message> (line
//!   <n>)" }`, the line counted within the statement; server errors the script did not catch
//!   keep their category (`Cancelled`, `Timeout`, `Connection`, `Sql` with the server's code).
//!   Running out of memory is `Sql { code: "OutOfMemory" }` and resets the session (its
//!   globals are gone).
//! - **Threading**: QuickJS runtimes are not `Send`. Each [`ShellSession`] owns one dedicated
//!   OS thread holding its runtime and context (started on first use, stopped when the session
//!   drops); statements are sent to it over a channel; its host function spawns each server
//!   call on the statement's tokio runtime and waits for the answer (it is not a runtime
//!   thread, so it may block). Globals (`var x = ...`) survive between statements of the same
//!   editor session.
//!
//! # Splitting ([`MongoShell::split`])
//!
//! A statement ends at a top-level `;` or at a newline where the text so far is complete
//! (brackets balanced, not inside a string, template literal or comment, the line does not
//! end with an operator, `.`, `,` or `=>`, and the next non-blank line does not start with
//! `.`, `)`, `]`, `}`, `?`, `:`, a binary operator, `else`, `catch` or `finally`), and the
//! statement is not a control head waiting for its body (`if (...)`, `for (...)`, a function
//! head, `else`). `use x`, `show x` and `it` lines are statements of their own. Text whose first non-blank character is `{` is one request
//! statement (the whole text; see the driver docs). Comments (`//`, `/* */`) before a
//! statement are not part of it.
//!
//! # Redaction and guards
//!
//! `redact_secrets` replaces the value of `pwd` / `password` keys in object literals
//! (`createUser`, `updateUser`, `db.auth`, `changeUserPassword`'s second argument) and
//! passwords in `mongodb://user:pass@` strings with `***`. `guard` asks for confirmation for
//! `dropDatabase()`, `.drop()`, `dropIndex`, `deleteMany({})` / `deleteMany()` with an empty
//! filter, `updateMany({}, ...)` and `renameCollection(..., true)`; it blocks nothing.

use std::borrow::Cow;
use std::sync::Mutex;
use std::time::Duration;

use futures::StreamExt;
use oxbase_dialect::{ScriptLanguage, StatementGuard};
use oxbase_driver::{CancellationToken, Connection, ResultItem, ResultStream};
use oxbase_sql::Statement;
use oxbase_types::{
    AggregateRequest, DocumentQuery, Error, FindRequest, ObjectKind, ObjectRef, Result,
};
use serde_json::Value as Json;

use crate::MongoConnection;
use crate::options::MongoOptions;

mod commands;
mod host;
mod lang;
mod lex;
mod rows;
mod session;
mod split;

use self::host::{StatementEnv, to_bson, to_document};
use self::session::{Job, Outcome, Worker};

/// Default stack limit of the JavaScript runtime.
pub const DEFAULT_STACK_BYTES: usize = 1024 * 1024;

/// How long a cancelled statement waits for the session thread to wind down.
const CANCEL_GRACE: Duration = Duration::from_millis(400);

/// The mongo shell as an editor language. See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct MongoShell;

impl ScriptLanguage for MongoShell {
    fn name(&self) -> &'static str {
        "mongo-shell"
    }

    fn editor_language(&self) -> &'static str {
        "javascript"
    }

    fn split(&self, text: &str) -> Vec<Statement> {
        split::split(text)
    }

    fn redact_secrets<'a>(&self, text: &'a str) -> Cow<'a, str> {
        lang::redact(text)
    }

    fn guard(&self, statement: &Statement) -> StatementGuard {
        lang::guard(&statement.text)
    }
}

/// Resource limits of one shell session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellLimits {
    /// `Runtime::set_memory_limit`.
    pub memory_bytes: usize,
    /// `Runtime::set_max_stack_size`.
    pub stack_bytes: usize,
    /// CPU time one statement's JavaScript may run; `None` = no limit.
    pub time_limit: Option<Duration>,
}

impl ShellLimits {
    /// The limits a profile's options ask for (`shell_memory_mb`, `shell_time_limit_ms`).
    pub fn from_options(options: &MongoOptions) -> Self {
        let memory = usize::try_from(options.shell_memory_mb).unwrap_or(usize::MAX);
        Self {
            memory_bytes: memory.saturating_mul(1024 * 1024),
            stack_bytes: DEFAULT_STACK_BYTES,
            time_limit: (options.shell_time_limit_ms > 0)
                .then(|| Duration::from_millis(options.shell_time_limit_ms)),
        }
    }
}

/// The JavaScript session of one [`MongoConnection`] (see the module docs, *Sandbox*). Cheap
/// to create: the runtime thread starts on the first [`ShellSession::run`].
pub struct ShellSession {
    limits: ShellLimits,
    worker: Mutex<Option<Worker>>,
}

impl ShellSession {
    pub fn new(limits: ShellLimits) -> Self {
        Self {
            limits,
            worker: Mutex::new(None),
        }
    }

    /// The limits every statement runs under.
    pub fn limits(&self) -> ShellLimits {
        self.limits
    }

    /// Evaluates one shell statement against `connection` (its client, current database and
    /// operation comments) and streams its result (see the module docs, *Results*). `use x`
    /// changes `connection`'s current database.
    pub async fn run(
        &self,
        connection: &MongoConnection,
        statement: &str,
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if statement.trim().is_empty() {
            return Ok(futures::stream::empty().boxed());
        }
        if let Some((word, argument)) = split::command_line(statement) {
            return commands::run(connection, word, argument, &cancel).await;
        }
        let comment = connection.next_comment();
        let database = connection
            .current_database()
            .unwrap_or_else(|| "test".to_owned());
        let env = StatementEnv {
            client: connection.client().clone(),
            comment: comment.clone(),
            handle: tokio::runtime::Handle::current(),
            cancel: cancel.clone(),
        };
        let (reply, mut outcome) = tokio::sync::oneshot::channel();
        self.send(Job {
            source: statement.to_owned(),
            database,
            env,
            reply,
        })?;
        let outcome = tokio::select! {
            biased;
            r = &mut outcome => r.map_err(|_| Error::internal("the shell thread stopped"))?,
            () = cancel.cancelled() => {
                // The session thread sees the token too (interrupt handler, host waits); stop
                // what the statement started on the server, then let the thread wind down.
                let _ = connection.kill_operation(&comment).await;
                let _ = tokio::time::timeout(CANCEL_GRACE, &mut outcome).await;
                return Err(Error::Cancelled);
            }
        };
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if let Some(database) = &outcome.switched {
            connection.set_current_database(database);
        }
        stream(connection, outcome, cancel).await
    }

    /// Sends `job` to the session thread, starting it (again) when needed.
    fn send(&self, job: Job) -> Result<()> {
        let mut worker = self
            .worker
            .lock()
            .map_err(|_| Error::internal("the shell session is poisoned"))?;
        let job = match worker.as_ref() {
            Some(w) => match w.send(job) {
                Ok(()) => return Ok(()),
                Err(job) => job,
            },
            None => job,
        };
        let fresh = Worker::start(self.limits)?;
        fresh
            .send(job)
            .map_err(|_| Error::internal("the shell thread did not start"))?;
        *worker = Some(fresh);
        Ok(())
    }
}

/// What a statement's value turned into (see the module docs, *Results*).
#[derive(Debug)]
enum Output {
    None,
    Request(DocumentQuery),
    Documents(Vec<bson::Document>),
    Value(bson::Bson),
    Write { affected: u64, row: bson::Document },
}

/// The result stream of a finished statement.
async fn stream(
    connection: &MongoConnection,
    outcome: Outcome,
    cancel: CancellationToken,
) -> Result<ResultStream> {
    let mut items: Vec<Result<ResultItem>> = outcome
        .notices
        .into_iter()
        .map(|n| Ok(ResultItem::Notice(n)))
        .collect();
    let output = match outcome
        .result
        .and_then(|described| parse_output(&described))
    {
        Ok(output) => output,
        Err(Error::Cancelled) => return Err(Error::Cancelled),
        Err(e) if items.is_empty() => return Err(e),
        Err(e) => {
            items.push(Err(e));
            return Ok(futures::stream::iter(items).boxed());
        }
    };
    let set = match output {
        Output::None => None,
        Output::Request(query) => {
            let text = serde_json::to_string(&query)
                .map_err(|e| Error::internal(format!("shell request: {e}")))?;
            return match Connection::execute(connection, &text, &[], cancel).await {
                Ok(rest) => Ok(futures::stream::iter(items).chain(rest).boxed()),
                Err(e) if items.is_empty() => Err(e),
                Err(e) => {
                    items.push(Err(e));
                    Ok(futures::stream::iter(items).boxed())
                }
            };
        }
        Output::Documents(docs) => Some(rows::documents(&docs)?),
        Output::Value(value) => Some(rows::value(&value)?),
        Output::Write { affected, row } => {
            items.push(Ok(ResultItem::RowsAffected(affected)));
            Some(rows::documents(std::slice::from_ref(&row))?)
        }
    };
    if let Some((schema, batch)) = set {
        items.push(Ok(ResultItem::Schema(schema)));
        items.push(Ok(ResultItem::Batch(batch)));
    }
    Ok(futures::stream::iter(items).boxed())
}

/// The prelude's description of a statement's value (`__oxbase_describe`).
fn parse_output(text: &str) -> Result<Output> {
    let value: Json =
        serde_json::from_str(text).map_err(|e| Error::internal(format!("shell result: {e}")))?;
    let kind = value.get("kind").and_then(Json::as_str).unwrap_or("none");
    Ok(match kind {
        "request" => Output::Request(request(value.get("request").unwrap_or(&Json::Null))?),
        "documents" => {
            let docs = value
                .get("docs")
                .and_then(Json::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            Output::Documents(
                docs.iter()
                    .map(|d| to_document(d, "document"))
                    .collect::<Result<Vec<_>>>()?,
            )
        }
        "value" => Output::Value(to_bson(value.get("value").unwrap_or(&Json::Null), "value")?),
        "write" => Output::Write {
            affected: count(value.get("affected")).unwrap_or(0),
            row: to_document(value.get("row").unwrap_or(&Json::Null), "write result")?,
        },
        _ => Output::None,
    })
}

/// A non-negative whole JSON number.
fn count(value: Option<&Json>) -> Option<u64> {
    let n = value?.as_f64()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked: finite and non-negative; counts stay far below 2^53"
    )]
    let whole = n.min(9.0e15) as u64;
    Some(whole)
}

/// The find / aggregate request of an unread cursor.
fn request(r: &Json) -> Result<DocumentQuery> {
    let text = |key: &str| {
        r.get(key)
            .and_then(Json::as_str)
            .map(str::to_owned)
            .ok_or_else(|| Error::internal(format!("shell request without {key}")))
    };
    let mut collection = ObjectRef::new(ObjectKind::Table, text("coll")?);
    collection.catalog = Some(text("db")?);
    let part = |key: &str| r.get(key).filter(|v| !v.is_null()).cloned();
    let max_time_ms = count(r.get("maxTimeMS")).filter(|n| *n > 0);
    match text("kind")?.as_str() {
        "find" => {
            let mut f = FindRequest::new(collection);
            f.filter = part("filter");
            f.projection = part("projection");
            f.sort = part("sort");
            f.skip = count(r.get("skip")).filter(|n| *n > 0);
            f.limit = count(r.get("limit")).filter(|n| *n > 0);
            f.hint = part("hint");
            f.collation = part("collation");
            f.max_time_ms = max_time_ms;
            Ok(DocumentQuery::Find(f))
        }
        "aggregate" => {
            let pipeline = r
                .get("pipeline")
                .and_then(Json::as_array)
                .cloned()
                .unwrap_or_default();
            let mut a = AggregateRequest::new(collection, pipeline);
            a.allow_disk_use = r
                .get("allowDiskUse")
                .and_then(Json::as_bool)
                .unwrap_or(false);
            a.collation = part("collation");
            a.max_time_ms = max_time_ms;
            Ok(DocumentQuery::Aggregate(a))
        }
        other => Err(Error::internal(format!("shell request of kind {other}"))),
    }
}

#[cfg(test)]
mod tests;
