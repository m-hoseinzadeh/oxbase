//! PostgreSQL dialect cells: catalog [`Fetcher`](oxbase_dialect::Fetcher)s, the object-tree
//! layout, capabilities and quoting (M2), and the table designer and grid editing roles (M3):
//! [`DdlGenerator`](oxbase_dialect::DdlGenerator) and
//! [`Comparator`](oxbase_dialect::Comparator) cells for tables, columns, indexes, foreign keys
//! and checks, the [`DmlWriter`](oxbase_dialect::DmlWriter) and
//! [`Capabilities::designer`]. Register them with [`register`].
//!
//! # Object tree
//!
//! ```text
//! connection
//! └─ databases                       pg_database (templates and no-connect databases hidden)
//!    └─ schemas                      pg_namespace of the session's database
//!       ├─ Tables, Views, Materialized views, Functions, Procedures (11+), Sequences
//!       │  table → Columns, Indexes, Foreign keys, Check constraints, Triggers
//!       │  view → Columns, Triggers (INSTEAD OF)
//!       │  materialized view → Columns, Indexes
//! ```
//!
//! A PostgreSQL session reads one database ([`DatabaseScope::Session`]): objects of another
//! database are read through a session opened on it. Fetchers refuse refs whose `catalog` names
//! another database than [`Connection::current_database`](oxbase_driver::Connection::current_database)
//! with `InvalidInput`. The database list itself is server-wide.
//!
//! # Fetcher cells
//!
//! Database, schema, table, column, index, foreign key, check, view, materialized view, function,
//! procedure, trigger and sequence (plus type and extension, which are not in the tree yet). All
//! catalog queries are fixed text in `sql.rs` with names bound as parameters, picked by server
//! version (PostgreSQL 10 and newer; see `sql.rs` for the version-dependent parts). System
//! schemas (`pg_catalog`, `information_schema`, `pg_toast*`) and everything in them are listed
//! with `system = true`, and so are relations, routines and types an extension owns (and the
//! children of such relations), whatever schema they live in.
//!
//! Table listings carry no `SIZE_BYTES`: `pg_total_relation_size` locks each relation and stats
//! its files, so one table held by an `ACCESS EXCLUSIVE` lock would stall the whole folder.
//!
//! Routines are told apart by [`ObjectRef::signature`](oxbase_types::ObjectRef::signature), the
//! text of `pg_get_function_identity_arguments` (`n integer`, `t text, n integer`, empty for no
//! arguments). `load` of a routine without a signature works when the name has one overload.
//!
//! Engine-specific extras beyond `oxbase_types::props` are the constants re-exported here
//! ([`NULLS_NOT_DISTINCT`], [`NOT_VALID`], ...).
//!
//! # Designer and grid editing (M3)
//!
//! - DDL (`ddl.rs`): native, transactional statements for every change (see its module docs
//!   for the order of an alter and the version gates); identifiers through [`PgQuoting`],
//!   literals through `quote_str`, expressions and type names checked lexically.
//! - Comparison: [`PgComparator`], which fills in the PostgreSQL default of every extra the
//!   designer edits ([`normalize_def`]: an absent `nulls_not_distinct` is `false`, an absent
//!   `match_type` is `simple`, ...) and then runs `oxbase_dialect::compare` with [`PgRules`]
//!   (the shared walk compares an extra only when both sides have it). [`PgRules`] folds type
//!   aliases ([`canonical_type`]) and the server's rewriting of expressions
//!   ([`normalize_expression`]).
//! - Grid writes (`dml.rs`): bound `$n` parameters, key-only `WHERE`, `RETURNING` read-back.
//!
//! # Bulk load (M5)
//!
//! [`PgBulkLoader`] (`bulk.rs`): import batches go through `COPY ... FROM STDIN` in text format
//! on the import session's client (the connection exposes it to this crate through
//! `Connection::as_any` / `Tx::as_any`; `docs/m5-contracts.md` §4.3).

mod bulk;
mod caps;
mod catalog_bulk;
mod compare;
mod ddl;
mod dml;
mod fetch;
// M6: keywords, functions and types for code insight.
mod language;
mod rows;
mod sql;
mod types;

use std::sync::Arc;

use oxbase_dialect::{
    Capabilities, ChildGroup, DESIGNER_KINDS, DatabaseScope, DesignerCapabilities, DialectRegistry,
    PgQuoting, TreeLayout,
};
use oxbase_types::{Engine, ObjectKind, PropId, ServerVersion};

