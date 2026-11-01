//! M6: the live schema caches behind code insight (`docs/m6-contracts.md` §3). Owner of the
//! bodies marked `unimplemented!("m6: cache")`: the **cache** stream.
//!
//! One [`SchemaCache`] per [`CacheKey`] (connection profile, database), shared by every editor
//! on that database:
//!
//! 1. **Startup.** [`SchemaCacheService::cache`] never touches the network: it answers from
//!    memory, else from the snapshot persisted in the app database
//!    ([`SchemaCacheRepository`]), else `None`.
//! 2. **Build.** [`SchemaCacheService::ensure`] starts a build in the background when the
//!    cache is missing or older than [`SchemaCacheConfig::stale_after`] and returns at once;
//!    [`SchemaCacheService::refresh`] builds (fully or for a [`RefreshScope`]) and waits. A
//!    build opens its own child session ([`ConnectionManager::open_child`]) on the requested
//!    database, so it never queues behind an editor's query or the object tree, reads the
//!    catalog with the engine's `CatalogReader` (`oxbase_dialect::catalog`), swaps the new
//!    cache in, persists it (snapshots up to `oxbase_schema_cache::codec::MAX_SNAPSHOT_BYTES`)
//!    and closes the session. One build per key at a time; a second request joins it.
//! 3. **Invalidation.** Services that change the schema publish a [`SchemaChangeEvent`] on
//!    [`SchemaEvents`]: the designer after an apply, the import after it created a table, the
//!    query service after a statement for which `oxbase_sql::schema_changes` is not empty
//!    (or after any statement of a script that ran `sp_rename`, dynamic SQL, ...). The cache
//!    service subscribes and refreshes the named relations (a partial read merged with
//!    `SchemaCache::merge`), or the whole cache when a change has no name or is not about a
//!    table or view. Refreshes coalesce for 300 ms.
//! 4. **Memory.** At most [`SchemaCacheConfig::max_caches`] caches stay in memory (least
//!    recently used are dropped; their snapshots stay on disk). A build that exceeds
//!    `CatalogScope::max_columns` keeps what it read and is marked truncated.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use oxbase_dialect::DialectRegistry;
use oxbase_schema_cache::SchemaCache;
use oxbase_sql::SchemaChange;
use oxbase_store::SchemaCacheRepository;
use oxbase_types::{Error, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{CancellationToken, ConnectionManager, SessionId};

/// Which cache: one per (connection profile, database).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheKey {
    pub profile_id: Uuid,
    /// The database name (`""` when the engine reports none, such as a SQLite file).
    pub database: String,
}

/// Settings of the [`SchemaCacheService`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaCacheConfig {
    /// A cache older than this is rebuilt in the background by `ensure`.
    pub stale_after: Duration,
    /// Caches kept in memory at once.
    pub max_caches: usize,
    /// Persist snapshots in the app database.
    pub persist: bool,
    /// Column limit of a build (`CatalogScope::max_columns`).
    pub max_columns: usize,
}

impl Default for SchemaCacheConfig {
    fn default() -> Self {
        Self {
            stale_after: Duration::from_secs(15 * 60),
            max_caches: 16,
            persist: true,
            max_columns: oxbase_dialect::CatalogScope::DEFAULT_MAX_COLUMNS,
        }
    }
}

/// The state of one cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheState {
    /// No names at all yet (no snapshot, no build finished).
    Missing,
    /// Fresh names.
    Ready,
    /// Names are there but older than `stale_after`, or a change event marked them stale; a
    /// refresh is due or running.
    Stale,
    /// The last build failed and there are no names (see `last_error`).
    Failed,
}

/// Where the names in memory came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheSource {
    /// The snapshot persisted by an earlier run.
    Disk,
    /// A catalog read in this run.
    Live,
}

/// What the UI shows about a cache (status bar, refresh button).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheStatus {
    pub key: CacheKey,
    pub state: CacheState,
    pub source: Option<CacheSource>,
    /// A build is running.
    pub building: bool,
    /// `SchemaCache::generation` of the cache in memory.
    pub generation: Option<u64>,
    /// When the catalog was read.
    pub read_at: Option<DateTime<Utc>>,
    pub relation_count: usize,
    pub column_count: usize,
    pub routine_count: usize,
    /// The build stopped at its column limit.
    pub truncated: bool,
    /// Duration of the last finished build, in milliseconds.
    pub last_build_ms: Option<u64>,
    /// Why the last build failed (category and message; no secrets).
    pub last_error: Option<Error>,
}

impl CacheStatus {
    /// The status of a key with no names and no build.
    pub fn missing(key: CacheKey) -> Self {
        Self {
            key,
            state: CacheState::Missing,
            source: None,
            building: false,
            generation: None,
            read_at: None,
            relation_count: 0,
            column_count: 0,
            routine_count: 0,
            truncated: false,
            last_build_ms: None,
            last_error: None,
        }
    }

