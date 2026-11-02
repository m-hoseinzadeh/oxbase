//! Key-value model (M7): key types, binary-safe text, key addresses, key-browser scans and
//! typed key edits, for key-value stores (Redis). Contract: `docs/m7-contracts.md` §3.2, §5.2.
//!
//! Keys, members, fields and values are **bytes**. They travel to and from the UI as
//! [`BinaryText`]: UTF-8 text when the bytes are valid UTF-8, lowercase hex otherwise. Arrow
//! pages carry them as `Binary` columns (the grid decodes them).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::{Error, Result};

/// Most edits one key apply may carry.
pub const MAX_KEY_EDITS: usize = 10_000;
/// Default `COUNT` hint of one `SCAN` round trip in the key browser.
pub const DEFAULT_SCAN_COUNT: u32 = 1000;

/// The value type of a key (Redis `TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyType {
    String,
    List,
    Set,
    /// Sorted set.
    Zset,
    Hash,
    Stream,
    /// A RedisJSON document (`TYPE` answers `ReJSON-RL`); optional module.
    Json,
    /// Anything else `TYPE` reports (module types such as `TSDB-TYPE`, `MBbloom--`); read-only.
    Other,
}

impl KeyType {
    pub const ALL: [KeyType; 8] = [
        KeyType::String,
        KeyType::List,
        KeyType::Set,
        KeyType::Zset,
        KeyType::Hash,
        KeyType::Stream,
        KeyType::Json,
        KeyType::Other,
    ];

    /// Stable identifier (also the serde form and the `props::KEY_TYPE` text).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::List => "list",
            Self::Set => "set",
            Self::Zset => "zset",
            Self::Hash => "hash",
            Self::Stream => "stream",
            Self::Json => "json",
            Self::Other => "other",
        }
    }

    /// The type for a Redis `TYPE` reply; `None` for `none` (no such key). Unknown names are
    /// [`KeyType::Other`].
    pub fn from_type_reply(reply: &str) -> Option<Self> {
        match reply {
            "none" => None,
            "string" => Some(Self::String),
            "list" => Some(Self::List),
            "set" => Some(Self::Set),
            "zset" => Some(Self::Zset),
            "hash" => Some(Self::Hash),
            "stream" => Some(Self::Stream),
            "ReJSON-RL" => Some(Self::Json),
            _ => Some(Self::Other),
        }
    }

    /// The name to pass to `SCAN ... TYPE <name>` (`None` for [`KeyType::Other`], which has no
    /// single name).
    pub const fn scan_type_name(self) -> Option<&'static str> {
        match self {
            Self::String => Some("string"),
            Self::List => Some("list"),
            Self::Set => Some("set"),
            Self::Zset => Some("zset"),
            Self::Hash => Some("hash"),
            Self::Stream => Some("stream"),
            Self::Json => Some("ReJSON-RL"),
            Self::Other => None,
        }
    }
}

impl fmt::Display for KeyType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for KeyType {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|t| t.as_str() == s)
            .ok_or_else(|| Error::invalid_input(format!("unknown key type '{s}'")))
    }
}

/// Bytes as JSON: `{"encoding": "utf8", "text": "..."}` when they are valid UTF-8, else
/// `{"encoding": "hex", "hex": "00ff"}` (lowercase, two digits per byte).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "encoding", rename_all = "snake_case")]
pub enum BinaryText {
    Utf8 { text: String },
    Hex { hex: String },
}

impl BinaryText {
    /// UTF-8 text.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Utf8 { text: text.into() }
    }

    /// The JSON form of `bytes`: text when valid UTF-8, else hex.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(text) => Self::Utf8 {
                text: text.to_owned(),
            },
            Err(_) => Self::Hex { hex: to_hex(bytes) },
        }
    }

    /// The bytes. `InvalidInput` for hex that is not an even number of hex digits.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        match self {
            Self::Utf8 { text } => Ok(text.as_bytes().to_vec()),
            Self::Hex { hex } => from_hex(hex),
        }
    }

    /// `true` for an empty value.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Utf8 { text } => text.is_empty(),
            Self::Hex { hex } => hex.is_empty(),
        }
    }

    /// How the value is shown in one line: the text, or the bytes escaped like `redis-cli`
    /// (`\xHH` for bytes outside printable ASCII, `\\` and `\"` escaped).
    pub fn display(&self) -> String {
        match self {
            Self::Utf8 { text } => text.clone(),
            Self::Hex { hex } => from_hex(hex).map_or_else(|_| hex.clone(), |b| escape_bytes(&b)),
        }
    }
}

