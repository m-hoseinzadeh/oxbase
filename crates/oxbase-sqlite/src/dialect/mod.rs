//! SQLite dialect cells (M2): the object-tree layout, the [`Fetcher`]s, [`Capabilities`] and
//! [`Quoting`].
//!
//! Call [`register`] once on the app's [`DialectRegistry`] (`oxbase_services::default_dialects`
//! does).
//!
//! # Tree
//!
//! ```text
//! connection
//! └─ main, attached databases, temp (when it holds objects)    DatabaseScope::Attached
//!    ├─ Tables → table → Columns, Indexes, Foreign keys, Check constraints, Triggers
//!    └─ Views  → view  → Columns, Triggers (INSTEAD OF)
//! ```
//!
//! `ObjectRef::catalog` is the SQLite schema name (`main`, `temp` or the attach name); a ref
//! without a catalog means `main`. SQLite has no schemas in the PostgreSQL sense, no routines,
//! sequences, users or comments.
//!
//! # Where the facts come from
//!
//! `sqlite_schema` (names, `CREATE` text), `pragma_database_list`, `pragma_table_list` (3.37+:
//! table type, `WITHOUT ROWID`, `STRICT`), `pragma_table_xinfo` (columns, generated and hidden
//! ones included), `pragma_index_list` / `pragma_index_xinfo` and `pragma_foreign_key_list`, every
//! one queried with bound parameters (table *and* schema). Constraint names, checks, generated
//! expressions, collations, `AUTOINCREMENT`, index expressions and predicates, view bodies and
//! trigger clauses are read from the stored `CREATE` text. See `catalog` for the single place a
//! (verified, quoted) database name is written into SQL.
//!
//! # Extras
//!
//! - Databases: [`props::FILE`] (file path, absent for in-memory), [`props::CURRENT`] (`main`),
//!   [`props::TEMPORARY`] (`temp`), [`props::ENCODING`] on load.
//! - Tables: [`props::TABLE_TYPE`] (`virtual`, `shadow`), [`MODULE`] (virtual-table module),
//!   [`props::WITHOUT_ROWID`], [`props::STRICT`], [`props::TEMPORARY`] (only set when true).
//! - Columns: [`AUTOINCREMENT`] (the `AUTOINCREMENT` keyword; `auto_increment` is also set for
//!   every rowid alias), [`HIDDEN`] (hidden virtual-table column; listed as `system`).
//! - Foreign keys: [`props::DEFERRABLE`], [`props::INITIALLY_DEFERRED`].
//! - Checks: [`COLUMN`] for a check declared on a column.
//! - Listings: unnamed foreign keys and checks carry [`UNNAMED`] (see `fetch`).

mod catalog;
mod fetch;
mod parse;
mod types;

use std::sync::Arc;

use oxbase_dialect::{
    AnsiQuoting, Capabilities, ChildGroup, DatabaseScope, DialectRegistry, Fetcher, Quoting,
    TreeLayout,
};
use oxbase_types::{Engine, ObjectKind, PropId, ServerVersion, props};

/// Column extra: declared with the `AUTOINCREMENT` keyword (bool).
pub const AUTOINCREMENT: PropId = PropId::from_static("autoincrement");
/// Check extra: the column a column-level check is declared on (text).
pub const COLUMN: PropId = PropId::from_static("column");
/// Column extra: a hidden column of a virtual table (bool).
pub const HIDDEN: PropId = PropId::from_static("hidden");
/// Table extra: the module of a virtual table (`fts5`, `rtree`, ...).
pub const MODULE: PropId = PropId::from_static("module");
/// Listing extra: a foreign key or check declared without a name, listed under a synthetic one
/// (bool).
pub const UNNAMED: PropId = PropId::from_static("unnamed");

/// The object kinds SQLite has.
pub const APPLICABLE: &[ObjectKind] = &[
    ObjectKind::Database,
    ObjectKind::Table,
    ObjectKind::Column,
    ObjectKind::Index,
    ObjectKind::ForeignKey,
    ObjectKind::Check,
    ObjectKind::View,
    ObjectKind::Trigger,
];

/// The SQLite object tree (see the module docs).
pub fn tree_layout() -> TreeLayout {
    use ObjectKind as K;
    TreeLayout::new(DatabaseScope::Attached)
        .with_root(&[ChildGroup::direct(K::Database)])
        .with_children(
            K::Database,
            &[ChildGroup::folder(K::Table), ChildGroup::folder(K::View)],
        )
        .with_children(
            K::Table,
            &[
                ChildGroup::folder(K::Column),
                ChildGroup::folder(K::Index),
                ChildGroup::folder(K::ForeignKey),
                ChildGroup::folder(K::Check),
                ChildGroup::folder(K::Trigger),
            ],
        )
        .with_children(
            K::View,
            &[
                ChildGroup::folder(K::Column),
                ChildGroup::folder(K::Trigger),
            ],
        )
}

/// What SQLite supports, by library version.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteCapabilities;

