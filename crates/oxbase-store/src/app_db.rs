//! The app database (M2): one SQLite file in [`app_data_dir`] holding query history and app
//! settings (PLAN.md §5). Later milestones add connections, job runs and the schema-cache
//! snapshot as new migrations.
//!
//! # File and connection
//!
//! - Path: [`app_data_dir`]`/`[`AppDb::FILE_NAME`]. The directory is created on open.
//! - Opened with `rusqlite` (bundled SQLite, already used by `oxbase-sqlite`), WAL journal,
//!   `foreign_keys = ON`, a busy timeout of 5 s. Blocking calls run on `spawn_blocking`.
//! - Only this process writes it. A second app instance gets `SQLITE_BUSY` after the timeout
//!   and reports an `Io` error rather than corrupting anything.
//!
//! # Migrations
//!
//! [`MIGRATIONS`] is an append-only list. `AppDb::open` creates `schema_migrations(version
//! INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at TEXT NOT NULL)` if missing, then applies,
//! in one transaction each and in order, every migration whose version is not recorded yet.
//! A database whose highest recorded version is newer than this build knows is opened as is
//! (forward compatible, PLAN.md §5) but never migrated down. A migration that fails rolls back
//! and `open` fails with `Error::Io`/`Internal`, leaving the file unchanged.
//!
//! Owner of the bodies: the **store** stream (`docs/m2-contracts.md`).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use uuid::Uuid;

use oxbase_types::{Error, Result};

use crate::history::{HistoryEntry, HistoryQuery, HistoryRepository, HistorySettings};

/// Environment variable that overrides [`app_data_dir`] (portable installs, tests, CI).
pub const DATA_DIR_ENV: &str = "OXBASE_DATA_DIR";

/// The app's data directory (local, not roaming):
///
/// | Platform | Directory |
/// |---|---|
/// | Linux / BSD | `$XDG_DATA_HOME/oxbase` or `~/.local/share/oxbase` |
/// | macOS | `~/Library/Application Support/dev.oxbase.app` |
/// | Windows | `%LOCALAPPDATA%\oxbase` |
///
/// A non-empty [`DATA_DIR_ENV`] (`OXBASE_DATA_DIR`) wins. The directory is not created here.
pub fn app_data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let base = dirs::data_local_dir().ok_or_else(|| {
        Error::unsupported(format!(
            "cannot find the user data directory; set {DATA_DIR_ENV}"
        ))
    })?;
    Ok(base.join(crate::file_repo::app_dir_name()))
}

/// One schema migration of the app database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Migration {
    /// Strictly increasing, starting at 1. Never reused or renumbered.
    pub version: u32,
    pub name: &'static str,
    /// SQL run in one transaction (may hold several statements).
    pub sql: &'static str,
}

/// Every migration, in order. Append only.
///
/// Version 1 (M2): `settings` (key/value JSON) and `query_history`. Times are Unix
/// milliseconds (UTC) so they sort as integers; ids are UUID text; `status` is
/// [`HistoryStatus::as_str`](crate::HistoryStatus::as_str).
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "settings and query history",
    sql: "\
CREATE TABLE settings (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
CREATE TABLE query_history (
    id            TEXT PRIMARY KEY NOT NULL,
    profile_id    TEXT NOT NULL,
    database_name TEXT,
    sql_text      TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    duration_ms   INTEGER NOT NULL,
    status        TEXT NOT NULL CHECK (status IN ('ok', 'error', 'cancelled')),
    row_count     INTEGER,
    error_text    TEXT
);
CREATE INDEX query_history_by_time ON query_history (started_at_ms DESC, id DESC);
CREATE INDEX query_history_by_profile ON query_history (profile_id, started_at_ms DESC);
",
}];

/// Settings key under which [`HistorySettings`] are stored (JSON) in the `settings` table.
pub const HISTORY_SETTINGS_KEY: &str = "history";

/// The app database. Cheap to clone; clones share one connection.
#[derive(Debug, Clone)]
pub struct AppDb {
    path: PathBuf,
}

impl AppDb {
    /// File name inside [`app_data_dir`].
    pub const FILE_NAME: &'static str = "oxbase.db";

    /// Opens (creating if needed) the database at `path` and applies pending [`MIGRATIONS`].
    #[allow(clippy::unused_async)] // m2: store fills the body
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let _ = &path;
        unimplemented!("m2: store")
    }

    /// [`AppDb::open`] on [`AppDb::FILE_NAME`] in [`app_data_dir`] (the directory is created).
    pub async fn open_default() -> Result<Self> {
        Self::open(app_data_dir()?.join(Self::FILE_NAME)).await
    }

    /// The database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Versions recorded in `schema_migrations`, ascending.
    #[allow(clippy::unused_async)] // m2: store fills the body
    pub async fn applied_migrations(&self) -> Result<Vec<u32>> {
        let _ = self;
        unimplemented!("m2: store")
    }

    /// Reads a JSON setting; `None` when the key is absent.
    #[allow(clippy::unused_async)] // m2: store fills the body
    pub async fn get_setting(&self, key: &str) -> Result<Option<serde_json::Value>> {
        let _ = (self, key);
        unimplemented!("m2: store")
    }

    /// Writes (upserts) a JSON setting.
    #[allow(clippy::unused_async)] // m2: store fills the body
    pub async fn set_setting(&self, key: &str, value: &serde_json::Value) -> Result<()> {
        let _ = (self, key, value);
        unimplemented!("m2: store")
    }
}

/// [`HistoryRepository`] in the app database (`query_history` table, settings under
/// [`HISTORY_SETTINGS_KEY`]).
///
/// Search uses `LIKE` with `%`, `_` and the escape character escaped, so it matches the
/// substring literally (ASCII case-insensitive, like [`HistoryQuery::matches`]).
#[derive(Debug, Clone)]
pub struct SqliteHistoryRepository {
    db: AppDb,
}

impl SqliteHistoryRepository {
    pub fn new(db: AppDb) -> Self {
        Self { db }
    }

    pub fn db(&self) -> &AppDb {
        &self.db
    }
}

#[async_trait]
impl HistoryRepository for SqliteHistoryRepository {
    async fn append(&self, entry: &HistoryEntry) -> Result<()> {
        let _ = entry;
        unimplemented!("m2: store")
    }

    async fn list(&self, query: &HistoryQuery) -> Result<Vec<HistoryEntry>> {
        let _ = query;
        unimplemented!("m2: store")
    }

    async fn delete(&self, ids: &[Uuid]) -> Result<u64> {
        let _ = ids;
        unimplemented!("m2: store")
    }

    async fn clear(&self, profile_id: Option<Uuid>) -> Result<u64> {
        let _ = profile_id;
        unimplemented!("m2: store")
    }

    async fn prune(&self, keep: u32) -> Result<u64> {
        let _ = keep;
        unimplemented!("m2: store")
    }

    async fn settings(&self) -> Result<HistorySettings> {
        unimplemented!("m2: store")
    }

    async fn save_settings(&self, settings: &HistorySettings) -> Result<()> {
        let _ = settings;
        unimplemented!("m2: store")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_ordered_from_one() {
        for (i, m) in MIGRATIONS.iter().enumerate() {
            assert_eq!(usize::try_from(m.version).unwrap(), i + 1, "{}", m.name);
            assert_ne!(m.sql.trim(), "");
        }
    }
}
