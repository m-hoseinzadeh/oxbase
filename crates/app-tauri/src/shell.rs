//! 1.3 desktop integration (`docs/files.md`): files opened from the OS or the command line, a
//! single running instance, and keeping the workspace when the app quits.
//!
//! - **Opening files.** Paths arrive from the first instance's command line (`setup`), from a
//!   second instance (`tauri-plugin-single-instance` hands its arguments to this one and exits),
//!   or from macOS (`RunEvent::Opened`). [`paths_from_args`] keeps the arguments that are
//!   existing files (flags such as `--e2e-file-dir=` are skipped). Each path becomes an
//!   [`OpenRequest`]: a SQLite database (by extension, checked for the SQLite header) gets its
//!   saved connection (`oxbase_services::profiles::sqlite_profile_for_file`); anything else is
//!   read as a query file (`src/documents.rs`). Nothing is run.
//! - **Events.** The UI subscribes once with `app_events_subscribe` (a `Channel`, so no event
//!   permission is needed). Paths that arrive before that are queued and sent on subscribe.
//! - **Quitting.** Closing the main window (or quitting on macOS) is held while the UI keeps its
//!   workspace: Rust sends [`AppEvent::CloseRequested`], the UI saves and calls
//!   `app_close_ready`, which closes the window. Without a subscribed UI, or when the UI does not
//!   answer within [`CLOSE_TIMEOUT`], the app closes anyway.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use oxbase_types::{Error, Result};
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

use crate::connections::ConnectionsState;
use crate::documents::{DocumentsState, OpenedDocument};
use crate::error::CommandError;

/// How long a close waits for the UI to keep its workspace.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

/// Most files one launch opens.
pub const MAX_OPEN_PATHS: usize = 32;

/// Extensions opened as SQLite databases (lowercase).
pub const SQLITE_EXTENSIONS: &[&str] = &["sqlite", "sqlite3", "db", "db3"];

/// The first 16 bytes of every SQLite database file.
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// A file the OS or the command line asked the app to open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum OpenRequest {
    /// A query file, to show in a query tab.
    Document { opened: OpenedDocument },
    /// A SQLite database and its saved connection, to connect.
    Sqlite {
        profile_id: Uuid,
        name: String,
        created: bool,
    },
    /// A file that could not be opened, and why.
    Failed { name: String, message: String },
}

/// What Rust tells the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AppEvent {
    /// Files to open.
    Open { requests: Vec<OpenRequest> },
    /// The app is about to close: keep the workspace, then call `app_close_ready`.
    CloseRequested,
}

#[derive(Default)]
struct Inner {
    channel: Option<Channel<AppEvent>>,
    queued: Vec<PathBuf>,
    close_asked: bool,
    closing: bool,
}

/// The UI's event channel, files waiting for it, and the close handshake. Managed as Tauri
/// state.
#[derive(Default)]
pub struct ShellState {
    inner: Mutex<Inner>,
}

/// The arguments (without the program name) that are existing files, made absolute against
/// `cwd`; flags (`-x`, `--name=value`) and macOS's `-psn_...` are skipped. At most
/// [`MAX_OPEN_PATHS`].
pub fn paths_from_args(args: impl IntoIterator<Item = OsString>, cwd: &Path) -> Vec<PathBuf> {
    args.into_iter()
        .filter(|arg| !arg.to_string_lossy().starts_with('-'))
        .map(|arg| {
            let path = PathBuf::from(arg);
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        })
        .filter(|path| path.is_file())
        .take(MAX_OPEN_PATHS)
        .collect()
}

fn is_sqlite_name(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| SQLITE_EXTENSIONS.iter().any(|s| e.eq_ignore_ascii_case(s)))
}

/// Whether the file is a SQLite database: an empty file, or one with the SQLite header.
fn has_sqlite_header(path: &Path) -> Result<bool> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut header = [0_u8; 16];
    let mut read = 0;
    while read < header.len() {
        let n = file.read(&mut header[read..])?;
        if n == 0 {
            break;
        }
        read += n;
    }
    Ok(read == 0 || &header == SQLITE_HEADER)
}

fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// What opening `path` means (module docs).
async fn resolve_one(app: &AppHandle, path: PathBuf) -> OpenRequest {
    let name = display_name(&path);
    let failed = |e: Error| OpenRequest::Failed {
        name: name.clone(),
        message: CommandError::from(e).message,
    };
    if is_sqlite_name(&path) {
        let at = path.clone();
        match tauri::async_runtime::spawn_blocking(move || has_sqlite_header(&at)).await {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => {
                return failed(Error::invalid_input("the file is not a SQLite database"));
            }
            Ok(Err(e)) => return failed(e),
            Err(_) => return failed(Error::internal("a file task stopped unexpectedly")),
        }
        let repo = std::sync::Arc::clone(app.state::<ConnectionsState>().profiles());
        return match oxbase_services::profiles::sqlite_profile_for_file(repo.as_ref(), &path).await
        {
            Ok((profile, created)) => OpenRequest::Sqlite {
                profile_id: profile.id,
                name: profile.name,
                created,
            },
            Err(e) => failed(e),
        };
    }
    match app.state::<DocumentsState>().open_path(path, true).await {
        Ok(opened) => OpenRequest::Document { opened },
        Err(e) => failed(e),
    }
}

async fn resolve(app: &AppHandle, paths: Vec<PathBuf>) -> Vec<OpenRequest> {
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        out.push(resolve_one(app, path).await);
    }
    out
}

