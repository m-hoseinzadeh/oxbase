//! The object tree (M2): children of a node and object definitions, through the dialect
//! registry. No engine special cases: the shape comes from the engine's
//! [`TreeLayout`](oxbase_dialect::TreeLayout), the rows from its fetchers.
//!
//! # Children of a node
//!
//! [`TreeService::children`] answers, for the engine of the session:
//!
//! - [`TreeNodeRef::Connection`]: the layout's root groups (M2: the databases).
//! - [`TreeNodeRef::Object`]: the groups of `layout.children(object.kind)`.
//! - [`TreeNodeRef::Folder`]: the objects of `kind` under `parent`, from
//!   `Fetcher::list(conn, parent)`.
//!
//! For each group: a *folder* group becomes one [`TreeNodeRef::Folder`] node (no query); a
//! *direct* group is listed right away with the kind's fetcher. Groups whose kind the server
//! version does not support (`DialectRegistry::supports_kind`) are left out. Objects with
//! `system = true` are left out unless [`TreeOptions::show_system`]. Labels: the object's
//! [`ObjectRef::display_name`], the folder's [`ObjectKind::plural_display_name`]. A node is
//! `expandable` when its kind has groups in the layout (folders always are).
//!
//! # Which session reads a node
//!
//! Every node is read on the session the caller names (a base session) except on engines with
//! [`DatabaseScope::Session`](oxbase_dialect::DatabaseScope::Session) (PostgreSQL): a node whose
//! [`ObjectRef::database_name`] differs from the base session's
//! [`SessionHandle::current_database`] is read on a child session opened on that database
//! ([`TreeService::session_for`]). The service keeps one such *metadata child* per (base,
//! database), reuses it, and closes them all in [`TreeService::forget`] (call it when the base
//! closes; closing the base also closes them). Reads hold [`SessionHandle::exclusive`].
//!
//! Owner of the bodies: the **services** stream (`docs/m2-contracts.md`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use oxbase_dialect::DialectRegistry;
use oxbase_types::{Error, ErrorCategory, ObjectDef, ObjectKind, ObjectRef, ObjectSummary, Result};

use crate::{CancellationToken, ConnectionManager, SessionHandle, SessionId};

/// Addresses one node of the object tree. Sent by the UI to `tree_children`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TreeNodeRef {
    /// The connection itself (the root).
    Connection,
    /// A folder grouping the objects of `kind` under `parent` ("Tables" of schema `public`).
    /// For a folder directly under the connection, `parent` is [`ObjectRef::root`].
    Folder { parent: ObjectRef, kind: ObjectKind },
    /// One database object.
    Object { object: ObjectRef },
}

impl TreeNodeRef {
    /// A stable key for this node (for UI caches and React keys): `connection`,
    /// `folder:<kind>:<parent>` or `object:<ref>`, built from the `Display` forms.
    pub fn key(&self) -> String {
        match self {
            Self::Connection => "connection".to_owned(),
            Self::Folder { parent, kind } => format!("folder:{kind}:{parent}"),
            Self::Object { object } => format!("object:{object}"),
        }
    }
}

/// One node returned by `tree_children`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeNode {
    pub node: TreeNodeRef,
    /// [`TreeNodeRef::key`] of `node`.
    pub key: String,
    /// What the tree shows.
    pub label: String,
    /// `true` when the node can have children.
    pub expandable: bool,
    /// For object nodes: the listing row (comment, row estimate, system flag, extras).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<ObjectSummary>,
}

impl TreeNode {
    /// A folder node for the objects of `kind` under `parent`.
    pub fn folder(parent: ObjectRef, kind: ObjectKind) -> Self {
        let node = TreeNodeRef::Folder { parent, kind };
        Self {
            key: node.key(),
            node,
            label: kind.plural_display_name().to_owned(),
            expandable: true,
            summary: None,
        }
    }

    /// An object node from a fetcher's listing row.
    pub fn object(summary: ObjectSummary, expandable: bool) -> Self {
        let node = TreeNodeRef::Object {
            object: summary.obj.clone(),
        };
        Self {
            key: node.key(),
            label: summary.obj.display_name(),
            node,
            expandable,
            summary: Some(summary),
        }
    }
}

/// Options of a `tree_children` call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TreeOptions {
    /// Also list system objects (`pg_catalog`, `information_schema`, MySQL `mysql`/`sys`, ...).
    pub show_system: bool,
}

/// Lists tree children and loads object definitions. See the module docs.
pub struct TreeService {
    manager: Arc<ConnectionManager>,
    dialects: Arc<DialectRegistry>,
    /// Metadata children per (base session, database), for `DatabaseScope::Session` engines.
    metadata: Mutex<HashMap<(SessionId, String), SessionHandle>>,
}

