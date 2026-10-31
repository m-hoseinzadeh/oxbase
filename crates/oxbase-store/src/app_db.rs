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
//! Owner of the bodies: the **store** stream (`docs/m2-contracts.md`); implemented with the
//! query stream in M2.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use uuid::Uuid;

use oxbase_types::{Error, Result};

use crate::history::{
    HistoryEntry, HistoryQuery, HistoryRepository, HistorySettings, HistoryStatus,
};

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
    conn: Arc<Mutex<Connection>>,
}

impl AppDb {
    /// File name inside [`app_data_dir`].
    pub const FILE_NAME: &'static str = "oxbase.db";

    /// Opens (creating if needed) the database at `path` and applies pending [`MIGRATIONS`].
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let at = path.clone();
        let conn = blocking(move || open_and_migrate(&at)).await?;
        Ok(Self {
            path,
            conn: Arc::new(Mutex::new(conn)),
        })
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
    pub async fn applied_migrations(&self) -> Result<Vec<u32>> {
        self.with(|conn| applied_versions(conn).map(|v| v.into_iter().collect()))
            .await
    }

    /// Reads a JSON setting; `None` when the key is absent.
    pub async fn get_setting(&self, key: &str) -> Result<Option<serde_json::Value>> {
        let key = key.to_owned();
        let text: Option<String> = self
            .with(move |conn| {
                conn.query_row(
                    "SELECT value FROM settings WHERE key = ?1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db_error)
            })
            .await?;
        text.map(|t| {
            serde_json::from_str(&t)
                .map_err(|e| Error::internal(format!("app database: unreadable setting: {e}")))
        })
        .transpose()
    }

    /// Writes (upserts) a JSON setting.
    pub async fn set_setting(&self, key: &str, value: &serde_json::Value) -> Result<()> {
        let key = key.to_owned();
        let text = value.to_string();
        self.with(move |conn| {
            conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![key, text],
            )
            .map_err(db_error)?;
            Ok(())
        })
        .await
    }

    /// Runs `f` on the connection, on the blocking pool.
    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let conn = Arc::clone(&self.conn);
        blocking(move || {
            let mut conn = conn
                .lock()
                .map_err(|_| Error::internal("app database lock poisoned"))?;
            f(&mut conn)
        })
        .await
    }
}

/// Opens the file with the connection settings of the module docs and migrates it.
fn open_and_migrate(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut conn = Connection::open(path).map_err(db_error)?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(db_error)?;
    let _mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(db_error)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(db_error)?;
    migrate(&mut conn, MIGRATIONS)?;
    Ok(conn)
}

/// Applies the migrations of `migrations` that are not recorded yet, each in a transaction.
fn migrate(conn: &mut Connection, migrations: &[Migration]) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            applied_at TEXT NOT NULL
        );",
    )
    .map_err(db_error)?;
    let applied = applied_versions(conn)?;
    let known = migrations.iter().map(|m| m.version).max().unwrap_or(0);
    if applied.last().is_some_and(|&newest| newest > known) {
        // Written by a newer build: use it as it is, never migrate down.
        return Ok(());
    }
    for m in migrations {
        if applied.contains(&m.version) {
            continue;
        }
        let tx = conn.transaction().map_err(db_error)?;
        tx.execute_batch(m.sql).map_err(|e| {
            Error::internal(format!(
                "app database: migration {} ({}) failed: {e}",
                m.version, m.name
            ))
        })?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            params![m.version, m.name, Utc::now().to_rfc3339()],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
    }
    Ok(())
}

fn applied_versions(conn: &Connection) -> Result<BTreeSet<u32>> {
    let mut stmt = conn
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .map_err(db_error)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, u32>(0))
        .map_err(db_error)?;
    rows.collect::<std::result::Result<BTreeSet<u32>, _>>()
        .map_err(db_error)
}

#[allow(clippy::needless_pass_by_value)] // used with `map_err`
fn db_error(e: rusqlite::Error) -> Error {
    if let rusqlite::Error::SqliteFailure(f, _) = &e
        && f.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return Error::invalid_input(format!("app database: {e}"));
    }
    Error::Io {
        kind: "other".to_owned(),
        message: format!("app database: {e}"),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| Error::internal("app database task stopped unexpectedly"))?
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

const HISTORY_COLUMNS: &str = "id, profile_id, database_name, sql_text, started_at_ms, \
                               duration_ms, status, row_count, error_text";

