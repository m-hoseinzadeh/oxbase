//! oxbase Tauri 2 desktop shell.
//!
//! This crate is a thin translation layer (PLAN.md §1, rule 1): it registers Tauri commands
//! and forwards them to the headless core. It must not contain business logic.
//!
//! Commands:
//! * [`app_info`] – core version and platform, shown in the UI status bar.
//! * [`demo_arrow_page`] – a small Arrow IPC stream returned as raw bytes through
//!   [`tauri::ipc::Response`], proving the binary result-page path (no JSON).

mod demo_page;

use serde::Serialize;
use tauri::ipc::Response;

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
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![app_info, demo_arrow_page])
        .run(tauri::generate_context!())
        .expect("error while running the oxbase desktop shell");
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