/// `bytes` as lowercase hex.
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// Bytes from hex (either case). `InvalidInput` for an odd length or a non-hex digit.
pub fn from_hex(hex: &str) -> Result<Vec<u8>> {
    fn digit(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let bytes = hex.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(Error::invalid_input("hex text needs two digits per byte"));
    }
    bytes
        .chunks_exact(2)
        .map(|pair| match (digit(pair[0]), digit(pair[1])) {
            (Some(hi), Some(lo)) => Ok((hi << 4) | lo),
            _ => Err(Error::invalid_input("hex text may only hold 0-9 and a-f")),
        })
        .collect()
}

/// `bytes` in `redis-cli` style: printable ASCII as is (with `\\` and `\"` escaped), `\n`,
/// `\r`, `\t` as such, every other byte as `\xHH`.
pub fn escape_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\\""),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(char::from(b)),
            _ => {
                out.push_str("\\x");
                out.push_str(&to_hex(&[b]));
            }
        }
    }
    out
}

/// One key: the logical database it lives in and its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyAddress {
    /// Logical database index as text (`"0"`); always `"0"` on a cluster.
    pub database: String,
    pub key: BinaryText,
}

impl KeyAddress {
    pub fn new(database: impl Into<String>, key: BinaryText) -> Self {
        Self {
            database: database.into(),
            key,
        }
    }

    /// `InvalidInput` for an empty key, a database that is not a number, or bad hex.
    pub fn validate(&self) -> Result<()> {
        validate_database(&self.database)?;
        if self.key.to_bytes()?.is_empty() {
            return Err(Error::invalid_input("the key name is empty"));
        }
        Ok(())
    }
}

/// `InvalidInput` unless `database` is a logical database index (`0` .. `65535`).
pub fn validate_database(database: &str) -> Result<()> {
    if database.parse::<u16>().is_err() {
        return Err(Error::invalid_input(format!(
            "'{database}' is not a logical database index"
        )));
    }
    Ok(())
}

/// What the key browser lists (`key_scan_open`): a cursor-driven `SCAN` over one logical
/// database (over every master of a cluster), streamed as Arrow pages. Never `KEYS`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyScanRequest {
    pub database: String,
    /// `MATCH` glob (`user:*`); `None` lists every key. Text: binary patterns are not offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// `TYPE` filter; `None` lists every type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_type: Option<KeyType>,
    /// `COUNT` hint per round trip (default [`DEFAULT_SCAN_COUNT`]; 1 .. 100 000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// Stop after this many keys (`None`: the whole database; the run's `maxRows` still
    /// applies).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

impl KeyScanRequest {
    /// Every key of `database`.
    pub fn all(database: impl Into<String>) -> Self {
        Self {
            database: database.into(),
            pattern: None,
            key_type: None,
            count: None,
            limit: None,
        }
    }

    /// The `COUNT` hint to send.
    pub fn count_hint(&self) -> u32 {
        self.count.unwrap_or(DEFAULT_SCAN_COUNT)
    }

    pub fn validate(&self) -> Result<()> {
        validate_database(&self.database)?;
        if let Some(count) = self.count
            && !(1..=100_000).contains(&count)
        {
            return Err(Error::invalid_input("the SCAN count must be 1 to 100000"));
        }
        if self.key_type == Some(KeyType::Other) {
            return Err(Error::invalid_input(
                "the key browser can filter by string, list, set, zset, hash, stream or json",
            ));
        }
        Ok(())
    }
}

/// What `key_info` returns about one key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyInfo {
    pub address: KeyAddress,
    pub key_type: KeyType,
    /// Milliseconds until the key expires (`PTTL`); `None` when it never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    /// String length, or members / fields / entries of a collection type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
    /// `OBJECT ENCODING` (`listpack`, `hashtable`, ...), when the server tells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// `MEMORY USAGE` in bytes, when the server tells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
}

/// One field of a stream entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamField {
    pub field: BinaryText,
    pub value: BinaryText,
}

/// Which end of a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ListEnd {
    Head,
    Tail,
}

