//! M3 table designer commands: `designer_load`, `designer_preview`, `designer_apply` and
//! `capabilities_get`. Thin wrappers over `oxbase_services::designer::DesignerService`; payloads
//! in `docs/m3-contracts.md` §7. The UI holds the `TableDesign` it edits and sends it whole.

use std::sync::Arc;

use oxbase_designer::{DesignPreview, TableDesign};
use oxbase_services::SessionId;
use oxbase_services::designer::{
    DesignApplyOptions, DesignApplyReport, DesignerService, EngineCapabilities,
};
use oxbase_types::ObjectRef;
use tauri::State;

use crate::connections::{ConnectionsState, isolated};
use crate::error::CommandError;

type CommandResult<T> = Result<T, CommandError>;

/// The service behind the designer commands, managed as Tauri state.
#[derive(Clone)]
pub struct DesignerState {
    designer: Arc<DesignerService>,
}

impl DesignerState {
    pub fn new(designer: Arc<DesignerService>) -> Self {
        Self { designer }
    }

    pub fn service(&self) -> &Arc<DesignerService> {
        &self.designer
    }
}

/// The design of an existing table.
#[tauri::command]
pub async fn designer_load(
    connections: State<'_, ConnectionsState>,
    designer: State<'_, DesignerState>,
    session_id: SessionId,
    table: ObjectRef,
    request_id: Option<String>,
) -> CommandResult<TableDesign> {
    let guard = connections.start_request(request_id)?;
    let service = Arc::clone(designer.service());
    let token = guard.token();
    let result = isolated(Vec::new(), async move {
        service.load(session_id, &table, token).await
    })
    .await;
    drop(guard);
    result
}

/// The DDL preview of an edited design.
#[tauri::command]
pub async fn designer_preview(
    connections: State<'_, ConnectionsState>,
    designer: State<'_, DesignerState>,
    session_id: SessionId,
    design: TableDesign,
    request_id: Option<String>,
) -> CommandResult<DesignPreview> {
    let guard = connections.start_request(request_id)?;
    let service = Arc::clone(designer.service());
    let token = guard.token();
    let result = isolated(Vec::new(), async move {
        service.preview(session_id, &design, token).await
    })
    .await;
    drop(guard);
    result
}

/// Applies an edited design and returns the outcome with the reloaded design.
#[tauri::command]
pub async fn designer_apply(
    connections: State<'_, ConnectionsState>,
    designer: State<'_, DesignerState>,
    session_id: SessionId,
    design: TableDesign,
    options: Option<DesignApplyOptions>,
    request_id: Option<String>,
) -> CommandResult<DesignApplyReport> {
    let guard = connections.start_request(request_id)?;
    let service = Arc::clone(designer.service());
    let token = guard.token();
    let result = isolated(Vec::new(), async move {
        service
            .apply(session_id, &design, options.unwrap_or_default(), token)
            .await
    })
    .await;
    drop(guard);
    result
}

/// What the engine of an open session offers (designer, grid editing, sort).
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn capabilities_get(
    designer: State<'_, DesignerState>,
    session_id: SessionId,
) -> CommandResult<EngineCapabilities> {
    designer
        .service()
        .capabilities(session_id)
        .map_err(CommandError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_store::MemoryHistoryRepository;
    use oxbase_types::{ErrorCategory, ObjectKind};
    use serde_json::{from_value, json};

    #[test]
    fn ui_arguments_map_to_the_command_types() {
        let design: TableDesign = from_value(json!({
            "table": { "kind": "table", "catalog": "app", "schema": "public", "name": "t" },
            "columns": [{
                "origin": 0,
                "column": { "name": "id", "data_type": { "type": "int", "bits": 64,
                            "unsigned": false }, "native_type": "bigint", "nullable": false },
                "defaultLiteral": "0"
            }],
            "primaryKey": { "columns": ["id"] },
            "checks": [{ "def": { "expression": "id > 0" } }]
        }))
        .unwrap();
        assert!(design.is_new());
        assert_eq!(design.table.kind, ObjectKind::Table);
        assert_eq!(design.columns[0].origin, Some(0));
        assert_eq!(design.columns[0].default_literal.as_deref(), Some("0"));
        assert_eq!(design.checks[0].origin, None);
        let options: DesignApplyOptions =
            from_value(json!({ "confirmDestructive": true })).unwrap();
        assert!(options.confirm_destructive);
    }

    #[test]
    fn capabilities_of_an_unknown_session_are_not_found() {
        let states = crate::backend::app_state_with(None, Arc::new(MemoryHistoryRepository::new()));
        let err = states
            .designer
            .service()
            .capabilities(SessionId::new())
            .unwrap_err();
        assert_eq!(CommandError::from(err).category, ErrorCategory::NotFound);
    }
}
