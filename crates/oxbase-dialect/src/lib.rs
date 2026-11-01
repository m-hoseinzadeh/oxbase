//! Per-engine dialect traits: [`Fetcher`], [`DdlGenerator`], [`Comparator`], [`Capabilities`],
//! [`DmlWriter`] and [`Quoting`].
//!
//! Each engine fills an object-kind x role matrix of implementations of these traits (PLAN §1,
//! rule 3). [`DialectRegistry`] holds that matrix and reports which cells are still missing, so
//! the conformance harness can print the object-kind x engine grid.
//!
//! Roles per (engine, object kind): [`Fetcher`], [`DdlGenerator`], [`Comparator`].
//! Roles per engine: [`Capabilities`], [`Quoting`], [`DmlWriter`].
//! Data per engine (M2): the object-tree [`TreeLayout`] and its [`DatabaseScope`].
//!
//! M3 adds the shared machinery the designer, the grid editor and the filter builder use:
//!
//! - [`sql`]: [`sql::SqlWriter`] (SQL with bound parameters in the engine's placeholder style),
//!   [`sql::check_expression`] (user-typed SQL is one expression), the table-data query
//!   [`sql::select_table_data`] with filter and sort rendering;
//! - [`compare`]: the structural table comparison every engine's [`Comparator`] cells run
//!   ([`compare::RulesComparator`] + the engine's [`compare::CompareRules`]), with fixed
//!   property ids and order;
//! - [`DdlContext`] for [`DdlGenerator`], [`DmlTarget`] / [`DmlPlan`] for [`DmlWriter`],
//!   [`DesignerCapabilities`] from [`Capabilities::designer`].
//!
//! M4 adds SQL Server: [`BracketQuoting`] qualifies names without the database (SQL Server
//! reads one database per session, [`DatabaseScope::Session`]), limits a `SELECT` with
//! [`Quoting::top_clause`] (`TOP (n)`), and [`LexRules::bracket_identifiers`] makes
//! [`sql::check_expression`] read `[...]` identifiers (also on for SQLite's [`AnsiQuoting`]).
//!
//! M5 adds bulk loading for import: the per-engine [`BulkLoader`] role (module [`bulk`]: load
//! batches in a fixed Arrow schema, batch-size rules per engine, the generic multi-row
//! [`InsertLoader`] every SQL engine falls back to), registered with
//! [`DialectRegistry::register_bulk_loader`]. The SQL dump rules are [`Quoting`] methods
//! (module [`dump`]: session statements, literals the target reads back, identity inserts), and
//! [`DdlContext::exact_values`] asks for column types that keep imported values exactly.
//!
//! M6 adds code insight data: the per-engine [`CatalogReader`] role (module [`catalog`]: read
//! every name of a database in a few queries for the schema cache) and the per-engine
//! [`SqlLanguage`] data (module [`language`]: keywords, reserved words, built-in functions,
//! data types, identifier characters), both registered next to the other engine-wide roles.
//!
//! How engine crates register their cells: see [`DialectRegistry`] (*Registration*). The
//! contracts are in `docs/m2-contracts.md`, `docs/m3-contracts.md`, `docs/m4-contracts.md` and
//! `docs/m5-contracts.md` and `docs/m6-contracts.md`.

pub mod bulk;
mod capability;
pub mod catalog;
pub mod compare;
mod ddl;
mod dml;
pub mod dump;
pub mod language;
mod quoting;
mod registry;
pub mod sql;
mod tree;

use async_trait::async_trait;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    ObjectDef, ObjectKind, ObjectRef, ObjectSummary, PropId, PropertyDiff, Result, RowEdit, Script,
    ServerVersion, TableDef,
};

pub use bulk::{
    BulkLimits, BulkLoader, BulkMethod, InsertLoader, LoadColumn, LoadSession, LoadTarget,
};
pub use capability::{DesignerCapabilities, Support};
pub use catalog::{CatalogReader, CatalogScope, RelationName};
pub use ddl::DdlContext;
pub use dml::{DmlPlan, DmlTarget, write_column_list, write_key_predicate};
pub use dump::{DumpLiteral, DumpSession, IdentityInsert};
pub use language::{FunctionInfo, FunctionKind, SqlLanguage, TypeInfo, TypeParams};
pub use quoting::{AnsiQuoting, BacktickQuoting, BracketQuoting, LexRules, PgQuoting, Quoting};
pub use registry::{
    CellRole, CellStatus, CoverageMatrix, DESIGNER_KINDS, DialectRegistry, EngineRole, MissingCell,
};
pub use tree::{ChildGroup, DatabaseScope, TreeLayout};

