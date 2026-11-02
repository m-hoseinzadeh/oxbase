//! The one bridge between the sandboxed JavaScript and Rust: `host(op, payload)`, called by
//! the prelude with a JSON payload and answering JSON (`{"ok": value}` or `{"error": {...}}`).
//! Documents cross it as canonical Extended JSON. Operations:
//!
//! | op | payload | answer |
//! |---|---|---|
//! | `command` | `{db, cmd}` | the reply document (`runCommand` with the statement's comment) |
//! | `find` | `{db, coll, filter, projection?, sort?, skip?, limit?, hint?, collation?, maxTimeMS?, batchSize?}` | `{id}` of a server cursor |
//! | `aggregate` | `{db, coll (null: database-level), pipeline, allowDiskUse?, collation?, maxTimeMS?, batchSize?, hint?}` | `{id}` |
//! | `next` | `{id, max}` | `{docs, done}` (at most `max` ≤ 1000 documents) |
//! | `close` | `{id}` | `null` |
//! | `print` | `{text}` | `null` (an info notice of the statement) |
//! | `use` | `{db}` | `null` (`db = db.getSiblingDB(x)`: the current database after the statement) |
//! | `oid`, `uuid` | `null` | a new ObjectId / UUID as hex |
//! | `sleep` | `{ms}` | `null` (at most 10 s; waiting does not count as CPU time) |
//!
//! Every server call runs on the tokio runtime of the statement (spawned, so the runtime's
//! own threads drive it) while the JavaScript thread waits; the statement's token stops the
//! wait at once. Errors keep their oxbase category: the prelude throws them as JavaScript
//! errors carrying `oxbaseErrorId`, and an uncaught one ends the statement with the original
//! error.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use bson::{Bson, Document};
use futures::StreamExt;
use mongodb::Cursor;
use mongodb::error::{ErrorKind, WriteFailure};
use mongodb::options::{AggregateOptions, Collation, FindOptions, Hint};
use oxbase_driver::{CancellationToken, Notice};
use oxbase_types::{Error, Result};
use serde_json::{Map, Value as Json, json};

use super::session::Watch;

/// Documents one statement may read into JavaScript (`toArray`, `forEach`, `next`); a cursor
/// returned as the result streams without this limit.
pub(super) const MAX_SCRIPT_DOCUMENTS: usize = 100_000;
/// Output lines (`print`, `printjson`) kept per statement, and their total size.
const MAX_NOTICES: usize = 1000;
const MAX_NOTICE_BYTES: usize = 1024 * 1024;
/// Longest `sleep`.
const MAX_SLEEP: Duration = Duration::from_secs(10);
/// Documents per `next` call.
const MAX_BATCH: usize = 1000;

/// Keys that make an object an Extended JSON value wrapper rather than a document.
const WRAPPER_KEYS: [&str; 17] = [
    "$oid",
    "$symbol",
    "$numberInt",
    "$numberLong",
    "$numberDouble",
    "$numberDecimal",
    "$binary",
    "$uuid",
    "$code",
    "$timestamp",
    "$regularExpression",
    "$dbPointer",
    "$date",
    "$minKey",
    "$maxKey",
    "$undefined",
    "$scope",
];

/// MongoDB error codes with their own category.
const AUTHENTICATION_FAILED: i32 = 18;
const MAX_TIME_MS_EXPIRED: i32 = 50;
const EXCEEDED_TIME_LIMIT: i32 = 262;
const INTERRUPTED: i32 = 11601;
const NAMESPACE_NOT_FOUND: i32 = 26;

/// What the host needs from the statement that is running.
#[derive(Clone)]
pub(super) struct StatementEnv {
    pub(super) client: mongodb::Client,
    pub(super) comment: String,
    pub(super) handle: tokio::runtime::Handle,
    pub(super) cancel: CancellationToken,
}

/// Host state of the session thread; per statement except the cursor counter.
#[derive(Default)]
pub(super) struct State {
    statement: Option<StatementEnv>,
    notices: Vec<Notice>,
    notice_bytes: usize,
    notices_dropped: bool,
    /// Errors the host returned during the statement, by `oxbaseErrorId`.
    errors: Vec<Error>,
    cursors: HashMap<u64, Cursor<Document>>,
    next_cursor: u64,
    fetched: usize,
    switched: Option<String>,
}

/// What a statement left behind in the host state.
pub(super) struct Leftovers {
    pub(super) notices: Vec<Notice>,
    pub(super) switched: Option<String>,
}

