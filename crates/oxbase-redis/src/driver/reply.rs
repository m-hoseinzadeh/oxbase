//! Reply → Arrow rendering (driver module docs, "Reply rendering").
//!
//! A reply becomes one result set: a command-aware shape for common read commands (`HGETALL`,
//! `ZRANGE ... WITHSCORES`, `LRANGE`, `SMEMBERS`, `XRANGE`, `INFO`, and the cursor commands),
//! otherwise a generic shape by RESP type. Integer replies of write commands are
//! `RowsAffected`. Rows are cut into batches of [`BATCH_ROWS`].

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::{ArrayRef, BinaryArray, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use oxbase_driver::{Notice, ResultItem, from_arrow};
use oxbase_types::Result;
use oxbase_types::keyvalue::{escape_bytes, to_hex};
use redis::Value;

use super::args::CommandInfo;

/// Rows per Arrow batch.
pub(crate) const BATCH_ROWS: usize = 4096;
/// Arrow field metadata: the RESP type of the column's values (`mixed` when they differ).
pub(crate) const META_NATIVE_TYPE: &str = "oxbase.native_type";
/// Arrow field metadata marking `Binary` columns of user bytes (`binary`).
pub(crate) const META_ENCODING: &str = "oxbase.encoding";
/// Arrow's canonical extension-name metadata key and the JSON extension name.
pub(crate) const META_EXTENSION_NAME: &str = "ARROW:extension:name";
pub(crate) const JSON_EXTENSION: &str = "arrow.json";

/// Renders the reply of the command `args` (described by `info`).
pub(crate) fn render(
    info: &CommandInfo,
    args: &[Vec<u8>],
    value: Value,
) -> Result<Vec<ResultItem>> {
    let value = strip_attribute(value);
    if let Value::Int(n) = value
        && info.counts_changes()
        && let Ok(n) = u64::try_from(n)
    {
        return Ok(vec![ResultItem::RowsAffected(n)]);
    }
    let table = match typed(info, args, value) {
        Ok(table) => table,
        Err(value) => generic(info, value),
    };
    table.into_items()
}

fn strip_attribute(value: Value) -> Value {
    match value {
        Value::Attribute { data, .. } => strip_attribute(*data),
        other => other,
    }
}

// ---- table model ------------------------------------------------------------------------------

enum Data {
    Utf8(Vec<Option<String>>),
    Binary(Vec<Option<Vec<u8>>>),
    Int64(Vec<Option<i64>>),
    Float64(Vec<Option<f64>>),
}

impl Data {
    fn len(&self) -> usize {
        match self {
            Self::Utf8(v) => v.len(),
            Self::Binary(v) => v.len(),
            Self::Int64(v) => v.len(),
            Self::Float64(v) => v.len(),
        }
    }

    fn data_type(&self) -> DataType {
        match self {
            Self::Utf8(_) => DataType::Utf8,
            Self::Binary(_) => DataType::Binary,
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
        }
    }

    fn slice(&self, from: usize, to: usize) -> ArrayRef {
        match self {
            Self::Utf8(v) => Arc::new(
                v[from..to]
                    .iter()
                    .map(Option::as_deref)
                    .collect::<StringArray>(),
            ),
            Self::Binary(v) => Arc::new(BinaryArray::from_iter(
                v[from..to].iter().map(Option::as_deref),
            )),
            Self::Int64(v) => Arc::new(Int64Array::from(v[from..to].to_vec())),
            Self::Float64(v) => Arc::new(Float64Array::from(v[from..to].to_vec())),
        }
    }
}

struct Column {
    name: &'static str,
    native: String,
    json: bool,
    data: Data,
}

impl Column {
    fn new(name: &'static str, native: &str, data: Data) -> Self {
        Self {
            name,
            native: native.to_owned(),
            json: false,
            data,
        }
    }

    fn json(mut self) -> Self {
        self.json = true;
        self
    }

    fn field(&self) -> Field {
        let mut meta = HashMap::from([(META_NATIVE_TYPE.to_owned(), self.native.clone())]);
        if matches!(self.data, Data::Binary(_)) {
            meta.insert(META_ENCODING.to_owned(), "binary".to_owned());
        }
        if self.json {
            meta.insert(META_EXTENSION_NAME.to_owned(), JSON_EXTENSION.to_owned());
        }
        Field::new(self.name, self.data.data_type(), true).with_metadata(meta)
    }
}

struct Table {
    columns: Vec<Column>,
    notices: Vec<Notice>,
}

impl Table {
    fn new(columns: Vec<Column>) -> Self {
        Self {
            columns,
            notices: Vec::new(),
        }
    }

    fn with_cursor(mut self, cursor: Option<String>) -> Self {
        if let Some(cursor) = cursor {
            self.notices.push(Notice::info(format!(
                "next cursor: {cursor} (0 means the iteration is complete)"
            )));
        }
        self
    }

    fn into_items(self) -> Result<Vec<ResultItem>> {
        let rows = self.columns.first().map_or(0, |c| c.data.len());
        let schema = Arc::new(Schema::new(
            self.columns.iter().map(Column::field).collect::<Vec<_>>(),
        ));
        let mut items: Vec<ResultItem> = self.notices.into_iter().map(ResultItem::Notice).collect();
        items.push(ResultItem::Schema(schema.clone()));
        let mut from = 0;
        while from < rows {
            let to = (from + BATCH_ROWS).min(rows);
            let arrays = self
                .columns
                .iter()
                .map(|c| c.data.slice(from, to))
                .collect();
            items.push(ResultItem::Batch(
                RecordBatch::try_new(schema.clone(), arrays).map_err(from_arrow)?,
            ));
            from = to;
        }
        Ok(items)
    }
}

// ---- scalar cells -----------------------------------------------------------------------------

/// One reply element as the generic columns show it.
struct Cell {
    kind: &'static str,
    text: Option<String>,
    raw: Option<Vec<u8>>,
    json: bool,
}

/// UTF-8 bytes as they are, other bytes escaped like `redis-cli` (`\xff`).
pub(crate) fn bytes_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => escape_bytes(bytes),
    }
}

