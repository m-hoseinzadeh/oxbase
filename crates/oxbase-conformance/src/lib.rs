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
//! cells without a registered Fetcher stay pending. Since M3 the `roundtrip` and `capabilities`
//! cells of the designer kinds (table, column, index, foreign key, check) run ([`roundtrip`]):
//! create → fetch → compare → alter → fetch → diff → drop on scratch tables named from
//! `tests/fixtures.toml` `[m3]`, and the designer capabilities checked against what the round
//! trip and two live probes saw; cells whose engine has not registered the roles stay pending.
//! Since M4 SQL Server runs all of these: its `fetch_list` cells list the `[m2]` names (schema
//! `m2`) plus the `[m4]` types and indexed-view children ([`seeds::m4_groups`]), its round trip
//! uses `[m3.mssql]` (schema `m3`), and [`guard`] is the 5-engine exit check.
//! Since M7 MongoDB and Redis run the connection checks through their drivers (statements per
//! engine in [`connect::Statements`]: JSON requests on MongoDB, console lines on Redis, the
//! `[m7]` seed's big collection / list for streaming and cancel), their `fetch_list` cells list
//! the `[m7.mongodb]` / `[m7.redis]` seed ([`seeds::m7_mongo_groups`],
//! [`seeds::m7_redis_groups`]) once their dialect cells are registered (pending until then), and
//! their `roundtrip` / `capabilities` cells are not applicable (no DDL designer,
//! [`matrix::has_no_designer`]); [`guard`] has the 7-engine exit check.
//! Results go to stdout and to `target/conformance/report.json` ([`report`]).
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
//! | `OXBASE_CONFORMANCE_SKIP_ROUNDTRIP=1` | Skip the M3 `roundtrip` / `capabilities` cells (reported as skipped) |
//! | `OXBASE_CONFORMANCE_REQUIRE_M2_FETCH=1` | Live test only: every M2 engine must register its tree layout ([`fetch::gate`]); set by the full CI lane |
//! | `OXBASE_CONFORMANCE_REQUIRE_M3_ROUNDTRIP=1` | Live test only: every `roundtrip` and `capabilities` cell of the designer kinds on PostgreSQL, MySQL, MariaDB and SQLite must pass, and no designer role may be missing ([`roundtrip::gate`]); the integrator sets it in the conformance CI step once the engine streams are merged |
//! | `OXBASE_CONFORMANCE_REQUIRE_M4=1` | Live test only, the M4 exit check ([`guard`]): every M1-M3 cell of PostgreSQL, MySQL, MariaDB, SQLite **and SQL Server** must pass (connection checks, `fetch_list` of every tree-layout kind, `roundtrip` / `capabilities` of the designer kinds), nothing skipped; the integrator sets it in the conformance CI step once the SQL Server dialect cells are registered |
//! | `OXBASE_CONFORMANCE_REQUIRE_M7=1` | Live test only, the M7 exit check ([`guard`]): the M4 guard plus MongoDB and Redis, so all seven engines must pass every applicable cell (connection checks, `fetch_list` of every tree-layout kind against the `[m7]` seed), nothing skipped; the integrator sets it once the MongoDB and Redis dialect cells are registered |
//! | `CARGO_TARGET_DIR` | Where `conformance/report.json` is written (default `<workspace>/target`) |

pub mod adapter;
pub mod connect;
pub mod fetch;
pub mod fixtures;
pub mod guard;
pub mod matrix;
pub mod probe;
pub mod report;
pub mod roundtrip;
pub mod runner;
pub mod seed_check;
pub mod seeds;
