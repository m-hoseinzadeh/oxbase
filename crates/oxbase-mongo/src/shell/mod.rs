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
//! | helpers | `ObjectId(hex?)`, `ISODate(text?)` / `new Date(...)`, `NumberInt(n)`, `NumberLong(n or text)`, `NumberDecimal(text)`, `UUID(text?)`, `BinData(subtype, base64)`, `Timestamp(t, i)`, `MinKey()`, `MaxKey()`, `RegExp` literals; `print(...)` / `printjson(x)` (notices); `EJSON.stringify` / `EJSON.parse` |
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
//! - a cursor (`find`, `aggregate`) streams as a document result set (columns inferred, plus
//!   `$document`; `oxbase_dialect::document`) without being materialized in JavaScript;
//! - a document or an array of documents: the same shape, from the values;
//! - write results (`insertOne`, `updateMany`, ...): `RowsAffected` (inserted / modified /
//!   deleted count) plus a one-row result with the acknowledged fields (`insertedId`,
//!   `matchedCount`, `modifiedCount`, `upsertedId`, `deletedCount`);
//! - `show ...`: one result set (`name` column, and `sizeOnDisk` / `empty` for `show dbs`);
//! - any other value: one row, column `value` (relaxed Extended JSON text, `arrow.json`);
//! - `undefined` (declarations, assignments, loops): no result set.
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
//! - **Threading**: QuickJS runtimes are not `Send`. Each [`ShellSession`] owns one dedicated
//!   OS thread holding its runtime and context (started on first use, stopped when the session
//!   drops); statements are sent to it over a channel; its host functions wait for the async
//!   driver with `tokio::runtime::Handle::block_on` (legal there: it is not a runtime thread).
//!   Globals (`var x = ...`) survive between statements of the same editor session.
//!
//! # Splitting ([`MongoShell::split`])
//!
//! A statement ends at a top-level `;` or at a newline where the text so far is complete
//! (brackets balanced, not inside a string, template literal or comment, the line does not
//! end with an operator, `.`, `,` or `=>`, and the next non-blank line does not start with
//! `.`, `)`, `]`, `}`, `?`, `:` or a binary operator). `use x`, `show x` and `it` lines are
//! statements of their own. Text whose first non-blank character is `{` is one request
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
use std::time::Duration;

use oxbase_dialect::{ScriptLanguage, StatementGuard};
use oxbase_driver::{CancellationToken, ResultStream};
use oxbase_sql::Statement;
use oxbase_types::Result;

use crate::MongoConnection;
use crate::options::MongoOptions;

/// Default stack limit of the JavaScript runtime.
pub const DEFAULT_STACK_BYTES: usize = 1024 * 1024;

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
        let _ = text;
        unimplemented!("m7: shell")
    }

    fn redact_secrets<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let _ = text;
        unimplemented!("m7: shell")
    }

    fn guard(&self, statement: &Statement) -> StatementGuard {
        let _ = statement;
        unimplemented!("m7: shell")
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
}

impl ShellSession {
    pub fn new(limits: ShellLimits) -> Self {
        Self { limits }
    }

    /// The limits every statement runs under.
    pub fn limits(&self) -> ShellLimits {
        self.limits
    }

    /// Evaluates one shell statement against `connection` (its client, current database and
    /// operation comments) and streams its result (see the module docs, *Results*). `use x`
    /// changes `connection`'s current database.
    #[allow(
        clippy::unused_async,
        reason = "m7 skeleton: the shell stream fills the body"
    )]
    pub async fn run(
        &self,
        connection: &MongoConnection,
        statement: &str,
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        let _ = (self, connection, statement, cancel);
        unimplemented!("m7: shell")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_driver::ConnectParams;
    use oxbase_types::Engine;

    #[test]
    fn limits_follow_the_profile_options() {
        let mut params = ConnectParams::network(Engine::MongoDb, "127.0.0.1");
        params
            .options
            .insert(crate::options::SHELL_TIME_LIMIT_MS.into(), "0".into());
        let options = MongoOptions::from_params(&params).unwrap();
        let limits = ShellLimits::from_options(&options);
        assert_eq!(limits.memory_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.stack_bytes, DEFAULT_STACK_BYTES);
        assert_eq!(limits.time_limit, None);
        let session = ShellSession::new(limits);
        assert_eq!(session.limits(), limits);
        assert_eq!(MongoShell.editor_language(), "javascript");
    }
}
