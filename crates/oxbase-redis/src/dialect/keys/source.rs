//! The two result sources of the key-value cell: the key browser's `SCAN` over every key
//! ([`ScanSource`]) and the chunked read of one key's value ([`ValueSource`]). Both are lazy
//! streams: a round trip is only made when the consumer asks for the next batch, so dropping
//! the stream (`maxRows`, the disk cap, a closed run) stops the reads, and `cancel` ends them
//! with one `Err(Cancelled)`.

use std::collections::HashSet;
use std::sync::Arc;

use arrow_array::{ArrayRef, BinaryArray, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use oxbase_dialect::keyvalue::{SCAN_DEDUP_LIMIT, VALUE_CHUNK};
use oxbase_driver::{
    CancellationToken, Connection, Notice, ResultItem, ResultSource, ResultStream, from_arrow,
};
use oxbase_types::{Error, KeyScanRequest, KeyType, Result};
use redis::Value;

use super::common::{
    Nodes, binary_field, check, console_arg, float_of, int_of, into_bytes, items, json_bytes,
    json_field, key_name, native, pairs, scan_done, scan_reply, text_of,
};
use crate::driver::RedisHandle;

/// Strings longer than this many bytes are read with `GETRANGE` in pieces of
/// [`STRING_PIECE`] bytes.
const STRING_PIECE: usize = 1024 * 1024;
/// At most this many bytes of a string value are read (a warning notice says when cut).
pub(crate) const STRING_READ_LIMIT: usize = 64 * 1024 * 1024;

fn chunk_i64() -> i64 {
    i64::try_from(VALUE_CHUNK).unwrap_or(1000)
}

/// One step of a lazy reader: items to emit, then whether the stream goes on.
type Step = (Vec<Result<ResultItem>>, bool);

/// Runs `step` until it says stop, as a `ResultStream` that starts with `schema`, checks
/// `cancel` before every round trip and ends after the first error.
fn lazy<S, F, Fut>(schema: SchemaRef, state: S, cancel: CancellationToken, step: F) -> ResultStream
where
    S: Send + 'static,
    F: Fn(S, CancellationToken) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = (S, Result<Step>)> + Send + 'static,
{
    let head = stream::iter([Ok(ResultItem::Schema(schema))]);
    let body: BoxStream<'static, Vec<Result<ResultItem>>> =
        stream::unfold(Some(state), move |state| {
            let cancel = cancel.clone();
            let next = state.map(|s| step(s, cancel.clone()));
            async move {
                let next = next?;
                if cancel.is_cancelled() {
                    return Some((vec![Err(Error::Cancelled)], None));
                }
                let (state, outcome) = next.await;
                match outcome {
                    Ok((items, more)) => Some((items, more.then_some(state))),
                    Err(e) => {
                        let e = if cancel.is_cancelled() {
                            Error::Cancelled
                        } else {
                            e
                        };
                        Some((vec![Err(e)], None))
                    }
                }
            }
        })
        .boxed();
    head.chain(body.flat_map(stream::iter)).boxed()
}

fn batch(schema: &SchemaRef, columns: Vec<ArrayRef>) -> Result<ResultItem> {
    RecordBatch::try_new(schema.clone(), columns)
        .map(ResultItem::Batch)
        .map_err(from_arrow)
}

// ---- key browser scan -------------------------------------------------------------------------

/// `SCAN` of one logical database (every master of a cluster). See `KeyValueStore::scan`.
#[derive(Debug, Clone)]
pub(crate) struct ScanSource {
    pub request: KeyScanRequest,
}

pub(crate) fn scan_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        binary_field("key", false),
        Field::new("key_text", DataType::Utf8, false),
        Field::new("type", DataType::Utf8, false),
        Field::new("ttl_ms", DataType::Int64, true),
    ]))
}

/// The scan as console text (`SCAN 0 MATCH m7:* COUNT 1000 TYPE hash`).
pub(crate) fn scan_text(request: &KeyScanRequest) -> String {
    let mut text = format!(
        "SCAN 0 MATCH {} COUNT {}",
        console_arg(request.pattern.as_deref().unwrap_or("*").as_bytes()),
        request.count_hint()
    );
    if let Some(name) = request.key_type.and_then(KeyType::scan_type_name) {
        text.push_str(" TYPE ");
        text.push_str(name);
    }
    text
}

