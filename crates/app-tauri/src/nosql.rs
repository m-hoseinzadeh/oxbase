//! M7 commands for MongoDB and Redis: document runs and edits, key scans, values and edits,
//! publish / subscribe. Thin wrappers over `oxbase_services::{documents, keys, pubsub}`;
//! payloads in `docs/m7-contracts.md` §8. The mongo shell and the Redis console need no command
//! of their own: they run through `query_run` / `statement_at` like SQL.
//!
//! - `document_find`, `document_aggregate`, `key_scan_open` and `key_value_open` start ordinary
//!   query runs: events on a `Channel` exactly like `query_run`, pages with `query_page`, stop
//!   with `query_cancel`, close with `query_close`.
//! - `pubsub_subscribe` streams [`PubSubEvent`]s on its own `Channel`. When the channel is gone
//!   (the panel or tab closed, the webview reloaded) the subscription is closed; the UI also
//!   calls `pubsub_unsubscribe` when the panel closes. Closing the session closes its
//!   subscriptions (`close_connection`).

use std::sync::Arc;

use oxbase_services::SessionId;
use oxbase_services::documents::DocumentService;
use oxbase_services::keys::{KeyService, KeyValueOpened};
use oxbase_services::pubsub::{PubSubEvent, PubSubService, SubscribeRequest, SubscriptionId};
use oxbase_services::query::{EditorId, RunEvent, RunHandle, RunId, RunOptions, RunTarget};
use oxbase_types::{
    AggregateRequest, BinaryText, DocumentApplyReport, DocumentEdit, DocumentQuery, FindRequest,
    KeyAddress, KeyApplyReport, KeyApplyRequest, KeyInfo, KeyScanRequest, ObjectRef,
};
use tauri::State;
use tauri::ipc::Channel;

use crate::connections::{ConnectionsState, isolated};
use crate::error::CommandError;

type CommandResult<T> = Result<T, CommandError>;

/// The services behind the M7 commands, managed as Tauri state.
#[derive(Clone)]
pub struct NoSqlState {
    documents: Arc<DocumentService>,
    keys: Arc<KeyService>,
    pubsub: Arc<PubSubService>,
}

impl NoSqlState {
    pub fn new(
        documents: Arc<DocumentService>,
        keys: Arc<KeyService>,
        pubsub: Arc<PubSubService>,
    ) -> Self {
        Self {
            documents,
            keys,
            pubsub,
        }
    }

    /// The pub/sub service (`close_connection` closes a session's subscriptions).
    pub(crate) fn pubsub(&self) -> &Arc<PubSubService> {
        &self.pubsub
    }
}

/// Forwards a run's events to the UI channel; returns the run id. A closed channel stops the
/// forwarding, not the run.
fn forward(handle: RunHandle, channel: Channel<RunEvent>) -> RunId {
    let id = handle.id;
    let mut events = handle.events;
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if channel.send(event).is_err() {
                break;
            }
        }
    });
    id
}

/// Starts `request` (a document query) in editor `editor_id` and forwards its events.
async fn open_document_query(
    connections: &ConnectionsState,
    nosql: &NoSqlState,
    target: RunTarget,
    request: DocumentQuery,
    options: Option<RunOptions>,
    on_event: Channel<RunEvent>,
    request_id: Option<String>,
) -> CommandResult<RunId> {
    let guard = connections.start_request(request_id)?;
    let documents = Arc::clone(&nosql.documents);
    let token = guard.token();
    let handle = isolated(Vec::new(), async move {
        documents
            .open(target, &request, options.unwrap_or_default(), token)
            .await
    })
    .await;
    drop(guard);
    Ok(forward(handle?, on_event))
}

/// Runs a `find` in editor `editor_id` of base session `session_id`; events like `query_run`.
#[tauri::command]
#[allow(clippy::too_many_arguments)] // Tauri maps each argument to one JSON field.
pub async fn document_find(
    connections: State<'_, ConnectionsState>,
    nosql: State<'_, NoSqlState>,
    session_id: SessionId,
    editor_id: EditorId,
    request: FindRequest,
    options: Option<RunOptions>,
    on_event: Channel<RunEvent>,
    request_id: Option<String>,
) -> CommandResult<RunId> {
    let target = RunTarget {
        session: session_id,
        editor: editor_id,
    };
    open_document_query(
        &connections,
        &nosql,
        target,
        DocumentQuery::Find(request),
        options,
        on_event,
        request_id,
    )
    .await
}

