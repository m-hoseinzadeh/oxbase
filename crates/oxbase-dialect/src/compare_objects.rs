//! Structural comparison of views, materialized views, routines, triggers and sequences (M8,
//! `docs/m8-contracts.md` §5.2), shared by every engine's [`Comparator`] cells for those kinds,
//! the way [`crate::compare`] is shared for tables.
//!
//! Structure sync compares two servers of the **same engine**, so engine-native text (a view
//! query, a routine body, a trigger condition) is compared as the server reports it, after
//! [`CompareRules::same_expression`] (whitespace collapsed by default) and after the caller has
//! moved names inside it from the source container to the target (`oxbase_diff::retarget_sql`).
//!
//! # Property ids and order (fixed)
//!
//! | Object | Properties, in order (`oxbase_types::props`) | Value |
//! |---|---|---|
//! | view, materialized view | `definition`, `columns`, `comment`, extras | text, JSON list, text |
//! | function, procedure | `args`, `returns`, `language`, `body` (or `definition` when the engine reports no body), `comment`, extras | JSON list of `RoutineArg`, text, text, text |
//! | trigger | `timing`, `events`, `update_columns`, `level`, `condition`, `function`, `body` (or `definition`), `enabled`, `comment`, extras | snake_case text, JSON list, JSON list, text, text, `schema.name` text, text, bool, text |
//! | sequence | `data_type`, `start`, `increment`, `min_value`, `max_value`, `cache`, `cycle`, `owned_by`, `comment`, extras | text, decimal text (×5), bool, `table.column` text, text |
//!
//! Every difference is a [`PropertyDiff::property`] on the compared object's own ref (from `b`).
//! Absent and empty comments are equal; extras follow the [`crate::compare`] rules (compared
//! only where both sides have them and [`CompareRules::compare_extra`] allows it). Two
//! definitions of different kinds, or a kind outside this module, compare as one `definition`
//! difference (never a panic).
//!
//! Owner of the body: the **structure-sync** stream.

use oxbase_types::{ObjectDef, PropertyDiff};

use crate::Comparator;
use crate::compare::CompareRules;

/// A [`Comparator`] for `View`, `MatView`, `Function`, `Procedure`, `Trigger` and `Sequence`
/// cells that runs [`diff_definitions`] with the engine's rules.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefinitionComparator<R>(pub R);

impl<R: CompareRules> Comparator for DefinitionComparator<R> {
    fn diff(&self, a: &ObjectDef, b: &ObjectDef) -> Vec<PropertyDiff> {
        diff_definitions(&self.0, a, b)
    }
}

/// Differences going from `a` (old) to `b` (new) for the kinds of the module docs, in the fixed
/// property order. Empty when equivalent.
pub fn diff_definitions(
    rules: &dyn CompareRules,
    a: &ObjectDef,
    b: &ObjectDef,
) -> Vec<PropertyDiff> {
    let _ = (rules, a, b);
    unimplemented!("m8: structure-sync")
}
