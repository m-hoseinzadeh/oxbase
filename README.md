# Oxbase

Oxbase is a cross-platform desktop database client written in Rust. It has a Tauri 2 shell and a
React + TypeScript UI. v1 targets PostgreSQL, MySQL, MariaDB, SQLite, SQL Server, MongoDB and
Redis on Linux, macOS and Windows. The core is headless, so every feature also works from
`oxbase-cli` and from tests. See [PLAN.md](PLAN.md) for the architecture and milestones.

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
  oxbase-store                                    app DB, documents, keychain secrets
  oxbase-services                                 orchestration shared by both front ends
  oxbase-cli                                      headless runner (binary: `oxbase`)
  oxbase-conformance                              cross-engine conformance harness
  app-tauri                                       Tauri 2 shell (no logic)
ui/                                               React + TS + Vite front end
tests/fixtures.toml                               test DB connection params (single source of truth)
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

These are the commands CI runs (`.github/workflows/ci.yml`). All of them were run and passed on
Linux x86_64 (Ubuntu, WebKitGTK 4.1 installed) on 2026-10-31.

```sh
# Rust
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build -p app-tauri --locked          # proves the Tauri shell links
                                           # (verified via `pnpm --dir ../../ui tauri build --debug --no-bundle`
                                           #  from crates/app-tauri, which runs the UI build first)
cargo run -p oxbase-cli -- --version          # prints "oxbase 0.0.1"

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
scripts/fixtures-up.sh redis      # or only some services
scripts/fixtures-status.sh        # state and health of each service
scripts/fixtures-down.sh          # remove the project's containers, network and volumes
```

The scripts only touch the `oxbase-fixtures` project. `OXBASE_FIXTURES_TIMEOUT` (default 300 s) sets
how long `fixtures-up.sh` waits. CI uses `scripts/ci-fixtures.sh up|logs|down` on the same
project.

## Conformance harness

`crates/oxbase-conformance` runs the engine x object kind x check matrix (PLAN.md §3) against the
fixtures. Rows and columns are `oxbase_types::ObjectKind` and `oxbase_types::Engine`; a cell is n/a
when the `oxbase_dialect::DialectRegistry` marks it not applicable. At M0 there are no drivers, so
it only proves each fixture answers its protocol handshake and reports every applicable cell
as pending.

```sh
scripts/fixtures-up.sh
cargo test -p oxbase-conformance --locked -- --include-ignored --nocapture   # live tests, prints the matrix
cargo run -p oxbase-conformance                                     # same matrix, writes target/conformance/report.json
scripts/fixtures-down.sh
```

Last verified 2026-10-31 on Linux x86_64: all 7 fixtures reachable (PostgreSQL 18.6, MySQL
8.4.13, MariaDB 11.8.9, SQL Server 2025, MongoDB 8.0.34, Redis 8.10, SQLite file); matrix of
420 cells = 237 pending, 183 n/a, 0 fail. Fixtures need roughly 1-2 GB of free disk for their
volumes (MongoDB refuses to start below 500 MB); `cargo clean` frees the shared `target/`.

Plain `cargo test` skips the live tests (they are `#[ignore]`). An unreachable fixture fails the
run; `OXBASE_CONFORMANCE_ALLOW_MISSING=1` (or `--allow-missing` on the binary) reports it as skipped
instead. Other knobs: `OXBASE_CONFORMANCE_PROBE_TIMEOUT_MS`, `OXBASE_CONFORMANCE_TCP_ONLY=1`.

## Desktop app

The Tauri 2 shell is `crates/app-tauri`; the UI is `ui/` (see [ui/README.md](ui/README.md)).
From `ui/`:

```sh
pnpm install
pnpm tauri dev                            # dev app with hot reload
pnpm tauri build --debug --no-bundle      # debug binary, no installers
pnpm tauri build                          # release bundles for the current OS
```

## CI

- `.github/workflows/ci.yml` (push to main, pull requests, weekly): fmt + clippy on Linux;
  `cargo test --workspace` on Linux, macOS and Windows; UI lint, typecheck, test and build;
  the conformance harness against the Docker fixtures; `cargo deny` (advisories, bans,
  licenses, sources).
- `.github/workflows/bundle.yml` (push to main, manual): Tauri dev bundles for Linux x86_64,
  Windows x86_64 and macOS aarch64/x86_64, uploaded as workflow artifacts.
  `scripts/ci-verify-signing.mjs` checks what was produced: `codesign --verify --deep --strict`,
  `spctl -a -t exec` and `xcrun stapler validate` on the macOS `.app`, and
  `Get-AuthenticodeSignature` (must be `Valid`, timestamped) on the Windows `.msi`, NSIS `.exe`
  and app `.exe`. Linux has no OS code signing; its bundles are listed with SHA-256.
  Signing is **enforced by default** (M0 exit check): the job fails before the build if this
  OS's secrets are missing and after it if a bundle is unsigned, ad-hoc signed or unnotarized.
  Opt out with the repository variable `OXBASE_REQUIRE_SIGNING=false` (push) or by unticking
  `require_signing` (manual run). Secrets: `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`,
  `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`, `WINDOWS_CERTIFICATE`,
  `WINDOWS_CERTIFICATE_PASSWORD` (optional: `APPLE_SIGNING_IDENTITY`,
  `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`); see the header of the
  workflow file.

## License

Oxbase is licensed under the [Apache License, Version 2.0](LICENSE). See [NOTICE](NOTICE).

Contributions are welcome under the [Contributor License Agreement](CLA.md); see
[CONTRIBUTING.md](CONTRIBUTING.md). Oxbase is developed as open core: this repository is fully
open source, and any future paid features live in a separate repository that builds on it.
