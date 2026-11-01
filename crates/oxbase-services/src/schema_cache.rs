//! M6: the live schema caches behind code insight (`docs/m6-contracts.md` §3), built by the
//! **cache** stream.
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

mod changes;

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use oxbase_dialect::{CatalogScope, DatabaseScope, DialectRegistry, RelationName};
use oxbase_schema_cache::{SchemaCache, codec};
use oxbase_sql::{DdlObject, DdlVerb, NamePart, SchemaChange};
use oxbase_store::{SchemaCacheRepository, StoredSnapshot};
use oxbase_types::{Error, ErrorCategory, ObjectKind, ObjectRef, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{CancellationToken, ConnectionManager, SessionId};
use changes::Refresh;

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
            max_columns: CatalogScope::DEFAULT_MAX_COLUMNS,
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
            .finish_non_exhaustive()
    }
}

/// How long the refreshes caused by change events of one key are gathered before they run.
pub const COALESCE: Duration = Duration::from_millis(300);

/// A failed build is not retried by [`SchemaCacheService::ensure`] for this long (a failing
/// connection is not hammered on every keystroke).
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// The schema caches of every open connection. See the module docs.
pub struct SchemaCacheService {
    manager: Arc<ConnectionManager>,
    dialects: Arc<DialectRegistry>,
    repo: Arc<dyn SchemaCacheRepository>,
    events: SchemaEvents,
    config: SchemaCacheConfig,
    core: Arc<Core>,
}

/// The state shared with background builds and the event listener.
struct Core {
    manager: Arc<ConnectionManager>,
    dialects: Arc<DialectRegistry>,
    repo: Arc<dyn SchemaCacheRepository>,
    config: SchemaCacheConfig,
    entries: Mutex<HashMap<CacheKey, Entry>>,
    /// LRU clock.
    clock: AtomicU64,
    /// Fires when the service is dropped: background builds and the listener stop.
    shutdown: CancellationToken,
    /// The subscription taken in `new`, until the listener task takes it over.
    receiver: Mutex<Option<broadcast::Receiver<SchemaChangeEvent>>>,
}

/// What a build reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildKind {
    Full,
    Partial,
}

/// One key's cache and bookkeeping.
#[derive(Default)]
struct Entry {
    cache: Option<SchemaCache>,
    source: Option<CacheSource>,
    /// LRU tick of the last use.
    used: u64,
    /// The persisted snapshot was looked for and there is none (or it was unreadable).
    disk_checked: bool,
    building: Option<BuildKind>,
    /// Finished builds (successful or not) and whether the last one succeeded.
    builds_finished: u64,
    last_build_ok: bool,
    last_build_ms: Option<u64>,
    last_error: Option<Error>,
    last_failure: Option<Instant>,
    /// Change events seen; a build clears `dirty` only when none arrived while it ran.
    changes: u64,
    dirty: bool,
    /// The base session to refresh on (the last one that used or changed the key).
    base: Option<SessionId>,
    /// Refresh waiting for the coalescing window.
    pending: Option<Pending>,
    /// Serializes the builds of the key.
    build_lock: Arc<tokio::sync::Mutex<()>>,
    /// Serializes loads of the persisted snapshot.
    load_lock: Arc<tokio::sync::Mutex<()>>,
    /// Woken whenever a build of the key ends (refreshes that join a build wait on it).
    done: Arc<tokio::sync::Notify>,
}

