//! M3 table data and grid editing commands: `table_data_open`, `edit_begin`, `edit_apply` and
//! `row_get_form`. Thin wrappers over `oxbase_services::edit::EditService`; payloads in
//! `docs/m3-contracts.md` §7.
//!
//! - `table_data_open` starts an ordinary query run (events on a `Channel`, pages with the M2
//!   `query_page`, stop with `query_cancel`, close with `query_close`); its optional
//!   `request_id` cancels only the table-definition load before the run starts.
//! - The pending-changes buffer lives in the UI and goes whole to `edit_apply`; there is no
//!   server-side "begin edit" state to discard.

use std::sync::Arc;

use oxbase_services::SessionId;
use oxbase_services::edit::{ApplyReport, EditService, Editability, FormRow, PendingChanges};
use oxbase_services::query::{EditorId, RunEvent, RunHandle, RunId, RunOptions, RunTarget};
use oxbase_types::TableDataQuery;
use tauri::State;
use tauri::ipc::Channel;

use crate::connections::{ConnectionsState, isolated};
use crate::error::CommandError;

type CommandResult<T> = Result<T, CommandError>;

/// The service behind the table data and editing commands, managed as Tauri state.
#[derive(Clone)]
pub struct DataState {
    edit: Arc<EditService>,
}

impl DataState {
    pub fn new(edit: Arc<EditService>) -> Self {
        Self { edit }
    }

    pub fn service(&self) -> &Arc<EditService> {
        &self.edit
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

/// Opens `request.table`'s rows in editor `editor_id` of base session `session_id`, filtered and
/// sorted. Returns the run id; events follow on `on_event` exactly like `query_run`.
#[tauri::command]
#[allow(clippy::too_many_arguments)] // Tauri maps each argument to one JSON field.
pub async fn table_data_open(
    connections: State<'_, ConnectionsState>,
    data: State<'_, DataState>,
    session_id: SessionId,
    editor_id: EditorId,
    request: TableDataQuery,
    options: Option<RunOptions>,
    on_event: Channel<RunEvent>,
    request_id: Option<String>,
) -> CommandResult<RunId> {
    let guard = connections.start_request(request_id)?;
    let edit = Arc::clone(data.service());
    let token = guard.token();
    let target = RunTarget {
        session: session_id,
        editor: editor_id,
    };
    let handle = isolated(Vec::new(), async move {
        edit.open_table_data(target, request, options.unwrap_or_default(), token)
            .await
    })
    .await;
    drop(guard);
    Ok(forward(handle?, on_event))
}

/// Whether and how run `run_id`'s result can be edited.
#[tauri::command]
pub async fn edit_begin(
    connections: State<'_, ConnectionsState>,
    data: State<'_, DataState>,
    run_id: RunId,
    request_id: Option<String>,
) -> CommandResult<Editability> {
    let guard = connections.start_request(request_id)?;
    let edit = Arc::clone(data.service());
    let token = guard.token();
    let result = isolated(Vec::new(), async move { edit.begin(run_id, token).await }).await;
    drop(guard);
    result
}

/// Applies the UI's pending changes to run `run_id`'s table in one transaction.
#[tauri::command]
pub async fn edit_apply(
    connections: State<'_, ConnectionsState>,
    data: State<'_, DataState>,
    run_id: RunId,
    changes: PendingChanges,
    request_id: Option<String>,
) -> CommandResult<ApplyReport> {
    let guard = connections.start_request(request_id)?;
    let edit = Arc::clone(data.service());
    let token = guard.token();
    let result = isolated(Vec::new(), async move {
        edit.apply(run_id, &changes, token).await
    })
    .await;
    drop(guard);
    result
}

/// One row for the form / JSON view.
#[tauri::command]
pub async fn row_get_form(
    data: State<'_, DataState>,
    run_id: RunId,
    result_index: usize,
    row: u64,
) -> CommandResult<FormRow> {
    let edit = Arc::clone(data.service());
    isolated(Vec::new(), async move {
        edit.row_form(run_id, result_index, row).await
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_services::edit::{CellInput, EditColumn, EditOutcome, EditStatus, InputKind};
    use oxbase_types::{DataType, Filter, FilterOp, ObjectKind, Value};
    use serde_json::{from_value, json, to_value};

    /// The argument JSON the UI wrappers (`ui/src/api/data.ts`) send, mapped to the command
    /// parameter types.
    #[test]
    fn ui_arguments_map_to_the_command_types() {
        let request: TableDataQuery = from_value(json!({
            "table": { "kind": "table", "catalog": "app", "schema": "public", "name": "t" },
            "filter": {
                "type": "and",
                "items": [
                    { "type": "condition", "column": "id", "op": "gt",
                      "values": [{ "type": "text", "value": "10" }] },
                    { "type": "rawSql", "sql": "note IS NOT NULL" }
                ]
            },
            "sort": [{ "column": "id", "descending": true, "nulls": "first" }],
            "limit": 1000
        }))
        .unwrap();
        assert_eq!(request.table.kind, ObjectKind::Table);
        assert_eq!(request.sort.len(), 1);
        assert_eq!(request.limit, Some(1000));
        let Some(Filter::And { items }) = &request.filter else {
            panic!("not an and group");
        };
        assert_eq!(
            items[0],
            Filter::condition("id", FilterOp::Gt, vec![Value::Text("10".into())])
        );
        assert!(request.validate().is_ok());

        let changes: PendingChanges = from_value(json!({
            "edits": [
                { "op": "update", "row": 0, "cells": [
                    { "column": "name", "input": { "type": "text", "text": "x" } } ] },
                { "op": "insert", "cells": [] },
                { "op": "delete", "row": 5 }
            ]
        }))
        .unwrap();
        assert_eq!(changes.edits.len(), 3);
        let run: RunId = from_value(json!("0192e2a0-0000-7000-8000-000000000001")).unwrap();
        assert_eq!(run.to_string(), "0192e2a0-0000-7000-8000-000000000001");
        assert_eq!(
            from_value::<CellInput>(json!({ "type": "default" })).unwrap(),
            CellInput::Default
        );
    }

    /// Results reach the UI in the shapes of `ui/src/api/types.ts`.
    #[test]
    fn results_serialise_as_the_ui_expects() {
        let column = EditColumn {
            name: "id".into(),
            index: 0,
            data_type: DataType::INT64,
            native_type: "bigint".into(),
            nullable: false,
            has_default: true,
            key: true,
            editable: true,
            read_only_reason: None,
            input: InputKind::Integer,
        };
        let json = to_value(&column).unwrap();
        assert_eq!(
            json["dataType"],
            json!({ "type": "int", "bits": 64, "unsigned": false })
        );
        assert_eq!(json["hasDefault"], true);
        assert_eq!(json["input"], json!({ "type": "integer" }));
        let report = ApplyReport {
            committed: false,
            outcomes: vec![EditOutcome {
                status: EditStatus::Conflict,
                error: None,
                row: Some(2),
                values: None,
                warning: None,
            }],
        };
        assert_eq!(
            to_value(&report).unwrap(),
            json!({ "committed": false, "outcomes": [{ "status": "conflict", "row": 2 }] })
        );
    }
}