/// `text` with the `LIKE` wildcards and the escape character escaped (escape: `\`).
fn like_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('%');
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawEntry> {
    Ok(RawEntry {
        id: row.get(0)?,
        profile_id: row.get(1)?,
        database: row.get(2)?,
        sql: row.get(3)?,
        started_at_ms: row.get(4)?,
        duration_ms: row.get(5)?,
        status: row.get(6)?,
        rows: row.get(7)?,
        error: row.get(8)?,
    })
}

/// A `query_history` row before its text columns are parsed.
struct RawEntry {
    id: String,
    profile_id: String,
    database: Option<String>,
    sql: String,
    started_at_ms: i64,
    duration_ms: i64,
    status: String,
    rows: Option<i64>,
    error: Option<String>,
}

impl RawEntry {
    fn parse(self) -> Result<HistoryEntry> {
        let bad = |what: &str| Error::internal(format!("app database: bad history {what}"));
        let status = match self.status.as_str() {
            "ok" => HistoryStatus::Ok,
            "error" => HistoryStatus::Error,
            "cancelled" => HistoryStatus::Cancelled,
            _ => return Err(bad("status")),
        };
        Ok(HistoryEntry {
            id: Uuid::parse_str(&self.id).map_err(|_| bad("id"))?,
            profile_id: Uuid::parse_str(&self.profile_id).map_err(|_| bad("profile id"))?,
            database: self.database,
            sql: self.sql,
            started_at: DateTime::<Utc>::from_timestamp_millis(self.started_at_ms)
                .ok_or_else(|| bad("time"))?,
            duration_ms: u64::try_from(self.duration_ms).unwrap_or(0),
            status,
            rows: self.rows.and_then(|r| u64::try_from(r).ok()),
            error: self.error,
        })
    }
}

#[async_trait]
impl HistoryRepository for SqliteHistoryRepository {
    async fn append(&self, entry: &HistoryEntry) -> Result<()> {
        let entry = entry.clone();
        self.db
            .with(move |conn| {
                conn.execute(
                    &format!(
                        "INSERT INTO query_history ({HISTORY_COLUMNS}) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
                    ),
                    params![
                        entry.id.to_string(),
                        entry.profile_id.to_string(),
                        entry.database,
                        entry.sql,
                        entry.started_at.timestamp_millis(),
                        to_i64(entry.duration_ms),
                        entry.status.as_str(),
                        entry.rows.map(to_i64),
                        entry.error,
                    ],
                )
                .map_err(|e| match db_error(e) {
                    Error::InvalidInput { .. } => {
                        Error::invalid_input(format!("history entry {} already exists", entry.id))
                    }
                    other => other,
                })?;
                Ok(())
            })
            .await
    }

    async fn list(&self, query: &HistoryQuery) -> Result<Vec<HistoryEntry>> {
        let mut clauses: Vec<&str> = Vec::new();
        let mut args: Vec<SqlValue> = Vec::new();
        if let Some(profile) = query.profile_id {
            clauses.push("profile_id = ?");
            args.push(SqlValue::Text(profile.to_string()));
        }
        if let Some(before) = query.before {
            clauses.push("started_at_ms < ?");
            args.push(SqlValue::Integer(before.timestamp_millis()));
        }
        if let Some(needle) = query.search_text() {
            clauses.push("sql_text LIKE ? ESCAPE '\\'");
            args.push(SqlValue::Text(like_literal(needle)));
        }
        let mut sql = format!("SELECT {HISTORY_COLUMNS} FROM query_history");
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY started_at_ms DESC, id DESC LIMIT ?");
        args.push(SqlValue::Integer(
            i64::try_from(query.effective_limit()).unwrap_or(i64::MAX),
        ));
        let raw = self
            .db
            .with(move |conn| {
                let mut stmt = conn.prepare(&sql).map_err(db_error)?;
                let rows = stmt
                    .query_map(params_from_iter(args.iter()), entry_from_row)
                    .map_err(db_error)?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(db_error)
            })
            .await?;
        raw.into_iter().map(RawEntry::parse).collect()
    }

    async fn delete(&self, ids: &[Uuid]) -> Result<u64> {
        let ids: Vec<String> = ids.iter().map(Uuid::to_string).collect();
        self.db
            .with(move |conn| {
                let tx = conn.transaction().map_err(db_error)?;
                let mut removed = 0_u64;
                {
                    let mut stmt = tx
                        .prepare("DELETE FROM query_history WHERE id = ?1")
                        .map_err(db_error)?;
                    for id in &ids {
                        let n = stmt.execute(params![id]).map_err(db_error)?;
                        removed += u64::try_from(n).unwrap_or(0);
                    }
                }
                tx.commit().map_err(db_error)?;
                Ok(removed)
            })
            .await
    }