pub use bulk::PgBulkLoader;
pub use catalog_bulk::PgCatalogReader;
pub use compare::{PgComparator, PgRules, canonical_type, normalize_def, normalize_expression};

pub use fetch::{
    BASE_TYPE, CTYPE, ENUM_LABELS, EXCLUSION, EXTENSION_SCHEMA, EXTENSION_VERSION,
    IDENTITY_SEQUENCE, INDEX_OPTIONS, KEY_COLLATIONS, MATCH_TYPE, NO_INHERIT, NOT_ENFORCED,
    NOT_VALID, NULLS_FIRST, NULLS_NOT_DISTINCT, OPCLASS_OPTIONS, OPCLASSES, PARALLEL, TYPE_KIND,
};

/// Object kinds PostgreSQL has (everything but MySQL events and key-value keys).
const APPLICABLE: [ObjectKind; 18] = [
    ObjectKind::Database,
    ObjectKind::Schema,
    ObjectKind::Table,
    ObjectKind::Column,
    ObjectKind::Index,
    ObjectKind::ForeignKey,
    ObjectKind::Check,
    ObjectKind::View,
    ObjectKind::MatView,
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Trigger,
    ObjectKind::Sequence,
    ObjectKind::User,
    ObjectKind::Role,
    ObjectKind::Grant,
    ObjectKind::Type,
    ObjectKind::Extension,
];

/// Registers the PostgreSQL fetchers, tree layout, capabilities and quoting (M2), and the DDL
/// generators, comparators and grid writer (M3) in `reg`.
pub fn register(reg: &mut DialectRegistry) {
    let engine = Engine::Postgres;
    for kind in fetch::KINDS {
        reg.register_fetcher(engine, kind, Arc::new(fetch::PgFetcher::new(kind)));
    }
    let ddl = Arc::new(ddl::PgDdl);
    let comparator = Arc::new(PgComparator);
    for kind in DESIGNER_KINDS {
        reg.register_ddl(engine, kind, ddl.clone());
        reg.register_comparator(engine, kind, comparator.clone());
    }
    reg.register_dml_writer(engine, Arc::new(dml::PgDmlWriter));
    reg.register_tree_layout(engine, tree_layout());
    reg.register_capabilities(engine, Arc::new(PgCapabilities));
    reg.register_quoting(engine, Arc::new(PgQuoting));
    // M5: import bulk loads with COPY ... FROM STDIN (bulk.rs).
    reg.register_bulk_loader(engine, Arc::new(PgBulkLoader));
    // M6: the schema cache reads the whole catalog in a fixed number of queries.
    reg.register_catalog_reader(engine, Arc::new(PgCatalogReader));
    // M6: language data for code insight (language.rs).
    reg.register_language(engine, language::LANGUAGE);
    reg.mark_not_applicable_except(engine, &APPLICABLE);
}

/// The PostgreSQL object tree (see the module docs).
pub fn tree_layout() -> TreeLayout {
    use ChildGroup as G;
    use ObjectKind as K;
    TreeLayout::new(DatabaseScope::Session)
        .with_root(&[G::direct(K::Database)])
        .with_children(K::Database, &[G::direct(K::Schema)])
        .with_children(
            K::Schema,
            &[
                G::folder(K::Table),
                G::folder(K::View),
                G::folder(K::MatView),
                G::folder(K::Function),
                G::folder(K::Procedure),
                G::folder(K::Sequence),
            ],
        )
        .with_children(
            K::Table,
            &[
                G::folder(K::Column),
                G::folder(K::Index),
                G::folder(K::ForeignKey),
                G::folder(K::Check),
                G::folder(K::Trigger),
            ],
        )
        .with_children(K::View, &[G::folder(K::Column), G::folder(K::Trigger)])
        .with_children(K::MatView, &[G::folder(K::Column), G::folder(K::Index)])
}

/// What PostgreSQL supports by server version.
#[derive(Debug, Clone, Copy, Default)]
pub struct PgCapabilities;

impl Capabilities for PgCapabilities {
    fn supports(&self, kind: ObjectKind, prop: PropId, v: &ServerVersion) -> bool {
        if !self.supports_kind(kind, v) {
            return false;
        }
        match prop.as_str() {
            // MySQL / MariaDB / SQLite / SQL Server only.
            "charset" | "storage_engine" | "row_format" | "without_rowid" | "strict"
            | "clustered" | "deterministic" | "sql_data_access" | "definer" | "view_algorithm"
            | "file" => false,
            "generated" => sql::at_least(v, 12),
            "include_columns" => sql::at_least(v, 11),
            "nulls_not_distinct" => sql::at_least(v, 15),
            _ => true,
        }
    }

