//! Live conformance runs against the Docker fixtures.
//!
//! Ignored by default because they need the fixture containers. Start them with
//! `scripts/fixtures-up.sh`, then run:
//!
//! ```text
//! cargo test -p oxbase-conformance -- --include-ignored --nocapture
//! ```
//!
//! An unreachable fixture fails the test with the engine name and port. Set
//! `OXBASE_CONFORMANCE_ALLOW_MISSING=1` to report it as skipped instead.

use oxbase_conformance::adapter::Engine;
use oxbase_conformance::{fixtures, report, runner};

/// M0 exit criterion: the harness runs empty against all engines.
#[test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh); run with --include-ignored"]
fn m0_harness_runs_empty_against_all_engines() {
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
    // No drivers exist at M0, so no cell may have run.
    assert_eq!(
        report.summary.pass + report.summary.fail,
        0,
        "{:?}",
        report.summary
    );
    assert!(report.summary.pending + report.summary.skipped > 0);
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