    async fn clear(&self, profile_id: Option<Uuid>) -> Result<u64> {
        self.db
            .with(move |conn| {
                let n = match profile_id {
                    Some(p) => conn.execute(
                        "DELETE FROM query_history WHERE profile_id = ?1",
                        params![p.to_string()],
                    ),
                    None => conn.execute("DELETE FROM query_history", []),
                }
                .map_err(db_error)?;
                Ok(u64::try_from(n).unwrap_or(0))
            })
            .await
    }

    async fn prune(&self, keep: u32) -> Result<u64> {
        if keep == 0 {
            return Ok(0);
        }
        self.db
            .with(move |conn| {
                let n = conn
                    .execute(
                        "DELETE FROM query_history WHERE id IN (\
                            SELECT id FROM query_history \
                            ORDER BY started_at_ms DESC, id DESC LIMIT -1 OFFSET ?1)",
                        params![keep],
                    )
                    .map_err(db_error)?;
                Ok(u64::try_from(n).unwrap_or(0))
            })
            .await
    }

    async fn settings(&self) -> Result<HistorySettings> {
        match self.db.get_setting(HISTORY_SETTINGS_KEY).await? {
            Some(value) => serde_json::from_value(value)
                .map_err(|e| Error::internal(format!("app database: bad history settings: {e}"))),
            None => Ok(HistorySettings::default()),
        }
    }