    fn supports_kind(&self, kind: ObjectKind, v: &ServerVersion) -> bool {
        match kind {
            ObjectKind::Event | ObjectKind::Key => false,
            ObjectKind::Procedure => sql::at_least(v, 11),
            _ => true,
        }
    }

    fn designer(&self, v: &ServerVersion) -> DesignerCapabilities {
        caps::designer(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_dialect::CellStatus;
    use oxbase_types::props;

    #[test]
    fn registers_every_layout_kind() {
        let mut reg = DialectRegistry::new();
        register(&mut reg);
        let engine = Engine::Postgres;
        assert_eq!(
            reg.missing_layout_fetchers(engine),
            Vec::<ObjectKind>::new()
        );
        let layout = reg.tree_layout(engine).unwrap();
        assert_eq!(layout.scope, DatabaseScope::Session);
        assert_eq!(layout.root(), &[ChildGroup::direct(ObjectKind::Database)]);
        assert_eq!(
            layout.children(ObjectKind::Database),
            &[ChildGroup::direct(ObjectKind::Schema)]
        );
        assert_eq!(layout.children(ObjectKind::Schema).len(), 6);
        assert!(!layout.is_expandable(ObjectKind::Function));
        assert!(reg.capabilities(engine).is_ok());
        assert!(reg.quoting(engine).is_ok());
        for kind in fetch::KINDS {
            assert!(reg.fetcher(engine, kind).is_ok(), "{kind}");
        }
        assert_eq!(
            reg.status(engine, ObjectKind::Event),
            CellStatus::NotApplicable
        );
        assert_eq!(
            reg.status(engine, ObjectKind::Key),
            CellStatus::NotApplicable
        );
        assert_ne!(
            reg.status(engine, ObjectKind::User),
            CellStatus::NotApplicable
        );
    }

    #[test]
    fn registers_every_designer_role() {
        let mut reg = DialectRegistry::new();
        register(&mut reg);
        assert_eq!(
            reg.missing_designer_roles(Engine::Postgres),
            Vec::<String>::new()
        );
        let caps = reg.capabilities(Engine::Postgres).unwrap();
        assert!(
            caps.designer(&ServerVersion::new(18, 6, 0))
                .transactional_ddl
        );
        for kind in DESIGNER_KINDS {
            assert!(reg.ddl(Engine::Postgres, kind).is_ok(), "{kind}");
            assert!(reg.comparator(Engine::Postgres, kind).is_ok(), "{kind}");
        }
        assert!(reg.dml_writer(Engine::Postgres).is_ok());
    }

    #[test]
    fn capabilities_follow_the_server_version() {
        let caps = PgCapabilities;
        let v10 = ServerVersion::new(10, 23, 0);
        let v11 = ServerVersion::new(11, 0, 0);
        let v18 = ServerVersion::new(18, 6, 0);
        assert!(!caps.supports_kind(ObjectKind::Procedure, &v10));
        assert!(caps.supports_kind(ObjectKind::Procedure, &v11));
        assert!(!caps.supports_kind(ObjectKind::Event, &v18));
        assert!(caps.supports_kind(ObjectKind::MatView, &v10));
        assert!(caps.supports(ObjectKind::Column, props::GENERATED, &v18));
        assert!(!caps.supports(ObjectKind::Column, props::GENERATED, &v11));
        assert!(!caps.supports(ObjectKind::Table, props::STORAGE_ENGINE, &v18));
        assert!(caps.supports(ObjectKind::Index, props::INCLUDE_COLUMNS, &v11));
        assert!(!caps.supports(ObjectKind::Index, NULLS_NOT_DISTINCT, &v11));
        assert!(caps.supports(ObjectKind::Table, props::UNLOGGED, &v10));
    }

    #[test]
    fn registry_resolves_postgres_kinds_by_version() {
        let mut reg = DialectRegistry::new();
        register(&mut reg);
        assert!(!reg.supports_kind(
            Engine::Postgres,
            ObjectKind::Procedure,
            &ServerVersion::new(10, 0, 0)
        ));
        assert!(reg.supports_kind(
            Engine::Postgres,
            ObjectKind::Procedure,
            &ServerVersion::new(18, 6, 0)
        ));
    }
}