struct ScanState {
    nodes: Nodes,
    node: usize,
    cursor: Vec<u8>,
    pattern: Vec<u8>,
    count: u32,
    key_type: Option<KeyType>,
    server_type_filter: bool,
    limit: Option<u64>,
    emitted: u64,
    seen: HashSet<Vec<u8>>,
    schema: SchemaRef,
}

impl ScanState {
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the lazy stream hands each step its own token"
    )]
    async fn step(mut self, cancel: CancellationToken) -> (Self, Result<Step>) {
        let outcome = self.next(&cancel).await;
        (self, outcome)
    }

    /// Scans until some keys are found or the scan ends.
    async fn next(&mut self, cancel: &CancellationToken) -> Result<Step> {
        loop {
            check(cancel)?;
            if self.node >= self.nodes.endpoints.len()
                || self.limit.is_some_and(|l| self.emitted >= l)
            {
                return Ok((Vec::new(), false));
            }
            let type_name = if self.server_type_filter {
                self.key_type.and_then(KeyType::scan_type_name)
            } else {
                None
            };
            let (next, keys) = self
                .nodes
                .scan(
                    self.node,
                    &self.cursor,
                    &self.pattern,
                    self.count,
                    type_name,
                    cancel,
                )
                .await?;
            if scan_done(&next) {
                self.node += 1;
                self.cursor = b"0".to_vec();
            } else {
                self.cursor = next;
            }
            let mut fresh = Vec::with_capacity(keys.len());
            for key in keys {
                if self.seen.contains(&key) {
                    continue;
                }
                if self.seen.len() < SCAN_DEDUP_LIMIT {
                    self.seen.insert(key.clone());
                }
                fresh.push(key);
            }
            let infos = self.nodes.types_and_ttls(&fresh, cancel).await?;
            let mut rows = Vec::new();
            for (key, info) in fresh.into_iter().zip(infos) {
                let Some((key_type, ttl)) = info else {
                    continue;
                };
                if self.key_type.is_some_and(|t| t != key_type) {
                    continue;
                }
                if self.limit.is_some_and(|l| self.emitted >= l) {
                    break;
                }
                self.emitted += 1;
                rows.push((key, key_type, ttl));
            }
            let more = self.node < self.nodes.endpoints.len()
                && self.limit.is_none_or(|l| self.emitted < l);
            if rows.is_empty() {
                if more {
                    continue;
                }
                return Ok((Vec::new(), false));
            }
            let columns: Vec<ArrayRef> = vec![
                Arc::new(BinaryArray::from_iter_values(
                    rows.iter().map(|(k, _, _)| k.as_slice()),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|(k, _, _)| key_name(k)),
                )),
                Arc::new(StringArray::from_iter_values(
                    rows.iter().map(|(_, t, _)| t.as_str()),
                )),
                Arc::new(Int64Array::from_iter(
                    rows.iter()
                        .map(|(_, _, ttl)| ttl.and_then(|t| i64::try_from(t).ok())),
                )),
            ];
            return Ok((vec![batch(&self.schema, columns)], more));
        }
    }
}

#[async_trait]
impl ResultSource for ScanSource {
    async fn open(&self, c: &dyn Connection, cancel: CancellationToken) -> Result<ResultStream> {
        check(&cancel)?;
        let redis = native(c)?;
        let nodes = Nodes::of(redis, &cancel).await?;
        let state = ScanState {
            nodes,
            node: 0,
            cursor: b"0".to_vec(),
            pattern: self
                .request
                .pattern
                .as_deref()
                .unwrap_or("*")
                .as_bytes()
                .to_vec(),
            count: self.request.count_hint(),
            key_type: self.request.key_type,
            // `SCAN ... TYPE` arrived in Redis 6.0; older servers are filtered here.
            server_type_filter: c.server_version().at_least(6, 0),
            limit: self.request.limit,
            emitted: 0,
            seen: HashSet::new(),
            schema: scan_schema(),
        };
        Ok(lazy(scan_schema(), state, cancel, ScanState::step))
    }
}

