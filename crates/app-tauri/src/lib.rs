//! oxbase Tauri 2 desktop shell.
//!
//! This crate is a thin translation layer (PLAN.md §1, rule 1): it registers Tauri commands
//! and forwards them to the headless core. It must not contain business logic.
//!
//! Commands:
//! * `app_info` – core version and platform, shown in the UI status bar.
//! * `demo_arrow_page` – a small Arrow IPC stream returned as raw bytes through
//!   [`tauri::ipc::Response`], proving the binary result-page path (no JSON).
//! * Connections (`src/connections.rs`): `list_profiles`, `save_profile`, `delete_profile`,
//!   `test_connection`, `open_connection`, `cancel_request`, `close_connection` and
//!   `list_open_connections`, forwarded to `oxbase-services` / `oxbase-store`.
//! * Workbench (M2, `src/workbench.rs`): `tree_children`, `object_load`, `query_run` (events
//!   through a `Channel`), `query_page` (Arrow IPC bytes), `query_cancel`, `query_close`,
//!   `query_close_editor`, `statement_at`, `history_list`, `history_delete`, `history_clear`,
//!   `history_get_settings` and `history_set_settings`. Contract: `docs/m2-contracts.md` §5.
//! * Table data and editing (M3, `src/data.rs`): `table_data_open` (a query run with a typed
//!   filter and sort), `edit_begin`, `edit_apply` and `row_get_form`.
//! * Table designer (M3, `src/designer.rs`): `designer_load`, `designer_preview`,
//!   `designer_apply` and `capabilities_get`. Contract: `docs/m3-contracts.md` §7.
//!
//! * Import and export (M5): file handles from Rust-side native dialogs (`src/files.rs`:
//!   `file_pick_open`, `file_pick_save`, `file_release`) and the transfer commands
//!   (`src/transfer.rs`: `import_preview`, `import_plan`, `import_run`, `import_cancel`,
//!   `export_run`, `export_cancel`). The webview never sends a file path. Contract:
//!   `docs/m5-contracts.md` §6-7.
//!
//! * Code insight (M6, `src/insight.rs`): `insight_complete`, `insight_diagnose`,
//!   `insight_hover`, `insight_close_editor`, `format_sql`, `schema_cache_refresh`,
//!   `schema_cache_status`, `snippet_list`, `snippet_save` and `snippet_delete`. Contract:
//!   `docs/m6-contracts.md` §7.
//!
//! Every fallible command rejects with [`error::CommandError`], serialized as
//! `{ "category": "<snake_case ErrorCategory>", "message": "..." }`.
//!
//! The command list is also declared in `build.rs` (the app ACL manifest): each command needs an
//! `allow-<command>` permission in `capabilities/default.json` before the UI may call it.

mod backend;
mod connections;
mod data;
mod demo_page;
mod designer;
mod error;
mod files;
mod insight;
mod profiles;
mod transfer;
mod workbench;

use serde::Serialize;
use tauri::Manager;
use tauri::ipc::Response;

pub use error::CommandError;

/// How long quitting waits for open sessions to close.
const EXIT_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Static information about the running core, returned to the UI as JSON.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    /// Core (workspace) version, e.g. `0.0.1`.
    pub version: String,
    /// Operating system: `linux`, `macos` or `windows`.
    pub platform: String,
    /// CPU architecture, e.g. `x86_64` or `aarch64`.
    pub arch: String,
}

impl AppInfo {
    fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            platform: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
        }
    }
}

/// Returns the core version and platform.
#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo::current()
}

/// Returns a demo result page as Arrow IPC stream bytes (an `ArrayBuffer` on the JS side).
#[tauri::command]
fn demo_arrow_page() -> Result<Response, String> {
    demo_page::demo_page_bytes()
        .map(Response::new)
        .map_err(|e| e.to_string())
}

/// Builds and runs the Tauri application. Called from `main`.
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir().ok();
            let states = backend::app_state(data_dir.as_deref());
            app.manage(states.connections);
            app.manage(states.workbench);
            app.manage(states.data);
            app.manage(states.designer);
            app.manage(states.transfer);
            app.manage(states.files);
            app.manage(states.insight);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            demo_arrow_page,
            connections::list_profiles,
            connections::save_profile,
            connections::delete_profile,
            connections::test_connection,
            connections::open_connection,
            connections::cancel_request,
            connections::close_connection,
            connections::list_open_connections,
            workbench::tree_children,
            workbench::object_load,
            workbench::query_run,
            workbench::query_page,
            workbench::query_cancel,
            workbench::query_close,
            workbench::query_close_editor,
            workbench::statement_at,
            workbench::history_list,
            workbench::history_delete,
            workbench::history_clear,
            workbench::history_get_settings,
            workbench::history_set_settings,
            data::table_data_open,
            data::edit_begin,
            data::edit_apply,
            data::row_get_form,
            designer::designer_load,
            designer::designer_preview,
            designer::designer_apply,
            designer::capabilities_get,
            files::file_pick_open,
            files::file_pick_save,
            files::file_release,
            transfer::import_preview,
            transfer::import_plan,
            transfer::import_run,
            transfer::import_cancel,
            transfer::export_run,
            transfer::export_cancel,
            insight::insight_complete,
            insight::insight_diagnose,
            insight::insight_hover,
            insight::insight_close_editor,
            insight::format_sql,
            insight::schema_cache_refresh,
            insight::schema_cache_status,
            insight::snippet_list,
            insight::snippet_save,
            insight::snippet_delete,
        ])
        .build(tauri::generate_context!())
        .expect("error while building the oxbase desktop shell");

    app.run(|handle, event| {
        if let tauri::RunEvent::Exit = event
            && let Some(state) = handle.try_state::<connections::ConnectionsState>()
        {
            // Close sessions (and their SSH tunnels) cleanly, but never let a dead link or a
            // stalled tunnel keep the app from quitting: give up after `EXIT_CLOSE_TIMEOUT`.
            // Errors no longer matter here.
            let manager = std::sync::Arc::clone(state.manager());
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            tauri::async_runtime::spawn(async move {
                let _ = manager.close_all().await;
                let _ = done_tx.send(());
            });
            let _ = done_rx.recv_timeout(EXIT_CLOSE_TIMEOUT);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_info_serialises_version_and_platform() {
        let json = serde_json::to_value(AppInfo::current()).expect("serialise");
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(json["platform"], std::env::consts::OS);
        assert_eq!(json["arch"], std::env::consts::ARCH);
    }
}
