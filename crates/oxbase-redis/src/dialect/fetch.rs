//! Redis catalog fetchers (redis stream): databases, key namespaces, keys and ACL users, as
//! described in the [`super`] module docs. Every command goes through the connection the tree
//! hands in (a session on the node's database); fetchers reach the concrete
//! [`RedisConnection`](crate::RedisConnection) with `Connection::as_any` for its options
//! (namespace separator, tree scan limit) and, on a cluster, its masters. Never `KEYS`.

use async_trait::async_trait;
use oxbase_dialect::Fetcher;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{ObjectDef, ObjectKind, ObjectRef, ObjectSummary, Result};

/// The fetcher of one Redis object kind (`Database`, `Schema`, `Key` or `User`).
#[derive(Debug, Clone, Copy)]
pub struct RedisFetcher {
    kind: ObjectKind,
}

impl RedisFetcher {
    pub fn new(kind: ObjectKind) -> Self {
        Self { kind }
    }

    /// The kind this fetcher lists.
    pub fn kind(&self) -> ObjectKind {
        self.kind
    }
}

#[async_trait]
impl Fetcher for RedisFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let _ = (c, parent, cancel);
        unimplemented!("m7: redis")
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        let _ = (c, obj, cancel);
        unimplemented!("m7: redis")
    }
}
