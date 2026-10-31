//! Cross-engine conformance test harness.
//!
//! Reads connection parameters from `tests/fixtures.toml` (the single source of truth) and runs
//! the create -> fetch -> alter -> diff -> drop round-trip matrix against every engine.
//!
//! The matrix is `engine x object kind x check` (see [`matrix`]), with applicability and
//! fetchers from `oxbase_services::default_dialects()` ([`adapter`]). The harness proves each
//! fixture is reachable ([`probe`]). Since M1 it also runs the connection checks of [`connect`]
//! (plain, TLS, SSH, TLS over SSH, cancel, streaming) through the real drivers and connection
//! manager, as a second table. Since M2 the `fetch_list` cells run ([`fetch`]): every registered
//! Fetcher lists the seed objects of `tests/fixtures.toml` `[m2]` ([`seeds`]) and loads them;
//! cells without a registered Fetcher stay pending. `roundtrip` and `capabilities` stay pending
//! (M3). Results go to stdout and to `target/conformance/report.json` ([`report`]).
//!
//! Environment:
//!
//! | Variable | Effect |
//! |---|---|
//! | `OXBASE_FIXTURES` | Path to `fixtures.toml` (relative paths resolve against the workspace root) |
//! | `OXBASE_CONFORMANCE_ALLOW_MISSING=1` | Unreachable fixtures are reported as skipped instead of failing |
//! | `OXBASE_CONFORMANCE_PROBE_TIMEOUT_MS` | Per-engine connect/handshake timeout (default 3000) |
//! | `OXBASE_CONFORMANCE_TCP_ONLY=1` | Skip protocol handshakes; a TCP connect is enough |
//! | `OXBASE_CONFORMANCE_SKIP_CONNECT=1` | Skip the M1 connection checks (reported as skipped) |
//! | `OXBASE_CONFORMANCE_SKIP_FETCH=1` | Skip the M2 `fetch_list` cells (reported as skipped) |
//! | `CARGO_TARGET_DIR` | Where `conformance/report.json` is written (default `<workspace>/target`) |

pub mod adapter;
pub mod connect;
pub mod fetch;
pub mod fixtures;
pub mod matrix;
pub mod probe;
pub mod report;
pub mod runner;
pub mod seeds;