fn double_text(d: f64) -> String {
    if d.is_infinite() {
        if d > 0.0 { "inf" } else { "-inf" }.to_owned()
    } else {
        d.to_string()
    }
}

/// The RESP type name of a value (`type` column).
fn kind(value: &Value) -> &'static str {
    match value {
        Value::Nil => "null",
        Value::Int(_) => "integer",
        Value::BulkString(_) => "string",
        Value::SimpleString(_) | Value::Okay => "status",
        Value::Double(_) => "double",
        Value::Boolean(_) => "boolean",
        Value::VerbatimString { .. } => "verbatim",
        Value::BigNumber(_) => "bignumber",
        Value::ServerError(_) => "error",
        Value::Array(_) => "array",
        Value::Set(_) => "set",
        Value::Map(_) => "map",
        Value::Push { .. } => "push",
        Value::Attribute { data, .. } => kind(data),
        _ => "unknown",
    }
}

fn cell(value: Value) -> Cell {
    let kind = kind(&value);
    let (text, raw, json) = match value {
        Value::Nil => (None, None, false),
        Value::Int(i) => (Some(i.to_string()), None, false),
        Value::BulkString(b) => (Some(bytes_text(&b)), Some(b), false),
        Value::SimpleString(s) => (Some(s), None, false),
        Value::Okay => (Some("OK".to_owned()), None, false),
        Value::Double(d) => (Some(double_text(d)), None, false),
        Value::Boolean(b) => (Some(b.to_string()), None, false),
        Value::VerbatimString { text, .. } => (Some(text), None, false),
        Value::BigNumber(n) => (Some(big_text(&n)), None, false),
        Value::ServerError(e) => (
            Some(match e.details() {
                Some(d) => format!("{} {d}", e.code()),
                None => e.code().to_owned(),
            }),
            None,
            false,
        ),
        Value::Attribute { data, .. } => return cell(*data),
        aggregate @ (Value::Array(_) | Value::Set(_) | Value::Map(_) | Value::Push { .. }) => {
            (Some(to_json(aggregate).to_string()), None, true)
        }
        other => (Some(format!("{other:?}")), None, false),
    };
    Cell {
        kind,
        text,
        raw,
        json,
    }
}

/// A RESP3 big number: the crate hands over its decimal digits.
fn big_text(n: &[u8]) -> String {
    String::from_utf8_lossy(n).into_owned()
}

/// A reply element as JSON (nested elements in the generic `value` column).
///
/// Byte strings are JSON strings when UTF-8, else `{"hex": "..."}`; a map is an object when
/// every key is text, else an array of `[key, value]` pairs; doubles that JSON cannot hold
/// (NaN, infinities) and big numbers are strings; an error is `{"error": "CODE message"}`.
pub(crate) fn to_json(value: Value) -> serde_json::Value {
    use serde_json::Value as J;
    match value {
        Value::Nil => J::Null,
        Value::Int(i) => J::from(i),
        Value::BulkString(b) => bytes_json(b),
        Value::SimpleString(s) => J::String(s),
        Value::Okay => J::String("OK".to_owned()),
        Value::Double(d) => {
            serde_json::Number::from_f64(d).map_or_else(|| J::String(double_text(d)), J::Number)
        }
        Value::Boolean(b) => J::Bool(b),
        Value::VerbatimString { text, .. } => J::String(text),
        Value::BigNumber(n) => J::String(big_text(&n)),
        Value::ServerError(e) => serde_json::json!({
            "error": match e.details() {
                Some(d) => format!("{} {d}", e.code()),
                None => e.code().to_owned(),
            }
        }),
        Value::Array(items) | Value::Set(items) | Value::Push { data: items, .. } => {
            J::Array(items.into_iter().map(to_json).collect())
        }
        Value::Map(entries) => map_json(entries),
        Value::Attribute { data, .. } => to_json(*data),
        other => J::String(format!("{other:?}")),
    }
}

fn bytes_json(b: Vec<u8>) -> serde_json::Value {
    match String::from_utf8(b) {
        Ok(text) => serde_json::Value::String(text),
        Err(e) => serde_json::json!({ "hex": to_hex(e.as_bytes()) }),
    }
}

fn key_text(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(b) => std::str::from_utf8(b).ok().map(str::to_owned),
        Value::SimpleString(s) | Value::VerbatimString { text: s, .. } => Some(s.clone()),
        Value::Int(i) => Some(i.to_string()),
        _ => None,
    }
}

