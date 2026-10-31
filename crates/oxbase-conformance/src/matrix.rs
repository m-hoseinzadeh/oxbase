//! The conformance matrix: engine x object kind x check.

use std::fmt;

use serde::{Deserialize, Serialize};

use oxbase_dialect::DialectRegistry;

use crate::adapter::{self, Engine, ObjectKind};

/// Detail attached to every applicable cell until the engine's driver exists.
pub const PENDING_NO_DRIVER: &str = "no driver yet";

/// What one matrix cell verifies for an (engine, object kind) pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    /// create -> fetch -> alter -> diff -> drop is stable (the diff after alter-and-refetch is
    /// empty, and drop leaves nothing behind).
    Roundtrip,
    /// `Fetcher::list` returns the object under its parent.
    FetchList,
    /// The dialect's capability flags agree with what the live server accepts.
    Capabilities,
}

impl Check {
    /// Every check, in the order cells render them.
    pub const ALL: [Self; 3] = [Self::Roundtrip, Self::FetchList, Self::Capabilities];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Roundtrip => "roundtrip",
            Self::FetchList => "fetch_list",
            Self::Capabilities => "capabilities",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Roundtrip => "create → fetch → alter → diff → drop round trip is stable",
            Self::FetchList => "fetch list returns the object under its parent",
            Self::Capabilities => "capability flags agree with the live server",
        }
    }
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome of one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellStatus {
    Pass,
    Fail,
    /// Applicable, but nothing can run yet (no driver / dialect cell).
    Pending,
    /// Applicable, but the fixture was unreachable and missing fixtures are allowed.
    Skipped,
    /// The engine has no objects of this kind.
    NotApplicable,
}

impl CellStatus {
    pub const ALL: [Self; 5] = [
        Self::Pass,
        Self::Fail,
        Self::Pending,
        Self::Skipped,
        Self::NotApplicable,
    ];

    /// One-character code used in the rendered matrix.
    pub const fn glyph(self) -> char {
        match self {
            Self::Pass => '+',
            Self::Fail => 'X',
            Self::Pending => 'p',
            Self::Skipped => 's',
            Self::NotApplicable => '-',
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Pending => "pending",
            Self::Skipped => "skipped",
            Self::NotApplicable => "n/a",
        }
    }
}

/// One (engine, object kind, check) result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    /// The fixture name (`[engines.<name>]`).
    pub engine: String,
    pub engine_kind: Engine,
    pub object_kind: ObjectKind,
    pub check: Check,
    pub status: CellStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Every cell for one engine, in [`ObjectKind::ALL`] x [`Check::ALL`] order, with
/// applicability from [`adapter::registry`]. See [`plan_engine_in`].
pub fn plan_engine(name: &str, engine: Engine) -> Vec<Cell> {
    plan_engine_in(&adapter::registry(), name, engine)
}

/// Every cell for one engine, in [`ObjectKind::ALL`] x [`Check::ALL`] order. Cells the
/// registry marks not applicable stay not applicable; the rest start as pending.
pub fn plan_engine_in(registry: &DialectRegistry, name: &str, engine: Engine) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(ObjectKind::ALL.len() * Check::ALL.len());
    for kind in ObjectKind::ALL {
        let applies = adapter::applies(registry, engine, kind);
        for check in Check::ALL {
            let (status, detail) = if applies {
                (CellStatus::Pending, Some(PENDING_NO_DRIVER.to_owned()))
            } else {
                (CellStatus::NotApplicable, None)
            };
            cells.push(Cell {
                engine: name.to_owned(),
                engine_kind: engine,
                object_kind: kind,
                check,
                status,
                detail,
            });
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_covers_every_kind_and_check_once() {
        let cells = plan_engine("pg", Engine::Postgres);
        assert_eq!(cells.len(), ObjectKind::ALL.len() * Check::ALL.len());
        for kind in ObjectKind::ALL {
            for check in Check::ALL {
                let n = cells
                    .iter()
                    .filter(|c| c.object_kind == kind && c.check == check)
                    .count();
                assert_eq!(n, 1, "{kind}/{check}");
            }
        }
    }

    #[test]
    fn plan_marks_applicable_cells_pending() {
        let cells = plan_engine("lite", Engine::Sqlite);
        let table = cells
            .iter()
            .find(|c| c.object_kind == ObjectKind::Table && c.check == Check::Roundtrip)
            .unwrap();
        assert_eq!(table.status, CellStatus::Pending);
        assert_eq!(table.detail.as_deref(), Some(PENDING_NO_DRIVER));
        let user = cells
            .iter()
            .find(|c| c.object_kind == ObjectKind::User)
            .unwrap();
        assert_eq!(user.status, CellStatus::NotApplicable);
        assert!(
            cells
                .iter()
                .all(|c| matches!(c.status, CellStatus::Pending | CellStatus::NotApplicable))
        );
    }

    #[test]
    fn glyphs_are_distinct() {
        let mut glyphs: Vec<_> = CellStatus::ALL.iter().map(|s| s.glyph()).collect();
        glyphs.sort_unstable();
        glyphs.dedup();
        assert_eq!(glyphs.len(), CellStatus::ALL.len());
    }
}
