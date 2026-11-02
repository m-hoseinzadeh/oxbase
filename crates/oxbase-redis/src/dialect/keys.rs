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

use std::sync::Arc;

use async_trait::async_trait;
use oxbase_dialect::KeyValueStore;
use oxbase_driver::{CancellationToken, Connection, ResultSource};
use oxbase_types::{
    KeyAddress, KeyApplyReport, KeyApplyRequest, KeyInfo, KeyScanRequest, KeyType, Result,
};

/// The Redis key-value cell.
#[derive(Debug, Clone, Copy, Default)]
pub struct RedisKeyValueStore;

#[async_trait]
impl KeyValueStore for RedisKeyValueStore {
    fn scan(&self, request: &KeyScanRequest) -> Result<Arc<dyn ResultSource>> {
        let _ = request;
        unimplemented!("m7: redis")
    }

    fn scan_text(&self, request: &KeyScanRequest) -> String {
        let _ = request;
        unimplemented!("m7: redis")
    }

    fn read_value(&self, key: &KeyAddress, key_type: KeyType) -> Result<Arc<dyn ResultSource>> {
        let _ = (key, key_type);
        unimplemented!("m7: redis")
    }

    fn read_text(&self, key: &KeyAddress, key_type: KeyType) -> String {
        let _ = (key, key_type);
        unimplemented!("m7: redis")
    }

    async fn info(
        &self,
        c: &dyn Connection,
        key: &KeyAddress,
        cancel: CancellationToken,
    ) -> Result<KeyInfo> {
        let _ = (c, key, cancel);
        unimplemented!("m7: redis")
    }

    async fn apply(
        &self,
        c: &dyn Connection,
        request: &KeyApplyRequest,
        cancel: CancellationToken,
    ) -> Result<KeyApplyReport> {
        let _ = (c, request, cancel);
        unimplemented!("m7: redis")
    }
}