impl State {
    pub(super) fn begin(&mut self, env: StatementEnv) {
        self.statement = Some(env);
        self.notices.clear();
        self.notice_bytes = 0;
        self.notices_dropped = false;
        self.errors.clear();
        self.fetched = 0;
        self.switched = None;
    }

    /// Ends the statement: closes its cursors (on the runtime: dropping a cursor sends
    /// `killCursors` from a task) and hands back its notices and database switch.
    pub(super) fn end(&mut self) -> Leftovers {
        if let Some(env) = self.statement.take() {
            let cursors: Vec<Cursor<Document>> = self.cursors.drain().map(|(_, c)| c).collect();
            // Both are dropped on the runtime: an open cursor sends killCursors from a task, and
            // a client whose last handle drops ends its sessions from one.
            let handle = env.handle.clone();
            handle.spawn(async move {
                drop(cursors);
                drop(env);
            });
        }
        Leftovers {
            notices: std::mem::take(&mut self.notices),
            switched: self.switched.take(),
        }
    }

    /// The error the host returned as `oxbaseErrorId` `id` during this statement.
    pub(super) fn error(&self, id: usize) -> Option<&Error> {
        self.errors.get(id)
    }

    fn print(&mut self, text: &str) {
        if self.notices_dropped {
            return;
        }
        if self.notices.len() >= MAX_NOTICES || self.notice_bytes + text.len() > MAX_NOTICE_BYTES {
            self.notices_dropped = true;
            self.notices.push(Notice::warning(format!(
                "the statement printed more than {MAX_NOTICES} lines or 1 MiB; the rest of its \
                 output was dropped"
            )));
            return;
        }
        self.notice_bytes += text.len();
        self.notices.push(Notice::info(text));
    }
}

/// Handles one `host(op, payload)` call (see the module docs).
pub(super) fn call(
    state: &std::cell::RefCell<State>,
    watch: &Watch,
    op: &str,
    payload: &str,
) -> String {
    let result = serde_json::from_str::<Json>(payload)
        .map_err(|e| Error::internal(format!("shell host call {op}: {e}")))
        .and_then(|args| dispatch(state, watch, op, &args));
    let reply = match result {
        Ok(value) => json!({ "ok": value }),
        Err(e) => {
            let mut state = state.borrow_mut();
            let id = state.errors.len();
            let (name, code, message) = match &e {
                Error::Sql { code, message } => ("MongoServerError", code.clone(), message.clone()),
                Error::Cancelled => ("CancelledError", None, e.to_string()),
                Error::Timeout { .. } => ("TimeoutError", None, e.to_string()),
                Error::Connection { .. } => ("ConnectionError", None, e.to_string()),
                Error::Auth { .. } => ("AuthError", None, e.to_string()),
                Error::InvalidInput { message } => ("TypeError", None, message.clone()),
                _ => ("MongoError", None, e.to_string()),
            };
            state.errors.push(e);
            json!({ "error": { "id": id, "name": name, "codeName": code, "message": message } })
        }
    };
    reply.to_string()
}