    /// Fills the counts, generation and read time from `cache`.
    #[must_use]
    pub fn with_cache(mut self, cache: &SchemaCache) -> Self {
        let snap = cache.snapshot();
        self.generation = Some(cache.generation());
        self.read_at = DateTime::<Utc>::from_timestamp_millis(snap.read_at_ms);
        self.relation_count = snap.relation_count();
        self.column_count = snap.column_count();
        self.routine_count = snap.routine_count();
        self.truncated = snap.truncated;
        self
    }
}

/// One relation to refresh.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshTarget {
    #[serde(default)]
    pub schema: Option<String>,
    pub name: String,
}

/// What [`SchemaCacheService::refresh`] reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RefreshScope {
    /// The whole database.
    #[default]
    Full,
    /// These schemas, whole.
    Schemas { names: Vec<String> },
    /// These relations (with their columns and keys).
    Relations { targets: Vec<RefreshTarget> },
}

/// Who changed the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeSource {
    Designer,
    Import,
    Query,
}

/// A schema change some service made through `session`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaChangeEvent {
    /// The session the change ran on (an editor's child session is fine: the cache service
    /// maps it to its profile).
    pub session: SessionId,
    /// The database it ran in, when the publisher knows it; else the session's current one.
    pub database: Option<String>,
    pub source: ChangeSource,
    /// What changed; empty = unknown (refresh everything).
    pub changes: Vec<SchemaChange>,
}

/// The broadcast channel schema changes are published on. Cheap to clone; a publish with no
/// subscriber is dropped silently, and a slow subscriber loses the oldest events (it then
/// refreshes everything: `broadcast::error::RecvError::Lagged`).
#[derive(Clone)]
pub struct SchemaEvents {
    tx: broadcast::Sender<SchemaChangeEvent>,
}

impl SchemaEvents {
    /// Events kept for a subscriber that has not read them yet.
    pub const CAPACITY: usize = 256;

    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(Self::CAPACITY);
        Self { tx }
    }

    /// Publishes `event` to every current subscriber.
    pub fn publish(&self, event: SchemaChangeEvent) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SchemaChangeEvent> {
        self.tx.subscribe()
    }
}

impl Default for SchemaEvents {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SchemaEvents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchemaEvents")
            .field("subscribers", &self.tx.receiver_count())
            .finish()
    }
}

/// The schema caches of every open connection. See the module docs.
pub struct SchemaCacheService {
    manager: Arc<ConnectionManager>,
    dialects: Arc<DialectRegistry>,
    repo: Arc<dyn SchemaCacheRepository>,
    events: SchemaEvents,
    config: SchemaCacheConfig,
}

impl SchemaCacheService {
    pub fn new(
        manager: Arc<ConnectionManager>,
        dialects: Arc<DialectRegistry>,
        repo: Arc<dyn SchemaCacheRepository>,
        events: SchemaEvents,
        config: SchemaCacheConfig,
    ) -> Self {
        Self {
            manager,
            dialects,
            repo,
            events,
            config,
        }
    }

    pub fn manager(&self) -> &Arc<ConnectionManager> {
        &self.manager
    }

    pub fn dialects(&self) -> &Arc<DialectRegistry> {
        &self.dialects
    }

    pub fn repository(&self) -> &Arc<dyn SchemaCacheRepository> {
        &self.repo
    }

    /// The bus services publish schema changes on (shared with the designer, import and
    /// query services by the app's wiring).
    pub fn events(&self) -> &SchemaEvents {
        &self.events
    }

    pub fn config(&self) -> &SchemaCacheConfig {
        &self.config
    }

    /// The cache key of an open session: its profile, and `database` or else the session's
    /// current database (`""` when it has none). `NotFound` for an unknown session.
    pub fn key_for(&self, session: SessionId, database: Option<&str>) -> Result<CacheKey> {
        let handle = self
            .manager
            .get(session)
            .ok_or_else(|| Error::not_found("this connection is not open"))?;
        let database = database
            .map(str::to_owned)
            .or_else(|| handle.current_database())
            .or_else(|| handle.summary().database.clone())
            .unwrap_or_default();
        Ok(CacheKey {
            profile_id: handle.summary().profile_id,
            database,
        })
    }

    /// The cache of `key` without any network access: memory, else the persisted snapshot
    /// (decoded and kept in memory), else `None`. Must return within a few milliseconds once
    /// the snapshot is in memory (it is called on every completion request).
    #[allow(clippy::unused_async)] // skeleton until the cache stream fills it
    pub async fn cache(&self, key: &CacheKey) -> Option<SchemaCache> {
        let _ = (self, key);
        unimplemented!("m6: cache")
    }

