//! M8 commands: data transfer, structure sync, data sync, backup and restore. Thin wrappers
//! over `oxbase_services::{transfer, sync, backup}`; payloads in `docs/m8-contracts.md` §8.
//!
//! - Request-style commands (`transfer_plan`, `transfer_pair_report`, `structure_compare`,
//!   `structure_script`, `structure_apply`, `data_script`, `backup_verify`, `backup_list`)
//!   accept an optional `request_id` that `cancel_request` cancels.
//! - Runs (`transfer_run`, `data_compare`, `data_apply`, `backup_run`, `backup_restore`) stream
//!   their events on a `Channel` and return their id; `transfer_cancel`, `data_cancel` and
//!   `backup_cancel` stop them. A closed channel stops the forwarding, not the run.
//! - `compare_forget` drops a structure or data compare the services keep by id.
//! - Backup files are file handles from `file_pick_save` / `file_pick_open` (never paths).
//! - Closing a connection cancels every M8 run that uses it (`close_connection` calls
//!   [`SyncState::cancel_session`]).

use std::sync::Arc;

use oxbase_services::SessionId;
use oxbase_services::backup::{
    BackupEvent, BackupRequest, BackupRunHandle, BackupService, Manifest, RestoreEvent,
    RestoreRequest, VerifyReport,
};
use oxbase_services::runs::OperationId;
use oxbase_services::sync::{
    DataApplyEvent, DataApplyRequest, DataCompareEvent, DataCompareRequest, DataRunHandle,
    DataScript, DataScriptRequest, DataSyncService, StructureApplyReport, StructureApplyRequest,
    StructureCompareRequest, StructureCompareResult, StructureScriptRequest, StructureSyncService,
    SyncScript,
};
use oxbase_services::transfer::{
    PairReport, TransferEvent, TransferHandle, TransferPlan, TransferRequest, TransferService,
};
use serde::Serialize;
use tauri::State;
use tauri::ipc::Channel;

use crate::connections::{ConnectionsState, isolated};
use crate::error::CommandError;
use crate::files::{FileHandle, FilesState};

type CommandResult<T> = Result<T, CommandError>;

/// The services behind the M8 commands, managed as Tauri state.
#[derive(Clone)]
pub struct SyncState {
    transfer: Arc<TransferService>,
    structure: Arc<StructureSyncService>,
    data: Arc<DataSyncService>,
    backup: Arc<BackupService>,
}

impl SyncState {
    pub fn new(
        transfer: Arc<TransferService>,
        structure: Arc<StructureSyncService>,
        data: Arc<DataSyncService>,
        backup: Arc<BackupService>,
    ) -> Self {
        Self {
            transfer,
            structure,
            data,
            backup,
        }
    }

    pub fn transfer(&self) -> &Arc<TransferService> {
        &self.transfer
    }

    pub fn structure(&self) -> &Arc<StructureSyncService> {
        &self.structure
    }

    pub fn data(&self) -> &Arc<DataSyncService> {
        &self.data
    }

    pub fn backup(&self) -> &Arc<BackupService> {
        &self.backup
    }

    /// Cancels every transfer, compare, apply, backup and restore that uses `session`;
    /// returns how many.
    pub fn cancel_session(&self, session: SessionId) -> usize {
        self.transfer.cancel_session(session)
            + self.structure.cancel_session(session)
            + self.data.cancel_session(session)
            + self.backup.cancel_session(session)
    }
}

/// Forwards a run's events to the UI channel. A closed channel stops the forwarding.
fn forward<E: Serialize + Send + 'static>(
    mut events: tokio::sync::mpsc::Receiver<E>,
    channel: Channel<E>,
) {
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            if channel.send(event).is_err() {
                break;
            }
        }
    });
}

// --- transfer --------------------------------------------------------------------------------