fn dispatch(
    state: &std::cell::RefCell<State>,
    watch: &Watch,
    op: &str,
    args: &Json,
) -> Result<Json> {
    match op {
        "print" => {
            state.borrow_mut().print(text_arg(args, "text")?);
            Ok(Json::Null)
        }
        "oid" => Ok(Json::String(ObjectId::new().to_hex())),
        "uuid" => Ok(Json::String(uuid::Uuid::new_v4().simple().to_string())),
        "use" => {
            let name = text_arg(args, "db")?;
            check_database_name(name)?;
            state.borrow_mut().switched = Some(name.to_owned());
            Ok(Json::Null)
        }
        "sleep" => {
            let ms = args
                .get("ms")
                .and_then(Json::as_f64)
                .unwrap_or(0.0)
                .max(0.0);
            let wanted = Duration::try_from_secs_f64(ms / 1000.0).unwrap_or(Duration::MAX);
            if wanted > MAX_SLEEP {
                return Err(Error::invalid_input("sleep() waits at most 10000 ms"));
            }
            let env = env(state)?;
            wait(&env, watch, async move {
                tokio::time::sleep(wanted).await;
                Ok(())
            })?;
            Ok(Json::Null)
        }
        "command" => {
            let env = env(state)?;
            let database = text_arg(args, "db")?.to_owned();
            let mut command = to_document(args.get("cmd").unwrap_or(&Json::Null), "command")?;
            if command.is_empty() {
                return Err(Error::invalid_input("a command needs at least one field"));
            }
            let comment = match command.get("comment") {
                None => env.comment.clone(),
                Some(Bson::String(text)) => format!("{} {text}", env.comment),
                Some(other) => format!("{} {}", env.comment, other.clone().into_relaxed_extjson()),
            };
            command.insert("comment", comment);
            let client = env.client.clone();
            let reply = wait(&env, watch, async move {
                client
                    .database(&database)
                    .run_command(command)
                    .await
                    .map_err(|e| map_error(&e))
            })?;
            Ok(Bson::Document(reply).into_canonical_extjson())
        }
        "find" => {
            let env = env(state)?;
            let database = text_arg(args, "db")?.to_owned();
            let collection = text_arg(args, "coll")?.to_owned();
            let filter = optional_document(args, "filter")?.unwrap_or_default();
            let mut options = FindOptions::default();
            options.projection = optional_document(args, "projection")?;
            options.sort = optional_document(args, "sort")?;
            options.skip = number(args, "skip")?.filter(|n| *n > 0);
            options.limit = number(args, "limit")?
                .filter(|n| *n != 0)
                .map(|n| i64::try_from(n).unwrap_or(i64::MAX));
            options.hint = hint(args)?;
            options.collation = collation(args)?;
            options.max_time = number(args, "maxTimeMS")?.map(Duration::from_millis);
            options.batch_size = batch_size(args)?;
            options.comment = Some(Bson::String(env.comment.clone()));
            let client = env.client.clone();
            let cursor = wait(&env, watch, async move {
                client
                    .database(&database)
                    .collection::<Document>(&collection)
                    .find(filter)
                    .with_options(options)
                    .await
                    .map_err(|e| map_error(&e))
            })?;
            Ok(json!({ "id": keep(state, cursor) }))
        }
        "aggregate" => {
            let env = env(state)?;
            let database = text_arg(args, "db")?.to_owned();
            let collection = args.get("coll").and_then(Json::as_str).map(str::to_owned);
            let pipeline = match args.get("pipeline") {
                Some(Json::Array(stages)) => stages
                    .iter()
                    .enumerate()
                    .map(|(i, s)| to_document(s, &format!("pipeline stage {}", i + 1)))
                    .collect::<Result<Vec<_>>>()?,
                _ => return Err(Error::invalid_input("aggregate needs a pipeline array")),
            };
            let mut options = AggregateOptions::default();
            options.allow_disk_use = args.get("allowDiskUse").and_then(Json::as_bool);
            options.collation = collation(args)?;
            options.max_time = number(args, "maxTimeMS")?.map(Duration::from_millis);
            options.batch_size = batch_size(args)?;
            options.hint = hint(args)?;
            options.comment = Some(Bson::String(env.comment.clone()));
            let client = env.client.clone();
            let cursor = wait(&env, watch, async move {
                let db = client.database(&database);
                let result = match collection {
                    Some(name) => {
                        db.collection::<Document>(&name)
                            .aggregate(pipeline)
                            .with_options(options)
                            .await
                    }
                    None => db.aggregate(pipeline).with_options(options).await,
                };
                result.map_err(|e| map_error(&e))
            })?;
            Ok(json!({ "id": keep(state, cursor) }))
        }
        "next" => {
            let env = env(state)?;
            let id = number(args, "id")?.unwrap_or(0);
            let max = number(args, "max")?
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(MAX_BATCH)
                .clamp(1, MAX_BATCH);
            let Some(cursor) = state.borrow_mut().cursors.remove(&id) else {
                return Ok(json!({ "docs": [], "done": true }));
            };
            let (cursor, docs) = wait(&env, watch, async move {
                let mut cursor = cursor;
                let mut docs = Vec::new();
                while docs.len() < max {
                    match cursor.next().await {
                        Some(Ok(doc)) => docs.push(doc),
                        Some(Err(e)) => return Err(map_error(&e)),
                        None => return Ok((None, docs)),
                    }
                }
                Ok((Some(cursor), docs))
            })?;
            let mut s = state.borrow_mut();
            s.fetched += docs.len();
            let done = cursor.is_none();
            if let Some(cursor) = cursor {
                s.cursors.insert(id, cursor);
            }
            if s.fetched > MAX_SCRIPT_DOCUMENTS {
                return Err(Error::invalid_input(format!(
                    "the script read more than {MAX_SCRIPT_DOCUMENTS} documents into JavaScript \
                     (toArray, forEach, next); return the cursor itself to stream every document \
                     into the result"
                )));
            }
            let docs: Vec<Json> = docs
                .into_iter()
                .map(|d| Bson::Document(d).into_canonical_extjson())
                .collect();
            Ok(json!({ "docs": docs, "done": done }))
        }
        "close" => {
            let id = number(args, "id")?.unwrap_or(0);
            let env = env(state)?;
            let cursor = state.borrow_mut().cursors.remove(&id);
            if let Some(cursor) = cursor {
                // Dropped on the runtime: an open cursor's drop sends killCursors from a task.
                env.handle.spawn(async move { drop(cursor) });
            }
            Ok(Json::Null)
        }
        other => Err(Error::internal(format!(
            "unknown shell host operation {other}"
        ))),
    }
}