// ---- value reads ------------------------------------------------------------------------------

/// The chunked read of one key's whole value. See `KeyValueStore::read_value`.
#[derive(Debug, Clone)]
pub(crate) struct ValueSource {
    pub key: Vec<u8>,
    pub key_type: KeyType,
}

/// The columns of a value read of `key_type` (`None` for [`KeyType::Other`]).
pub(crate) fn value_schema(key_type: KeyType) -> Option<SchemaRef> {
    let fields = match key_type {
        KeyType::String => vec![binary_field("value", false)],
        KeyType::List => vec![
            Field::new("index", DataType::Int64, false),
            binary_field("value", false),
        ],
        KeyType::Hash => vec![binary_field("field", false), binary_field("value", false)],
        KeyType::Set => vec![binary_field("member", false)],
        KeyType::Zset => vec![
            binary_field("member", false),
            Field::new("score", DataType::Float64, false),
        ],
        KeyType::Stream => vec![
            Field::new("id", DataType::Utf8, false),
            json_field("fields", false),
        ],
        KeyType::Json => vec![json_field("value", false)],
        KeyType::Other => return None,
    };
    Some(Arc::new(Schema::new(fields)))
}

/// The read as console commands (`HSCAN m7:hash 0 COUNT 1000`).
pub(crate) fn read_text(key: &[u8], key_type: KeyType) -> String {
    let k = console_arg(key);
    let last = VALUE_CHUNK - 1;
    match key_type {
        KeyType::String => format!("GET {k}"),
        KeyType::List => format!("LRANGE {k} 0 {last}"),
        KeyType::Hash => format!("HSCAN {k} 0 COUNT {VALUE_CHUNK}"),
        KeyType::Set => format!("SSCAN {k} 0 COUNT {VALUE_CHUNK}"),
        KeyType::Zset => format!("ZRANGE {k} 0 {last} WITHSCORES"),
        KeyType::Stream => format!("XRANGE {k} - + COUNT {VALUE_CHUNK}"),
        KeyType::Json => format!("JSON.GET {k} $"),
        KeyType::Other => format!("TYPE {k}"),
    }
}

/// Where a value read is.
enum Position {
    /// Strings and JSON: one read.
    Start,
    /// `LRANGE` / `ZRANGE` by index.
    Index(i64),
    /// `HSCAN` / `SSCAN` cursor.
    Cursor(Vec<u8>),
    /// `XRANGE` from this id (inclusive).
    Id(String),
}

struct ValueState {
    handle: RedisHandle,
    key: Vec<u8>,
    key_type: KeyType,
    position: Position,
    seen: HashSet<Vec<u8>>,
    schema: SchemaRef,
}

