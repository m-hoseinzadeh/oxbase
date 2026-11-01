//! M6: user snippets and schema-cache snapshots in the app database (`docs/m6-contracts.md`
//! §3.3 and §6), with in-memory twins for tests and runs without an app database.
//!
//! Migration 2 adds the tables:
//!
//! - `snippets(id, name, prefix, body, description, engines, created_at_ms, updated_at_ms)`:
//!   `engines` is a JSON array of engine names (`[]` = every SQL engine);
//! - `schema_cache_snapshots(profile_id, database_name, format, saved_at_ms, snapshot)`: one
//!   row per (profile, database), the snapshot bytes as `oxbase_schema_cache::codec` writes
//!   them. Never holds secrets (names and types only).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use oxbase_types::{Engine, Error, Result, Snippet, SnippetInput};

use crate::app_db::{AppDb, db_error};

// --- snippets ---------------------------------------------------------------------------------

/// User snippets. `list` orders by prefix, then name, then id.
#[async_trait]
pub trait SnippetRepository: Send + Sync + fmt::Debug {
    /// Every user snippet.
    async fn list(&self) -> Result<Vec<Snippet>>;

    /// Validates `input` ([`SnippetInput::validate`]) and creates it (`id: None`, a new UUID
    /// v7) or creates / replaces the snippet with that id. Returns the stored snippet.
    async fn save(&self, input: &SnippetInput) -> Result<Snippet>;

    /// Removes a snippet; `false` when it did not exist.
    async fn delete(&self, id: Uuid) -> Result<bool>;
}

fn to_snippet(id: Uuid, input: &SnippetInput) -> Snippet {
    Snippet {
        id: id.to_string(),
        name: input.name.clone(),
        prefix: input.prefix.clone(),
        body: input.body.clone(),
        description: input.description.clone(),
        engines: input.engines.clone(),
        builtin: false,
    }
}

fn sort_snippets(list: &mut [Snippet]) {
    list.sort_by(|a, b| {
        a.prefix
            .cmp(&b.prefix)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// In-memory [`SnippetRepository`].
#[derive(Debug, Default)]
pub struct MemorySnippetRepository {
    snippets: Mutex<BTreeMap<Uuid, Snippet>>,
}

impl MemorySnippetRepository {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<Uuid, Snippet>>> {
        self.snippets
            .lock()
            .map_err(|_| Error::internal("snippet store lock poisoned"))
    }
}

#[async_trait]
impl SnippetRepository for MemorySnippetRepository {
    async fn list(&self) -> Result<Vec<Snippet>> {
        let mut list: Vec<Snippet> = self.lock()?.values().cloned().collect();
        sort_snippets(&mut list);
        Ok(list)
    }

    async fn save(&self, input: &SnippetInput) -> Result<Snippet> {
        input.validate()?;
        let id = input.id.unwrap_or_else(Uuid::now_v7);
        let snippet = to_snippet(id, input);
        self.lock()?.insert(id, snippet.clone());
        Ok(snippet)
    }

    async fn delete(&self, id: Uuid) -> Result<bool> {
        Ok(self.lock()?.remove(&id).is_some())
    }
}

/// [`SnippetRepository`] in the app database (`snippets` table).
#[derive(Debug, Clone)]
pub struct SqliteSnippetRepository {
    db: AppDb,
}

impl SqliteSnippetRepository {
    pub fn new(db: AppDb) -> Self {
        Self { db }
    }
}

fn engines_json(engines: &[Engine]) -> String {
    serde_json::to_string(engines).unwrap_or_else(|_| "[]".to_owned())
}

#[async_trait]
impl SnippetRepository for SqliteSnippetRepository {
    async fn list(&self) -> Result<Vec<Snippet>> {
        let rows: Vec<(String, String, String, String, String, String)> = self
            .db
            .with(|conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, name, prefix, body, description, engines FROM snippets \
                         ORDER BY prefix, name, id",
                    )
                    .map_err(db_error)?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                        ))
                    })
                    .map_err(db_error)?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(db_error)
            })
            .await?;
        rows.into_iter()
            .map(|(id, name, prefix, body, description, engines)| {
                let engines: Vec<Engine> = serde_json::from_str(&engines)
                    .map_err(|_| Error::internal("app database: bad snippet engine list"))?;
                Ok(Snippet {
                    id,
                    name,
                    prefix,
                    body,
                    description,
                    engines,
                    builtin: false,
                })
            })
            .collect()
    }

    async fn save(&self, input: &SnippetInput) -> Result<Snippet> {
        input.validate()?;
        let id = input.id.unwrap_or_else(Uuid::now_v7);
        let snippet = to_snippet(id, input);
        let row = snippet.clone();
        let now = Utc::now().timestamp_millis();
        self.db
            .with(move |conn| {
                conn.execute(
                    "INSERT INTO snippets \
                     (id, name, prefix, body, description, engines, created_at_ms, updated_at_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7) \
                     ON CONFLICT (id) DO UPDATE SET name = excluded.name, \
                     prefix = excluded.prefix, body = excluded.body, \
                     description = excluded.description, engines = excluded.engines, \
                     updated_at_ms = excluded.updated_at_ms",
                    params![
                        row.id,
                        row.name,
                        row.prefix,
                        row.body,
                        row.description,
                        engines_json(&row.engines),
                        now
                    ],
                )
                .map_err(db_error)?;
                Ok(())
            })
            .await?;
        Ok(snippet)
    }

    async fn delete(&self, id: Uuid) -> Result<bool> {
        let id = id.to_string();
        self.db
            .with(move |conn| {
                let n = conn
                    .execute("DELETE FROM snippets WHERE id = ?1", params![id])
                    .map_err(db_error)?;
                Ok(n > 0)
            })
            .await
    }
}