/// A coalesced refresh.
#[derive(Debug, Clone)]
struct Pending {
    session: SessionId,
    /// `None`: the whole database.
    relations: Option<Vec<RelationName>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

impl SchemaCacheService {
    pub fn new(
        manager: Arc<ConnectionManager>,
        dialects: Arc<DialectRegistry>,
        repo: Arc<dyn SchemaCacheRepository>,
        events: SchemaEvents,
        config: SchemaCacheConfig,
    ) -> Self {
        let core = Arc::new(Core {
            manager: Arc::clone(&manager),
            dialects: Arc::clone(&dialects),
            repo: Arc::clone(&repo),
            config: config.clone(),
            entries: Mutex::new(HashMap::new()),
            clock: AtomicU64::new(0),
            shutdown: CancellationToken::new(),
            receiver: Mutex::new(Some(events.subscribe())),
        });
        // Listen right away when there is a runtime; otherwise on the first async call.
        Core::start_listener(&core);
        Self {
            manager,
            dialects,
            repo,
            events,
            config,
            core,
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
        self.core.key_for(session, database)
    }

    /// The cache of `key` without any network access: memory, else the persisted snapshot
    /// (decoded and kept in memory), else `None`. Must return within a few milliseconds once
    /// the snapshot is in memory (it is called on every completion request).
    pub async fn cache(&self, key: &CacheKey) -> Option<SchemaCache> {
        Core::start_listener(&self.core);
        self.core.cache(key).await
    }

    /// Starts a background build of the session's cache when it is missing or stale (no-op
    /// while one runs) and returns the current status at once.
    pub async fn ensure(
        &self,
        session: SessionId,
        database: Option<String>,
    ) -> Result<CacheStatus> {
        Core::start_listener(&self.core);
        let key = self.core.key_for(session, database.as_deref())?;
        let base = self.core.base_of(session);
        let cache = self.core.cache(&key).await;
        let start = {
            let mut entries = lock(&self.core.entries);
            let entry = entries.entry(key.clone()).or_default();
            entry.base = Some(base);
            let due = match &cache {
                None => true,
                Some(cache) => entry.dirty || self.core.is_old(cache),
            };
            let resting = entry
                .last_failure
                .is_some_and(|t| t.elapsed() < RETRY_AFTER)
                || entry
                    .last_error
                    .as_ref()
                    .is_some_and(|e| e.category() == ErrorCategory::Unsupported);
            let start = due && !resting && entry.building.is_none() && entry.pending.is_none();
            if start {
                entry.building = Some(BuildKind::Full);
            }
            start
        };
        if start {
            let core = Arc::clone(&self.core);
            let key = key.clone();
            let cancel = self.core.shutdown.child_token();
            tokio::spawn(async move {
                let _ = core
                    .build(&key, base, CatalogScope::full(), cancel, true)
                    .await;
            });
        }
        Ok(self.core.status(&key))
    }

    /// Reads `scope` of the session's database now (joining a running build of the same key),
    /// merges or replaces the cache, persists it and returns the new status. `Cancelled` when
    /// `cancel` fires (the old cache stays).
    pub async fn refresh(
        &self,
        session: SessionId,
        database: Option<String>,
        scope: RefreshScope,
        cancel: CancellationToken,
    ) -> Result<CacheStatus> {
        Core::start_listener(&self.core);
        let key = self.core.key_for(session, database.as_deref())?;
        let base = self.core.base_of(session);
        let scope = self.core.catalog_scope(&scope);
        // A full refresh asked for while a full build runs joins that build.
        let joining = scope.is_full().then(|| {
            let mut entries = lock(&self.core.entries);
            let entry = entries.entry(key.clone()).or_default();
            entry.base = Some(base);
            (entry.building == Some(BuildKind::Full)).then_some(entry.builds_finished)
        });
        if let Some(Some(finished)) = joining {
            let done = Arc::clone(
                &lock(&self.core.entries)
                    .entry(key.clone())
                    .or_default()
                    .done,
            );
            let wait = async {
                loop {
                    // Created before the check, so a build finishing in between still wakes it.
                    let notified = done.notified();
                    let (over, ok) = lock(&self.core.entries)
                        .get(&key)
                        .map_or((true, false), |e| {
                            (e.builds_finished > finished, e.last_build_ok)
                        });
                    if over {
                        return Ok(ok);
                    }
                    notified.await;
                }
            };
            if crate::until_cancelled(&cancel, wait).await? {
                return Ok(self.core.status(&key));
            }
        }
        self.core.build(&key, base, scope, cancel, false).await
    }

    /// The status of the session's cache (no network access).
    pub async fn status(
        &self,
        session: SessionId,
        database: Option<String>,
    ) -> Result<CacheStatus> {
        Core::start_listener(&self.core);
        let key = self.core.key_for(session, database.as_deref())?;
        self.core.cache(&key).await;
        Ok(self.core.status(&key))
    }

    /// Applies a schema change (module docs, step 3): marks the cache stale and schedules the
    /// partial or full refresh. Called by the service's own subscription to [`SchemaEvents`];
    /// public for tests.
    pub fn invalidate(&self, event: &SchemaChangeEvent) {
        self.core.invalidate(event);
    }

    /// Drops the caches and persisted snapshots of a profile (when it is deleted).
    pub async fn forget_profile(&self, profile_id: Uuid) -> Result<()> {
        lock(&self.core.entries).retain(|k, _| k.profile_id != profile_id);
        self.repo.delete(profile_id, None).await?;
        Ok(())
    }
}

impl Drop for SchemaCacheService {
    fn drop(&mut self) {
        self.core.shutdown.cancel();
    }
}

impl Core {
    /// Spawns the event listener once, when a Tokio runtime is available.
    fn start_listener(core: &Arc<Self>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Some(receiver) = lock(&core.receiver).take() else {
            return;
        };
        let weak = Arc::downgrade(core);
        let shutdown = core.shutdown.clone();
        runtime.spawn(listen(weak, receiver, shutdown));
    }

