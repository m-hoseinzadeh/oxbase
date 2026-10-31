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

use oxbase_conformance::adapter::Engine;
use oxbase_conformance::matrix::CellStatus;
use oxbase_conformance::{fixtures, report, runner};

/// M0 exit criterion (the harness runs against all engines) plus the M1 connection checks:
/// every applicable check of the M1 engines passes, and the engines without an M1 driver report
/// pending.
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
    // Object-kind cells need dialect cells (M2+), so none may have run.
    assert_eq!(
        report.summary.pass + report.summary.fail,
        0,
        "{:?}",
        report.summary
    );
    assert!(report.summary.pending + report.summary.skipped > 0);

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