impl TreeService {
    pub fn new(manager: Arc<ConnectionManager>, dialects: Arc<DialectRegistry>) -> Self {
        Self {
            manager,
            dialects,
            metadata: Mutex::new(HashMap::new()),
        }
    }

    pub fn manager(&self) -> &Arc<ConnectionManager> {
        &self.manager
    }

    pub fn dialects(&self) -> &Arc<DialectRegistry> {
        &self.dialects
    }

    /// The children of `node` in the tree of the base session `session`. Cancelled calls
    /// return `Error::Cancelled` and nothing else. An unknown session is `NotFound`; an engine
    /// without a tree layout or a fetcher for a listed kind is `Unsupported`.
    #[allow(clippy::unused_async)] // m2: services fills the body
    pub async fn children(
        &self,
        session: SessionId,
        node: &TreeNodeRef,
        options: &TreeOptions,
        cancel: CancellationToken,
    ) -> Result<Vec<TreeNode>> {
        let _ = (self, session, node, options, cancel);
        unimplemented!("m2: services")
    }

    /// The full definition of `object` (`Fetcher::load` of its kind), read on the session
    /// [`TreeService::session_for`] picks.
    #[allow(clippy::unused_async)] // m2: services fills the body
    pub async fn load(
        &self,
        session: SessionId,
        object: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        let _ = (self, session, object, cancel);
        unimplemented!("m2: services")
    }

    /// The session that reads objects of `database` for the base session `base`: the base
    /// itself, or (on `DatabaseScope::Session` engines, for another database) a cached metadata
    /// child. See the module docs.
    #[allow(clippy::unused_async)] // m2: services fills the body
    pub async fn session_for(
        &self,
        base: SessionId,
        database: Option<&str>,
        cancel: CancellationToken,
    ) -> Result<SessionHandle> {
        let _ = (self, base, database, cancel);
        unimplemented!("m2: services")
    }

    /// Closes and forgets the metadata children of `base`. Unknown bases are fine.
    pub async fn forget(&self, base: SessionId) -> Result<()> {
        let children: Vec<SessionHandle> = {
            let mut metadata = self
                .metadata
                .lock()
                .map_err(|_| Error::internal("tree session table lock poisoned"))?;
            let keys: Vec<_> = metadata
                .keys()
                .filter(|(b, _)| *b == base)
                .cloned()
                .collect();
            keys.iter().filter_map(|k| metadata.remove(k)).collect()
        };
        for child in children {
            // Already gone when the base was closed first: fine.
            match self.manager.close(child.id()).await {
                Ok(()) => {}
                Err(e) if e.category() == ErrorCategory::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl std::fmt::Debug for TreeService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeService")
            .field("dialects", &self.dialects)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn node_refs_serialize_with_a_type_tag() {
        let schema = ObjectRef::schema(Some("oxbase_test"), "m2");
        let folder = TreeNodeRef::Folder {
            parent: schema.clone(),
            kind: ObjectKind::Table,
        };
        let json = serde_json::to_value(&folder).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "folder",
                "parent": {"kind": "schema", "catalog": "oxbase_test", "name": "m2"},
                "kind": "table"
            })
        );
        assert_eq!(serde_json::from_value::<TreeNodeRef>(json).unwrap(), folder);
        assert_eq!(
            serde_json::to_value(TreeNodeRef::Connection).unwrap(),
            serde_json::json!({"type": "connection"})
        );
        let obj = TreeNodeRef::Object {
            object: schema.child(ObjectKind::Table, "m2_parent"),
        };
        assert_eq!(
            serde_json::to_value(&obj).unwrap()["object"]["name"],
            "m2_parent"
        );
        assert_eq!(folder.key(), "folder:table:schema:oxbase_test.m2");
        assert_eq!(obj.key(), "object:table:oxbase_test.m2.m2_parent");
    }

    #[test]
    fn nodes_carry_labels_and_keys() {
        let schema = ObjectRef::schema(Some("db"), "public");
        let f = TreeNode::folder(schema.clone(), ObjectKind::MatView);
        assert_eq!(f.label, "Materialized views");
        assert!(f.expandable);
        let fun = ObjectSummary::new(
            schema
                .child(ObjectKind::Function, "m2_fn")
                .with_signature("integer"),
        );
        let n = TreeNode::object(fun, false);
        assert_eq!(n.label, "m2_fn(integer)");
        let json = serde_json::to_value(&n).unwrap();
        assert_eq!(json["expandable"], false);
        assert_eq!(json["summary"]["ref"]["signature"], "integer");
        let opts: TreeOptions = serde_json::from_str("{}").unwrap();
        assert!(!opts.show_system);
    }
}
