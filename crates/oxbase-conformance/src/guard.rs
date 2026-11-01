//! The M4 exit check, "conformance green on 5 engines" (`docs/m4-contracts.md` §8.2): with
//! `OXBASE_CONFORMANCE_REQUIRE_M4=1` every [`M4_ENGINES`] engine (PostgreSQL, MySQL, MariaDB,
//! SQLite and SQL Server) must pass its M1-M3 cells, with nothing pending, skipped or failed.
//!
//! The guard has three parts, all checked by the live test:
//!
//! - [`check_run`]: nothing may be skipped (no `ALLOW_MISSING`, no `SKIP_*`);
//! - [`gate`]: every M4 engine has a fixture, each fixture is reachable, every applicable
//!   connection check passes and no matrix cell fails or is skipped;
//! - the `fetch_list` and `roundtrip` / `capabilities` gates with their `require_m4` option
//!   ([`crate::fetch::GateOptions`], [`crate::roundtrip::GateOptions`]): a tree layout, every
//!   layout kind passing, the designer roles and every designer-kind cell passing, for all five.
//!
//! Without the guard SQL Server cells may pass or stay pending (until the integrator registers
//! the SQL Server dialect cells), never fail.

use crate::adapter::Engine;
use crate::fixtures::ResolvedEngine;
use crate::matrix::CellStatus;
use crate::report::Report;
use crate::runner::{RunOptions, is_truthy};

/// The environment variable that turns the guard on (the integrator sets it in CI).
pub const REQUIRE_ENV: &str = "OXBASE_CONFORMANCE_REQUIRE_M4";

/// The engines the M4 exit check covers.
pub const M4_ENGINES: [Engine; 5] = [
    Engine::Postgres,
    Engine::MySql,
    Engine::MariaDb,
    Engine::Sqlite,
    Engine::MsSql,
];

/// Whether [`REQUIRE_ENV`] is set to a true value.
pub fn require_from_env() -> bool {
    is_truthy(std::env::var(REQUIRE_ENV).ok().as_deref())
}

/// `Err` when the run options would let the guard pass by not running something.
pub fn check_run(opts: &RunOptions) -> Result<(), String> {
    let skips = [
        ("OXBASE_CONFORMANCE_ALLOW_MISSING", opts.allow_missing),
        ("OXBASE_CONFORMANCE_SKIP_CONNECT", opts.skip_connect),
        ("OXBASE_CONFORMANCE_SKIP_FETCH", opts.skip_fetch),
        ("OXBASE_CONFORMANCE_SKIP_ROUNDTRIP", opts.skip_roundtrip),
        ("OXBASE_CONFORMANCE_TCP_ONLY", !opts.probe.handshake),
    ];
    let set: Vec<&str> = skips
        .iter()
        .filter(|(_, on)| *on)
        .map(|(name, _)| *name)
        .collect();
    if set.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{REQUIRE_ENV} is set together with {}: the 5-engine guard cannot skip anything",
            set.join(", ")
        ))
    }
}

/// Every problem of `report` under the guard (empty = acceptable). The `fetch_list` and
/// round-trip specifics are checked by their own gates (see the module docs).
pub fn gate(report: &Report, engines: &[ResolvedEngine]) -> Vec<String> {
    let mut problems = Vec::new();
    for want in M4_ENGINES {
        if !engines.iter().any(|e| e.engine == want) {
            problems.push(format!(
                "no {want} fixture in the fixtures file ({REQUIRE_ENV} is set)"
            ));
        }
    }
    let guarded = |name: &str| {
        engines
            .iter()
            .any(|e| e.name == name && M4_ENGINES.contains(&e.engine))
    };
    for e in report.engines.iter().filter(|e| guarded(&e.name)) {
        if !e.probe.reachable {
            problems.push(format!(
                "{}: fixture unreachable at {} ({REQUIRE_ENV} is set)",
                e.name, e.target
            ));
        }
    }
    for c in report.connections.iter().filter(|c| guarded(&c.engine)) {
        if !matches!(c.status, CellStatus::Pass | CellStatus::NotApplicable) {
            problems.push(format!(
                "{} {} connection check: {} ({}; {REQUIRE_ENV} is set: must pass)",
                c.engine,
                c.check,
                c.status.label(),
                c.detail.as_deref().unwrap_or("no detail")
            ));
        }
    }
    for c in report.cells.iter().filter(|c| guarded(&c.engine)) {
        if matches!(c.status, CellStatus::Fail | CellStatus::Skipped) {
            problems.push(format!(
                "{} {} {}: {} ({}; {REQUIRE_ENV} is set)",
                c.engine,
                c.object_kind,
                c.check,
                c.status.label(),
                c.detail.as_deref().unwrap_or("no detail")
            ));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_covers_the_five_sql_engines() {
        assert_eq!(M4_ENGINES.len(), 5);
        assert!(M4_ENGINES.contains(&Engine::MsSql));
        for e in crate::fetch::M2_ENGINES {
            assert!(M4_ENGINES.contains(&e), "{e}");
        }
        for e in crate::roundtrip::M3_ENGINES {
            assert!(M4_ENGINES.contains(&e), "{e}");
        }
        assert!(!M4_ENGINES.contains(&Engine::MongoDb));
        assert!(!M4_ENGINES.contains(&Engine::Redis));
    }

    #[test]
    fn nothing_may_be_skipped_under_the_guard() {
        let mut opts = RunOptions::from_vars(None, None, None);
        assert_eq!(check_run(&opts), Ok(()));
        opts.skip_roundtrip = true;
        opts.allow_missing = true;
        let err = check_run(&opts).unwrap_err();
        assert!(
            err.contains("OXBASE_CONFORMANCE_ALLOW_MISSING")
                && err.contains("OXBASE_CONFORMANCE_SKIP_ROUNDTRIP"),
            "{err}"
        );
        let tcp = RunOptions::from_vars(None, None, Some("1"));
        assert!(
            check_run(&tcp)
                .unwrap_err()
                .contains("OXBASE_CONFORMANCE_TCP_ONLY")
        );
    }
}