fn map_json(entries: Vec<(Value, Value)>) -> serde_json::Value {
    if entries.iter().all(|(k, _)| key_text(k).is_some()) {
        let mut object = serde_json::Map::new();
        for (k, v) in entries {
            object.insert(key_text(&k).unwrap_or_default(), to_json(v));
        }
        serde_json::Value::Object(object)
    } else {
        serde_json::Value::Array(
            entries
                .into_iter()
                .map(|(k, v)| serde_json::Value::Array(vec![to_json(k), to_json(v)]))
                .collect(),
        )
    }
}

/// `native` of a column: the one kind every row has, else `mixed`.
fn uniform(kinds: &[&'static str], empty: &str) -> String {
    match kinds.first() {
        None => empty.to_owned(),
        Some(first) if kinds.iter().all(|k| k == first) => (*first).to_owned(),
        Some(_) => "mixed".to_owned(),
    }
}

// ---- generic shapes ---------------------------------------------------------------------------

/// Commands whose RESP2 reply is a flat list of key / value pairs (RESP3 sends a map).
const PAIR_COMMANDS: [(&str, Option<&str>); 2] = [("CONFIG", Some("GET")), ("HELLO", None)];

fn generic(info: &CommandInfo, value: Value) -> Table {
    let pairs = PAIR_COMMANDS
        .iter()
        .any(|(name, sub)| info.name == *name && (sub.is_none() || info.sub.as_deref() == *sub));
    match value {
        Value::Map(entries) => map_table(entries),
        Value::Array(items) if pairs && items.len() % 2 == 0 => map_table(into_pairs(items)),
        Value::Array(items) | Value::Set(items) | Value::Push { data: items, .. } => {
            list_table(items)
        }
        scalar => scalar_table(scalar),
    }
}

fn into_pairs(items: Vec<Value>) -> Vec<(Value, Value)> {
    let mut out = Vec::with_capacity(items.len() / 2);
    let mut it = items.into_iter();
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        out.push((k, v));
    }
    out
}

/// `type`, `value`, `raw` (+ the kinds of the value rows, for metadata).
struct Cells {
    kinds: Vec<&'static str>,
    texts: Vec<Option<String>>,
    raws: Vec<Option<Vec<u8>>>,
    all_json: bool,
}

impl Cells {
    fn with_capacity(n: usize) -> Self {
        Self {
            kinds: Vec::with_capacity(n),
            texts: Vec::with_capacity(n),
            raws: Vec::with_capacity(n),
            all_json: true,
        }
    }

    fn push(&mut self, value: Value) {
        let c = cell(value);
        self.all_json &= c.json || c.text.is_none();
        self.kinds.push(c.kind);
        self.texts.push(c.text);
        self.raws.push(c.raw);
    }

    fn columns(self) -> [Column; 3] {
        let native = uniform(&self.kinds, "null");
        let any_text = self.texts.iter().any(Option::is_some);
        let value = Column::new("value", &native, Data::Utf8(self.texts));
        let value = if self.all_json && any_text {
            value.json()
        } else {
            value
        };
        [
            Column::new(
                "type",
                "simple string",
                Data::Utf8(self.kinds.iter().map(|k| Some((*k).to_owned())).collect()),
            ),
            value,
            Column::new("raw", "bulk string", Data::Binary(self.raws)),
        ]
    }
}

fn scalar_table(value: Value) -> Table {
    let mut cells = Cells::with_capacity(1);
    cells.push(value);
    Table::new(cells.columns().into())
}

fn list_table(items: Vec<Value>) -> Table {
    let n = items.len();
    let mut cells = Cells::with_capacity(n);
    let mut index = Vec::with_capacity(n);
    for (i, item) in items.into_iter().enumerate() {
        index.push(i64::try_from(i + 1).ok());
        cells.push(item);
    }
    let mut columns = vec![Column::new("#", "integer", Data::Int64(index))];
    columns.extend(cells.columns());
    Table::new(columns)
}

fn map_table(entries: Vec<(Value, Value)>) -> Table {
    let n = entries.len();
    let mut keys = Vec::with_capacity(n);
    let mut key_raws = Vec::with_capacity(n);
    let mut cells = Cells::with_capacity(n);
    for (k, v) in entries {
        let key = cell(k);
        keys.push(key.text);
        key_raws.push(key.raw);
        cells.push(v);
    }
    let mut columns = vec![
        Column::new("key", "string", Data::Utf8(keys)),
        Column::new("key_raw", "bulk string", Data::Binary(key_raws)),
    ];
    columns.extend(cells.columns());
    Table::new(columns)
}

// ---- command-aware shapes ---------------------------------------------------------------------

fn has_arg(args: &[Vec<u8>], word: &str) -> bool {
    args.iter()
        .skip(1)
        .any(|a| a.eq_ignore_ascii_case(word.as_bytes()))
}

