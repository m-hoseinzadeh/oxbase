//! The grid [`DmlWriter`] (M4, **ddl** stream). Contract: `docs/m4-contracts.md` §4.5 and the M3
//! `DmlWriter` rules (bound values, key-only match, read-back).
//!
//! Every plan is one batch with `returning = true` whose single result set has exactly one row
//! per affected row, so the edit service's "exactly one row" count is right whatever triggers
//! do, `SET NOCOUNT` says, or `@@ROWCOUNT` reports:
//!
//! ```sql
//! SET NOCOUNT ON;
//! DECLARE @k TABLE ([id] int);                      -- key columns, their native types
//! UPDATE [s].[t] SET [name] = @P1
//!   OUTPUT INSERTED.[id] INTO @k                    -- INTO: allowed on tables with triggers
//!   WHERE [id] = @P2;
//! SELECT t.[id], t.[name], ... FROM [s].[t] AS t    -- read-back after triggers ran
//!   JOIN @k AS k ON t.[id] = k.[id];                -- (SELECT k.* FROM @k without read_back)
//! SET NOCOUNT OFF;                                  -- a batch without parameters runs in
//!                                                   -- the session's own scope
//! ```
//!
//! - `INSERT ... OUTPUT INSERTED.<key> INTO @k VALUES (...)` / `DEFAULT VALUES` gets generated
//!   keys (identity, `NEWID()` / `NEWSEQUENTIALID()` defaults) without `SCOPE_IDENTITY()`.
//! - `DELETE ... OUTPUT DELETED.<key> INTO @k WHERE <key>; SELECT k.* FROM @k`.
//! - An update that changes key values reads back by `INSERTED` (the new values).
//! - Never written: identity columns (`IdentityDef::always`), computed columns,
//!   [`crate::extras::ROW_VERSION`] and [`crate::extras::PERIOD`] columns (`InvalidInput`
//!   naming the column).
//! - `Value::Null` is written as the keyword `NULL`, not a parameter (an `nvarchar` NULL
//!   parameter does not convert to `varbinary`, `image` or CLR types).
//! - Key columns of type `text`, `ntext`, `image`, `xml`, CLR types or `(max)` types cannot be
//!   compared or declared in `@k`: `Unsupported` ("this key cannot identify rows on SQL Server").
//! - Every table is transactional (`transactional()` keeps the default `true`).

use oxbase_dialect::{DmlPlan, DmlTarget, DmlWriter};
use oxbase_types::{Result, RowEdit};

/// The SQL Server grid writer.
pub(crate) struct MsSqlDmlWriter;

impl DmlWriter for MsSqlDmlWriter {
    fn plan(&self, edit: &RowEdit, target: &DmlTarget<'_>) -> Result<DmlPlan> {
        let _ = (edit, target);
        unimplemented!("m4: ddl (dml)")
    }
}
