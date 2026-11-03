//! Atomic key edits (`KeyValueStore::apply`, docs/m7-contracts.md §5.2).
//!
//! 0. Request checks without the server: an expiry of 0 ms (it deletes the key) needs
//!    `confirm_destructive` like a delete; a TTL above [`MAX_TTL_MS`] is refused; a `ListSet`
//!    may not follow a `ListPush` / `ListRemove` of the same request (the index and `expected`
//!    would name another element than the one checked); an explicit `XADD` id may not follow a
//!    server-chosen one.
//! 1. `WATCH key` (and the rename target), on a dedicated connection like the session's
//!    (standalone, sentinel) or on the session itself on a cluster (routed to the key's node;
//!    applies of one cluster session take turns, see [`LinkTarget`]).
//! 2. Checks, before anything is written: `TYPE` (missing, existing with `create`, another
//!    type than the editor read or than the edits need → conflict), `LINDEX` for `ListSet`
//!    (another value than `expected` → conflict; out of range → error), `EXISTS` of a
//!    `RENAMENX` target, the last generated id (`XINFO STREAM`) for `XADD` with explicit or
//!    `ms-*` ids, `JSON.TYPE` of a `JSON.SET` path below the root (it must exist, or be a new
//!    member of an existing object), and `STRLEN` before a `SET` without
//!    `confirm_destructive` (a value larger than the editor reads cannot be overwritten
//!    unconfirmed). They exclude the errors that `EXEC` would only report after earlier edits
//!    were applied (Redis does not roll back).
//! 3. `MULTI`, one command per edit, `EXEC`. A nil `EXEC` (a watched key changed) is a
//!    conflict and nothing was written; an error while queuing (an unknown command) aborts
//!    everything too. Should an edit still fail inside `EXEC` (a nil `JSON.SET` included), the
//!    report says which, and the caller reads the key back so the UI refreshes instead of
//!    retrying the edits that were applied.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, PoisonError, Weak};

use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{Error, KeyApplyReport, KeyApplyRequest, KeyEdit, KeyType, ListEnd, Result};
use redis::Value;
use tokio::sync::{Mutex, OwnedMutexGuard};

use super::common::{
    CLUSTER_FAN_OUT, bytes_of, check, int_of, items, key_type_of, map_of, text_of,
};
use super::source::{STRING_READ_LIMIT, parse_stream_id};
use crate::driver::{DedicatedConnection, RedisConnection, RedisHandle};

/// The highest TTL an `expire` edit takes (Redis refuses an expiry time that overflows).
pub(crate) const MAX_TTL_MS: u64 = 1 << 62;

const JSON_MODULE: &str = "JSON values need the RedisJSON module, which this server does not have";

/// The turns of the applies (and checked string reads) of each cluster session, by the
/// session's address. On a cluster they `WATCH` on the session's shared connection to the key's
/// node, where the `EXEC` of another apply would end the `WATCH` (and with it the conflict
/// check), so they run one at a time. An entry lives while an apply holds it.
static CLUSTER_TURNS: LazyLock<std::sync::Mutex<HashMap<usize, Weak<Mutex<()>>>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn cluster_turns(c: &RedisConnection) -> Arc<Mutex<()>> {
    let id = std::ptr::from_ref(c).addr();
    let mut all = CLUSTER_TURNS.lock().unwrap_or_else(PoisonError::into_inner);
    all.retain(|_, turns| turns.strong_count() > 0);
    if let Some(turns) = all.get(&id).and_then(Weak::upgrade) {
        return turns;
    }
    let turns = Arc::new(Mutex::new(()));
    all.insert(id, Arc::downgrade(&turns));
    turns
}

/// What a [`Link`] opens on; owned, so a result stream can keep it.
#[derive(Clone)]
pub(super) struct LinkTarget {
    handle: RedisHandle,
    /// `Some` on a cluster (see [`CLUSTER_TURNS`]).
    turns: Option<Arc<Mutex<()>>>,
}

impl LinkTarget {
    pub(super) fn of(c: &RedisConnection) -> Self {
        Self {
            handle: c.handle(),
            turns: c.is_cluster().then(|| cluster_turns(c)),
        }
    }

    /// A link for `key`: a dedicated connection like the session's (standalone, sentinel), or
    /// the session itself once no other apply of the session runs (a cluster).
    pub(super) async fn open(&self, key: &[u8], cancel: &CancellationToken) -> Result<Link> {
        let Some(turns) = &self.turns else {
            let config = redis::AsyncConnectionConfig::new().set_response_timeout(None);
            return Ok(Link::Dedicated(
                self.handle.dedicated(config, cancel).await?,
            ));
        };
        let turn = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Error::Cancelled),
            turn = turns.clone().lock_owned() => turn,
        };
        Ok(Link::Session {
            handle: self.handle.clone(),
            key: key.to_vec(),
            _turn: turn,
        })
    }
}

