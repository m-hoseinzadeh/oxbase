//! Query history (M2): one entry per executed statement, newest first.
//!
//! [`HistoryRepository`] is the storage contract. [`MemoryHistoryRepository`] is complete (tests,
//! headless runs); [`SqliteHistoryRepository`](crate::SqliteHistoryRepository) stores entries in
//! the app database ([`AppDb`](crate::AppDb)).
//!
//! **Secrets.** SQL text can contain secrets (`CREATE USER ... PASSWORD '...'`). Repositories
//! store what they are given; the caller (`oxbase-services::history::HistoryService`) redacts
//! with `oxbase_sql::redact_secrets` *before* `append` and honours
//! [`HistorySettings::enabled`]. A repository never logs SQL text.

use std::fmt;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use oxbase_types::{Error, Result};

/// How a statement ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryStatus {
    Ok,
    Error,
    Cancelled,
}

impl HistoryStatus {
    /// Stable lowercase name (the serde form, also stored in the app database).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One executed statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// Time-ordered id (UUID v7).
    pub id: Uuid,
    /// The connection profile the statement ran on.
    pub profile_id: Uuid,
    /// The database the statement ran in, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Statement text, already redacted and possibly truncated by the caller.
    pub sql: String,
    pub started_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub status: HistoryStatus,
    /// Rows returned (result sets) or affected (DML), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    /// Error message for `status == Error`, already redacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A history listing request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HistoryQuery {
    /// Only entries of this profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<Uuid>,
    /// Case-insensitive (at least for ASCII) substring of `sql`. Empty or blank means none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    /// Only entries that started strictly before this time (paging: pass the last entry's
    /// `started_at`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<DateTime<Utc>>,
    /// At most this many entries; clamped to `1..=`[`HistoryQuery::MAX_LIMIT`].
    pub limit: u32,
}

impl HistoryQuery {
    /// Default page size.
    pub const DEFAULT_LIMIT: u32 = 200;
    /// Largest page a repository returns.
    pub const MAX_LIMIT: u32 = 5000;

    /// The limit actually applied.
    pub fn effective_limit(&self) -> usize {
        usize::try_from(self.limit.clamp(1, Self::MAX_LIMIT)).unwrap_or(usize::MAX)
    }

    /// The search text, trimmed, if any.
    pub fn search_text(&self) -> Option<&str> {
        self.search
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// `true` if `entry` passes the profile, search and `before` filters (not the limit).
    pub fn matches(&self, entry: &HistoryEntry) -> bool {
        self.profile_id.is_none_or(|p| p == entry.profile_id)
            && self.before.is_none_or(|b| entry.started_at < b)
            && self.search_text().is_none_or(|needle| {
                entry
                    .sql
                    .to_ascii_lowercase()
                    .contains(&needle.to_ascii_lowercase())
            })
    }
}

impl Default for HistoryQuery {
    fn default() -> Self {
        Self {
            profile_id: None,
            search: None,
            before: None,
            limit: Self::DEFAULT_LIMIT,
        }
    }
}

/// User settings for the history, stored next to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HistorySettings {
    /// `false`: nothing is recorded (existing entries stay until cleared).
    pub enabled: bool,
    /// Keep at most this many entries; older ones are pruned after appends. 0 = unlimited.
    pub max_entries: u32,
}

impl HistorySettings {
    pub const DEFAULT_MAX_ENTRIES: u32 = 10_000;
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: Self::DEFAULT_MAX_ENTRIES,
        }
    }
}

/// Stores [`HistoryEntry`]s and the [`HistorySettings`].
///
/// `list` orders newest first (`started_at` descending, then `id` descending). `delete` and
/// `clear` return how many entries were removed.
#[async_trait]
pub trait HistoryRepository: Send + Sync + fmt::Debug {
    async fn append(&self, entry: &HistoryEntry) -> Result<()>;

    async fn list(&self, query: &HistoryQuery) -> Result<Vec<HistoryEntry>>;

    async fn delete(&self, ids: &[Uuid]) -> Result<u64>;

    /// Removes every entry, or only those of `profile_id`.
    async fn clear(&self, profile_id: Option<Uuid>) -> Result<u64>;

    /// Removes the oldest entries so at most `keep` remain (`0` keeps everything).
    async fn prune(&self, keep: u32) -> Result<u64>;

    /// The stored settings, or the defaults when none were saved.
    async fn settings(&self) -> Result<HistorySettings>;

    async fn save_settings(&self, settings: &HistorySettings) -> Result<()>;
}

