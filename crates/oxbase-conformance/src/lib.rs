//! Cross-engine conformance test harness.
//!
//! Reads connection parameters from `tests/fixtures.toml` (the single source of truth) and runs
//! the create -> fetch -> alter -> diff -> drop round-trip matrix against every engine.
//!
//! The matrix is `engine x object kind x check` (see [`matrix`]). At M0 there are no drivers, so
//! the harness only proves each fixture is reachable ([`probe`]) and reports every applicable
//! cell as `pending (no driver yet)`. Results go to stdout as a table and to
//! `target/conformance/report.json` ([`report`]).
//!
//! Environment:
//!
//! | Variable | Effect |
//! |---|---|
//! | `OXBASE_FIXTURES` | Path to `fixtures.toml` (relative paths resolve against the workspace root) |
//! | `OXBASE_CONFORMANCE_ALLOW_MISSING=1` | Unreachable fixtures are reported as skipped instead of failing |
//! | `OXBASE_CONFORMANCE_PROBE_TIMEOUT_MS` | Per-engine connect/handshake timeout (default 3000) |
//! | `OXBASE_CONFORMANCE_TCP_ONLY=1` | Skip protocol handshakes; a TCP connect is enough |
//! | `CARGO_TARGET_DIR` | Where `conformance/report.json` is written (default `<workspace>/target`) |

pub mod adapter;
pub mod fixtures;
pub mod matrix;
pub mod probe;
pub mod report;
pub mod runner;
