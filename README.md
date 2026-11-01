# Oxbase

Oxbase is a cross-platform desktop database client written in Rust. It has a Tauri 2 shell and a
React + TypeScript UI. v1 targets PostgreSQL, MySQL, MariaDB, SQLite, SQL Server, MongoDB and
Redis on Linux, macOS and Windows. The core is headless, so every feature also works from
`oxbase-cli` and from tests. See [PLAN.md](PLAN.md) for the architecture and milestones.

## What works (M3)

On PostgreSQL, MySQL, MariaDB and SQLite (M1 connections: plain, TLS, SSH, keychain secrets):

- **Object tree**: connection, databases, schemas (PostgreSQL), then folders for tables, views,
  materialized views, functions, procedures and sequences; under a table its columns, indexes,
  foreign keys, check constraints and triggers. Children load when a node is first expanded
  and loading can be cancelled. System objects are hidden unless you turn them on. Other
  PostgreSQL databases are read on their own metadata session.
- **Query editor** (Monaco): run the whole script, the selection, or the statement at the cursor
  (`oxbase-sql` splits the script for each dialect: dollar quotes, `DELIMITER`, compound
  `BEGIN ... END` bodies, SQLite trigger bodies). Each editor tab has its own session, so a long
  query never blocks the tree.
- **Streaming results grid** (Glide Data Grid): rows stream into a server-side result store
  that spills to Arrow IPC files past 64 MiB; the grid fetches only the visible pages as Arrow
  IPC bytes, never JSON. Several result sets per run each get a tab, with a messages tab for
  notices, rows affected and errors. Stop cancels the server-side statement and keeps the rows
  already received. The next run starts right away: if the server is still stopping the
  cancelled statement (MySQL and MariaDB can take several seconds to finish a killed streaming
  query), it runs on a new session and a warning says that the old session's temporary
  tables, settings and open transaction are gone.
- **History**: every statement is recorded in the app database with its status, duration and
  row count. Password and key literals are masked first. You can search, reopen, delete, clear,
  or turn history off.

- **Table data** (M3): double-click a table in the tree to open its rows. **Filter & sort**
  builds nested AND / OR / NOT groups of conditions (equals, ranges, `LIKE`-style contains /
  starts / ends with, `IN`, `BETWEEN`, `IS NULL`) and sort keys with NULLS first / last; raw SQL
  conditions are allowed but marked and checked to be one expression. Every value is sent as a
  bound parameter, never formatted into SQL.
- **Grid editing** (M3): a table with a primary key, or a NOT NULL unique key, is editable;
  otherwise (no key, a view, a query result) the grid is read-only and says why. Edit cells in
  a typed editor (NULL is not the empty string, `DEFAULT` for new rows), add, duplicate,
  delete and revert rows; changes stay pending until **Apply**, which runs them in **one
  transaction** and requires every update and delete to hit exactly one row, so a row changed
  or deleted by someone else rolls the whole apply back. Rows are read back after the commit
  (generated columns and keys included). **Form** and **JSON** views show the selected rows.
- **Table designer** (M3): **Design** or **New table** in the Objects panel. Tabs for columns,
  indexes, foreign keys, checks and options; renames are tracked as renames, not drop + add.
  The preview shows the exact script, which statements are destructive (they need a
  confirmation), and whether it runs in one transaction (PostgreSQL, SQLite) or step by step
  (MySQL and MariaDB commit each DDL statement). SQLite changes it cannot make in place run as
  the table rebuild in one transaction, keeping rows, indexes, triggers and views, with foreign
  keys switched off only for the copy. Apply refuses when the table changed on the server since
  it was loaded. What each engine and version supports comes from its capabilities.

Contracts: [docs/m3-contracts.md](docs/m3-contracts.md). SQL Server follows in M4.

## Layout

