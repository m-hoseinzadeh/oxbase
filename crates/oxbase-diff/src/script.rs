//! Structure sync scripts (`docs/m8-contracts.md` §5.4): the DDL that makes the target look like
//! the source for the selected [`DiffItem`](crate::DiffItem)s, in dependency order. Owner of the
//! bodies marked `unimplemented!("m8: structure-sync")`: the **structure-sync** stream.
//!
//! # Steps
//!
//! Every statement comes from the engine's `DdlGenerator` cells for the item's kind
//! (`create` for `only_in_source`, `alter(target_def, source_def)` for `different`,
//! `drop_object` for `only_in_target` when drops are included), with a `DdlContext` holding the
//! server version, the table as it is now on the target and its triggers (the SQLite rebuild
//! path needs both), and `exact_values` off. A kind whose generator cannot alter in place
//! (views, routines, triggers on most engines) is dropped and re-created, or replaced
//! (`CREATE OR REPLACE`, `CREATE OR ALTER`) where the engine has it; that choice is the cell's.
//!
//! # Order
//!
//! 1. Drops of dependents of anything that changes or goes: triggers, then views and
//!    materialized views (reverse dependency order), then routines that are dropped or
//!    re-created.
//! 2. Foreign keys that change or go (on tables that stay), and foreign keys that point at
//!    tables about to be dropped.
//! 3. Drops of tables only in the target (`include_drops`), children before parents.
//! 4. Sequences to create or alter.
//! 5. Tables to create (without their foreign keys), parents first; then alters of the
//!    different tables (without foreign-key additions). The SQLite rebuild steps of one table
//!    stay together, with their prepare / finally phases.
//! 6. Foreign keys to add (new tables' and changed ones), after every table exists.
//! 7. Routines, then views and materialized views (dependency order: a view that reads
//!    another view comes after it), then triggers.
//!
//! Dependencies between views and routines come from a token scan of their SQL for the names of
//! compared objects (`oxbase_sql` tokens; the structure-sync stream) fed to
//! `oxbase_dialect::DependencyGraph`; tables come from foreign keys
//! (`oxbase_dialect::table_dependencies`).
//!
//! # Flags
//!
//! [`SyncScript::destructive`] when any step is (drops, narrowing changes, rebuilds);
//! [`SyncScript::atomic`] when every main step is transactional (`Script::is_atomic`) and the
//! engine runs DDL in transactions (PostgreSQL, SQL Server, SQLite; MySQL / MariaDB DDL commits
//! each statement, and the preview says so: [`SyncScript::transactional_ddl`] false).

use oxbase_dialect::DialectRegistry;
use oxbase_types::{Engine, Result, Script};
use serde::{Deserialize, Serialize};

use crate::structure::{SchemaSnapshot, StructureDiff, StructureOptions};

/// A generated structure sync script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncScript {
    pub engine: Engine,
    /// The steps, in execution order of the module docs (phases inside).
    pub script: Script,
    /// The whole script as text, statements ended with the engine's terminator
    /// (`Quoting::script_terminator`), for the preview and for saving.
    pub sql: String,
    /// The item ids the script covers, in step order of their first step.
    pub items: Vec<String>,
    pub destructive: bool,
    /// Runs as one transaction (module docs, *Flags*).
    pub atomic: bool,
    /// The engine runs DDL inside transactions.
    pub transactional_ddl: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl SyncScript {
    /// `true` when there is nothing to run.
    pub fn is_empty(&self) -> bool {
        self.script.is_empty()
    }
}

/// The script for the items `selection` of `diff` (empty selection: every item that is not
/// `same`, and `only_in_target` items only with `options.include_drops`). `source` and
/// `target` are the snapshots `diff` was computed from. `InvalidInput` for an unknown id;
/// `Unsupported` (naming the object) when a needed generator is missing.
pub fn sync_script(
    dialects: &DialectRegistry,
    diff: &StructureDiff,
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
    selection: &[String],
    options: &StructureOptions,
) -> Result<SyncScript> {
    let _ = (dialects, diff, source, target, selection, options);
    unimplemented!("m8: structure-sync")
}
