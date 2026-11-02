//! M6: the per-engine [`CatalogReader`] role, which reads a [`CatalogSnapshot`] for the schema
//! cache in a small, fixed number of catalog queries (`docs/m6-contracts.md` §3.2).
//!
//! The M2 [`Fetcher`](crate::Fetcher)s list one parent at a time (columns per table), which is
//! one round trip per table: 5 000 tables would take minutes over a slow link. A catalog reader
//! instead reads every column of a database with one query (`information_schema.columns`,
//! `pg_attribute`, `sys.columns`, SQLite's `pragma_table_info` joined to `sqlite_schema`).
//!
//! Like [`BulkLoader`](crate::BulkLoader) it is an engine-wide role outside
//! [`EngineRole::ALL`](crate::EngineRole::ALL), so the conformance matrix does not change.

use async_trait::async_trait;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{CatalogSnapshot, Result};

/// A table or view name inside a [`CatalogScope`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelationName {
    /// `None` on engines without schemas (MySQL, MariaDB: the database's own schema).
    pub schema: Option<String>,
    pub name: String,
}

/// What a [`CatalogReader`] reads. The default reads everything (a full build).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogScope {
    /// Only these schemas (whole: relations, columns, routines, sequences, types).
    pub schemas: Option<Vec<String>>,
    /// Only these relations (with their columns and foreign keys; nothing else). Names are
    /// exact catalog names; a name that does not exist is simply absent from the result.
    /// When both `schemas` and `relations` are set, the reader returns the union.
    pub relations: Option<Vec<RelationName>>,
    /// Include system schemas (`pg_catalog`, `information_schema`, `sys`, MySQL `mysql` /
    /// `performance_schema` / `sys`). Off by default: completion offers them only when the
    /// user qualifies a name with one, and the cache then reads that schema on demand.
    pub include_system: bool,
    /// Stop after this many columns and set [`CatalogSnapshot::truncated`] (memory guard,
    /// `docs/m6-contracts.md` §3.4).
    pub max_columns: usize,
}

impl CatalogScope {
    /// Column limit of [`CatalogScope::full`]: about four times the 5 000-table x 20-column
    /// budget of the exit check.
    pub const DEFAULT_MAX_COLUMNS: usize = 400_000;

    /// Everything outside the system schemas.
    pub fn full() -> Self {
        Self {
            schemas: None,
            relations: None,
            include_system: false,
            max_columns: Self::DEFAULT_MAX_COLUMNS,
        }
    }

    /// `true` when this scope reads the whole database (no schema or relation filter).
    pub fn is_full(&self) -> bool {
        self.schemas.is_none() && self.relations.is_none()
    }
}

impl Default for CatalogScope {
    fn default() -> Self {
        Self::full()
    }
}

/// Reads the names of the connection's current database. One implementation per engine,
/// registered with [`DialectRegistry::register_catalog_reader`](crate::DialectRegistry::register_catalog_reader).
///
/// # Rules for implementations (cache stream)
///
/// - **Few round trips.** A full read makes a fixed number of queries independent of the
///   number of tables (one each for schemas, relations, columns with key flags, foreign keys,
///   routines, sequences, types, settings; or fewer). Never one query per table.
/// - **Catalog queries only, bound parameters** for every name from `scope` (as for
///   [`Fetcher`](crate::Fetcher)); version-aware queries.
/// - **What it fills.** [`CatalogSnapshot::new`] with the engine, server version and current
///   database; `databases` (other databases the login can see; MySQL / MariaDB / SQL Server /
///   PostgreSQL; SQLite: attached database names); `default_schema` and `search_path`
///   (PostgreSQL `current_schemas(false)`, SQL Server `SCHEMA_NAME()`, MySQL the database,
///   SQLite `main`); `case_rules` from server settings (MySQL / MariaDB
///   `@@lower_case_table_names`: 0 = `Sensitive` table and database names, 1 or 2 =
///   `Insensitive`; SQL Server the database collation: `_CS_` = `Sensitive`, else
///   `Insensitive`); relations of kind table, view, materialized view and foreign table with
///   columns in ordinal order, primary-key flags and foreign keys; routines with display
///   arguments and return types; sequences; user-defined types; `read_at_ms`; then
///   [`CatalogSnapshot::normalize`].
/// - **Scope.** A partial scope returns a snapshot holding only the requested schemas and
///   relations (other fields filled as for a full read); the cache merges it.
/// - **Limits.** Stop adding columns after `scope.max_columns` and set `truncated`.
/// - **Cancel.** Pass `cancel` to every `execute`; once cancelled return `Error::Cancelled`
///   and no partial snapshot.
#[async_trait]
pub trait CatalogReader: Send + Sync {
    async fn read(
        &self,
        c: &dyn Connection,
        scope: &CatalogScope,
        cancel: CancellationToken,
    ) -> Result<CatalogSnapshot>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes() {
        let full = CatalogScope::default();
        assert!(full.is_full());
        assert!(!full.include_system);
        assert_eq!(full.max_columns, CatalogScope::DEFAULT_MAX_COLUMNS);
        let part = CatalogScope {
            relations: Some(vec![RelationName {
                schema: Some("public".into()),
                name: "t".into(),
            }]),
            ..CatalogScope::full()
        };
        assert!(!part.is_full());
    }
}