/// The plan of a transfer (also its dry run: target DDL, mapping report, load order).
#[tauri::command]
pub async fn transfer_plan(
    connections: State<'_, ConnectionsState>,
    sync: State<'_, SyncState>,
    request: TransferRequest,
    request_id: Option<String>,
) -> CommandResult<TransferPlan> {
    let guard = connections.start_request(request_id)?;
    let service = Arc::clone(sync.transfer());
    let token = guard.token();
    let result = isolated(
        Vec::new(),
        async move { service.plan(&request, token).await },
    )
    .await;
    drop(guard);
    result
}

/// How every representative logical type maps between the engines of two open sessions.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn transfer_pair_report(
    sync: State<'_, SyncState>,
    source_session: SessionId,
    target_session: SessionId,
) -> CommandResult<PairReport> {
    sync.transfer()
        .pair_report(source_session, target_session)
        .map_err(CommandError::from)
}

/// Starts a transfer; events follow on `on_event`.
#[tauri::command]
pub async fn transfer_run(
    sync: State<'_, SyncState>,
    request: TransferRequest,
    on_event: Channel<TransferEvent>,
) -> CommandResult<OperationId> {
    let service = Arc::clone(sync.transfer());
    let TransferHandle { id, events } =
        isolated(Vec::new(), async move { service.start(request).await }).await?;
    forward(events, on_event);
    Ok(id)
}

/// Cancels a running transfer; `false` when it is unknown or already ended.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn transfer_cancel(sync: State<'_, SyncState>, transfer_id: OperationId) -> bool {
    sync.transfer().cancel(transfer_id)
}

// --- structure sync --------------------------------------------------------------------------

/// Compares two containers; the result is kept by its `compareId`.
#[tauri::command]
pub async fn structure_compare(
    connections: State<'_, ConnectionsState>,
    sync: State<'_, SyncState>,
    request: StructureCompareRequest,
    request_id: Option<String>,
) -> CommandResult<StructureCompareResult> {
    let guard = connections.start_request(request_id)?;
    let service = Arc::clone(sync.structure());
    let token = guard.token();
    let result = isolated(
        Vec::new(),
        async move { service.compare(&request, token).await },
    )
    .await;
    drop(guard);
    result
}

/// The sync script of the selected items of a kept compare. Changes nothing.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn structure_script(
    sync: State<'_, SyncState>,
    request: StructureScriptRequest,
) -> CommandResult<SyncScript> {
    sync.structure()
        .script(&request)
        .map_err(CommandError::from)
}

/// Applies the selected items and re-compares.
#[tauri::command]
pub async fn structure_apply(
    connections: State<'_, ConnectionsState>,
    sync: State<'_, SyncState>,
    request: StructureApplyRequest,
    request_id: Option<String>,
) -> CommandResult<StructureApplyReport> {
    let guard = connections.start_request(request_id)?;
    let service = Arc::clone(sync.structure());
    let token = guard.token();
    let result = isolated(
        Vec::new(),
        async move { service.apply(&request, token).await },
    )
    .await;
    drop(guard);
    result
}

/// Drops a structure or data compare kept by the services; `false` when neither knows it.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn compare_forget(sync: State<'_, SyncState>, compare_id: OperationId) -> bool {
    let structure = sync.structure().forget(compare_id);
    let data = sync.data().forget(compare_id);
    structure || data
}

// --- data sync -------------------------------------------------------------------------------

/// Starts a data compare; events follow on `on_event`. The terminal event's result carries the
/// `compareId` for `data_script` / `data_apply`.
#[tauri::command]
pub async fn data_compare(
    sync: State<'_, SyncState>,
    request: DataCompareRequest,
    on_event: Channel<DataCompareEvent>,
) -> CommandResult<OperationId> {
    let service = Arc::clone(sync.data());
    let DataRunHandle { id, events } =
        isolated(
            Vec::new(),
            async move { service.start_compare(request).await },
        )
        .await?;
    forward(events, on_event);
    Ok(id)
}

