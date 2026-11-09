//! 1.3: query files on disk, saved queries, recent files, which connection a file runs on, and
//! the workspace (open query tabs) kept between app runs.
//!
//! # Text files
//!
//! [`read_text_file`] reads a UTF-8 script of at most [`MAX_TEXT_BYTES`]; a leading byte order
//! mark is dropped and remembered ([`TextFile::bom`]), so [`write_text_file`] writes it back.
//! Line endings are kept as they are (the editor keeps the file's own). Writes replace the file
//! atomically where the platform allows it: the text goes to a temporary file next to the target,
//! which is then renamed over it (the target's permissions are copied first). A symbolic link is
//! followed, so the file it points to is replaced, not the link.
//!
//! # Saved queries
//!
//! [`SavedQueries`] keeps one folder per connection profile (`<root>/<profile id>/`, the root
//! being `queries/` in [`app_data_dir`](crate::app_data_dir)). A saved query is a plain
//! `<name>.sql` file there, so other tools can open it too. Names are checked by
//! [`validate_query_name`].
//!
//! # Recent files, links and the workspace
//!
//! [`DocumentStore`] keeps three JSON values in the app database's `settings` table (or in
//! memory): the most recent files ([`RECENT_LIMIT`], newest first), the connection and database
//! each file last ran on ([`FileLink`], at most [`LINK_LIMIT`], the oldest dropped first), and the
//! [`StoredWorkspace`]. Paths are stored as UTF-8 text; a path that is not valid UTF-8 is not
//! remembered. Unreadable stored values count as empty.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use oxbase_types::{Error, Result};

use crate::app_db::AppDb;

/// Most bytes a query tab opens or saves.
pub const MAX_TEXT_BYTES: u64 = 16 * 1024 * 1024;

/// Most recent files kept.
pub const RECENT_LIMIT: usize = 20;

/// Most file links kept.
pub const LINK_LIMIT: usize = 500;

/// Longest saved query name, in characters.
pub const MAX_QUERY_NAME_CHARS: usize = 100;

/// `settings` keys of the [`DocumentStore`] values.
pub const RECENT_KEY: &str = "documents.recent";
pub const LINKS_KEY: &str = "documents.links";
pub const WORKSPACE_KEY: &str = "workspace";

/// The text of a file and whether it started with a byte order mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextFile {
    pub text: String,
    pub bom: bool,
}

fn unreadable(e: &std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        Error::not_found("the file does not exist any more")
    } else {
        Error::not_found(format!("could not read the file: {e}"))
    }
}

fn too_large(len: u64) -> Error {
    Error::invalid_input(format!(
        "the file is {} MB; a query tab holds files of at most {} MB",
        len.div_ceil(1024 * 1024),
        MAX_TEXT_BYTES / (1024 * 1024)
    ))
}

/// Reads a UTF-8 text file of at most [`MAX_TEXT_BYTES`] (module docs).
pub fn read_text_file(path: &Path) -> Result<TextFile> {
    let len = std::fs::metadata(path).map_err(|e| unreadable(&e))?.len();
    if len > MAX_TEXT_BYTES {
        return Err(too_large(len));
    }
    let bytes = std::fs::read(path).map_err(|e| unreadable(&e))?;
    let text = String::from_utf8(bytes).map_err(|_| {
        Error::invalid_input("the file is not UTF-8 text; save it as UTF-8 and open it again")
    })?;
    Ok(match text.strip_prefix('\u{feff}') {
        Some(rest) => TextFile {
            text: rest.to_owned(),
            bom: true,
        },
        None => TextFile { text, bom: false },
    })
}

