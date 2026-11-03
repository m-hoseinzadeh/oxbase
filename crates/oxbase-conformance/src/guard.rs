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
//!
//! # The 7-engine guard (M7)
//!
//! `OXBASE_CONFORMANCE_REQUIRE_M7=1` (`docs/m7-contracts.md` §11; the integrator sets it once
//! the MongoDB and Redis dialect cells are registered) extends the guard to [`M7_ENGINES`]: it
//! implies the M4 guard for the SQL engines, and MongoDB and Redis must also have a reachable
//! fixture, pass every applicable connection check (plain, TLS, SSH, TLS over SSH, cancel, the
//! 200 000-row stream) and every `fetch_list` cell of their tree layout (the `fetch_list` gate
//! with `require_m7`), with nothing failed or skipped. Their `roundtrip` / `capabilities` cells
//! are not applicable once their cells are registered (no DDL designer in M7,
//! [`crate::matrix`]). Without it their connection checks must still pass (both drivers are
//! merged) and their `fetch_list` cells may stay pending until the dialect cells land.

use crate::adapter::Engine;
use crate::fixtures::ResolvedEngine;
use crate::matrix::CellStatus;
use crate::report::Report;
use crate::runner::{RunOptions, is_truthy};

/// The environment variable that turns the guard on (the integrator sets it in CI).
pub const REQUIRE_ENV: &str = "OXBASE_CONFORMANCE_REQUIRE_M4";

/// The environment variable that turns the 7-engine guard on (M7; implies [`REQUIRE_ENV`]).
pub const REQUIRE_M7_ENV: &str = "OXBASE_CONFORMANCE_REQUIRE_M7";

/// The engines the M4 exit check covers.
pub const M4_ENGINES: [Engine; 5] = [
    Engine::Postgres,
    Engine::MySql,
    Engine::MariaDb,
    Engine::Sqlite,
    Engine::MsSql,
];

/// The engines the M7 exit check covers: every engine.
pub const M7_ENGINES: [Engine; 7] = Engine::ALL;

/// Whether [`REQUIRE_ENV`] is set to a true value.
pub fn require_from_env() -> bool {
    is_truthy(std::env::var(REQUIRE_ENV).ok().as_deref())
}

/// Whether [`REQUIRE_M7_ENV`] is set to a true value.
pub fn require_m7_from_env() -> bool {
    is_truthy(std::env::var(REQUIRE_M7_ENV).ok().as_deref())
}

/// `Err` when the run options would let the guard pass by not running something.
pub fn check_run(opts: &RunOptions) -> Result<(), String> {
    check_run_for(opts, REQUIRE_ENV, "5-engine")
}

/// [`check_run`] for the 7-engine guard.
pub fn check_run_m7(opts: &RunOptions) -> Result<(), String> {
    check_run_for(opts, REQUIRE_M7_ENV, "7-engine")
}

fn check_run_for(opts: &RunOptions, env: &str, what: &str) -> Result<(), String> {
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
            "{env} is set together with {}: the {what} guard cannot skip anything",
            set.join(", ")
        ))
    }
}

/// Every problem of `report` under the guard (empty = acceptable). The `fetch_list` and
/// round-trip specifics are checked by their own gates (see the module docs).
pub fn gate(report: &Report, engines: &[ResolvedEngine]) -> Vec<String> {
    gate_for(report, engines, &M4_ENGINES, REQUIRE_ENV)
}

/// [`gate`] for the 7-engine guard ([`M7_ENGINES`], [`REQUIRE_M7_ENV`]).
pub fn gate_m7(report: &Report, engines: &[ResolvedEngine]) -> Vec<String> {
    gate_for(report, engines, &M7_ENGINES, REQUIRE_M7_ENV)
}