/// Where the `WATCH` / `MULTI` / `EXEC` of one apply (or one checked string read) runs.
pub(super) enum Link {
    Dedicated(DedicatedConnection),
    /// A cluster session: `WATCH` and the transaction are routed by `key` to its node.
    Session {
        handle: RedisHandle,
        key: Vec<u8>,
        _turn: OwnedMutexGuard<()>,
    },
}

impl Link {
    pub(super) async fn command(
        &self,
        c: &redis::Cmd,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        match self {
            Link::Session { handle, .. } => handle.command(c, cancel).await,
            Link::Dedicated(d) => {
                let mut conn = d.connection().clone();
                let reply = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(Error::Cancelled),
                    reply = c.query_async::<Value>(&mut conn) => reply,
                };
                match reply {
                    Ok(Value::ServerError(e)) => Err(server_error(&e)),
                    Ok(v) => Ok(v),
                    Err(e) => Err(redis_error(&e)),
                }
            }
        }
    }

    /// `WATCH keys`.
    pub(super) async fn watch(&self, keys: &[&[u8]], cancel: &CancellationToken) -> Result<()> {
        let mut watch = redis::cmd("WATCH");
        for key in keys {
            watch.arg(*key);
        }
        self.command(&watch, cancel).await?;
        Ok(())
    }

    /// Runs the atomic pipeline: `Ok(None)` when `EXEC` answered nil (a watched key changed).
    pub(super) async fn exec(
        &self,
        pipe: &redis::Pipeline,
        cancel: &CancellationToken,
    ) -> Result<Option<Vec<Value>>> {
        match self {
            Link::Session { handle, .. } => match handle.pipeline(pipe, cancel).await {
                Ok(values) => Ok(Some(values)),
                Err(Error::Sql { code, message })
                    if code.as_deref() == Some("EXECABORT") && message.contains("watched key") =>
                {
                    Ok(None)
                }
                Err(e) => Err(e),
            },
            Link::Dedicated(d) => {
                let mut conn = d.connection().clone();
                let reply = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(Error::Cancelled),
                    reply = pipe.query_async::<Value>(&mut conn) => reply,
                };
                match reply {
                    Ok(Value::Nil) => Ok(None),
                    Ok(Value::Array(values)) => Ok(Some(values)),
                    Ok(other) => Err(Error::internal(format!(
                        "EXEC returned {} instead of a list of replies",
                        kind_of(&other)
                    ))),
                    Err(e) => Err(redis_error(&e)),
                }
            }
        }
    }

    /// Ends the `WATCH` of an apply that stops before `EXEC` (a dedicated connection just
    /// closes). On a cluster this is a one-command transaction routed by the key, since
    /// `UNWATCH` (no key) would go to every master; `EXEC` ends every `WATCH` of the
    /// connection.
    pub(super) async fn unwatch(&self) {
        if let Link::Session { handle, key, .. } = self {
            let mut pipe = redis::pipe();
            pipe.atomic().cmd("EXISTS").arg(key.as_slice());
            let _ = handle.pipeline(&pipe, &CancellationToken::new()).await;
        }
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Nil => "nil",
        Value::Int(_) => "an integer",
        Value::Map(_) => "a map",
        _ => "another reply",
    }
}

/// An error reply as `Error::Sql` with its first word as the code.
fn server_error(e: &redis::ServerError) -> Error {
    Error::sql(
        Some(e.code().to_owned()),
        e.details()
            .map_or_else(|| e.code().to_owned(), str::to_owned),
    )
}

/// A `redis` crate error of the dedicated connection.
fn redis_error(e: &redis::RedisError) -> Error {
    if let Some(code) = e.code() {
        return Error::sql(
            Some(code.to_owned()),
            e.detail().map_or_else(|| e.to_string(), str::to_owned),
        );
    }
    if e.is_timeout() {
        return Error::timeout(format!("Redis did not answer in time: {e}"));
    }
    Error::connection(format!("Redis connection: {e}"))
}

/// What the checks found: why nothing may be written.
enum Refusal {
    Conflict(String),
    Error(String),
}

fn report(refusal: Refusal) -> KeyApplyReport {
    let (conflict, error) = match refusal {
        Refusal::Conflict(e) => (true, e),
        Refusal::Error(e) => (false, e),
    };
    KeyApplyReport {
        committed: false,
        conflict,
        error: Some(error),
        info: None,
    }
}