/// Writes `text` (with a byte order mark first when `bom`) to `path`, replacing it atomically
/// where possible (module docs). The parent folder must exist.
pub fn write_text_file(path: &Path, text: &str, bom: bool) -> Result<()> {
    let len = u64::try_from(text.len())
        .unwrap_or(u64::MAX)
        .saturating_add(3 * u64::from(bom));
    if len > MAX_TEXT_BYTES {
        return Err(too_large(len));
    }
    // Replace the file a link points to, not the link.
    let target = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::canonicalize(path).map_err(|e| write_error(&e))?
        }
        _ => path.to_path_buf(),
    };
    let mut bytes = Vec::with_capacity(text.len() + 3);
    if bom {
        bytes.extend_from_slice("\u{feff}".as_bytes());
    }
    bytes.extend_from_slice(text.as_bytes());

    let dir = target
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    if !dir.is_dir() {
        return Err(Error::not_found(
            "the folder of the file does not exist any more",
        ));
    }
    let name = target
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let temp = dir.join(format!(".{name}.{}.tmp", Uuid::new_v4().simple()));
    let written = std::fs::write(&temp, &bytes).and_then(|()| {
        if let Ok(meta) = std::fs::metadata(&target) {
            // Keep the file's permissions (best effort).
            let _ = std::fs::set_permissions(&temp, meta.permissions());
        }
        std::fs::rename(&temp, &target)
    });
    if written.is_ok() {
        return Ok(());
    }
    // A temporary file could not be made or renamed here (a read-only folder with a writable
    // file, a file another program holds open on Windows): write in place.
    let _ = std::fs::remove_file(&temp);
    std::fs::write(&target, &bytes).map_err(|e| write_error(&e))
}

fn write_error(e: &std::io::Error) -> Error {
    Error::Io {
        kind: format!("{:?}", e.kind()).to_lowercase(),
        message: format!("could not save the file: {e}"),
    }
}

// --- saved queries ----------------------------------------------------------------------------

/// Checks a saved query name and returns it trimmed: 1 to [`MAX_QUERY_NAME_CHARS`] characters,
/// none of `/ \ : * ? " < > |` or control characters, not starting with a dot, and not a name
/// Windows reserves (`CON`, `NUL`, `COM1`, ...).
pub fn validate_query_name(name: &str) -> Result<String> {
    let name = name.trim();
    let bad_char = |c: char| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|');
    if name.is_empty() {
        return Err(Error::invalid_input("the query needs a name"));
    }
    if name.chars().count() > MAX_QUERY_NAME_CHARS {
        return Err(Error::invalid_input(format!(
            "the name is longer than {MAX_QUERY_NAME_CHARS} characters"
        )));
    }
    if name.starts_with('.') || name.ends_with('.') {
        return Err(Error::invalid_input(
            "the name cannot start or end with a dot",
        ));
    }
    if name.chars().any(|c| bad_char(c) || c.is_control()) {
        return Err(Error::invalid_input(
            "the name cannot contain / \\ : * ? \" < > | or control characters",
        ));
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        return Err(Error::invalid_input(format!(
            "'{name}' is a name Windows reserves; pick another"
        )));
    }
    Ok(name.to_owned())
}

/// A saved query file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedQuery {
    pub profile_id: Uuid,
    /// The file name without `.sql`.
    pub name: String,
    pub path: PathBuf,
    /// Last change, Unix milliseconds (0 when unknown).
    pub modified_ms: i64,
}

/// The saved query folders (module docs).
#[derive(Debug, Clone)]
pub struct SavedQueries {
    root: PathBuf,
}

