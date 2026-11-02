//! The conformance matrix: engine x object kind x check.

use std::fmt;

use serde::{Deserialize, Serialize};

use oxbase_dialect::{DESIGNER_KINDS, DialectRegistry};

use crate::adapter::{self, Engine, ObjectKind};
use crate::roundtrip;

/// Detail of an applicable `fetch_list` cell whose engine registered no [`Fetcher`] for the
/// kind (yet).
///
/// [`Fetcher`]: oxbase_dialect::Fetcher
pub const PENDING_NO_FETCHER: &str = "pending (no Fetcher registered)";
/// Detail of an applicable `fetch_list` cell that has a fetcher but has not run (yet).
pub const PENDING_NOT_RUN: &str = "pending (Fetcher registered; not run)";
/// Detail of an applicable `roundtrip` cell of a kind outside the M3 designer kinds: no
/// round trip is defined for it yet.
pub const PENDING_ROUNDTRIP: &str = "pending (no round trip for this kind in M3)";
/// Detail of an applicable `capabilities` cell of a kind outside the M3 designer kinds.
pub const PENDING_CAPABILITIES: &str = "pending (no live capability probe for this kind yet)";
/// Detail of a `roundtrip` / `capabilities` cell of an engine without a DDL designer.
pub const NO_DESIGNER: &str = "no DDL designer: the engine registers no DdlGenerator";

/// Whether `engine` registered its dialect cells (a tree layout) but no [`DdlGenerator`] for
/// any kind: an engine without a DDL designer (MongoDB and Redis in M7), whose `roundtrip` /
/// `capabilities` cells are not applicable. An engine that registered nothing yet keeps them
/// pending.
///
/// [`DdlGenerator`]: oxbase_dialect::DdlGenerator
pub fn has_no_designer(registry: &DialectRegistry, engine: Engine) -> bool {
    registry.tree_layout(engine).is_ok()
        && ObjectKind::ALL
            .iter()
            .all(|&k| registry.ddl(engine, k).is_err())
}

/// Why an applicable matrix cell is still pending before the run fills it in.
pub fn pending_detail(
    registry: &DialectRegistry,
    engine: Engine,
    kind: ObjectKind,
    check: Check,
) -> String {
    let designer = DESIGNER_KINDS.contains(&kind);
    let detail = match check {
        Check::Roundtrip | Check::Capabilities if designer => {
            return roundtrip::pending_detail(registry, engine, kind, check);
        }
        Check::Roundtrip => PENDING_ROUNDTRIP,
        Check::Capabilities => PENDING_CAPABILITIES,
        Check::FetchList if registry.fetcher(engine, kind).is_ok() => PENDING_NOT_RUN,
        Check::FetchList => PENDING_NO_FETCHER,
    };
    detail.to_owned()
}

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
    /// How long the check took, for cells that ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// Every cell for one engine, in [`ObjectKind::ALL`] x [`Check::ALL`] order, with
/// applicability from [`adapter::registry`] (`oxbase_services::default_dialects`). See
/// [`plan_engine_in`].
pub fn plan_engine(name: &str, engine: Engine) -> Vec<Cell> {
    plan_engine_in(&adapter::registry(), name, engine)
}

