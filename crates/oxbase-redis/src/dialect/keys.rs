//! The Redis key-value store (redis stream): key-browser scans, typed value reads in chunks,
//! key info and atomic key edits. Contract: `oxbase_dialect::keyvalue` module docs and
//! `docs/m7-contracts.md` §5.2.
//!
//! Commands per [`KeyEdit`](oxbase_types::KeyEdit): `SET [KEEPTTL]`, `PEXPIRE` / `PERSIST`,
//! `RENAMENX` / `RENAME`, `UNLINK`, `HSET` / `HSETNX`, `HDEL`, `LPUSH` / `RPUSH`, `LSET` (with
//! an `LINDEX` check), `LREM`, `SADD`, `SREM`, `ZADD`, `ZREM`, `XADD`, `XDEL`, `JSON.SET`; all of
//! one request inside `WATCH key` + `MULTI` / `EXEC` after `TYPE` (and `EXISTS` for `create`)
//! checks. Info: `TYPE`, `PTTL`, the length command of the type (`STRLEN`, `LLEN`, `HLEN`,
//! `SCARD`, `ZCARD`, `XLEN`), `OBJECT ENCODING`, `MEMORY USAGE` (the last two optional: an ACL
//! may forbid them, then `None`).
//!
//! # As built
//!
//! - **Scan** (`ScanSource`): `SCAN cursor MATCH p COUNT n [TYPE t]` per master (cluster) or on
//!   the one server, one batch per round trip that found keys; `TYPE` + `PTTL` of the page in
//!   one pipeline (a cluster: single commands side by side, since a pipeline cannot span
//!   slots). Servers before 6.0 get no `TYPE` argument and are filtered here. The stream is
//!   lazy: dropping it stops the scan.
//! - **Values** (`ValueSource`): strings `STRLEN` then `GET`, or `GETRANGE` in 1 MiB pieces
//!   above [`VALUE_CHUNK`](oxbase_dialect::keyvalue::VALUE_CHUNK) bytes, at most 64 MiB (a
//!   warning notice says when cut); lists `LRANGE` windows; hashes / sets `HSCAN` / `SSCAN`
//!   cursors (repeats dropped); sorted sets `ZRANGE start stop WITHSCORES` windows by index, so
//!   the rows come in score order; streams `XRANGE` from the id after the last one read; JSON
//!   `JSON.GET key $`, shown as the document itself (the one element of the `$` result).
//! - **Edits**: see the private `edit` module. Standalone and sentinel sessions run them on a
//!   [`RedisConnection::dedicated`](crate::RedisConnection::dedicated) connection (so nothing
//!   else on the session can slip into the `MULTI`); a cluster on the session (the key's node).
//!   After the `EXEC` the key is read back with [`KeyValueStore::info`] (after a rename: the
//!   new key; nothing after a delete or when the key expired at once).

pub(super) mod common;
mod edit;
mod source;

use std::sync::Arc;

use async_trait::async_trait;
use oxbase_dialect::KeyValueStore;
use oxbase_driver::{CancellationToken, Connection, ResultSource};
use oxbase_types::{
    BinaryText, Error, KeyAddress, KeyApplyReport, KeyApplyRequest, KeyEdit, KeyInfo,
    KeyScanRequest, KeyType, Result,
};
use redis::Value;

use common::{check, int_of, key_type_of, native, text_of};
use source::{ScanSource, ValueSource};

/// The Redis key-value cell.
#[derive(Debug, Clone, Copy, Default)]
pub struct RedisKeyValueStore;

#[async_trait]
impl KeyValueStore for RedisKeyValueStore {
    fn scan(&self, request: &KeyScanRequest) -> Result<Arc<dyn ResultSource>> {
        request.validate()?;
        Ok(Arc::new(ScanSource {
            request: request.clone(),
        }))
    }

    fn scan_text(&self, request: &KeyScanRequest) -> String {
        source::scan_text(request)
    }

    fn read_value(&self, key: &KeyAddress, key_type: KeyType) -> Result<Arc<dyn ResultSource>> {
        key.validate()?;
        if key_type == KeyType::Other {
            return Err(Error::unsupported(
                "values of this key type (a module type) cannot be read by the key browser",
            ));
        }
        Ok(Arc::new(ValueSource {
            key: key.key.to_bytes()?,
            key_type,
        }))
    }

    fn read_text(&self, key: &KeyAddress, key_type: KeyType) -> String {
        let bytes = key
            .key
            .to_bytes()
            .unwrap_or_else(|_| key.key.display().into_bytes());
        source::read_text(&bytes, key_type)
    }

    async fn info(
        &self,
        c: &dyn Connection,
        key: &KeyAddress,
        cancel: CancellationToken,
    ) -> Result<KeyInfo> {
        check(&cancel)?;
        let redis = native(c)?;
        let bytes = key.key.to_bytes()?;
        read_info(redis, key, &bytes, &cancel)
            .await?
            .ok_or_else(|| {
                Error::not_found(format!("the key {} does not exist", key.key.display()))
            })
    }