impl SavedQueries {
    /// Folder name inside the app data directory.
    pub const DIR_NAME: &'static str = "queries";

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `queries/` in [`app_data_dir`](crate::app_data_dir).
    pub fn open_default() -> Result<Self> {
        Ok(Self::new(crate::app_data_dir()?.join(Self::DIR_NAME)))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The folder of `profile`'s saved queries.
    pub fn folder(&self, profile: Uuid) -> PathBuf {
        self.root.join(profile.to_string())
    }

    /// The profile `path` is a saved query of, if it is one (a `.sql` file directly in a
    /// profile's folder).
    pub fn owner(&self, path: &Path) -> Option<Uuid> {
        let parent = path.parent()?;
        if parent.parent()? != self.root.as_path() || !is_sql(path) {
            return None;
        }
        Uuid::parse_str(parent.file_name()?.to_str()?).ok()
    }

    /// Every saved query of every profile, by profile, then name (case-insensitive).
    pub fn list(&self) -> Result<Vec<SavedQuery>> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in entries.flatten() {
            let Some(profile) = entry
                .file_name()
                .to_str()
                .and_then(|n| Uuid::parse_str(n).ok())
            else {
                continue;
            };
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                out.extend(self.list_of(profile)?);
            }
        }
        out.sort_by(|a, b| {
            a.profile_id
                .cmp(&b.profile_id)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(out)
    }

    /// The saved queries of `profile`, by name (case-insensitive).
    pub fn list_of(&self, profile: Uuid) -> Result<Vec<SavedQuery>> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(self.folder(profile)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry.file_type().is_ok_and(|t| t.is_file()) || !is_sql(&path) {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let modified_ms = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map_or(0, unix_ms);
            out.push(SavedQuery {
                profile_id: profile,
                name: name.to_owned(),
                path,
                modified_ms,
            });
        }
        out.sort_by_key(|q| q.name.to_lowercase());
        Ok(out)
    }

    /// The path a query named `name` of `profile` would have, after checking the name and that
    /// no saved query of the profile already has it (ignoring case).
    pub fn free_path(&self, profile: Uuid, name: &str) -> Result<PathBuf> {
        let name = validate_query_name(name)?;
        let taken = self
            .list_of(profile)?
            .iter()
            .any(|q| q.name.to_lowercase() == name.to_lowercase());
        if taken {
            return Err(Error::invalid_input(format!(
                "a saved query named '{name}' already exists for this connection"
            )));
        }
        Ok(self.folder(profile).join(format!("{name}.sql")))
    }

    /// Creates the saved query `name` of `profile` with `text`. Returns its path.
    pub fn create(&self, profile: Uuid, name: &str, text: &str) -> Result<PathBuf> {
        let path = self.free_path(profile, name)?;
        std::fs::create_dir_all(self.folder(profile))?;
        write_text_file(&path, text, false)?;
        Ok(path)
    }

    /// Renames the saved query at `path`. Returns its new path (the same one for a name that
    /// only differs in case on a case-insensitive file system is handled by the rename).
    pub fn rename(&self, path: &Path, name: &str) -> Result<PathBuf> {
        let profile = self
            .owner(path)
            .ok_or_else(|| Error::invalid_input("this file is not a saved query"))?;
        let name = validate_query_name(name)?;
        let current = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if current == name {
            return Ok(path.to_path_buf());
        }
        let target = if current.to_lowercase() == name.to_lowercase() {
            self.folder(profile).join(format!("{name}.sql"))
        } else {
            self.free_path(profile, &name)?
        };
        std::fs::rename(path, &target).map_err(|e| unreadable(&e))?;
        Ok(target)
    }

    /// Deletes the saved query at `path`; `false` when it was already gone.
    pub fn delete(&self, path: &Path) -> Result<bool> {
        if self.owner(path).is_none() {
            return Err(Error::invalid_input("this file is not a saved query"));
        }
        match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

fn is_sql(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("sql"))
}

fn unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn now_ms() -> i64 {
    unix_ms(SystemTime::now())
}

// --- recent files, links and the workspace -------------------------------------------------

/// The connection (profile) and database a file runs on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileLink {
    pub profile_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

impl FileLink {
    /// A database name of at most 1000 characters.
    pub fn validate(&self) -> Result<()> {
        if self
            .database
            .as_ref()
            .is_some_and(|d| d.chars().count() > 1000)
        {
            return Err(Error::invalid_input("the database name is too long"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecentEntry {
    path: String,
    opened_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkEntry {
    #[serde(flatten)]
    link: FileLink,
    updated_at_ms: i64,
}

/// One query tab of the [`StoredWorkspace`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredTab {
    pub title: String,
    /// The file the tab shows, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The tab's text when it is not the file's (unsaved changes, or no file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The file starts with a byte order mark.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bom: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

/// The open query tabs, kept between app runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredWorkspace {
    #[serde(default)]
    pub tabs: Vec<StoredTab>,
    /// Index of the active tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<usize>,
}

#[derive(Debug)]
enum Backend {
    Db(AppDb),
    Memory(Mutex<HashMap<String, serde_json::Value>>),
}

/// Recent files, file links and the workspace (module docs). Cheap to clone.
#[derive(Debug, Clone)]
pub struct DocumentStore {
    backend: Arc<Backend>,
    /// Serializes read-modify-write updates of one value.
    update: Arc<tokio::sync::Mutex<()>>,
}

fn path_key(path: &Path) -> Option<String> {
    path.to_str().map(str::to_owned)
}

impl DocumentStore {
    /// Values in the app database's `settings` table.
    pub fn new(db: AppDb) -> Self {
        Self {
            backend: Arc::new(Backend::Db(db)),
            update: Arc::default(),
        }
    }

    /// Values in memory (tests, runs without an app database).
    pub fn in_memory() -> Self {
        Self {
            backend: Arc::new(Backend::Memory(Mutex::default())),
            update: Arc::default(),
        }
    }

    async fn get<T: DeserializeOwned + Default>(&self, key: &str) -> Result<T> {
        let value = match self.backend.as_ref() {
            Backend::Db(db) => db.get_setting(key).await.unwrap_or(None),
            Backend::Memory(map) => map
                .lock()
                .map_err(|_| Error::internal("document store lock poisoned"))?
                .get(key)
                .cloned(),
        };
        Ok(value
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default())
    }

    async fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let value = serde_json::to_value(value)
            .map_err(|e| Error::internal(format!("document store: {e}")))?;
        match self.backend.as_ref() {
            Backend::Db(db) => db.set_setting(key, &value).await,
            Backend::Memory(map) => {
                map.lock()
                    .map_err(|_| Error::internal("document store lock poisoned"))?
                    .insert(key.to_owned(), value);
                Ok(())
            }
        }
    }

    /// The recent files, newest first.
    pub async fn recent(&self) -> Result<Vec<PathBuf>> {
        let list: Vec<RecentEntry> = self.get(RECENT_KEY).await?;
        Ok(list.into_iter().map(|e| PathBuf::from(e.path)).collect())
    }

    /// Moves `path` to the top of the recent files.
    pub async fn touch_recent(&self, path: &Path) -> Result<()> {
        let Some(key) = path_key(path) else {
            return Ok(());
        };
        let _guard = self.update.lock().await;
        let mut list: Vec<RecentEntry> = self.get(RECENT_KEY).await?;
        list.retain(|e| e.path != key);
        list.insert(
            0,
            RecentEntry {
                path: key,
                opened_at_ms: now_ms(),
            },
        );
        list.truncate(RECENT_LIMIT);
        self.set(RECENT_KEY, &list).await
    }

    /// Drops `path` from the recent files.
    pub async fn remove_recent(&self, path: &Path) -> Result<()> {
        let Some(key) = path_key(path) else {
            return Ok(());
        };
        let _guard = self.update.lock().await;
        let mut list: Vec<RecentEntry> = self.get(RECENT_KEY).await?;
        let before = list.len();
        list.retain(|e| e.path != key);
        if list.len() == before {
            return Ok(());
        }
        self.set(RECENT_KEY, &list).await
    }

    /// Forgets every recent file.
    pub async fn clear_recent(&self) -> Result<()> {
        let _guard = self.update.lock().await;
        self.set(RECENT_KEY, &Vec::<RecentEntry>::new()).await
    }

    /// The connection `path` last ran on.
    pub async fn link(&self, path: &Path) -> Result<Option<FileLink>> {
        let Some(key) = path_key(path) else {
            return Ok(None);
        };
        let links: HashMap<String, LinkEntry> = self.get(LINKS_KEY).await?;
        Ok(links.get(&key).map(|e| e.link.clone()))
    }

    /// Remembers (`Some`) or forgets (`None`) the connection `path` runs on.
    pub async fn set_link(&self, path: &Path, link: Option<&FileLink>) -> Result<()> {
        let Some(key) = path_key(path) else {
            return Ok(());
        };
        if let Some(link) = link {
            link.validate()?;
        }
        let _guard = self.update.lock().await;
        let mut links: HashMap<String, LinkEntry> = self.get(LINKS_KEY).await?;
        match link {
            Some(link) => {
                if links.get(&key).is_some_and(|e| &e.link == link) {
                    return Ok(());
                }
                links.insert(
                    key,
                    LinkEntry {
                        link: link.clone(),
                        updated_at_ms: now_ms(),
                    },
                );
                while links.len() > LINK_LIMIT {
                    let oldest = links
                        .iter()
                        .min_by_key(|(_, e)| e.updated_at_ms)
                        .map(|(k, _)| k.clone());
                    match oldest {
                        Some(k) => {
                            links.remove(&k);
                        }
                        None => break,
                    }
                }
            }
            None => {
                if links.remove(&key).is_none() {
                    return Ok(());
                }
            }
        }
        self.set(LINKS_KEY, &links).await
    }

    /// Moves the link of `from` to `to` (a renamed saved query).
    pub async fn move_link(&self, from: &Path, to: &Path) -> Result<()> {
        if let Some(link) = self.link(from).await? {
            self.set_link(to, Some(&link)).await?;
            self.set_link(from, None).await?;
        }
        Ok(())
    }

    /// The workspace of the last run (empty when none was kept).
    pub async fn workspace(&self) -> Result<StoredWorkspace> {
        self.get(WORKSPACE_KEY).await
    }

    /// Keeps `workspace` for the next run.
    pub async fn save_workspace(&self, workspace: &StoredWorkspace) -> Result<()> {
        let _guard = self.update.lock().await;
        self.set(WORKSPACE_KEY, workspace).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_files_keep_their_byte_order_mark_and_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.sql");
        std::fs::write(&path, "\u{feff}SELECT 'é';\r\nSELECT 2;\r\n").unwrap();
        let file = read_text_file(&path).unwrap();
        assert!(file.bom);
        assert_eq!(file.text, "SELECT 'é';\r\nSELECT 2;\r\n");
        write_text_file(&path, "SELECT 3;\r\n", file.bom).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            "\u{feff}SELECT 3;\r\n".as_bytes()
        );
        write_text_file(&path, "SELECT 4;\n", false).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "SELECT 4;\n");
        // No temporary file is left behind.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["q.sql"]);
    }

    #[test]
    fn unreadable_files_say_why() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin.sql");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        let err = read_text_file(&path).unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);
        let err = read_text_file(&dir.path().join("missing.sql")).unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::NotFound);
        let err = write_text_file(&dir.path().join("gone").join("x.sql"), "x", false).unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn saving_through_a_link_replaces_the_linked_file() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.sql");
        let link = dir.path().join("link.sql");
        std::fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_text_file(&link, "new", false).unwrap();
        assert_eq!(std::fs::read_to_string(real).unwrap(), "new");
        assert!(
            std::fs::symlink_metadata(link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn query_names_are_checked() {
        assert_eq!(
            validate_query_name("  Monthly report ").unwrap(),
            "Monthly report"
        );
        assert_eq!(validate_query_name("v1.2 totals").unwrap(), "v1.2 totals");
        for bad in [
            "", "  ", ".hidden", "end.", "a/b", "a\\b", "a:b", "a*b", "a?b", "a\"b", "a<b", "a|b",
            "a\nb", "CON", "nul", "com1", "LPT9.sql",
        ] {
            assert!(validate_query_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_query_name("COM10").is_ok());
        assert!(validate_query_name(&"x".repeat(MAX_QUERY_NAME_CHARS)).is_ok());
        assert!(validate_query_name(&"x".repeat(MAX_QUERY_NAME_CHARS + 1)).is_err());
    }

    #[test]
    fn saved_queries_live_in_one_folder_per_profile() {
        let dir = tempfile::tempdir().unwrap();
        let queries = SavedQueries::new(dir.path().join("queries"));
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(queries.list().unwrap(), Vec::new());

        let report = queries.create(a, "report", "SELECT 1;").unwrap();
        assert_eq!(report, queries.folder(a).join("report.sql"));
        assert_eq!(std::fs::read_to_string(&report).unwrap(), "SELECT 1;");
        queries.create(a, "Archive", "SELECT 2;").unwrap();
        queries.create(b, "report", "SELECT 3;").unwrap();
        // Names are unique per profile, ignoring case.
        assert!(queries.create(a, "REPORT", "x").is_err());
        // Other files and folders are not saved queries.
        std::fs::write(queries.folder(a).join("notes.txt"), "x").unwrap();
        std::fs::create_dir_all(dir.path().join("queries").join("not-a-uuid")).unwrap();

        let names: Vec<_> = queries
            .list_of(a)
            .unwrap()
            .into_iter()
            .map(|q| q.name)
            .collect();
        assert_eq!(names, ["Archive", "report"]);
        assert_eq!(queries.list().unwrap().len(), 3);
        assert_eq!(queries.owner(&report), Some(a));
        assert_eq!(queries.owner(&dir.path().join("report.sql")), None);

        let renamed = queries.rename(&report, "Report 2026").unwrap();
        assert_eq!(renamed, queries.folder(a).join("Report 2026.sql"));
        assert!(!report.exists());
        assert!(queries.rename(&renamed, "archive").is_err());
        // Only the case changes.
        let recased = queries.rename(&renamed, "report 2026").unwrap();
        assert_eq!(std::fs::read_to_string(&recased).unwrap(), "SELECT 1;");

        assert!(queries.delete(&recased).unwrap());
        assert!(!queries.delete(&recased).unwrap());
        assert!(queries.delete(&dir.path().join("x.sql")).is_err());
    }

    #[tokio::test]
    async fn recent_files_are_newest_first_and_bounded() {
        let store = DocumentStore::in_memory();
        assert_eq!(store.recent().await.unwrap(), Vec::<PathBuf>::new());
        for i in 0..RECENT_LIMIT + 3 {
            store
                .touch_recent(Path::new(&format!("/q/{i}.sql")))
                .await
                .unwrap();
        }
        store.touch_recent(Path::new("/q/5.sql")).await.unwrap();
        let recent = store.recent().await.unwrap();
        assert_eq!(recent.len(), RECENT_LIMIT);
        assert_eq!(recent[0], PathBuf::from("/q/5.sql"));
        assert_eq!(
            recent[1],
            PathBuf::from(format!("/q/{}.sql", RECENT_LIMIT + 2))
        );
        assert_eq!(recent.iter().filter(|p| p.ends_with("5.sql")).count(), 1);
        store.remove_recent(Path::new("/q/5.sql")).await.unwrap();
        assert_eq!(store.recent().await.unwrap().len(), RECENT_LIMIT - 1);
        store.clear_recent().await.unwrap();
        assert_eq!(store.recent().await.unwrap(), Vec::<PathBuf>::new());
    }

    #[tokio::test]
    async fn links_remember_the_connection_of_a_file() {
        let store = DocumentStore::in_memory();
        let path = Path::new("/q/report.sql");
        assert_eq!(store.link(path).await.unwrap(), None);
        let link = FileLink {
            profile_id: Uuid::now_v7(),
            database: Some("sales".into()),
        };
        store.set_link(path, Some(&link)).await.unwrap();
        assert_eq!(store.link(path).await.unwrap(), Some(link.clone()));
        store
            .move_link(path, Path::new("/q/renamed.sql"))
            .await
            .unwrap();
        assert_eq!(store.link(path).await.unwrap(), None);
        assert_eq!(
            store.link(Path::new("/q/renamed.sql")).await.unwrap(),
            Some(link)
        );
        store
            .set_link(Path::new("/q/renamed.sql"), None)
            .await
            .unwrap();
        assert_eq!(store.link(Path::new("/q/renamed.sql")).await.unwrap(), None);
        let long = FileLink {
            profile_id: Uuid::now_v7(),
            database: Some("x".repeat(1001)),
        };
        assert!(store.set_link(path, Some(&long)).await.is_err());
    }

    #[tokio::test]
    async fn the_workspace_round_trips_through_the_app_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = AppDb::open(dir.path().join("oxbase.db")).await.unwrap();
        let store = DocumentStore::new(db.clone());
        assert_eq!(store.workspace().await.unwrap(), StoredWorkspace::default());
        let workspace = StoredWorkspace {
            tabs: vec![
                StoredTab {
                    title: "report.sql".into(),
                    path: Some("/q/report.sql".into()),
                    bom: true,
                    profile_id: Some(Uuid::now_v7()),
                    database: Some("sales".into()),
                    ..StoredTab::default()
                },
                StoredTab {
                    title: "Query 2".into(),
                    text: Some("SELECT 1;".into()),
                    ..StoredTab::default()
                },
            ],
            active: Some(1),
        };
        store.save_workspace(&workspace).await.unwrap();
        // Another store on the same database (the next app run) reads it back.
        let again = DocumentStore::new(db);
        assert_eq!(again.workspace().await.unwrap(), workspace);
        store
            .touch_recent(Path::new("/q/report.sql"))
            .await
            .unwrap();
        assert_eq!(
            again.recent().await.unwrap(),
            [PathBuf::from("/q/report.sql")]
        );
    }
}