// --- schema-cache snapshots -------------------------------------------------------------------

/// One persisted schema-cache snapshot.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredSnapshot {
    pub profile_id: Uuid,
    pub database: String,
    /// `CatalogSnapshot::FORMAT` of the build that wrote it.
    pub format: u32,
    pub saved_at: DateTime<Utc>,
    /// The encoded snapshot (`oxbase_schema_cache::codec`).
    pub bytes: Vec<u8>,
}

impl fmt::Debug for StoredSnapshot {
    /// Without the bytes (they can be megabytes).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredSnapshot")
            .field("profile_id", &self.profile_id)
            .field("database", &self.database)
            .field("format", &self.format)
            .field("saved_at", &self.saved_at)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// Persisted schema-cache snapshots, one per (profile, database).
#[async_trait]
pub trait SchemaCacheRepository: Send + Sync + fmt::Debug {
    /// The snapshot of (`profile_id`, `database`), if any.
    async fn load(&self, profile_id: Uuid, database: &str) -> Result<Option<StoredSnapshot>>;

    /// Stores (replaces) the snapshot of its (profile, database).
    async fn save(&self, snapshot: &StoredSnapshot) -> Result<()>;

    /// Removes the snapshots of a profile (every database when `database` is `None`, as when
    /// the profile is deleted). Returns how many were removed.
    async fn delete(&self, profile_id: Uuid, database: Option<&str>) -> Result<u64>;
}

/// In-memory [`SchemaCacheRepository`].
#[derive(Debug, Default)]
pub struct MemorySchemaCacheRepository {
    snapshots: Mutex<BTreeMap<(Uuid, String), StoredSnapshot>>,
}

impl MemorySchemaCacheRepository {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<(Uuid, String), StoredSnapshot>>> {
        self.snapshots
            .lock()
            .map_err(|_| Error::internal("schema cache store lock poisoned"))
    }
}

#[async_trait]
impl SchemaCacheRepository for MemorySchemaCacheRepository {
    async fn load(&self, profile_id: Uuid, database: &str) -> Result<Option<StoredSnapshot>> {
        Ok(self
            .lock()?
            .get(&(profile_id, database.to_owned()))
            .cloned())
    }

