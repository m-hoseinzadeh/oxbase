//! Keys (M7): the key browser's scans, key info, typed value reads and atomic key edits,
//! through the engine's [`KeyValueStore`](oxbase_dialect::KeyValueStore). No engine special
//! case. Contract: `docs/m7-contracts.md` §3.2, §5.2, §8.
//!
//! - [`KeyService::open_scan`] and [`KeyService::open_value`] start query runs
//!   ([`QueryService::run_source`]) in the caller's editor on the key's logical database
//!   (`RunOptions::database`), with history off: events, `query_page`, cancel and close as for
//!   `query_run`. A scan never uses `KEYS`; a value read streams in chunks.
//! - [`KeyService::info`] and [`KeyService::apply`] run on the session the tree uses for the
//!   key's database (a child session per logical database on `DatabaseScope::Session`
//!   engines), holding `SessionHandle::exclusive`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use oxbase_types::{
    Error, KeyAddress, KeyApplyReport, KeyApplyRequest, KeyInfo, KeyScanRequest, Result,
};

use crate::query::{QueryService, RunHandle, RunId, RunOptions, RunTarget};
use crate::tree::TreeService;
use crate::{CancellationToken, SessionHandle, SessionId, check_cancelled};

/// What `key_value_open` returns: the run reading the value and the key's info (its type picks
/// the typed editor).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyValueOpened {
    pub run_id: RunId,
    pub info: KeyInfo,
}

/// See the module docs.
pub struct KeyService {
    query: Arc<QueryService>,
    tree: Arc<TreeService>,
}

impl KeyService {
    pub fn new(query: Arc<QueryService>, tree: Arc<TreeService>) -> Self {
        Self { query, tree }
    }

    /// Starts a scan of every key `request` matches in editor `target.editor` (see the module
    /// docs). Columns: `key`, `key_text`, `type`, `ttl_ms`.
    pub async fn open_scan(
        &self,
        target: RunTarget,
        request: &KeyScanRequest,
        options: RunOptions,
        cancel: CancellationToken,
    ) -> Result<RunHandle> {
        check_cancelled(&cancel)?;
        request.validate()?;
        let base = self.base(target.session)?;
        let store = self
            .query
            .dialects()
            .key_value_store(base.summary().engine)?;
        let source = store.scan(request)?;
        let text = store.scan_text(request);
        let options = RunOptions {
            database: Some(request.database.clone()),
            record_history: false,
            ..options
        };
        check_cancelled(&cancel)?;
        self.query.run_source(target, text, source, options).await
    }

    /// Reads `key`'s info, then starts a run streaming its whole value in editor
    /// `target.editor`. `NotFound` when the key does not exist; `Unsupported` for a type
    /// without a typed reader.
    pub async fn open_value(
        &self,
        target: RunTarget,
        key: &KeyAddress,
        options: RunOptions,
        cancel: CancellationToken,
    ) -> Result<(RunHandle, KeyInfo)> {
        let info = self.info(target.session, key, cancel.clone()).await?;
        let base = self.base(target.session)?;
        let store = self
            .query
            .dialects()
            .key_value_store(base.summary().engine)?;
        let source = store.read_value(key, info.key_type)?;
        let text = store.read_text(key, info.key_type);
        let options = RunOptions {
            database: Some(key.database.clone()),
            record_history: false,
            ..options
        };
        check_cancelled(&cancel)?;
        let handle = self.query.run_source(target, text, source, options).await?;
        Ok((handle, info))
    }

    /// Type, TTL, length, encoding and memory of `key`; `NotFound` when it does not exist.
    pub async fn info(
        &self,
        session: SessionId,
        key: &KeyAddress,
        cancel: CancellationToken,
    ) -> Result<KeyInfo> {
        check_cancelled(&cancel)?;
        key.validate()?;
        let handle = self
            .tree
            .session_for(session, Some(&key.database), cancel.clone())
            .await?;
        let store = self
            .query
            .dialects()
            .key_value_store(handle.summary().engine)?;
        let _guard = handle.exclusive().await;
        store.info(handle.connection().as_ref(), key, cancel).await
    }

    /// Applies `request` atomically (see `KeyApplyRequest`). `InvalidInput` (nothing written)
    /// when it is malformed or a destructive edit is not confirmed.
    pub async fn apply(
        &self,
        session: SessionId,
        request: &KeyApplyRequest,
        cancel: CancellationToken,
    ) -> Result<KeyApplyReport> {
        check_cancelled(&cancel)?;
        request.validate()?;
        let handle = self
            .tree
            .session_for(session, Some(&request.address.database), cancel.clone())
            .await?;
        let store = self
            .query
            .dialects()
            .key_value_store(handle.summary().engine)?;
        let _guard = handle.exclusive().await;
        store
            .apply(handle.connection().as_ref(), request, cancel)
            .await
    }

    /// The open base session `id`.
    fn base(&self, id: SessionId) -> Result<SessionHandle> {
        self.query
            .manager()
            .get(id)
            .ok_or_else(|| Error::not_found(format!("no open session {id}")))
    }
}