/// One typed edit of a key (the typed editors and the key browser). See
/// [`KeyApplyRequest`] for how a list of them is applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum KeyEdit {
    /// `SET key value [KEEPTTL]` (string keys; creates the key).
    SetString {
        value: BinaryText,
        #[serde(default)]
        keep_ttl: bool,
    },
    /// `PEXPIRE key ms`, or `PERSIST key` when `ttl_ms` is `None`.
    Expire {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ttl_ms: Option<u64>,
    },
    /// `RENAMENX key new` (or `RENAME` with `overwrite`, which is destructive). Must be the last
    /// edit of a request.
    Rename {
        new_key: BinaryText,
        #[serde(default)]
        overwrite: bool,
    },
    /// `UNLINK key` (destructive). Must be the only edit of a request.
    Delete,
    /// `HSET key field value` (or `HSETNX` with `only_new`).
    HashSet {
        field: BinaryText,
        value: BinaryText,
        #[serde(default)]
        only_new: bool,
    },
    /// `HDEL key field...`.
    HashDelete { fields: Vec<BinaryText> },
    /// `LPUSH` / `RPUSH key value...`.
    ListPush {
        end: ListEnd,
        values: Vec<BinaryText>,
    },
    /// `LSET key index value`; with `expected`, only if `LINDEX key index` still returns it
    /// (checked inside the same `WATCH` / `MULTI`).
    ListSet {
        index: i64,
        value: BinaryText,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected: Option<BinaryText>,
    },
    /// `LREM key count value` (`count` 0 removes every occurrence).
    ListRemove { value: BinaryText, count: i64 },
    /// `SADD key member...`.
    SetAdd { members: Vec<BinaryText> },
    /// `SREM key member...`.
    SetRemove { members: Vec<BinaryText> },
    /// `ZADD key score member` (adds, or changes the score of an existing member). The score
    /// must be finite.
    ZsetAdd { member: BinaryText, score: f64 },
    /// `ZREM key member...`.
    ZsetRemove { members: Vec<BinaryText> },
    /// `XADD key <id or *> field value ...`.
    StreamAdd {
        /// Explicit entry id (`1700000000000-0`); `None` lets the server assign one (`*`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        fields: Vec<StreamField>,
    },
    /// `XDEL key id...`.
    StreamDelete { ids: Vec<String> },
    /// `JSON.SET key path value` (RedisJSON; `Unsupported` without the module).
    JsonSet { path: String, value: Json },
}

impl KeyEdit {
    /// `true` for edits that can lose data the user did not touch: `Delete`, and `Rename` with
    /// `overwrite`. They need `KeyApplyRequest::confirm_destructive`.
    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            Self::Delete
                | Self::Rename {
                    overwrite: true,
                    ..
                }
        )
    }

    /// The key type this edit works on; `None` for edits valid on any type (`Expire`,
    /// `Rename`, `Delete`).
    pub fn key_type(&self) -> Option<KeyType> {
        match self {
            Self::SetString { .. } => Some(KeyType::String),
            Self::Expire { .. } | Self::Rename { .. } | Self::Delete => None,
            Self::HashSet { .. } | Self::HashDelete { .. } => Some(KeyType::Hash),
            Self::ListPush { .. } | Self::ListSet { .. } | Self::ListRemove { .. } => {
                Some(KeyType::List)
            }
            Self::SetAdd { .. } | Self::SetRemove { .. } => Some(KeyType::Set),
            Self::ZsetAdd { .. } | Self::ZsetRemove { .. } => Some(KeyType::Zset),
            Self::StreamAdd { .. } | Self::StreamDelete { .. } => Some(KeyType::Stream),
            Self::JsonSet { .. } => Some(KeyType::Json),
        }
    }

    /// Checks one edit on its own: non-empty member lists, valid hex, a finite score, stream
    /// entries with at least one field.
    pub fn validate(&self) -> Result<()> {
        let check_all = |items: &[BinaryText], what: &str| -> Result<()> {
            if items.is_empty() {
                return Err(Error::invalid_input(format!("no {what} given")));
            }
            items.iter().try_for_each(|i| i.to_bytes().map(drop))
        };
        match self {
            Self::SetString { value, .. } | Self::ListRemove { value, .. } => {
                value.to_bytes().map(drop)
            }
            Self::Expire { .. } | Self::Delete => Ok(()),
            Self::Rename { new_key, .. } => {
                if new_key.to_bytes()?.is_empty() {
                    return Err(Error::invalid_input("the new key name is empty"));
                }
                Ok(())
            }
            Self::HashSet { field, value, .. } => {
                field.to_bytes()?;
                value.to_bytes().map(drop)
            }
            Self::HashDelete { fields } => check_all(fields, "fields"),
            Self::ListPush { values, .. } => check_all(values, "values"),
            Self::ListSet {
                value, expected, ..
            } => {
                value.to_bytes()?;
                expected.as_ref().map_or(Ok(()), |e| e.to_bytes().map(drop))
            }
            Self::SetAdd { members }
            | Self::SetRemove { members }
            | Self::ZsetRemove { members } => check_all(members, "members"),
            Self::ZsetAdd { member, score } => {
                if !score.is_finite() {
                    return Err(Error::invalid_input(
                        "a sorted-set score must be a finite number",
                    ));
                }
                member.to_bytes().map(drop)
            }
            Self::StreamAdd { fields, .. } => {
                if fields.is_empty() {
                    return Err(Error::invalid_input(
                        "a stream entry needs at least one field",
                    ));
                }
                fields.iter().try_for_each(|f| {
                    f.field.to_bytes()?;
                    f.value.to_bytes().map(drop)
                })
            }
            Self::StreamDelete { ids } => {
                if ids.is_empty() || ids.iter().any(String::is_empty) {
                    return Err(Error::invalid_input("no stream entry ids given"));
                }
                Ok(())
            }
            Self::JsonSet { path, .. } => {
                if path.is_empty() {
                    return Err(Error::invalid_input(
                        "a JSON path is needed ($ for the root)",
                    ));
                }
                Ok(())
            }
        }
    }
}

