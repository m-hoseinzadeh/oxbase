//! SQL Server dialect cells (M4): catalog [`Fetcher`](oxbase_dialect::Fetcher)s (**fetch**
//! stream), the [`DdlGenerator`](oxbase_dialect::DdlGenerator), comparator rules
//! ([`MsSqlRules`]), designer capabilities and the [`DmlWriter`](oxbase_dialect::DmlWriter)
//! (**ddl** stream). This file (registration and the tree layout) is the Design step's.
//! Contract: `docs/m4-contracts.md` §3-4.
//!
//! # Object tree
//!
//! ```text
//! connection
//! └─ databases                      sys.databases (system databases flagged)
//!    └─ schemas                     sys.schemas of that database
//!       ├─ Tables                   table → Columns, Indexes, Foreign keys, Check constraints,
//!       │                                   Triggers (DML triggers)
//!       ├─ Views                    view  → Columns, Indexes (indexed views), Triggers
//!       │                                   (INSTEAD OF)
//!       ├─ Functions                scalar, inline table-valued, multi-statement table-valued,
//!       │                           CLR (function_kind extra)
//!       ├─ Procedures
//!       ├─ Sequences
//!       └─ Types                    alias and table types, list and load only (type_kind)
//! ```
//!
//! [`DatabaseScope::Session`]: one session reads one database, like PostgreSQL. Objects of
//! another database are read on a metadata child session opened on it (`ConnectParams::database`
//! = that database), so every catalog query runs against the session's own `sys.*` views with
//! bound parameters only, and no name is ever formatted into SQL. `ObjectRef::catalog` is the
//! database, `ObjectRef::schema` the schema. Names are written `[schema].[name]` without the
//! database ([`BracketQuoting`]).
//!
//! Not in M4: database-level DDL triggers (not listed), synonyms (no object kind), users, roles
//! and permissions (kinds applicable, no fetchers: pending cells), partition schemes and
//! functions, full-text catalogs, external tables, assemblies.
//!
//! M5 (**load** stream): the import [`MsSqlBulkLoader`] (`bulk.rs`), batched multi-row
//! `INSERT`s under the 2 100-parameter limit, with `IDENTITY_INSERT` when an identity column is
//! loaded.

mod bulk;
mod caps;
mod compare;
mod ddl;
mod dml;
mod fetch;

use std::sync::Arc;

use oxbase_dialect::{
    BracketQuoting, ChildGroup, DESIGNER_KINDS, DatabaseScope, DialectRegistry, TreeLayout,
};
use oxbase_types::{Engine, ObjectKind};

pub use bulk::MsSqlBulkLoader;
pub use caps::MsSqlCapabilities;
pub use compare::MsSqlRules;
pub use fetch::catalog_bulk::MsSqlCatalogReader;

/// Object kinds SQL Server has. `User`, `Role` and `Grant` exist but have no fetcher in M4.
pub const APPLICABLE: [ObjectKind; 16] = [
    ObjectKind::Database,
    ObjectKind::Schema,
    ObjectKind::Table,
    ObjectKind::Column,
    ObjectKind::Index,
    ObjectKind::ForeignKey,
    ObjectKind::Check,
    ObjectKind::View,
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Trigger,
    ObjectKind::Sequence,
    ObjectKind::User,
    ObjectKind::Role,
    ObjectKind::Grant,
    ObjectKind::Type,
];

/// Registers every SQL Server dialect cell in `reg`: fetchers for the tree layout's kinds, the
/// layout, capabilities, quoting, the DDL generators and comparators of the designer kinds, and
/// the grid writer.
pub fn register(reg: &mut DialectRegistry) {
    let engine = Engine::MsSql;
    for kind in fetch::KINDS {
        reg.register_fetcher(engine, kind, Arc::new(fetch::MsSqlFetcher::new(kind)));
    }
    let ddl = Arc::new(ddl::MsSqlDdl);
    let comparator = compare::comparator();
    for kind in DESIGNER_KINDS {
        reg.register_ddl(engine, kind, ddl.clone());
        reg.register_comparator(engine, kind, comparator.clone());
    }
    reg.register_dml_writer(engine, Arc::new(dml::MsSqlDmlWriter));
    reg.register_tree_layout(engine, tree_layout());
    reg.register_capabilities(engine, Arc::new(MsSqlCapabilities));
    reg.register_quoting(engine, Arc::new(BracketQuoting));
    // M5: import bulk loads as batched multi-row INSERTs (bulk.rs, load stream).
    reg.register_bulk_loader(engine, Arc::new(MsSqlBulkLoader::new()));
    // M6: the schema cache reads the whole catalog in a fixed number of queries.
    reg.register_catalog_reader(engine, Arc::new(MsSqlCatalogReader));
    reg.mark_not_applicable_except(engine, &APPLICABLE);
}

/// The SQL Server object tree (see the module docs).
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
                G::folder(K::Function),
                G::folder(K::Procedure),
                G::folder(K::Sequence),
                G::folder(K::Type),
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
        .with_children(
            K::View,
            &[
                G::folder(K::Column),
                G::folder(K::Index),
                G::folder(K::Trigger),
            ],
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_dialect::CellStatus;
    use oxbase_types::ServerVersion;

    #[test]
    fn registers_every_layout_kind_and_designer_role() {
        let mut reg = DialectRegistry::new();
        register(&mut reg);
        let engine = Engine::MsSql;
        assert_eq!(
            reg.missing_layout_fetchers(engine),
            Vec::<ObjectKind>::new()
        );
        assert_eq!(reg.missing_designer_roles(engine), Vec::<String>::new());
        let layout = reg.tree_layout(engine).unwrap();
        assert_eq!(layout.scope, DatabaseScope::Session);
        assert_eq!(layout.root(), &[ChildGroup::direct(ObjectKind::Database)]);
        assert_eq!(layout.children(ObjectKind::Schema).len(), 6);
        assert_eq!(layout.children(ObjectKind::View).len(), 3);
        assert!(!layout.is_expandable(ObjectKind::Function));
        for kind in fetch::KINDS {
            assert!(reg.fetcher(engine, kind).is_ok(), "{kind}");
        }
        assert!(reg.dml_writer(engine).is_ok());
        assert_eq!(reg.quoting(engine).unwrap().placeholder(2), "@P2");
        assert_eq!(
            reg.status(engine, ObjectKind::MatView),
            CellStatus::NotApplicable
        );
        assert_eq!(
            reg.status(engine, ObjectKind::Extension),
            CellStatus::NotApplicable
        );
        assert_ne!(
            reg.status(engine, ObjectKind::User),
            CellStatus::NotApplicable
        );
        assert!(reg.supports_kind(engine, ObjectKind::Sequence, &ServerVersion::new(14, 0, 0)));
        assert!(!reg.supports_kind(engine, ObjectKind::MatView, &ServerVersion::new(17, 0, 0)));
    }
}