impl ValueState {
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the lazy stream hands each step its own token"
    )]
    async fn step(mut self, cancel: CancellationToken) -> (Self, Result<Step>) {
        let outcome = self.next(&cancel).await;
        (self, outcome)
    }

    async fn run(&self, c: &redis::Cmd, cancel: &CancellationToken) -> Result<Value> {
        self.handle.command(c, cancel).await
    }

    fn command(&self, name: &str) -> redis::Cmd {
        let mut c = redis::cmd(name);
        c.arg(self.key.as_slice());
        c
    }

    /// Keeps `item` unless it was seen (cursor commands may repeat elements).
    fn first_time(&mut self, item: &[u8]) -> bool {
        if self.seen.contains(item) {
            return false;
        }
        if self.seen.len() < SCAN_DEDUP_LIMIT {
            self.seen.insert(item.to_vec());
        }
        true
    }

    async fn next(&mut self, cancel: &CancellationToken) -> Result<Step> {
        check(cancel)?;
        match self.key_type {
            KeyType::String => self.string(cancel).await,
            KeyType::List => self.list(cancel).await,
            KeyType::Hash | KeyType::Set => self.scanned(cancel).await,
            KeyType::Zset => self.zset(cancel).await,
            KeyType::Stream => self.stream(cancel).await,
            KeyType::Json => self.json(cancel).await,
            KeyType::Other => Ok((Vec::new(), false)),
        }
    }

    async fn string(&mut self, cancel: &CancellationToken) -> Result<Step> {
        let length = int_of(&self.run(&self.command("STRLEN"), cancel).await?).unwrap_or(0);
        let length = usize::try_from(length).unwrap_or(0);
        let mut notices = Vec::new();
        let value = if length <= VALUE_CHUNK {
            match self.run(&self.command("GET"), cancel).await? {
                // Gone since the type was read.
                Value::Nil => return Ok((Vec::new(), false)),
                other => into_bytes(other).unwrap_or_default(),
            }
        } else {
            let wanted = length.min(STRING_READ_LIMIT);
            if wanted < length {
                notices.push(Ok(ResultItem::Notice(Notice::warning(format!(
                    "the value has {length} bytes; only the first {STRING_READ_LIMIT} are shown"
                )))));
            }
            let mut value = Vec::with_capacity(wanted);
            while value.len() < wanted {
                check(cancel)?;
                let start = value.len();
                let end = (start + STRING_PIECE).min(wanted) - 1;
                let mut c = self.command("GETRANGE");
                c.arg(start).arg(end);
                let piece = into_bytes(self.run(&c, cancel).await?).unwrap_or_default();
                if piece.is_empty() {
                    break;
                }
                value.extend_from_slice(&piece);
            }
            value
        };
        let item = batch(
            &self.schema,
            vec![Arc::new(BinaryArray::from_iter_values([value.as_slice()]))],
        );
        notices.push(item);
        Ok((notices, false))
    }

    async fn list(&mut self, cancel: &CancellationToken) -> Result<Step> {
        let start = match self.position {
            Position::Index(i) => i,
            _ => 0,
        };
        let mut c = self.command("LRANGE");
        c.arg(start).arg(start + chunk_i64() - 1);
        let values: Vec<Vec<u8>> = items(self.run(&c, cancel).await?)
            .into_iter()
            .filter_map(into_bytes)
            .collect();
        let n = i64::try_from(values.len()).unwrap_or(i64::MAX);
        self.position = Position::Index(start + n);
        if values.is_empty() {
            return Ok((Vec::new(), false));
        }
        let more = n >= chunk_i64();
        let columns: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from_iter_values(start..start + n)),
            Arc::new(BinaryArray::from_iter_values(
                values.iter().map(Vec::as_slice),
            )),
        ];
        Ok((vec![batch(&self.schema, columns)], more))
    }

    /// `HSCAN` (hash) or `SSCAN` (set).
    async fn scanned(&mut self, cancel: &CancellationToken) -> Result<Step> {
        let cursor = match &self.position {
            Position::Cursor(c) => c.clone(),
            _ => b"0".to_vec(),
        };
        let hash = self.key_type == KeyType::Hash;
        let mut c = self.command(if hash { "HSCAN" } else { "SSCAN" });
        c.arg(cursor.as_slice()).arg("COUNT").arg(VALUE_CHUNK);
        let (next, elements) = scan_reply(self.run(&c, cancel).await?)?;
        let more = !scan_done(&next);
        self.position = Position::Cursor(next);
        let columns: Vec<ArrayRef> = if hash {
            let mut fields = Vec::new();
            let mut values = Vec::new();
            for (f, v) in pairs(Value::Array(elements)) {
                let (Some(f), Some(v)) = (into_bytes(f), into_bytes(v)) else {
                    continue;
                };
                if self.first_time(&f) {
                    fields.push(f);
                    values.push(v);
                }
            }
            if fields.is_empty() {
                return Ok((Vec::new(), more));
            }
            vec![
                Arc::new(BinaryArray::from_iter_values(
                    fields.iter().map(Vec::as_slice),
                )),
                Arc::new(BinaryArray::from_iter_values(
                    values.iter().map(Vec::as_slice),
                )),
            ]
        } else {
            let mut members = Vec::new();
            for m in elements.into_iter().filter_map(into_bytes) {
                if self.first_time(&m) {
                    members.push(m);
                }
            }
            if members.is_empty() {
                return Ok((Vec::new(), more));
            }
            vec![Arc::new(BinaryArray::from_iter_values(
                members.iter().map(Vec::as_slice),
            ))]
        };
        Ok((vec![batch(&self.schema, columns)], more))
    }

    async fn zset(&mut self, cancel: &CancellationToken) -> Result<Step> {
        let start = match self.position {
            Position::Index(i) => i,
            _ => 0,
        };
        let mut c = self.command("ZRANGE");
        c.arg(start).arg(start + chunk_i64() - 1).arg("WITHSCORES");
        let mut members = Vec::new();
        let mut scores = Vec::new();
        for (m, s) in pairs(self.run(&c, cancel).await?) {
            let (Some(m), Some(s)) = (into_bytes(m), float_of(&s)) else {
                continue;
            };
            members.push(m);
            scores.push(s);
        }
        let n = i64::try_from(members.len()).unwrap_or(i64::MAX);
        self.position = Position::Index(start + n);
        if members.is_empty() {
            return Ok((Vec::new(), false));
        }
        let more = n >= chunk_i64();
        let columns: Vec<ArrayRef> = vec![
            Arc::new(BinaryArray::from_iter_values(
                members.iter().map(Vec::as_slice),
            )),
            Arc::new(Float64Array::from(scores)),
        ];
        Ok((vec![batch(&self.schema, columns)], more))
    }

    async fn stream(&mut self, cancel: &CancellationToken) -> Result<Step> {
        let start = match &self.position {
            Position::Id(id) => id.clone(),
            _ => "-".to_owned(),
        };
        let mut c = self.command("XRANGE");
        c.arg(start.as_str()).arg("+").arg("COUNT").arg(VALUE_CHUNK);
        let mut ids = Vec::new();
        let mut fields = Vec::new();
        for entry in items(self.run(&c, cancel).await?) {
            let mut parts = items(entry).into_iter();
            let (Some(id), Some(body)) = (parts.next().and_then(|v| text_of(&v)), parts.next())
            else {
                continue;
            };
            let mut object = serde_json::Map::new();
            for (f, v) in pairs(body) {
                let (Some(f), Some(v)) = (into_bytes(f), into_bytes(v)) else {
                    continue;
                };
                object.insert(String::from_utf8_lossy(&f).into_owned(), json_bytes(&v));
            }
            ids.push(id);
            fields.push(serde_json::Value::Object(object).to_string());
        }
        let Some(last) = ids.last() else {
            return Ok((Vec::new(), false));
        };
        let more = ids.len() >= VALUE_CHUNK;
        match next_stream_id(last) {
            Some(next) => self.position = Position::Id(next),
            None => return Err(Error::internal(format!("unexpected stream id '{last}'"))),
        }
        let columns: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(ids)),
            Arc::new(StringArray::from(fields)),
        ];
        Ok((vec![batch(&self.schema, columns)], more))
    }

    async fn json(&mut self, cancel: &CancellationToken) -> Result<Step> {
        let mut c = self.command("JSON.GET");
        c.arg("$");
        let reply = self.run(&c, cancel).await?;
        let Some(text) = text_of(&reply) else {
            return Ok((Vec::new(), false));
        };
        // `JSON.GET key $` answers `[document]`: the document itself is shown.
        let value = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(serde_json::Value::Array(mut all)) if all.len() == 1 => all.remove(0).to_string(),
            _ => text,
        };
        let columns: Vec<ArrayRef> = vec![Arc::new(StringArray::from(vec![value]))];
        Ok((vec![batch(&self.schema, columns)], false))
    }
}

