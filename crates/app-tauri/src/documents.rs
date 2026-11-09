//! 1.3 query files, saved queries, recent files and the workspace (`docs/files.md`).
//!
//! The webview never sends a file-system path. A query tab that shows a file holds an opaque
//! [`DocumentId`]; [`DocumentsState`] maps it to the path for this app run (the same id for the
//! same path). Files are chosen in native dialogs shown from Rust, come from the recent-files
//! list or the saved queries (both listed by Rust with ids), or are opened by the OS
//! (`src/shell.rs`). The UI gets each document's name and, for display only, its folder.
//!
//! Commands:
//!
//! - `document_open` (native open dialog), `document_reopen` (a recent file or saved query by
//!   id), `document_recent`, `document_recent_clear`;
//! - `document_save` (to the tab's file), `document_save_as` (native save dialog),
//!   `document_set_link` (remember the connection and database a file runs on);
//! - `saved_query_list`, `saved_query_create`, `saved_query_rename`, `saved_query_delete`;
//! - `workspace_load`, `workspace_save`: the open query tabs, kept between runs.
//!
//! File reads and writes run on the blocking pool. The storage rules (byte order mark, atomic
//! replace, names, limits) are `oxbase_store::documents`'.
//!
//! **Debug builds only**: with `--e2e-file-dir=<dir>` (`src/files.rs`), `document_open` opens the
//! single file in `<dir>/open/` and `document_save_as` saves to `<dir>/save/<defaultName>`
//! without a dialog, like the M5 file hook.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use oxbase_store::{DocumentStore, FileLink, SavedQueries, StoredTab, StoredWorkspace, TextFile};
use oxbase_types::{Error, Result};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use uuid::Uuid;

use crate::error::CommandError;
use crate::files::{FileFilter, dialog, local_path, validate_default_name};

type CommandResult<T> = std::result::Result<T, CommandError>;

/// Most tabs a workspace keeps.
pub const MAX_WORKSPACE_TABS: usize = 200;

/// Most text a workspace keeps in all, in bytes. Tabs past it are kept without their unsaved
/// text.
pub const MAX_WORKSPACE_TEXT: usize = 64 * 1024 * 1024;

/// An opaque reference to a file a query tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocumentId(pub Uuid);

/// The connection a saved query belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedQueryOwner {
    pub profile_id: Uuid,
}

/// What the UI gets to know about a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentInfo {
    pub id: DocumentId,
    /// The file name; a saved query's name (without `.sql`).
    pub name: String,
    /// Where the file is, for display only: its folder (the home folder shown as `~`), or
    /// "Saved queries".
    pub location: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_query: Option<SavedQueryOwner>,
}

/// A document opened in a tab: what it is, its text and the connection it last ran on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenedDocument {
    pub document: DocumentInfo,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<FileLink>,
}

/// A saved query in the Queries panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedQueryItem {
    pub document: DocumentInfo,
    pub profile_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    pub modified_ms: i64,
}

/// One query tab the UI keeps for the next run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTabInput {
    pub title: String,
    #[serde(default)]
    pub document: Option<DocumentId>,
    /// The tab's text when it is not the file's (unsaved changes, or no file).
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub profile_id: Option<Uuid>,
    #[serde(default)]
    pub database: Option<String>,
}

/// The query tabs the UI keeps for the next run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInput {
    #[serde(default)]
    pub tabs: Vec<WorkspaceTabInput>,
    #[serde(default)]
    pub active: Option<usize>,
}

/// A query tab of the last run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoredTab {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<DocumentInfo>,
    pub text: String,
    /// The text differs from the file's (or there is no file and some text).
    pub dirty: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

/// The query tabs of the last run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoredWorkspace {
    pub tabs: Vec<RestoredTab>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<usize>,
    /// Names of files that could not be opened again (moved or deleted); their tabs are gone.
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone)]
struct Doc {
    path: PathBuf,
    bom: bool,
}