/// Reads objects of one kind from a live connection. One implementation per (engine, kind).
///
/// Both methods can be long-running (a catalog with thousands of tables), so both take a
/// [`CancellationToken`] (PLAN §1, rule 4). Implementations pass it on to every
/// [`Connection::execute`] they make and check it between round trips; once it is cancelled they
/// return `Err(Error::Cancelled)` and no partial result.
///
/// # M2 rules for implementations
///
/// - **Catalog queries only, with bound parameters.** Names from `parent` / `obj` are passed as
///   parameters (`$1` / `?`), never interpolated into SQL text, so no name can change the
///   query. Only fixed SQL text is concatenated (for example a version-specific column list).
/// - **Version-aware.** Pick the catalog query from [`Connection::server_version`] (PostgreSQL
///   `prokind` is 11+, MariaDB sequences 10.3+, SQLite `pragma_table_list` 3.37+, ...). A kind
///   the server version does not have is answered by [`Capabilities::supports_kind`], not by a
///   failing query.
/// - **Parents** (what `list` receives): `Database` → [`ObjectRef::root`]; `Schema` → a
///   `Database` ref; schema-level kinds (Table, View, MatView, Function, Procedure, Sequence) → a
///   `Schema` ref (PostgreSQL) or a `Database` ref (MySQL, MariaDB, SQLite, where the database
///   name is the `ObjectRef` name and children get it as `catalog`); table children (Column,
///   Index, ForeignKey, Check, Trigger) → the owning table, view or materialized view.
/// - **Refs** of listed objects are built with [`ObjectRef::child`] from `parent` (so `catalog`
///   is always set on engines with [`DatabaseScope::Session`] / [`DatabaseScope::Catalog`]),
///   plus [`ObjectRef::with_signature`] for PostgreSQL routines.
/// - **Order**: `list` returns objects sorted by name (byte order of the server's collation is
///   fine); columns in ordinal order.
/// - **System objects** (`pg_catalog`, `information_schema`, MySQL `mysql` / `sys` /
///   `performance_schema`, SQLite `sqlite_*` tables) are listed with `system = true`; the tree
///   hides them unless asked.
/// - `load` returns the [`ObjectDef`] variant documented on [`ObjectDef`] for the kind and
///   fails with `Error::NotFound` when the object is gone.
#[async_trait]
pub trait Fetcher: Send + Sync {
    /// Lists the objects of this fetcher's kind directly under `parent` (for example the tables
    /// of a schema). The parent of a `Database` listing is [`ObjectRef::root`].
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>>;

    /// Loads the full typed definition of `obj`. Fails with `Error::NotFound` if it is gone.
    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef>;
}

