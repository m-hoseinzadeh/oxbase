//! Query history (M2): what `QueryService` records after every executed statement, and the
//! listing/deleting the UI does.
//!
//! [`HistoryService::record`] is the only way entries are written:
//!
//! 1. If [`HistorySettings::enabled`] is off, nothing is written (`Ok(None)`).
//! 2. The SQL text and the error message are passed through `oxbase_sql::redact_secrets` for the
//!    statement's dialect (password literals become `'***'`), then cut to
//!    [`MAX_SQL_BYTES`] / [`MAX_ERROR_BYTES`] at a character boundary (with a trailing `…`).
//! 3. The entry gets a UUID v7 id and is appended; every [`PRUNE_EVERY`] appends the repository
//!    is pruned to [`HistorySettings::max_entries`].
//!
//! Recording failures never fail the query run: `QueryService` logs them (without SQL text)
//! and goes on.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use oxbase_sql::{SqlDialect, redact_secrets};
use oxbase_store::{HistoryEntry, HistoryQuery, HistoryRepository, HistorySettings, HistoryStatus};
use oxbase_types::Result;

/// Longest SQL text stored per entry, in bytes.
pub const MAX_SQL_BYTES: usize = 64 * 1024;
/// Longest error message stored per entry, in bytes.
pub const MAX_ERROR_BYTES: usize = 4 * 1024;
/// Appends between two prunes.
pub const PRUNE_EVERY: u32 = 64;
/// Appended by [`truncate`] when it cuts text.
const ELLIPSIS: &str = "…";

/// One executed statement, before redaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRecord {
    pub profile_id: Uuid,
    pub database: Option<String>,
    /// Dialect used to find the literals to redact.
    pub dialect: SqlDialect,
    /// The statement text as it ran (not yet redacted).
    pub sql: String,
    pub started_at: DateTime<Utc>,
    pub duration: Duration,
    pub status: HistoryStatus,
    pub rows: Option<u64>,
    /// Error message for `HistoryStatus::Error` (not yet redacted).
    pub error: Option<String>,
}

/// Records and lists query history. See the module docs.
#[derive(Debug)]
pub struct HistoryService {
    repo: Arc<dyn HistoryRepository>,
    appends: AtomicU32,
}

impl HistoryService {
    pub fn new(repo: Arc<dyn HistoryRepository>) -> Self {
        Self {
            repo,
            appends: AtomicU32::new(0),
        }
    }

    pub fn repository(&self) -> &Arc<dyn HistoryRepository> {
        &self.repo
    }

    /// Stores `record` (redacted) unless history is off. Returns the stored entry.
    pub async fn record(&self, record: HistoryRecord) -> Result<Option<HistoryEntry>> {
        let settings = self.repo.settings().await?;
        if !settings.enabled {
            return Ok(None);
        }
        let sql = truncate(&redact_secrets(record.dialect, &record.sql), MAX_SQL_BYTES);
        let error = record
            .error
            .as_deref()
            .map(|e| truncate(&redact_secrets(record.dialect, e), MAX_ERROR_BYTES));
        let entry = HistoryEntry {
            id: Uuid::now_v7(),
            profile_id: record.profile_id,
            database: record.database,
            sql,
            started_at: record.started_at,
            duration_ms: u64::try_from(record.duration.as_millis()).unwrap_or(u64::MAX),
            status: record.status,
            rows: record.rows,
            error,
        };
        self.repo.append(&entry).await?;
        let n = self.appends.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(PRUNE_EVERY) && settings.max_entries > 0 {
            self.repo.prune(settings.max_entries).await?;
        }
        Ok(Some(entry))
    }

    /// Newest first; see [`HistoryQuery`].
    pub async fn list(&self, query: &HistoryQuery) -> Result<Vec<HistoryEntry>> {
        self.repo.list(query).await
    }

    pub async fn delete(&self, ids: &[Uuid]) -> Result<u64> {
        self.repo.delete(ids).await
    }

    /// Removes every entry, or only those of `profile_id`.
    pub async fn clear(&self, profile_id: Option<Uuid>) -> Result<u64> {
        self.repo.clear(profile_id).await
    }

    pub async fn settings(&self) -> Result<HistorySettings> {
        self.repo.settings().await
    }

    /// Saves the settings. Lowering `max_entries` prunes right away.
    pub async fn set_settings(&self, settings: &HistorySettings) -> Result<()> {
        self.repo.save_settings(settings).await?;
        if settings.max_entries > 0 {
            self.repo.prune(settings.max_entries).await?;
        }
        Ok(())
    }
}

/// `text` cut to at most `max` bytes at a character boundary, with `…` appended when cut.
pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let budget = max.saturating_sub(ELLIPSIS.len());
    let end = (0..=budget)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    format!("{}{ELLIPSIS}", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_store::MemoryHistoryRepository;

    #[test]
    fn truncates_at_char_boundaries() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcdef", 5), "ab…");
        // 'é' is 2 bytes: never split it.
        let t = truncate("éééé", 6);
        assert_eq!(t, "é…");
        assert!(t.len() <= 6);
        assert_eq!(truncate("abc", 0), "…");
    }

    #[tokio::test]
    async fn disabled_history_records_nothing() {
        let repo = Arc::new(MemoryHistoryRepository::new());
        let service = HistoryService::new(repo);
        service
            .set_settings(&HistorySettings {
                enabled: false,
                max_entries: 5,
            })
            .await
            .unwrap();
        let stored = service
            .record(HistoryRecord {
                profile_id: Uuid::nil(),
                database: None,
                dialect: SqlDialect::Postgres,
                sql: "ALTER USER x PASSWORD 'marker-secret'".into(),
                started_at: Utc::now(),
                duration: Duration::from_millis(1),
                status: HistoryStatus::Ok,
                rows: None,
                error: None,
            })
            .await
            .unwrap();
        assert_eq!(stored, None);
        assert_eq!(
            service.list(&HistoryQuery::default()).await.unwrap(),
            Vec::<HistoryEntry>::new()
        );
        assert!(!service.settings().await.unwrap().enabled);
    }
}
