//! Which syntax each SQL dialect accepts, for the dialect-mismatch findings and for the few
//! places where the parser must read a construct differently (T-SQL batches that hold several
//! statements, MySQL `@variables`).
//!
//! This is data, one row per lexing dialect, kept next to the code that reads it until the
//! per-engine `SqlLanguage` data grows feature flags; no insight code tests the engine itself.

use oxbase_sql::SqlDialect;

/// The syntax features of one dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Features {
    /// The product name used in messages.
    pub name: &'static str,
    /// `LIMIT n` ends a query.
    pub limit: bool,
    /// `LIMIT offset, count`.
    pub limit_comma: bool,
    /// `SELECT TOP n`.
    pub top: bool,
    /// `a ILIKE b`.
    pub ilike: bool,
    /// `expr::type` casts (T-SQL: `type::method()` static calls).
    pub double_colon: bool,
    /// `` `name` `` quoted identifiers.
    pub backticks: bool,
    /// `@name` / `@@name` variables lexed as `@` and a word.
    pub at_params: bool,
    /// The MySQL client's `DELIMITER` command is understood (by the splitter).
    pub delimiter: bool,
    /// The `GO` batch separator is understood (by the splitter).
    pub go: bool,
    /// One statement of the splitter is a batch that may hold several SQL statements without
    /// `;` between them, and a routine body starts after `AS`.
    pub batches: bool,
    /// `INSERT / UPDATE / DELETE ... RETURNING`.
    pub returning: bool,
}

const POSTGRES: Features = Features {
    name: "PostgreSQL",
    limit: true,
    limit_comma: false,
    top: false,
    ilike: true,
    double_colon: true,
    backticks: false,
    at_params: false,
    delimiter: false,
    go: false,
    batches: false,
    returning: true,
};

const MYSQL: Features = Features {
    name: "MySQL",
    limit: true,
    limit_comma: true,
    top: false,
    ilike: false,
    double_colon: false,
    backticks: true,
    at_params: true,
    delimiter: true,
    go: false,
    batches: false,
    returning: false,
};

const MARIADB: Features = Features {
    name: "MariaDB",
    returning: true,
    ..MYSQL
};

const SQLITE: Features = Features {
    name: "SQLite",
    limit: true,
    limit_comma: true,
    top: false,
    ilike: false,
    double_colon: false,
    backticks: true,
    at_params: true,
    delimiter: false,
    go: false,
    batches: false,
    returning: true,
};

const TSQL: Features = Features {
    name: "SQL Server",
    limit: false,
    limit_comma: false,
    top: true,
    ilike: false,
    double_colon: true,
    backticks: false,
    at_params: false,
    delimiter: false,
    go: true,
    batches: true,
    returning: false,
};

/// The features of `sql`.
pub(crate) const fn features(sql: SqlDialect) -> Features {
    match sql {
        SqlDialect::Postgres => POSTGRES,
        SqlDialect::MySql { mariadb: false } => MYSQL,
        SqlDialect::MySql { mariadb: true } => MARIADB,
        SqlDialect::Sqlite => SQLITE,
        SqlDialect::TSql => TSQL,
    }
}
