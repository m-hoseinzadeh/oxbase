//! What a [`DdlGenerator`](crate::DdlGenerator) knows besides the definitions (M3).

use oxbase_types::{Rename, ServerVersion, TableDef, TriggerDef};

/// Context of one DDL generation.
///
/// - `version`: the server version (syntax and features differ by version).
/// - `table`: for child kinds (`Column`, `Index`, `ForeignKey`, `Check`) and for any change an
///   engine can only make by rebuilding the table (SQLite), the owning table **as it is now on
///   the server**. Generators that need it and do not get it fail with `InvalidInput`.
/// - `triggers`: the triggers defined on that table, with their full `definition`, for engines
///   whose table rebuild drops them (SQLite re-creates them after the rename). The designer
///   service loads them with the engine's `Trigger` fetcher; empty when the engine has none.
/// - `renames`: objects the designer renamed (`old` → `new`). A table `alter` renames first,
///   then compares and changes the renamed objects; a name that changed without a rename entry
///   is a drop plus an add.
/// - `exact_values` (M5): the table is made to hold data copied from elsewhere (an import's new
///   table, a SQL dump's `CREATE`). Columns without a native type then get a declaration that
///   keeps every value exactly where the engine's usual one would not, with a script warning
///   (SQLite: decimals of more than 15 digits as `DECIMAL_TEXT(p,s)`, which has `TEXT`
///   affinity; a `DECIMAL` column has `NUMERIC` affinity and keeps about 15 significant
///   digits). The designer leaves it off.
#[derive(Debug, Clone, Copy)]
pub struct DdlContext<'a> {
    pub version: &'a ServerVersion,
    pub table: Option<&'a TableDef>,
    pub triggers: &'a [TriggerDef],
    pub renames: &'a [Rename],
    pub exact_values: bool,
}

impl<'a> DdlContext<'a> {
    /// A context with only the server version.
    pub fn new(version: &'a ServerVersion) -> Self {
        Self {
            version,
            table: None,
            triggers: &[],
            renames: &[],
            exact_values: false,
        }
    }

    /// Builder: column types that keep copied values exactly (`exact_values`).
    #[must_use]
    pub fn with_exact_values(mut self) -> Self {
        self.exact_values = true;
        self
    }

    /// Builder: the owning table as it is now.
    #[must_use]
    pub fn with_table(mut self, table: &'a TableDef) -> Self {
        self.table = Some(table);
        self
    }

    /// Builder: the triggers on that table.
    #[must_use]
    pub fn with_triggers(mut self, triggers: &'a [TriggerDef]) -> Self {
        self.triggers = triggers;
        self
    }

    /// Builder: the designer's renames.
    #[must_use]
    pub fn with_renames(mut self, renames: &'a [Rename]) -> Self {
        self.renames = renames;
        self
    }
}
