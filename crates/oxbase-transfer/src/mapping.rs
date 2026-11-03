//! Column and table mapping across engines (`docs/m8-contracts.md` §4.1, §4.3): the pure part
//! of a transfer plan. Owner of the bodies marked `unimplemented!("m8: transfer")`: the
//! **transfer** stream.
//!
//! # Rules
//!
//! - **Same engine** (source and target engine equal): every column keeps its native type
//!   ([`TypeMapper::same_engine`]), fidelity `exact`; the target table is the source
//!   [`TableDef`] relocated to the target container (`oxbase_types::relocate_def`), with
//!   defaults, collations, generated columns, checks, comments and extras kept.
//! - **Across engines**: logical type = the source engine's [`TypeMapper::logical`]; target =
//!   the target engine's [`TypeMapper::map`]. The target `TableDef` keeps names, nullability,
//!   the primary key, unique and plain indexes on plain columns (when
//!   `TransferOptions::create_indexes`), and auto-increment / identity (as the target's
//!   auto-increment); it **drops** engine-native text that would not parse on the target
//!   (defaults other than numeric / string / boolean literals, collations, generated
//!   expressions, checks, expression and partial indexes, extras), each with a plan warning.
//! - **Overrides** ([`ColumnOverride`]) win: `skip`, `target_name`, `data_type` (mapped with
//!   the target's `map` for its native text unless `native_type` is given). An overridden
//!   column's fidelity is computed against its source logical type with [`fidelity_between`].
//! - **Foreign keys** between transferred tables are planned separately
//!   ([`TablePlan::foreign_keys`]) and added after every table is loaded; a foreign key to a
//!   table outside the transfer is dropped with a warning.
//! - **Append / truncate** into an existing table: columns are matched by target name; the
//!   existing column's type wins, and the mapping's fidelity compares the source logical type
//!   with the existing column's logical type.
//! - A table whose plan has an `unsupported` column cannot run until the column is skipped or
//!   overridden (`TransferPlan::runnable` is false and says why).

use oxbase_dialect::{Fidelity, TypeMapper, TypeMapping};
use oxbase_types::{ColumnDef, DataType, Engine, ObjectRef, Result, ServerVersion, TableDef};
use serde::{Deserialize, Serialize};

use crate::spec::{ColumnOverride, TransferOptions};

/// The two engines and their mappers.
#[derive(Clone, Copy)]
pub struct MappingContext<'a> {
    pub source_engine: Engine,
    pub source_version: &'a ServerVersion,
    pub source_mapper: &'a dyn TypeMapper,
    pub target_engine: Engine,
    pub target_version: &'a ServerVersion,
    pub target_mapper: &'a dyn TypeMapper,
}

impl MappingContext<'_> {
    /// Source and target are the same engine (module docs, *Same engine*).
    pub fn same_engine(&self) -> bool {
        self.source_engine == self.target_engine
    }
}

/// How one source column is copied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnPlan {
    pub source: String,
    /// The source column's logical type ([`TypeMapper::logical`]).
    pub source_type: DataType,
    /// The source column's native type text (empty for query results).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_native: String,
    /// The target column name.
    pub target: String,
    /// What the target column is (logical type, native text, fidelity, reason).
    pub mapping: TypeMapping,
    /// A user override decided this column.
    #[serde(default)]
    pub overridden: bool,
    /// Left out of the load (override, or `IdentityMode::Generate` on an identity column).
    #[serde(default)]
    pub skipped: bool,
}

/// The fidelity of storing values of logical type `from` in a column of logical type `to`
/// (for overrides and for appends into existing columns): `exact` when equal; `widened` when
/// every value of `from` fits `to` (wider integers / decimals / varchar lengths, any type into
/// text or JSON); `narrowed` when values can be cut or refused; `unsupported` when there is no
/// conversion at all (binary into a number, JSON into a date). Owner: transfer stream.
pub fn fidelity_between(from: &DataType, to: &DataType) -> (Fidelity, Option<String>) {
    let _ = (from, to);
    unimplemented!("m8: transfer")
}

