//! Per-engine dialect traits: [`Fetcher`], [`DdlGenerator`], [`Comparator`], [`Capabilities`],
//! [`DmlWriter`] and [`Quoting`].
//!
//! Each engine fills an object-kind x role matrix of implementations of these traits (PLAN §1,
//! rule 3). [`DialectRegistry`] holds that matrix and reports which cells are still missing, so
//! the conformance harness can print the object-kind x engine grid.
//!
//! Roles per (engine, object kind): [`Fetcher`], [`DdlGenerator`], [`Comparator`].
//! Roles per engine: [`Capabilities`], [`Quoting`], [`DmlWriter`].

mod quoting;
mod registry;

use async_trait::async_trait;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    ObjectDef, ObjectKind, ObjectRef, ObjectSummary, PropId, PropertyDiff, Result, RowEdit, RowKey,
    Script, ServerVersion,
};

pub use quoting::{AnsiQuoting, BacktickQuoting, BracketQuoting, PgQuoting, Quoting};
pub use registry::{
    CellRole, CellStatus, CoverageMatrix, DialectRegistry, EngineRole, MissingCell,
};

/// Reads objects of one kind from a live connection. One implementation per (engine, kind).
///
/// Both methods can be long-running (a catalog with thousands of tables), so both take a
/// [`CancellationToken`] (PLAN §1, rule 4). Implementations pass it on to every
/// [`Connection::execute`] they make and check it between round trips; once it is cancelled they
/// return `Err(Error::Cancelled)` and no partial result.
#[async_trait]
pub trait Fetcher: Send + Sync {
    /// Lists the objects of this fetcher's kind directly under `parent` (for example the tables
    /// of a schema). The parent of a `Database` listing is [`ObjectRef::root`].
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>>;

    /// Loads the full typed definition of `obj`. Fails with `Error::NotFound` if it is gone.
    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef>;
}

/// Produces DDL for one object kind. One implementation per (engine, kind).
///
/// Generation does not fail: properties the engine or version cannot express are skipped and
/// reported in [`Script::warnings`].
pub trait DdlGenerator: Send + Sync {
    /// `CREATE` script for `d`.
    fn create(&self, d: &ObjectDef) -> Script;

    /// Script that turns `old` into `new` (in place where possible, else drop and re-create with
    /// destructive statements flagged).
    fn alter(&self, old: &ObjectDef, new: &ObjectDef) -> Script;

    /// `DROP` script for `r`.
    ///
    /// Named `drop_object` rather than `drop` so that calling it on an `Arc<dyn DdlGenerator>`
    /// or `Box<dyn DdlGenerator>` does not resolve to `Drop::drop`.
    fn drop_object(&self, r: &ObjectRef) -> Script;
}

/// Compares two definitions of the same kind. One implementation per (engine, kind), so engine
/// normalisation (default expression spelling, implicit collations, ...) stays out of the diff.
pub trait Comparator: Send + Sync {
    /// Differences going from `a` (source / old) to `b` (target / new). Empty when equivalent.
    fn diff(&self, a: &ObjectDef, b: &ObjectDef) -> Vec<PropertyDiff>;
}

/// Answers what an engine supports at a given server version. One implementation per engine.
pub trait Capabilities: Send + Sync {
    /// `true` if property `prop` of objects of `kind` is supported by server version `v`.
    fn supports(&self, kind: ObjectKind, prop: PropId, v: &ServerVersion) -> bool;

    /// `true` if objects of `kind` exist at all on server version `v` (SQLite has no users,
    /// MySQL has no sequences, ...). Defaults to `true`.
    fn supports_kind(&self, kind: ObjectKind, v: &ServerVersion) -> bool {
        let _ = (kind, v);
        true
    }
}

/// Turns grid edits into DML (grid write-back). One implementation per engine.
pub trait DmlWriter: Send + Sync {
    /// One statement per edit, in order, addressing rows by `key`. Callers validate each edit with
    /// [`RowKey::validate`] first.
    fn apply(&self, edits: &[RowEdit], key: &RowKey) -> Script;
}