impl ShellState {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Files to open: sent to the UI now, or queued until it subscribes.
    pub fn receive(&self, app: &AppHandle, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let mut inner = self.lock();
        if inner.channel.is_none() {
            inner.queued.extend(paths);
            inner.queued.truncate(MAX_OPEN_PATHS);
            return;
        }
        drop(inner);
        Self::deliver(app.clone(), paths);
    }

    /// Resolves `paths` and sends them; a send that fails queues them again.
    fn deliver(app: AppHandle, paths: Vec<PathBuf>) {
        tauri::async_runtime::spawn(async move {
            let requests = resolve(&app, paths.clone()).await;
            let shell = app.state::<ShellState>();
            let mut inner = shell.lock();
            let sent = inner
                .channel
                .as_ref()
                .is_some_and(|channel| channel.send(AppEvent::Open { requests }).is_ok());
            if !sent {
                inner.channel = None;
                inner.queued.extend(paths);
                inner.queued.truncate(MAX_OPEN_PATHS);
            }
        });
    }

    fn subscribe(&self, app: &AppHandle, channel: Channel<AppEvent>) {
        let mut inner = self.lock();
        inner.channel = Some(channel);
        inner.close_asked = false;
        let queued = std::mem::take(&mut inner.queued);
        drop(inner);
        if !queued.is_empty() {
            Self::deliver(app.clone(), queued);
        }
    }

    /// Whether to hold a close of the main window (or a quit) while the UI keeps its
    /// workspace. The first call asks the UI and starts the [`CLOSE_TIMEOUT`].
    pub fn hold_close(&self, app: &AppHandle) -> bool {
        let mut inner = self.lock();
        if inner.closing {
            return false;
        }
        if inner.close_asked {
            return true;
        }
        let asked = inner
            .channel
            .as_ref()
            .is_some_and(|channel| channel.send(AppEvent::CloseRequested).is_ok());
        if !asked {
            inner.channel = None;
            return false;
        }
        inner.close_asked = true;
        drop(inner);
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(CLOSE_TIMEOUT).await;
            app.state::<ShellState>().finish_close(&app);
        });
        true
    }

    /// Lets the close through and closes the main window (which quits the app).
    pub fn finish_close(&self, app: &AppHandle) {
        {
            let mut inner = self.lock();
            if inner.closing {
                return;
            }
            inner.closing = true;
        }
        match app.get_webview_window("main") {
            Some(window) => {
                if window.destroy().is_err() {
                    app.exit(0);
                }
            }
            None => app.exit(0),
        }
    }
}

/// Shows the main window (a second instance or the OS opened files).
pub fn focus_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Subscribes the UI to [`AppEvent`]s; files waiting to be opened are sent at once.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn app_events_subscribe(
    app: AppHandle,
    shell: State<'_, ShellState>,
    on_event: Channel<AppEvent>,
) {
    shell.subscribe(&app, on_event);
}

/// The UI kept its workspace: close the app.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn app_close_ready(app: AppHandle, shell: State<'_, ShellState>) {
    shell.finish_close(&app);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_existing_files_are_taken_from_the_arguments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.sql"), "x").unwrap();
        std::fs::create_dir(dir.path().join("folder")).unwrap();
        let absolute = dir.path().join("b.sqlite");
        std::fs::write(&absolute, "").unwrap();
        let args = [
            "a.sql",
            "--e2e-file-dir=/tmp",
            "-psn_0_12345",
            "missing.sql",
            "folder",
        ]
        .into_iter()
        .map(OsString::from)
        .chain([absolute.clone().into_os_string()]);
        assert_eq!(
            paths_from_args(args, dir.path()),
            [dir.path().join("a.sql"), absolute]
        );
        let many = (0..MAX_OPEN_PATHS + 3).map(|_| OsString::from("a.sql"));
        assert_eq!(paths_from_args(many, dir.path()).len(), MAX_OPEN_PATHS);
    }

    #[test]
    fn sqlite_files_are_known_by_extension_and_header() {
        assert!(is_sqlite_name(Path::new("/x/orders.SQLite")));
        assert!(is_sqlite_name(Path::new("/x/app.db")));
        assert!(!is_sqlite_name(Path::new("/x/report.sql")));
        assert!(!is_sqlite_name(Path::new("/x/db")));
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.db");
        let mut bytes = SQLITE_HEADER.to_vec();
        bytes.extend_from_slice(&[0; 84]);
        std::fs::write(&real, &bytes).unwrap();
        assert!(has_sqlite_header(&real).unwrap());
        let empty = dir.path().join("empty.sqlite");
        std::fs::write(&empty, b"").unwrap();
        assert!(has_sqlite_header(&empty).unwrap());
        let thumbs = dir.path().join("Thumbs.db");
        std::fs::write(&thumbs, b"\xd0\xcf\x11\xe0 not sqlite").unwrap();
        assert!(!has_sqlite_header(&thumbs).unwrap());
    }

    #[test]
    fn events_serialize_for_the_ui() {
        let event = AppEvent::Open {
            requests: vec![
                OpenRequest::Sqlite {
                    profile_id: Uuid::nil(),
                    name: "orders.sqlite".into(),
                    created: true,
                },
                OpenRequest::Failed {
                    name: "x.db".into(),
                    message: "the file is not a SQLite database".into(),
                },
            ],
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "open");
        assert_eq!(json["requests"][0]["kind"], "sqlite");
        assert_eq!(
            json["requests"][0]["profileId"],
            "00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(json["requests"][1]["kind"], "failed");
        assert_eq!(
            serde_json::to_value(AppEvent::CloseRequested).unwrap()["type"],
            "closeRequested"
        );
    }
}
