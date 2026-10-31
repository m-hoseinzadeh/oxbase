//! Supported database engines.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::Error;

/// A database product oxbase can talk to.
///
/// MariaDB is its own engine even though it shares the MySQL driver: dialect cells and
/// capabilities differ, and keeping them apart makes the object-kind x engine matrix honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Postgres,
    MySql,
    MariaDb,
    Sqlite,
    MsSql,
    MongoDb,
    Redis,
}

impl Engine {
    /// Every engine, in a stable order (used for matrices and reports).
    pub const ALL: [Engine; 7] = [
        Engine::Postgres,
        Engine::MySql,
        Engine::MariaDb,
        Engine::Sqlite,
        Engine::MsSql,
        Engine::MongoDb,
        Engine::Redis,
    ];

    /// Stable lowercase identifier (also the serde form).
    pub const fn as_str(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::MySql => "mysql",
            Engine::MariaDb => "mariadb",
            Engine::Sqlite => "sqlite",
            Engine::MsSql => "mssql",
            Engine::MongoDb => "mongodb",
            Engine::Redis => "redis",
        }
    }

    /// Human-readable product name.
    pub const fn display_name(self) -> &'static str {
        match self {
            Engine::Postgres => "PostgreSQL",
            Engine::MySql => "MySQL",
            Engine::MariaDb => "MariaDB",
            Engine::Sqlite => "SQLite",
            Engine::MsSql => "SQL Server",
            Engine::MongoDb => "MongoDB",
            Engine::Redis => "Redis",
        }
    }

    /// `true` for engines queried with SQL (everything except MongoDB and Redis).
    pub const fn is_sql(self) -> bool {
        !matches!(self, Engine::MongoDb | Engine::Redis)
    }

    /// `true` for MySQL and MariaDB, which share a wire protocol and driver.
    pub const fn is_mysql_family(self) -> bool {
        matches!(self, Engine::MySql | Engine::MariaDb)
    }

    /// `true` when the engine is a network server (SQLite is an embedded file).
    pub const fn is_networked(self) -> bool {
        !matches!(self, Engine::Sqlite)
    }

    /// The engine's well-known default TCP port, if it has one.
    pub const fn default_port(self) -> Option<u16> {
        match self {
            Engine::Postgres => Some(5432),
            Engine::MySql | Engine::MariaDb => Some(3306),
            Engine::Sqlite => None,
            Engine::MsSql => Some(1433),
            Engine::MongoDb => Some(27017),
            Engine::Redis => Some(6379),
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Engine {
    type Err = Error;

    /// Accepts the identifier from [`Engine::as_str`] plus common aliases, case-insensitively.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.trim().to_ascii_lowercase();
        Ok(match lower.as_str() {
            "postgres" | "postgresql" | "pg" => Engine::Postgres,
            "mysql" => Engine::MySql,
            "mariadb" => Engine::MariaDb,
            "sqlite" | "sqlite3" => Engine::Sqlite,
            "mssql" | "sqlserver" | "sql server" => Engine::MsSql,
            "mongodb" | "mongo" => Engine::MongoDb,
            "redis" => Engine::Redis,
            _ => return Err(Error::invalid_input(format!("unknown engine '{s}'"))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_names() {
        for e in Engine::ALL {
            assert_eq!(e.as_str().parse::<Engine>().unwrap(), e);
            let json = serde_json::to_string(&e).unwrap();
            assert_eq!(json, format!("\"{}\"", e.as_str()));
            assert_eq!(serde_json::from_str::<Engine>(&json).unwrap(), e);
        }
    }

    #[test]
    fn aliases_and_errors() {
        assert_eq!("PostgreSQL".parse::<Engine>().unwrap(), Engine::Postgres);
        assert_eq!(" mongo ".parse::<Engine>().unwrap(), Engine::MongoDb);
        assert!("oracle".parse::<Engine>().is_err());
    }

    #[test]
    fn traits() {
        assert!(Engine::Sqlite.is_sql());
        assert!(!Engine::Redis.is_sql());
        assert!(Engine::MariaDb.is_mysql_family());
        assert_eq!(Engine::Sqlite.default_port(), None);
        assert_eq!(Engine::MsSql.default_port(), Some(1433));
    }
}