/// Runs an `aggregate` in editor `editor_id` of base session `session_id`; events like
/// `query_run`.
#[tauri::command]
#[allow(clippy::too_many_arguments)] // Tauri maps each argument to one JSON field.
pub async fn document_aggregate(
    connections: State<'_, ConnectionsState>,
    nosql: State<'_, NoSqlState>,
    session_id: SessionId,
    editor_id: EditorId,
    request: AggregateRequest,
    options: Option<RunOptions>,
    on_event: Channel<RunEvent>,
    request_id: Option<String>,
) -> CommandResult<RunId> {
    let target = RunTarget {
        session: session_id,
        editor: editor_id,
    };
    open_document_query(
        &connections,
        &nosql,
        target,
        DocumentQuery::Aggregate(request),
        options,
        on_event,
        request_id,
    )
    .await
}

/// The whole document of one row of a document run, as canonical Extended JSON text.
#[tauri::command]
pub async fn document_at(
    nosql: State<'_, NoSqlState>,
    run_id: RunId,
    result_index: usize,
    row: u64,
) -> CommandResult<String> {
    let documents = Arc::clone(&nosql.documents);
    isolated(Vec::new(), async move {
        documents.document_at(run_id, result_index, row).await
    })
    .await
}

/// One document by `_id` (Extended JSON text); `null` when there is none.
#[tauri::command]
pub async fn document_get(
    connections: State<'_, ConnectionsState>,
    nosql: State<'_, NoSqlState>,
    session_id: SessionId,
    collection: ObjectRef,
    id: String,
    request_id: Option<String>,
) -> CommandResult<Option<String>> {
    let guard = connections.start_request(request_id)?;
    let documents = Arc::clone(&nosql.documents);
    let token = guard.token();
    let result = isolated(Vec::new(), async move {
        documents.get(session_id, &collection, &id, token).await
    })
    .await;
    drop(guard);
    result
}

/// Applies document edits one by one (optimistic); per-edit outcomes in the report.
#[tauri::command]
pub async fn document_apply(
    connections: State<'_, ConnectionsState>,
    nosql: State<'_, NoSqlState>,
    session_id: SessionId,
    collection: ObjectRef,
    edits: Vec<DocumentEdit>,
    request_id: Option<String>,
) -> CommandResult<DocumentApplyReport> {
    let guard = connections.start_request(request_id)?;
    let documents = Arc::clone(&nosql.documents);
    let token = guard.token();
    let result = isolated(Vec::new(), async move {
        documents
            .apply(session_id, &collection, &edits, token)
            .await
    })
    .await;
    drop(guard);
    result
}

/// Scans every key `request` matches (`SCAN`, never `KEYS`) in editor `editor_id`; events like
/// `query_run`, rows `key`, `key_text`, `type`, `ttl_ms`.
#[tauri::command]
#[allow(clippy::too_many_arguments)] // Tauri maps each argument to one JSON field.
pub async fn key_scan_open(
    connections: State<'_, ConnectionsState>,
    nosql: State<'_, NoSqlState>,
    session_id: SessionId,
    editor_id: EditorId,
    request: KeyScanRequest,
    options: Option<RunOptions>,
    on_event: Channel<RunEvent>,
    request_id: Option<String>,
) -> CommandResult<RunId> {
    let guard = connections.start_request(request_id)?;
    let keys = Arc::clone(&nosql.keys);
    let token = guard.token();
    let target = RunTarget {
        session: session_id,
        editor: editor_id,
    };
    let handle = isolated(Vec::new(), async move {
        keys.open_scan(target, &request, options.unwrap_or_default(), token)
            .await
    })
    .await;
    drop(guard);
    Ok(forward(handle?, on_event))
}

/// Type, TTL, length, encoding and memory of one key.
#[tauri::command]
pub async fn key_info(
    connections: State<'_, ConnectionsState>,
    nosql: State<'_, NoSqlState>,
    session_id: SessionId,
    key: KeyAddress,
    request_id: Option<String>,
) -> CommandResult<KeyInfo> {
    let guard = connections.start_request(request_id)?;
    let keys = Arc::clone(&nosql.keys);
    let token = guard.token();
    let result = isolated(Vec::new(), async move {
        keys.info(session_id, &key, token).await
    })
    .await;
    drop(guard);
    result
}

/// Streams the whole value of one key in editor `editor_id` (events like `query_run`) and
/// returns the run id with the key's info (its type picks the editor).
#[tauri::command]
#[allow(clippy::too_many_arguments)] // Tauri maps each argument to one JSON field.
pub async fn key_value_open(
    connections: State<'_, ConnectionsState>,
    nosql: State<'_, NoSqlState>,
    session_id: SessionId,
    editor_id: EditorId,
    key: KeyAddress,
    options: Option<RunOptions>,
    on_event: Channel<RunEvent>,
    request_id: Option<String>,
) -> CommandResult<KeyValueOpened> {
    let guard = connections.start_request(request_id)?;
    let keys = Arc::clone(&nosql.keys);
    let token = guard.token();
    let target = RunTarget {
        session: session_id,
        editor: editor_id,
    };
    let opened = isolated(Vec::new(), async move {
        keys.open_value(target, &key, options.unwrap_or_default(), token)
            .await
    })
    .await;
    drop(guard);
    let (handle, info) = opened?;
    let run_id = forward(handle, on_event);
    Ok(KeyValueOpened { run_id, info })
}

