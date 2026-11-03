//! MongoDB dialect cells (M7). This file (registration, tree layout, capabilities) is the
//! Design step's; the **mongo** stream fills `fetch.rs`, `documents.rs` and `bulk.rs`.
//! Contract: `docs/m7-contracts.md` §3.1, §5.1, §5.3.
//!
//! # Object tree
//!
//! ```text
//! connection
//! └─ databases                  listDatabases (nameOnly: false for sizes; `admin`, `config`,
//!    │                          `local` flagged system)
//!    ├─ Collections             listCollections type collection (and timeseries); system.*
//!    │  └─ collection           flagged system; DOCUMENT_COUNT from estimatedDocumentCount
//!    │     └─ Indexes           listIndexes (the `_id_` index flagged PRIMARY_KEY)
//!    ├─ Views                   listCollections type view
//!    ├─ Users                   usersInfo of that database (no credentials)
//!    └─ Roles                   rolesInfo of that database (user-defined; built-in flagged
//!                               system)
//! ```
//!
//! [`DatabaseScope::Catalog`]: one client sees every database; `ObjectRef::catalog` is the
//! database. A collection is `ObjectKind::Table` (labelled "Collection" in this tree), a view
//! `View`, an index an `Index` child of its collection. Names are never interpolated into code:
//! commands are BSON documents with the names as values.
//!
//! `load`:
//!
//! - collection → `ObjectDef::Table`: columns inferred from a `$sample` of
//!   `sample_size` documents (name, `DataType` from the BSON type, `NATIVE_TYPE` = the BSON
//!   type or `mixed`, nullable when some document lacks the field or has null), primary key
//!   `_id`, indexes from `listIndexes` (`IndexDef` with key columns, unique, sparse in
//!   `extras::SPARSE`, partial filter as `predicate` text, TTL in `extras::EXPIRE_AFTER_SECONDS`),
//!   extras `extras::CAPPED`, `extras::VALIDATOR` (JSON), `extras::OPTIONS` (the whole
//!   `listCollections` options as JSON), `DOCUMENT_COUNT`, `SIZE_BYTES`;
//! - view → `ObjectDef::View` with `definition` = the pipeline as canonical Extended JSON and
//!   `extras::VIEW_ON`;
//! - index → `ObjectDef::Index`;
//! - database → `ObjectDef::Schema` with `SIZE_BYTES`;
//! - user / role → `ObjectDef::Generic` with `extras::ROLES` (JSON) and, for roles,
//!   `extras::PRIVILEGES` (JSON); never credentials or SCRAM data.

mod bulk;
mod documents;
mod fetch;

use std::sync::Arc;

use oxbase_dialect::{Capabilities, ChildGroup, DatabaseScope, DialectRegistry, TreeLayout};
use oxbase_types::{Engine, ObjectKind, PropId, ServerVersion};

use crate::shell::MongoShell;

pub use bulk::MongoBulkLoader;
pub use documents::MongoDocumentStore;
pub use fetch::MongoFetcher;

/// Object kinds MongoDB has in the tree: databases, collections (`Table`), views, indexes,
/// users and roles.
pub const APPLICABLE: [ObjectKind; 6] = [
    ObjectKind::Database,
    ObjectKind::Table,
    ObjectKind::View,
    ObjectKind::Index,
    ObjectKind::User,
    ObjectKind::Role,
];

/// Registers every MongoDB dialect cell in `reg`: a fetcher per layout kind, the tree layout,
/// capabilities, the shell language, the document store and the import loader.
pub fn register(reg: &mut DialectRegistry) {
    let engine = Engine::MongoDb;
    for kind in APPLICABLE {
        reg.register_fetcher(engine, kind, Arc::new(MongoFetcher::new(kind)));
    }
    reg.register_tree_layout(engine, tree_layout());
    reg.register_capabilities(engine, Arc::new(MongoCapabilities));
    reg.register_script_language(engine, Arc::new(MongoShell));
    reg.register_document_store(engine, Arc::new(MongoDocumentStore));
    reg.register_bulk_loader(engine, Arc::new(MongoBulkLoader));
    reg.mark_not_applicable_except(engine, &APPLICABLE);
}

/// The MongoDB object tree (see the module docs).
pub fn tree_layout() -> TreeLayout {
    use ChildGroup as G;
    use ObjectKind as K;
    TreeLayout::new(DatabaseScope::Catalog)
        .with_root(&[G::direct(K::Database)])
        .with_children(
            K::Database,
            &[
                G::folder(K::Table),
                G::folder(K::View),
                G::folder(K::User),
                G::folder(K::Role),
            ],
        )
        .with_children(K::Table, &[G::folder(K::Index)])
        .with_label(K::Table, "Collection", "Collections")
}

/// What MongoDB supports per version: every applicable kind on every supported version
/// (MongoDB 5.0+; CI runs 7.0).
#[derive(Debug, Clone, Copy, Default)]
pub struct MongoCapabilities;

impl Capabilities for MongoCapabilities {
    /// No designer properties (collections have no DDL designer in M7).
    fn supports(&self, kind: ObjectKind, prop: PropId, v: &ServerVersion) -> bool {
        let _ = (kind, prop, v);
        false
    }

    fn supports_kind(&self, kind: ObjectKind, v: &ServerVersion) -> bool {
        let _ = v;
        APPLICABLE.contains(&kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_dialect::CellStatus;

    #[test]
    fn registers_every_m7_role() {
        let mut reg = DialectRegistry::new();
        register(&mut reg);
        let engine = Engine::MongoDb;
        assert_eq!(reg.missing_m7_roles(engine), Vec::<String>::new());
        assert_eq!(
            reg.missing_layout_fetchers(engine),
            Vec::<ObjectKind>::new()
        );
        let layout = reg.tree_layout(engine).unwrap();
        assert_eq!(layout.scope, DatabaseScope::Catalog);
        assert_eq!(layout.folder_label(ObjectKind::Table), "Collections");
        assert_eq!(layout.folder_label(ObjectKind::View), "Views");
        assert_eq!(layout.children(ObjectKind::Database).len(), 4);
        assert!(layout.is_expandable(ObjectKind::Table));
        assert!(!layout.is_expandable(ObjectKind::View));
        assert_eq!(reg.script_language(engine).unwrap().name(), "mongo-shell");
        assert!(reg.document_store(engine).is_ok());
        assert!(reg.has_bulk_loader(engine));
        assert_eq!(
            reg.status(engine, ObjectKind::Column),
            CellStatus::NotApplicable
        );
        assert!(reg.dml_writer(engine).is_err());
        assert!(!reg.supports_kind(engine, ObjectKind::Trigger, &ServerVersion::new(7, 0, 45)));
    }
}