```
crates/
  oxbase-types, oxbase-driver, oxbase-dialect     core types and traits
  oxbase-pg, oxbase-mysql, oxbase-sqlite,         one driver + dialect crate per engine
  oxbase-mssql, oxbase-mongo, oxbase-redis
  oxbase-net                                      SSH tunnels, TLS
  oxbase-sql, oxbase-insight,                     SQL text processing, code insight
  oxbase-schema-cache
  oxbase-designer, oxbase-diff, oxbase-transfer,  feature crates
  oxbase-io, oxbase-backup, oxbase-model,
  oxbase-bi, oxbase-ai, oxbase-jobs
  oxbase-store                                    app DB (history), documents, keychain secrets
  oxbase-services                                 orchestration shared by both front ends
  oxbase-cli                                      headless runner (binary: `oxbase`)
  oxbase-conformance                              cross-engine conformance harness
  app-tauri                                       Tauri 2 shell (no logic)
ui/                                               React + TS + Vite front end
tests/fixtures.toml                               test DB connection params and M2/M3 seed object
                                                  names (single source of truth)
docs/                                             frozen contracts per milestone (m1-contracts.md,
                                                  m2-contracts.md, m3-contracts.md)
```

## Prerequisites

- Rust stable (selected by `rust-toolchain.toml`; MSRV 1.90).
- Node 24 and pnpm 12.15 (the versions CI uses), for `ui/`.
- Docker with the compose v2 plugin, for the engine fixtures. Keep at least ~2 GB of disk free:
  MongoDB refuses to start with less than 500 MB available.
- Linux only, to build `app-tauri`: the WebKitGTK stack. On Debian/Ubuntu,
  `bash scripts/ci-install-linux-deps.sh` installs everything CI installs
  (`libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev libayatana-appindicator3-dev
  librsvg2-dev build-essential file patchelf pkg-config libssl-dev libxdo-dev`).
  Check with `pkg-config --exists webkit2gtk-4.1`.

## Checks

These are the checks of `.github/workflows/ci.yml`, plus a few manual ones. For the M2
integration (2026-10-31) the CI ones passed in GitHub Actions on Linux x86_64 (fmt, clippy,
`cargo test --workspace`, keychain, UI lint/typecheck/test/build, Docker conformance,
`cargo deny`). `cargo doc`, `cargo build -p app-tauri`, `cargo run -p oxbase-cli` and the
`cargo tree` checks are not CI jobs; they were last run locally for the M1 integration
(`cargo deny` 0.20.2).

```sh
# Rust
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked
cargo build -p app-tauri --locked          # proves the Tauri shell links
                                           # (verified via `pnpm --dir ../../ui tauri build --debug --no-bundle`
                                           #  from crates/app-tauri, which runs the UI build first)
cargo run -p oxbase-cli -- --version          # prints "oxbase 0.0.1"
cargo deny check                              # licenses, advisories, bans, sources
cargo tree -i openssl-sys; cargo tree -i aws-lc-sys   # both must find nothing (rustls + ring only)
scripts/keychain-tests.sh                     # real OS keychain tests (see Conformance harness)

# UI (from ui/)
pnpm install --frozen-lockfile
node ../scripts/ci-check-tauri-versions.mjs   # Tauri crate and npm versions agree
pnpm run lint
pnpm run typecheck
pnpm run test
pnpm run build
```

Without the WebKitGTK libraries, leave out the shell with the aliases in `.cargo/config.toml`:
`cargo lint`, `cargo test-core` and `cargo check-core` (each is the command above with
`--exclude app-tauri`). `cargo build` with no `-p` builds the default members, which also leave
out `app-tauri`. Jobs that compile `app-tauri` without building the UI first need
`node scripts/ci-ensure-frontend-dist.mjs`, which creates an empty `ui/dist`.

## Test fixtures

Every engine runs in Docker under the compose project `oxbase-fixtures`
(`tests/docker/compose.yml`, seeded from `tests/docker/init/<engine>/`). Connection parameters
live in `tests/fixtures.toml`, the single source of truth: host ports 15400-15499, bound to
127.0.0.1, test-only credentials.

