//! M8 conformance against the Docker fixtures (`docs/m8-contracts.md` §12): structure sync, data
//! sync, the transfer matrix and backup / restore on PostgreSQL, MySQL, MariaDB, SQL Server and
//! SQLite, on scratch copies of the `[m8]` seed ([`oxbase_conformance::m8`]).
//!
//! Ignored by default because it needs the fixture containers:
//!
//! ```text
//! scripts/fixtures-up.sh
//! cargo test -p oxbase-conformance --test m8_live -- --include-ignored --nocapture
//! ```
//!
//! Prints the per-engine table and the transfer matrix, writes
//! `target/conformance/m8-report.json`, and fails on any failed cell. Cells whose stream has not
//! landed are pending; `OXBASE_CONFORMANCE_REQUIRE_M8=1` (the integrator's switch) makes pending
//! and skipped cells, and missing M8 roles, fail too.

use oxbase_conformance::{fixtures, m8, report};

#[test]
#[ignore = "needs the Docker fixtures (scripts/fixtures-up.sh); run with --include-ignored"]
fn m8_sync_transfer_and_backup_cells() {
    let opts = m8::M8Options::from_env();
    assert!(
        !(opts.require && opts.skip),
        "{} cannot be combined with {}",
        m8::REQUIRE_ENV,
        m8::SKIP_ENV
    );
    let path = fixtures::fixtures_path();
    let report = m8::run_path(&path, opts).unwrap_or_else(|e| panic!("cannot load fixtures: {e}"));
    println!("{}", report.render());
    let out = report::report_path(m8::REPORT_FILE);
    report
        .write_json(&out)
        .unwrap_or_else(|e| panic!("cannot write {}: {e}", out.display()));
    println!("JSON report: {}", out.display());

    let problems = report.problems(opts);
    assert!(
        problems.is_empty(),
        "M8 conformance ({}: {}):\n  {}",
        m8::REQUIRE_ENV,
        opts.require,
        problems.join("\n  ")
    );
    // Without the guard, preview what it would refuse.
    if !opts.require {
        let guarded = report.problems(m8::M8Options {
            require: true,
            ..opts
        });
        println!(
            "M8 guard ({}): {}",
            m8::REQUIRE_ENV,
            if guarded.is_empty() {
                "nothing open (every cell passes; the guard can be turned on)".to_owned()
            } else {
                format!("{} open: {}", guarded.len(), guarded.join("; "))
            }
        );
    }
}
