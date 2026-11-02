//! Redis dialect cells (M7). This file (registration, tree layout, capabilities) is the Design
//! step's; the **redis** stream fills `fetch.rs`, `keys.rs` and `pubsub.rs`. Contract:
//! `docs/m7-contracts.md` §3.2, §5.2, §6.
//!
//! # Object tree
//!
//! ```text
//! connection
//! ├─ databases               0 .. CONFIG GET databases - 1 (a cluster: only 0); KEY_COUNT
//! │  │                       from INFO keyspace; empty databases are listed too
//! │  ├─ namespace "m7:"      Schema: key prefixes up to a separator (`namespace_separator`)
//! │  │  ├─ namespace "m7:ns:"
//! │  │  │  └─ key "m7:ns:a:1"
//! │  │  └─ key "m7:list"     Key: KEY_TYPE, TTL_MS
//! │  └─ key "plain"
//! └─ Users                   ACL users (ACL LIST; Redis 6+)
//! ```
//!
//! [`DatabaseScope::Session`]: a connection sits on one logical database (`SELECT`), so the
//! tree reads database `n` on a metadata child session opened with `ConnectParams::database =
//! n` (the services do this; no cell ever sends `SELECT`). Refs:
//!
//! - database: `ObjectRef::database("3")`;
//! - namespace: kind `Schema`, `catalog` = the database, `name` = the full prefix including
//!   the trailing separator (`m7:ns:`), `schema` = the enclosing namespace (`ObjectRef::child`
//!   of the parent ref); its children are the keys and sub-namespaces directly below it;
//! - key: kind `Key`, `catalog` = the database, `name` = the whole key (UTF-8; other bytes as
//!   `\xHH` with `props::KEY_HEX` holding the hex), `schema` = the enclosing namespace.
//!
//! Listing a database or namespace node `SCAN`s `MATCH <prefix>*` (glob characters of the
//! prefix escaped) with `COUNT 1000` up to `tree_scan_limit` keys, groups the keys by their
//! next separator, and returns sub-namespaces (`KEY_COUNT`) and the keys at this level
//! (`KEY_TYPE`, `TTL_MS` read in one pipeline), sorted by name, at most 1000 of each
//! (`TRUNCATED` on the parent's listing rows when cut). A cluster scans every master. `load`
//! of a key returns `ObjectDef::Generic` with extras `KEY_TYPE`, `TTL_MS`, `extras::LENGTH`,
//! `extras::ENCODING`, `extras::MEMORY_BYTES`; of a database `ObjectDef::Schema` with
//! `KEY_COUNT`; of a user `ObjectDef::Generic` with its ACL rule text (`ACL GETUSER`,
//! passwords never included: only `extras::ACL_FLAGS`, `extras::ACL_COMMANDS`,
//! `extras::ACL_KEYS`, `extras::ACL_CHANNELS`).

mod fetch;
mod keys;
mod pubsub;

use std::sync::Arc;

use oxbase_dialect::{Capabilities, ChildGroup, DatabaseScope, DialectRegistry, TreeLayout};
use oxbase_types::{Engine, ObjectKind, PropId, ServerVersion};

use crate::console::RedisConsole;

pub use fetch::RedisFetcher;
pub use keys::RedisKeyValueStore;
pub use pubsub::RedisPubSub;

/// Object kinds Redis has: logical databases, key namespaces (`Schema`), keys and ACL users.
pub const APPLICABLE: [ObjectKind; 4] = [
    ObjectKind::Database,
    ObjectKind::Schema,
    ObjectKind::Key,
    ObjectKind::User,
];

/// Registers every Redis dialect cell in `reg`: a fetcher per layout kind, the tree layout,
/// capabilities, the console language, the key-value store and pub/sub.
pub fn register(reg: &mut DialectRegistry) {
    let engine = Engine::Redis;
    for kind in APPLICABLE {
        reg.register_fetcher(engine, kind, Arc::new(RedisFetcher::new(kind)));
    }
    reg.register_tree_layout(engine, tree_layout());
    reg.register_capabilities(engine, Arc::new(RedisCapabilities));
    reg.register_script_language(engine, Arc::new(RedisConsole));
    reg.register_key_value_store(engine, Arc::new(RedisKeyValueStore));
    reg.register_pubsub(engine, Arc::new(RedisPubSub));
    reg.mark_not_applicable_except(engine, &APPLICABLE);
}

/// The Redis object tree (see the module docs).
pub fn tree_layout() -> TreeLayout {
    use ChildGroup as G;
    use ObjectKind as K;
    TreeLayout::new(DatabaseScope::Session)
        .with_root(&[G::direct(K::Database), G::folder(K::User)])
        .with_children(K::Database, &[G::direct(K::Schema), G::direct(K::Key)])
        .with_children(K::Schema, &[G::direct(K::Schema), G::direct(K::Key)])
        .with_label(K::Schema, "Namespace", "Namespaces")
}

/// What Redis supports per version: ACL users from 6.0; everything else always.
#[derive(Debug, Clone, Copy, Default)]
pub struct RedisCapabilities;

impl Capabilities for RedisCapabilities {
    /// No designer properties (Redis has no DDL).
    fn supports(&self, kind: ObjectKind, prop: PropId, v: &ServerVersion) -> bool {
        let _ = (kind, prop, v);
        false
    }

    fn supports_kind(&self, kind: ObjectKind, v: &ServerVersion) -> bool {
        match kind {
            ObjectKind::User => v.at_least(6, 0),
            other => APPLICABLE.contains(&other),
        }
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
        let engine = Engine::Redis;
        assert_eq!(reg.missing_m7_roles(engine), Vec::<String>::new());
        assert_eq!(
            reg.missing_layout_fetchers(engine),
            Vec::<ObjectKind>::new()
        );
        let layout = reg.tree_layout(engine).unwrap();
        assert_eq!(layout.scope, DatabaseScope::Session);
        assert_eq!(layout.folder_label(ObjectKind::User), "Users");
        assert_eq!(layout.kind_label(ObjectKind::Schema), "Namespace");
        assert!(layout.is_expandable(ObjectKind::Schema));
        assert!(!layout.is_expandable(ObjectKind::Key));
        assert_eq!(reg.script_language(engine).unwrap().name(), "redis-console");
        assert_eq!(
            reg.status(engine, ObjectKind::Table),
            CellStatus::NotApplicable
        );
        assert!(reg.quoting(engine).is_err());
        assert!(!reg.supports_kind(engine, ObjectKind::User, &ServerVersion::new(5, 0, 14)));
        assert!(reg.supports_kind(engine, ObjectKind::User, &ServerVersion::new(8, 10, 3)));
        assert!(!reg.supports_kind(engine, ObjectKind::View, &ServerVersion::new(8, 10, 3)));
    }
}