/// The plan of one column: same-engine, cross-engine or overridden (module docs).
pub fn map_column(
    ctx: &MappingContext<'_>,
    column: &ColumnDef,
    column_override: Option<&ColumnOverride>,
) -> ColumnPlan {
    let _ = (ctx, column, column_override);
    unimplemented!("m8: transfer")
}

/// The table to create on the target for `source` (named `target`), from its column plans
/// (module docs): relocated for the same engine, rebuilt with mapped types across engines,
/// without foreign keys (they are added after the load). Returns the definition and its
/// warnings (dropped defaults, checks, indexes, ...).
pub fn target_table(
    ctx: &MappingContext<'_>,
    source: &TableDef,
    target: ObjectRef,
    columns: &[ColumnPlan],
    options: &TransferOptions,
) -> Result<(TableDef, Vec<String>)> {
    let _ = (ctx, source, target, columns, options);
    unimplemented!("m8: transfer")
}

/// One row of a pair report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRow {
    pub logical: DataType,
    pub mapping: TypeMapping,
}

/// How every [`representative_types`] logical type maps from `source` to `target` (the
/// per-pair lossiness report; golden-tested per pair by the transfer stream and shown by the
/// wizard's "type mapping" help).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairReport {
    pub source: Engine,
    pub target: Engine,
    pub rows: Vec<PairRow>,
}

/// The pair report of `ctx` over [`representative_types`]. Owner: transfer stream.
pub fn pair_report(ctx: &MappingContext<'_>) -> PairReport {
    let _ = ctx;
    unimplemented!("m8: transfer")
}

/// One logical type per family the mapping tables must answer, in a fixed order: the rows of
/// every [`PairReport`] and of the golden mapping tests.
pub fn representative_types() -> Vec<DataType> {
    let int = |bits, unsigned| DataType::Int { bits, unsigned };
    vec![
        DataType::Bool,
        int(8, false),
        int(16, false),
        int(32, false),
        int(64, false),
        int(32, true),
        int(64, true),
        DataType::Float { bits: 32 },
        DataType::Float { bits: 64 },
        DataType::Decimal {
            precision: Some(18),
            scale: Some(4),
        },
        DataType::Decimal {
            precision: Some(38),
            scale: Some(10),
        },
        DataType::Decimal {
            precision: None,
            scale: None,
        },
        DataType::Char { length: Some(10) },
        DataType::VarChar { length: Some(255) },
        DataType::VarChar { length: None },
        DataType::Text,
        DataType::Binary { length: Some(16) },
        DataType::VarBinary { length: None },
        DataType::Date,
        DataType::Time { precision: Some(6) },
        DataType::Timestamp { precision: Some(6) },
        DataType::TimestampTz { precision: Some(6) },
        DataType::Interval,
        DataType::Uuid,
        DataType::Json { binary: false },
        DataType::Json { binary: true },
        DataType::Array {
            element: Box::new(DataType::INT32),
        },
        DataType::Array {
            element: Box::new(DataType::Text),
        },
        DataType::Enum {
            labels: vec!["small".into(), "medium".into(), "large".into()],
        },
        DataType::Other {
            name: "geometry".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn representative_types_cover_every_family_once() {
        let types = representative_types();
        assert_eq!(types.len(), 30);
        for (i, t) in types.iter().enumerate() {
            assert!(!types[..i].contains(t), "{t} twice");
        }
        // Every DataType variant appears.
        let has = |f: fn(&DataType) -> bool| types.iter().any(f);
        assert!(has(|t| matches!(t, DataType::Bool)));
        assert!(has(|t| matches!(t, DataType::Interval)));
        assert!(has(|t| matches!(t, DataType::Uuid)));
        assert!(has(|t| matches!(t, DataType::Array { .. })));
        assert!(has(|t| matches!(t, DataType::Enum { .. })));
        assert!(has(|t| matches!(t, DataType::Other { .. })));
        assert!(has(|t| matches!(t, DataType::Json { binary: true })));
    }
}
