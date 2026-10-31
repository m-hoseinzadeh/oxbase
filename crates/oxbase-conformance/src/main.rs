//! `oxbase-conformance`: runs the conformance matrix once, prints the table and writes the JSON
//! report. Exit codes: 0 ok, 1 the run failed its gate, 2 usage or I/O error.

use std::io::{self, Write as _};
use std::process::ExitCode;

use oxbase_conformance::{fixtures, report, runner};

const USAGE: &str =
    "usage: oxbase-conformance [--allow-missing] [--tcp-only] [--skip-connect] [--skip-fetch]
                          [--skip-roundtrip]

Probes every engine in tests/fixtures.toml ($OXBASE_FIXTURES), runs the M1
connection checks (plain, TLS, SSH, TLS over SSH, cancel, 200k-row stream), the
M2 fetch_list cells (list and load the [m2] seed objects through the registered
Fetchers) and the M3 roundtrip / capabilities cells (create, fetch, compare,
alter, drop scratch tables named from [m3] through the registered DdlGenerators,
Fetchers and Comparators), prints the object-kind x engine matrix, the M3 table
and the connection table, and writes target/conformance/report.json.

  --allow-missing  report unreachable fixtures as skipped instead of failing
                   (same as OXBASE_CONFORMANCE_ALLOW_MISSING=1)
  --tcp-only       skip protocol handshakes (same as OXBASE_CONFORMANCE_TCP_ONLY=1)
  --skip-connect   skip the connection checks (same as OXBASE_CONFORMANCE_SKIP_CONNECT=1)
  --skip-fetch     skip the fetch_list cells (same as OXBASE_CONFORMANCE_SKIP_FETCH=1)
  --skip-roundtrip skip the roundtrip / capabilities cells
                   (same as OXBASE_CONFORMANCE_SKIP_ROUNDTRIP=1)";

fn main() -> ExitCode {
    let mut opts = runner::RunOptions::from_env();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--allow-missing" => opts.allow_missing = true,
            "--tcp-only" => opts.probe.handshake = false,
            "--skip-connect" => opts.skip_connect = true,
            "--skip-fetch" => opts.skip_fetch = true,
            "--skip-roundtrip" => opts.skip_roundtrip = true,
            "-h" | "--help" => {
                return match write_stdout(&format!("{USAGE}\n")) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("error: cannot write to stdout: {e}");
                        ExitCode::from(2)
                    }
                };
            }
            other => {
                eprintln!("unknown argument {other:?}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    let path = fixtures::fixtures_path();
    let report = match runner::run_path(&path, &opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    // Write the JSON first so a closed stdout cannot stop it from being written.
    let out = report::default_report_path();
    if let Err(e) = report.write_json(&out) {
        eprintln!("error: cannot write {}: {e}", out.display());
        return ExitCode::from(2);
    }
    let text = format!("{}JSON report: {}\n", report.render(), out.display());
    if let Err(e) = write_stdout(&text) {
        eprintln!("error: cannot write to stdout: {e}");
        return ExitCode::from(2);
    }

    match report.gate() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("\n{msg}");
            ExitCode::FAILURE
        }
    }
}

/// Writes to stdout without panicking. A reader that closes the pipe early (`| head`) is not an
/// error: the rest of the output is dropped and the exit code still reflects the run.
fn write_stdout(text: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}