#[derive(Default)]
struct Registry {
    docs: HashMap<DocumentId, Doc>,
    by_path: HashMap<PathBuf, DocumentId>,
}

/// Documents of this app run, the document store and the saved query folders, managed as
/// Tauri state.
pub struct DocumentsState {
    store: DocumentStore,
    queries: SavedQueries,
    registry: Mutex<Registry>,
}

fn gone() -> Error {
    Error::not_found("this file is no longer known; open it again")
}

/// `path` made absolute (relative to the current folder), without resolving links.
fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

fn base_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// `dir` for display, with the home folder as `~`.
fn display_folder(dir: &Path) -> String {
    if let Some(home) = dirs_home()
        && let Ok(rest) = dir.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_owned()
        } else {
            format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display())
        };
    }
    dir.display().to_string()
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|_| Error::internal("a file task stopped unexpectedly"))?
}

/// The open and save dialog filters.
fn sql_filters() -> Vec<FileFilter> {
    vec![
        FileFilter {
            name: "SQL".into(),
            extensions: vec!["sql".into()],
        },
        FileFilter {
            name: "Text".into(),
            extensions: vec!["txt".into()],
        },
        FileFilter {
            name: "All files".into(),
            extensions: vec!["*".into()],
        },
    ]
}

impl DocumentsState {
    pub fn new(store: DocumentStore, queries: SavedQueries) -> Self {
        Self {
            store,
            queries,
            registry: Mutex::default(),
        }
    }

    pub fn store(&self) -> &DocumentStore {
        &self.store
    }

