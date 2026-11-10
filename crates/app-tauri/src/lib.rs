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
//! * MongoDB and Redis (M7, `src/nosql.rs`): `document_find`, `document_aggregate`,
//!   `document_at`, `document_get`, `document_apply`, `key_scan_open`, `key_info`,
//!   `key_value_open`, `key_apply`, `pubsub_subscribe` (events through a `Channel`),
//!   `pubsub_unsubscribe` and `pubsub_publish`. The mongo shell and the Redis console run
//!   through `query_run` and `statement_at`. Contract: `docs/m7-contracts.md` §8.
//!
//! * Transfer, structure sync, data sync, backup and restore (M8, `src/sync.rs`):
//!   `transfer_plan`, `transfer_pair_report`, `transfer_run` (events through a `Channel`),
//!   `transfer_cancel`, `structure_compare`, `structure_script`, `structure_apply`,
//!   `compare_forget`, `data_compare` (`Channel`), `data_script`, `data_apply` (`Channel`),
//!   `data_cancel`, `backup_run` (`Channel`), `backup_restore` (`Channel`), `backup_verify`,
//!   `backup_list` and `backup_cancel`. Contract: `docs/m8-contracts.md` §8.
//!
//! * Query files, saved queries and the workspace (1.3, `src/documents.rs`): `document_open`,
//!   `document_reopen`, `document_recent`, `document_recent_clear`, `document_save`,
//!   `document_save_as`, `document_set_link`, `saved_query_list`, `saved_query_create`,
//!   `saved_query_rename`, `saved_query_delete`, `workspace_load` and `workspace_save`.
//!   Desktop integration (1.3, `src/shell.rs`): files opened from the OS or the command line, one
//!   running instance (`tauri-plugin-single-instance`; on macOS `src/single_instance.rs`, a
//!   per-user socket), and quitting after the UI kept its
//!   workspace: `app_events_subscribe` (events through a `Channel`) and `app_close_ready`.
//!   Guide: `docs/files.md`.
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
mod documents;
mod error;
mod files;
mod insight;
mod nosql;
mod profiles;
mod shell;
#[cfg(any(target_os = "macos", all(test, unix)))]
mod single_instance;
mod sync;
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
    /// Core (workspace) version, e.g. `1.4.0`.
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

/// A later launch handed over its command line (`argv`, the executable first) and working
/// directory: open its files here.
fn second_instance(app: &tauri::AppHandle, argv: Vec<String>, cwd: &str) {
    let args = argv.into_iter().skip(1).map(std::ffi::OsString::from);
    let paths = shell::paths_from_args(args, std::path::Path::new(cwd));
    shell::focus_main(app);
    app.state::<shell::ShellState>().receive(app, paths);
}

/// Builds and runs the Tauri application. Called from `main`.
pub fn run() {
    let builder = tauri::Builder::default();
    // First, so a second instance hands its files over before anything else starts. On macOS
    // the plugin meets at a fixed path in the shared /tmp, so Oxbase uses its own hand-over in a
    // private per-user directory there (`single_instance`).
    #[cfg(target_os = "macos")]
    let builder = builder.plugin(single_instance::init(|app, argv, cwd| {
        second_instance(app, argv, &cwd);
    }));
    #[cfg(not(target_os = "macos"))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
        second_instance(app, argv, &cwd);
    }));
    let app = builder
        .plugin(tauri_plugin_dialog::init())
        .manage(shell::ShellState::new())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event
                && window.label() == "main"
                && window
                    .state::<shell::ShellState>()
                    .hold_close(window.app_handle())
            {
                api.prevent_close();
            }
        })
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
            app.manage(states.nosql);
            app.manage(states.sync);
            app.manage(states.documents);
            // Files on the command line wait until the UI subscribes.
            if let Ok(cwd) = std::env::current_dir() {
                let paths = shell::paths_from_args(std::env::args_os().skip(1), &cwd);
                app.state::<shell::ShellState>()
                    .receive(app.handle(), paths);
            }
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
            workbench::session_usage,
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
            files::file_read_text,
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
            nosql::document_find,
            nosql::document_aggregate,
            nosql::document_at,
            nosql::document_get,
            nosql::document_apply,
            nosql::key_scan_open,
            nosql::key_info,
            nosql::key_value_open,
            nosql::key_apply,
            nosql::pubsub_subscribe,
            nosql::pubsub_unsubscribe,
            nosql::pubsub_publish,
            sync::transfer_plan,
            sync::transfer_pair_report,
            sync::transfer_run,
            sync::transfer_cancel,
            sync::structure_compare,
            sync::structure_script,
            sync::structure_apply,
            sync::compare_forget,
            sync::data_compare,
            sync::data_script,
            sync::data_apply,
            sync::data_cancel,
            sync::backup_run,
            sync::backup_restore,
            sync::backup_verify,
            sync::backup_list,
            sync::backup_cancel,
            documents::document_open,
            documents::document_reopen,
            documents::document_recent,
            documents::document_recent_clear,
            documents::document_save,
            documents::document_save_as,
            documents::document_set_link,
            documents::saved_query_list,
            documents::saved_query_create,
            documents::saved_query_rename,
            documents::saved_query_delete,
            documents::workspace_load,
            documents::workspace_save,
            shell::app_events_subscribe,
            shell::app_close_ready,
        ])
        .build(tauri::generate_context!())
        .expect("error while building the oxbase desktop shell");

    app.run(|handle, event| {
        // A quit the user asked for (macOS: Cmd+Q) waits for the UI to keep its workspace, like
        // closing the window.
        if let tauri::RunEvent::ExitRequested {
            code: None, api, ..
        } = &event
            && handle.state::<shell::ShellState>().hold_close(handle)
        {
            api.prevent_exit();
        }
        // macOS hands opened files (Finder, `open -a Oxbase file.sql`) over as an event.
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Opened { urls } = &event {
            let paths = urls
                .iter()
                .filter(|url| url.scheme() == "file")
                .filter_map(|url| url.to_file_path().ok())
                .filter(|path| path.is_file())
                .take(shell::MAX_OPEN_PATHS)
                .collect();
            shell::focus_main(handle);
            handle.state::<shell::ShellState>().receive(handle, paths);
        }
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