/// Applies `request` on `c` (a session on `request.address.database`). Returns the report
/// without its `info` (the caller reads the key back), and whether anything was written
/// (`true` when committed, and when some edits failed inside `EXEC` while others applied).
pub(crate) async fn apply(
    c: &RedisConnection,
    request: &KeyApplyRequest,
    cancel: &CancellationToken,
) -> Result<(KeyApplyReport, bool)> {
    check(cancel)?;
    request_checks(request)?;
    let key = request.address.key.to_bytes()?;
    let rename = request
        .edits
        .iter()
        .find_map(|e| match e {
            KeyEdit::Rename { new_key, overwrite } => Some((new_key, *overwrite)),
            _ => None,
        })
        .map(|(new_key, overwrite)| new_key.to_bytes().map(|k| (k, overwrite)))
        .transpose()?;
    let link = LinkTarget::of(c).open(&key, cancel).await?;
    let mut watched = vec![key.as_slice()];
    if let Some((new_key, _)) = &rename
        && *new_key != key
    {
        watched.push(new_key.as_slice());
    }
    link.watch(&watched, cancel).await?;

    let refusal = match checks(&link, c, &key, rename.as_ref(), request, cancel).await {
        Ok(refusal) => refusal,
        Err(e) => {
            link.unwatch().await;
            return Err(e);
        }
    };
    if let Some(refusal) = refusal {
        link.unwatch().await;
        return Ok((report(refusal), false));
    }

    let mut pipe = redis::pipe();
    pipe.atomic().ignore_errors();
    for edit in &request.edits {
        pipe.add_command(command(&key, edit)?);
    }
    let replies = match link.exec(&pipe, cancel).await {
        Ok(Some(replies)) => replies,
        Ok(None) => {
            let refusal = Refusal::Conflict(
                "the key changed while the changes were being applied; nothing was written"
                    .to_owned(),
            );
            return Ok((report(refusal), false));
        }
        Err(e) if e.is_cancelled() => return Err(e),
        Err(e) => {
            link.unwatch().await;
            if is_unknown_command(&e)
                && request
                    .edits
                    .iter()
                    .any(|e| matches!(e, KeyEdit::JsonSet { .. }))
            {
                return Err(Error::unsupported(JSON_MODULE));
            }
            return Ok((
                report(Refusal::Error(format!("nothing was written: {e}"))),
                false,
            ));
        }
    };
    let failed = failures(&replies, &request.edits);
    if !failed.is_empty() {
        let applied = request.edits.len().saturating_sub(failed.len());
        let text = if applied == 0 {
            format!("{}; nothing was written", failed.join("; "))
        } else {
            format!(
                "{} (the other {applied} change(s) were applied)",
                failed.join("; ")
            )
        };
        return Ok((report(Refusal::Error(text)), applied > 0));
    }
    Ok((
        KeyApplyReport {
            committed: true,
            conflict: false,
            error: None,
            info: None,
        },
        true,
    ))
}

/// The edits that failed inside `EXEC`: an error reply, or a nil `JSON.SET` (its path did not
/// exist).
fn failures(replies: &[Value], edits: &[KeyEdit]) -> Vec<String> {
    replies
        .iter()
        .zip(edits)
        .filter_map(|(reply, edit)| match (reply, edit) {
            (Value::ServerError(e), _) => Some(format!("{}: {}", edit_name(edit), server_error(e))),
            (Value::Nil, KeyEdit::JsonSet { path, .. }) => {
                Some(format!("JSON.SET: the path {path} does not exist"))
            }
            _ => None,
        })
        .collect()
}

fn is_unknown_command(e: &Error) -> bool {
    let text = e.to_string().to_ascii_lowercase();
    text.contains("unknown command")
}

