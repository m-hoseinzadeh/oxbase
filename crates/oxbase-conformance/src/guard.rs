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
    use std::path::PathBuf;

    use super::*;
    use crate::adapter::ObjectKind;
    use crate::connect::{ConnCell, ConnCheck};
    use crate::matrix::{Cell, Check};
    use crate::probe::{Handshake, ProbeOutcome, Target};
    use crate::report::{EngineStatus, REPORT_SCHEMA_VERSION, Summary};

    fn target(port: u16) -> Target {
        Target::Tcp {
            host: "127.0.0.1".to_owned(),
            port,
        }
    }

    fn resolved(name: &str, engine: Engine, port: u16) -> ResolvedEngine {
        ResolvedEngine {
            name: name.to_owned(),
            engine,
            target: target(port),
            version_prefix: None,
        }
    }

    fn status(name: &str, kind: Engine, port: u16, reachable: bool) -> EngineStatus {
        EngineStatus {
            name: name.to_owned(),
            kind,
            target: target(port),
            probe: ProbeOutcome {
                reachable,
                latency_ms: 1,
                handshake: Handshake::NotAttempted,
                server_version: None,
                version_matches: None,
                expected_version: None,
                error: (!reachable).then(|| "TCP connect failed: refused".to_owned()),
            },
        }
    }

    fn conn(name: &str, kind: Engine, check: ConnCheck, status: CellStatus) -> ConnCell {
        ConnCell {
            engine: name.to_owned(),
            engine_kind: kind,
            check,
            status,
            detail: Some(format!("{name} {}", status.label())),
            elapsed_ms: None,
        }
    }

    fn cell(name: &str, kind: Engine, object: ObjectKind, check: Check, st: CellStatus) -> Cell {
        Cell {
            engine: name.to_owned(),
            engine_kind: kind,
            object_kind: object,
            check,
            status: st,
            detail: None,
            elapsed_ms: None,
        }
    }

    fn report(engines: Vec<EngineStatus>, connections: Vec<ConnCell>, cells: Vec<Cell>) -> Report {
        Report {
            report_schema_version: REPORT_SCHEMA_VERSION,
            milestone: "M4".to_owned(),
            generated_at_unix: 0,
            fixtures_path: PathBuf::from("/repo/tests/fixtures.toml"),
            compose_project: None,
            allow_missing: false,
            engines,
            missing_engines: Vec::new(),
            checks: Vec::new(),
            summary: Summary::from_cells(&cells),
            cells,
            connection_checks: Vec::new(),
            connections,
            connection_summary: Summary::default(),
        }
    }

    #[test]
    fn the_gate_reports_every_open_item_of_the_five_engines_only() {
        // SQLite has no fixture; mongodb and redis are outside the guard.
        let engines = vec![
            resolved("postgres", Engine::Postgres, 15432),
            resolved("mysql", Engine::MySql, 15406),
            resolved("mariadb", Engine::MariaDb, 15407),
            resolved("mssql", Engine::MsSql, 15433),
            resolved("mongodb", Engine::MongoDb, 15417),
            resolved("redis", Engine::Redis, 15479),
        ];
        let r = report(
            vec![
                status("postgres", Engine::Postgres, 15432, true),
                status("mysql", Engine::MySql, 15406, true),
                status("mariadb", Engine::MariaDb, 15407, true),
                status("mssql", Engine::MsSql, 15433, false),
                status("mongodb", Engine::MongoDb, 15417, false),
                status("redis", Engine::Redis, 15479, false),
            ],
            vec![
                conn(
                    "postgres",
                    Engine::Postgres,
                    ConnCheck::Connect,
                    CellStatus::Pass,
                ),
                conn(
                    "mysql",
                    Engine::MySql,
                    ConnCheck::Ssh,
                    CellStatus::NotApplicable,
                ),
                conn("mssql", Engine::MsSql, ConnCheck::Tls, CellStatus::Fail),
                conn(
                    "mariadb",
                    Engine::MariaDb,
                    ConnCheck::Cancel,
                    CellStatus::Skipped,
                ),
                conn("redis", Engine::Redis, ConnCheck::Connect, CellStatus::Fail),
                conn(
                    "mongodb",
                    Engine::MongoDb,
                    ConnCheck::Tls,
                    CellStatus::Skipped,
                ),
            ],
            vec![
                cell(
                    "postgres",
                    Engine::Postgres,
                    ObjectKind::Table,
                    Check::FetchList,
                    CellStatus::Pass,
                ),
                cell(
                    "mysql",
                    Engine::MySql,
                    ObjectKind::Index,
                    Check::Roundtrip,
                    CellStatus::Fail,
                ),
                cell(
                    "mssql",
                    Engine::MsSql,
                    ObjectKind::View,
                    Check::FetchList,
                    CellStatus::Skipped,
                ),
                // Pending is the fetch / roundtrip gates' business, not this one's.
                cell(
                    "mssql",
                    Engine::MsSql,
                    ObjectKind::Check,
                    Check::Capabilities,
                    CellStatus::Pending,
                ),
                cell(
                    "mariadb",
                    Engine::MariaDb,
                    ObjectKind::Column,
                    Check::Roundtrip,
                    CellStatus::NotApplicable,
                ),
                cell(
                    "mongodb",
                    Engine::MongoDb,
                    ObjectKind::Table,
                    Check::FetchList,
                    CellStatus::Fail,
                ),
                cell(
                    "redis",
                    Engine::Redis,
                    ObjectKind::Table,
                    Check::FetchList,
                    CellStatus::Skipped,
                ),
            ],
        );
        let problems = gate(&r, &engines);
        let has = |needle: &str| problems.iter().filter(|p| p.contains(needle)).count();
        assert_eq!(has("no sqlite fixture"), 1, "{problems:?}");
        assert_eq!(has("mssql: fixture unreachable"), 1, "{problems:?}");
        assert_eq!(has("mssql tls connection check: fail"), 1, "{problems:?}");
        assert_eq!(
            has("mariadb cancel connection check: skipped"),
            1,
            "{problems:?}"
        );
        assert_eq!(has("mysql index roundtrip: fail"), 1, "{problems:?}");
        assert_eq!(has("mssql view fetch_list: skipped"), 1, "{problems:?}");
        assert_eq!(has("mongodb"), 0, "{problems:?}");
        assert_eq!(has("redis"), 0, "{problems:?}");
        assert_eq!(problems.len(), 6, "{problems:?}");

        // All five present, reachable and passing: nothing open.
        let five: Vec<ResolvedEngine> = M4_ENGINES
            .iter()
            .enumerate()
            .map(|(i, &e)| resolved(e.as_str(), e, 15000 + u16::try_from(i).unwrap()))
            .collect();
        let ok = report(
            five.iter()
                .map(|e| status(&e.name, e.engine, 1, true))
                .collect(),
            five.iter()
                .map(|e| conn(&e.name, e.engine, ConnCheck::Connect, CellStatus::Pass))
                .collect(),
            five.iter()
                .map(|e| {
                    cell(
                        &e.name,
                        e.engine,
                        ObjectKind::Table,
                        Check::FetchList,
                        CellStatus::Pass,
                    )
                })
                .collect(),
        );
        assert_eq!(gate(&ok, &five), Vec::<String>::new());
    }

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
