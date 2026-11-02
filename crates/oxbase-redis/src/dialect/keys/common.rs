//! Helpers the Redis cells share: the native session behind a `dyn Connection`, reply readers
//! for the `redis::Value` shapes of RESP2 and RESP3, key scans over every master, console text
//! of commands and Arrow field builders.

use arrow_schema::{DataType, Field};
use oxbase_dialect::keyvalue::META_ENCODING;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::keyvalue::{escape_bytes, to_hex};
use oxbase_types::{Endpoint, Error, KeyType, Result};
use redis::Value;
use std::collections::HashMap;

use crate::driver::{RedisConnection, RedisHandle};

/// Arrow field metadata: extension name (`arrow.json` columns).
pub(crate) const META_EXTENSION_NAME: &str = "ARROW:extension:name";
/// The JSON extension name.
pub(crate) const JSON_EXTENSION: &str = "arrow.json";
/// Arrow field metadata: the RESP / Redis type of the column's values.
pub(crate) const META_NATIVE_TYPE: &str = "oxbase.native_type";
/// Concurrent single-key commands on a cluster (where one pipeline cannot span slots).
pub(crate) const CLUSTER_FAN_OUT: usize = 64;

/// The concrete Redis session behind `c` (cells are only called with their own engine's
/// connections).
pub(crate) fn native(c: &dyn Connection) -> Result<&RedisConnection> {
    c.as_any()
        .and_then(|any| any.downcast_ref::<RedisConnection>())
        .ok_or_else(|| Error::internal("the Redis cells need a Redis connection"))
}

/// `Cancelled` when `cancel` fired.
pub(crate) fn check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

// ---- reply readers ----------------------------------------------------------------------------

/// The bytes of a string-like reply (bulk, simple, verbatim strings; integers and doubles as
/// their decimal text); `None` for nil and aggregates.
pub(crate) fn bytes_of(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::BulkString(b) => Some(b.clone()),
        Value::SimpleString(s) => Some(s.as_bytes().to_vec()),
        Value::VerbatimString { text, .. } => Some(text.as_bytes().to_vec()),
        Value::Okay => Some(b"OK".to_vec()),
        Value::Int(i) => Some(i.to_string().into_bytes()),
        Value::Double(d) => Some(d.to_string().into_bytes()),
        Value::Attribute { data, .. } => bytes_of(data),
        _ => None,
    }
}

/// [`bytes_of`] taking the value (no copy for bulk strings).
pub(crate) fn into_bytes(value: Value) -> Option<Vec<u8>> {
    match value {
        Value::BulkString(b) => Some(b),
        Value::Attribute { data, .. } => into_bytes(*data),
        other => bytes_of(&other),
    }
}

