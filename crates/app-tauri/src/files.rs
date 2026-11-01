//! M5 file handles: native open / save dialogs shown **from Rust**, returning opaque handles.
//! Contract: `docs/m5-contracts.md` §6.
//!
//! The webview never sends a file-system path to a command that reads or writes a file.
//! `file_pick_open` / `file_pick_save` show the platform dialog (`tauri-plugin-dialog`'s Rust
//! API, so the webview needs no dialog or fs permission for them), keep the chosen path in
//! [`FilesState`] and return a [`PickedFile`]: an opaque [`FileHandle`] plus the file's base
//! name for display. Import commands accept *read* handles, `export_run` a *write* handle.
//!
//! - Handles live in memory for this app run; at most [`MAX_HANDLES`] are kept (the oldest is
//!   dropped first). `file_release` drops one early.
//! - A read handle can be used any number of times (preview, plan, run). A write handle is
//!   used up by `export_run`.
//! - A write handle remembers whether the file existed when it was picked: the native save
//!   dialog asked the user to confirm replacing it, so the export may overwrite it. A file that
//!   appears after the pick is never overwritten (the export fails with "already exists").
//! - The webview chooses only the filter list, the dialog title and the suggested *base name*
//!   of a save dialog (no directories: [`validate_default_name`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use oxbase_types::{Error, ErrorCategory, Result};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use tauri_plugin_dialog::{DialogExt, FileDialogBuilder, FilePath};
use uuid::Uuid;

use crate::error::CommandError;

type CommandResult<T> = std::result::Result<T, CommandError>;

/// Most handles kept at once.
pub const MAX_HANDLES: usize = 64;

/// An opaque reference to a file the user picked in a native dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileHandle(pub Uuid);

/// What a handle may be used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAccess {
    /// Picked in an open dialog: import commands.
    Read,
    /// Picked in a save dialog: `export_run`.
    Write,
}

/// A dialog filter: a label and extensions without the dot (`["csv", "tsv"]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileFilter {
    pub name: String,
    pub extensions: Vec<String>,
}

/// What a pick returns to the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PickedFile {
    pub handle: FileHandle,
    /// The file's base name (no directories), for display.
    pub name: String,
    pub access: FileAccess,
    /// Size in bytes (read handles).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// The file existed when it was picked (write handles: the user confirmed replacing it).
    pub exists: bool,
}

struct Entry {
    path: PathBuf,
    access: FileAccess,
    existed: bool,
    seq: u64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<FileHandle, Entry>,
    next_seq: u64,
}

/// The handles of this app run, managed as Tauri state.
#[derive(Default)]
pub struct FilesState {
    inner: Mutex<Inner>,
}

fn unknown_handle() -> Error {
    Error::not_found("the file is no longer available; pick it again")
}

impl FilesState {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers a path a native dialog returned. Drops the oldest handle beyond
    /// [`MAX_HANDLES`].
    pub fn register(&self, path: PathBuf, access: FileAccess) -> PickedFile {
        let metadata = std::fs::metadata(&path).ok();
        let exists = metadata.is_some();
        let size = match access {
            FileAccess::Read => metadata.map(|m| m.len()),
            FileAccess::Write => None,
        };
        let name = base_name(&path);
        let handle = FileHandle(Uuid::new_v4());
        let mut inner = self.lock();
        let seq = inner.next_seq;
        inner.next_seq += 1;
        inner.entries.insert(
            handle,
            Entry {
                path,
                access,
                existed: exists,
                seq,
            },
        );
        while inner.entries.len() > MAX_HANDLES {
            let oldest = inner
                .entries
                .iter()
                .min_by_key(|(_, e)| e.seq)
                .map(|(h, _)| *h);
            match oldest {
                Some(h) => {
                    inner.entries.remove(&h);
                }
                None => break,
            }
        }
        PickedFile {
            handle,
            name,
            access,
            size,
            exists,
        }
    }

    /// The path of read handle `handle`. `NotFound` for an unknown or dropped handle,
    /// `InvalidInput` for a write handle.
    pub fn read_path(&self, handle: FileHandle) -> Result<PathBuf> {
        let inner = self.lock();
        let entry = inner.entries.get(&handle).ok_or_else(unknown_handle)?;
        if entry.access != FileAccess::Read {
            return Err(Error::invalid_input(
                "this file was picked for saving, not for reading",
            ));
        }
        Ok(entry.path.clone())
    }