    async fn apply(
        &self,
        c: &dyn Connection,
        request: &KeyApplyRequest,
        cancel: CancellationToken,
    ) -> Result<KeyApplyReport> {
        check(&cancel)?;
        request.validate()?;
        let redis = native(c)?;
        let mut report = edit::apply(redis, request, &cancel).await?;
        if !report.committed {
            return Ok(report);
        }
        // The key afterwards: the rename's new name, nothing after a delete.
        let mut target = Some(request.address.key.clone());
        for edit in &request.edits {
            match edit {
                KeyEdit::Delete => target = None,
                KeyEdit::Rename { new_key, .. } => target = Some(new_key.clone()),
                _ => {}
            }
        }
        if let Some(target) = target {
            let address = KeyAddress::new(request.address.database.clone(), target);
            let bytes = address.key.to_bytes()?;
            report.info = read_info(redis, &address, &bytes, &cancel).await?;
        }
        Ok(report)
    }
}

/// [`KeyValueStore::info`]; `None` when the key does not exist. The commands go out side by
/// side on the session (one round trip on a multiplexed connection, routed by slot on a
/// cluster).
pub(crate) async fn read_info(
    c: &crate::RedisConnection,
    address: &KeyAddress,
    key: &[u8],
    cancel: &CancellationToken,
) -> Result<Option<KeyInfo>> {
    let one = |name: &str| {
        let mut cmd = common::cmd(name);
        cmd.arg(key);
        cmd
    };
    let (type_cmd, ttl_cmd, encoding_cmd, memory_cmd) = (
        one("TYPE"),
        one("PTTL"),
        one("OBJECT ENCODING"),
        one("MEMORY USAGE"),
    );
    let (key_type, ttl, encoding, memory) = futures::join!(
        c.command(&type_cmd, cancel),
        c.command(&ttl_cmd, cancel),
        c.command(&encoding_cmd, cancel),
        c.command(&memory_cmd, cancel),
    );
    let Some(key_type) = key_type_of(&key_type?) else {
        return Ok(None);
    };
    let ttl = int_of(&ttl?).unwrap_or(-1);
    if ttl == -2 {
        return Ok(None);
    }
    let length = match length_command(key_type) {
        Some(name) => match c.command(&one(name), cancel).await {
            Ok(v) => int_of(&v).and_then(|n| u64::try_from(n).ok()),
            Err(e) if e.is_cancelled() => return Err(e),
            Err(_) => None,
        },
        None => None,
    };
    check(cancel)?;
    Ok(Some(KeyInfo {
        address: KeyAddress::new(address.database.clone(), BinaryText::from_bytes(key)),
        key_type,
        ttl_ms: u64::try_from(ttl).ok(),
        length,
        encoding: encoding.ok().and_then(|v| match v {
            Value::Nil => None,
            v => text_of(&v),
        }),
        memory_bytes: memory
            .ok()
            .and_then(|v| int_of(&v))
            .and_then(|n| u64::try_from(n).ok()),
    }))
}

/// The command that counts the elements of a key of `key_type`.
fn length_command(key_type: KeyType) -> Option<&'static str> {
    match key_type {
        KeyType::String => Some("STRLEN"),
        KeyType::List => Some("LLEN"),
        KeyType::Hash => Some("HLEN"),
        KeyType::Set => Some("SCARD"),
        KeyType::Zset => Some("ZCARD"),
        KeyType::Stream => Some("XLEN"),
        KeyType::Json | KeyType::Other => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_checked_before_any_server_work() {
        let store = RedisKeyValueStore;
        let mut scan = KeyScanRequest::all("0");
        scan.key_type = Some(KeyType::Other);
        assert!(store.scan(&scan).is_err());
        scan.key_type = Some(KeyType::Hash);
        assert!(store.scan(&scan).is_ok());
        assert_eq!(
            store.scan_text(&scan),
            "SCAN 0 MATCH * COUNT 1000 TYPE hash"
        );
        let key = KeyAddress::new("0", BinaryText::text("m7:list"));
        assert!(store.read_value(&key, KeyType::List).is_ok());
        let err = store.read_value(&key, KeyType::Other).err().unwrap();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::Unsupported);
        assert_eq!(store.read_text(&key, KeyType::List), "LRANGE m7:list 0 999");
        let empty = KeyAddress::new("0", BinaryText::text(""));
        assert!(store.read_value(&empty, KeyType::String).is_err());
        assert_eq!(length_command(KeyType::Stream), Some("XLEN"));
        assert_eq!(length_command(KeyType::Json), None);
        assert_eq!(source::STRING_READ_LIMIT, 64 * 1024 * 1024);
    }
}
