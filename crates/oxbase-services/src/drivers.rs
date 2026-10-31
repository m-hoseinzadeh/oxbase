//! The driver registry used by the app and the CLI.

use std::sync::Arc;

use oxbase_driver::DriverRegistry;
use oxbase_types::Engine;

/// A registry with every driver this build ships: PostgreSQL, MySQL, MariaDB (served by the
/// MySQL driver) and SQLite.
pub fn default_drivers() -> DriverRegistry {
    let mut drivers = DriverRegistry::new();
    drivers.register(Arc::new(oxbase_pg::PgDriver::new()));
    let mysql = Arc::new(oxbase_mysql::MySqlDriver::new());
    drivers.register_as(Engine::MySql, mysql.clone());
    drivers.register_as(Engine::MariaDb, mysql);
    drivers.register(Arc::new(oxbase_sqlite::SqliteDriver));
    drivers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_drivers_cover_the_m1_engines() {
        let drivers = default_drivers();
        assert_eq!(
            drivers.engines(),
            vec![
                Engine::Postgres,
                Engine::MySql,
                Engine::MariaDb,
                Engine::Sqlite
            ]
        );
    }
}
