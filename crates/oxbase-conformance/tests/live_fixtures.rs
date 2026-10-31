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

use oxbase_conformance::adapter::{self, Engine};
use oxbase_conformance::fixtures::Fixtures;
use oxbase_conformance::matrix::{CellStatus, Check};
use oxbase_conformance::{fetch, fixtures, report, runner};

/// M0 exit criterion (the harness runs against all engines), the M1 connection checks (every
/// applicable check of the M1 engines passes, engines without a driver report pending) and the
/// M2 `fetch_list` cells: every cell with a registered Fetcher and a seed object passes, the
/// rest stay pending (or n/a when the server version lacks the kind).
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
    let skipped_ok = opts.allow_missing || opts.skip_fetch;
    for c in &report.cells {
        if c.check != Check::FetchList {
            // roundtrip and capabilities have no runner before M3.
            assert!(
                !matches!(c.status, CellStatus::Pass | CellStatus::Fail),
                "{} {} {}: {:?} {:?}",
                c.engine,
                c.object_kind,
                c.check,
                c.status,
                c.detail
            );
            continue;
        }
        let e = resolved.iter().find(|e| e.name == c.engine).unwrap();
        let must_run = fetch::runnable_kinds(&registry, e.engine).contains(&c.object_kind)
            && !fetch::groups_for(&fx, e, c.object_kind).is_empty();
        let ok = match c.status {
            CellStatus::Pass => must_run,
            CellStatus::Pending => !must_run,
            CellStatus::NotApplicable => true,
            CellStatus::Skipped => skipped_ok,
            CellStatus::Fail => false,
        };
        assert!(
            ok,
            "{} {} fetch_list: {:?} {:?} (registered fetcher and seed: {must_run})",
            c.engine, c.object_kind, c.status, c.detail
        );
    }
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