```sh
scripts/fixtures-up.sh            # start all engines, wait until healthy and seeded
scripts/fixtures-up.sh postgres mysql mariadb ssh   # or only some services (the M1 set)
scripts/fixtures-status.sh        # state and health of each service
scripts/fixtures-down.sh          # remove the project's containers, network and volumes
scripts/fixtures-certs.sh [--force]   # (re)generate the test PKI and SSH keys; fixtures-up runs it
```

TLS and SSH (M1):

- **Test PKI** (`[tls]`): `scripts/fixtures-certs.sh` (OpenSSL 1.1.1+/3.x and ssh-keygen) writes a
  CA, a server certificate for `localhost`, `127.0.0.1`, `postgres`, `mysql` and `mariadb`, and a
  client certificate (CN `oxbase_tls`) to `target/fixtures/tls`. PostgreSQL, MySQL and MariaDB
  run with TLS on. Each has a user `oxbase_tls` that the server accepts **only** over TLS
  (`[engines.<name>.tls_user]`); the normal `oxbase` user works with and without TLS.
- **SSH bastion** (`[ssh]`): the `ssh` service, an OpenSSH server on `127.0.0.1:15422` with user
  `oxbase` (password, plain key `id_ed25519` and passphrase key `id_ed25519_enc`, all in
  `target/fixtures/ssh`). Its host key is fixed, so `known_hosts` (Strict) and
  `host_key.fingerprint` (pinning) work. Tunnels reach the engines by their compose service
  names (`[ssh.targets]`: `postgres:5432`, `mysql:3306`, `mariadb:3306`), so `VerifyFull`
  through a tunnel checks the certificate against the service name, not `127.0.0.1`.
- `fixtures-up.sh` recreates the services that mount this material when it was regenerated.

M2 seed objects: `[m2]` in `tests/fixtures.toml` names the objects every engine's init script
creates (`m2_parent`, `m2_child`, `m2_view`, `m2_fn`, `m2_proc`, `m2_trg`, `m2_seq`, PostgreSQL
`m2_mview` in schema `m2` and a second database `oxbase_m2_other`), so the fetcher and tree tests
agree on names. SQLite has no container: its tests apply `tests/docker/init/sqlite/01-m2.sql`
to a temporary file.

The scripts only touch the `oxbase-fixtures` project. `OXBASE_FIXTURES_TIMEOUT` (default 300 s) sets
how long `fixtures-up.sh` waits. CI uses `scripts/ci-fixtures.sh up|logs|down` on the same
project.

## Conformance harness

`crates/oxbase-conformance` prints two tables and writes both to
`target/conformance/report.json` (report schema 2):

1. **Object-kind matrix** (engine x object kind x check, PLAN.md §3). Rows and columns are
   `oxbase_types::ObjectKind` and `oxbase_types::Engine`; a cell is n/a when the registry of
   `oxbase_services::default_dialects()` (the one the app uses) marks it not applicable. Each
   cell shows three checks: `roundtrip` / `fetch_list` / `capabilities`. From M2, `fetch_list`
   runs the registered `Fetcher` of every (engine, kind): it lists the `[m2]` seed objects of
   `tests/fixtures.toml` under their parent, checks the refs, the order and `system = false`,
   and loads each one (the definition must survive a JSON round trip). From M3, `roundtrip`
   and `capabilities` run for table, column, index, foreign key and check: on scratch tables
   `m3_rt_<kind>_<token>` the harness creates, fetches and compares (the diff must be empty),
   then alters step by step (for columns: add, fill, NOT NULL, default, comment, type, rename,
   drop), fetching and comparing after every step, with a seeded row (quotes, a backslash,
   non-ASCII text) that must survive every alter, a reserved-word column `order` and a column
   renamed to `sel"ect_ü`, and finally drops. `capabilities` checks that what the engine's
   `DesignerCapabilities` claims (native, rebuild, unsupported; transactional DDL and check
   enforcement, both probed) matches what the generator does. Other kinds stay pending;
   mssql waits for M4, mongodb and redis for M7. The harness also proves each fixture answers
   its handshake.
