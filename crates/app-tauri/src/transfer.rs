//! M5 import and export commands: `import_preview`, `import_plan`, `import_run`,
//! `import_cancel`, `export_run` and `export_cancel`. Thin wrappers over
//! `oxbase_services::import::ImportService` and `oxbase_services::export::ExportService`;
//! payloads in `docs/m5-contracts.md` §7.
//!
//! Files are named by [`FileHandle`]s from `file_pick_open` / `file_pick_save` (`src/files.rs`),
//! never by paths. `import_preview` and `import_plan` accept an optional `request_id` that
//! `cancel_request` cancels; a running import or export stops with `import_cancel` /
//! `export_cancel`.

use std::sync::Arc;

use oxbase_io::{ReadOptions, SourcePreview};
use oxbase_services::export::{
    ExportDestination, ExportEvent, ExportHandle, ExportId, ExportRequest, ExportService,
};
use oxbase_services::import::{
    ImportEvent, ImportHandle, ImportId, ImportPlan, ImportRequest, ImportService, ImportSource,
};
use tauri::State;
use tauri::ipc::Channel;

use crate::connections::{ConnectionsState, isolated};
use crate::error::CommandError;
use crate::files::{FileHandle, FilesState};

type CommandResult<T> = Result<T, CommandError>;

/// The services behind the import and export commands, managed as Tauri state.
#[derive(Clone)]
pub struct TransferState {
    import: Arc<ImportService>,
    export: Arc<ExportService>,
}

impl TransferState {
    pub fn new(import: Arc<ImportService>, export: Arc<ExportService>) -> Self {
        Self { import, export }
    }

    pub fn import(&self) -> &Arc<ImportService> {
        &self.import
    }

    pub fn export(&self) -> &Arc<ExportService> {
        &self.export
    }
}

/// Forwards an import's events to the UI channel; returns its id. A closed channel stops the
/// forwarding, not the import.
fn forward_import(handle: ImportHandle, channel: Channel<ImportEvent>) -> ImportId {
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

/// Forwards an export's events to the UI channel; returns its id.
fn forward_export(handle: ExportHandle, channel: Channel<ExportEvent>) -> ExportId {
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

/// Previews the picked file with `options`: columns with inferred types, the first rows as
/// text, what was detected.
#[tauri::command]
pub async fn import_preview(
    connections: State<'_, ConnectionsState>,
    files: State<'_, FilesState>,
    transfer: State<'_, TransferState>,
    file: FileHandle,
    options: ReadOptions,
    request_id: Option<String>,
) -> CommandResult<SourcePreview> {
    let source = ImportSource {
        path: files.read_path(file)?,
        options,
    };
    let guard = connections.start_request(request_id)?;
    let import = Arc::clone(transfer.import());
    let token = guard.token();
    let result = isolated(
        Vec::new(),
        async move { import.preview(&source, token).await },
    )
    .await;
    drop(guard);
    result
}

/// Checks `request` against its target and returns the plan (DDL, columns, method,
/// transaction policy, warnings). Changes nothing.
#[tauri::command]
pub async fn import_plan(
    connections: State<'_, ConnectionsState>,
    files: State<'_, FilesState>,
    transfer: State<'_, TransferState>,
    file: FileHandle,
    request: ImportRequest,
    request_id: Option<String>,
) -> CommandResult<ImportPlan> {
    let source = ImportSource {
        path: files.read_path(file)?,
        options: request.options.clone(),
    };
    let guard = connections.start_request(request_id)?;
    let import = Arc::clone(transfer.import());
    let token = guard.token();
    let result = isolated(Vec::new(), async move {
        import.plan(&source, &request, token).await
    })
    .await;
    drop(guard);
    result
}

/// Starts importing the picked file per `request`. Returns the import id; events follow on
/// `on_event` (`Started`, `Progress`, `RowError`, `Committed`, then one terminal event).
#[tauri::command]
pub async fn import_run(
    files: State<'_, FilesState>,
    transfer: State<'_, TransferState>,
    file: FileHandle,
    request: ImportRequest,
    on_event: Channel<ImportEvent>,
) -> CommandResult<ImportId> {
    let source = ImportSource {
        path: files.read_path(file)?,
        options: request.options.clone(),
    };
    let import = Arc::clone(transfer.import());
    let handle = isolated(
        Vec::new(),
        async move { import.start(source, request).await },
    )
    .await?;
    Ok(forward_import(handle, on_event))
}

/// Cancels a running import; `false` when it is unknown or already ended.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn import_cancel(transfer: State<'_, TransferState>, import_id: ImportId) -> bool {
    transfer.import().cancel(import_id)
}

/// Starts exporting `request` into the file picked with `file_pick_save` (the handle is used
/// up). Returns the export id; events follow on `on_event`.
#[tauri::command]
pub async fn export_run(
    files: State<'_, FilesState>,
    transfer: State<'_, TransferState>,
    file: FileHandle,
    request: ExportRequest,
    on_event: Channel<ExportEvent>,
) -> CommandResult<ExportId> {
    let (path, overwrite) = files.take_write_target(file)?;
    let destination = ExportDestination { path, overwrite };
    let export = Arc::clone(transfer.export());
    let handle = isolated(Vec::new(), async move {
        export.start(request, destination).await
    })
    .await?;
    Ok(forward_export(handle, on_event))
}

/// Cancels a running export; `false` when it is unknown or already ended.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn export_cancel(transfer: State<'_, TransferState>, export_id: ExportId) -> bool {
    transfer.export().cancel(export_id)
}
