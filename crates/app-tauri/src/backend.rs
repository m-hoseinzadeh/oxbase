//! Picks the core implementations the desktop app runs with: drivers, dialects, the secret
//! store, the profile repository, the SSH tunnel provider, (M2) the tree, query and history
//! services, and (M3) the edit and designer services. Construction only; no behaviour.

use std::path::Path;
use std::sync::Arc;

use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_services::designer::DesignerService;
use oxbase_services::edit::EditService;
use oxbase_services::history::HistoryService;
use oxbase_services::query::{QueryConfig, QueryService};
use oxbase_services::tree::TreeService;
use oxbase_services::{ConnectionManager, DriverRegistry};
use oxbase_store::{
    AppDb, FileProfileRepository, HistoryRepository, KeyringSecretStore, MemoryHistoryRepository,
    MemoryProfileRepository, ProfileRepository, SqliteHistoryRepository,
};

use crate::connections::ConnectionsState;
use crate::data::DataState;
use crate::designer::DesignerState;
use crate::workbench::WorkbenchState;

/// Every piece of Tauri state the commands use. All share one `ConnectionManager` and one
/// dialect registry.
pub struct AppStates {
    pub connections: ConnectionsState,
    pub workbench: WorkbenchState,
    /// M3: table data and grid editing.
    pub data: DataState,
    /// M3: the table designer.
    pub designer: DesignerState,
}

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
        Err(error) => {
            // Only the error's category: the message could carry a path.
            eprintln!(
                "oxbase: warning: the app database could not be opened ({}); query history is \
                 kept in memory for this run only",
                error.category()
            );
            Arc::new(MemoryHistoryRepository::new())
        }
    }
}

/// Builds the state behind every command, with history in the app database
/// ([`history_repository`]).
pub fn app_state(fallback_dir: Option<&Path>) -> AppStates {
    app_state_with(fallback_dir, history_repository())
}

/// [`app_state`] with the given history repository.
pub fn app_state_with(
    fallback_dir: Option<&Path>,
    history: Arc<dyn HistoryRepository>,
) -> AppStates {
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
        Arc::clone(&dialects),
        Arc::clone(&history),
        QueryConfig::default(),
    ));
    let edit = Arc::new(EditService::new(
        Arc::clone(&query),
        Arc::clone(&tree),
        Arc::clone(&dialects),
    ));
    let designer = Arc::new(DesignerService::new(
        Arc::clone(&manager),
        Arc::clone(&tree),
        dialects,
    ));
    AppStates {
        connections: ConnectionsState::new(manager, profile_repository(fallback_dir)),
        workbench: WorkbenchState::new(tree, query, history),
        data: DataState::new(edit),
        designer: DesignerState::new(designer),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::Engine;

    #[test]
    fn builds_the_app_state() {
        // In memory: tests never touch the user's app database.
        let states = app_state_with(None, Arc::new(MemoryHistoryRepository::new()));
        assert_eq!(
            states.connections.manager().list_open(),
            Vec::<oxbase_services::SessionSummary>::new()
        );
        // The M3 services share the manager and the registry.
        assert!(Arc::ptr_eq(
            states.designer.service().manager(),
            states.connections.manager()
        ));
        assert!(Arc::ptr_eq(
            states.data.service().dialects(),
            states.designer.service().dialects()
        ));
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