2. **Connection checks** (M1), through the real drivers, `ConnectionManager` and russh provider:
   `connect` (plain, server version equals `version_prefix`), `tls` (`VerifyFull` with the
   fixture CA as `oxbase_tls`, `tls_in_use`; plaintext and a wrong server name refused), `ssh`
   (bastion, password auth, Strict known_hosts), `tls_over_ssh` (encrypted-key auth plus
   `VerifyFull` against the service name; a wrong name refused), `cancel` (`pg_sleep(30)` /
   `SLEEP(30)` / an endless recursive CTE ends with `Cancelled` and the session answers
   `SELECT 1` within 2 s, via the token and via `cancel_running`) and `stream_200k` (200 000
   generated rows arrive in more than one batch). SQLite has no TLS/SSH columns; mssql, mongodb
   and redis show `pending (M4)` / `pending (M7)`.

```sh
scripts/fixtures-up.sh
cargo test -p oxbase-conformance --locked -- --include-ignored --nocapture --test-threads 1   # prints both tables
cargo run -p oxbase-conformance                  # same tables, writes target/conformance/report.json
scripts/fixtures-down.sh
```

The other live tests also need the fixtures: driver, tunnel and connection-manager scenarios
(M1), the dialect fetcher tests of each engine crate, and the query-run tests of
`oxbase-services` (M2: a 1 000 000-row query that spills to disk, random pages, cancel
mid-stream, multi-statement scripts, history). The query-run tests print their timings to stderr
with a `[m2 query]` prefix:

```sh
cargo test -p oxbase-net -p oxbase-pg -p oxbase-mysql -p oxbase-sqlite -p oxbase-services \
    --locked -- --include-ignored --test-threads 1
```

The OS keychain tests (`keychain_smoke` and `keychain_long_values` in `oxbase-store`,
`keychain_backed_profile_round_trip` in `oxbase-services`, which saves and opens a profile through
`ConnectionManager` on top of `KeyringSecretStore`) are `#[ignore]`d and also need
`OXBASE_KEYCHAIN_TESTS=1`. Run them with:

```sh
scripts/keychain-tests.sh     # cargo test -p oxbase-store -p oxbase-services -- --ignored keychain
```

macOS and Windows use the login keychain / Credential Manager. On Linux the script starts a
private D-Bus session with its own gnome-keyring (throwaway keyring in a temp dir; needs
`dbus-run-session` and `gnome-keyring-daemon`), so it works without a desktop session and never
touches your keyrings; `OXBASE_KEYCHAIN_USE_SESSION=1` uses your current session instead. Any
command can follow the script name, e.g.
`scripts/keychain-tests.sh cargo test -p oxbase-services -- --include-ignored --test-threads 1`
runs the services live tests with the keychain test included. CI runs the script on Linux, macOS
and Windows (`keychain` job).

Last verified 2026-11-01 in the GitHub Actions conformance job (Linux x86_64, M3 integration,
run 41254616693) with all 7 fixtures up: every fixture reachable (PostgreSQL 18.6, MySQL
8.4.13, MariaDB 11.8.9, SQL Server 2025, MongoDB 7.0.45, Redis, SQLite 3.54.1 file).

- Object-kind matrix, 420 cells: **82 pass, 0 fail**, 158 pending, 180 n/a. The 42
  `fetch_list` passes are every M2 cell: PostgreSQL 13 (database, schema, table, column,
  index, foreign key, check, view, materialized view, function, procedure, trigger,
  sequence), MySQL 10 and MariaDB 11 (the same without schema and materialized view; sequence
  on MariaDB only), SQLite 8 (database, table, column, index, foreign key, check, view,
  trigger). The 40 M3 passes are `roundtrip` and `capabilities` for table, column, index,
  foreign key and check on each of PostgreSQL, MySQL, MariaDB and SQLite (the M3 exit check;
  CI sets `OXBASE_CONFORMANCE_REQUIRE_M3_ROUNDTRIP=1`, so a pending or failing one fails the
  job). The only skipped operations are the ones the contract allows: table and column
  comments on SQLite.

  | Engine | table | column | index | foreign key | check |
  |---|---|---|---|---|---|
  | PostgreSQL | pass / pass | pass / pass | pass / pass | pass / pass | pass / pass |
  | MySQL | pass / pass | pass / pass | pass / pass | pass / pass | pass / pass |
  | MariaDB | pass / pass | pass / pass | pass / pass | pass / pass | pass / pass |
  | SQLite | pass / pass | pass / pass | pass / pass | pass / pass | pass / pass |

  (`roundtrip` / `capabilities`.) SQLite does NOT NULL, default, foreign key and check changes
  as the table rebuild; MySQL and MariaDB DDL is not transactional (probed).
