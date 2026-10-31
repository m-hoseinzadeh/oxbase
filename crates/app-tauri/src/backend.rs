//! Picks the core implementations the desktop app runs with: drivers, dialects, the secret
//! store, the profile repository, the SSH tunnel provider, and (M2) the tree, query and history
//! services. Construction only; no behaviour.

use std::path::Path;
use std::sync::Arc;

use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_services::history::HistoryService;
use oxbase_services::query::{QueryConfig, QueryService};
use oxbase_services::tree::TreeService;
use oxbase_services::{ConnectionManager, DriverRegistry};
use oxbase_store::{
    AppDb, FileProfileRepository, HistoryRepository, KeyringSecretStore, MemoryHistoryRepository,
    MemoryProfileRepository, ProfileRepository, SqliteHistoryRepository,
};

use crate::connections::ConnectionsState;
use crate::workbench::WorkbenchState;

/// The drivers the app ships with: the shared registry from `oxbase-services` (PostgreSQL,
/// MySQL, MariaDB and SQLite), the same one `oxbase-cli` uses.
pub fn driver_registry() -> DriverRegistry {
    oxbase_services::default_drivers()
}

/// Where saved profiles live: `connections.json` in the shared oxbase config directory (the
/// same file `oxbase-cli` reads), else in the Tauri app data directory, else in memory.
pub fn profile_repository(fallback_dir: Option<&Path>) -> Arc<dyn ProfileRepository> {
    if let Ok(repo) = FileProfileRepository::open_default() {
        return Arc::new(repo);
    }
    match fallback_dir {
        Some(dir) => Arc::new(FileProfileRepository::in_dir(dir)),
        None => Arc::new(MemoryProfileRepository::new()),
    }
}

/// Where query history lives: the app database (`oxbase.db` in `oxbase_store::app_data_dir`,
/// which `OXBASE_DATA_DIR` overrides), the same file `oxbase-cli` uses. When it cannot be opened
/// (read-only disk, a file locked or damaged) history is kept in memory for this run, so the app
/// still starts.
///
/// Call it outside an async runtime (it blocks on opening the database), as Tauri's `setup` is.
pub fn history_repository() -> Arc<dyn HistoryRepository> {
    match tauri::async_runtime::block_on(AppDb::open_default()) {
        Ok(db) => Arc::new(SqliteHistoryRepository::new(db)),
        Err(_) => Arc::new(MemoryHistoryRepository::new()),
    }
}

/// Builds the state behind the connection commands and the M2 workbench commands, with history
/// in the app database ([`history_repository`]).
pub fn app_state(fallback_dir: Option<&Path>) -> (ConnectionsState, WorkbenchState) {
    app_state_with(fallback_dir, history_repository())
}

/// [`app_state`] with the given history repository. Both states share one
/// `ConnectionManager`.
pub fn app_state_with(
    fallback_dir: Option<&Path>,
    history: Arc<dyn HistoryRepository>,
) -> (ConnectionsState, WorkbenchState) {
    let manager = Arc::new(ConnectionManager::new(
        driver_registry(),
        Arc::new(KeyringSecretStore::new()),
        Arc::new(RusshTunnelProvider::new()),
    ));
    let dialects = Arc::new(oxbase_services::default_dialects());
    let history = Arc::new(HistoryService::new(history));
    let tree = Arc::new(TreeService::new(
        Arc::clone(&manager),
        Arc::clone(&dialects),
    ));
    let query = Arc::new(QueryService::new(
        Arc::clone(&manager),
        dialects,
        Arc::clone(&history),
        QueryConfig::default(),
    ));
    (
        ConnectionsState::new(manager, profile_repository(fallback_dir)),
        WorkbenchState::new(tree, query, history),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::Engine;

    #[test]
    fn builds_the_app_state() {
        // In memory: tests never touch the user's app database.
        let (connections, _workbench) =
            app_state_with(None, Arc::new(MemoryHistoryRepository::new()));
        assert_eq!(
            connections.manager().list_open(),
            Vec::<oxbase_services::SessionSummary>::new()
        );
    }

    #[test]
    fn ships_the_m1_drivers() {
        assert_eq!(
            driver_registry().engines(),
            vec![
                Engine::Postgres,
                Engine::MySql,
                Engine::MariaDb,
                Engine::Sqlite
            ]
        );
    }
}
