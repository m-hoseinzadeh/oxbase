//! M2 workbench commands: object tree, query runs, result pages, history. Thin wrappers over
//! `oxbase-services` (`TreeService`, `QueryService`, `HistoryService`); see
//! `docs/m2-contracts.md` §5 for the payloads.
//!
//! - Long calls (`tree_children`, `object_load`) accept an optional `request_id`, cancelled by
//!   the M1 `cancel_request` command.
//! - `query_run` returns the run id at once; its [`RunEvent`]s go to the UI through a
//!   `tauri::ipc::Channel`. A closed channel stops the forwarding, not the run.
//! - `query_page` returns an Arrow IPC stream as raw bytes ([`Response`]), never JSON rows.

use std::sync::Arc;

use oxbase_services::history::HistoryService;
use oxbase_services::query::{
    EditorId, QueryService, RunEvent, RunId, RunOptions, RunTarget, StatementSpan,
};
use oxbase_services::tree::{TreeNode, TreeNodeRef, TreeOptions, TreeService};
use oxbase_services::{ConnectionManager, DialectRegistry, SessionId, SessionMode};
use oxbase_store::{HistoryEntry, HistoryQuery, HistorySettings};
use oxbase_types::{Engine, ObjectDef, ObjectRef};
use tauri::State;
use tauri::ipc::{Channel, Response};
use uuid::Uuid;

use crate::connections::{ConnectionsState, isolated};
use crate::error::CommandError;

type CommandResult<T> = Result<T, CommandError>;

/// The services behind the workbench commands, managed as Tauri state.
#[derive(Clone)]
pub struct WorkbenchState {
    tree: Arc<TreeService>,
    query: Arc<QueryService>,
    history: Arc<HistoryService>,
}

impl WorkbenchState {
    pub fn new(
        tree: Arc<TreeService>,
        query: Arc<QueryService>,
        history: Arc<HistoryService>,
    ) -> Self {
        Self {
            tree,
            query,
            history,
        }
    }

    /// The object tree service (the connection commands forget its sessions on close).
    pub(crate) fn tree(&self) -> &Arc<TreeService> {
        &self.tree
    }
}

/// Children of a tree node of the open (base) session `session_id`.
#[tauri::command]
pub async fn tree_children(
    connections: State<'_, ConnectionsState>,
    workbench: State<'_, WorkbenchState>,
    session_id: SessionId,
    node: TreeNodeRef,
    options: Option<TreeOptions>,
    request_id: Option<String>,
) -> CommandResult<Vec<TreeNode>> {
    let guard = connections.start_request(request_id)?;
    let tree = Arc::clone(&workbench.tree);
    let token = guard.token();
    let result = isolated(Vec::new(), async move {
        tree.children(session_id, &node, &options.unwrap_or_default(), token)
            .await
    })
    .await;
    drop(guard);
    result
}

/// The full definition of one object.
#[tauri::command]
pub async fn object_load(
    connections: State<'_, ConnectionsState>,
    workbench: State<'_, WorkbenchState>,
    session_id: SessionId,
    object: ObjectRef,
    request_id: Option<String>,
) -> CommandResult<ObjectDef> {
    let guard = connections.start_request(request_id)?;
    let tree = Arc::clone(&workbench.tree);
    let token = guard.token();
    let result = isolated(Vec::new(), async move {
        tree.load(session_id, &object, token).await
    })
    .await;
    drop(guard);
    result
}

/// Starts running `script` in editor `editor_id` of base session `session_id`. Returns the run
/// id; events follow on `on_event`.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub async fn query_run(
    workbench: State<'_, WorkbenchState>,
    session_id: SessionId,
    editor_id: EditorId,
    script: String,
    options: Option<RunOptions>,
    on_event: Channel<RunEvent>,
) -> CommandResult<RunId> {
    let query = Arc::clone(&workbench.query);
    let target = RunTarget {
        session: session_id,
        editor: editor_id,
    };
    let handle = isolated(Vec::new(), async move {
        query
            .run(target, &script, options.unwrap_or_default())
            .await
    })
    .await?;
    let id = handle.id;
    let mut events = handle.events;
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if on_event.send(event).is_err() {
                // The webview is gone; the run goes on and stays pageable until closed.
                break;
            }
        }
    });
    Ok(id)
}

/// Rows `[offset, offset + limit)` of a result as Arrow IPC stream bytes.
#[tauri::command]
pub async fn query_page(
    workbench: State<'_, WorkbenchState>,
    run_id: RunId,
    result_index: usize,
    offset: u64,
    limit: usize,
) -> CommandResult<Response> {
    let query = Arc::clone(&workbench.query);
    let bytes = isolated(Vec::new(), async move {
        query.page_ipc(run_id, result_index, offset, limit).await
    })
    .await?;
    Ok(Response::new(bytes))
}

