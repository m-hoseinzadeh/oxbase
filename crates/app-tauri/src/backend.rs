//! Picks the core implementations the desktop app runs with: drivers, dialects, the secret
//! store, the profile repository, the SSH tunnel provider, (M2) the tree, query and history
//! services, (M3) the edit and designer services, (M5) the import and export services and
//! the file-handle registry, and (M6) the schema-cache, insight and snippet services.
//! Construction only; no behaviour.

use std::path::Path;
use std::sync::Arc;

use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_services::designer::DesignerService;
use oxbase_services::edit::EditService;
use oxbase_services::export::{ExportConfig, ExportService};
use oxbase_services::history::HistoryService;
use oxbase_services::import::{ImportConfig, ImportService};
use oxbase_services::insight::{InsightConfig, InsightService};
use oxbase_services::query::{QueryConfig, QueryService};
use oxbase_services::schema_cache::{SchemaCacheConfig, SchemaCacheService, SchemaEvents};
use oxbase_services::snippets::SnippetService;
use oxbase_services::tree::TreeService;
use oxbase_services::{ConnectionManager, DriverRegistry};
use oxbase_store::{
    AppDb, FileProfileRepository, HistoryRepository, KeyringSecretStore, MemoryHistoryRepository,
    MemoryProfileRepository, MemorySchemaCacheRepository, MemorySnippetRepository,
    ProfileRepository, SchemaCacheRepository, SnippetRepository, SqliteHistoryRepository,
    SqliteSchemaCacheRepository, SqliteSnippetRepository,
};

use crate::connections::ConnectionsState;
use crate::data::DataState;
use crate::designer::DesignerState;
use crate::files::FilesState;
use crate::insight::InsightState;
use crate::transfer::TransferState;
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
    /// M5: import and export.
    pub transfer: TransferState,
    /// M5: file handles from the native dialogs.
    pub files: FilesState,
    /// M6: code insight, schema caches and snippets.
    pub insight: InsightState,
}

/// The repositories kept in the app database.
pub struct AppRepositories {
    pub history: Arc<dyn HistoryRepository>,
    /// M6: user snippets.
    pub snippets: Arc<dyn SnippetRepository>,
    /// M6: schema-cache snapshots.
    pub schema_caches: Arc<dyn SchemaCacheRepository>,
}

impl AppRepositories {
    /// Everything in memory except `history` (tests).
    pub fn in_memory(history: Arc<dyn HistoryRepository>) -> Self {
        Self {
            history,
            snippets: Arc::new(MemorySnippetRepository::new()),
            schema_caches: Arc::new(MemorySchemaCacheRepository::new()),
        }
    }
}

/// The app database's repositories (`oxbase.db` in `oxbase_store::app_data_dir`, which
/// `OXBASE_DATA_DIR` overrides), opened once. When the database cannot be opened, everything
/// is kept in memory for this run, so the app still starts.
///
/// Call it outside an async runtime (it blocks on opening the database), as Tauri's `setup` is.
pub fn app_repositories() -> AppRepositories {
    match tauri::async_runtime::block_on(AppDb::open_default()) {
        Ok(db) => AppRepositories {
            history: Arc::new(SqliteHistoryRepository::new(db.clone())),
            snippets: Arc::new(SqliteSnippetRepository::new(db.clone())),
            schema_caches: Arc::new(SqliteSchemaCacheRepository::new(db)),
        },
        Err(error) => {
            // Only the error's category: the message could carry a path.
            eprintln!(
                "oxbase: warning: the app database could not be opened ({}); query history, \
                 snippets and schema caches are kept in memory for this run only",
                error.category()
            );
            AppRepositories::in_memory(Arc::new(MemoryHistoryRepository::new()))
        }
    }
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

/// Builds the state behind every command, with history, snippets and schema caches in the
/// app database ([`app_repositories`]).
pub fn app_state(fallback_dir: Option<&Path>) -> AppStates {
    app_state_with_repositories(fallback_dir, app_repositories())
}

/// [`app_state`] with the given history repository and everything else in memory (tests).
#[cfg(test)]
pub fn app_state_with(
    fallback_dir: Option<&Path>,
    history: Arc<dyn HistoryRepository>,
) -> AppStates {
    app_state_with_repositories(fallback_dir, AppRepositories::in_memory(history))
}

/// [`app_state`] with the given repositories.
pub fn app_state_with_repositories(
    fallback_dir: Option<&Path>,
    repositories: AppRepositories,
) -> AppStates {
    let AppRepositories {
        history,
        snippets,
        schema_caches,
    } = repositories;
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
        Arc::clone(&dialects),
    ));
    let import = Arc::new(ImportService::new(
        Arc::clone(&manager),
        Arc::clone(&tree),
        Arc::clone(&dialects),
        ImportConfig::default(),
    ));
    let export = Arc::new(ExportService::new(
        Arc::clone(&manager),
        Arc::clone(&query),
        Arc::clone(&tree),
        Arc::clone(&dialects),
        ExportConfig::default(),
    ));
    // M6: the designer, import and query services publish on `events` once the cache stream
    // wires them (docs/m6-contracts.md §3.5).
    let events = SchemaEvents::new();
    let caches = Arc::new(SchemaCacheService::new(
        Arc::clone(&manager),
        Arc::clone(&dialects),
        schema_caches,
        events,
        SchemaCacheConfig::default(),
    ));
    let insight = Arc::new(InsightService::new(
        dialects,
        caches,
        Arc::new(SnippetService::new(snippets)),
        InsightConfig::default(),
    ));
    AppStates {
        connections: ConnectionsState::new(manager, profile_repository(fallback_dir)),
        workbench: WorkbenchState::new(tree, query, history),
        data: DataState::new(edit),
        designer: DesignerState::new(designer),
        transfer: TransferState::new(import, export),
        files: FilesState::new(),
        insight: InsightState::new(insight),
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
        // M5: import and export share them too, and no file handle exists yet.
        assert!(Arc::ptr_eq(
            states.transfer.import().manager(),
            states.connections.manager()
        ));
        assert!(Arc::ptr_eq(
            states.transfer.export().dialects(),
            states.designer.service().dialects()
        ));
        assert!(Arc::ptr_eq(
            states.transfer.export().query(),
            states.data.service().query()
        ));
        assert_eq!(states.files.handle_count(), 0);
        // M6: insight shares the manager and the registry too.
        assert!(Arc::ptr_eq(
            states.insight.service().caches().manager(),
            states.connections.manager()
        ));
        assert!(Arc::ptr_eq(
            states.insight.service().dialects(),
            states.designer.service().dialects()
        ));
    }

    #[test]
    fn ships_the_m1_and_m4_drivers() {
        assert_eq!(
            driver_registry().engines(),
            vec![
                Engine::Postgres,
                Engine::MySql,
                Engine::MariaDb,
                Engine::Sqlite,
                Engine::MsSql
            ]
        );
    }
}