/// Newest first: `started_at` descending, then `id` descending.
fn newest_first(a: &HistoryEntry, b: &HistoryEntry) -> std::cmp::Ordering {
    b.started_at.cmp(&a.started_at).then(b.id.cmp(&a.id))
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// In-memory [`HistoryRepository`] for tests and runs without an app database.
#[derive(Debug, Default)]
pub struct MemoryHistoryRepository {
    state: Mutex<MemoryState>,
}

#[derive(Debug, Default)]
struct MemoryState {
    entries: Vec<HistoryEntry>,
    settings: Option<HistorySettings>,
}

impl MemoryHistoryRepository {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemoryState>> {
        self.state
            .lock()
            .map_err(|_| Error::internal("history lock poisoned"))
    }
}

#[async_trait]
impl HistoryRepository for MemoryHistoryRepository {
    async fn append(&self, entry: &HistoryEntry) -> Result<()> {
        let mut state = self.lock()?;
        if state.entries.iter().any(|e| e.id == entry.id) {
            return Err(Error::invalid_input(format!(
                "history entry {} already exists",
                entry.id
            )));
        }
        state.entries.push(entry.clone());
        Ok(())
    }

    async fn list(&self, query: &HistoryQuery) -> Result<Vec<HistoryEntry>> {
        let state = self.lock()?;
        let mut out: Vec<HistoryEntry> = state
            .entries
            .iter()
            .filter(|e| query.matches(e))
            .cloned()
            .collect();
        out.sort_by(newest_first);
        out.truncate(query.effective_limit());
        Ok(out)
    }

    async fn delete(&self, ids: &[Uuid]) -> Result<u64> {
        let mut state = self.lock()?;
        let before = state.entries.len();
        state.entries.retain(|e| !ids.contains(&e.id));
        Ok(count(before - state.entries.len()))
    }

    async fn clear(&self, profile_id: Option<Uuid>) -> Result<u64> {
        let mut state = self.lock()?;
        let before = state.entries.len();
        state
            .entries
            .retain(|e| profile_id.is_some_and(|p| p != e.profile_id));
        Ok(count(before - state.entries.len()))
    }

    async fn prune(&self, keep: u32) -> Result<u64> {
        let mut state = self.lock()?;
        let keep = usize::try_from(keep).unwrap_or(usize::MAX);
        if keep == 0 || state.entries.len() <= keep {
            return Ok(0);
        }
        state.entries.sort_by(newest_first);
        let removed = state.entries.len() - keep;
        state.entries.truncate(keep);
        Ok(count(removed))
    }

    async fn settings(&self) -> Result<HistorySettings> {
        Ok(self.lock()?.settings.clone().unwrap_or_default())
    }

    async fn save_settings(&self, settings: &HistorySettings) -> Result<()> {
        self.lock()?.settings = Some(settings.clone());
        Ok(())
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
            duration_ms: 3,
            status: HistoryStatus::Ok,
            rows: Some(1),
            error: None,
        }
    }

    #[tokio::test]
    async fn memory_repository_lists_filters_and_removes() {
        let repo = MemoryHistoryRepository::new();
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let e1 = entry(a, 1, "SELECT 1");
        let e2 = entry(a, 2, "select * from Orders");
        let e3 = entry(b, 3, "UPDATE orders SET x = 1");
        for e in [&e1, &e2, &e3] {
            repo.append(e).await.unwrap();
        }
        assert!(repo.append(&e1).await.is_err(), "duplicate id");

        let all = repo.list(&HistoryQuery::default()).await.unwrap();
        assert_eq!(all, vec![e3.clone(), e2.clone(), e1.clone()]);

        let q = HistoryQuery {
            search: Some(" ORDERS ".into()),
            ..HistoryQuery::default()
        };
        assert_eq!(repo.list(&q).await.unwrap(), vec![e3.clone(), e2.clone()]);
        let q = HistoryQuery {
            profile_id: Some(a),
            limit: 1,
            ..HistoryQuery::default()
        };
        assert_eq!(repo.list(&q).await.unwrap(), vec![e2.clone()]);
        let q = HistoryQuery {
            before: Some(e2.started_at),
            ..HistoryQuery::default()
        };
        assert_eq!(repo.list(&q).await.unwrap(), vec![e1.clone()]);
        let q = HistoryQuery {
            limit: 0,
            ..HistoryQuery::default()
        };
        assert_eq!(q.effective_limit(), 1);

        assert_eq!(repo.delete(&[e1.id, Uuid::now_v7()]).await.unwrap(), 1);
        assert_eq!(repo.clear(Some(b)).await.unwrap(), 1);
        assert_eq!(repo.list(&HistoryQuery::default()).await.unwrap(), vec![e2]);
        assert_eq!(repo.clear(None).await.unwrap(), 1);
        assert_eq!(
            repo.list(&HistoryQuery::default()).await.unwrap(),
            Vec::<HistoryEntry>::new()
        );
    }

    #[tokio::test]
    async fn prune_keeps_the_newest() {
        let repo = MemoryHistoryRepository::new();
        let p = Uuid::now_v7();
        for i in 0..5 {
            repo.append(&entry(p, i, &format!("SELECT {i}")))
                .await
                .unwrap();
        }
        assert_eq!(repo.prune(0).await.unwrap(), 0);
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

    #[tokio::test]
    async fn settings_default_and_persist() {
        let repo = MemoryHistoryRepository::new();
        assert_eq!(repo.settings().await.unwrap(), HistorySettings::default());
        let off = HistorySettings {
            enabled: false,
            max_entries: 10,
        };
        repo.save_settings(&off).await.unwrap();
        assert_eq!(repo.settings().await.unwrap(), off);
    }

    #[test]
    fn serde_shapes() {
        let e = entry(Uuid::nil(), 0, "SELECT 1");
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["profileId"], Uuid::nil().to_string());
        assert_eq!(json["status"], "ok");
        assert_eq!(json["durationMs"], 3);
        assert!(json.get("error").is_none());
        let q: HistoryQuery = serde_json::from_str(r#"{"search":"x"}"#).unwrap();
        assert_eq!(q.limit, HistoryQuery::DEFAULT_LIMIT);
        let s: HistorySettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, HistorySettings::default());
        assert_eq!(HistoryStatus::Cancelled.as_str(), "cancelled");
    }
}