fn gate_for(
    report: &Report,
    engines: &[ResolvedEngine],
    required: &[Engine],
    env: &str,
) -> Vec<String> {
    let mut problems = Vec::new();
    // OXBASE_CONFORMANCE_ENGINES: only the selected engines are required.
    for &want in required.iter().filter(|&&e| crate::adapter::is_selected(e)) {
        if !engines.iter().any(|e| e.engine == want) {
            problems.push(format!(
                "no {want} fixture in the fixtures file ({env} is set)"
            ));
        }
    }
    let guarded = |name: &str| {
        engines
            .iter()
            .any(|e| e.name == name && required.contains(&e.engine))
    };
    for e in report.engines.iter().filter(|e| guarded(&e.name)) {
        if !e.probe.reachable {
            problems.push(format!(
                "{}: fixture unreachable at {} ({env} is set)",
                e.name, e.target
            ));
        }
    }
    for c in report.connections.iter().filter(|c| guarded(&c.engine)) {
        if !matches!(c.status, CellStatus::Pass | CellStatus::NotApplicable) {
            problems.push(format!(
                "{} {} connection check: {} ({}; {env} is set: must pass)",
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
                "{} {} {}: {} ({}; {env} is set)",
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
    fn the_m7_gate_adds_mongodb_and_redis() {
        let engines = vec![
            resolved("postgres", Engine::Postgres, 15432),
            resolved("mongodb", Engine::MongoDb, 15417),
            resolved("redis", Engine::Redis, 15479),
        ];
        let r = report(
            vec![
                status("postgres", Engine::Postgres, 15432, true),
                status("mongodb", Engine::MongoDb, 15417, false),
                status("redis", Engine::Redis, 15479, true),
            ],
            vec![
                conn("redis", Engine::Redis, ConnCheck::Connect, CellStatus::Fail),
                conn("redis", Engine::Redis, ConnCheck::Tls, CellStatus::Pending),
                conn(
                    "postgres",
                    Engine::Postgres,
                    ConnCheck::Connect,
                    CellStatus::Pass,
                ),
            ],
            vec![
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
                    ObjectKind::Key,
                    Check::FetchList,
                    CellStatus::Pass,
                ),
            ],
        );
        let problems = gate_m7(&r, &engines);
        let has = |needle: &str| problems.iter().filter(|p| p.contains(needle)).count();
        assert_eq!(has("mongodb: fixture unreachable"), 1, "{problems:?}");
        assert_eq!(
            has("redis connect connection check: fail"),
            1,
            "{problems:?}"
        );
        assert_eq!(
            has("redis tls connection check: pending"),
            1,
            "{problems:?}"
        );
        assert_eq!(has("mongodb table fetch_list: fail"), 1, "{problems:?}");
        // mysql, mariadb, sqlite and mssql have no fixture here.
        assert_eq!(has("fixture in the fixtures file"), 4, "{problems:?}");
        assert!(
            problems
                .iter()
                .all(|p| p.contains("OXBASE_CONFORMANCE_REQUIRE_M7 is set")),
            "{problems:?}"
        );
        assert_eq!(problems.len(), 8, "{problems:?}");
        // The M4 gate ignores both.
        let m4 = gate(&r, &engines);
        assert!(
            m4.iter()
                .all(|p| !p.contains("mongodb") && !p.contains("redis")),
            "{m4:?}"
        );
        assert_eq!(M7_ENGINES.len(), 7);
        for e in M4_ENGINES {
            assert!(M7_ENGINES.contains(&e), "{e}");
        }
        let mut opts = RunOptions::from_vars(None, None, None);
        assert_eq!(check_run_m7(&opts), Ok(()));
        opts.skip_fetch = true;
        let err = check_run_m7(&opts).unwrap_err();
        assert!(
            err.starts_with("OXBASE_CONFORMANCE_REQUIRE_M7 is set together with")
                && err.contains("OXBASE_CONFORMANCE_SKIP_FETCH")
                && err.contains("7-engine"),
            "{err}"
        );
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
