//! Atomic key edits (`KeyValueStore::apply`, docs/m7-contracts.md §5.2).
//!
//! 1. `WATCH key` (and the rename target), on a dedicated connection like the session's
//!    (standalone, sentinel) or on the session itself on a cluster (the key's node; one slot).
//! 2. Checks, before anything is written: `TYPE` (missing, existing with `create`, another
//!    type than the editor read or than the edits need → conflict), `LINDEX` for `ListSet`
//!    (another value than `expected` → conflict; out of range → error), `EXISTS` of a
//!    `RENAMENX` target, and the last entry id for `XADD` with explicit ids. They exclude the
//!    errors that `EXEC` would only report after earlier edits were applied (Redis does not
//!    roll back).
//! 3. `MULTI`, one command per edit, `EXEC`. A nil `EXEC` (a watched key changed) is a
//!    conflict and nothing was written; an error while queuing (an unknown command) aborts
//!    everything too.

use futures::StreamExt;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{Error, KeyApplyReport, KeyApplyRequest, KeyEdit, KeyType, ListEnd, Result};
use redis::Value;

use super::common::{CLUSTER_FAN_OUT, bytes_of, check, int_of, items, key_type_of, text_of};
use super::source::parse_stream_id;
use crate::driver::{DedicatedConnection, RedisConnection};

/// Where the `WATCH` / `MULTI` / `EXEC` of one apply runs.
enum Link<'a> {
    Dedicated(DedicatedConnection),
    Session(&'a RedisConnection),
}