/// Cancels a running run. Returns whether it was running.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn query_cancel(workbench: State<'_, WorkbenchState>, run_id: RunId) -> bool {
    workbench.query.cancel(run_id)
}

/// Closes a run and deletes its stored results.
#[tauri::command]
pub async fn query_close(workbench: State<'_, WorkbenchState>, run_id: RunId) -> CommandResult<()> {
    let query = Arc::clone(&workbench.query);
    isolated(Vec::new(), async move { query.close(run_id).await }).await
}

/// Closes an editor: its runs and its session.
#[tauri::command]
pub async fn query_close_editor(
    workbench: State<'_, WorkbenchState>,
    editor_id: EditorId,
) -> CommandResult<()> {
    let query = Arc::clone(&workbench.query);
    isolated(
        Vec::new(),
        async move { query.close_editor(&editor_id).await },
    )
    .await
}

/// One open connection's database sessions, for the status bar's session counter.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsage {
    /// The connection (its base session).
    pub session_id: SessionId,
    pub profile_name: String,
    pub session_mode: SessionMode,
    /// Every session the connection holds: its own plus the children.
    pub total: usize,
    /// Query tabs keeping a session between runs (per-tab mode).
    pub idle_tabs: usize,
    /// Queries running now, each on its own session.
    pub running: usize,
    /// The rest: other databases in the tree, a schema-cache read, a grid edit, an import...
    pub other: usize,
}

/// Every open connection's sessions (no network access).
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn session_usage(
    connections: State<'_, ConnectionsState>,
    workbench: State<'_, WorkbenchState>,
) -> Vec<SessionUsage> {
    usage_of(connections.manager(), &workbench.query)
}

pub(crate) fn usage_of(manager: &ConnectionManager, query: &QueryService) -> Vec<SessionUsage> {
    manager
        .list_open()
        .into_iter()
        .map(|base| {
            let children = manager.children_of(base.id).len();
            let editors = query.editor_usage(base.id);
            SessionUsage {
                session_id: base.id,
                profile_name: base.profile_name,
                session_mode: base.session_mode,
                total: 1 + children,
                idle_tabs: editors.idle,
                running: editors.running,
                other: children.saturating_sub(editors.idle + editors.running),
            }
        })
        .collect()
}

/// The statement under the cursor; `offset` is in UTF-16 code units (Monaco `getOffsetAt`).
/// M7: every engine with an editor (SQL, and the mongo shell / Redis console languages).
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned arguments.
pub fn statement_at(
    workbench: State<'_, WorkbenchState>,
    engine: Engine,
    text: String,
    offset: usize,
) -> CommandResult<Option<StatementSpan>> {
    span_at(workbench.query.dialects(), engine, &text, offset)
}

/// [`statement_at`] against `dialects`.
fn span_at(
    dialects: &DialectRegistry,
    engine: Engine,
    text: &str,
    offset: usize,
) -> CommandResult<Option<StatementSpan>> {
    QueryService::statement_at_with(dialects, engine, text, offset).map_err(CommandError::from)
}

/// History entries, newest first.
#[tauri::command]
pub async fn history_list(
    workbench: State<'_, WorkbenchState>,
    query: Option<HistoryQuery>,
) -> CommandResult<Vec<HistoryEntry>> {
    let history = Arc::clone(&workbench.history);
    isolated(Vec::new(), async move {
        history.list(&query.unwrap_or_default()).await
    })
    .await
}

/// Deletes history entries; returns how many were removed.
#[tauri::command]
pub async fn history_delete(
    workbench: State<'_, WorkbenchState>,
    ids: Vec<Uuid>,
) -> CommandResult<u64> {
    let history = Arc::clone(&workbench.history);
    isolated(Vec::new(), async move { history.delete(&ids).await }).await
}

/// Clears the history (of one profile, or all); returns how many entries were removed.
#[tauri::command]
pub async fn history_clear(
    workbench: State<'_, WorkbenchState>,
    profile_id: Option<Uuid>,
) -> CommandResult<u64> {
    let history = Arc::clone(&workbench.history);
    isolated(Vec::new(), async move { history.clear(profile_id).await }).await
}

/// The history settings (on/off, retention).
#[tauri::command]
pub async fn history_get_settings(
    workbench: State<'_, WorkbenchState>,
) -> CommandResult<HistorySettings> {
    let history = Arc::clone(&workbench.history);
    isolated(Vec::new(), async move { history.settings().await }).await
}