/// Step 0 of the module docs: request shapes that would fail half-way inside `EXEC`, or that
/// delete without confirmation.
fn request_checks(request: &KeyApplyRequest) -> Result<()> {
    let mut moved = false;
    let mut auto_id = false;
    for edit in &request.edits {
        match edit {
            KeyEdit::Expire { ttl_ms: Some(0) } if !request.confirm_destructive => {
                return Err(Error::invalid_input(
                    "an expiry of 0 ms deletes the key at once, so it needs the same \
                     confirmation as a delete",
                ));
            }
            KeyEdit::Expire { ttl_ms: Some(ms) } if *ms > MAX_TTL_MS => {
                return Err(Error::invalid_input(format!(
                    "a TTL of {ms} ms is out of range (at most {MAX_TTL_MS} ms)"
                )));
            }
            KeyEdit::ListPush { .. } | KeyEdit::ListRemove { .. } => moved = true,
            KeyEdit::ListSet { .. } if moved => {
                return Err(Error::invalid_input(
                    "a list element is set by its index, so it cannot follow a push or a remove \
                     in the same apply; apply them one after the other",
                ));
            }
            KeyEdit::StreamAdd { id, .. } => {
                if id.as_deref().is_none_or(|id| id == "*") {
                    auto_id = true;
                } else if auto_id {
                    return Err(Error::invalid_input(
                        "an explicit stream entry id cannot follow a server-chosen one in the \
                         same apply",
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The checks of step 2 of the module docs; `Some` when nothing may be written.
async fn checks(
    link: &Link,
    c: &RedisConnection,
    key: &[u8],
    rename: Option<&(Vec<u8>, bool)>,
    request: &KeyApplyRequest,
    cancel: &CancellationToken,
) -> Result<Option<Refusal>> {
    let mut t = redis::cmd("TYPE");
    t.arg(key);
    let actual = key_type_of(&link.command(&t, cancel).await?);
    match (request.create, actual) {
        (true, Some(_)) => {
            return Ok(Some(Refusal::Conflict(
                "a key with this name already exists".to_owned(),
            )));
        }
        (false, None) => {
            return Ok(Some(Refusal::Conflict(
                "the key does not exist (any more)".to_owned(),
            )));
        }
        _ => {}
    }
    if let (Some(read), Some(now)) = (request.key_type, actual)
        && read != now
    {
        return Ok(Some(Refusal::Conflict(format!(
            "the key is now a {now}, not a {read}"
        ))));
    }
    let first = request.edits.iter().find_map(KeyEdit::key_type);
    let Some(effective) = actual.or(first) else {
        return Ok(None);
    };
    if let Some(edit) = request
        .edits
        .iter()
        .find(|e| e.key_type().is_some_and(|t| t != effective))
    {
        return Ok(Some(Refusal::Conflict(format!(
            "{} cannot change a {effective} key",
            edit_name(edit)
        ))));
    }
    if let Some((new_key, overwrite)) = rename {
        if new_key.as_slice() == key {
            return Ok(Some(Refusal::Error(
                "the new key name is the same as the old one".to_owned(),
            )));
        }
        if !overwrite {
            let mut e = redis::cmd("EXISTS");
            e.arg(new_key.as_slice());
            if int_of(&link.command(&e, cancel).await?).unwrap_or(0) > 0 {
                return Ok(Some(Refusal::Error(
                    "a key with the new name already exists".to_owned(),
                )));
            }
        }
    }

    let exists = actual.is_some();
    if let Some(refusal) = list_checks(link, key, request, cancel).await? {
        return Ok(Some(refusal));
    }
    if let Some(refusal) = stream_checks(link, key, request, exists, cancel).await? {
        return Ok(Some(refusal));
    }
    if let Some(refusal) = json_checks(link, key, request, exists, cancel).await? {
        return Ok(Some(refusal));
    }
    if effective == KeyType::String {
        if let Some(refusal) = string_checks(link, key, request, exists, cancel).await? {
            return Ok(Some(refusal));
        }
        if request
            .edits
            .iter()
            .any(|e| matches!(e, KeyEdit::SetString { keep_ttl: true, .. }))
            && !c.server_version().at_least(6, 0)
            && c.server_version().major > 0
        {
            return Ok(Some(Refusal::Error(
                "keeping the TTL of a string needs Redis 6.0 or newer".to_owned(),
            )));
        }
    }
    Ok(None)
}

/// A `SET` over a value larger than the editor reads ([`STRING_READ_LIMIT`]) would replace
/// bytes nobody saw: it needs `confirm_destructive`.
async fn string_checks(
    link: &Link,
    key: &[u8],
    request: &KeyApplyRequest,
    exists: bool,
    cancel: &CancellationToken,
) -> Result<Option<Refusal>> {
    if !exists
        || request.confirm_destructive
        || !request
            .edits
            .iter()
            .any(|e| matches!(e, KeyEdit::SetString { .. }))
    {
        return Ok(None);
    }
    let mut strlen = redis::cmd("STRLEN");
    strlen.arg(key);
    let length = int_of(&link.command(&strlen, cancel).await?).unwrap_or(0);
    if usize::try_from(length).is_ok_and(|n| n > STRING_READ_LIMIT) {
        return Ok(Some(Refusal::Error(format!(
            "the value has {length} bytes, more than the {STRING_READ_LIMIT} the editor reads; \
             overwriting it needs the same confirmation as a delete"
        ))));
    }
    Ok(None)
}

/// `LINDEX` for every `ListSet` (side by side): another value than `expected` is a conflict,
/// a missing element an error. (A `ListSet` never follows a push or a remove of the same
/// request, see [`request_checks`], so the list checked is the list it changes.)
async fn list_checks(
    link: &Link,
    key: &[u8],
    request: &KeyApplyRequest,
    cancel: &CancellationToken,
) -> Result<Option<Refusal>> {
    let sets: Vec<(i64, Option<oxbase_types::BinaryText>)> = request
        .edits
        .iter()
        .filter_map(|edit| match edit {
            KeyEdit::ListSet {
                index, expected, ..
            } => Some((*index, expected.clone())),
            _ => None,
        })
        .collect();
    if sets.is_empty() {
        return Ok(None);
    }
    // Side by side in groups (no closure over references: the futures must be `Send` for
    // every lifetime).
    let mut replies = Vec::with_capacity(sets.len());
    for chunk in sets.chunks(CLUSTER_FAN_OUT) {
        let mut pending = Vec::with_capacity(chunk.len());
        for (index, _) in chunk {
            let mut c = redis::cmd("LINDEX");
            c.arg(key).arg(*index);
            pending.push(async move { link.command(&c, cancel).await });
        }
        replies.extend(futures::future::join_all(pending).await);
    }
    for ((index, expected), reply) in sets.into_iter().zip(replies) {
        let current = bytes_of(&reply?);
        match (current, expected) {
            (None, _) => {
                return Ok(Some(Refusal::Error(format!(
                    "the list has no element at index {index}"
                ))));
            }
            (Some(current), Some(expected)) if current != expected.to_bytes()? => {
                return Ok(Some(Refusal::Conflict(format!(
                    "the list element at index {index} changed"
                ))));
            }
            _ => {}
        }
    }
    Ok(None)
}

/// A new stream entry id of an `XADD`: `ms-seq` (or `ms`), or `ms-*` (the server picks the
/// sequence).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewId {
    Exact((u64, u64)),
    Ms(u64),
}

fn new_id(id: &str) -> Option<NewId> {
    match id.strip_suffix("-*") {
        Some(ms) => ms.parse().ok().map(NewId::Ms),
        None => parse_stream_id(id).map(NewId::Exact),
    }
}

/// The id `XADD` gives `id` after `top` (the stream's last generated id).
fn next_id(id: NewId, top: (u64, u64)) -> (u64, u64) {
    match id {
        NewId::Exact(id) => id,
        NewId::Ms(ms) if ms == top.0 => (ms, top.1.saturating_add(1)),
        // `0-*` on a new stream is `0-1` (`0-0` is not a valid id).
        NewId::Ms(ms) => (ms, u64::from(ms == 0)),
    }
}

/// Explicit and `ms-*` `XADD` ids must grow: past the stream's last generated id (which
/// `XDEL` of the top entry does not lower) and from one edit to the next.
async fn stream_checks(
    link: &Link,
    key: &[u8],
    request: &KeyApplyRequest,
    exists: bool,
    cancel: &CancellationToken,
) -> Result<Option<Refusal>> {
    let explicit: Vec<&str> = request
        .edits
        .iter()
        .filter_map(|e| match e {
            KeyEdit::StreamAdd { id: Some(id), .. } if id != "*" => Some(id.as_str()),
            _ => None,
        })
        .collect();
    if explicit.is_empty() {
        return Ok(None);
    }
    let mut top = if exists {
        last_stream_id(link, key, cancel).await?
    } else {
        (0, 0)
    };
    for id in explicit {
        let Some(parsed) = new_id(id) else {
            return Ok(Some(Refusal::Error(format!(
                "'{id}' is not a stream entry id (milliseconds-sequence)"
            ))));
        };
        let next = next_id(parsed, top);
        if next <= top {
            return Ok(Some(Refusal::Error(format!(
                "the entry id {id} must be greater than the stream's last id {}-{}",
                top.0, top.1
            ))));
        }
        top = next;
    }
    Ok(None)
}

/// The last generated id of a stream (`XINFO STREAM`), else its last entry's id.
async fn last_stream_id(link: &Link, key: &[u8], cancel: &CancellationToken) -> Result<(u64, u64)> {
    let mut info = redis::cmd("XINFO");
    info.arg("STREAM").arg(key);
    if let Ok(reply) = link.command(&info, cancel).await
        && let Some(id) = map_of(reply)
            .get("last-generated-id")
            .and_then(text_of)
            .and_then(|id| parse_stream_id(&id))
    {
        return Ok(id);
    }
    check(cancel)?;
    let mut last = redis::cmd("XREVRANGE");
    last.arg(key).arg("+").arg("-").arg("COUNT").arg(1);
    Ok(items(link.command(&last, cancel).await?)
        .into_iter()
        .next()
        .and_then(|entry| items(entry).into_iter().next())
        .and_then(|id| text_of(&id))
        .and_then(|id| parse_stream_id(&id))
        .unwrap_or((0, 0)))
}

/// `true` for the root path of a JSON value.
fn is_json_root(path: &str) -> bool {
    path == "$" || path == "."
}

/// The parent of a JSON path whose last step names an object member (`$.a.b` → `$.a`,
/// `$['k']` → `$`, legacy `.a` or `a` → `.`); `None` for any other last step (an index, a
/// wildcard, a filter, recursive descent).
fn json_parent(path: &str) -> Option<&str> {
    if let Some(inner) = path.strip_suffix(']') {
        let open = inner.rfind('[')?;
        let name = &inner[open + 1..];
        let quoted = name.len() >= 2
            && ((name.starts_with('\'') && name.ends_with('\''))
                || (name.starts_with('"') && name.ends_with('"')));
        let parent = &path[..open];
        return (quoted && !parent.is_empty() && !parent.ends_with('.')).then_some(parent);
    }
    let simple = |name: &str| {
        !name.is_empty()
            && name
                .chars()
                .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '-')
    };
    match path.rfind('.') {
        Some(dot) => {
            let (parent, name) = (&path[..dot], &path[dot + 1..]);
            if !simple(name) || parent.ends_with('.') {
                return None;
            }
            Some(if parent.is_empty() { "." } else { parent })
        }
        None => (simple(path) && !path.starts_with('$')).then_some("."),
    }
}

/// The type names of a `JSON.TYPE` reply (nested in RESP3), none when the path matches
/// nothing.
fn json_type_names(value: Value) -> Vec<String> {
    match value {
        Value::Array(list) | Value::Set(list) => {
            list.into_iter().flat_map(json_type_names).collect()
        }
        Value::Nil => Vec::new(),
        other => text_of(&other).into_iter().collect(),
    }
}

/// `JSON.TYPE key path`: the types of what `path` matches (an error reply, such as a legacy
/// path that does not exist, matches nothing).
async fn json_types(
    link: &Link,
    key: &[u8],
    path: &str,
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    let mut c = redis::cmd("JSON.TYPE");
    c.arg(key).arg(path);
    match link.command(&c, cancel).await {
        Ok(reply) => Ok(json_type_names(reply)),
        Err(e) if is_unknown_command(&e) => Err(Error::unsupported(JSON_MODULE)),
        Err(Error::Sql { .. }) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// `JSON.SET` below the root: the path must exist, or be a new member of an existing object
/// (else `JSON.SET` answers nil inside `EXEC` while the other edits apply). A new key starts
/// at the root, and a path below the root is set one per apply (an earlier `JSON.SET` of the
/// same apply could move it).
async fn json_checks(
    link: &Link,
    key: &[u8],
    request: &KeyApplyRequest,
    exists: bool,
    cancel: &CancellationToken,
) -> Result<Option<Refusal>> {
    let paths: Vec<&str> = request
        .edits
        .iter()
        .filter_map(|e| match e {
            KeyEdit::JsonSet { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    let Some(path) = paths.iter().copied().find(|p| !is_json_root(p)) else {
        return Ok(None);
    };
    if !exists {
        return Ok(Some(Refusal::Error(
            "a new JSON key is written at the root path $ first".to_owned(),
        )));
    }
    if paths.len() > 1 {
        return Ok(Some(Refusal::Error(
            "a JSON path below the root is changed alone (an earlier change of the same apply \
             could move it); apply the changes one after the other"
                .to_owned(),
        )));
    }
    if !json_types(link, key, path, cancel).await?.is_empty() {
        return Ok(None);
    }
    let parent_is_object = match json_parent(path) {
        Some(parent) => {
            let types = json_types(link, key, parent, cancel).await?;
            !types.is_empty() && types.iter().all(|t| t == "object")
        }
        None => false,
    };
    if parent_is_object {
        return Ok(None);
    }
    Ok(Some(Refusal::Error(format!(
        "the JSON path {path} does not exist; only a new member of an existing object can be \
         created"
    ))))
}

/// A short name of an edit for messages.
fn edit_name(edit: &KeyEdit) -> &'static str {
    match edit {
        KeyEdit::SetString { .. } => "SET",
        KeyEdit::Expire { ttl_ms: Some(_) } => "PEXPIRE",
        KeyEdit::Expire { ttl_ms: None } => "PERSIST",
        KeyEdit::Rename {
            overwrite: true, ..
        } => "RENAME",
        KeyEdit::Rename { .. } => "RENAMENX",
        KeyEdit::Delete => "UNLINK",
        KeyEdit::HashSet { only_new: true, .. } => "HSETNX",
        KeyEdit::HashSet { .. } => "HSET",
        KeyEdit::HashDelete { .. } => "HDEL",
        KeyEdit::ListPush {
            end: ListEnd::Head, ..
        } => "LPUSH",
        KeyEdit::ListPush { .. } => "RPUSH",
        KeyEdit::ListSet { .. } => "LSET",
        KeyEdit::ListRemove { .. } => "LREM",
        KeyEdit::SetAdd { .. } => "SADD",
        KeyEdit::SetRemove { .. } => "SREM",
        KeyEdit::ZsetAdd { .. } => "ZADD",
        KeyEdit::ZsetRemove { .. } => "ZREM",
        KeyEdit::StreamAdd { .. } => "XADD",
        KeyEdit::StreamDelete { .. } => "XDEL",
        KeyEdit::JsonSet { .. } => "JSON.SET",
    }
}

/// The command of one edit of `key`.
pub(crate) fn command(key: &[u8], edit: &KeyEdit) -> Result<redis::Cmd> {
    let mut c = redis::cmd(edit_name(edit));
    c.arg(key);
    match edit {
        KeyEdit::SetString { value, keep_ttl } => {
            c.arg(value.to_bytes()?);
            if *keep_ttl {
                c.arg("KEEPTTL");
            }
        }
        KeyEdit::Expire { ttl_ms } => {
            if let Some(ms) = ttl_ms {
                c.arg(*ms);
            }
        }
        KeyEdit::Rename { new_key, .. } => {
            c.arg(new_key.to_bytes()?);
        }
        KeyEdit::Delete => {}
        KeyEdit::HashSet { field, value, .. } => {
            c.arg(field.to_bytes()?).arg(value.to_bytes()?);
        }
        KeyEdit::HashDelete { fields: list }
        | KeyEdit::ListPush { values: list, .. }
        | KeyEdit::SetAdd { members: list }
        | KeyEdit::SetRemove { members: list }
        | KeyEdit::ZsetRemove { members: list } => {
            for item in list {
                c.arg(item.to_bytes()?);
            }
        }
        KeyEdit::ListSet { index, value, .. } => {
            c.arg(*index).arg(value.to_bytes()?);
        }
        KeyEdit::ListRemove { value, count } => {
            c.arg(*count).arg(value.to_bytes()?);
        }
        KeyEdit::ZsetAdd { member, score } => {
            c.arg(score_text(*score)).arg(member.to_bytes()?);
        }
        KeyEdit::StreamAdd { id, fields } => {
            c.arg(id.as_deref().unwrap_or("*"));
            for f in fields {
                c.arg(f.field.to_bytes()?).arg(f.value.to_bytes()?);
            }
        }
        KeyEdit::StreamDelete { ids } => {
            for id in ids {
                c.arg(id.as_str());
            }
        }
        KeyEdit::JsonSet { path, value } => {
            c.arg(path.as_str()).arg(value.to_string());
        }
    }
    Ok(c)
}

/// A finite score as Redis reads it back exactly (shortest round-trip decimal).
fn score_text(score: f64) -> String {
    format!("{score:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{BinaryText, KeyAddress, StreamField};

    fn args(c: &redis::Cmd) -> Vec<Vec<u8>> {
        c.args_iter()
            .map(|a| match a {
                redis::Arg::Simple(b) => b.to_vec(),
                _ => b"<cursor>".to_vec(),
            })
            .collect()
    }

    fn words(c: &redis::Cmd) -> Vec<String> {
        args(c)
            .iter()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect()
    }

    #[test]
    fn edits_become_commands() {
        let k = b"m7:k";
        let t = BinaryText::text;
        let cases: Vec<(KeyEdit, &str)> = vec![
            (
                KeyEdit::SetString {
                    value: t("v"),
                    keep_ttl: true,
                },
                "SET m7:k v KEEPTTL",
            ),
            (KeyEdit::Expire { ttl_ms: Some(1500) }, "PEXPIRE m7:k 1500"),
            (KeyEdit::Expire { ttl_ms: None }, "PERSIST m7:k"),
            (
                KeyEdit::Rename {
                    new_key: t("n"),
                    overwrite: false,
                },
                "RENAMENX m7:k n",
            ),
            (
                KeyEdit::Rename {
                    new_key: t("n"),
                    overwrite: true,
                },
                "RENAME m7:k n",
            ),
            (KeyEdit::Delete, "UNLINK m7:k"),
            (
                KeyEdit::HashSet {
                    field: t("f"),
                    value: t("v"),
                    only_new: true,
                },
                "HSETNX m7:k f v",
            ),
            (
                KeyEdit::HashDelete {
                    fields: vec![t("a"), t("b")],
                },
                "HDEL m7:k a b",
            ),
            (
                KeyEdit::ListPush {
                    end: ListEnd::Head,
                    values: vec![t("x")],
                },
                "LPUSH m7:k x",
            ),
            (
                KeyEdit::ListPush {
                    end: ListEnd::Tail,
                    values: vec![t("x"), t("y")],
                },
                "RPUSH m7:k x y",
            ),
            (
                KeyEdit::ListSet {
                    index: -1,
                    value: t("z"),
                    expected: None,
                },
                "LSET m7:k -1 z",
            ),
            (
                KeyEdit::ListRemove {
                    value: t("z"),
                    count: 0,
                },
                "LREM m7:k 0 z",
            ),
            (
                KeyEdit::ZsetAdd {
                    member: t("m"),
                    score: 2.5,
                },
                "ZADD m7:k 2.5 m",
            ),
            (
                KeyEdit::ZsetAdd {
                    member: t("m"),
                    score: 3.0,
                },
                "ZADD m7:k 3.0 m",
            ),
            (
                KeyEdit::StreamAdd {
                    id: None,
                    fields: vec![StreamField {
                        field: t("k"),
                        value: t("v"),
                    }],
                },
                "XADD m7:k * k v",
            ),
            (
                KeyEdit::StreamDelete {
                    ids: vec!["1-0".into()],
                },
                "XDEL m7:k 1-0",
            ),
            (
                KeyEdit::JsonSet {
                    path: "$.a".into(),
                    value: serde_json::json!({"b": [1, null]}),
                },
                "JSON.SET m7:k $.a {\"b\":[1,null]}",
            ),
        ];
        for (edit, expected) in cases {
            assert_eq!(words(&command(k, &edit).unwrap()).join(" "), expected);
        }
        let bin = KeyEdit::SetAdd {
            members: vec![BinaryText::from_bytes(&[0, 0xff])],
        };
        assert_eq!(args(&command(k, &bin).unwrap())[2], [0, 0xff]);
        assert_eq!(score_text(0.1), "0.1");
        assert_eq!(score_text(1e300), "1e300");
        assert_eq!(score_text(-7.25e-9), "-7.25e-9");
    }

    #[test]
    fn refusals_become_reports() {
        let r = report(Refusal::Conflict("changed".into()));
        assert!(!r.committed && r.conflict);
        assert_eq!(r.error.as_deref(), Some("changed"));
        let r = report(Refusal::Error("bad".into()));
        assert!(!r.committed && !r.conflict);
        assert!(is_unknown_command(&Error::sql(
            Some("ERR"),
            "unknown command 'JSON.SET'"
        )));
    }

    fn request(edits: Vec<KeyEdit>) -> KeyApplyRequest {
        KeyApplyRequest {
            address: KeyAddress::new("0", BinaryText::text("m7:k")),
            key_type: None,
            create: false,
            edits,
            confirm_destructive: false,
        }
    }

    #[test]
    fn request_shapes_that_would_fail_half_way_are_refused() {
        let t = BinaryText::text;
        let zero = request(vec![KeyEdit::Expire { ttl_ms: Some(0) }]);
        let err = request_checks(&zero).unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);
        let confirmed = KeyApplyRequest {
            confirm_destructive: true,
            ..zero
        };
        assert!(request_checks(&confirmed).is_ok());
        assert!(request_checks(&request(vec![KeyEdit::Expire { ttl_ms: Some(1) }])).is_ok());
        assert!(
            request_checks(&request(vec![KeyEdit::Expire {
                ttl_ms: Some(MAX_TTL_MS + 1)
            }]))
            .is_err()
        );
        assert!(
            request_checks(&request(vec![KeyEdit::Expire {
                ttl_ms: Some(u64::MAX)
            }]))
            .is_err()
        );
        let set = KeyEdit::ListSet {
            index: 0,
            value: t("x"),
            expected: None,
        };
        let push = KeyEdit::ListPush {
            end: ListEnd::Head,
            values: vec![t("y")],
        };
        let remove = KeyEdit::ListRemove {
            value: t("y"),
            count: 1,
        };
        assert!(request_checks(&request(vec![set.clone(), push.clone()])).is_ok());
        assert!(request_checks(&request(vec![push, set.clone()])).is_err());
        assert!(request_checks(&request(vec![remove, set])).is_err());
        let add = |id: Option<&str>| KeyEdit::StreamAdd {
            id: id.map(str::to_owned),
            fields: vec![StreamField {
                field: t("f"),
                value: t("v"),
            }],
        };
        assert!(request_checks(&request(vec![add(Some("5-1")), add(None)])).is_ok());
        assert!(request_checks(&request(vec![add(None), add(Some("9-1"))])).is_err());
        assert!(request_checks(&request(vec![add(Some("*")), add(Some("9-*"))])).is_err());
    }

    #[test]
    fn stream_ids_must_grow_past_the_last_generated_one() {
        assert_eq!(new_id("5-1"), Some(NewId::Exact((5, 1))));
        assert_eq!(new_id("7"), Some(NewId::Exact((7, 0))));
        assert_eq!(new_id("7-*"), Some(NewId::Ms(7)));
        assert_eq!(new_id("x-*"), None);
        assert_eq!(new_id("1-x"), None);
        assert_eq!(next_id(NewId::Ms(5), (5, 3)), (5, 4));
        assert_eq!(next_id(NewId::Ms(6), (5, 3)), (6, 0));
        assert_eq!(next_id(NewId::Ms(0), (0, 0)), (0, 1));
        // Below the top: no growth, so the check refuses it.
        assert!(next_id(NewId::Ms(4), (5, 3)) <= (5, 3));
        assert!(next_id(NewId::Ms(5), (5, u64::MAX)) <= (5, u64::MAX));
        assert!(next_id(NewId::Exact((0, 0)), (0, 0)) <= (0, 0));
    }

    #[test]
    fn json_paths_know_their_parent() {
        assert!(is_json_root("$") && is_json_root("."));
        assert!(!is_json_root("$.a"));
        assert_eq!(json_parent("$.a"), Some("$"));
        assert_eq!(json_parent("$.a.b_c"), Some("$.a"));
        assert_eq!(json_parent("$['k']"), Some("$"));
        assert_eq!(json_parent("$.a[\"k k\"]"), Some("$.a"));
        assert_eq!(json_parent(".a"), Some("."));
        assert_eq!(json_parent("a"), Some("."));
        assert_eq!(json_parent("$[0]"), None);
        assert_eq!(json_parent("$.a[*]"), None);
        assert_eq!(json_parent("$..a"), None);
        assert_eq!(json_parent("$.*"), None);
        assert_eq!(json_parent("$"), None);
        let resp3 = Value::Array(vec![Value::Array(vec![Value::BulkString(
            b"object".to_vec(),
        )])]);
        assert_eq!(json_type_names(resp3), ["object"]);
        assert_eq!(
            json_type_names(Value::Array(Vec::new())),
            Vec::<String>::new()
        );
        assert_eq!(json_type_names(Value::Nil), Vec::<String>::new());
        assert_eq!(
            json_type_names(Value::SimpleString("integer".into())),
            ["integer"]
        );
    }

    #[test]
    fn failures_inside_exec_are_named() {
        let edits = vec![
            KeyEdit::Expire { ttl_ms: None },
            KeyEdit::JsonSet {
                path: "$.a.b".into(),
                value: serde_json::json!(1),
            },
        ];
        let ok = [Value::Int(1), Value::Okay];
        assert_eq!(failures(&ok, &edits), Vec::<String>::new());
        let nil = [Value::Int(1), Value::Nil];
        assert_eq!(
            failures(&nil, &edits),
            ["JSON.SET: the path $.a.b does not exist"]
        );
    }
}