/// The typed shape of `info`'s reply, or the value back when the command has none or the reply
/// does not have the expected structure (an error reply, `nil`, an unexpected RESP3 form).
fn typed(info: &CommandInfo, args: &[Vec<u8>], value: Value) -> std::result::Result<Table, Value> {
    match info.name.as_str() {
        "LRANGE" => list_range(args, value),
        "ZRANGE" | "ZREVRANGE" | "ZRANGEBYSCORE" | "ZREVRANGEBYSCORE" | "ZUNION" | "ZINTER"
        | "ZDIFF" | "ZRANDMEMBER"
            if has_arg(args, "WITHSCORES") =>
        {
            scored(value, None)
        }
        "ZSCAN" if !has_arg(args, "NOSCORES") => cursor_reply(value, scored),
        "HGETALL" => fields(value, None),
        "HSCAN" if !has_arg(args, "NOVALUES") => cursor_reply(value, fields),
        "SMEMBERS" | "SINTER" | "SUNION" | "SDIFF" => members(value, None),
        "SSCAN" => cursor_reply(value, members),
        "XRANGE" | "XREVRANGE" => stream_entries(value),
        "INFO" => info_sections(value),
        _ => Err(value),
    }
}

fn bulk(value: &Value) -> bool {
    matches!(value, Value::BulkString(_))
}

fn into_bytes(value: Value) -> Option<Vec<u8>> {
    match value {
        Value::BulkString(b) => Some(b),
        _ => None,
    }
}

fn cursor_text(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(b) => std::str::from_utf8(b).ok().map(str::to_owned),
        Value::SimpleString(s) => Some(s.clone()),
        Value::Int(i) => Some(i.to_string()),
        _ => None,
    }
}

/// `[cursor, elements]` of the `*SCAN` commands: `shape` renders the elements and the cursor
/// becomes an info notice.
fn cursor_reply(
    value: Value,
    shape: impl FnOnce(Value, Option<String>) -> std::result::Result<Table, Value>,
) -> std::result::Result<Table, Value> {
    match value {
        Value::Array(items)
            if items.len() == 2
                && cursor_text(&items[0]).is_some()
                && matches!(items[1], Value::Array(_)) =>
        {
            let mut it = items.into_iter();
            let cursor = it.next().as_ref().and_then(cursor_text);
            let elements = it.next().unwrap_or(Value::Nil);
            shape(elements, cursor.clone()).map_err(|elements| {
                Value::Array(vec![
                    Value::BulkString(cursor.unwrap_or_default().into_bytes()),
                    elements,
                ])
            })
        }
        other => Err(other),
    }
}

/// `LRANGE key start stop`: `index`, `value`.
fn list_range(args: &[Vec<u8>], value: Value) -> std::result::Result<Table, Value> {
    let Value::Array(items) = value else {
        return Err(value);
    };
    if !items.iter().all(bulk) {
        return Err(Value::Array(items));
    }
    let number = |i: usize| {
        args.get(i)
            .and_then(|a| std::str::from_utf8(a).ok())
            .and_then(|a| a.trim().parse::<i64>().ok())
    };
    let n = i64::try_from(items.len()).unwrap_or(i64::MAX);
    // A non-negative start counts from the head. Negative positions count from the tail: with a
    // negative stop the last element returned is at `stop`, which gives the others.
    let first = match (number(2), number(3)) {
        (Some(start), _) if start >= 0 => Some(start),
        (_, Some(stop)) if stop < 0 => Some(stop - n + 1),
        (start, _) => start,
    };
    let index = (0..n).map(|i| first.map(|f| f + i)).collect();
    let values = items.into_iter().map(into_bytes).collect();
    Ok(Table::new(vec![
        Column::new("index", "integer", Data::Int64(index)),
        Column::new("value", "bulk string", Data::Binary(values)),
    ]))
}