fn env(state: &std::cell::RefCell<State>) -> Result<StatementEnv> {
    state.borrow().statement.clone().ok_or_else(|| {
        Error::invalid_input("the database is reachable only while a statement runs")
    })
}

fn keep(state: &std::cell::RefCell<State>, cursor: Cursor<Document>) -> u64 {
    let mut s = state.borrow_mut();
    s.next_cursor += 1;
    let id = s.next_cursor;
    s.cursors.insert(id, cursor);
    id
}

/// Runs `work` on the statement's runtime and waits for it on this (non-runtime) thread; the
/// statement's token ends the wait with `Cancelled`. The time spent waiting is not CPU time.
fn wait<T: Send + 'static>(
    env: &StatementEnv,
    watch: &Watch,
    work: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    if env.cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let cancel = env.cancel.clone();
    let started = Instant::now();
    env.handle.spawn(async move {
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(Error::Cancelled),
            r = work => r,
        };
        let _ = tx.send(result);
    });
    let result = rx
        .recv()
        .unwrap_or_else(|_| Err(Error::connection("the shell's async runtime stopped")));
    watch.waited(started.elapsed());
    result
}

fn text_arg<'a>(args: &'a Json, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| Error::invalid_input(format!("missing {key}")))
}

fn number(args: &Json, key: &str) -> Result<Option<u64>> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|n| n.is_finite())
            .map(|n| {
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "a JavaScript count: whole, made non-negative and saturated"
                )]
                let n = n.abs().trunc().min(9.0e15) as u64;
                Some(n)
            })
            .ok_or_else(|| Error::invalid_input(format!("{key} must be a number"))),
    }
}

fn batch_size(args: &Json) -> Result<Option<u32>> {
    Ok(number(args, "batchSize")?
        .filter(|n| *n > 0)
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX)))
}

fn optional_document(args: &Json, key: &str) -> Result<Option<Document>> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => to_document(v, key).map(Some),
    }
}

fn hint(args: &Json) -> Result<Option<Hint>> {
    match args.get("hint") {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(name)) => Ok(Some(Hint::Name(name.clone()))),
        Some(keys) => to_document(keys, "hint").map(|d| Some(Hint::Keys(d))),
    }
}

fn collation(args: &Json) -> Result<Option<Collation>> {
    optional_document(args, "collation")?
        .map(|doc| {
            bson::deserialize_from_document::<Collation>(doc)
                .map_err(|e| Error::invalid_input(format!("collation: {e}")))
        })
        .transpose()
}

/// A JSON object (canonical Extended JSON from the prelude) as a BSON document. An object
/// whose keys are exactly `$regex` (and `$options`) stays the query operator.
pub(super) fn to_document(value: &Json, what: &str) -> Result<Document> {
    match to_bson(value, what)? {
        Bson::Document(d) => Ok(d),
        _ => Err(Error::invalid_input(format!("{what} must be a document"))),
    }
}

/// A JSON value (Extended JSON) as BSON, with the `$regex` operator exception.
pub(super) fn to_bson(value: &Json, what: &str) -> Result<Bson> {
    match value {
        Json::Object(object) => {
            if !is_regex_operator(object)
                && object.keys().any(|k| WRAPPER_KEYS.contains(&k.as_str()))
            {
                return Bson::try_from(value.clone()).map_err(|e| {
                    Error::invalid_input(format!("{what}: invalid Extended JSON value: {e}"))
                });
            }
            let mut out = Document::new();
            for (key, v) in object {
                out.insert(key.clone(), to_bson(v, what)?);
            }
            Ok(Bson::Document(out))
        }
        Json::Array(items) => items
            .iter()
            .map(|v| to_bson(v, what))
            .collect::<Result<Vec<_>>>()
            .map(Bson::Array),
        scalar => {
            Bson::try_from(scalar.clone()).map_err(|e| Error::invalid_input(format!("{what}: {e}")))
        }
    }
}

fn is_regex_operator(object: &Map<String, Json>) -> bool {
    object.contains_key("$regex") && object.keys().all(|k| k == "$regex" || k == "$options")
}