/// Saves the history settings.
#[tauri::command]
pub async fn history_set_settings(
    workbench: State<'_, WorkbenchState>,
    settings: HistorySettings,
) -> CommandResult<()> {
    let history = Arc::clone(&workbench.history);
    isolated(
        Vec::new(),
        async move { history.set_settings(&settings).await },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_store::MemoryHistoryRepository;

    #[tokio::test]
    async fn history_commands_forward_to_the_service() {
        let history = Arc::new(HistoryService::new(
            Arc::new(MemoryHistoryRepository::new()),
        ));
        let off = HistorySettings {
            enabled: false,
            max_entries: 3,
        };
        history.set_settings(&off).await.unwrap();
        assert_eq!(history.settings().await.unwrap(), off);
        assert_eq!(
            history
                .repository()
                .list(&HistoryQuery::default())
                .await
                .unwrap(),
            vec![]
        );
    }

    /// The argument JSON the UI wrappers (`ui/src/api/workbench.ts`) send, mapped to the
    /// command parameter types.
    #[test]
    fn ui_arguments_map_to_the_command_types() {
        use oxbase_types::ObjectKind;
        use serde_json::{from_value, json};

        let node: TreeNodeRef = from_value(json!({
            "type": "folder",
            "parent": { "kind": "schema", "catalog": "app", "name": "public" },
            "kind": "table"
        }))
        .unwrap();
        let mut schema = ObjectRef::new(ObjectKind::Schema, "public");
        schema.catalog = Some("app".into());
        assert_eq!(
            node,
            TreeNodeRef::Folder {
                parent: schema,
                kind: ObjectKind::Table
            }
        );
        assert_eq!(
            from_value::<TreeNodeRef>(json!({ "type": "connection" })).unwrap(),
            TreeNodeRef::Connection
        );
        let routine: TreeNodeRef = from_value(json!({
            "type": "object",
            "object": { "kind": "function", "catalog": "app", "schema": "public",
                        "name": "f", "signature": "" }
        }))
        .unwrap();
        let TreeNodeRef::Object { object } = routine else {
            panic!("not an object node");
        };
        assert_eq!(object.signature.as_deref(), Some(""));

        let show: TreeOptions = from_value(json!({ "showSystem": true })).unwrap();
        assert!(show.show_system);

        // Absent run options keep their defaults (stop on error, record history).
        let options: RunOptions = from_value(json!({ "maxRows": 10, "database": "db2" })).unwrap();
        assert_eq!(options.max_rows, Some(10));
        assert_eq!(options.database.as_deref(), Some("db2"));
        assert!(options.stop_on_error && options.record_history && !options.keep_previous);

        let editor: EditorId = from_value(json!("tab-1")).unwrap();
        assert_eq!(editor.as_str(), "tab-1");
        let run: RunId = from_value(json!("0192e2a0-0000-7000-8000-000000000001")).unwrap();
        assert_eq!(run.to_string(), "0192e2a0-0000-7000-8000-000000000001");
        let session: SessionId = from_value(json!("0192e2a0-0000-7000-8000-000000000002")).unwrap();
        assert_eq!(
            session.0.to_string(),
            "0192e2a0-0000-7000-8000-000000000002"
        );

        let query: HistoryQuery = from_value(json!({
            "profileId": "0192e2a0-0000-7000-8000-000000000003",
            "search": "select",
            "limit": 50
        }))
        .unwrap();
        assert_eq!(query.search.as_deref(), Some("select"));
        assert_eq!(query.limit, 50);
        assert!(query.profile_id.is_some());
        let settings: HistorySettings =
            from_value(json!({ "enabled": false, "maxEntries": 0 })).unwrap();
        assert_eq!(
            settings,
            HistorySettings {
                enabled: false,
                max_entries: 0
            }
        );
        let ids: Vec<Uuid> = from_value(json!(["0192e2a0-0000-7000-8000-000000000004"])).unwrap();
        assert_eq!(ids.len(), 1);
    }

    /// Run events reach the UI as `{ "type": ..., camelCase fields }` (`RunEvent` in
    /// `ui/src/api/types.ts`).
    #[test]
    fn run_events_serialise_as_the_ui_expects() {
        let event = serde_json::to_value(RunEvent::Rows {
            result_index: 2,
            total_so_far: 1_000,
        })
        .unwrap();
        assert_eq!(
            event,
            serde_json::json!({ "type": "rows", "resultIndex": 2, "totalSoFar": 1000 })
        );
    }

    #[test]
    fn statement_at_maps_errors() {
        let dialects = DialectRegistry::new();
        let err = span_at(&dialects, Engine::MongoDb, "db.x.find()", 0).unwrap_err();
        assert_eq!(err.category, oxbase_types::ErrorCategory::Unsupported);
        let span = span_at(&dialects, Engine::Postgres, "SELECT 1;\nSELECT 2", 12)
            .unwrap()
            .unwrap();
        assert_eq!(span.index, 1);
    }
}
