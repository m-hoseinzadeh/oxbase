//! Live conformance runs against the Docker fixtures.
//!
//! Ignored by default because they need the fixture containers. Start them with
//! `scripts/fixtures-up.sh`, then run:
//!
//! ```text
//! cargo test -p oxbase-conformance -- --include-ignored --nocapture --test-threads 1
//! ```
//!
//! `--test-threads 1` keeps the connection checks (which time cancellation) from competing with
//! the reachability probes.
//!
//! An unreachable fixture fails the test with the engine name and port. Set
//! `OXBASE_CONFORMANCE_ALLOW_MISSING=1` to report it as skipped instead.
//!
//! `OXBASE_CONFORMANCE_REQUIRE_M2_FETCH=1` (set by every CI lane since M2)
//! also requires every M2 engine (PostgreSQL, MySQL, MariaDB, SQLite) to have registered its
//! tree layout in `oxbase_services::default_dialects`, so a registration left out fails the
//! job instead of leaving its cells pending.
//!
//! `OXBASE_CONFORMANCE_REQUIRE_M3_ROUNDTRIP=1` (the integrator sets it in the conformance CI
//! step once the M3 engine streams are merged) requires every `roundtrip` and `capabilities`
//! cell of the designer kinds (table, column, index, foreign key, check) on PostgreSQL, MySQL,
//! MariaDB and SQLite to pass, and `missing_designer_roles` to be empty for each of them
//! ([`roundtrip::gate`]). Without it, those cells may pass or stay pending, never fail. It
//! cannot be combined with `OXBASE_CONFORMANCE_SKIP_ROUNDTRIP` (the test fails), and only
//! `OXBASE_CONFORMANCE_ALLOW_MISSING` (unreachable fixtures) excuses skipped cells then.

use oxbase_conformance::adapter::{self, Engine};
use oxbase_conformance::fixtures::Fixtures;
use oxbase_conformance::matrix::{CellStatus, Check};
use oxbase_conformance::{fetch, fixtures, report, roundtrip, runner};

/// M0 exit criterion (the harness runs against all engines), the M1 connection checks (every
/// applicable check of the M1 engines passes, engines without a driver report pending), the
/// M2 `fetch_list` cells ([`fetch::gate`]): every cell with a registered Fetcher and a seed
/// object passes, every kind of a registered tree layout passes or is n/a, each engine with a
/// layout passes at least one cell, and n/a never hides a seeded kind; and the M3 `roundtrip`
/// / `capabilities` cells ([`roundtrip::gate`]).
#[test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh); run with --include-ignored"]
fn harness_runs_against_all_engines_and_m1_connection_checks_pass() {
    let opts = runner::RunOptions::from_env();
    let path = fixtures::fixtures_path();
    let report =
        runner::run_path(&path, &opts).unwrap_or_else(|e| panic!("cannot load fixtures: {e}"));

    println!("{}", report.render());
    // Not report.json: that one belongs to `cargo run -p oxbase-conformance`, which may run at
    // the same time against the shared target dir.
    let out = report::report_path("report-live-test.json");
    report
        .write_json(&out)
        .unwrap_or_else(|e| panic!("cannot write {}: {e}", out.display()));
    println!("JSON report: {}", out.display());

    if let Err(msg) = report.gate() {
        panic!("{msg}");
    }
    assert_eq!(report.summary.fail, 0, "{:?}", report.summary);

    let fx = Fixtures::from_path(&path).unwrap();
    let resolved = fx.resolve().unwrap();
    let registry = adapter::registry();
    let m3 = roundtrip::GateOptions::for_run(&opts, roundtrip::GateOptions::require_m3_from_env())
        .unwrap_or_else(|e| panic!("{e}"));
    let problems = roundtrip::gate(&report.cells, &fx, &resolved, &registry, m3);
    assert!(
        problems.is_empty(),
        "roundtrip / capabilities gate ({m3:?}):\n  {}",
        problems.join("\n  ")
    );
    let ran = |check: Check| {
        report
            .cells
            .iter()
            .filter(|c| c.check == check && c.status == CellStatus::Pass)
            .count()
    };
    println!(
        "roundtrip cells passed: {}, capabilities cells passed: {} ({}: {})",
        ran(Check::Roundtrip),
        ran(Check::Capabilities),
        roundtrip::REQUIRE_ENV,
        m3.require_m3
    );
    let gate = fetch::GateOptions {
        skipped_ok: opts.allow_missing || opts.skip_fetch,
        require_m2: std::env::var("OXBASE_CONFORMANCE_REQUIRE_M2_FETCH")
            .is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes" | "on")),
    };
    let problems = fetch::gate(&report.cells, &fx, &resolved, &registry, gate);
    assert!(
        problems.is_empty(),
        "fetch_list gate ({gate:?}):\n  {}",
        problems.join("\n  ")
    );
    let registered: Vec<&str> = fetch::M2_ENGINES
        .iter()
        .filter(|&&e| registry.tree_layout(e).is_ok())
        .map(|e| e.as_str())
        .collect();
    println!(
        "M2 engines with a registered tree layout: {} (OXBASE_CONFORMANCE_REQUIRE_M2_FETCH: {})",
        if registered.is_empty() {
            "none".to_owned()
        } else {
            registered.join(", ")
        },
        gate.require_m2
    );
    let passed = report
        .cells
        .iter()
        .filter(|c| c.check == Check::FetchList && c.status == CellStatus::Pass)
        .count();
    println!("fetch_list cells passed: {passed}");

    for c in &report.connections {
        let m1 = matches!(
            c.engine_kind,
            Engine::Postgres | Engine::MySql | Engine::MariaDb | Engine::Sqlite
        );
        let ok = match c.status {
            CellStatus::Pass => m1,
            CellStatus::Pending => !m1,
            CellStatus::NotApplicable => true,
            // Only with OXBASE_CONFORMANCE_ALLOW_MISSING / SKIP_CONNECT.
            CellStatus::Skipped => opts.allow_missing || opts.skip_connect,
            CellStatus::Fail => false,
        };
        assert!(
            ok,
            "{} {}: {:?} {:?}",
            c.engine, c.check, c.status, c.detail
        );
    }
}

macro_rules! fixture_reachable {
    ($($test:ident => $engine:expr),* $(,)?) => {$(
        #[test]
        #[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh); run with --include-ignored"]
        fn $test() {
            let opts = runner::RunOptions::from_env();
            match runner::check_engine(&fixtures::fixtures_path(), $engine, &opts) {
                Ok(line) => println!("{line}"),
                Err(msg) => panic!("{msg}"),
            }
        }
    )*};
}

fixture_reachable! {
    postgres_fixture_reachable => Engine::Postgres,
    mysql_fixture_reachable => Engine::MySql,
    mariadb_fixture_reachable => Engine::MariaDb,
    mssql_fixture_reachable => Engine::MsSql,
    mongodb_fixture_reachable => Engine::MongoDb,
    redis_fixture_reachable => Engine::Redis,
    sqlite_fixture_reachable => Engine::Sqlite,
}