/// `key_apply`: edits of **one** key, applied atomically.
///
/// The engine runs them inside `WATCH key` + `MULTI` / `EXEC` on the session (one slot on a
/// cluster, since it is one key): either every edit is applied or none (`committed = false`
/// with `conflict = true` when the key changed between the checks and `EXEC`). With `create`,
/// the key must not exist yet and the first edit creates it (its type must match `key_type`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyApplyRequest {
    pub address: KeyAddress,
    /// The type the editor read; the apply is a `conflict` when the key now has another type.
    /// With `create` it may be `None` (the first edit decides the type).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_type: Option<KeyType>,
    /// The key must not exist yet (new key dialog).
    #[serde(default)]
    pub create: bool,
    pub edits: Vec<KeyEdit>,
    /// Allows destructive edits ([`KeyEdit::is_destructive`]); the UI asks first.
    #[serde(default)]
    pub confirm_destructive: bool,
}

impl KeyApplyRequest {
    /// Checks the request without the server: a valid address, 1 ..= [`MAX_KEY_EDITS`] valid
    /// edits whose types match `key_type`, `Delete` alone, `Rename` last, destructive edits
    /// confirmed, and with `create` a first edit that writes a value.
    pub fn validate(&self) -> Result<()> {
        self.address.validate()?;
        if self.edits.is_empty() {
            return Err(Error::invalid_input("there are no changes to apply"));
        }
        if self.edits.len() > MAX_KEY_EDITS {
            return Err(Error::invalid_input(format!(
                "at most {MAX_KEY_EDITS} changes of one key can be applied at once"
            )));
        }
        let last = self.edits.len() - 1;
        for (i, edit) in self.edits.iter().enumerate() {
            edit.validate()?;
            if let (Some(expected), Some(actual)) = (self.key_type, edit.key_type())
                && expected != actual
            {
                return Err(Error::invalid_input(format!(
                    "a {actual} change cannot apply to a {expected} key"
                )));
            }
            if matches!(edit, KeyEdit::Delete) && self.edits.len() > 1 {
                return Err(Error::invalid_input("a delete must be the only change"));
            }
            if matches!(edit, KeyEdit::Rename { .. }) && i != last {
                return Err(Error::invalid_input("a rename must be the last change"));
            }
            if edit.is_destructive() && !self.confirm_destructive {
                return Err(Error::invalid_input(
                    "this change deletes or overwrites data and needs confirmation",
                ));
            }
        }
        if self.create {
            let first = self.edits[0].key_type();
            if first.is_none() || (self.key_type.is_some() && first != self.key_type) {
                return Err(Error::invalid_input(
                    "a new key starts with a change that writes its value",
                ));
            }
        }
        Ok(())
    }
}

