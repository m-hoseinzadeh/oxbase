//! oxbase desktop entry point. All wiring lives in [`app_tauri::run`].

// Hide the extra console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    app_tauri::run();
}
