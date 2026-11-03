//! Structure compare and sync, data compare and sync (M8). Contract: `docs/m8-contracts.md` §5
//! (structure) and §6 (data).
//!
//! Module map:
//!
//! - [`structure`]: [`SchemaSnapshot`] (every object of a container through the engine's
//!   fetchers), [`compare`] (the engine's `Comparator` cells) → [`StructureDiff`] of
//!   [`DiffItem`]s.
//! - [`script`]: [`sync_script`], the dependency-ordered DDL that makes the target look like the
//!   source ([`SyncScript`]), from the engine's `DdlGenerator` cells.
//! - [`retarget`]: [`retarget_sql`], names inside SQL text moved from the source container to the
//!   target.
//! - [`data`]: key resolution, the chunked key-ordered table compare ([`data::compare_table`]),
//!   DML through the engine's `DmlWriter` ([`data::data_statements`]).
//! - [`store`]: [`DiffStore`], the differing rows spilled to Arrow IPC files.
//!
//! Both syncs compare two connections of the **same engine** (PostgreSQL, MySQL, MariaDB, SQL
//! Server, SQLite). Sessions, the apply (`oxbase_services::designer::execute_script` for
//! structure, batched transactions for data), re-compares, events and cancellation live in
//! `oxbase_services::sync`. This crate never opens a connection itself.

pub mod data;
pub mod retarget;
pub mod script;
pub mod store;
pub mod structure;

pub use data::{
    CompareProgress, DataApplyOptions, DataCompareOptions, DataScript, DataScriptOptions,
    KeyChoice, RowCounts, RowSample, TableCompareStatus, TableDataDiff, TablePair,
};
pub use retarget::retarget_sql;
pub use script::{SyncScript, sync_script};
pub use store::{DiffStore, RowOp};
pub use structure::{
    DiffCounts, DiffItem, DiffStatus, SchemaSnapshot, SkippedObject, StructureDiff,
    StructureOptions, compare, item_id, load_snapshot,
};