    async fn save_settings(&self, settings: &HistorySettings) -> Result<()> {
        let value = serde_json::to_value(settings)
            .map_err(|e| Error::internal(format!("history settings: {e}")))?;
        self.db.set_setting(HISTORY_SETTINGS_KEY, &value).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use pretty_assertions::assert_eq;

    fn entry(profile: Uuid, secs: i64, sql: &str) -> HistoryEntry {
        HistoryEntry {
            id: Uuid::now_v7(),
            profile_id: profile,
            database: Some("oxbase_test".into()),
            sql: sql.into(),
            started_at: Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap(),
            duration_ms: 12,
            status: HistoryStatus::Ok,
            rows: Some(3),
            error: None,
        }
    }

    async fn open(dir: &tempfile::TempDir) -> AppDb {
        AppDb::open(dir.path().join("nested").join(AppDb::FILE_NAME))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn migrations_apply_once_and_reopen_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir).await;
        assert!(db.path().is_file());
        assert_eq!(db.applied_migrations().await.unwrap(), [1]);
        drop(db);
        let db = open(&dir).await;
        assert_eq!(db.applied_migrations().await.unwrap(), [1]);
        let mode: String = db
            .with(|c| {
                c.query_row("PRAGMA journal_mode", [], |r| r.get(0))
                    .map_err(db_error)
            })
            .await
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[tokio::test]
    async fn a_database_from_a_newer_build_opens_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir).await;
        db.with(|c| {
            c.execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (99, 'future', 'x')",
                [],
            )
            .map_err(db_error)?;
            Ok(())
        })
        .await
        .unwrap();
        drop(db);
        let db = open(&dir).await;
        assert_eq!(db.applied_migrations().await.unwrap(), [1, 99]);
        // Settings and history still work.
        db.set_setting("k", &serde_json::json!({"a": 1}))
            .await
            .unwrap();
        assert_eq!(
            db.get_setting("k").await.unwrap(),
            Some(serde_json::json!({"a": 1}))
        );
    }

    #[test]
    fn a_failing_migration_rolls_back() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, MIGRATIONS).unwrap();
        let bad = [
            MIGRATIONS[0],
            Migration {
                version: 2,
                name: "broken",
                sql: "CREATE TABLE half (a INTEGER); THIS IS NOT SQL;",
            },
        ];
        let err = migrate(&mut conn, &bad).unwrap_err();
        assert!(err.to_string().contains("migration 2"), "{err}");
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'half'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0);
        assert_eq!(
            applied_versions(&conn)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            [1]
        );
    }

    #[tokio::test]
    async fn settings_round_trip_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir).await;
        assert_eq!(db.get_setting("missing").await.unwrap(), None);
        db.set_setting("k", &serde_json::json!(1)).await.unwrap();
        db.set_setting("k", &serde_json::json!("two"))
            .await
            .unwrap();
        let repo = SqliteHistoryRepository::new(db.clone());
        assert_eq!(repo.settings().await.unwrap(), HistorySettings::default());
        let off = HistorySettings {
            enabled: false,
            max_entries: 7,
        };
        repo.save_settings(&off).await.unwrap();
        drop((db, repo));
        let db = open(&dir).await;
        assert_eq!(
            db.get_setting("k").await.unwrap(),
            Some(serde_json::json!("two"))
        );
        let repo = SqliteHistoryRepository::new(db);
        assert_eq!(repo.settings().await.unwrap(), off);
    }

    #[tokio::test]
    async fn history_round_trip_search_paging_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SqliteHistoryRepository::new(open(&dir).await);
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let e1 = entry(a, 1, "SELECT 1");
        let mut e2 = entry(a, 2, "select * from Orders WHERE x LIKE 'a%_b'");
        e2.status = HistoryStatus::Error;
        e2.rows = None;
        e2.error = Some("SQL error: boom".into());
        e2.database = None;
        let mut e3 = entry(b, 3, "UPDATE orders SET x = 1");
        e3.status = HistoryStatus::Cancelled;
        // Same start time as e3: the id breaks the tie.
        let e4 = entry(b, 3, "SELECT 100% done_x");
        for e in [&e1, &e2, &e3, &e4] {
            repo.append(e).await.unwrap();
        }
        let err = repo.append(&e1).await.unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);

        let all = repo.list(&HistoryQuery::default()).await.unwrap();
        assert_eq!(all, vec![e4.clone(), e3.clone(), e2.clone(), e1.clone()]);

        let search = |text: &str| HistoryQuery {
            search: Some(text.into()),
            ..HistoryQuery::default()
        };
        assert_eq!(
            repo.list(&search(" ORDERS ")).await.unwrap(),
            vec![e3.clone(), e2.clone()]
        );
        // `%` and `_` are literal.
        assert_eq!(repo.list(&search("a%_b")).await.unwrap(), vec![e2.clone()]);
        assert_eq!(
            repo.list(&search("%")).await.unwrap(),
            vec![e4.clone(), e2.clone()]
        );
        assert_eq!(repo.list(&search("e_x")).await.unwrap(), vec![e4.clone()]);
        assert_eq!(repo.list(&search("e_")).await.unwrap(), vec![e4.clone()]);
        assert_eq!(repo.list(&search("\\")).await.unwrap(), Vec::new());
        assert_eq!(repo.list(&search("  ")).await.unwrap().len(), 4);

        let q = HistoryQuery {
            profile_id: Some(a),
            limit: 1,
            ..HistoryQuery::default()
        };
        assert_eq!(repo.list(&q).await.unwrap(), vec![e2.clone()]);
        // Paging with `before`.
        let q = HistoryQuery {
            before: Some(e3.started_at),
            ..HistoryQuery::default()
        };
        assert_eq!(repo.list(&q).await.unwrap(), vec![e2.clone(), e1.clone()]);

        assert_eq!(repo.delete(&[e1.id, Uuid::now_v7()]).await.unwrap(), 1);
        assert_eq!(repo.clear(Some(b)).await.unwrap(), 2);
        assert_eq!(repo.list(&HistoryQuery::default()).await.unwrap(), vec![e2]);
        assert_eq!(repo.clear(None).await.unwrap(), 1);
        assert_eq!(
            repo.list(&HistoryQuery::default()).await.unwrap(),
            Vec::<HistoryEntry>::new()
        );
    }

    #[tokio::test]
    async fn prune_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SqliteHistoryRepository::new(open(&dir).await);
        let p = Uuid::now_v7();
        for i in 0..5 {
            repo.append(&entry(p, i, &format!("SELECT {i}")))
                .await
                .unwrap();
        }
        assert_eq!(repo.prune(0).await.unwrap(), 0);
        assert_eq!(repo.prune(10).await.unwrap(), 0);
        assert_eq!(repo.prune(2).await.unwrap(), 3);
        let left: Vec<String> = repo
            .list(&HistoryQuery::default())
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.sql)
            .collect();
        assert_eq!(left, ["SELECT 4", "SELECT 3"]);
    }

    #[test]
    fn like_patterns_escape_wildcards() {
        assert_eq!(like_literal("a%b_c\\d"), "%a\\%b\\_c\\\\d%");
    }

    #[test]
    fn migrations_are_ordered_from_one() {
        for (i, m) in MIGRATIONS.iter().enumerate() {
            assert_eq!(usize::try_from(m.version).unwrap(), i + 1, "{}", m.name);
            assert_ne!(m.sql.trim(), "");
        }
    }
}