- Connection checks, 42: 21 pass (postgres, mysql, mariadb, sqlite), 18 pending (mssql M4,
  mongodb/redis M7), 3 n/a (SQLite TLS/SSH), 0 fail.
- Query runs (`oxbase-services` live tests, 8 MiB memory budget so the result spills; CI run
  41229578451. In a second run, 41230261610, the 1M-row stream took 1645 / 1778 / 2247 / 877 ms
  (same engine order) and every other figure stayed in the same range):

  | Engine | 1M rows streamed | first page | random 200-row page p95 | spill | peak batch memory | cancel after ~230-250k rows: run ended |
  |---|---|---|---|---|---|---|
  | PostgreSQL | 1698 ms | 9 ms | 0.17 ms | 28 MiB | 7.6 MiB | 1 ms after cancel |
  | MySQL | 1885 ms | 52 ms | 0.23 ms | 28 MiB | 7.8 MiB | 1013 ms after cancel |
  | MariaDB | 1625 ms | 36 ms | 0.28 ms | 28 MiB | 7.8 MiB | 1001 ms after cancel |
  | SQLite | 885 ms | 3 ms | 0.29 ms | 21 MiB | 6.6 MiB | 0 ms after cancel |

  After the cancel, the editor's next `SELECT 1` must finish within 2 s. In CI run
  41231834815 it took 455 ms (PostgreSQL), 3 ms (MySQL), 3 ms (MariaDB) and 1 ms (SQLite).
  On MySQL and MariaDB the driver was still stopping the killed stream after 1 s, so that
  `SELECT 1` ran on a new session. On PostgreSQL and SQLite it ran on the same session.

The dialect, driver, tunnel and connection-manager live tests and the keychain tests passed in
the same CI run, as did the M3 live tests: the DDL / DML cells of each engine
(`oxbase-pg` `m3_live` 10, `oxbase-mysql` `m3_live` 19 for MySQL and MariaDB, `oxbase-sqlite`
`m3` 16) and the `oxbase-services` edit and designer tests on all four engines (open
`m3_edit_pk` filtered and sorted; insert / update / delete with read-back; a conflict when
another session deleted the row; `m3_edit_uq` editable by `code`; `m3_edit_nokey` and
`m3_edit_view` read-only; designer create / alter / reload, and a conflict when the table
changed in between). CI runs them with `OXBASE_SERVICES_REQUIRE_M3_CELLS=1`, so a missing
engine cell or seed table fails instead of reporting "pending".

Disk: the full stack needs about 4-5 GB on top of the images already present (the SQL Server,
MongoDB and Redis images alone are about 4 GB, and SQL Server keeps writing while it runs).
With less free space, start only the M1 set (`scripts/fixtures-up.sh postgres mysql mariadb
ssh`) and run the harness with `OXBASE_CONFORMANCE_ALLOW_MISSING=1`; MongoDB refuses to start
below 500 MB free. `cargo clean` frees the shared `target/` (a full debug build of the
workspace, tests and app is about 6-7 GB).