/// The smallest stream id after `id` (`ms-seq` → `ms-(seq+1)`).
pub(crate) fn next_stream_id(id: &str) -> Option<String> {
    let (ms, seq) = parse_stream_id(id)?;
    Some(match seq.checked_add(1) {
        Some(seq) => format!("{ms}-{seq}"),
        None => format!("{}-0", ms.checked_add(1)?),
    })
}

/// `ms-seq` (or `ms`, sequence 0) as numbers.
pub(crate) fn parse_stream_id(id: &str) -> Option<(u64, u64)> {
    match id.split_once('-') {
        Some((ms, seq)) => Some((ms.parse().ok()?, seq.parse().ok()?)),
        None => Some((id.parse().ok()?, 0)),
    }
}

#[async_trait]
impl ResultSource for ValueSource {
    async fn open(&self, c: &dyn Connection, cancel: CancellationToken) -> Result<ResultStream> {
        check(&cancel)?;
        let schema = value_schema(self.key_type).ok_or_else(|| {
            Error::unsupported("values of this key type cannot be read by the key browser")
        })?;
        let redis = native(c)?;
        let state = ValueState {
            handle: redis.handle(),
            key: self.key.clone(),
            key_type: self.key_type,
            position: Position::Start,
            seen: HashSet::new(),
            schema: schema.clone(),
        };
        Ok(lazy(schema, state, cancel, ValueState::step))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_ids_advance() {
        assert_eq!(
            next_stream_id("1767261600000-0").unwrap(),
            "1767261600000-1"
        );
        assert_eq!(next_stream_id(&format!("5-{}", u64::MAX)).unwrap(), "6-0");
        assert_eq!(next_stream_id("7").unwrap(), "7-1");
        assert_eq!(next_stream_id("x-1"), None);
        assert_eq!(parse_stream_id("3-4"), Some((3, 4)));
    }

    #[test]
    fn console_texts_name_the_commands() {
        let mut r = KeyScanRequest::all("0");
        assert_eq!(scan_text(&r), "SCAN 0 MATCH * COUNT 1000");
        r.pattern = Some("m7:*".into());
        r.key_type = Some(KeyType::Json);
        r.count = Some(50);
        assert_eq!(scan_text(&r), "SCAN 0 MATCH m7:* COUNT 50 TYPE ReJSON-RL");
        assert_eq!(
            read_text(b"m7:hash", KeyType::Hash),
            "HSCAN m7:hash 0 COUNT 1000"
        );
        assert_eq!(
            read_text(b"m7:zset", KeyType::Zset),
            "ZRANGE m7:zset 0 999 WITHSCORES"
        );
        assert_eq!(read_text(&[0xff], KeyType::String), "GET \"\\xff\"");
    }

    #[test]
    fn schemas_follow_the_contract() {
        let names = |t: KeyType| -> Vec<String> {
            value_schema(t)
                .unwrap()
                .fields()
                .iter()
                .map(|f| f.name().clone())
                .collect()
        };
        assert_eq!(names(KeyType::List), ["index", "value"]);
        assert_eq!(names(KeyType::Hash), ["field", "value"]);
        assert_eq!(names(KeyType::Zset), ["member", "score"]);
        assert_eq!(names(KeyType::Stream), ["id", "fields"]);
        assert!(value_schema(KeyType::Other).is_none());
        let scan = scan_schema();
        assert_eq!(scan.field(0).data_type(), &DataType::Binary);
        assert!(scan.field(3).is_nullable());
    }

    #[tokio::test]
    async fn lazy_streams_stop_on_cancel() {
        let schema = value_schema(KeyType::Set).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let s = lazy(schema, 0_u32, cancel, |n, _| async move {
            (n + 1, Ok((Vec::new(), true)))
        });
        let items: Vec<_> = s.collect().await;
        assert_eq!(items.len(), 2);
        assert!(matches!(items[0], Ok(ResultItem::Schema(_))));
        assert!(matches!(&items[1], Err(e) if e.is_cancelled()));
    }

    #[tokio::test]
    async fn lazy_streams_end_after_an_error() {
        let schema = value_schema(KeyType::Set).unwrap();
        let s = lazy(schema, 0_u32, CancellationToken::new(), |n, _| async move {
            if n < 2 {
                (n + 1, Ok((Vec::new(), true)))
            } else {
                (n, Err(Error::internal("boom")))
            }
        });
        let items: Vec<_> = s.collect().await;
        assert_eq!(items.len(), 2);
        assert!(items[1].is_err());
    }
}
