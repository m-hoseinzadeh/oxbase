//! Comparator rules for SQL Server (M4, **ddl** stream): the shared structural walk of
//! `oxbase_dialect::compare` with [`MsSqlRules`]. Contract: `docs/m4-contracts.md` §4.3.
//!
//! What the rules must fold so that a fetched definition compares equal to the one it was
//! generated from (the round-trip invariant):
//!
//! - type aliases and spellings: `int` / `INT`, `nvarchar(max)` / `nvarchar(-1)`, `float` /
//!   `float(53)`, `real` / `float(24)`, `decimal(18, 0)` / `decimal`, `rowversion` /
//!   `timestamp`, `sysname` / `nvarchar(128)`;
//! - default and check text as the server stores it: redundant parentheses (`((0))` = `0`,
//!   `('x')` = `'x'`, `([qty]>=(0))` = `qty >= 0`), bracketed identifiers, case of keywords and
//!   built-in function names (`getdate()` / `GETDATE()`), `N'x'` vs `'x'` only where the column is
//!   Unicode;
//! - extras compared only when both sides have them ([`crate::extras::DEFAULT_CONSTRAINT`]
//!   names the server generated, `CLUSTERED` of the primary key defaulting to `true`).
//!
//! The defaults of `CompareRules` are a placeholder until the stream overrides them.

use std::sync::Arc;

use oxbase_dialect::Comparator;
use oxbase_dialect::compare::{CompareRules, RulesComparator};

/// SQL Server's comparison rules.
#[derive(Debug, Clone, Copy, Default)]
pub struct MsSqlRules;

impl CompareRules for MsSqlRules {}

/// The comparator registered for every designer kind.
pub(crate) fn comparator() -> Arc<dyn Comparator> {
    Arc::new(RulesComparator(MsSqlRules))
}