    async fn save(&self, snapshot: &StoredSnapshot) -> Result<()> {
        self.lock()?.insert(
            (snapshot.profile_id, snapshot.database.clone()),
            snapshot.clone(),
        );
        Ok(())
    }

    async fn delete(&self, profile_id: Uuid, database: Option<&str>) -> Result<u64> {
        let mut map = self.lock()?;
        let before = map.len();
        map.retain(|(p, d), _| !(*p == profile_id && database.is_none_or(|db| db == d.as_str())));
        Ok(u64::try_from(before - map.len()).unwrap_or(0))
    }
}

/// [`SchemaCacheRepository`] in the app database (`schema_cache_snapshots` table).
#[derive(Debug, Clone)]
pub struct SqliteSchemaCacheRepository {
    db: AppDb,
}

impl SqliteSchemaCacheRepository {
    pub fn new(db: AppDb) -> Self {
        Self { db }
    }
}

#[async_trait]
impl SchemaCacheRepository for SqliteSchemaCacheRepository {
    async fn load(&self, profile_id: Uuid, database: &str) -> Result<Option<StoredSnapshot>> {
        let key = (profile_id.to_string(), database.to_owned());
        let row: Option<(u32, i64, Vec<u8>)> = self
            .db
            .with(move |conn| {
                conn.query_row(
                    "SELECT format, saved_at_ms, snapshot FROM schema_cache_snapshots \
                     WHERE profile_id = ?1 AND database_name = ?2",
                    params![key.0, key.1],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(db_error)
            })
            .await?;
        row.map(|(format, saved_at_ms, bytes)| {
            Ok(StoredSnapshot {
                profile_id,
                database: database.to_owned(),
                format,
                saved_at: DateTime::<Utc>::from_timestamp_millis(saved_at_ms).ok_or_else(|| {
                    Error::internal("app database: bad schema cache snapshot time")
                })?,
                bytes,
            })
        })
        .transpose()
    }

    async fn save(&self, snapshot: &StoredSnapshot) -> Result<()> {
        let s = snapshot.clone();
        self.db
            .with(move |conn| {
                conn.execute(
                    "INSERT INTO schema_cache_snapshots \
                     (profile_id, database_name, format, saved_at_ms, snapshot) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT (profile_id, database_name) DO UPDATE SET \
                     format = excluded.format, saved_at_ms = excluded.saved_at_ms, \
                     snapshot = excluded.snapshot",
                    params![
                        s.profile_id.to_string(),
                        s.database,
                        s.format,
                        s.saved_at.timestamp_millis(),
                        s.bytes
                    ],
                )
                .map_err(db_error)?;
                Ok(())
            })
            .await
    }

    async fn delete(&self, profile_id: Uuid, database: Option<&str>) -> Result<u64> {
        let profile = profile_id.to_string();
        let database = database.map(str::to_owned);
        self.db
            .with(move |conn| {
                let n = match database {
                    Some(db) => conn.execute(
                        "DELETE FROM schema_cache_snapshots \
                         WHERE profile_id = ?1 AND database_name = ?2",
                        params![profile, db],
                    ),
                    None => conn.execute(
                        "DELETE FROM schema_cache_snapshots WHERE profile_id = ?1",
                        params![profile],
                    ),
                }
                .map_err(db_error)?;
                Ok(u64::try_from(n).unwrap_or(0))
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::TimeZone;
    use pretty_assertions::assert_eq;

    use super::*;

    fn input(prefix: &str) -> SnippetInput {
        SnippetInput {
            id: None,
            name: format!("Snippet {prefix}"),
            prefix: prefix.to_owned(),
            body: "SELECT ${1:*} FROM $2".to_owned(),
            description: "d".to_owned(),
            engines: vec![Engine::Postgres],
        }
    }

    async fn snippet_contract(repo: Arc<dyn SnippetRepository>) {
        assert_eq!(repo.list().await.unwrap(), Vec::<Snippet>::new());
        let b = repo.save(&input("bb")).await.unwrap();
        let a = repo.save(&input("aa")).await.unwrap();
        assert!(!a.builtin);
        assert_eq!(repo.list().await.unwrap(), vec![a.clone(), b.clone()]);
        // Replace by id.
        let mut edit = input("cc");
        edit.id = Some(Uuid::parse_str(&a.id).unwrap());
        edit.engines = vec![];
        let a2 = repo.save(&edit).await.unwrap();
        assert_eq!(a2.id, a.id);
        assert_eq!(repo.list().await.unwrap(), vec![b.clone(), a2.clone()]);
        // Invalid input is rejected and nothing changes.
        let err = repo.save(&input("bad prefix")).await.unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);
        assert_eq!(repo.list().await.unwrap().len(), 2);
        assert!(repo.delete(Uuid::parse_str(&b.id).unwrap()).await.unwrap());
        assert!(!repo.delete(Uuid::parse_str(&b.id).unwrap()).await.unwrap());
        assert_eq!(repo.list().await.unwrap(), vec![a2]);
    }

    fn snapshot(profile: Uuid, db: &str, byte: u8) -> StoredSnapshot {
        StoredSnapshot {
            profile_id: profile,
            database: db.to_owned(),
            format: 1,
            saved_at: Utc.timestamp_millis_opt(1_700_000_000_123).unwrap(),
            bytes: vec![byte; 3],
        }
    }

    async fn cache_contract(repo: Arc<dyn SchemaCacheRepository>) {
        let (p, q) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(repo.load(p, "app").await.unwrap(), None);
        repo.save(&snapshot(p, "app", 1)).await.unwrap();
        repo.save(&snapshot(p, "app", 2)).await.unwrap();
        repo.save(&snapshot(p, "other", 3)).await.unwrap();
        repo.save(&snapshot(q, "app", 4)).await.unwrap();
        assert_eq!(
            repo.load(p, "app").await.unwrap(),
            Some(snapshot(p, "app", 2))
        );
        assert_eq!(repo.delete(p, Some("app")).await.unwrap(), 1);
        assert_eq!(repo.load(p, "app").await.unwrap(), None);
        assert_eq!(repo.delete(p, None).await.unwrap(), 1);
        assert_eq!(repo.load(p, "other").await.unwrap(), None);
        assert_eq!(
            repo.load(q, "app").await.unwrap(),
            Some(snapshot(q, "app", 4))
        );
        let debug = format!("{:?}", snapshot(q, "app", 4));
        assert!(debug.contains("bytes: 3"), "{debug}");
    }

    #[tokio::test]
    async fn memory_repositories_follow_the_contract() {
        snippet_contract(Arc::new(MemorySnippetRepository::new())).await;
        cache_contract(Arc::new(MemorySchemaCacheRepository::new())).await;
    }

    #[tokio::test]
    async fn sqlite_repositories_follow_the_contract_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(AppDb::FILE_NAME);
        let db = AppDb::open(&path).await.unwrap();
        assert_eq!(db.applied_migrations().await.unwrap(), [1, 2]);
        snippet_contract(Arc::new(SqliteSnippetRepository::new(db.clone()))).await;
        cache_contract(Arc::new(SqliteSchemaCacheRepository::new(db.clone()))).await;
        let kept = SqliteSnippetRepository::new(db.clone())
            .save(&input("keep"))
            .await
            .unwrap();
        drop(db);
        let db = AppDb::open(&path).await.unwrap();
        let list = SqliteSnippetRepository::new(db).list().await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1], kept);
    }
}
