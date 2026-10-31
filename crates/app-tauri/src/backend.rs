//! Picks the core implementations the desktop app runs with: drivers, the secret store, the
//! profile repository and the SSH tunnel provider. Construction only; no behaviour.

use std::path::Path;
use std::sync::Arc;

use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_services::{ConnectionManager, DriverRegistry};
use oxbase_store::{
    FileProfileRepository, KeyringSecretStore, MemoryProfileRepository, ProfileRepository,
};

use crate::connections::ConnectionsState;

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

/// Builds the state behind the connection commands.
pub fn connections_state(fallback_dir: Option<&Path>) -> ConnectionsState {
    let manager = ConnectionManager::new(
        driver_registry(),
        Arc::new(KeyringSecretStore::new()),
        Arc::new(RusshTunnelProvider::new()),
    );
    ConnectionsState::new(Arc::new(manager), profile_repository(fallback_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::Engine;

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