impl Link<'_> {
    async fn open<'a>(c: &'a RedisConnection, cancel: &CancellationToken) -> Result<Link<'a>> {
        if c.is_cluster() {
            return Ok(Link::Session(c));
        }
        let config = redis::AsyncConnectionConfig::new().set_response_timeout(None);
        Ok(Link::Dedicated(c.dedicated(config, cancel).await?))
    }

    async fn command(&self, c: &redis::Cmd, cancel: &CancellationToken) -> Result<Value> {
        match self {
            Link::Session(s) => s.command(c, cancel).await,
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

    /// Runs the atomic pipeline: `Ok(None)` when `EXEC` answered nil (a watched key changed).
    async fn exec(
        &self,
        pipe: &redis::Pipeline,
        cancel: &CancellationToken,
    ) -> Result<Option<Vec<Value>>> {
        match self {
            Link::Session(s) => match s.pipeline(pipe, cancel).await {
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

    /// Drops the `WATCH` of an apply that stops before `EXEC` (a dedicated connection just
    /// closes).
    async fn unwatch(&self, cancel: &CancellationToken) {
        if let Link::Session(s) = self {
            let _ = s.command(&redis::cmd("UNWATCH"), cancel).await;
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
/// without its `info` (the caller reads the key back).
pub(crate) async fn apply(
    c: &RedisConnection,
    request: &KeyApplyRequest,
    cancel: &CancellationToken,
) -> Result<KeyApplyReport> {
    check(cancel)?;
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
    let link = Link::open(c, cancel).await?;
    let mut watch = redis::cmd("WATCH");
    watch.arg(key.as_slice());
    if let Some((new_key, _)) = &rename
        && *new_key != key
    {
        watch.arg(new_key.as_slice());
    }
    link.command(&watch, cancel).await?;

    let refusal = match checks(&link, c, &key, rename.as_ref(), request, cancel).await {
        Ok(refusal) => refusal,
        Err(e) => {
            link.unwatch(cancel).await;
            return Err(e);
        }
    };
    if let Some(refusal) = refusal {
        link.unwatch(cancel).await;
        return Ok(report(refusal));
    }

    let mut pipe = redis::pipe();
    pipe.atomic().ignore_errors();
    for edit in &request.edits {
        pipe.add_command(command(&key, edit)?);
    }
    let replies = match link.exec(&pipe, cancel).await {
        Ok(Some(replies)) => replies,
        Ok(None) => {
            return Ok(report(Refusal::Conflict(
                "the key changed while the changes were being applied; nothing was written"
                    .to_owned(),
            )));
        }
        Err(e) if e.is_cancelled() => return Err(e),
        Err(e) => {
            link.unwatch(cancel).await;
            if is_unknown_command(&e)
                && request
                    .edits
                    .iter()
                    .any(|e| matches!(e, KeyEdit::JsonSet { .. }))
            {
                return Err(Error::unsupported(
                    "JSON values need the RedisJSON module, which this server does not have",
                ));
            }
            return Ok(report(Refusal::Error(format!("nothing was written: {e}"))));
        }
    };
    let failed: Vec<String> = replies
        .iter()
        .zip(&request.edits)
        .filter_map(|(reply, edit)| match reply {
            Value::ServerError(e) => Some(format!("{}: {}", edit_name(edit), server_error(e))),
            _ => None,
        })
        .collect();
    if !failed.is_empty() {
        let applied = request.edits.len() - failed.len();
        return Ok(report(Refusal::Error(format!(
            "{} (the other {applied} change(s) were applied)",
            failed.join("; ")
        ))));
    }
    Ok(KeyApplyReport {
        committed: true,
        conflict: false,
        error: None,
        info: None,
    })
}

fn is_unknown_command(e: &Error) -> bool {
    let text = e.to_string().to_ascii_lowercase();
    text.contains("unknown command")
}

/// The checks of step 2 of the module docs; `Some` when nothing may be written.
async fn checks(
    link: &Link<'_>,
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

    if let Some(refusal) = list_checks(link, key, request, cancel).await? {
        return Ok(Some(refusal));
    }
    if let Some(refusal) = stream_checks(link, key, request, cancel).await? {
        return Ok(Some(refusal));
    }
    if effective == KeyType::String
        && request
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
    Ok(None)
}

/// `LINDEX` for every `ListSet` (side by side): another value than `expected` is a conflict,
/// a missing element an error unless an earlier edit of the request pushed values.
async fn list_checks(
    link: &Link<'_>,
    key: &[u8],
    request: &KeyApplyRequest,
    cancel: &CancellationToken,
) -> Result<Option<Refusal>> {
    let mut sets = Vec::new();
    let mut pushed = false;
    for edit in &request.edits {
        match edit {
            KeyEdit::ListPush { .. } => pushed = true,
            KeyEdit::ListSet {
                index, expected, ..
            } => sets.push((*index, expected.clone(), pushed)),
            _ => {}
        }
    }
    if sets.is_empty() {
        return Ok(None);
    }
    let replies: Vec<Result<Value>> = futures::stream::iter(&sets)
        .map(|(index, _, _)| async move {
            let mut c = redis::cmd("LINDEX");
            c.arg(key).arg(*index);
            link.command(&c, cancel).await
        })
        .buffered(CLUSTER_FAN_OUT)
        .collect()
        .await;
    for ((index, expected, pushed), reply) in sets.into_iter().zip(replies) {
        let current = bytes_of(&reply?);
        match (current, expected) {
            (None, _) if !pushed => {
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

/// Explicit `XADD` ids must grow: past the stream's last entry and from one edit to the next.
async fn stream_checks(
    link: &Link<'_>,
    key: &[u8],
    request: &KeyApplyRequest,
    cancel: &CancellationToken,
) -> Result<Option<Refusal>> {
    let explicit: Vec<&str> = request
        .edits
        .iter()
        .filter_map(|e| match e {
            KeyEdit::StreamAdd { id: Some(id), .. } if id != "*" && !id.ends_with("-*") => {
                Some(id.as_str())
            }
            _ => None,
        })
        .collect();
    if explicit.is_empty() {
        return Ok(None);
    }
    let mut last = redis::cmd("XREVRANGE");
    last.arg(key).arg("+").arg("-").arg("COUNT").arg(1);
    let mut top = items(link.command(&last, cancel).await?)
        .into_iter()
        .next()
        .and_then(|entry| items(entry).into_iter().next())
        .and_then(|id| text_of(&id))
        .and_then(|id| parse_stream_id(&id))
        .unwrap_or((0, 0));
    for id in explicit {
        let Some(parsed) = parse_stream_id(id) else {
            return Ok(Some(Refusal::Error(format!(
                "'{id}' is not a stream entry id (milliseconds-sequence)"
            ))));
        };
        if parsed <= top {
            return Ok(Some(Refusal::Error(format!(
                "the entry id {id} must be greater than the stream's last id {}-{}",
                top.0, top.1
            ))));
        }
        top = parsed;
    }
    Ok(None)
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
    use oxbase_types::{BinaryText, StreamField};

    fn args(c: &redis::Cmd) -> Vec<Vec<u8>> {
        c.args_iter()
            .map(|a| match a {
                redis::Arg::Simple(b) => b.to_vec(),
                redis::Arg::Cursor => b"<cursor>".to_vec(),
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
}