    /// Uses up write handle `handle`: its path and whether an existing file may be replaced.
    /// `NotFound` for an unknown or used handle, `InvalidInput` for a read handle (which stays
    /// registered).
    pub fn take_write_target(&self, handle: FileHandle) -> Result<(PathBuf, bool)> {
        let mut inner = self.lock();
        match inner.entries.get(&handle) {
            None => return Err(unknown_handle()),
            Some(entry) if entry.access != FileAccess::Write => {
                return Err(Error::invalid_input(
                    "this file was picked for reading, not for saving",
                ));
            }
            Some(_) => {}
        }
        let entry = inner.entries.remove(&handle).ok_or_else(unknown_handle)?;
        Ok((entry.path, entry.existed))
    }

    /// Drops `handle`; `false` when it was unknown.
    pub fn release(&self, handle: FileHandle) -> bool {
        self.lock().entries.remove(&handle).is_some()
    }

    /// Number of live handles.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// `true` when no handle is live.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn base_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// Checks dialog filters from the UI: at most 16, names up to 100 characters, 1 to 16
/// extensions of 1 to 16 ASCII letters or digits each (`*` alone is allowed: all files).
pub fn validate_filters(filters: &[FileFilter]) -> Result<()> {
    if filters.len() > 16 {
        return Err(Error::invalid_input("too many file filters"));
    }
    for filter in filters {
        if filter.name.trim().is_empty() || filter.name.chars().count() > 100 {
            return Err(Error::invalid_input("a file filter needs a short name"));
        }
        if filter.extensions.is_empty() || filter.extensions.len() > 16 {
            return Err(Error::invalid_input(
                "a file filter needs 1 to 16 extensions",
            ));
        }
        for ext in &filter.extensions {
            let ok = ext == "*"
                || (!ext.is_empty()
                    && ext.len() <= 16
                    && ext.chars().all(|c| c.is_ascii_alphanumeric()));
            if !ok {
                return Err(Error::invalid_input(format!(
                    "'{ext}' is not a file extension (letters and digits, no dot)"
                )));
            }
        }
    }
    Ok(())
}

/// Checks a suggested save name from the UI: a base name (no `/`, `\`, `:` or control
/// characters, not `.` / `..`), 1 to 255 bytes.
pub fn validate_default_name(name: &str) -> Result<()> {
    let bad = name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| matches!(c, '/' | '\\' | ':') || c.is_control());
    if bad {
        return Err(Error::invalid_input(
            "the suggested file name must be a plain name without folders",
        ));
    }
    Ok(())
}

fn dialog(
    app: &AppHandle,
    filters: &[FileFilter],
    title: Option<String>,
) -> FileDialogBuilder<tauri::Wry> {
    let mut builder = app.dialog().file();
    for filter in filters {
        let extensions: Vec<&str> = filter.extensions.iter().map(String::as_str).collect();
        builder = builder.add_filter(filter.name.clone(), &extensions);
    }
    if let Some(title) = title {
        builder = builder.set_title(title);
    }
    builder
}

fn local_path(picked: FilePath) -> CommandResult<PathBuf> {
    picked.into_path().map_err(|_| {
        CommandError::new(
            ErrorCategory::Unsupported,
            "the chosen file is not on the local file system",
        )
    })
}

/// Shows the native open dialog. `None` when the user cancelled.
#[tauri::command]
pub async fn file_pick_open(
    app: AppHandle,
    files: State<'_, FilesState>,
    filters: Vec<FileFilter>,
    title: Option<String>,
) -> CommandResult<Option<PickedFile>> {
    validate_filters(&filters)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    dialog(&app, &filters, title).pick_file(move |picked| {
        let _ = tx.send(picked);
    });
    let Some(picked) = rx.await.map_err(|_| CommandError::aborted())? else {
        return Ok(None);
    };
    let path = local_path(picked)?;
    Ok(Some(files.register(path, FileAccess::Read)))
}