/// Every cell for one engine, in [`ObjectKind::ALL`] x [`Check::ALL`] order. Cells the
/// registry marks not applicable stay not applicable, and so do the `roundtrip` /
/// `capabilities` cells of an engine without a DDL designer ([`has_no_designer`]); the rest
/// start as pending.
pub fn plan_engine_in(registry: &DialectRegistry, name: &str, engine: Engine) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(ObjectKind::ALL.len() * Check::ALL.len());
    let no_designer = has_no_designer(registry, engine);
    for kind in ObjectKind::ALL {
        let applies = adapter::applies(registry, engine, kind);
        for check in Check::ALL {
            let designer_check = matches!(check, Check::Roundtrip | Check::Capabilities);
            let (status, detail) = if applies && designer_check && no_designer {
                (CellStatus::NotApplicable, Some(NO_DESIGNER.to_owned()))
            } else if applies {
                (
                    CellStatus::Pending,
                    Some(pending_detail(registry, engine, kind, check)),
                )
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
                elapsed_ms: None,
            });
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(name: &str, engine: Engine) -> Vec<Cell> {
        plan_engine_in(&adapter::provisional_registry(), name, engine)
    }

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
        let cells = plan("lite", Engine::Sqlite);
        let detail = |kind, check| {
            cells
                .iter()
                .find(|c| c.object_kind == kind && c.check == check)
                .unwrap()
                .detail
                .clone()
        };
        // Designer kinds name the roles they still need; other kinds have no round trip.
        let table_rt = detail(ObjectKind::Table, Check::Roundtrip).unwrap();
        assert!(
            table_rt.starts_with("pending (missing: table DdlGenerator, table Comparator"),
            "{table_rt}"
        );
        let table_caps = detail(ObjectKind::Table, Check::Capabilities).unwrap();
        assert!(
            table_caps.starts_with("pending (needs the round trip; missing: "),
            "{table_caps}"
        );
        assert_eq!(
            detail(ObjectKind::View, Check::Roundtrip).as_deref(),
            Some(PENDING_ROUNDTRIP)
        );
        assert_eq!(
            detail(ObjectKind::Table, Check::FetchList).as_deref(),
            Some(PENDING_NO_FETCHER)
        );
        assert_eq!(
            detail(ObjectKind::View, Check::Capabilities).as_deref(),
            Some(PENDING_CAPABILITIES)
        );
        // Every engine has a driver since M7: Redis cells name what is missing, like the rest.
        let redis = plan("r", Engine::Redis);
        let key_fetch = redis
            .iter()
            .find(|c| c.object_kind == ObjectKind::Key && c.check == Check::FetchList)
            .unwrap();
        assert_eq!(key_fetch.detail.as_deref(), Some(PENDING_NO_FETCHER));
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
    fn engines_without_a_ddl_designer_have_no_roundtrip_cells() {
        let mut reg = adapter::provisional_registry();
        // Nothing registered yet: pending like any other engine.
        assert!(!has_no_designer(&reg, Engine::MongoDb));
        reg.register_fetcher(
            Engine::MongoDb,
            ObjectKind::Table,
            std::sync::Arc::new(NoFetch),
        );
        assert!(!has_no_designer(&reg, Engine::MongoDb));
        reg.register_tree_layout(
            Engine::MongoDb,
            oxbase_dialect::TreeLayout::new(oxbase_dialect::DatabaseScope::Catalog),
        );
        assert!(has_no_designer(&reg, Engine::MongoDb));
        let cells = plan_engine_in(&reg, "m", Engine::MongoDb);
        let table = |check| {
            cells
                .iter()
                .find(|c| c.object_kind == ObjectKind::Table && c.check == check)
                .unwrap()
        };
        assert_eq!(table(Check::Roundtrip).status, CellStatus::NotApplicable);
        assert_eq!(table(Check::Capabilities).status, CellStatus::NotApplicable);
        assert_eq!(table(Check::Roundtrip).detail.as_deref(), Some(NO_DESIGNER));
        assert_eq!(table(Check::FetchList).status, CellStatus::Pending);
        assert_eq!(
            table(Check::FetchList).detail.as_deref(),
            Some(PENDING_NOT_RUN)
        );
        // Kinds the engine does not have stay plain n/a.
        let column = cells
            .iter()
            .find(|c| c.object_kind == ObjectKind::Column && c.check == Check::Roundtrip)
            .unwrap();
        assert_eq!(column.status, CellStatus::NotApplicable);
        assert_eq!(column.detail, None);
        // The SQL engines of the real registry all have a designer.
        let real = adapter::registry();
        for engine in crate::guard::M4_ENGINES {
            assert!(!has_no_designer(&real, engine), "{engine}");
        }
    }

    #[test]
    fn registered_fetchers_are_pending_until_run() {
        let reg = adapter::provisional_registry();
        let mut with = reg.clone();
        with.register_fetcher(
            Engine::Sqlite,
            ObjectKind::View,
            std::sync::Arc::new(NoFetch),
        );
        assert_eq!(
            pending_detail(&with, Engine::Sqlite, ObjectKind::View, Check::FetchList),
            PENDING_NOT_RUN
        );
        assert_eq!(
            pending_detail(&reg, Engine::Sqlite, ObjectKind::View, Check::FetchList),
            PENDING_NO_FETCHER
        );
    }

    struct NoFetch;

    #[async_trait::async_trait]
    impl oxbase_dialect::Fetcher for NoFetch {
        async fn list(
            &self,
            _c: &dyn oxbase_driver::Connection,
            _parent: &oxbase_types::ObjectRef,
            _cancel: oxbase_driver::CancellationToken,
        ) -> oxbase_types::Result<Vec<oxbase_types::ObjectSummary>> {
            Ok(Vec::new())
        }

        async fn load(
            &self,
            _c: &dyn oxbase_driver::Connection,
            obj: &oxbase_types::ObjectRef,
            _cancel: oxbase_driver::CancellationToken,
        ) -> oxbase_types::Result<oxbase_types::ObjectDef> {
            Err(oxbase_types::Error::not_found(obj.to_string()))
        }
    }

    #[test]
    fn glyphs_are_distinct() {
        let mut glyphs: Vec<_> = CellStatus::ALL.iter().map(|s| s.glyph()).collect();
        glyphs.sort_unstable();
        glyphs.dedup();
        assert_eq!(glyphs.len(), CellStatus::ALL.len());
    }
}
