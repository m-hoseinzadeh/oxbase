//! M6 code insight commands: `insight_complete`, `insight_diagnose`, `insight_hover`,
//! `insight_close_editor`, `format_sql`, `schema_cache_refresh`, `schema_cache_status`,
//! `snippet_list`, `snippet_save` and `snippet_delete`. Thin wrappers over
//! `oxbase_services::insight::InsightService` (with its schema-cache and snippet services);
//! payloads in `docs/m6-contracts.md` §7.
//!
//! Completion and diagnostics requests carry the editor's id and a growing `seq`: an older
//! request answers `superseded: true` instead of running (`RequestGate`). They and
//! `schema_cache_refresh` also accept a `request_id` that `cancel_request` cancels.

use std::sync::Arc;

use oxbase_services::SessionId;
use oxbase_services::insight::{
    CompletionResponse, DiagnosticsResponse, FormatRequest, FormatResponse, HoverResponse,
    InsightRequest, InsightService,
};
use oxbase_services::schema_cache::{CacheStatus, RefreshScope};
use oxbase_types::{Engine, Snippet, SnippetInput};
use tauri::State;

use crate::connections::{ConnectionsState, isolated};
use crate::error::CommandError;

type CommandResult<T> = Result<T, CommandError>;

/// The service behind the insight commands, managed as Tauri state.
#[derive(Clone)]
pub struct InsightState {
    insight: Arc<InsightService>,
}

impl InsightState {
    pub fn new(insight: Arc<InsightService>) -> Self {
        Self { insight }
    }

    pub fn service(&self) -> &Arc<InsightService> {
        &self.insight
    }
}

/// Completions at the editor's cursor.
#[tauri::command]
pub async fn insight_complete(
    connections: State<'_, ConnectionsState>,
    insight: State<'_, InsightState>,
    request: InsightRequest,
    limit: Option<usize>,
    request_id: Option<String>,
) -> CommandResult<CompletionResponse> {
    let guard = connections.start_request(request_id)?;
    let service = Arc::clone(insight.service());
    let token = guard.token();
    let result = isolated(Vec::new(), async move {
        service.complete(request, limit, token).await
    })
    .await;
    drop(guard);
    result
}

/// Diagnostics of the editor's text.
#[tauri::command]
pub async fn insight_diagnose(
    connections: State<'_, ConnectionsState>,
    insight: State<'_, InsightState>,
    request: InsightRequest,
    request_id: Option<String>,
) -> CommandResult<DiagnosticsResponse> {
    let guard = connections.start_request(request_id)?;
    let service = Arc::clone(insight.service());
    let token = guard.token();
    let result = isolated(
        Vec::new(),
        async move { service.diagnose(request, token).await },
    )
    .await;
    drop(guard);
    result
}

/// What to show over the identifier at the editor's cursor (`null` when nothing).
#[tauri::command]
pub async fn insight_hover(
    insight: State<'_, InsightState>,
    request: InsightRequest,
) -> CommandResult<Option<HoverResponse>> {
    let service = Arc::clone(insight.service());
    isolated(Vec::new(), async move { service.hover(request).await }).await
}