Plain `cargo test` skips the live tests (they are `#[ignore]`). An unreachable fixture fails the
run; `OXBASE_CONFORMANCE_ALLOW_MISSING=1` (or `--allow-missing` on the binary) reports it as skipped
instead. A failed connection check always fails the run. Other knobs:
`OXBASE_CONFORMANCE_PROBE_TIMEOUT_MS`, `OXBASE_CONFORMANCE_TCP_ONLY=1`,
`OXBASE_CONFORMANCE_SKIP_CONNECT=1` (or `--skip-connect`: no connection checks),
`OXBASE_CONFORMANCE_REQUIRE_M2_FETCH=1` and `OXBASE_CONFORMANCE_REQUIRE_M3_ROUNDTRIP=1` (a
pending M2 / M3 cell fails; CI sets both).

## App data

Profiles live in `connections.json` in the config directory (`OXBASE_CONFIG_DIR` overrides it).
From M2 the app database `oxbase.db` (query history and settings, SQLite with versioned
migrations) lives in the local data directory: `~/.local/share/oxbase` on Linux,
`~/Library/Application Support/dev.oxbase.app` on macOS, `%LOCALAPPDATA%\oxbase` on Windows
(`OXBASE_DATA_DIR` overrides it). Query results that do not fit in memory spill to Arrow IPC
files under `<temp>/oxbase-results/run-*`, deleted when the run or the app closes. History stores
statements with password literals masked and can be turned off.

## Desktop app

The Tauri 2 shell is `crates/app-tauri`; the UI is `ui/` (see [ui/README.md](ui/README.md)).
From `ui/`:

```sh
pnpm install
pnpm tauri dev                            # dev app with hot reload
pnpm tauri build --debug --no-bundle      # debug binary, no installers
pnpm tauri build                          # release bundles for the current OS
```

### Trying the query editor

1. Start a fixture (`scripts/fixtures-up.sh postgres`) or use any reachable server or SQLite
   file. In **Connections**, add a profile (the PostgreSQL fixture is `127.0.0.1:15432`,
   database `oxbase_test`, user `oxbase`; see `tests/fixtures.toml`) and click **Connect**.
2. **Objects** shows the open connection. Expand it to browse databases, schemas and the
   per-kind folders (the fixture seeds schema `m2`). F5 refreshes, and **System objects**
   shows the catalog schemas and other system objects.
3. Click **New query** in the top toolbar, or in the **Objects** panel with a node selected, so
   the editor opens on that connection and database. The editor toolbar can switch both.
4. Run with **Run** (Ctrl+Shift+Enter, whole script), **Run current** (Ctrl+Enter, the
   statement at the cursor) or **Run selection**. To see streaming and spilling, run
   `SELECT g FROM generate_series(1, 1000000) g;` on PostgreSQL and scroll the grid while rows
   arrive. **Stop** cancels it and keeps the rows already received.
5. **History** lists past statements. Search them, **Open in editor**, **Delete**, clear them
   all, or turn history off.

### Editing data and designing tables

1. With the fixture, expand `oxbase_test` > `m3` > **Tables** and double-click `m3_edit_pk`.
   **Filter & sort** narrows and orders the rows (the query is re-run on the server).
2. Double-click a cell (or start typing) to edit it, **Add row** / **Duplicate row** /
   **Delete row** to change rows, **Revert row** to drop a row's pending changes. Pending
   changes are coloured; **Apply** writes them all in one transaction, **Discard** drops them.
   `m3_edit_nokey` and `m3_edit_view` open read-only and say why. **Form** and **JSON** show
   the selected rows.
3. **Design** (in the tab, or in the Objects panel with a table selected) opens the table
   designer; **New table** starts an empty one. Change columns, indexes, foreign keys, checks
   or options, then **Preview** shows the script. **Apply** runs it (destructive statements
   need a confirmation) and reloads the table.

### Webview smoke test

`ui/e2e/smoke.mjs` drives the built app through its real webview (WebKitGTK, Linux only) with
`tauri-driver` and `WebKitWebDriver`: connect, a 1 000 000-row query, Arrow pages decoded in the
webview, scrolling, page latency, cancel, and a table-data cell edit applied and seen after a
reload. See [ui/e2e/README.md](ui/e2e/README.md). Locally (needs `webkit2gtk-driver`,
`cargo install tauri-driver --locked --version 2.1.0`, Docker, and `xvfb-run` without a
display):

