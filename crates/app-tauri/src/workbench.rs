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

use oxbase_services::SessionId;
use oxbase_services::history::HistoryService;
use oxbase_services::query::{
    EditorId, QueryService, RunEvent, RunId, RunOptions, RunTarget, StatementSpan,
};
use oxbase_services::tree::{TreeNode, TreeNodeRef, TreeOptions, TreeService};
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

/// The statement under the cursor; `offset` is in UTF-16 code units (Monaco `getOffsetAt`).
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned arguments.
pub fn statement_at(
    engine: Engine,
    text: String,
    offset: usize,
) -> CommandResult<Option<StatementSpan>> {
    QueryService::statement_at(engine, &text, offset).map_err(CommandError::from)
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
    use oxbase_store::{HistoryRepository, MemoryHistoryRepository};

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

    #[test]
    fn statement_at_maps_errors() {
        let err = statement_at(Engine::MongoDb, "db.x.find()".into(), 0).unwrap_err();
        assert_eq!(err.category, oxbase_types::ErrorCategory::Unsupported);
    }
}