/// Applies typed edits of one key atomically.
#[tauri::command]
pub async fn key_apply(
    connections: State<'_, ConnectionsState>,
    nosql: State<'_, NoSqlState>,
    session_id: SessionId,
    request: KeyApplyRequest,
    request_id: Option<String>,
) -> CommandResult<KeyApplyReport> {
    let guard = connections.start_request(request_id)?;
    let keys = Arc::clone(&nosql.keys);
    let token = guard.token();
    let result = isolated(Vec::new(), async move {
        keys.apply(session_id, &request, token).await
    })
    .await;
    drop(guard);
    result
}

/// Subscribes to channels / patterns; events stream on `on_event` until unsubscribed, the
/// channel closes or the session closes.
#[tauri::command]
pub async fn pubsub_subscribe(
    connections: State<'_, ConnectionsState>,
    nosql: State<'_, NoSqlState>,
    session_id: SessionId,
    request: SubscribeRequest,
    on_event: Channel<PubSubEvent>,
    request_id: Option<String>,
) -> CommandResult<SubscriptionId> {
    let guard = connections.start_request(request_id)?;
    let pubsub = Arc::clone(&nosql.pubsub);
    let token = guard.token();
    let subscribed = {
        let pubsub = Arc::clone(&pubsub);
        isolated(Vec::new(), async move {
            pubsub.subscribe(session_id, &request, token).await
        })
        .await
    };
    drop(guard);
    let handle = subscribed?;
    let id = handle.id;
    let mut events = handle.events;
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            let last = matches!(event, PubSubEvent::Closed { .. });
            if on_event.send(event).is_err() {
                // The UI is gone: stop the subscription.
                let _ = pubsub.unsubscribe(id).await;
                break;
            }
            if last {
                break;
            }
        }
    });
    Ok(id)
}

/// Closes one subscription; `false` when it was not open.
#[tauri::command]
pub async fn pubsub_unsubscribe(
    nosql: State<'_, NoSqlState>,
    subscription_id: SubscriptionId,
) -> CommandResult<bool> {
    let pubsub = Arc::clone(&nosql.pubsub);
    isolated(Vec::new(), async move {
        pubsub.unsubscribe(subscription_id).await
    })
    .await
}

/// Publishes `payload` on `channel`; returns the number of receivers.
#[tauri::command]
pub async fn pubsub_publish(
    connections: State<'_, ConnectionsState>,
    nosql: State<'_, NoSqlState>,
    session_id: SessionId,
    channel: BinaryText,
    payload: BinaryText,
    request_id: Option<String>,
) -> CommandResult<u64> {
    let guard = connections.start_request(request_id)?;
    let pubsub = Arc::clone(&nosql.pubsub);
    let token = guard.token();
    let result = isolated(Vec::new(), async move {
        pubsub.publish(session_id, &channel, &payload, token).await
    })
    .await;
    drop(guard);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{KeyType, ObjectKind};
    use serde_json::json;

    #[test]
    fn payloads_deserialize_from_the_ui_shapes() {
        let find: FindRequest = serde_json::from_value(json!({
            "collection": {"kind": "table", "catalog": "oxbase_test", "name": "m7_docs"},
            "filter": {"n": {"$gt": 1}},
            "limit": 50
        }))
        .unwrap();
        assert_eq!(find.collection.kind, ObjectKind::Table);
        assert_eq!(find.limit, Some(50));
        let edits: Vec<DocumentEdit> = serde_json::from_value(json!([
            {"type": "delete", "id": "{\"$oid\":\"65a000000000000000000001\"}"},
            {"type": "insert", "document": "{\"a\": 1}"}
        ]))
        .unwrap();
        assert_eq!(edits.len(), 2);
        let key: KeyAddress = serde_json::from_value(json!({
            "database": "0",
            "key": {"encoding": "utf8", "text": "m7:hash"}
        }))
        .unwrap();
        assert!(key.validate().is_ok());
        let opened = KeyValueOpened {
            run_id: RunId::new(),
            info: KeyInfo {
                address: key,
                key_type: KeyType::Hash,
                ttl_ms: None,
                length: Some(3),
                encoding: None,
                memory_bytes: None,
            },
        };
        let text = serde_json::to_value(&opened).unwrap();
        assert_eq!(text["info"]["keyType"], "hash");
        assert!(text["runId"].is_string());
        let request: SubscribeRequest = serde_json::from_value(json!({
            "channels": [{"encoding": "utf8", "text": "m7:news"}]
        }))
        .unwrap();
        assert!(request.validate().is_ok());
    }
}