    fn lock(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The id of `path` (a new one for a path not seen in this run); `bom` is recorded.
    pub fn register(&self, path: PathBuf, bom: bool) -> DocumentId {
        let mut registry = self.lock();
        if let Some(id) = registry.by_path.get(&path).copied() {
            if let Some(doc) = registry.docs.get_mut(&id) {
                doc.bom = bom;
            }
            return id;
        }
        let id = DocumentId(Uuid::new_v4());
        registry.by_path.insert(path.clone(), id);
        registry.docs.insert(id, Doc { path, bom });
        id
    }

    /// The id `path` has in this run, registering it (with no byte order mark) if new.
    fn id_of(&self, path: &Path) -> DocumentId {
        if let Some(id) = self.lock().by_path.get(path).copied() {
            return id;
        }
        self.register(path.to_path_buf(), false)
    }

    fn doc(&self, id: DocumentId) -> Result<Doc> {
        self.lock().docs.get(&id).cloned().ok_or_else(gone)
    }

    fn set_bom(&self, id: DocumentId, bom: bool) {
        if let Some(doc) = self.lock().docs.get_mut(&id) {
            doc.bom = bom;
        }
    }

    /// Points `id` at `path` (a renamed saved query).
    fn repoint(&self, id: DocumentId, path: PathBuf) {
        let mut registry = self.lock();
        let old = registry.docs.get(&id).map(|d| d.path.clone());
        if let Some(old) = old {
            registry.by_path.remove(&old);
        }
        registry.by_path.insert(path.clone(), id);
        let bom = registry.docs.get(&id).is_some_and(|d| d.bom);
        registry.docs.insert(id, Doc { path, bom });
    }

    fn forget(&self, id: DocumentId) {
        let mut registry = self.lock();
        if let Some(doc) = registry.docs.remove(&id) {
            registry.by_path.remove(&doc.path);
        }
    }

    /// What the UI shows for document `id` at `path`.
    pub fn info(&self, id: DocumentId, path: &Path) -> DocumentInfo {
        match self.queries.owner(path) {
            Some(profile_id) => DocumentInfo {
                id,
                name: path
                    .file_stem()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                location: "Saved queries".to_owned(),
                saved_query: Some(SavedQueryOwner { profile_id }),
            },
            None => DocumentInfo {
                id,
                name: base_name(path),
                location: path.parent().map(display_folder).unwrap_or_default(),
                saved_query: None,
            },
        }
    }

    /// The connection `path` runs on: its link, and a saved query always runs on its owner.
    async fn link_of(&self, path: &Path) -> Result<Option<FileLink>> {
        let link = self.store.link(path).await?;
        Ok(match self.queries.owner(path) {
            Some(profile_id) => Some(FileLink {
                profile_id,
                database: link
                    .filter(|l| l.profile_id == profile_id)
                    .and_then(|l| l.database),
            }),
            None => link,
        })
    }

    /// Reads `path` into a document; `recent` puts a file (not a saved query) on the recent
    /// list. A file that cannot be read any more leaves the recent list.
    pub async fn open_path(&self, path: PathBuf, recent: bool) -> Result<OpenedDocument> {
        let path = absolute(path);
        let at = path.clone();
        let file: TextFile = match blocking(move || oxbase_store::read_text_file(&at)).await {
            Ok(file) => file,
            Err(e) => {
                if e.category() == oxbase_types::ErrorCategory::NotFound {
                    let _ = self.store.remove_recent(&path).await;
                }
                return Err(e);
            }
        };
        let id = self.register(path.clone(), file.bom);
        if recent && self.queries.owner(&path).is_none() {
            // The recent list is a convenience: failing to update it does not fail the open.
            let _ = self.store.touch_recent(&path).await;
        }
        Ok(OpenedDocument {
            document: self.info(id, &path),
            text: file.text,
            link: self.link_of(&path).await?,
        })
    }

    async fn write(&self, id: DocumentId, text: String) -> Result<DocumentInfo> {
        let doc = self.doc(id)?;
        let path = doc.path.clone();
        blocking(move || oxbase_store::write_text_file(&doc.path, &text, doc.bom)).await?;
        if self.queries.owner(&path).is_none() {
            let _ = self.store.touch_recent(&path).await;
        }
        Ok(self.info(id, &path))
    }

    /// The tabs of the last run (files read again unless their unsaved text was kept).
    pub async fn restore(&self, stored: StoredWorkspace) -> RestoredWorkspace {
        let mut out = RestoredWorkspace::default();
        for (index, tab) in stored.tabs.into_iter().enumerate() {
            let restored = match tab.path {
                Some(path) => {
                    let path = PathBuf::from(path);
                    match tab.text {
                        Some(text) => {
                            let id = self.register(path.clone(), tab.bom);
                            Some(RestoredTab {
                                title: tab.title,
                                document: Some(self.info(id, &path)),
                                text,
                                dirty: true,
                                profile_id: tab.profile_id,
                                database: tab.database,
                            })
                        }
                        None => match self.open_path(path, false).await {
                            Ok(opened) => Some(RestoredTab {
                                title: tab.title,
                                document: Some(opened.document),
                                text: opened.text,
                                dirty: false,
                                profile_id: tab.profile_id,
                                database: tab.database,
                            }),
                            Err(_) => {
                                out.skipped.push(tab.title);
                                None
                            }
                        },
                    }
                }
                None => {
                    let text = tab.text.unwrap_or_default();
                    Some(RestoredTab {
                        title: tab.title,
                        document: None,
                        dirty: !text.is_empty(),
                        text,
                        profile_id: tab.profile_id,
                        database: tab.database,
                    })
                }
            };
            if let Some(restored) = restored {
                if stored.active == Some(index) {
                    out.active = Some(out.tabs.len());
                }
                out.tabs.push(restored);
            }
        }
        out
    }

    /// The workspace to keep for `input` (document ids become paths; texts past
    /// [`MAX_WORKSPACE_TEXT`] are dropped).
    pub fn to_stored(&self, input: WorkspaceInput) -> StoredWorkspace {
        let registry = self.lock();
        let mut budget = MAX_WORKSPACE_TEXT;
        let tabs = input
            .tabs
            .into_iter()
            .take(MAX_WORKSPACE_TABS)
            .map(|tab| {
                let doc = tab.document.and_then(|id| registry.docs.get(&id));
                let text = tab.text.filter(|t| {
                    if t.len() > budget {
                        return false;
                    }
                    budget -= t.len();
                    true
                });
                StoredTab {
                    title: tab.title.chars().take(200).collect(),
                    path: doc.and_then(|d| d.path.to_str().map(str::to_owned)),
                    bom: doc.is_some_and(|d| d.bom),
                    // A tab without a file keeps its text (empty when it had none or too much).
                    text: if doc.is_some() {
                        text
                    } else {
                        Some(text.unwrap_or_default())
                    },
                    profile_id: tab.profile_id,
                    database: tab.database.filter(|d| d.chars().count() <= 1000),
                }
            })
            .collect::<Vec<_>>();
        let active = input.active.filter(|&i| i < tabs.len());
        StoredWorkspace { tabs, active }
    }
}

/// `path` with `.sql` added when the user typed a name without an extension and no such file
/// exists (the native dialog only confirmed replacing the name as typed).
fn with_default_extension(path: PathBuf) -> PathBuf {
    if path.extension().is_some() {
        return path;
    }
    let with = path.with_extension("sql");
    if with.exists() { path } else { with }
}

/// Shows the native open dialog and opens the chosen file. `None` when the user cancelled.
#[tauri::command]
pub async fn document_open(
    app: AppHandle,
    documents: State<'_, DocumentsState>,
) -> CommandResult<Option<OpenedDocument>> {
    #[cfg(debug_assertions)]
    if let Some(dir) = crate::files::e2e::dir() {
        let path = crate::files::e2e::open_file(&dir)?;
        return Ok(Some(documents.open_path(path, true).await?));
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    dialog(&app, &sql_filters(), Some("Open SQL file".to_owned())).pick_file(move |picked| {
        let _ = tx.send(picked);
    });
    let Some(picked) = rx.await.map_err(|_| CommandError::aborted())? else {
        return Ok(None);
    };
    let path = local_path(picked)?;
    Ok(Some(documents.open_path(path, true).await?))
}

/// Opens a document by id again: a recent file or a saved query.
#[tauri::command]
pub async fn document_reopen(
    documents: State<'_, DocumentsState>,
    id: DocumentId,
) -> CommandResult<OpenedDocument> {
    let path = documents.doc(id)?.path;
    Ok(documents.open_path(path, true).await?)
}

/// The recent files that still exist, newest first.
#[tauri::command]
pub async fn document_recent(
    documents: State<'_, DocumentsState>,
) -> CommandResult<Vec<DocumentInfo>> {
    let paths = documents.store.recent().await?;
    let existing: Vec<PathBuf> =
        blocking(move || Ok(paths.into_iter().filter(|p| p.is_file()).collect())).await?;
    Ok(existing
        .iter()
        .map(|path| documents.info(documents.id_of(path), path))
        .collect())
}

/// Forgets the recent files.
#[tauri::command]
pub async fn document_recent_clear(documents: State<'_, DocumentsState>) -> CommandResult<()> {
    Ok(documents.store.clear_recent().await?)
}

/// Saves `text` to document `id`'s file.
#[tauri::command]
pub async fn document_save(
    documents: State<'_, DocumentsState>,
    id: DocumentId,
    text: String,
) -> CommandResult<DocumentInfo> {
    Ok(documents.write(id, text).await?)
}

/// Shows the native save dialog with `default_name` suggested and saves `text` there. The new
/// document keeps `from`'s byte order mark and runs on `link`. `None` when the user cancelled.
#[tauri::command]
pub async fn document_save_as(
    app: AppHandle,
    documents: State<'_, DocumentsState>,
    from: Option<DocumentId>,
    default_name: String,
    text: String,
    link: Option<FileLink>,
) -> CommandResult<Option<DocumentInfo>> {
    validate_default_name(&default_name)?;
    if let Some(link) = &link {
        link.validate()?;
    }
    let bom = from
        .and_then(|id| documents.doc(id).ok())
        .is_some_and(|d| d.bom);
    #[cfg(debug_assertions)]
    let hooked =
        crate::files::e2e::dir().map(|dir| crate::files::e2e::save_file(&dir, &default_name));
    #[cfg(not(debug_assertions))]
    let hooked: Option<PathBuf> = None;
    let path = match hooked {
        Some(path) => path,
        None => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            dialog(&app, &sql_filters(), Some("Save SQL file".to_owned()))
                .set_file_name(default_name)
                .save_file(move |picked| {
                    let _ = tx.send(picked);
                });
            let Some(picked) = rx.await.map_err(|_| CommandError::aborted())? else {
                return Ok(None);
            };
            with_default_extension(local_path(picked)?)
        }
    };
    let path = absolute(path);
    if documents.queries.owner(&path).is_some() {
        return Err(Error::invalid_input(
            "save it as a saved query from the query tab instead of into the saved queries folder",
        )
        .into());
    }
    let at = path.clone();
    blocking(move || oxbase_store::write_text_file(&at, &text, bom)).await?;
    let id = documents.register(path.clone(), bom);
    let _ = documents.store.touch_recent(&path).await;
    if let Some(link) = &link {
        let _ = documents.store.set_link(&path, Some(link)).await;
    }
    Ok(Some(documents.info(id, &path)))
}

/// Remembers (`Some`) or forgets (`None`) the connection and database document `id` runs on.
#[tauri::command]
pub async fn document_set_link(
    documents: State<'_, DocumentsState>,
    id: DocumentId,
    link: Option<FileLink>,
) -> CommandResult<()> {
    let path = documents.doc(id)?.path;
    Ok(documents.store.set_link(&path, link.as_ref()).await?)
}

/// Every saved query of every connection.
#[tauri::command]
pub async fn saved_query_list(
    documents: State<'_, DocumentsState>,
) -> CommandResult<Vec<SavedQueryItem>> {
    let queries = documents.queries.clone();
    let list = blocking(move || queries.list()).await?;
    let mut out = Vec::with_capacity(list.len());
    for query in list {
        let id = documents.id_of(&query.path);
        let database = documents
            .store
            .link(&query.path)
            .await?
            .filter(|l| l.profile_id == query.profile_id)
            .and_then(|l| l.database);
        out.push(SavedQueryItem {
            document: documents.info(id, &query.path),
            profile_id: query.profile_id,
            database,
            modified_ms: query.modified_ms,
        });
    }
    Ok(out)
}

/// Saves `text` as a new saved query `name` of connection `profile_id`, running in `database`.
#[tauri::command]
pub async fn saved_query_create(
    documents: State<'_, DocumentsState>,
    profile_id: Uuid,
    name: String,
    text: String,
    database: Option<String>,
) -> CommandResult<DocumentInfo> {
    let link = FileLink {
        profile_id,
        database,
    };
    link.validate()?;
    let queries = documents.queries.clone();
    let path = blocking(move || queries.create(profile_id, &name, &text)).await?;
    let id = documents.register(path.clone(), false);
    documents.store.set_link(&path, Some(&link)).await?;
    Ok(documents.info(id, &path))
}

/// Renames saved query `id`; it keeps its id.
#[tauri::command]
pub async fn saved_query_rename(
    documents: State<'_, DocumentsState>,
    id: DocumentId,
    name: String,
) -> CommandResult<DocumentInfo> {
    let from = documents.doc(id)?.path;
    let queries = documents.queries.clone();
    let at = from.clone();
    let to = blocking(move || queries.rename(&at, &name)).await?;
    documents.repoint(id, to.clone());
    documents.store.move_link(&from, &to).await?;
    Ok(documents.info(id, &to))
}

/// Deletes saved query `id`; `false` when its file was already gone.
#[tauri::command]
pub async fn saved_query_delete(
    documents: State<'_, DocumentsState>,
    id: DocumentId,
) -> CommandResult<bool> {
    let path = documents.doc(id)?.path;
    let queries = documents.queries.clone();
    let at = path.clone();
    let existed = blocking(move || queries.delete(&at)).await?;
    documents.forget(id);
    let _ = documents.store.set_link(&path, None).await;
    Ok(existed)
}

/// The query tabs of the last run.
#[tauri::command]
pub async fn workspace_load(
    documents: State<'_, DocumentsState>,
) -> CommandResult<RestoredWorkspace> {
    let stored = documents.store.workspace().await?;
    Ok(documents.restore(stored).await)
}

/// Keeps the open query tabs for the next run.
#[tauri::command]
pub async fn workspace_save(
    documents: State<'_, DocumentsState>,
    workspace: WorkspaceInput,
) -> CommandResult<()> {
    let stored = documents.to_stored(workspace);
    Ok(documents.store.save_workspace(&stored).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(dir: &Path) -> DocumentsState {
        DocumentsState::new(
            DocumentStore::in_memory(),
            SavedQueries::new(dir.join("queries")),
        )
    }

    #[tokio::test]
    async fn documents_open_save_and_keep_their_byte_order_mark() {
        let dir = tempfile::tempdir().unwrap();
        let docs = state(dir.path());
        let path = dir.path().join("report.sql");
        std::fs::write(&path, "\u{feff}SELECT 1;\r\n").unwrap();

        let opened = docs.open_path(path.clone(), true).await.unwrap();
        assert_eq!(opened.text, "SELECT 1;\r\n");
        assert_eq!(opened.document.name, "report.sql");
        assert_eq!(opened.document.saved_query, None);
        assert_eq!(opened.link, None);
        // The same file keeps its id.
        let again = docs.open_path(path.clone(), true).await.unwrap();
        assert_eq!(again.document.id, opened.document.id);

        docs.write(opened.document.id, "SELECT 2;\r\n".into())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            "\u{feff}SELECT 2;\r\n".as_bytes()
        );
        assert_eq!(docs.store().recent().await.unwrap(), [path.clone()]);

        // The UI never sees the path, only the name and the folder.
        let json = serde_json::to_value(&opened).unwrap();
        assert_eq!(json["document"]["name"], "report.sql");
        assert!(json["document"].get("path").is_none());

        // A deleted file leaves the recent list when opened again.
        std::fs::remove_file(&path).unwrap();
        assert!(docs.open_path(path, true).await.is_err());
        assert!(docs.store().recent().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn saved_queries_run_on_their_connection() {
        let dir = tempfile::tempdir().unwrap();
        let docs = state(dir.path());
        let profile = Uuid::now_v7();
        let path = docs.queries.create(profile, "totals", "SELECT 1;").unwrap();
        docs.store
            .set_link(
                &path,
                Some(&FileLink {
                    profile_id: profile,
                    database: Some("sales".into()),
                }),
            )
            .await
            .unwrap();
        let opened = docs.open_path(path, true).await.unwrap();
        assert_eq!(opened.document.name, "totals");
        assert_eq!(opened.document.location, "Saved queries");
        assert_eq!(
            opened.document.saved_query,
            Some(SavedQueryOwner {
                profile_id: profile
            })
        );
        assert_eq!(
            opened.link,
            Some(FileLink {
                profile_id: profile,
                database: Some("sales".into())
            })
        );
        // Saved queries are not recent files.
        assert!(docs.store().recent().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_workspace_keeps_unsaved_text_and_reads_saved_files_again() {
        let dir = tempfile::tempdir().unwrap();
        let docs = state(dir.path());
        let clean = dir.path().join("clean.sql");
        let edited = dir.path().join("edited.sql");
        let gone = dir.path().join("gone.sql");
        for path in [&clean, &edited, &gone] {
            std::fs::write(path, "on disk").unwrap();
        }
        let id = |path: &Path| docs.register(path.to_path_buf(), false);
        let (clean_id, edited_id, gone_id) = (id(&clean), id(&edited), id(&gone));
        let profile = Uuid::now_v7();
        let stored = docs.to_stored(WorkspaceInput {
            tabs: vec![
                WorkspaceTabInput {
                    title: "clean.sql".into(),
                    document: Some(clean_id),
                    profile_id: Some(profile),
                    database: Some("sales".into()),
                    ..WorkspaceTabInput::default()
                },
                WorkspaceTabInput {
                    title: "gone.sql".into(),
                    document: Some(gone_id),
                    ..WorkspaceTabInput::default()
                },
                WorkspaceTabInput {
                    title: "edited.sql".into(),
                    document: Some(edited_id),
                    text: Some("unsaved".into()),
                    ..WorkspaceTabInput::default()
                },
                WorkspaceTabInput {
                    title: "Query 4".into(),
                    text: Some("SELECT 4;".into()),
                    ..WorkspaceTabInput::default()
                },
            ],
            active: Some(2),
        });
        assert_eq!(stored.tabs[0].path.as_deref(), clean.to_str());
        assert_eq!(stored.tabs[0].text, None);
        std::fs::write(&clean, "changed outside").unwrap();
        std::fs::remove_file(&gone).unwrap();

        // The next run: a new registry.
        let next = state(dir.path());
        let restored = next.restore(stored).await;
        assert_eq!(restored.skipped, ["gone.sql"]);
        let titles: Vec<_> = restored.tabs.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["clean.sql", "edited.sql", "Query 4"]);
        assert_eq!(restored.active, Some(1));
        assert_eq!(restored.tabs[0].text, "changed outside");
        assert!(!restored.tabs[0].dirty);
        assert_eq!(restored.tabs[0].profile_id, Some(profile));
        assert_eq!(restored.tabs[0].database.as_deref(), Some("sales"));
        assert_eq!(restored.tabs[1].text, "unsaved");
        assert!(restored.tabs[1].dirty);
        assert!(restored.tabs[1].document.is_some());
        assert_eq!(restored.tabs[2].text, "SELECT 4;");
        assert!(restored.tabs[2].dirty);
        assert!(restored.tabs[2].document.is_none());
    }

    #[test]
    fn workspace_input_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let docs = state(dir.path());
        let tabs = (0..MAX_WORKSPACE_TABS + 5)
            .map(|i| WorkspaceTabInput {
                title: format!("Query {i}{}", "x".repeat(300)),
                text: Some("SELECT 1;".into()),
                ..WorkspaceTabInput::default()
            })
            .collect();
        let stored = docs.to_stored(WorkspaceInput {
            tabs,
            active: Some(MAX_WORKSPACE_TABS + 1),
        });
        assert_eq!(stored.tabs.len(), MAX_WORKSPACE_TABS);
        assert_eq!(stored.tabs[0].title.chars().count(), 200);
        assert_eq!(stored.active, None);
        // An unknown document id keeps no path.
        let stored = docs.to_stored(WorkspaceInput {
            tabs: vec![WorkspaceTabInput {
                title: "lost".into(),
                document: Some(DocumentId(Uuid::new_v4())),
                ..WorkspaceTabInput::default()
            }],
            active: None,
        });
        assert_eq!(stored.tabs[0].path, None);
        assert_eq!(stored.tabs[0].text.as_deref(), Some(""));
    }

    #[test]
    fn names_without_an_extension_get_sql() {
        let dir = tempfile::tempdir().unwrap();
        let typed = dir.path().join("report");
        assert_eq!(
            with_default_extension(typed.clone()),
            dir.path().join("report.sql")
        );
        std::fs::write(dir.path().join("report.sql"), "x").unwrap();
        assert_eq!(with_default_extension(typed.clone()), typed);
        let txt = dir.path().join("notes.txt");
        assert_eq!(with_default_extension(txt.clone()), txt);
    }

    #[test]
    fn folders_show_the_home_folder_as_a_tilde() {
        if let Some(home) = dirs_home() {
            assert_eq!(display_folder(&home), "~");
            let inside = home.join("projects");
            assert_eq!(
                display_folder(&inside),
                format!("~{}projects", std::path::MAIN_SEPARATOR)
            );
        }
    }
}
