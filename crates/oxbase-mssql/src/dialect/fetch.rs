//! Catalog [`Fetcher`] cells (M4, **fetch** stream). Contract: `docs/m4-contracts.md` §3 and the
//! [`crate::dialect`] module docs.
//!
//! Rules on top of the M2 `Fetcher` rules (bound parameters only, sorted output, `system`
//! flags, `Cancelled` with nothing partial, `NotFound` from `load`):
//!
//! - One session reads one database ([`oxbase_dialect::DatabaseScope::Session`]): queries use
//!   the session database's catalog views (`sys.schemas`, `sys.objects`, `sys.columns`, ...) and
//!   never a three-part name. A ref whose `catalog` is not
//!   [`Connection::current_database`](oxbase_driver::Connection::current_database) is
//!   `InvalidInput`. The `Database` listing reads `sys.databases` (server-wide).
//! - Names are bound as `@P1..@Pn` (`nvarchar`). Object ids are looked up with
//!   `OBJECT_ID(QUOTENAME(@P1) + N'.' + QUOTENAME(@P2))` or by joining `sys.objects` on schema id
//!   and name, never by formatting names into SQL text.
//! - System objects: databases `master`, `model`, `msdb`, `tempdb` (and `database_id <= 4`);
//!   schemas `sys`, `INFORMATION_SCHEMA`, `guest` and the fixed database-role schemas
//!   (`db_owner`, `db_datareader`, ..., `schema_id >= 16384`); objects with `is_ms_shipped = 1`.
//!   Listed with `system = true`.
//! - Definitions of views, routines and triggers come from `sys.sql_modules.definition`
//!   (`OBJECT_DEFINITION`), unchanged; `NULL` (`WITH ENCRYPTION`) sets
//!   [`crate::extras::ENCRYPTED`].
//! - Comments: the `MS_Description` extended property (`sys.extended_properties`, `class` 1 for
//!   objects and columns, 3 for schemas, 0 for the database).
//! - Defaults and check expressions are the server's normalized text (`((0))`, `([qty]>=(0))`);
//!   the comparator rules (ddl stream) fold the parentheses.

use async_trait::async_trait;
use oxbase_dialect::Fetcher;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{ObjectDef, ObjectKind, ObjectRef, ObjectSummary, Result};

/// The kinds with a fetcher: everything in the tree layout.
pub(crate) const KINDS: [ObjectKind; 13] = [
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
    ObjectKind::Type,
];

/// The fetcher of one object kind.
pub(crate) struct MsSqlFetcher {
    kind: ObjectKind,
}

impl MsSqlFetcher {
    pub(crate) const fn new(kind: ObjectKind) -> Self {
        Self { kind }
    }
}

#[async_trait]
impl Fetcher for MsSqlFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let _ = (c, parent, cancel);
        unimplemented!("m4: fetch ({} list)", self.kind)
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        let _ = (c, obj, cancel);
        unimplemented!("m4: fetch ({} load)", self.kind)
    }
}