/// What `key_apply` returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyApplyReport {
    /// `EXEC` ran and every edit was applied.
    pub committed: bool,
    /// Nothing was applied because the key changed, has another type, exists although
    /// `create` was asked, or a `ListSet` found another value than `expected`.
    pub conflict: bool,
    /// Why nothing was applied (conflict or a server error), when not committed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The key afterwards (after a rename: the new key; `None` after a delete).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<KeyInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_types_map_type_replies() {
        for t in KeyType::ALL {
            assert_eq!(t.as_str().parse::<KeyType>().unwrap(), t);
            assert_eq!(serde_json::to_value(t).unwrap(), json!(t.as_str()));
        }
        assert_eq!(KeyType::from_type_reply("none"), None);
        assert_eq!(KeyType::from_type_reply("ReJSON-RL"), Some(KeyType::Json));
        assert_eq!(KeyType::from_type_reply("TSDB-TYPE"), Some(KeyType::Other));
        assert_eq!(KeyType::Json.scan_type_name(), Some("ReJSON-RL"));
        assert_eq!(KeyType::Other.scan_type_name(), None);
    }

    #[test]
    fn binary_text_keeps_every_byte() {
        let text = BinaryText::from_bytes("grüße".as_bytes());
        assert_eq!(text, BinaryText::text("grüße"));
        let raw = [0x00, 0xff, b'a', b'"', b'\\', b'\n'];
        let bin = BinaryText::from_bytes(&raw);
        assert_eq!(
            bin,
            BinaryText::Hex {
                hex: "00ff61225c0a".to_owned()
            }
        );
        assert_eq!(bin.to_bytes().unwrap(), raw);
        assert_eq!(bin.display(), "\\x00\\xffa\\\"\\\\\\n");
        assert_eq!(
            serde_json::to_value(&bin).unwrap(),
            json!({"encoding": "hex", "hex": "00ff61225c0a"})
        );
        assert_eq!(
            serde_json::to_value(BinaryText::text("a")).unwrap(),
            json!({"encoding": "utf8", "text": "a"})
        );
        assert!(from_hex("abc").is_err());
        assert!(from_hex("zz").is_err());
        assert_eq!(from_hex("ABcd").unwrap(), [0xab, 0xcd]);
    }

    #[test]
    fn scan_requests_validate() {
        let mut r = KeyScanRequest::all("0");
        assert!(r.validate().is_ok());
        assert_eq!(r.count_hint(), DEFAULT_SCAN_COUNT);
        r.count = Some(0);
        assert!(r.validate().is_err());
        r.count = Some(500);
        r.key_type = Some(KeyType::Other);
        assert!(r.validate().is_err());
        r.key_type = Some(KeyType::Hash);
        assert!(r.validate().is_ok());
        r.database = "x".to_owned();
        assert!(r.validate().is_err());
        let text = serde_json::to_value(KeyScanRequest {
            pattern: Some("m7:*".to_owned()),
            ..KeyScanRequest::all("1")
        })
        .unwrap();
        assert_eq!(text, json!({"database": "1", "pattern": "m7:*"}));
    }

    fn request(edits: Vec<KeyEdit>) -> KeyApplyRequest {
        KeyApplyRequest {
            address: KeyAddress::new("0", BinaryText::text("m7:hash")),
            key_type: Some(KeyType::Hash),
            create: false,
            edits,
            confirm_destructive: false,
        }
    }

    #[test]
    fn apply_requests_validate() {
        let set = KeyEdit::HashSet {
            field: BinaryText::text("f"),
            value: BinaryText::text("v"),
            only_new: false,
        };
        assert!(request(vec![set.clone()]).validate().is_ok());
        assert!(request(vec![]).validate().is_err());
        let wrong_type = KeyEdit::SetAdd {
            members: vec![BinaryText::text("m")],
        };
        assert!(request(vec![wrong_type]).validate().is_err());
        assert!(request(vec![KeyEdit::Delete]).validate().is_err());
        let mut confirmed = request(vec![KeyEdit::Delete]);
        confirmed.confirm_destructive = true;
        assert!(confirmed.validate().is_ok());
        confirmed.edits.push(set.clone());
        assert!(confirmed.validate().is_err());
        let rename = KeyEdit::Rename {
            new_key: BinaryText::text("m7:hash2"),
            overwrite: false,
        };
        assert!(
            request(vec![rename.clone(), set.clone()])
                .validate()
                .is_err()
        );
        assert!(request(vec![set.clone(), rename]).validate().is_ok());
        assert!(
            request(vec![KeyEdit::ZsetAdd {
                member: BinaryText::text("a"),
                score: f64::NAN
            }])
            .validate()
            .is_err()
        );
        let mut create = request(vec![KeyEdit::Expire { ttl_ms: Some(10) }]);
        create.create = true;
        assert!(create.validate().is_err());
        create.edits.insert(0, set);
        assert!(create.validate().is_ok());
        let text = serde_json::to_value(&create).unwrap();
        assert_eq!(text["edits"][0]["type"], "hashSet");
        assert_eq!(text["edits"][1]["ttlMs"], 10);
        let back: KeyApplyRequest = serde_json::from_value(text).unwrap();
        assert_eq!(back, create);
    }
}