/// Model properties every SQLite version has.
const ALWAYS: &[PropId] = &[
    props::NAME,
    props::DATA_TYPE,
    props::NULLABLE,
    props::DEFAULT,
    props::AUTO_INCREMENT,
    props::COLLATION,
    props::POSITION,
    props::PRIMARY_KEY,
    props::COLUMNS,
    props::UNIQUE,
    props::PREDICATE,
    props::REF_TABLE,
    props::REF_COLUMNS,
    props::ON_DELETE,
    props::ON_UPDATE,
    props::EXPRESSION,
    props::DEFINITION,
    props::TEMPORARY,
    props::TABLE_TYPE,
    props::FILE,
    props::ENCODING,
    props::NATIVE_TYPE,
    props::CURRENT,
    AUTOINCREMENT,
    COLUMN,
    HIDDEN,
    MODULE,
];

impl Capabilities for SqliteCapabilities {
    fn supports(&self, kind: ObjectKind, prop: PropId, v: &ServerVersion) -> bool {
        if !self.supports_kind(kind, v) {
            return false;
        }
        if prop == props::STRICT {
            kind == ObjectKind::Table && v.at_least(3, 37)
        } else if prop == props::WITHOUT_ROWID {
            kind == ObjectKind::Table && v.at_least_patch(3, 8, 2)
        } else if prop == props::GENERATED {
            matches!(kind, ObjectKind::Table | ObjectKind::Column) && v.at_least(3, 31)
        } else if prop == props::DEFERRABLE || prop == props::INITIALLY_DEFERRED {
            matches!(kind, ObjectKind::Table | ObjectKind::ForeignKey)
        } else {
            // No comments, owners, charsets, tablespaces, storage engines, ...
            ALWAYS.contains(&prop)
        }
    }

    fn supports_kind(&self, kind: ObjectKind, _v: &ServerVersion) -> bool {
        APPLICABLE.contains(&kind)
    }
}

/// Registers the SQLite cells: a fetcher per [`APPLICABLE`] kind, the [`tree_layout`],
/// [`SqliteCapabilities`], ANSI quoting (`"ident"`, `X'..'`) and not-applicable marks for
/// every other kind.
pub fn register(reg: &mut DialectRegistry) {
    let engine = Engine::Sqlite;
    let fetchers: [(ObjectKind, Arc<dyn Fetcher>); 8] = [
        (ObjectKind::Database, Arc::new(fetch::DatabaseFetcher)),
        (ObjectKind::Table, Arc::new(fetch::TableFetcher)),
        (ObjectKind::Column, Arc::new(fetch::ColumnFetcher)),
        (ObjectKind::Index, Arc::new(fetch::IndexFetcher)),
        (ObjectKind::ForeignKey, Arc::new(fetch::ForeignKeyFetcher)),
        (ObjectKind::Check, Arc::new(fetch::CheckFetcher)),
        (ObjectKind::View, Arc::new(fetch::ViewFetcher)),
        (ObjectKind::Trigger, Arc::new(fetch::TriggerFetcher)),
    ];
    for (kind, fetcher) in fetchers {
        reg.register_fetcher(engine, kind, fetcher);
    }
    let quoting: Arc<dyn Quoting> = Arc::new(AnsiQuoting);
    reg.register_tree_layout(engine, tree_layout())
        .register_capabilities(engine, Arc::new(SqliteCapabilities))
        .register_quoting(engine, quoting)
        .mark_not_applicable_except(engine, APPLICABLE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_dialect::CellStatus;

    #[test]
    fn registers_a_complete_layout() {
        let mut reg = DialectRegistry::new();
        register(&mut reg);
        assert_eq!(
            reg.missing_layout_fetchers(Engine::Sqlite),
            Vec::<ObjectKind>::new()
        );
        assert_eq!(
            reg.tree_layout(Engine::Sqlite).unwrap().scope,
            DatabaseScope::Attached
        );
        for kind in ObjectKind::ALL {
            let applicable = APPLICABLE.contains(&kind);
            assert_eq!(
                reg.fetcher(Engine::Sqlite, kind).is_ok(),
                applicable,
                "{kind}"
            );
            if !applicable {
                assert_eq!(reg.status(Engine::Sqlite, kind), CellStatus::NotApplicable);
            }
        }
        assert_eq!(
            reg.quoting(Engine::Sqlite).unwrap().quote_ident("a\"b"),
            "\"a\"\"b\""
        );
        assert!(reg.capabilities(Engine::Sqlite).is_ok());
    }

    #[test]
    fn capabilities_follow_the_version() {
        let caps = SqliteCapabilities;
        let new: ServerVersion = "3.54.1".parse().unwrap();
        let old: ServerVersion = "3.31.0".parse().unwrap();
        assert!(caps.supports(ObjectKind::Table, props::STRICT, &new));
        assert!(!caps.supports(ObjectKind::Table, props::STRICT, &old));
        assert!(caps.supports(ObjectKind::Column, props::GENERATED, &old));
        assert!(caps.supports(ObjectKind::Table, props::WITHOUT_ROWID, &old));
        assert!(!caps.supports(ObjectKind::Table, props::COMMENT, &new));
        assert!(!caps.supports(ObjectKind::Sequence, props::NAME, &new));
        assert!(caps.supports(ObjectKind::Column, props::COLLATION, &new));
        assert!(caps.supports_kind(ObjectKind::Trigger, &new));
        assert!(!caps.supports_kind(ObjectKind::Function, &new));
        assert!(!caps.supports_kind(ObjectKind::Schema, &new));
    }
}