    fn key_for(&self, session: SessionId, database: Option<&str>) -> Result<CacheKey> {
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

    /// The base session of `session` (itself when it is one, or unknown).
    fn base_of(&self, session: SessionId) -> SessionId {
        self.manager
            .get(session)
            .and_then(|h| h.summary().parent)
            .unwrap_or(session)
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// `true` when `cache` was read longer than `stale_after` ago.
    fn is_old(&self, cache: &SchemaCache) -> bool {
        let age = now_ms().saturating_sub(cache.snapshot().read_at_ms);
        u64::try_from(age).unwrap_or(0) > millis(self.config.stale_after)
    }

    fn catalog_scope(&self, scope: &RefreshScope) -> CatalogScope {
        let mut out = CatalogScope {
            max_columns: self.config.max_columns,
            ..CatalogScope::full()
        };
        match scope {
            RefreshScope::Full => {}
            RefreshScope::Schemas { names } => out.schemas = Some(names.clone()),
            RefreshScope::Relations { targets } => {
                out.relations = Some(
                    targets
                        .iter()
                        .map(|t| RelationName {
                            schema: t.schema.clone(),
                            name: t.name.clone(),
                        })
                        .collect(),
                );
            }
        }
        out
    }

    /// The cache in memory, else the persisted one (see [`SchemaCacheService::cache`]).
    async fn cache(&self, key: &CacheKey) -> Option<SchemaCache> {
        let load_lock = {
            let mut entries = lock(&self.entries);
            let entry = entries.entry(key.clone()).or_default();
            if let Some(cache) = &entry.cache {
                entry.used = self.tick();
                return Some(cache.clone());
            }
            if entry.disk_checked || !self.config.persist {
                return None;
            }
            Arc::clone(&entry.load_lock)
        };
        let _loading = load_lock.lock().await;
        {
            let mut entries = lock(&self.entries);
            let entry = entries.entry(key.clone()).or_default();
            if let Some(cache) = &entry.cache {
                entry.used = self.tick();
                return Some(cache.clone());
            }
            if entry.disk_checked {
                return None;
            }
        }
        let stored = match self.repo.load(key.profile_id, &key.database).await {
            Ok(stored) => stored,
            Err(e) => {
                tracing::debug!(category = %e.category(), "reading a schema cache snapshot failed");
                return None;
            }
        };
        let decoded = match stored {
            None => None,
            Some(stored) => {
                let decoded = tokio::task::spawn_blocking(move || {
                    codec::decode(&stored.bytes).map(SchemaCache::new)
                })
                .await;
                match decoded {
                    Ok(Ok(cache)) => Some(cache),
                    Ok(Err(e)) => {
                        // Another format or garbage: read the catalog again on the next build.
                        tracing::debug!(error = %e, "dropping an unreadable schema cache snapshot");
                        let _ = self.repo.delete(key.profile_id, Some(&key.database)).await;
                        None
                    }
                    Err(_) => None,
                }
            }
        };
        let mut entries = lock(&self.entries);
        let entry = entries.entry(key.clone()).or_default();
        if let Some(cache) = &entry.cache {
            // A build finished while the snapshot was decoded: it wins.
            entry.used = self.tick();
            return Some(cache.clone());
        }
        entry.disk_checked = true;
        let cache = decoded?;
        entry.cache = Some(cache.clone());
        entry.source = Some(CacheSource::Disk);
        entry.used = self.tick();
        Self::evict(&mut entries, key, self.config.max_caches);
        Some(cache)
    }

    /// Drops the least recently used caches beyond `max` (never `keep`).
    fn evict(entries: &mut HashMap<CacheKey, Entry>, keep: &CacheKey, max: usize) {
        loop {
            let loaded = entries.values().filter(|e| e.cache.is_some()).count();
            if loaded <= max.max(1) {
                return;
            }
            let victim = entries
                .iter()
                .filter(|(k, e)| e.cache.is_some() && *k != keep)
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone());
            let Some(victim) = victim else {
                return;
            };
            if let Some(e) = entries.get_mut(&victim) {
                e.cache = None;
                e.source = None;
                // The persisted snapshot can be loaded again.
                e.disk_checked = false;
            }
        }
    }

    fn status(&self, key: &CacheKey) -> CacheStatus {
        let entries = lock(&self.entries);
        let Some(entry) = entries.get(key) else {
            return CacheStatus::missing(key.clone());
        };
        let mut status = CacheStatus::missing(key.clone());
        status.building = entry.building.is_some() || entry.pending.is_some();
        status.last_build_ms = entry.last_build_ms;
        status.last_error.clone_from(&entry.last_error);
        status.source = entry.source;
        if let Some(cache) = &entry.cache {
            status.state = if entry.dirty || self.is_old(cache) {
                CacheState::Stale
            } else {
                CacheState::Ready
            };
            return status.with_cache(cache);
        }
        if entry.last_error.is_some() {
            status.state = CacheState::Failed;
        }
        status
    }

    /// Builds (`scope` full) or refreshes (partial) the cache of `key` on a child of `base`,
    /// one build per key at a time. `marked`: the caller already set `building`.
    async fn build(
        &self,
        key: &CacheKey,
        base: SessionId,
        mut scope: CatalogScope,
        cancel: CancellationToken,
        marked: bool,
    ) -> Result<CacheStatus> {
        if let Err(e) = crate::check_cancelled(&cancel) {
            if marked {
                self.finish(key, None, false);
            }
            return Err(e);
        }
        let (build_lock, set_here) = {
            let mut entries = lock(&self.entries);
            let entry = entries.entry(key.clone()).or_default();
            let set_here = !marked && entry.building.is_none();
            if set_here {
                entry.building = Some(if scope.is_full() {
                    BuildKind::Full
                } else {
                    BuildKind::Partial
                });
            }
            (Arc::clone(&entry.build_lock), set_here)
        };
        let guard = match crate::until_cancelled(&cancel, async {
            Ok(build_lock.lock_owned().await)
        })
        .await
        {
            Ok(guard) => guard,
            Err(e) => {
                if (set_here || marked)
                    && let Some(entry) = lock(&self.entries).get_mut(key)
                {
                    entry.building = None;
                }
                return Err(e);
            }
        };
        let (seen, current) = {
            let mut entries = lock(&self.entries);
            let entry = entries.entry(key.clone()).or_default();
            entry.building = Some(if scope.is_full() {
                BuildKind::Full
            } else {
                BuildKind::Partial
            });
            (entry.changes, entry.cache.clone())
        };
        // A partial refresh needs names to merge into (memory or disk), else it reads all.
        let current = match current {
            Some(c) => Some(c),
            None if !scope.is_full() => self.cache(key).await,
            None => None,
        };
        if current.is_none() {
            scope.schemas = None;
            scope.relations = None;
        }
        let started = Instant::now();
        let outcome = self
            .read_and_index(key, base, &scope, current, &cancel)
            .await;
        let elapsed = millis(started.elapsed());
        let result = match outcome {
            Ok(cache) => {
                self.persist(key, &cache).await;
                {
                    let mut entries = lock(&self.entries);
                    let entry = entries.entry(key.clone()).or_default();
                    entry.cache = Some(cache);
                    entry.source = Some(CacheSource::Live);
                    entry.used = self.tick();
                    entry.disk_checked = true;
                    entry.last_build_ms = Some(elapsed);
                    entry.last_error = None;
                    entry.last_failure = None;
                    if entry.changes == seen {
                        entry.dirty = false;
                    }
                    Self::evict(&mut entries, key, self.config.max_caches);
                }
                self.finish(key, Some(elapsed), true);
                Ok(self.status(key))
            }
            Err(e) => {
                if !e.is_cancelled() {
                    tracing::debug!(category = %e.category(), "schema cache build failed");
                    let mut entries = lock(&self.entries);
                    let entry = entries.entry(key.clone()).or_default();
                    entry.last_error = Some(e.clone());
                    entry.last_failure = Some(Instant::now());
                }
                self.finish(key, None, false);
                Err(e)
            }
        };
        drop(guard);
        result
    }

    /// Marks the end of a build of `key`.
    fn finish(&self, key: &CacheKey, elapsed: Option<u64>, ok: bool) {
        let mut entries = lock(&self.entries);
        let entry = entries.entry(key.clone()).or_default();
        entry.building = None;
        entry.builds_finished += 1;
        entry.last_build_ok = ok;
        if elapsed.is_some() {
            entry.last_build_ms = elapsed;
        }
        entry.done.notify_waiters();
    }

    /// Reads `scope` on a session of its own and builds the new index (merged into `current`
    /// for a partial scope).
    async fn read_and_index(
        &self,
        key: &CacheKey,
        base: SessionId,
        scope: &CatalogScope,
        current: Option<SchemaCache>,
        cancel: &CancellationToken,
    ) -> Result<SchemaCache> {
        let base_handle = self
            .manager
            .get(base)
            .ok_or_else(|| Error::not_found("this connection is not open"))?;
        let engine = base_handle.summary().engine;
        let reader = self.dialects.catalog_reader(engine)?;
        let (session, dedicated) = if self.manager.children_share_data(base) {
            let attached = self
                .dialects
                .tree_layout(engine)
                .is_ok_and(|l| l.scope == DatabaseScope::Attached);
            let database = (!attached && !key.database.is_empty()).then_some(key.database.as_str());
            let child = self
                .manager
                .open_child(base, database, cancel.clone())
                .await?;
            (child, true)
        } else {
            (base_handle, false)
        };
        let read = async {
            let _guard =
                crate::until_cancelled(cancel, async { Ok(session.exclusive().await) }).await?;
            reader
                .read(session.connection().as_ref(), scope, cancel.clone())
                .await
        }
        .await;
        if dedicated {
            match self.manager.close(session.id()).await {
                Ok(()) => {}
                Err(e) if e.category() == ErrorCategory::NotFound => {}
                Err(e) => {
                    tracing::debug!(category = %e.category(), "closing the cache session failed");
                }
            }
        }
        let snapshot = read?;
        crate::check_cancelled(cancel)?;
        let scope = scope.clone();
        let index = tokio::task::spawn_blocking(move || match current {
            Some(current) if !scope.is_full() => {
                let scope = changes::merge_scope(&current, &snapshot, &scope);
                current.merge(snapshot, &scope)
            }
            _ => SchemaCache::new(snapshot),
        });
        index
            .await
            .map_err(|_| Error::internal("building the schema cache index stopped unexpectedly"))
    }

    /// Saves the snapshot of `cache` for the next start (best effort).
    async fn persist(&self, key: &CacheKey, cache: &SchemaCache) {
        if !self.config.persist {
            return;
        }
        let cache = cache.clone();
        let Ok(bytes) = tokio::task::spawn_blocking(move || codec::encode(cache.snapshot())).await
        else {
            return;
        };
        if bytes.len() > codec::MAX_SNAPSHOT_BYTES {
            tracing::debug!(
                bytes = bytes.len(),
                "schema cache snapshot too large to persist"
            );
            return;
        }
        let stored = StoredSnapshot {
            profile_id: key.profile_id,
            database: key.database.clone(),
            format: oxbase_types::CatalogSnapshot::FORMAT,
            saved_at: Utc::now(),
            bytes,
        };
        if let Err(e) = self.repo.save(&stored).await {
            tracing::debug!(category = %e.category(), "saving a schema cache snapshot failed");
        }
    }

    /// See [`SchemaCacheService::invalidate`].
    fn invalidate(self: &Arc<Self>, event: &SchemaChangeEvent) {
        let Ok(key) = self.key_for(event.session, event.database.as_deref()) else {
            return;
        };
        let base = self.base_of(event.session);
        let current = lock(&self.entries).get(&key).and_then(|e| e.cache.clone());
        let plan = changes::plan(current.as_ref(), &event.changes);
        for database in &plan.other_databases {
            let other = CacheKey {
                profile_id: key.profile_id,
                database: database.clone(),
            };
            if let Some(e) = lock(&self.entries).get_mut(&other) {
                e.dirty = true;
                e.changes += 1;
            }
        }
        match plan.refresh {
            Refresh::Nothing => {}
            Refresh::Everything => self.schedule(&key, base, None),
            Refresh::Relations(relations) => self.schedule(&key, base, Some(relations)),
        }
    }

    /// Marks every key stale and refreshes the ones in memory (a lagged subscription).
    fn invalidate_all(self: &Arc<Self>) {
        let keys: Vec<(CacheKey, Option<SessionId>)> = lock(&self.entries)
            .iter()
            .map(|(k, e)| (k.clone(), e.base))
            .collect();
        for (key, base) in keys {
            match base {
                Some(base) => self.schedule(&key, base, None),
                None => {
                    if let Some(e) = lock(&self.entries).get_mut(&key) {
                        e.dirty = true;
                        e.changes += 1;
                    }
                }
            }
        }
    }

    /// Marks `key` stale and queues a refresh (`None` = everything) that runs after the
    /// coalescing window.
    fn schedule(
        self: &Arc<Self>,
        key: &CacheKey,
        base: SessionId,
        relations: Option<Vec<RelationName>>,
    ) {
        let spawn = {
            let mut entries = lock(&self.entries);
            let entry = entries.entry(key.clone()).or_default();
            entry.dirty = true;
            entry.changes += 1;
            entry.base = Some(base);
            if entry.cache.is_none() {
                // Nothing in memory to refresh: the next `ensure` reads it again.
                entry.disk_checked = false;
                return;
            }
            if let Some(pending) = &mut entry.pending {
                pending.session = base;
                match (&mut pending.relations, relations) {
                    (Some(have), Some(more)) => {
                        for r in more {
                            if !have.contains(&r) {
                                have.push(r);
                            }
                        }
                    }
                    (have, _) => *have = None,
                }
                false
            } else {
                entry.pending = Some(Pending {
                    session: base,
                    relations,
                });
                true
            }
        };
        if !spawn {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let core = Arc::clone(self);
        let key = key.clone();
        runtime.spawn(async move {
            tokio::select! {
                () = core.shutdown.cancelled() => return,
                () = tokio::time::sleep(COALESCE) => {}
            }
            let Some(pending) = lock(&core.entries)
                .get_mut(&key)
                .and_then(|e| e.pending.take())
            else {
                return;
            };
            let scope = CatalogScope {
                relations: pending.relations,
                max_columns: core.config.max_columns,
                ..CatalogScope::full()
            };
            let cancel = core.shutdown.child_token();
            let _ = core
                .build(&key, pending.session, scope, cancel, false)
                .await;
        });
    }
}

/// Applies the events of `receiver` until the service is dropped.
async fn listen(
    core: Weak<Core>,
    mut receiver: broadcast::Receiver<SchemaChangeEvent>,
    shutdown: CancellationToken,
) {
    loop {
        let received = tokio::select! {
            () = shutdown.cancelled() => return,
            r = receiver.recv() => r,
        };
        let Some(core) = core.upgrade() else {
            return;
        };
        match received {
            Ok(event) => core.invalidate(&event),
            Err(broadcast::error::RecvError::Lagged(_)) => core.invalidate_all(),
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// The [`SchemaChangeEvent`] for changes of tables or views the designer or the import made
/// through `session`: `changes` pairs each verb with the object as the tree names it.
pub(crate) fn object_event(
    dialects: &DialectRegistry,
    engine: oxbase_types::Engine,
    session: SessionId,
    source: ChangeSource,
    changes: &[(DdlVerb, &ObjectRef)],
) -> SchemaChangeEvent {
    // On engines whose session sees attached databases, the catalog of a ref is its schema.
    let attached = dialects
        .tree_layout(engine)
        .is_ok_and(|l| l.scope == DatabaseScope::Attached);
    let mut database = None;
    let changes = changes
        .iter()
        .map(|(verb, obj)| {
            let schema = if attached {
                obj.catalog.clone()
            } else {
                if database.is_none() {
                    database = obj.database_name().map(str::to_owned);
                }
                obj.schema.clone()
            };
            let part = |text: String| NamePart { text, quoted: true };
            let mut name: Vec<NamePart> = schema.into_iter().map(part).collect();
            name.push(part(obj.name.clone()));
            SchemaChange {
                verb: *verb,
                object: if obj.kind == ObjectKind::View {
                    DdlObject::View
                } else {
                    DdlObject::Table
                },
                name,
            }
        })
        .collect();
    SchemaChangeEvent {
        session,
        database,
        source,
        changes,
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
mod service_tests;

#[cfg(test)]
mod tests {
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
        let v = serde_json::to_value(status).unwrap();
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
        let v = serde_json::to_value(failed).unwrap();
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
