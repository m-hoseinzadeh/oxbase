//! The [`DdlGenerator`] for tables, columns, indexes, foreign keys and checks (M4, **ddl**
//! stream). Contract: `docs/m4-contracts.md` §4 and the M3 `DdlGenerator` rules.
//!
//! Summary of the SQL Server rules:
//!
//! - Identifiers through [`oxbase_dialect::BracketQuoting`] (`[a]]b]`), names `[schema].[name]`
//!   (no database part: the designer runs on a session of the table's database). Literals
//!   (comments, literal defaults, `sp_rename` / extended-property arguments) through
//!   `quote_str` (`N'...'`). One step per statement; every step transactional (SQL Server DDL
//!   rolls back), so scripts are atomic.
//! - Renames: `EXEC sp_rename N'[s].[t]', N'new'` (table), `N'[s].[t].[c]', N'new', N'COLUMN'`,
//!   `N'[s].[t].[ix]', N'new', N'INDEX'`, `N'[s].[fk]', N'new', N'OBJECT'` (constraints).
//! - Defaults are named constraints: `CONSTRAINT [df] DEFAULT (<expr>) FOR [c]`; changing or
//!   dropping a default drops the constraint by its name ([`crate::extras::DEFAULT_CONSTRAINT`]
//!   of the old column) and adds a new one. New defaults are named `DF_<table>_<column>`.
//! - `ALTER TABLE ... ALTER COLUMN [c] <type> [COLLATE x] NULL|NOT NULL` always restates the
//!   nullability. Before it, drop what blocks it (default constraint unless only the length,
//!   precision or scale grows; indexes, unique / primary keys, foreign keys and checks that use
//!   the column; computed columns that reference it) and recreate them after, in the same
//!   script. A `rowversion` column and computed columns are never altered (drop + add).
//! - `IDENTITY` cannot be added to or removed from an existing column, nor its seed / increment
//!   changed: `Unsupported` with that reason. A new column may have it (one per table).
//! - Computed columns: `[c] AS (<expr>) [PERSISTED]`; a change is drop + add (destructive).
//! - Comments: `sys.sp_addextendedproperty` / `sp_updateextendedproperty` /
//!   `sp_dropextendedproperty` with `@name = N'MS_Description'` and level0/1/2 names as `N'...'`
//!   literals.
//! - Indexes: `CREATE [UNIQUE] [CLUSTERED|NONCLUSTERED] INDEX [ix] ON [s].[t] ([c] ASC|DESC, ...)
//!   [INCLUDE (...)] [WHERE <predicate>]`; unique constraints and primary keys through
//!   `ALTER TABLE ... ADD CONSTRAINT`; `DROP INDEX [ix] ON [s].[t]`. Columnstore, XML, spatial
//!   and hash indexes are kept as they are; changing one is `Unsupported`.
//! - Foreign keys: `NO ACTION`, `CASCADE`, `SET NULL`, `SET DEFAULT` (no `RESTRICT`).
//! - Reordering columns, temporal (system-versioned) and memory-optimized table changes:
//!   `Unsupported` in M4.

use oxbase_dialect::{DdlContext, DdlGenerator};
use oxbase_types::{ObjectDef, ObjectRef, Result, Script};

/// The DDL generator of every designer kind.
pub(crate) struct MsSqlDdl;

impl DdlGenerator for MsSqlDdl {
    fn create(&self, d: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        let _ = (d, ctx);
        unimplemented!("m4: ddl (create)")
    }

    fn alter(&self, old: &ObjectDef, new: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        let _ = (old, new, ctx);
        unimplemented!("m4: ddl (alter)")
    }

    fn drop_object(&self, r: &ObjectRef, ctx: &DdlContext<'_>) -> Result<Script> {
        let _ = (r, ctx);
        unimplemented!("m4: ddl (drop)")
    }
}