/// Shows the native save dialog with `default_name` suggested. `None` when the user cancelled.
#[tauri::command]
pub async fn file_pick_save(
    app: AppHandle,
    files: State<'_, FilesState>,
    default_name: String,
    filters: Vec<FileFilter>,
    title: Option<String>,
) -> CommandResult<Option<PickedFile>> {
    validate_filters(&filters)?;
    validate_default_name(&default_name)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    dialog(&app, &filters, title)
        .set_file_name(default_name)
        .save_file(move |picked| {
            let _ = tx.send(picked);
        });
    let Some(picked) = rx.await.map_err(|_| CommandError::aborted())? else {
        return Ok(None);
    };
    let path = local_path(picked)?;
    Ok(Some(files.register(path, FileAccess::Write)))
}

/// Drops a handle; `false` when it was unknown.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passes `State` by value.
pub fn file_release(files: State<'_, FilesState>, handle: FileHandle) -> bool {
    files.release(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_resolve_by_access_and_expire() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("in.csv");
        std::fs::write(&existing, b"a\n1\n").unwrap();
        let state = FilesState::new();

        let read = state.register(existing.clone(), FileAccess::Read);
        assert_eq!(read.name, "in.csv");
        assert_eq!(read.size, Some(4));
        assert!(read.exists);
        assert_eq!(state.read_path(read.handle).unwrap(), existing);
        // Reusable.
        assert_eq!(state.read_path(read.handle).unwrap(), existing);
        // Not a write handle, and still registered after the refusal.
        assert!(state.take_write_target(read.handle).is_err());
        assert!(state.read_path(read.handle).is_ok());

        let new_file = dir.path().join("out.parquet");
        let write = state.register(new_file.clone(), FileAccess::Write);
        assert!(!write.exists);
        assert_eq!(write.size, None);
        assert!(state.read_path(write.handle).is_err());
        assert_eq!(
            state.take_write_target(write.handle).unwrap(),
            (new_file, false)
        );
        // Used up.
        let err = state.take_write_target(write.handle).unwrap_err();
        assert_eq!(err.category(), ErrorCategory::NotFound);

        let replace = state.register(existing.clone(), FileAccess::Write);
        assert!(replace.exists);
        assert_eq!(
            state.take_write_target(replace.handle).unwrap(),
            (existing, true)
        );

        assert!(state.release(read.handle));
        assert!(!state.release(read.handle));
        assert!(state.is_empty());
    }

    #[test]
    fn oldest_handles_are_dropped() {
        let state = FilesState::new();
        let first = state.register(PathBuf::from("first.csv"), FileAccess::Read);
        for i in 0..MAX_HANDLES {
            state.register(PathBuf::from(format!("{i}.csv")), FileAccess::Read);
        }
        assert_eq!(state.len(), MAX_HANDLES);
        assert!(state.read_path(first.handle).is_err());
    }

    #[test]
    fn picked_files_serialize_without_paths() {
        let state = FilesState::new();
        let picked = state.register(
            PathBuf::from("/home/someone/private/orders.csv"),
            FileAccess::Read,
        );
        let json = serde_json::to_string(&picked).unwrap();
        assert!(json.contains("\"name\":\"orders.csv\""), "{json}");
        assert!(!json.contains("private"), "{json}");
        assert!(json.contains("\"access\":\"read\""), "{json}");
    }

    #[test]
    fn ui_input_is_checked() {
        let csv = FileFilter {
            name: "CSV".into(),
            extensions: vec!["csv".into(), "tsv".into()],
        };
        validate_filters(std::slice::from_ref(&csv)).unwrap();
        let dotted = FileFilter {
            name: "bad".into(),
            extensions: vec![".csv".into()],
        };
        assert!(validate_filters(&[dotted]).is_err());
        let all = FileFilter {
            name: "All files".into(),
            extensions: vec!["*".into()],
        };
        validate_filters(&[csv, all]).unwrap();

        validate_default_name("orders 2026.csv").unwrap();
        for bad in [
            "", ".", "..", "../x.csv", "a/b.csv", "C:x.csv", "a\\b", "a\nb",
        ] {
            assert!(validate_default_name(bad).is_err(), "{bad:?}");
        }
    }
}
