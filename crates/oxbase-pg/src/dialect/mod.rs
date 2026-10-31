//! PostgreSQL dialect cells (M2): catalog [`Fetcher`](oxbase_dialect::Fetcher)s, the object-tree
//! layout, capabilities and quoting. Register them with [`register`].
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

mod fetch;
mod rows;
mod sql;
mod types;

use std::sync::Arc;

use oxbase_dialect::{
    Capabilities, ChildGroup, DatabaseScope, DialectRegistry, PgQuoting, TreeLayout,
};
use oxbase_types::{Engine, ObjectKind, PropId, ServerVersion};

pub use fetch::{
    BASE_TYPE, CTYPE, ENUM_LABELS, EXCLUSION, EXTENSION_SCHEMA, EXTENSION_VERSION,
    IDENTITY_SEQUENCE, MATCH_TYPE, NO_INHERIT, NOT_ENFORCED, NOT_VALID, NULLS_FIRST,
    NULLS_NOT_DISTINCT, PARALLEL, TYPE_KIND,
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

/// Registers the PostgreSQL fetchers, tree layout, capabilities and quoting in `reg`.
pub fn register(reg: &mut DialectRegistry) {
    let engine = Engine::Postgres;
    for kind in fetch::KINDS {
        reg.register_fetcher(engine, kind, Arc::new(fetch::PgFetcher::new(kind)));
    }
    reg.register_tree_layout(engine, tree_layout());
    reg.register_capabilities(engine, Arc::new(PgCapabilities));
    reg.register_quoting(engine, Arc::new(PgQuoting));
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