/// A string-like reply as UTF-8 text (lossy).
pub(crate) fn text_of(value: &Value) -> Option<String> {
    bytes_of(value).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// An integer reply (or a string holding one).
pub(crate) fn int_of(value: &Value) -> Option<i64> {
    match value {
        Value::Int(i) => Some(*i),
        Value::Attribute { data, .. } => int_of(data),
        other => text_of(other).and_then(|t| t.trim().parse().ok()),
    }
}

/// A floating-point reply (RESP3 double, or the text of a RESP2 score).
pub(crate) fn float_of(value: &Value) -> Option<f64> {
    match value {
        Value::Double(d) => Some(*d),
        Value::Int(i) => text_of(&Value::Int(*i)).and_then(|t| t.parse().ok()),
        Value::Attribute { data, .. } => float_of(data),
        other => text_of(other).and_then(|t| parse_score(t.trim())),
    }
}

/// A Redis score text (`inf`, `-inf`, `+inf` included).
pub(crate) fn parse_score(text: &str) -> Option<f64> {
    match text.to_ascii_lowercase().as_str() {
        "inf" | "+inf" => Some(f64::INFINITY),
        "-inf" => Some(f64::NEG_INFINITY),
        other => other.parse().ok(),
    }
}

/// The elements of an aggregate reply: arrays and sets as they are, maps flattened to `key,
/// value, ...`, nil as empty; anything else as a one-element list.
pub(crate) fn items(value: Value) -> Vec<Value> {
    match value {
        Value::Array(v) | Value::Set(v) => v,
        Value::Map(pairs) => pairs.into_iter().flat_map(|(k, v)| [k, v]).collect(),
        Value::Nil => Vec::new(),
        Value::Attribute { data, .. } => items(*data),
        other => vec![other],
    }
}

/// Pairs of an aggregate reply: a RESP3 map, an array of two-element arrays (RESP3
/// `WITHSCORES`), or a flat RESP2 array `k1, v1, k2, v2`.
pub(crate) fn pairs(value: Value) -> Vec<(Value, Value)> {
    match value {
        Value::Map(pairs) => pairs,
        Value::Attribute { data, .. } => pairs(*data),
        other => {
            let list = items(other);
            if !list.is_empty()
                && list
                    .iter()
                    .all(|v| matches!(v, Value::Array(a) if a.len() == 2))
            {
                list.into_iter()
                    .filter_map(|v| {
                        let mut pair = items(v).into_iter();
                        Some((pair.next()?, pair.next()?))
                    })
                    .collect()
            } else {
                let mut out = Vec::with_capacity(list.len() / 2);
                let mut it = list.into_iter();
                while let (Some(k), Some(v)) = (it.next(), it.next()) {
                    out.push((k, v));
                }
                out
            }
        }
    }
}

/// A `SCAN`-family reply: the next cursor and the returned elements.
pub(crate) fn scan_reply(value: Value) -> Result<(Vec<u8>, Vec<Value>)> {
    let mut parts = items(value).into_iter();
    let cursor = parts.next().and_then(into_bytes);
    let elements = parts.next().map(items);
    match (cursor, elements) {
        (Some(cursor), Some(elements)) => Ok((cursor, elements)),
        _ => Err(Error::internal(
            "a SCAN reply did not hold a cursor and a list",
        )),
    }
}

/// `true` for the cursor that ends a scan.
pub(crate) fn scan_done(cursor: &[u8]) -> bool {
    cursor == b"0"
}

/// A RESP3 map or RESP2 flat array of text keys, as a lookup table.
pub(crate) fn map_of(value: Value) -> HashMap<String, Value> {
    pairs(value)
        .into_iter()
        .filter_map(|(k, v)| text_of(&k).map(|k| (k, v)))
        .collect()
}

// ---- commands -----------------------------------------------------------------------------------

/// A command from its words and arguments.
pub(crate) fn cmd(name: &str) -> redis::Cmd {
    let mut c = redis::Cmd::new();
    for word in name.split(' ') {
        c.arg(word);
    }
    c
}

/// Escapes the glob characters of `prefix` (`*`, `?`, `[`, `]`, `\`) for `SCAN ... MATCH`.
pub(crate) fn escape_glob(prefix: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix.len() + 1);
    for &b in prefix {
        if matches!(b, b'*' | b'?' | b'[' | b']' | b'\\') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out
}

/// One argument as the console would read it back: bare when it is plain printable text,
/// else double-quoted with `redis-cli` escapes.
pub(crate) fn console_arg(arg: &[u8]) -> String {
    let plain = !arg.is_empty()
        && arg
            .iter()
            .all(|&b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\'' | b'\\'));
    if plain {
        String::from_utf8_lossy(arg).into_owned()
    } else if let Ok(text) = std::str::from_utf8(arg)
        && !text.chars().any(char::is_control)
        && !text.contains(['"', '\\'])
    {
        format!("\"{text}\"")
    } else {
        format!("\"{}\"", escape_bytes(arg))
    }
}

/// How a key's bytes are named in the tree: the UTF-8 text, else `redis-cli` escapes.
pub(crate) fn key_name(key: &[u8]) -> String {
    match std::str::from_utf8(key) {
        Ok(text) => text.to_owned(),
        Err(_) => escape_bytes(key),
    }
}