#[allow(
    clippy::cast_precision_loss,
    reason = "a sorted-set score is a double on the server; integers beyond 2^53 cannot occur"
)]
fn score(value: &Value) -> Option<f64> {
    match value {
        Value::Double(d) => Some(*d),
        Value::Int(i) => Some(*i as f64),
        Value::BulkString(b) => std::str::from_utf8(b).ok()?.trim().parse().ok(),
        Value::SimpleString(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Members with scores: RESP3 `[[member, score], ...]` or RESP2 `[member, score, ...]`.
fn scored(value: Value, cursor: Option<String>) -> std::result::Result<Table, Value> {
    let Value::Array(items) = value else {
        return Err(value);
    };
    let nested = !items.is_empty()
        && items.iter().all(|i| {
            matches!(i, Value::Array(p) if p.len() == 2 && bulk(&p[0]) && score(&p[1]).is_some())
        });
    let flat = items.len() % 2 == 0
        && items
            .as_chunks::<2>()
            .0
            .iter()
            .all(|[member, score_value]| bulk(member) && score(score_value).is_some());
    let pairs: Vec<(Value, Value)> = if nested {
        items
            .into_iter()
            .filter_map(|i| match i {
                Value::Array(p) => {
                    let mut p = p.into_iter();
                    Some((p.next()?, p.next()?))
                }
                _ => None,
            })
            .collect()
    } else if flat {
        into_pairs(items)
    } else {
        return Err(Value::Array(items));
    };
    let scores = pairs.iter().map(|(_, s)| score(s)).collect();
    let members = pairs.into_iter().map(|(m, _)| into_bytes(m)).collect();
    Ok(Table::new(vec![
        Column::new("member", "bulk string", Data::Binary(members)),
        Column::new("score", "double", Data::Float64(scores)),
    ])
    .with_cursor(cursor))
}

/// Hash fields: RESP3 map or RESP2 `[field, value, ...]`.
fn fields(value: Value, cursor: Option<String>) -> std::result::Result<Table, Value> {
    let pairs = match value {
        Value::Map(entries) if entries.iter().all(|(k, v)| bulk(k) && bulk(v)) => entries,
        Value::Array(items) if items.len() % 2 == 0 && items.iter().all(bulk) => into_pairs(items),
        other => return Err(other),
    };
    let mut names = Vec::with_capacity(pairs.len());
    let mut values = Vec::with_capacity(pairs.len());
    for (k, v) in pairs {
        names.push(into_bytes(k));
        values.push(into_bytes(v));
    }
    Ok(Table::new(vec![
        Column::new("field", "bulk string", Data::Binary(names)),
        Column::new("value", "bulk string", Data::Binary(values)),
    ])
    .with_cursor(cursor))
}

/// Set members: RESP3 set or RESP2 array.
fn members(value: Value, cursor: Option<String>) -> std::result::Result<Table, Value> {
    let items = match value {
        Value::Set(items) | Value::Array(items) if items.iter().all(bulk) => items,
        other => return Err(other),
    };
    Ok(Table::new(vec![Column::new(
        "member",
        "bulk string",
        Data::Binary(items.into_iter().map(into_bytes).collect()),
    )])
    .with_cursor(cursor))
}

/// `XRANGE`: `[[id, [field, value, ...]], ...]` → `id`, `fields` (JSON object).
fn stream_entries(value: Value) -> std::result::Result<Table, Value> {
    let Value::Array(entries) = value else {
        return Err(value);
    };
    let well_formed = entries.iter().all(|e| match e {
        Value::Array(p) => {
            p.len() == 2
                && key_text(&p[0]).is_some()
                && matches!(&p[1], Value::Array(f) if f.len() % 2 == 0)
        }
        _ => false,
    });
    if !well_formed {
        return Err(Value::Array(entries));
    }
    let mut ids = Vec::with_capacity(entries.len());
    let mut fields = Vec::with_capacity(entries.len());
    for entry in entries {
        let Value::Array(parts) = entry else { continue };
        let mut parts = parts.into_iter();
        let id = parts.next().as_ref().and_then(key_text);
        let object = match parts.next() {
            Some(Value::Array(flat)) => {
                let mut object = serde_json::Map::new();
                for (k, v) in into_pairs(flat) {
                    let key = match &k {
                        Value::BulkString(b) => bytes_text(b),
                        other => key_text(other).unwrap_or_default(),
                    };
                    object.insert(key, to_json(v));
                }
                serde_json::Value::Object(object)
            }
            _ => serde_json::Value::Null,
        };
        ids.push(id);
        fields.push(Some(object.to_string()));
    }
    Ok(Table::new(vec![
        Column::new("id", "bulk string", Data::Utf8(ids)),
        Column::new("fields", "map", Data::Utf8(fields)).json(),
    ]))
}

/// `INFO` text: `# Section` headers and `key:value` lines → `section`, `key`, `value`.
fn info_sections(value: Value) -> std::result::Result<Table, Value> {
    let text = match &value {
        Value::BulkString(b) => std::str::from_utf8(b).ok().map(str::to_owned),
        Value::VerbatimString { text, .. } => Some(text.clone()),
        _ => None,
    };
    let Some(text) = text else {
        return Err(value);
    };
    let (sections, keys, values) = parse_info(&text);
    if keys.is_empty() {
        return Err(value);
    }
    Ok(Table::new(vec![
        Column::new("section", "verbatim", Data::Utf8(sections)),
        Column::new("key", "verbatim", Data::Utf8(keys)),
        Column::new("value", "verbatim", Data::Utf8(values)),
    ]))
}

type InfoRows = (
    Vec<Option<String>>,
    Vec<Option<String>>,
    Vec<Option<String>>,
);

fn parse_info(text: &str) -> InfoRows {
    let (mut sections, mut keys, mut values) = (Vec::new(), Vec::new(), Vec::new());
    let mut section = String::new();
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('#') {
            name.trim().clone_into(&mut section);
        } else if let Some((key, value)) = line.split_once(':') {
            sections.push(Some(section.clone()));
            keys.push(Some(key.to_owned()));
            values.push(Some(value.to_owned()));
        }
    }
    (sections, keys, values)
}

/// The value of `key` in `INFO` text (`redis_version`).
pub(crate) fn info_field(text: &str, key: &str) -> Option<String> {
    let (_, keys, values) = parse_info(text);
    keys.iter()
        .position(|k| k.as_deref() == Some(key))
        .and_then(|i| values[i].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Array;
    use arrow_array::cast::AsArray;
    use arrow_array::types::{Float64Type, Int64Type};
    use pretty_assertions::assert_eq;

    use crate::driver::args::command_args;

    fn bulk(s: &[u8]) -> Value {
        Value::BulkString(s.to_vec())
    }

    fn run(line: &str, value: Value) -> Vec<ResultItem> {
        let args = command_args(line, &[]).unwrap();
        render(&CommandInfo::of(&args), &args, value).unwrap()
    }

    /// The schema and the rows of a single-result-set rendering.
    fn table(items: &[ResultItem]) -> (Arc<Schema>, Vec<RecordBatch>) {
        let mut schema = None;
        let mut batches = Vec::new();
        for item in items {
            match item {
                ResultItem::Schema(s) => schema = Some(s.clone()),
                ResultItem::Batch(b) => batches.push(b.clone()),
                ResultItem::Notice(_) => {}
                ResultItem::RowsAffected(_) => panic!("unexpected RowsAffected"),
            }
        }
        (schema.expect("a schema"), batches)
    }

    fn names(schema: &Schema) -> Vec<&str> {
        schema.fields().iter().map(|f| f.name().as_str()).collect()
    }

    fn texts(batch: &RecordBatch, col: &str) -> Vec<Option<String>> {
        let a = batch.column_by_name(col).unwrap().as_string::<i32>();
        (0..a.len())
            .map(|i| a.is_valid(i).then(|| a.value(i).to_owned()))
            .collect()
    }

    fn binaries(batch: &RecordBatch, col: &str) -> Vec<Option<Vec<u8>>> {
        let a = batch.column_by_name(col).unwrap().as_binary::<i32>();
        (0..a.len())
            .map(|i| a.is_valid(i).then(|| a.value(i).to_vec()))
            .collect()
    }

    fn ints(batch: &RecordBatch, col: &str) -> Vec<Option<i64>> {
        let a = batch
            .column_by_name(col)
            .unwrap()
            .as_primitive::<Int64Type>();
        (0..a.len())
            .map(|i| a.is_valid(i).then(|| a.value(i)))
            .collect()
    }

    fn meta<'a>(schema: &'a Schema, col: &str, key: &str) -> Option<&'a str> {
        schema
            .field_with_name(col)
            .unwrap()
            .metadata()
            .get(key)
            .map(String::as_str)
    }

    /// A reply, its `type`, its `value` text and its `raw` bytes.
    type Case<'a> = (Value, &'a str, Option<&'a str>, Option<&'a [u8]>);

    #[test]
    fn scalars_render_one_row_per_resp_type() {
        let big = Value::BigNumber(b"3492890328409238509324850943850943825024385".to_vec());
        let cases: Vec<Case<'_>> = vec![
            (bulk(b"hello"), "string", Some("hello"), Some(b"hello")),
            (
                bulk(&[0, 0xff, 0xfe, b'\n']),
                "string",
                Some("\\x00\\xff\\xfe\\n"),
                Some(&[0, 0xff, 0xfe, b'\n']),
            ),
            (
                bulk("grüße".as_bytes()),
                "string",
                Some("grüße"),
                Some("grüße".as_bytes()),
            ),
            (Value::Okay, "status", Some("OK"), None),
            (
                Value::SimpleString("PONG".into()),
                "status",
                Some("PONG"),
                None,
            ),
            (Value::Int(42), "integer", Some("42"), None),
            (Value::Double(1.5), "double", Some("1.5"), None),
            (Value::Double(f64::INFINITY), "double", Some("inf"), None),
            (Value::Boolean(true), "boolean", Some("true"), None),
            (Value::Nil, "null", None, None),
            (
                big,
                "bignumber",
                Some("3492890328409238509324850943850943825024385"),
                None,
            ),
            (
                Value::VerbatimString {
                    format: redis::VerbatimFormat::Text,
                    text: "some text".into(),
                },
                "verbatim",
                Some("some text"),
                None,
            ),
        ];
        for (value, kind, text, raw) in cases {
            let items = run("GET k", value);
            let (schema, batches) = table(&items);
            assert_eq!(names(&schema), ["type", "value", "raw"]);
            assert_eq!(texts(&batches[0], "type"), [Some(kind.to_owned())]);
            assert_eq!(
                texts(&batches[0], "value"),
                [text.map(str::to_owned)],
                "{kind}"
            );
            assert_eq!(
                binaries(&batches[0], "raw"),
                [raw.map(<[u8]>::to_vec)],
                "{kind}"
            );
            assert_eq!(meta(&schema, "raw", META_ENCODING), Some("binary"));
            assert_eq!(meta(&schema, "value", META_NATIVE_TYPE), Some(kind));
        }
    }

    #[test]
    fn arrays_render_a_row_per_element_with_nested_json() {
        let value = Value::Array(vec![
            bulk(b"a"),
            Value::Int(2),
            Value::Nil,
            Value::Array(vec![bulk(b"x"), bulk(&[0xff]), Value::Double(f64::NAN)]),
            Value::Map(vec![(bulk(b"k"), Value::Boolean(false))]),
            Value::Map(vec![(Value::Array(vec![]), Value::Int(1))]),
        ]);
        let items = run("SCAN 0", value);
        let (schema, batches) = table(&items);
        assert_eq!(names(&schema), ["#", "type", "value", "raw"]);
        let b = &batches[0];
        assert_eq!(ints(b, "#"), [1, 2, 3, 4, 5, 6].map(Some));
        assert_eq!(
            texts(b, "type"),
            ["string", "integer", "null", "array", "map", "map"].map(|s| Some(s.to_owned()))
        );
        assert_eq!(
            texts(b, "value"),
            [
                Some("a".to_owned()),
                Some("2".to_owned()),
                None,
                Some(r#"["x",{"hex":"ff"},"NaN"]"#.to_owned()),
                Some(r#"{"k":false}"#.to_owned()),
                Some("[[[],1]]".to_owned()),
            ]
        );
        assert_eq!(meta(&schema, "value", META_NATIVE_TYPE), Some("mixed"));
        assert_eq!(meta(&schema, "value", META_EXTENSION_NAME), None);

        // Only aggregates: the column is JSON.
        let items = run(
            "CLUSTER SLOTS",
            Value::Array(vec![Value::Array(vec![Value::Int(0)])]),
        );
        let (schema, _) = table(&items);
        assert_eq!(
            meta(&schema, "value", META_EXTENSION_NAME),
            Some(JSON_EXTENSION)
        );

        // An empty array is an empty result set.
        let items = run("KEYS nothing:*", Value::Array(vec![]));
        let (schema, batches) = table(&items);
        assert_eq!(names(&schema), ["#", "type", "value", "raw"]);
        assert_eq!(batches, Vec::<RecordBatch>::new());
    }

    #[test]
    fn maps_and_resp2_pairs_render_key_rows() {
        let resp3 = Value::Map(vec![
            (bulk(b"maxmemory"), bulk(b"0")),
            (Value::SimpleString("proto".into()), Value::Int(3)),
        ]);
        let resp2 = Value::Array(vec![
            bulk(b"maxmemory"),
            bulk(b"0"),
            bulk(b"proto"),
            Value::Int(3),
        ]);
        for value in [resp3, resp2] {
            let items = run("CONFIG GET *", value);
            let (schema, batches) = table(&items);
            assert_eq!(names(&schema), ["key", "key_raw", "type", "value", "raw"]);
            let b = &batches[0];
            assert_eq!(
                texts(b, "key"),
                [Some("maxmemory".to_owned()), Some("proto".to_owned())]
            );
            assert_eq!(
                texts(b, "value"),
                [Some("0".to_owned()), Some("3".to_owned())]
            );
            assert_eq!(binaries(b, "key_raw")[0], Some(b"maxmemory".to_vec()));
        }
        // HELLO in RESP2 is pairs too; an odd array stays a list.
        let items = run("HELLO", Value::Array(vec![bulk(b"server"), bulk(b"redis")]));
        assert_eq!(
            names(&table(&items).0),
            ["key", "key_raw", "type", "value", "raw"]
        );
        let items = run("CONFIG GET x", Value::Array(vec![bulk(b"x")]));
        assert_eq!(names(&table(&items).0), ["#", "type", "value", "raw"]);
    }

    #[test]
    fn write_commands_report_rows_affected() {
        assert!(matches!(
            run("DEL a b", Value::Int(2))[..],
            [ResultItem::RowsAffected(2)]
        ));
        assert!(matches!(
            run("hset h f v", Value::Int(1))[..],
            [ResultItem::RowsAffected(1)]
        ));
        // Not a write, or not a count: a result set.
        let (schema, _) = table(&run("INCR n", Value::Int(5)));
        assert_eq!(names(&schema), ["type", "value", "raw"]);
        let (schema, _) = table(&run("PEXPIRE k -1", Value::Int(-2)));
        assert_eq!(names(&schema), ["type", "value", "raw"]);
    }

    #[test]
    fn typed_shapes_for_common_reads() {
        // HGETALL, RESP3 map and RESP2 pairs.
        for value in [
            Value::Map(vec![
                (bulk(b"f1"), bulk(b"v1")),
                (bulk(b"f3"), bulk("grüße".as_bytes())),
            ]),
            Value::Array(vec![
                bulk(b"f1"),
                bulk(b"v1"),
                bulk(b"f3"),
                bulk("grüße".as_bytes()),
            ]),
        ] {
            let (schema, batches) = table(&run("HGETALL m7:hash", value));
            assert_eq!(names(&schema), ["field", "value"]);
            assert_eq!(
                binaries(&batches[0], "field"),
                [Some(b"f1".to_vec()), Some(b"f3".to_vec())]
            );
            assert_eq!(
                binaries(&batches[0], "value")[1],
                Some("grüße".as_bytes().to_vec())
            );
            assert_eq!(meta(&schema, "value", META_ENCODING), Some("binary"));
        }

        // ZRANGE WITHSCORES, RESP3 nested and RESP2 flat.
        for value in [
            Value::Array(vec![
                Value::Array(vec![bulk(b"a"), Value::Double(1.0)]),
                Value::Array(vec![bulk(b"b"), Value::Double(2.5)]),
            ]),
            Value::Array(vec![bulk(b"a"), bulk(b"1"), bulk(b"b"), bulk(b"2.5")]),
        ] {
            let (schema, batches) = table(&run("ZRANGE m7:zset 0 -1 withscores", value));
            assert_eq!(names(&schema), ["member", "score"]);
            let scores = batches[0].column(1).as_primitive::<Float64Type>();
            assert_eq!(scores.values().to_vec(), [1.0, 2.5]);
        }
        // ZRANGE without scores stays a list.
        let (schema, _) = table(&run("ZRANGE z 0 -1", Value::Array(vec![bulk(b"a")])));
        assert_eq!(names(&schema), ["#", "type", "value", "raw"]);

        // LRANGE indexes from the start argument, or from the tail with negative positions.
        let list = || Value::Array(vec![bulk(b"a"), bulk(b"b"), bulk(b"c")]);
        let (schema, batches) = table(&run("LRANGE l 5 7", list()));
        assert_eq!(names(&schema), ["index", "value"]);
        assert_eq!(ints(&batches[0], "index"), [5, 6, 7].map(Some));
        let (_, batches) = table(&run("LRANGE l -3 -1", list()));
        assert_eq!(ints(&batches[0], "index"), [-3, -2, -1].map(Some));
        let (_, batches) = table(&run("LRANGE l -100 -2", list()));
        assert_eq!(ints(&batches[0], "index"), [-4, -3, -2].map(Some));

        // SMEMBERS: RESP3 set.
        let (schema, batches) = table(&run(
            "SMEMBERS s",
            Value::Set(vec![bulk(b"a"), bulk(&[0xff])]),
        ));
        assert_eq!(names(&schema), ["member"]);
        assert_eq!(
            binaries(&batches[0], "member"),
            [Some(b"a".to_vec()), Some(vec![0xff])]
        );

        // XRANGE: id and a JSON object of the fields.
        let entry = |id: &[u8], v: &[u8]| {
            Value::Array(vec![bulk(id), Value::Array(vec![bulk(b"k"), bulk(v)])])
        };
        let (schema, batches) = table(&run(
            "XRANGE s - +",
            Value::Array(vec![entry(b"1-0", b"v1"), entry(b"1-1", &[0xff])]),
        ));
        assert_eq!(names(&schema), ["id", "fields"]);
        assert_eq!(
            texts(&batches[0], "id"),
            [Some("1-0".to_owned()), Some("1-1".to_owned())]
        );
        assert_eq!(
            texts(&batches[0], "fields"),
            [
                Some(r#"{"k":"v1"}"#.to_owned()),
                Some(r#"{"k":{"hex":"ff"}}"#.to_owned())
            ]
        );
        assert_eq!(
            meta(&schema, "fields", META_EXTENSION_NAME),
            Some(JSON_EXTENSION)
        );

        // INFO.
        let text = "# Server\r\nredis_version:8.10.3\r\nrun_id:abc\r\n\r\n# Clients\r\nconnected_clients:1\r\n";
        let (schema, batches) = table(&run("INFO", bulk(text.as_bytes())));
        assert_eq!(names(&schema), ["section", "key", "value"]);
        assert_eq!(
            texts(&batches[0], "section"),
            ["Server", "Server", "Clients"].map(|s| Some(s.to_owned()))
        );
        assert_eq!(texts(&batches[0], "value")[0], Some("8.10.3".to_owned()));
        assert_eq!(info_field(text, "redis_version").as_deref(), Some("8.10.3"));
        assert_eq!(info_field(text, "nothing"), None);
    }

    #[test]
    fn cursor_commands_render_elements_and_a_cursor_notice() {
        let reply = Value::Array(vec![
            bulk(b"17"),
            Value::Array(vec![bulk(b"f"), bulk(b"v")]),
        ]);
        let items = run("HSCAN h 0", reply);
        let notice = items.iter().find_map(|i| match i {
            ResultItem::Notice(n) => Some(n.message.clone()),
            _ => None,
        });
        assert!(notice.unwrap().contains("17"));
        let (schema, _) = table(&items);
        assert_eq!(names(&schema), ["field", "value"]);

        let zscan = Value::Array(vec![bulk(b"0"), Value::Array(vec![bulk(b"m"), bulk(b"3")])]);
        assert_eq!(
            names(&table(&run("ZSCAN z 0", zscan)).0),
            ["member", "score"]
        );
        let sscan = Value::Array(vec![bulk(b"0"), Value::Array(vec![bulk(b"m")])]);
        assert_eq!(names(&table(&run("SSCAN s 0", sscan)).0), ["member"]);
        // A malformed reply falls back to the generic shape.
        let odd = Value::Array(vec![bulk(b"0"), Value::Array(vec![bulk(b"f")])]);
        assert_eq!(
            names(&table(&run("HSCAN h 0", odd)).0),
            ["#", "type", "value", "raw"]
        );
    }

    #[test]
    fn large_replies_are_cut_into_batches() {
        let n = BATCH_ROWS * 2 + 10;
        let value = Value::Array((0..n).map(|i| bulk(format!("e{i}").as_bytes())).collect());
        let (schema, batches) = table(&run("LRANGE big 0 -1", value));
        assert_eq!(names(&schema), ["index", "value"]);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches.iter().map(RecordBatch::num_rows).sum::<usize>(), n);
        assert_eq!(
            ints(&batches[2], "index").last().copied().flatten(),
            Some(i64::try_from(n - 1).unwrap())
        );
    }

    #[test]
    fn errors_inside_replies_are_rows() {
        let exec = redis::parse_redis_value(b"*2\r\n+OK\r\n-WRONGTYPE Operation against a key\r\n")
            .unwrap();
        let (_, batches) = table(&run("EXEC", exec));
        assert_eq!(
            texts(&batches[0], "type"),
            [Some("status".to_owned()), Some("error".to_owned())]
        );
        assert_eq!(
            texts(&batches[0], "value")[1].as_deref(),
            Some("WRONGTYPE Operation against a key")
        );
    }

    #[test]
    fn metadata_keys_match_the_dialect_contract() {
        assert_eq!(META_ENCODING, oxbase_dialect::keyvalue::META_ENCODING);
    }
}