/// Produces DDL for one object kind. One implementation per (engine, kind).
///
/// # M3 rules for implementations
///
/// - **Kinds.** Every M3 engine registers a generator for `Table`, `Column`, `Index`,
///   `ForeignKey` and `Check` ([`DESIGNER_KINDS`]). The designer uses the `Table` cell for
///   everything (a table's `alter` covers its columns, keys, indexes, foreign keys, checks and
///   comments); the child cells serve the conformance round trip and, later, sync. A child
///   generator may build the table's new definition from [`DdlContext::table`] and delegate to
///   the table generator.
/// - **Quoting.** Every identifier goes through the engine's [`Quoting`]; comments and literal
///   defaults through [`Quoting::quote_str`] (quotes, backslashes and unicode escaped for the
///   engine). Expressions (defaults, checks, generated columns, index predicates) are
///   engine-native SQL from the model and are inserted as they are; the designer has checked
///   user-typed ones with [`sql::check_expression`].
/// - **Steps.** One [`oxbase_types::ScriptStep`] per statement, with a human `description`,
///   `destructive` for anything that can lose data (drop column / table / constraint, a
///   narrowing type change, a table rebuild), and `transactional = false` for statements that
///   commit implicitly (every MySQL/MariaDB DDL statement).
/// - **Order of an `alter`.** Renames ([`DdlContext::renames`]) first; then drops of foreign
///   keys, checks and indexes that change or go; then column drops, changes and adds; then the
///   primary key; then new or changed indexes, checks and foreign keys; comments last. An empty
///   script when nothing changed.
/// - **Rebuilds.** An engine that cannot alter in place (SQLite beyond `RENAME` / `ADD COLUMN`
///   / `DROP COLUMN` of a plain column) emits the documented table rebuild inside the script's
///   transaction, with foreign-key enforcement switched off in a prepare step and restored in a
///   finally step only if it was on (see the `oxbase_types::script` module docs), a
///   `foreign_key_check` that must return no rows, and every index and trigger of the table
///   ([`DdlContext::triggers`]) re-created. All rebuild steps are destructive.
/// - **Errors.** What the engine or version cannot do is `Error::Unsupported` naming it; a
///   change the model cannot express safely is `Error::InvalidInput`. Properties that are only
///   informational and cannot be expressed are skipped with a [`Script::warn`] line.
pub trait DdlGenerator: Send + Sync {
    /// `CREATE` script for `d` (a table with its columns, keys, indexes, foreign keys, checks
    /// and comments; or a child added to [`DdlContext::table`]).
    fn create(&self, d: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script>;

    /// Script that turns `old` into `new` (in place where possible, else a rebuild with
    /// destructive steps flagged). Empty when they are equivalent.
    fn alter(&self, old: &ObjectDef, new: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script>;

    /// `DROP` script for `r`.
    ///
    /// Named `drop_object` rather than `drop` so that calling it on an `Arc<dyn DdlGenerator>`
    /// or `Box<dyn DdlGenerator>` does not resolve to `Drop::drop`.
    fn drop_object(&self, r: &ObjectRef, ctx: &DdlContext<'_>) -> Result<Script>;
}

/// Compares two definitions of the same kind. One implementation per (engine, kind), so engine
/// normalisation (default expression spelling, implicit collations, ...) stays out of the diff.
///
/// M3: engines register [`compare::RulesComparator`] with their [`compare::CompareRules`] for
/// the [`DESIGNER_KINDS`], which fixes the property ids and the order of the result (see the
/// [`compare`] module docs). A definition fetched right after `DdlGenerator::create` /
/// `alter` must compare equal (empty diff) to the definition the script was generated from;
/// that is the conformance round trip.
pub trait Comparator: Send + Sync {
    /// Differences going from `a` (source / old) to `b` (target / new). Empty when equivalent.
    fn diff(&self, a: &ObjectDef, b: &ObjectDef) -> Vec<PropertyDiff>;
}

/// Answers what an engine supports at a given server version. One implementation per engine.
pub trait Capabilities: Send + Sync {
    /// `true` if property `prop` of objects of `kind` is supported by server version `v`.
    fn supports(&self, kind: ObjectKind, prop: PropId, v: &ServerVersion) -> bool;

    /// `true` if objects of `kind` exist at all on server version `v` (SQLite has no users,
    /// MySQL has no sequences, ...). Defaults to `true`.
    fn supports_kind(&self, kind: ObjectKind, v: &ServerVersion) -> bool {
        let _ = (kind, v);
        true
    }

    /// What the table designer may offer on server version `v` (M3). Defaults to a read-only
    /// designer.
    fn designer(&self, v: &ServerVersion) -> DesignerCapabilities {
        let _ = v;
        DesignerCapabilities::read_only()
    }
}

/// Turns one grid edit into parameterized DML (grid write-back). One implementation per engine.
///
/// See the [`DmlPlan`] / [`DmlTarget`] docs (module `dml`) for the full contract: bound values
/// only, rows matched by key, `RETURNING` or a read-back query per engine and version, and the
/// service's "exactly one row" rule. Callers validate the edit with
/// [`oxbase_types::RowKey::validate`] first.
pub trait DmlWriter: Send + Sync {
    /// The statement(s) for `edit` on `target`.
    fn plan(&self, edit: &RowEdit, target: &DmlTarget<'_>) -> Result<DmlPlan>;

    /// `true` when writes to `table` take part in transactions, so a rollback undoes them.
    /// `false` for storage that keeps every write as soon as it runs (MySQL / MariaDB tables
    /// on `MyISAM`, `MEMORY`, `ARCHIVE`, ...): the edit service then reports writes that ran
    /// before a failure as applied instead of rolled back. Defaults to `true`.
    fn transactional(&self, table: &TableDef) -> bool {
        let _ = table;
        true
    }
}