```sh
(cd ui && pnpm install --frozen-lockfile && pnpm run build)
cargo build -p app-tauri --locked --features tauri/custom-protocol
scripts/e2e-fixtures.sh up
scripts/e2e-smoke.sh            # results in target/e2e (e2e-perf.json, failure screenshot)
scripts/e2e-fixtures.sh down
```

In CI (job **Webview smoke (Linux)**, run 41254616693) page requests through Tauri IPC plus
Arrow decoding in the webview took p50 2 ms / p95 11 ms / max 11 ms (the grid's own 7 pages,
`window.__oxbasePerf`; 60 random 200-row probe pages: p50 1 ms, p95 2 ms, max 6 ms; limit
p95 < 250 ms); Stop ended a streaming query 33 ms after the click; the edit step committed
and showed the new value after a reload.

## CI

- `.github/workflows/ci.yml` has two lanes. **Fast lane**, on pushes to `wip/**` branches:
  Linux only (lint, tests, keychain, UI, Docker conformance, cargo-deny), about 10-15 minutes.
  **Full lane**, on pull requests, pushes to main, weekly and manual runs: fmt + clippy on Linux;
  `cargo test --workspace` on Linux, macOS and Windows; the OS keychain tests
  (`scripts/keychain-tests.sh`) on Linux, macOS and Windows; UI lint, typecheck, test and build;
  the conformance harness and the M1-M3 live tests against the Docker fixtures; the webview
  smoke test (Linux; also on the fast lane of `wip/*-e2e`, `wip/*-app` and `wip/*-integrate`);
  `cargo deny` (advisories, bans, licenses, sources).
- `.github/workflows/bundle.yml` (push to main, manual): Tauri dev bundles for Linux x86_64,
  Windows x86_64 and macOS aarch64/x86_64, uploaded as workflow artifacts.
  `scripts/ci-verify-signing.mjs` checks what was produced: `codesign --verify --deep --strict`,
  `spctl -a -t exec` and `xcrun stapler validate` on the macOS `.app`, and
  `Get-AuthenticodeSignature` (must be `Valid`, timestamped) on the Windows `.msi`, NSIS `.exe`
  and app `.exe`. Linux has no OS code signing; its bundles are listed with SHA-256.
  Signing is **opt-in** until the project has Apple and Windows certificates. Turn it on with
  the repository variable `OXBASE_REQUIRE_SIGNING=true` (push) or by ticking `require_signing`
  (manual run); the job then fails before the build if this OS's secrets are missing and after
  it if a bundle is unsigned, ad-hoc signed or unnotarized. Secrets: `APPLE_CERTIFICATE`,
  `APPLE_CERTIFICATE_PASSWORD`, `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`, `WINDOWS_CERTIFICATE`,
  `WINDOWS_CERTIFICATE_PASSWORD` (optional: `APPLE_SIGNING_IDENTITY`,
  `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`); see the header of the
  workflow file.

### Verifying changes in CI

Maintainers build and test in GitHub Actions rather than locally. Work in progress goes to a
`wip/<milestone>-<stream>` branch; each milestone lands on main as one squash-merged pull
request from a `milestone/<id>` branch, which runs the full lane. To push and wait for the result:

```bash
node scripts/ci-wait.mjs --push            # push HEAD to the current wip/* branch, wait for CI
node scripts/ci-wait.mjs --sha <commit>    # just wait for (or report) the run on a commit
```

It prints one line per job and, for failed jobs, only the error lines of the log (compiler
errors, failed tests, panics, `##[error]`). Exit code 0 means all green. It uses `GITHUB_TOKEN`
or the github.com credential that `git push` uses, and never pushes to main.

## License

Oxbase is licensed under the [Apache License, Version 2.0](LICENSE). See [NOTICE](NOTICE).

Contributions are welcome under the [Contributor License Agreement](CLA.md); see
[CONTRIBUTING.md](CONTRIBUTING.md). Oxbase is developed as open core: this repository is fully
open source, and any future paid features live in a separate repository that builds on it.
