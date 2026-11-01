//! The object-tree layout of an engine (M2): which kinds appear under which node, and how
//! databases relate to sessions.
//!
//! The layout is data, registered once per engine with
//! [`DialectRegistry::register_tree_layout`](crate::DialectRegistry::register_tree_layout).
//! `oxbase-services::tree` walks it generically: it never contains engine special cases. Rows of
//! a group are listed by the engine's [`Fetcher`](crate::Fetcher) for that kind, called with the
//! node as `parent`. Groups whose kind the server version does not support
//! ([`Capabilities::supports_kind`](crate::Capabilities::supports_kind)) are left out.
//!
//! # The tree
//!
//! ```text
//! connection (root)
//! └─ Database            direct group: one node per database, no folder
//!    └─ Schema           (PostgreSQL only) direct group
//!       ├─ Tables        folder group: a "Tables" node, its children are the tables
//!       │  └─ table
//!       │     ├─ Columns / Indexes / Foreign keys / Checks / Triggers   (folders)
//!       ├─ Views, Materialized views, Functions, Procedures, Sequences  (folders)
//! ```
//!
//! M2 layouts and the M4 SQL Server layout (each engine registers its own; these are the
//! contract):
//!
//! | Node kind | PostgreSQL | MySQL / MariaDB | SQLite | SQL Server (M4) |
//! |---|---|---|---|---|
//! | root | Database | Database | Database (`main`, `temp` when used, attached) | Database |
//! | Database | Schema | folders: Table, View, Function, Procedure, Sequence (MariaDB 10.3+) | folders: Table, View | Schema |
//! | Schema | folders: Table, View, MatView, Function, Procedure (11+), Sequence | - | - | folders: Table, View, Function, Procedure, Sequence, Type |
//! | Table | folders: Column, Index, ForeignKey, Check, Trigger | same | same | same |
//! | View | folders: Column, Trigger | folder: Column | folders: Column, Trigger | folders: Column, Index, Trigger |
//! | MatView | folders: Column, Index | - | - | - |
//!
//! Routines, sequences and the other leaves have no children.

use std::collections::{BTreeMap, BTreeSet};

use oxbase_types::ObjectKind;

/// How databases relate to sessions on an engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DatabaseScope {
    /// One session sees exactly one database (PostgreSQL, SQL Server). Objects of another
    /// database are read through a session opened on that database; `ObjectRef::catalog` names
    /// it.
    Session,
    /// One session sees every database, and a database is what other engines call a schema
    /// (MySQL, MariaDB). Fetchers filter by `ObjectRef::catalog` with a bound parameter.
    Catalog,
    /// One session sees `main`, `temp` and the databases attached to it (SQLite).
    /// `ObjectRef::catalog` is the schema name (`main`, `temp`, or the attach name).
    Attached,
}

/// One group of children under a tree node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChildGroup {
    pub kind: ObjectKind,
    /// `true`: the node shows a folder ("Tables") whose children are the objects. `false`: the
    /// objects appear directly under the node (databases under the connection, schemas under a
    /// PostgreSQL database).
    pub folder: bool,
}

impl ChildGroup {
    /// Objects of `kind` grouped in a folder.
    pub const fn folder(kind: ObjectKind) -> Self {
        Self { kind, folder: true }
    }

    /// Objects of `kind` listed directly under the node.
    pub const fn direct(kind: ObjectKind) -> Self {
        Self {
            kind,
            folder: false,
        }
    }
}

/// The object-tree layout of one engine. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeLayout {
    /// How databases relate to sessions.
    pub scope: DatabaseScope,
    /// Groups under the connection node.
    root: Vec<ChildGroup>,
    /// Groups under a node of the key's kind. Kinds without an entry have no children.
    children: BTreeMap<ObjectKind, Vec<ChildGroup>>,
}

impl TreeLayout {
    /// An empty layout with the given database scope.
    pub fn new(scope: DatabaseScope) -> Self {
        Self {
            scope,
            root: Vec::new(),
            children: BTreeMap::new(),
        }
    }

    /// Builder: the groups under the connection node, in display order.
    #[must_use]
    pub fn with_root(mut self, groups: &[ChildGroup]) -> Self {
        self.root = groups.to_vec();
        self
    }

    /// Builder: the groups under nodes of `parent`, in display order. Replaces earlier groups.
    #[must_use]
    pub fn with_children(mut self, parent: ObjectKind, groups: &[ChildGroup]) -> Self {
        if groups.is_empty() {
            self.children.remove(&parent);
        } else {
            self.children.insert(parent, groups.to_vec());
        }
        self
    }

    /// The groups under the connection node.
    pub fn root(&self) -> &[ChildGroup] {
        &self.root
    }

    /// The groups under a node of `parent` (empty for leaves).
    pub fn children(&self, parent: ObjectKind) -> &[ChildGroup] {
        self.children.get(&parent).map_or(&[][..], Vec::as_slice)
    }

    /// `true` if nodes of `kind` have child groups.
    pub fn is_expandable(&self, kind: ObjectKind) -> bool {
        self.children.contains_key(&kind)
    }

    /// Every kind that appears as a group anywhere in the layout. Each one needs a `Fetcher`
    /// (see `DialectRegistry::missing_layout_fetchers`).
    pub fn kinds(&self) -> BTreeSet<ObjectKind> {
        self.root
            .iter()
            .chain(self.children.values().flatten())
            .map(|g| g.kind)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ObjectKind as K;

    #[test]
    fn builds_and_answers() {
        let layout = TreeLayout::new(DatabaseScope::Catalog)
            .with_root(&[ChildGroup::direct(K::Database)])
            .with_children(
                K::Database,
                &[ChildGroup::folder(K::Table), ChildGroup::folder(K::View)],
            )
            .with_children(K::Table, &[ChildGroup::folder(K::Column)])
            .with_children(K::View, &[]);
        assert_eq!(layout.root(), &[ChildGroup::direct(K::Database)]);
        assert_eq!(layout.children(K::Database).len(), 2);
        assert_eq!(layout.children(K::Column), [] as [ChildGroup; 0]);
        assert!(layout.is_expandable(K::Table));
        assert!(!layout.is_expandable(K::View));
        assert_eq!(
            layout.kinds().into_iter().collect::<Vec<_>>(),
            [K::Database, K::Table, K::Column, K::View]
        );
        assert!(ChildGroup::folder(K::Table).folder);
        assert!(!ChildGroup::direct(K::Schema).folder);
    }
}
