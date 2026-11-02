//! MongoDB catalog fetchers (mongo stream), as described in the [`super`] module docs. They
//! reach the client with `Connection::as_any` → [`MongoConnection`](crate::MongoConnection)
//! and run commands with the connection's operation comment, honouring the token between
//! round trips (`Error::Cancelled`, nothing partial).

use async_trait::async_trait;
use oxbase_dialect::Fetcher;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{ObjectDef, ObjectKind, ObjectRef, ObjectSummary, Result};

/// The fetcher of one MongoDB object kind.
#[derive(Debug, Clone, Copy)]
pub struct MongoFetcher {
    kind: ObjectKind,
}

impl MongoFetcher {
    pub fn new(kind: ObjectKind) -> Self {
        Self { kind }
    }

    /// The kind this fetcher lists.
    pub fn kind(&self) -> ObjectKind {
        self.kind
    }
}

#[async_trait]
impl Fetcher for MongoFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let _ = (c, parent, cancel);
        unimplemented!("m7: mongo")
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        let _ = (c, obj, cancel);
        unimplemented!("m7: mongo")
    }
}