/// The DML script preview of a kept data compare.
#[tauri::command]
pub async fn data_script(
    connections: State<'_, ConnectionsState>,
    sync: State<'_, SyncState>,
    request: DataScriptRequest,
    request_id: Option<String>,
) -> CommandResult<DataScript> {
    let guard = connections.start_request(request_id)?;
    let service = Arc::clone(sync.data());
    let token = guard.token();
    let result = isolated(
        Vec::new(),
        async move { service.script(&request, token).await },
    )
    .await;
    drop(guard);
    result
}

/// Starts applying a kept data compare; events follow on `on_event`.
#[tauri::command]
pub async fn data_apply(
    sync: State<'_, SyncState>,
    request: DataApplyRequest,
    on_event: Channel<DataApplyEvent>,
) -> CommandResult<OperationId> {
    let service = Arc::clone(sync.data());
    let DataRunHandle { id, events } =
        isolated(
            Vec::new(),
            async move { service.start_apply(request).await },
        )
        .await?;
    forward(events, on_event);
    Ok(id)
}

/// Cancels a running data compare or apply; `false` when it is unknown or already ended.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn data_cancel(sync: State<'_, SyncState>, operation_id: OperationId) -> bool {
    sync.data().cancel(operation_id)
}

// --- backup / restore ------------------------------------------------------------------------

/// Starts a backup into the file picked with `file_pick_save` (the handle is used up).
#[tauri::command]
pub async fn backup_run(
    files: State<'_, FilesState>,
    sync: State<'_, SyncState>,
    file: FileHandle,
    request: BackupRequest,
    on_event: Channel<BackupEvent>,
) -> CommandResult<OperationId> {
    let (path, overwrite) = files.take_write_target(file)?;
    let service = Arc::clone(sync.backup());
    let BackupRunHandle { id, events } = isolated(Vec::new(), async move {
        service.start_backup(request, &path, overwrite).await
    })
    .await?;
    forward(events, on_event);
    Ok(id)
}

/// Starts restoring the archive picked with `file_pick_open`.
#[tauri::command]
pub async fn backup_restore(
    files: State<'_, FilesState>,
    sync: State<'_, SyncState>,
    file: FileHandle,
    request: RestoreRequest,
    on_event: Channel<RestoreEvent>,
) -> CommandResult<OperationId> {
    let path = files.read_path(file)?;
    let service = Arc::clone(sync.backup());
    let BackupRunHandle { id, events } = isolated(Vec::new(), async move {
        service.start_restore(request, &path).await
    })
    .await?;
    forward(events, on_event);
    Ok(id)
}

/// Recomputes every checksum of the picked archive.
#[tauri::command]
pub async fn backup_verify(
    connections: State<'_, ConnectionsState>,
    files: State<'_, FilesState>,
    sync: State<'_, SyncState>,
    file: FileHandle,
    request_id: Option<String>,
) -> CommandResult<VerifyReport> {
    let path = files.read_path(file)?;
    let guard = connections.start_request(request_id)?;
    let service = Arc::clone(sync.backup());
    let token = guard.token();
    let result = isolated(
        Vec::new(),
        async move { service.verify(&path, token).await },
    )
    .await;
    drop(guard);
    result
}

/// The manifest of the picked archive (objects, engine, versions, sizes).
#[tauri::command]
pub async fn backup_list(
    connections: State<'_, ConnectionsState>,
    files: State<'_, FilesState>,
    sync: State<'_, SyncState>,
    file: FileHandle,
    request_id: Option<String>,
) -> CommandResult<Manifest> {
    let path = files.read_path(file)?;
    let guard = connections.start_request(request_id)?;
    let service = Arc::clone(sync.backup());
    let token = guard.token();
    let result = isolated(Vec::new(), async move { service.list(&path, token).await }).await;
    drop(guard);
    result
}

/// Cancels a running backup or restore; `false` when it is unknown or already ended.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn backup_cancel(sync: State<'_, SyncState>, operation_id: OperationId) -> bool {
    sync.backup().cancel(operation_id)
}