/// Database names MongoDB accepts (no `/\. "$*<>:|?`, at most 63 bytes).
pub(super) fn check_database_name(name: &str) -> Result<()> {
    let bad = name.is_empty()
        || name.len() > 63
        || name
            .chars()
            .any(|c| c.is_control() || "/\\. \"$*<>:|?".contains(c));
    if bad {
        return Err(Error::invalid_input(format!(
            "'{name}' is not a valid database name"
        )));
    }
    Ok(())
}

/// A `mongodb` error as an oxbase error (the categories of the driver docs).
pub(super) fn map_error(e: &mongodb::error::Error) -> Error {
    match e.kind.as_ref() {
        ErrorKind::Authentication { message, .. } => Error::auth(message.clone()),
        ErrorKind::Command(c) => server_error(c.code, &c.code_name, &c.message),
        ErrorKind::Write(WriteFailure::WriteError(w)) => server_error(
            w.code,
            w.code_name.as_deref().unwrap_or_default(),
            &w.message,
        ),
        ErrorKind::Write(WriteFailure::WriteConcernError(w)) => {
            server_error(w.code, &w.code_name, &w.message)
        }
        ErrorKind::ServerSelection { message, .. }
        | ErrorKind::ProxyConnect { message, .. }
        | ErrorKind::DnsResolve { message, .. }
        | ErrorKind::ConnectionPoolCleared { message, .. } => Error::connection(message.clone()),
        ErrorKind::Io(io) => Error::connection(io.to_string()),
        ErrorKind::Shutdown => Error::connection("the MongoDB connection is closed"),
        ErrorKind::InvalidArgument { message, .. } => Error::invalid_input(message.clone()),
        ErrorKind::BsonDeserialization(err)
        | ErrorKind::BsonSerialization(err)
        | ErrorKind::Bson(err) => Error::invalid_input(err.to_string()),
        _ => Error::sql(None::<String>, e.to_string()),
    }
}

fn server_error(code: i32, code_name: &str, message: &str) -> Error {
    match code {
        AUTHENTICATION_FAILED => Error::auth(message),
        INTERRUPTED => Error::Cancelled,
        MAX_TIME_MS_EXPIRED | EXCEEDED_TIME_LIMIT => Error::timeout(message),
        NAMESPACE_NOT_FOUND if code_name.is_empty() => {
            Error::sql(Some("NamespaceNotFound"), message)
        }
        _ => {
            let code = if code_name.is_empty() {
                code.to_string()
            } else {
                code_name.to_owned()
            };
            Error::sql(Some(code), message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    #[test]
    fn extended_json_with_the_regex_operator() {
        let value = json!({
            "_id": {"$oid": "65a000000000000000000001"},
            "n": {"$numberInt": "5"},
            "name": {"$regex": "^a", "$options": "i"},
            "re": {"$regularExpression": {"pattern": "x", "options": ""}},
            "list": [{"$numberLong": "7"}, "s"],
        });
        let d = to_document(&value, "filter").unwrap();
        assert_eq!(d.get("n"), Some(&Bson::Int32(5)));
        assert_eq!(
            d.get_document("name").unwrap(),
            &doc! {"$regex": "^a", "$options": "i"}
        );
        assert!(matches!(d.get("re"), Some(Bson::RegularExpression(_))));
        assert_eq!(d.get_array("list").unwrap()[0], Bson::Int64(7));
        assert!(to_document(&json!([1]), "filter").is_err());
        assert!(to_document(&json!({"$oid": "zz"}), "filter").is_err());
    }

    #[test]
    fn database_names() {
        for good in ["oxbase_test", "a-b", "ü"] {
            assert!(check_database_name(good).is_ok(), "{good}");
        }
        for bad in ["", "a.b", "a b", "a/b", "$x", &"x".repeat(64)] {
            assert!(check_database_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn server_errors_map_to_categories() {
        assert!(server_error(11601, "Interrupted", "x").is_cancelled());
        assert!(matches!(
            server_error(50, "MaxTimeMSExpired", "x"),
            Error::Timeout { .. }
        ));
        assert_eq!(
            server_error(26, "NamespaceNotFound", "ns").sql_code(),
            Some("NamespaceNotFound")
        );
        assert_eq!(server_error(2, "", "x").sql_code(), Some("2"));
    }

    #[test]
    fn printing_is_bounded() {
        let mut state = State::default();
        for i in 0..(MAX_NOTICES + 5) {
            state.print(&format!("line {i}"));
        }
        assert_eq!(state.notices.len(), MAX_NOTICES + 1);
        assert!(state.notices.last().unwrap().message.contains("dropped"));
        let left = state.end();
        assert_eq!(left.notices.len(), MAX_NOTICES + 1);
        assert_eq!(state.notices.len(), 0);
    }
}