/// The bytes behind an escaped name (`\xHH`, `\\`, `\"`, `\n`, `\r`, `\t`), when `name` is in
/// that form; `None` when it has no escapes or a malformed one.
pub(crate) fn unescape_name(name: &str) -> Option<Vec<u8>> {
    if !name.contains('\\') {
        return None;
    }
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        match bytes.get(i + 1)? {
            b'\\' => out.push(b'\\'),
            b'"' => out.push(b'"'),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'x' => {
                let hex = std::str::from_utf8(bytes.get(i + 2..i + 4)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            _ => return None,
        }
        i += 2;
    }
    Some(out)
}

/// The key type a `TYPE` reply names; `None` for `none` (no such key).
pub(crate) fn key_type_of(reply: &Value) -> Option<KeyType> {
    text_of(reply).and_then(|t| KeyType::from_type_reply(&t))
}

// ---- Arrow -----------------------------------------------------------------------------------

/// A `Binary` column of user bytes (`oxbase.encoding = binary`).
pub(crate) fn binary_field(name: &str, nullable: bool) -> Field {
    Field::new(name, DataType::Binary, nullable).with_metadata(HashMap::from([
        (META_ENCODING.to_owned(), "binary".to_owned()),
        (META_NATIVE_TYPE.to_owned(), "string".to_owned()),
    ]))
}

/// A `Utf8` column holding JSON text (`arrow.json`).
pub(crate) fn json_field(name: &str, nullable: bool) -> Field {
    Field::new(name, DataType::Utf8, nullable).with_metadata(HashMap::from([(
        META_EXTENSION_NAME.to_owned(),
        JSON_EXTENSION.to_owned(),
    )]))
}

/// Bytes as JSON: a string when UTF-8, else `{"hex": "..."}`.
pub(crate) fn json_bytes(bytes: &[u8]) -> serde_json::Value {
    match std::str::from_utf8(bytes) {
        Ok(text) => serde_json::Value::String(text.to_owned()),
        Err(_) => serde_json::json!({ "hex": to_hex(bytes) }),
    }
}

// ---- key scans --------------------------------------------------------------------------------

/// Where scans run: the masters of a cluster, else the one server.
pub(crate) struct Nodes {
    pub handle: RedisHandle,
    pub nodes: Vec<Endpoint>,
    pub cluster: bool,
}

impl Nodes {
    /// The nodes of `c`'s session (`CLUSTER SLOTS` on a cluster).
    pub(crate) async fn of(c: &RedisConnection, cancel: &CancellationToken) -> Result<Self> {
        let handle = c.handle();
        let nodes = if c.is_cluster() {
            handle.masters(cancel).await?
        } else {
            // `command_on` ignores the node outside a cluster.
            vec![
                c.params()
                    .server_endpoint()
                    .unwrap_or_else(|| Endpoint::new("", 0)),
            ]
        };
        Ok(Self {
            handle,
            nodes,
            cluster: c.is_cluster(),
        })
    }

    /// One `SCAN` round trip on node `index`: the next cursor and the keys.
    pub(crate) async fn scan(
        &self,
        index: usize,
        cursor: &[u8],
        pattern: &[u8],
        count: u32,
        key_type: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
        let mut c = redis::cmd("SCAN");
        c.arg(cursor)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg(count);
        if let Some(t) = key_type {
            c.arg("TYPE").arg(t);
        }
        let reply = self
            .handle
            .command_on(&self.nodes[index], &c, cancel)
            .await?;
        let (cursor, keys) = scan_reply(reply)?;
        Ok((cursor, keys.into_iter().filter_map(into_bytes).collect()))
    }

    /// `TYPE` and `PTTL` of every key, in order: `None` for a key that is gone. One pipeline
    /// on one server; single commands side by side on a cluster (a pipeline cannot span slots).
    pub(crate) async fn types_and_ttls(
        &self,
        keys: &[Vec<u8>],
        cancel: &CancellationToken,
    ) -> Result<Vec<Option<(KeyType, Option<u64>)>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let replies: Vec<(Value, Value)> = if self.cluster {
            // Side by side in groups (no closure over references: the futures must be `Send`
            // for every lifetime).
            let mut replies = Vec::with_capacity(keys.len());
            for chunk in keys.chunks(CLUSTER_FAN_OUT) {
                let mut pending = Vec::with_capacity(chunk.len());
                for key in chunk {
                    let mut t = redis::cmd("TYPE");
                    t.arg(key.as_slice());
                    let mut p = redis::cmd("PTTL");
                    p.arg(key.as_slice());
                    let handle = &self.handle;
                    pending.push(async move {
                        let t = handle.command(&t, cancel).await?;
                        let p = handle.command(&p, cancel).await?;
                        Ok::<_, Error>((t, p))
                    });
                }
                for reply in futures::future::join_all(pending).await {
                    replies.push(reply?);
                }
            }
            replies
        } else {
            let mut pipe = redis::pipe();
            for key in keys {
                pipe.cmd("TYPE").arg(key.as_slice());
                pipe.cmd("PTTL").arg(key.as_slice());
            }
            let mut replies = self.handle.pipeline(&pipe, cancel).await?.into_iter();
            let mut out = Vec::with_capacity(keys.len());
            while let (Some(t), Some(p)) = (replies.next(), replies.next()) {
                out.push((t, p));
            }
            out
        };
        Ok(replies
            .into_iter()
            .map(|(t, p)| {
                let key_type = key_type_of(&t)?;
                let ttl = int_of(&p)?;
                if ttl == -2 {
                    return None;
                }
                Some((key_type, u64::try_from(ttl).ok()))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_read_in_both_protocols() {
        let resp2 = Value::Array(vec![
            Value::BulkString(b"a".to_vec()),
            Value::BulkString(b"1".to_vec()),
            Value::BulkString(b"b".to_vec()),
            Value::BulkString(b"2.5".to_vec()),
        ]);
        let resp3 = Value::Array(vec![
            Value::Array(vec![Value::BulkString(b"a".to_vec()), Value::Double(1.0)]),
            Value::Array(vec![Value::BulkString(b"b".to_vec()), Value::Double(2.5)]),
        ]);
        for reply in [resp2, resp3] {
            let p = pairs(reply);
            assert_eq!(p.len(), 2);
            assert_eq!(bytes_of(&p[1].0).unwrap(), b"b");
            assert_eq!(float_of(&p[1].1), Some(2.5));
        }
        let map = Value::Map(vec![(
            Value::SimpleString("databases".into()),
            Value::BulkString(b"16".to_vec()),
        )]);
        assert_eq!(int_of(&map_of(map)["databases"]), Some(16));
        assert_eq!(parse_score("-inf"), Some(f64::NEG_INFINITY));
        assert_eq!(
            float_of(&Value::BulkString(b"+inf".to_vec())),
            Some(f64::INFINITY)
        );
        let scan = Value::Array(vec![
            Value::BulkString(b"17".to_vec()),
            Value::Array(vec![Value::BulkString(b"k".to_vec())]),
        ]);
        let (cursor, keys) = scan_reply(scan).unwrap();
        assert_eq!(cursor, b"17");
        assert!(!scan_done(&cursor));
        assert_eq!(keys.len(), 1);
        assert!(scan_reply(Value::Nil).is_err());
    }

    #[test]
    fn globs_and_console_args_are_escaped() {
        assert_eq!(escape_glob(b"a*b?[c]\\"), b"a\\*b\\?\\[c\\]\\\\".to_vec());
        assert_eq!(console_arg(b"m7:hash"), "m7:hash");
        assert_eq!(console_arg(b"two words"), "\"two words\"");
        assert_eq!(console_arg(b""), "\"\"");
        assert_eq!(console_arg(&[0, 0xff, b'"']), "\"\\x00\\xff\\\"\"");
        let parsed =
            crate::command::parse_command_line(&format!("GET {}", console_arg(&[0, 0xff, b'"'])))
                .unwrap();
        assert_eq!(parsed[1], [0, 0xff, b'"']);
    }

    #[test]
    fn names_of_binary_keys_round_trip() {
        let raw = [b'k', 0x00, 0xff, b'\\', b'"', b'\n'];
        let name = key_name(&raw);
        assert_eq!(name, "k\\x00\\xff\\\\\\\"\\n");
        assert_eq!(unescape_name(&name).unwrap(), raw);
        assert_eq!(key_name(b"plain"), "plain");
        assert_eq!(unescape_name("plain"), None);
        assert_eq!(unescape_name("bad\\q"), None);
        assert_eq!(unescape_name("cut\\x0"), None);
    }

    #[test]
    fn fields_carry_their_metadata() {
        let f = binary_field("key", false);
        assert_eq!(f.metadata()[META_ENCODING], "binary");
        assert_eq!(f.data_type(), &DataType::Binary);
        let j = json_field("fields", false);
        assert_eq!(j.metadata()[META_EXTENSION_NAME], JSON_EXTENSION);
        assert_eq!(json_bytes(&[0xff]), serde_json::json!({"hex": "ff"}));
        assert_eq!(json_bytes(b"v"), serde_json::json!("v"));
        assert_eq!(key_type_of(&Value::SimpleString("none".into())), None);
        assert_eq!(
            key_type_of(&Value::SimpleString("ReJSON-RL".into())),
            Some(KeyType::Json)
        );
    }
}