/// Forgets an editor's pending insight requests (its tab closed).
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` and arguments by value.
pub fn insight_close_editor(insight: State<'_, InsightState>, editor_id: String) {
    insight.service().close_editor(&editor_id);
}

/// The formatted text (or the formatted statements of a range).
#[tauri::command]
pub async fn format_sql(
    insight: State<'_, InsightState>,
    request: FormatRequest,
) -> CommandResult<FormatResponse> {
    let service = Arc::clone(insight.service());
    isolated(Vec::new(), async move { service.format(request).await }).await
}

/// Reads the session's catalog now (whole or a scope) and returns the cache status.
#[tauri::command]
pub async fn schema_cache_refresh(
    connections: State<'_, ConnectionsState>,
    insight: State<'_, InsightState>,
    session_id: SessionId,
    database: Option<String>,
    scope: Option<RefreshScope>,
    request_id: Option<String>,
) -> CommandResult<CacheStatus> {
    let guard = connections.start_request(request_id)?;
    let caches = Arc::clone(insight.service().caches());
    let token = guard.token();
    let result = isolated(Vec::new(), async move {
        caches
            .refresh(session_id, database, scope.unwrap_or_default(), token)
            .await
    })
    .await;
    drop(guard);
    result
}

/// The status of the session's schema cache (no network access).
#[tauri::command]
pub async fn schema_cache_status(
    insight: State<'_, InsightState>,
    session_id: SessionId,
    database: Option<String>,
) -> CommandResult<CacheStatus> {
    let caches = Arc::clone(insight.service().caches());
    isolated(Vec::new(), async move {
        caches.status(session_id, database).await
    })
    .await
}

/// User snippets then built-in ones, for `engine` when given.
#[tauri::command]
pub async fn snippet_list(
    insight: State<'_, InsightState>,
    engine: Option<Engine>,
) -> CommandResult<Vec<Snippet>> {
    let snippets = Arc::clone(insight.service().snippets());
    isolated(Vec::new(), async move { snippets.list(engine).await }).await
}

/// Creates or replaces a user snippet.
#[tauri::command]
pub async fn snippet_save(
    insight: State<'_, InsightState>,
    snippet: SnippetInput,
) -> CommandResult<Snippet> {
    let snippets = Arc::clone(insight.service().snippets());
    isolated(Vec::new(), async move { snippets.save(&snippet).await }).await
}

/// Deletes a user snippet; `false` when it did not exist.
#[tauri::command]
pub async fn snippet_delete(insight: State<'_, InsightState>, id: String) -> CommandResult<bool> {
    let snippets = Arc::clone(insight.service().snippets());
    isolated(Vec::new(), async move { snippets.delete(&id).await }).await
}

#[cfg(test)]
mod tests {
    use oxbase_services::insight::Utf16Range;
    use oxbase_store::MemoryHistoryRepository;
    use serde_json::{from_value, json};

    use super::*;

    #[test]
    fn ui_arguments_map_to_the_command_types() {
        let request: InsightRequest = from_value(json!({
            "editorId": "tab-1",
            "seq": 4,
            "engine": "mysql",
            "sessionId": "0190f5d2-6a8b-7c3e-9f00-000000000001",
            "database": "shop",
            "text": "SELECT * FROM ",
            "offset": 14
        }))
        .unwrap();
        assert_eq!(request.engine, Engine::MySql);
        assert_eq!(request.database.as_deref(), Some("shop"));
        assert_eq!(request.offset, 14);
        let format: FormatRequest = from_value(json!({
            "engine": "postgres", "text": "select 1", "range": null, "options": null
        }))
        .unwrap();
        assert_eq!(format.range, None::<Utf16Range>);
        let scope: RefreshScope =
            from_value(json!({ "type": "schemas", "names": ["public"] })).unwrap();
        assert_eq!(
            scope,
            RefreshScope::Schemas {
                names: vec!["public".into()]
            }
        );
        let input: SnippetInput = from_value(json!({
            "name": "Mine", "prefix": "mine", "body": "SELECT $0", "engines": ["sqlite"]
        }))
        .unwrap();
        assert_eq!(input.engines, vec![Engine::Sqlite]);
    }

    #[tokio::test]
    async fn snippet_commands_reach_the_service() {
        let states = crate::backend::app_state_with(None, Arc::new(MemoryHistoryRepository::new()));
        let snippets = Arc::clone(states.insight.service().snippets());
        let builtins = snippets.list(Some(Engine::Postgres)).await.unwrap();
        assert_eq!(builtins.iter().filter(|s| !s.builtin).count(), 0);
        let saved = snippets
            .save(&SnippetInput {
                id: None,
                name: "Mine".into(),
                prefix: "mine".into(),
                body: "SELECT $0".into(),
                description: String::new(),
                engines: vec![],
            })
            .await
            .unwrap();
        assert_eq!(snippets.list(None).await.unwrap()[0], saved);
        let err = CommandError::from(snippets.delete("builtin:select").await.unwrap_err());
        assert_eq!(err.category, oxbase_types::ErrorCategory::InvalidInput);
    }
}