    /// Starts a background build of the session's cache when it is missing or stale (no-op
    /// while one runs) and returns the current status at once.
    #[allow(clippy::unused_async)] // skeleton until the cache stream fills it
    pub async fn ensure(
        &self,
        session: SessionId,
        database: Option<String>,
    ) -> Result<CacheStatus> {
        let _ = (self, session, database);
        unimplemented!("m6: cache")
    }

    /// Reads `scope` of the session's database now (joining a running build of the same key),
    /// merges or replaces the cache, persists it and returns the new status. `Cancelled` when
    /// `cancel` fires (the old cache stays).
    #[allow(clippy::unused_async)] // skeleton until the cache stream fills it
    pub async fn refresh(
        &self,
        session: SessionId,
        database: Option<String>,
        scope: RefreshScope,
        cancel: CancellationToken,
    ) -> Result<CacheStatus> {
        let _ = (self, session, database, scope, cancel);
        unimplemented!("m6: cache")
    }

    /// The status of the session's cache (no network access).
    #[allow(clippy::unused_async)] // skeleton until the cache stream fills it
    pub async fn status(
        &self,
        session: SessionId,
        database: Option<String>,
    ) -> Result<CacheStatus> {
        let _ = (self, session, database);
        unimplemented!("m6: cache")
    }

    /// Applies a schema change (module docs, step 3): marks the cache stale and schedules the
    /// partial or full refresh. Called by the service's own subscription to [`SchemaEvents`];
    /// public for tests.
    pub fn invalidate(&self, event: &SchemaChangeEvent) {
        let _ = (self, event);
        unimplemented!("m6: cache")
    }

    /// Drops the caches and persisted snapshots of a profile (when it is deleted).
    #[allow(clippy::unused_async)] // skeleton until the cache stream fills it
    pub async fn forget_profile(&self, profile_id: Uuid) -> Result<()> {
        let _ = (self, profile_id);
        unimplemented!("m6: cache")
    }
}

impl fmt::Debug for SchemaCacheService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchemaCacheService")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use oxbase_sql::{DdlObject, DdlVerb, NamePart};
    use serde_json::json;

    use super::*;

    #[test]
    fn payloads_have_their_wire_form() {
        let scope: RefreshScope = serde_json::from_value(json!({
            "type": "relations",
            "targets": [{ "schema": "public", "name": "t" }, { "name": "u" }]
        }))
        .unwrap();
        assert_eq!(
            scope,
            RefreshScope::Relations {
                targets: vec![
                    RefreshTarget {
                        schema: Some("public".into()),
                        name: "t".into()
                    },
                    RefreshTarget {
                        schema: None,
                        name: "u".into()
                    }
                ]
            }
        );
        let full: RefreshScope = serde_json::from_value(json!({ "type": "full" })).unwrap();
        assert_eq!(full, RefreshScope::default());

        let key = CacheKey {
            profile_id: Uuid::nil(),
            database: "app".into(),
        };
        let status = CacheStatus::missing(key.clone());
        let v = serde_json::to_value(&status).unwrap();
        assert_eq!(v["state"], "missing");
        assert_eq!(v["key"]["profileId"], Uuid::nil().to_string());
        assert_eq!(v["relationCount"], 0);
        assert_eq!(v["lastError"], serde_json::Value::Null);

        let cache = SchemaCache::new(oxbase_schema_cache::synthetic::snapshot(
            &oxbase_schema_cache::synthetic::SyntheticSpec {
                tables: 10,
                columns_per_table: 3,
                routines: 2,
                ..oxbase_schema_cache::synthetic::SyntheticSpec::exit_check()
            },
        ));
        let status = CacheStatus::missing(key).with_cache(&cache);
        assert_eq!(
            (
                status.relation_count,
                status.column_count,
                status.routine_count
            ),
            (10, 30, 2)
        );
        assert_eq!(status.generation, Some(cache.generation()));
        let mut failed = status;
        failed.last_error = Some(Error::connection("refused"));
        let v = serde_json::to_value(&failed).unwrap();
        assert_eq!(v["lastError"]["category"], "connection");
    }

    #[tokio::test]
    async fn events_reach_every_subscriber() {
        let events = SchemaEvents::new();
        // No subscriber: dropped silently.
        events.publish(SchemaChangeEvent {
            session: SessionId::new(),
            database: None,
            source: ChangeSource::Query,
            changes: vec![],
        });
        let mut a = events.subscribe();
        let mut b = events.clone().subscribe();
        let event = SchemaChangeEvent {
            session: SessionId::new(),
            database: Some("app".into()),
            source: ChangeSource::Designer,
            changes: vec![SchemaChange {
                verb: DdlVerb::Alter,
                object: DdlObject::Table,
                name: vec![NamePart {
                    text: "t".into(),
                    quoted: false,
                }],
            }],
        };
        events.publish(event.clone());
        assert_eq!(a.recv().await.unwrap(), event);
        assert_eq!(b.recv().await.unwrap(), event);
        assert!(format!("{events:?}").contains("subscribers: 2"));
    }
}
