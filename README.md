# Oxbase

Oxbase is a cross-platform desktop database client written in Rust. It has a Tauri 2 shell and a
React + TypeScript UI. v1 targets PostgreSQL, MySQL, MariaDB, SQLite, SQL Server, MongoDB and
Redis on Linux, macOS and Windows. The core is headless, so every feature also works from
`oxbase-cli` and from tests. See [PLAN.md](PLAN.md) for the architecture and milestones.

## What works (1.0)

Oxbase 1.0 is the core client: milestones M0 to M8 of [PLAN.md](PLAN.md). Jobs, ER modeling, BI,
AI and the hardening milestone (M9 to M13) are planned for later major versions.

On PostgreSQL, MySQL, MariaDB, SQLite and SQL Server (M1 connections: plain, TLS, SSH,
keychain secrets; SQL Server since M4):

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

- **SQL Server** (M4, SQL Server 2017 and later; CI runs 2025): the driver is `tiberius` (TDS
  7.4) with oxbase's own TLS handshake on rustls + ring (TDS 8.0 strict encryption is an
  option), in-band cancel that also works through an SSH tunnel, SQL authentication. The tree
  adds a **Types** folder (alias and table types) and indexes and triggers under views. The
  editor runs one `GO` batch per statement, so `#temp` tables, `SET` and `USE` persist across
  batches. The designer writes identity, computed (persisted or not), clustered indexes,
  included columns and filtered indexes, names default constraints and renames through
  `sp_rename`; DDL runs in one transaction. The grid writer returns exactly one row per edit
  even with triggers. Known gaps (`docs/m4-contracts.md` §11 and §13): no `PRINT` output (the
  messages tab says so), no row counts for plain batches, integers arrive as 64-bit and
  decimals as text, `GO <n>` runs once, no Windows / Entra authentication, named instances
  need a port. `money` is exact: tiberius is vendored with a patched money decoder
  (`vendor/tiberius/OXBASE.md`).

Contracts: [docs/m3-contracts.md](docs/m3-contracts.md),
[docs/m4-contracts.md](docs/m4-contracts.md) (SQL Server) and
[docs/m5-contracts.md](docs/m5-contracts.md) (import and export).

- **Import and export** (M5): CSV / TSV, JSON (array or NDJSON), XLSX and Parquet in both
  directions, SQL dumps out. **Import** (right-click a table or a database in the tree): pick a
  file, set the format options (delimiter, encoding with BOM detection, header, NULL texts,
  date and number formats, sheet), check the preview with inferred column types, map source
  columns to target columns (skip, rename, change type), choose the target mode (create,
  append, truncate, delete all, replace; destructive ones need a confirmation), the error
  policy (stop, or skip up to N bad rows) and the transaction policy, and watch progress
  (bytes, rows read, inserted, committed, skipped) with Cancel. Rows go through each engine's
  bulk path: PostgreSQL `COPY FROM STDIN`, MySQL / MariaDB / SQLite / SQL Server multi-row
  parameterized `INSERT`s sized to the engine's parameter limit (never `LOAD DATA LOCAL
  INFILE`). The report says what a cancel or failure left in the table. **Export** any table
  (with its filter and sort), query or result grid (all stored rows, spilled ones included) to
  CSV, JSON, XLSX (sheets split at 1 048 576 rows), Parquet (Snappy, gzip, Brotli, LZ4, zstd) or
  a SQL dump for any of the five engines. Files are written to a temporary file and renamed on
  success, so a failed or cancelled export leaves nothing behind. CSV and XLSX output guards
  formula-looking text against spreadsheet formula injection (on by default). Files are chosen
  in native dialogs shown by the Rust side; the UI only holds opaque file handles. Contracts
  and every deviation: [docs/m5-contracts.md](docs/m5-contracts.md).

- **Code insight** (M6) in the query editor, for all five SQL engines: **completion** of
  keywords, schemas, tables, views, CTEs, aliases, columns (with type, nullability and keys),
  functions with signatures, types, procedures and snippets, from the cursor's context (which
  clause, which tables are in scope, qualifiers like `o.` or `sales.`) and a per-connection
  **schema cache**. The cache reads the whole catalog in a fixed number of queries on its own
  session, is saved in the app database (instant after a restart), and refreshes by itself
  after a designer apply, an import that creates a table, or DDL run in the editor (also when
  it is committed later); the toolbar shows its state and has a refresh button.
  **Diagnostics** (400 ms after the last edit): unterminated strings, identifiers and comments
  and unbalanced parentheses are errors; unknown tables, schemas and columns, ambiguous columns,
  wrong argument counts of built-in functions and `UPDATE` / `DELETE` without `WHERE` are at
  most warnings, and only hints whenever the cache cannot be sure. **Hover** shows a table's
  columns, a column's type and keys, a function's signature. **Format** (Shift+Alt+F or the
  toolbar; selection or whole script) lays out clauses, lists, joins, subqueries and `CASE`
  arms and never changes meaning: its output must lex to the same tokens as the input apart
  from whitespace and keyword case, with comments kept, or the text is left alone.
  **Snippets**: 19 built-in ones and your own (toolbar **Snippets**). Parsing is a tolerant,
  hand-written parser over the same lexer as the statement splitter (tree-sitter was evaluated
  and not used, see [docs/m6-contracts.md](docs/m6-contracts.md) §2.1). Numbers: see *Code
  insight (M6)* below.

- **MongoDB** (M7, MongoDB 5.0 and later; CI runs 7.0.45): the official `mongodb` crate
  behind an in-process SOCKS5 dialer, so SSH tunnels and every TLS mode (`VerifyCa`, platform
  roots, inline PEM, keychain client keys, the server name checked separately from the dial
  address) work as on the SQL engines; SCRAM-SHA-256 / SHA-1, X.509 and PLAIN; host lists,
  replica sets and `mongodb+srv` (not through SSH). The tree shows databases, then
  **Collections** (with their indexes), Views, Users and Roles. Double-click a collection to
  open the **document grid**: columns are inferred from a sample (one `Int64` column for int +
  long, JSON text for nested and mixed values, Decimal128 as exact text), and the **tree** and
  **JSON** views show the whole selected document. The **find / aggregate builder** writes the
  filter, projection, sort (key order kept), skip and limit, or pipeline stages; history keeps
  each request as shell code. Documents are edited, inserted and deleted as Extended JSON text
  with an optimistic check: a document changed on the server since it was read is a conflict
  and nothing is written. **New query** on a MongoDB connection opens the **mongo shell**, a
  sandboxed subset on QuickJS (`db.<coll>.find / aggregate / insertOne / updateMany /
  countDocuments / ...`, `use`, `show dbs / collections`, `ObjectId`, `ISODate`,
  `NumberLong`, ...; no file, network or process access; memory, stack and CPU limits). Cursors
  stream into the same result grid as SQL results; Stop kills the server operation
  (`killOp`) within 2 s. `dropDatabase()`, `drop()`, `dropIndex` and `deleteMany` /
  `updateMany` with an empty filter ask for confirmation; `pwd` / `password` values are masked
  in history. Collections can be imported into (`insertMany`) and exported like any table.
- **Redis** (M7; CI runs 8.10.3): the `redis` crate (RESP3 on 6.0 and later, else RESP2),
  standalone (also through SSH), **cluster** and **sentinel**, TLS through rustls with the same
  dial / server-name split. The tree lists databases 0..N-1, then namespaces (prefixes split at
  `:` by default) and keys with their type and TTL, plus ACL users. The **key browser** scans
  with a pattern and type filter (cursor `SCAN` only, never `KEYS`; a cluster's masters are
  scanned in turn) and opens a key in its **typed editor**: string (text or hex), hash, list,
  set, sorted set, stream and RedisJSON values stream in chunks of 1000, so a 200 000-member
  key opens in bounded memory. Edits (set a string; add, change or remove hash fields, list
  items, set and sorted-set members and stream entries; TTL, rename, delete) run atomically
  under `WATCH` / `MULTI`; a key changed since it was read is a conflict, and delete and an
  overwriting rename need a confirmation. The **console** runs one command per line with `redis-cli` quoting and shows
  replies per RESP type (tables for hashes, sorted sets with scores, ...); `FLUSHALL`,
  `FLUSHDB`, `CONFIG SET`, `KEYS` and the other dangerous commands ask for confirmation,
  `SUBSCRIBE` / `MONITOR` are refused (use the pub/sub panel), and `AUTH` / `ACL SETUSER` /
  `CONFIG SET requirepass` secrets are masked in history. The **pub/sub** panel subscribes to
  channels and patterns on its own connection, shows messages as they arrive (a bounded buffer
  that counts dropped messages) and publishes. Stop cancels a blocking command with
  `CLIENT UNBLOCK`.

Contracts and every deviation: [docs/m7-contracts.md](docs/m7-contracts.md). Numbers: see
*MongoDB and Redis (M7)* below.

Data transfer, structure sync, data sync and backup (M8; PostgreSQL, MySQL, MariaDB, SQLite and
SQL Server; nothing writes to a target before an explicit apply, and every script can be
previewed first):

- **Data transfer** (toolbar **Transfer...**): tables, views or a query from one connection into
  a database or schema of any of the five SQL engines. A per-engine type mapper maps each
  column and says how faithful the mapping is (exact, widened, narrowed or unsupported, with the
  reason as a tooltip); types, names and skips can be overridden per column. The review step
  shows the target DDL, the load order (parents before children, cycles reported) and the
  warnings. Rows stream as Arrow pages through the target's bulk loader (one batch in memory
  per side), per-table or per-batch transactions, stop or continue on bad rows (reported),
  foreign keys added after the load, progress and cancel. Same-engine copies keep native types.
- **Structure sync** (toolbar **Compare**): compares two schemas or databases of the same engine
  (tables with their columns, keys, indexes, checks and foreign keys, views, materialized
  views, sequences, functions, procedures and triggers), shows only-in-source, only-in-target
  and different items with a DDL diff, and scripts the selected items in dependency order.
  Drops are flagged and need a confirmation; the script runs in one transaction on PostgreSQL,
  SQL Server and SQLite (MySQL and MariaDB DDL commits per statement, and the preview says
  so), and a re-compare follows the apply.
- **Data sync**: compares tables by key in keyset chunks (an optional per-engine digest skips
  equal chunks), refuses tables without a primary or unique key unless key columns are chosen,
  spills differing rows to disk, shows counts and sample rows per insert / update / delete, and
  scripts DML with bound values (deletes off by default and confirmed separately); the apply
  runs in batched transactions and re-compares.
- **Backup / restore** (toolbar **Backup**): one `.oxbackup` ZIP per schema or database
  (manifest, object definitions and DDL, table rows as zstd-compressed Arrow IPC, a SHA-256 per
  entry, no connection details), verify and list, and restore into an existing schema or
  database of the same engine (structure, data or both; drop existing and truncate need a
  confirmation).

Contracts and every deviation: [docs/m8-contracts.md](docs/m8-contracts.md). Numbers: see
*Transfer, sync and backup (M8)* below.

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
  oxbase-store                                    app DB (history, snippets, schema-cache
                                                  snapshots), documents, keychain secrets
  oxbase-services                                 orchestration shared by both front ends
  oxbase-cli                                      headless runner (binary: `oxbase`)
  oxbase-conformance                              cross-engine conformance harness
  app-tauri                                       Tauri 2 shell (no logic)
ui/                                               React + TS + Vite front end
tests/fixtures.toml                               test DB connection params, M2-M4, M7 and M8 seed
                                                  names, the M5 import / export expectations and
                                                  the M6 insight perf scenarios (single source of
                                                  truth)
tests/data/m5/                                    small hand-written import samples (M5)
docs/                                             frozen contracts per milestone (m1-contracts.md
                                                  to m8-contracts.md)
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

These are the checks of `.github/workflows/ci.yml`, plus a few manual ones. For the M4
integration (2026-11-01) the CI ones passed in GitHub Actions on Linux, macOS and Windows; for
the M2 integration (2026-10-31) they passed in GitHub Actions on Linux x86_64 (fmt, clippy,
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
cargo run -p oxbase-cli -- --version          # prints "oxbase 1.0.0"
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
  CA, a server certificate for `localhost`, `127.0.0.1`, `postgres`, `mysql`, `mariadb`,
  `mssql`, `mongodb` and `redis`, and a client certificate (CN `oxbase_tls`) to `target/fixtures/tls`. PostgreSQL,
  MySQL, MariaDB and SQL Server run with TLS on. Each has a user `oxbase_tls` that the server
  accepts **only** over TLS (`[engines.<name>.tls_user]`; on SQL Server a logon trigger enforces
  it); the normal user (`oxbase`, `sa` on SQL Server) works with and without TLS.
- **SSH bastion** (`[ssh]`): the `ssh` service, an OpenSSH server on `127.0.0.1:15422` with user
  `oxbase` (password, plain key `id_ed25519` and passphrase key `id_ed25519_enc`, all in
  `target/fixtures/ssh`). Its host key is fixed, so `known_hosts` (Strict) and
  `host_key.fingerprint` (pinning) work. Tunnels reach the engines by their compose service
  names (`[ssh.targets]`: `postgres:5432`, `mysql:3306`, `mariadb:3306`, `mssql:1433`), so `VerifyFull`
  through a tunnel checks the certificate against the service name, not `127.0.0.1`.
- `fixtures-up.sh` recreates the services that mount this material when it was regenerated.

M2 seed objects: `[m2]` in `tests/fixtures.toml` names the objects every engine's init script
creates (`m2_parent`, `m2_child`, `m2_view`, `m2_fn`, `m2_proc`, `m2_trg`, `m2_seq`, PostgreSQL
`m2_mview` in schema `m2` and a second database `oxbase_m2_other`), so the fetcher and tree tests
agree on names. SQLite has no container: its tests apply `tests/docker/init/sqlite/01-m2.sql`
to a temporary file.

M3 and M4 seeds: `[m3]` names the round-trip scratch tables and the editable seed tables;
`[m2.mssql]`, `[m3.mssql]` and `[m4]` (M4) name the SQL Server seeds (schemas `m2`, `m3`, `m4` of
`oxbase_test`, applied by the container's entrypoint wrapper), the TLS-only login `oxbase_tls`
(enforced by a logon trigger) and the SSH target `mssql:1433`; see `docs/m4-contracts.md` §5.

M7 (contracts in `docs/m7-contracts.md` §9): `[m7.mongodb]` and `[m7.redis]` name the
MongoDB and Redis seeds (a document collection covering every mapped BSON type, a
200 000-document collection, a view, indexes, a user and role; a key of every Redis type, two
200 000-element keys, a namespace tree, a key in database 1, an ACL user), `[ssh.targets]` gains
`mongodb:27017`, `redis:6379` and `redis:6380` (TLS), MongoDB runs with optional TLS
(`allowTLS`) and Redis with a TLS-only port on `127.0.0.1:15480`, and `[m7.redis_cluster]` /
`[m7.redis_sentinel]` describe a 3-master cluster on 15501-15503 and a sentinel setup on 15511,
15512 and 15526 (host networking). `scripts/fixtures-up.sh` starts them with the others.

M8 (contracts in `docs/m8-contracts.md` §9): `[m8]` names a source and a target schema
(PostgreSQL and SQL Server: `m8_src` / `m8_tgt` of `oxbase_test`) or database (MySQL and
MariaDB: `m8_src` / `m8_tgt`; SQLite: two files) with designed structure differences (added,
dropped and changed columns, a type, nullability and default change, an index change, a new
foreign key, a changed view, a sequence, a rename-looking pair, case-sensitive and reserved
names) and data differences (inserted, updated and deleted rows with NULLs, unicode, big
decimals, binary and zoned timestamps, a 100 000-row table), restore targets, and the
PostgreSQL type set of the PG → MySQL transfer with the expected MySQL column types. No new
container or port.

M5 adds no seed objects: import and export tests create and drop scratch tables
`m5_t_<case>_<token>` (`[m5]`). `[m5.files."<name>"]` lists what each sample in `tests/data/m5`
must read as (rows, columns, types, values, issues, encodings); XLSX and Parquet samples are
built at runtime. `[m5.perf]` and `[m5.perf.checksum]` define the 1 GB import exit check, whose
CSV is generated on the CI runner and never committed (`docs/m5-contracts.md` §8-9).

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
   enforcement, both probed) matches what the generator does. Other kinds stay pending. From
   M7, `fetch_list` also lists the `[m7]` seeds of MongoDB (database, collection, index, view,
   user, role) and Redis (database, namespace, key, user). The harness also proves each
   fixture answers its handshake. With `OXBASE_CONFORMANCE_REQUIRE_M4=1` (CI, the M4 exit
   check) every M1-M3 cell of PostgreSQL, MySQL, MariaDB, SQLite and SQL Server must pass,
   nothing pending or skipped; `OXBASE_CONFORMANCE_REQUIRE_M7=1` (CI since M7) extends that
   guard to all seven engines.
2. **Connection checks** (M1), through the real drivers, `ConnectionManager` and russh provider:
   `connect` (plain, server version equals `version_prefix`), `tls` (`VerifyFull` with the
   fixture CA as `oxbase_tls`, `tls_in_use`; plaintext and a wrong server name refused), `ssh`
   (bastion, password auth, Strict known_hosts), `tls_over_ssh` (encrypted-key auth plus
   `VerifyFull` against the service name; a wrong name refused), `cancel` (`pg_sleep(30)` /
   `SLEEP(30)` / an endless recursive CTE / `WAITFOR DELAY` ends with `Cancelled` and the
   session answers `SELECT 1` within 2 s, via the token and via `cancel_running`) and
   `stream_200k` (200 000 generated rows arrive in more than one batch). SQLite has no TLS/SSH
   columns. MongoDB cancels a `$where` sleep (`killOp`, the server operation must be gone) and
   streams `m7_big`; Redis cancels `BLPOP 0` (`CLIENT UNBLOCK`) and streams `m7:big:list`;
   MongoDB's `tls` check runs as `oxbase` (no TLS-only user exists on MongoDB) and Redis's on
   the TLS-only port.

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
cargo test -p oxbase-net -p oxbase-pg -p oxbase-mysql -p oxbase-sqlite -p oxbase-mssql \
    -p oxbase-mongo -p oxbase-redis -p oxbase-services --locked -- --include-ignored --test-threads 1
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

Last verified 2026-11-03 in the GitHub Actions conformance job (Linux x86_64, M7 integration,
fast-lane run 41547092752 on `wip/m7-integrate` at f90055a; the full-lane run 41549606517 on
`milestone/m7` ran the same commit) with all 7 fixtures up plus the Redis cluster and sentinel:
every fixture reachable (PostgreSQL 18.6, MySQL 8.4.13, MariaDB 11.8.9, SQL Server 2025
17.0.5035, MongoDB 7.0.45, Redis 8.10.3, SQLite 3.54.1 file). CI sets
`OXBASE_CONFORMANCE_REQUIRE_M7=1`: the 7-engine guard reported nothing open.

- Object-kind matrix, 420 cells: **115 pass, 0 fail**, 108 pending, 0 skipped, 197 n/a. The 65
  `fetch_list` passes are every M2 cell: PostgreSQL 13 (database, schema, table, column,
  index, foreign key, check, view, materialized view, function, procedure, trigger,
  sequence), MySQL 10 and MariaDB 11 (the same without schema and materialized view; sequence
  on MariaDB only), SQLite 8 (database, table, column, index, foreign key, check, view,
  trigger) and SQL Server 13 (database, schema, table, column, index, foreign key, check,
  view, function, procedure, trigger, sequence, type; indexes and triggers also under an
  indexed view), plus M7: MongoDB 6 (database, collection, index, view, user, role) and Redis 4
  (database, namespace, key, user). The 50 M3 passes are `roundtrip` and `capabilities` for
  table, column, index, foreign key and check on each of the five SQL engines (CI also sets
  `OXBASE_CONFORMANCE_REQUIRE_M3_ROUNDTRIP=1`). The only skipped operations are the ones the
  contract allows: table and column comments on SQLite. The pending cells are user, role,
  grant, event, extension and the `roundtrip` / `capabilities` checks of the non-designer
  kinds on the SQL engines (not in M2-M7); MongoDB and Redis have no designer, so their
  `roundtrip` / `capabilities` checks are n/a.

  | Engine | table | column | index | foreign key | check |
  |---|---|---|---|---|---|
  | PostgreSQL | pass / pass | pass / pass | pass / pass | pass / pass | pass / pass |
  | MySQL | pass / pass | pass / pass | pass / pass | pass / pass | pass / pass |
  | MariaDB | pass / pass | pass / pass | pass / pass | pass / pass | pass / pass |
  | SQLite | pass / pass | pass / pass | pass / pass | pass / pass | pass / pass |
  | SQL Server | pass / pass | pass / pass | pass / pass | pass / pass | pass / pass |

  (`roundtrip` / `capabilities`.) SQLite does NOT NULL, default, foreign key and check changes
  as the table rebuild; MySQL and MariaDB DDL is not transactional, SQL Server's is (probed).
- Connection checks, 42: **39 pass**, 3 n/a (SQLite TLS/SSH), 0 pending, 0 fail.

  | Engine | connect | tls | ssh | tls_over_ssh | cancel | stream_200k |
  |---|---|---|---|---|---|---|
  | PostgreSQL | pass | pass | pass | pass | pass | pass |
  | MySQL | pass | pass | pass | pass | pass | pass |
  | MariaDB | pass | pass | pass | pass | pass | pass |
  | SQLite | pass | n/a | n/a | n/a | pass | pass |
  | SQL Server | pass | pass | pass | pass | pass | pass |
  | MongoDB | pass | pass | pass | pass | pass | pass |
  | Redis | pass | pass | pass | pass | pass | pass |

  SQL Server details (M4 fast-lane run 41298123652): TLS as the TLS-only login `oxbase_tls`
  (plaintext and a wrong name refused), SSH to `mssql:1433`, `VerifyFull` through the tunnel,
  cancel of `WAITFOR DELAY '00:00:30'` (token: ended in 9 ms, session answered in 12 ms;
  `cancel_running`: 1 ms / 2 ms), 200 000 rows in 49 batches (1506 ms). MongoDB and Redis
  details: *MongoDB and Redis (M7)* below.
- Query runs (`oxbase-services` live tests, 8 MiB memory budget so the result spills; M4
  full-lane run 41298470889. In the fast-lane run 41298123652 the 1M-row stream took 1376 / 1335 / 1315 /
  692 / 3520 ms in the same engine order and every other figure stayed in the same range):

  | Engine | 1M rows streamed | first page | random 200-row page p95 | spill | peak batch memory | cancel after ~220-250k rows: run ended | next `SELECT 1` |
  |---|---|---|---|---|---|---|---|
  | PostgreSQL | 1791 ms | 10 ms | 0.16 ms | 28 MiB | 7.6 MiB | 188 ms after cancel | 534 ms, same session |
  | MySQL | 2021 ms | 72 ms | 0.20 ms | 28 MiB | 7.9 MiB | 1001 ms after cancel | 3 ms, new session |
  | MariaDB | 1827 ms | 42 ms | 0.21 ms | 28 MiB | 7.2 MiB | 1001 ms after cancel | 5 ms, new session |
  | SQLite | 880 ms | 3 ms | 0.21 ms | 21 MiB | 7.9 MiB | 0 ms after cancel | 1 ms, same session |
  | SQL Server | 4408 ms | 165 ms | 0.20 ms | 28 MiB | 7.9 MiB | 1001 ms after cancel | 83 ms, new session |

  After the cancel, the editor's next `SELECT 1` must finish within 2 s. On MySQL, MariaDB and
  SQL Server the driver was still stopping the cancelled stream after 1 s, so that `SELECT 1`
  ran on a new session. On SQL Server the T-SQL multi-statement script runs one statement per
  `GO` batch (a `#temp` table across batches, a batch of two `SELECT`s giving two result sets)
  and reports no row counts for plain batches (a known tiberius gap).

### Import and export (M5)

Last verified 2026-11-01 in the M5 integration runs: the numbers below are from the full-lane
CI run 41330355496 on `milestone/m5` at a424690 (green; its Conformance job passed on the second
attempt after the SQL Server container stopped at startup on the first, see
`scripts/ci-fixtures.sh`) and Bundle run 41330357469 on the same commit (green).

The exit check, job **Import perf (1 GB CSV)** (`crates/oxbase-services/tests/import_perf.rs`,
`--profile perf`): a generated CSV of 10 000 000 rows and 9 columns (integers, text with
quotes, commas, LF / CRLF, Unicode and formula-looking values, decimals, floats, booleans,
dates, microsecond timestamps, NULLs), 1 238 093 427 bytes, imported into a new table with a
commit every 100 000 rows:

| Engine | Method | Time | Rows/s | MB/s | Progress events (max gap) | Peak RSS growth | Checksum | Cancel at ~25% |
|---|---|---|---|---|---|---|---|---|
| PostgreSQL 18.6 | `COPY` (text) | 66.7 s | 149 992 | 18.6 | 224 (501 ms / 1.07% of the file) | 24 MiB | equal | `Cancelled` 0 ms after the call; 2 600 000 committed rows in the table, as reported |
| MySQL 8.4.13 | multi-row `INSERT` | 106.0 s | 94 316 | 11.7 | 517 (286 ms / 0.58%) | 94 MiB | equal | `Cancelled` after 169 ms; 2 500 000 committed rows in the table, as reported |

Limits asserted: progress at most 1.5 s or 1% of the file apart (never over 10 s), RSS growth
under 512 MiB, cancel within 2 s, and a 20 000-row probe import read back column by column
before the big run. The checksum (count, sums of `id`, `qty`, amount in cents, count and length
of the notes) matched on both engines.

Bulk loaders (`bulk_live.rs`, 100 000 rows of every canonical type, read back equal; debug
build): PostgreSQL `COPY` 0.85 s, MySQL 3.29 s, MariaDB 1.57 s, SQLite 1.60 s, SQL Server
20.44 s (parameterized multi-row inserts with typed parameters; no TDS bulk load yet, see
`docs/m5-contracts.md` §13; an import plan into SQL Server says to expect about 4 000 to 7 000
rows per second). Each loader also passes cancel mid-call (rolled back, nothing left)
and duplicate-key failure.

Round trips (`oxbase-services` import and export live tests, `OXBASE_SERVICES_REQUIRE_M5_IO=1`):
on each of PostgreSQL, MySQL, MariaDB, SQLite and SQL Server a 1 000-row table was exported to
CSV, JSON, XLSX and Parquet, imported again into a new table and compared equal (XLSX to the
millisecond); the PostgreSQL and SQLite SQL dumps ran against the database and gave the same
rows, and `oxbase-io`'s live test ran each engine's own dump on PostgreSQL, MySQL, MariaDB and
SQL Server. Also on every engine: every target mode with one transaction and with batches,
failure and cancel leaving exactly what §5.2 of the contract says, text fidelity (NULL vs empty,
astral characters, CR / LF / tab, quotes, backslashes), the skip policy, plan rejections, every
committed sample file, export of a filtered and sorted table, a query and a 300 000-row result
spilled to disk, export cancel leaving no file, and no overwrite without permission.

The webview smoke test (job **Webview smoke (Linux)**, with `OXBASE_E2E_REQUIRE_M5=1`) imports
`tests/data/m5/basic.csv` through the app's IPC into PostgreSQL (`copy_text`, 5 rows), exports
the query result to CSV and compares every value, using the debug-only `--e2e-file-dir` file
hook (`ui/e2e/README.md`).

The dialect, driver, tunnel and connection-manager live tests and the keychain tests passed in
the same CI run, as did the M3 and M4 live tests: the DDL / DML cells of each engine
(`oxbase-pg` `m3_live` 10, `oxbase-mysql` `m3_live` 19 for MySQL and MariaDB, `oxbase-sqlite`
`m3` 16, `oxbase-mssql` `m4_live` 5: round trips re-fetched through the `Table` fetcher,
ALTER COLUMN with its blockers dropped and recreated, unnamed constraint lookups, grid writes
counting exactly one row per edit with the audit trigger on, datetime keys), the SQL Server
driver (`driver_live` 11: TLS modes, TDS 8.0 strict, the TLS-only login, every type of
`m4.m4_types`, parameters, multiple result sets, `#temp` and `USE` across batches, cancel,
transactions, the marker-password leak check) and fetchers (`fetch_live` 6), and the
`oxbase-services` edit and designer tests on all five engines (open `m3_edit_pk` filtered and
sorted; insert / update / delete with read-back; a conflict when another session deleted the
row; `m3_edit_uq` editable by `code`; `m3_edit_nokey` and `m3_edit_view` read-only; designer
create / alter / reload, and a conflict when the table changed in between). CI runs them with
`OXBASE_SERVICES_REQUIRE_M3_CELLS=1`, so a missing engine cell or seed table fails instead of
reporting "pending".

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
`OXBASE_CONFORMANCE_REQUIRE_M2_FETCH=1`, `OXBASE_CONFORMANCE_REQUIRE_M3_ROUNDTRIP=1` (a
pending M2 / M3 cell fails) and `OXBASE_CONFORMANCE_REQUIRE_M4=1` (the 5-engine guard: every
M1-M3 cell of the five SQL engines must pass and nothing may be skipped); CI sets all three.

### Code insight (M6)

Last verified 2026-11-02 in the M6 integration runs: full-lane CI run 41364795338 on
`milestone/m6` at eb62f80 (green; its Conformance job passed on the second attempt after a
Docker Hub rate limit on the first) and Bundle run 41364798320 on the same commit (green).

The exit check, job **Insight perf (5k-table completion)** (`OXBASE_REQUIRE_M6_PERF=1`):
`context_at` + `complete` from text and cursor to the ranked list, 15 scenarios
(`[[m6.perf.scenarios]]` in `tests/fixtures.toml`: statement start, `FROM` empty / prefix /
fuzzy / schema-qualified, select list, qualified columns, `JOIN` target and `ON`, `WHERE`,
`INSERT` columns, `UPDATE SET`, CTE columns, function prefix, a cursor after a 500-statement
script) over 16 pairs of related tables, 240 cursor positions, 3 000 timed runs:

| Where | Catalog | p50 | p95 | max | Budget |
|---|---|---|---|---|---|
| `complete_perf.rs`, `--profile perf` | synthetic, 5 000 tables x 20 columns, 500 routines (built in 51 ms, ~20 MiB) | 0.18 ms | 1.49 ms | 1.97 ms | p95 50 ms per scenario |
| `complete_perf.rs`, `--release` | same | 0.18 ms | 1.51 ms | 1.82 ms | same |
| `schema_cache_perf.rs`, `InsightService::complete` (gate, blocking pool, UTF-16) | live PostgreSQL, 5 018 relations, 100 056 columns | 0.23 ms | 1.02 ms | 1.47 ms | same |
| Webview smoke, 50 `insight_complete` calls through Tauri IPC | e2e PostgreSQL database | 1 ms | 2 ms | 2 ms | p95 150 ms |

A cancelled completion returned 0.6 ms after the cancel (limit 5 ms). The live 5k-table
PostgreSQL catalog (`m6_perf`, 5 000 tables x 20 columns, each with a foreign key) was read
through the real catalog reader in 524 ms (limit 15 s) into an 18.2 MiB index (budget 64 MiB;
6.4 MiB snapshot) and reloaded from the app database in 43 ms. In the app (webview smoke)
`customers` was listed 70 ms after typing `SELECT * FROM cust`, WebDriver polling included.

Schema-cache live tests (Conformance job, all five engines): the catalog reader reads every
seeded object (columns, keys, foreign keys, routines, sequences, types, case rules, partial
scopes, cancel); an editor `CREATE` / `DROP`, a designer apply and an import create reached the
cache in 311 to 444 ms (limit 2 s; the 300 ms refresh coalescing included), and a `CREATE`
inside a transaction committed by a later run 0 to 338 ms after the `COMMIT`.

Diagnostics: the valid-SQL corpus (`crates/oxbase-insight/tests/corpus/`: 205 PostgreSQL, 191
MySQL (also run as MariaDB), 169 SQLite and 167 SQL Server statements) gives zero errors and zero
warnings without a cache and with a matching one (0 to 4 hints per dialect); a 1 MiB script is
diagnosed in 64 to 99 ms depending on the CI runner (best of five, perf profile; budget 100 ms,
the job fails above 150 ms). The formatter's properties (`check_format`
token equivalence and idempotence) held on 23 232 inputs (the corpus with all 96 option
combinations) and 31 578 more (every prefix, every single-token deletion and random
insertions), 0 violations; 100 KiB formats in 4 to 25 ms per engine (budget 200 ms). The
diagnostics and formatter numbers come from the `insight-perf` jobs of fast-lane runs
41367296758 (34de45a) and 41371573273 (30457db), the commits that started printing them.

Tests: 1 205 Rust tests passed on Linux and macOS and 1 203 on Windows (235 live / perf tests
ignored there and run in the Conformance and perf jobs), 1 021 in the Conformance job, 284 UI
tests.

### MongoDB and Redis (M7)

The exit check (PLAN.md §6, "browse, edit and query on both engines") runs live in the CI
Conformance job against MongoDB 7.0.45 and Redis 8.10.3 (plus a 3-master Redis cluster and a
sentinel setup), with `OXBASE_CONFORMANCE_REQUIRE_M7`, `OXBASE_SERVICES_REQUIRE_M7`,
`OXBASE_REDIS_REQUIRE_M7` and `OXBASE_SHELL_REQUIRE_M7_SEED` set, so a missing fixture, seed or
cell fails instead of being pending. Numbers from fast-lane run 41547092752 (f90055a):

- **Connections** (conformance): MongoDB plain 17 ms, `VerifyFull` TLS on the shared port (a
  wrong name refused), SSH to `mongodb:27017`, `VerifyFull` through the tunnel 477 ms; cancel of
  a running operation: the token ended the run in 0 ms and the server operation was gone 1 ms
  later, `cancel_running` 209 ms (the `$currentOp` search retries once after 200 ms); 200 000
  documents in 200 batches in 897 ms. Redis plain 2 ms, TLS on the TLS-only port (plaintext to
  it and a wrong name refused), SSH to `redis:6379`, `VerifyFull` to `redis:6380` through the
  tunnel 352 ms; `BLPOP 0` cancelled in 0 ms (token) / 1 ms (`cancel_running`), the session
  answered 1-2 ms later; 200 000 list elements in 49 batches in 283 ms.
- **Browse**: the tree lists `oxbase_test` → Collections (`m7_docs`, `m7_big`, `m7_edit`) with
  `m7_docs`'s five indexes, Views (`m7_view`), Users and Roles; Redis databases 0..15 (key
  counts; `m7:db1:key` read on database 1's own session), the namespace tree under `m7:` with
  key types and TTLs, ACL users without passwords.
- **Query**: a `find` on `m7_docs` gives the contract's column table (one `Int64` column for
  int + long, a `mixed` JSON column, `only_late` only in the `$document` column with a sample of
  10); a `$group` aggregate; `m7_big` (200 000 documents) streams every row in about 0.66 s
  (driver test) and pages at random offsets, and a cancel mid-stream ends within 2 s. The
  shell: `db.m7_docs.countDocuments({})` = 12, `db.m7_big.find({g: 7}).sort({_id: 1}).limit(5)`
  → 7, 107, 207, 307, 407, a `$group` total of 9 900 000, `use` switching the database; a
  `while (true) {}` stops on cancel and at the CPU limit, an allocation loop at the memory
  limit. Redis: a `SCAN` of `m7:*` returns every seed key once; each type opens in its typed
  columns; `m7:big:list` and `m7:big:zset` (200 000 each) stream in 98 ms and 176 ms; a value
  read is cancelled within 2 s; the console renders `HGETALL`, `ZRANGE ... WITHSCORES` and
  binary `GET` replies, and `FLUSHDB` is refused without a confirmation; the cluster scan finds
  the 100 seed keys across the three masters; sentinel mode reads through `oxmaster`.
- **Edit**: document insert, update (including a field that was missing), replace and delete
  on `m7_edit`; a stale `previous` (also a case-only or type-only change) is a conflict and
  writes nothing. Every key edit on scratch keys applies atomically under `WATCH` / `MULTI`; a
  type conflict and a `delete` without confirmation are refused. Pub/sub: subscriptions to
  `m7:news` and `m7:*` both receive a published message, and a full buffer reports `dropped`.
  A collection exports to CSV and imports into a new collection (`insertMany`).
- **Live tests**: `oxbase-mongo` driver 7, fetch 6, documents 7, shell 4; `oxbase-redis`
  driver 14, fetch 5, keys 7, pub/sub 3, cluster 3, console 3; `oxbase-services` m7_mongo 5,
  m7_redis 1, m7_shell 2 (history redaction of a `createUser` `pwd` and an `AUTH` password
  checked there; the other redaction rules are unit-tested); all passed.
- **Webview smoke** (Linux, `OXBASE_E2E_REQUIRE_M7=1`): MongoDB `m7_docs` opened with 12
  documents and no `$document` column, a test document edited in the document dialog and read
  back, the shell's `countDocuments` showed 12 (8.7 s for the step); Redis `m7:ns:*` found 4
  keys, `m7:hash` opened with 3 rows, a scratch string key edited and read back, the console's
  `HGETALL m7:hash` rendered (5.9 s). Screenshots `ui-04-mongo-collection-{dark,light}` and
  `ui-05-redis-key-browser-{dark,light}`.

Tests at the M7 integration (full-lane run 41549606517 on `milestone/m7` at f90055a, Bundle run
41549609930 green on all four targets): 1 394 Rust tests passed on Linux and macOS and 1 392 on
Windows (302 live / perf tests ignored there and run in the Conformance and perf jobs; the
QuickJS shell builds and runs on all three), 1 260 in the Conformance job, 377 UI tests.

### Transfer, sync and backup (M8)

The exit check (PLAN.md §6, "sync between PG↔PG, MySQL↔MySQL and MSSQL↔MSSQL produces correct
scripts; transfer PG→MySQL works") runs live in the CI Conformance job on scratch copies of the
`[m8]` seeds, with `OXBASE_CONFORMANCE_REQUIRE_M8` and `OXBASE_SERVICES_REQUIRE_M8` set, so a
missing fixture, seed or cell fails instead of being pending. Numbers from fast-lane run
41605886760 (df06e10):

- **M8 conformance**: 40 cells, **40 pass**, 0 fail, 0 pending. Per engine (PostgreSQL, MySQL,
  MariaDB, SQLite, SQL Server) the `seed`, `sync_structure`, `sync_data` and `backup` cells
  pass, and the transfer matrix (every SQL engine into every other one, 20 cells, `m8_data`
  read back and compared column by column) passes.
- **Structure sync**: the designed items are reported with their designed status (PostgreSQL
  12, MySQL 11, MariaDB 12, SQL Server 12, SQLite 10); an unconfirmed destructive apply is
  refused; the applied scripts (PostgreSQL 21 statements in one transaction, MySQL 13 and
  MariaDB 14 statements not atomic, SQL Server 19 in one transaction, SQLite 31 with table
  rebuilds in one transaction) run without errors and a re-compare and a fresh compare are in
  sync, with the existing rows kept. Golden scripts per engine are pinned in
  `crates/oxbase-diff/tests/golden/`.
- **Data sync**: `m8_data` 2 / 5 / 2 (inserts / updates / deletes), the 100 000-row `m8_big`
  1 / 1 / 1 with 8 of 10 chunks skipped by the digest on PostgreSQL, MySQL, MariaDB and SQL
  Server, `m8_composite` 0 / 1 / 0, `m8_nokey` refused; the 13-statement script applies in one
  batch (deletes refused without confirmation) and the re-compare is in sync on every engine.
- **Transfer PG → MySQL**: the `[m8.transfer]` set (4 tables, 24 columns) gets the expected
  MySQL column types and fidelities, values read back equal, `m8_parent` loads before
  `m8_child` and the foreign keys exist after the load (348 ms). Golden pair reports for all 25
  engine pairs are pinned in `crates/oxbase-transfer/tests/golden/`.
- **Backup / restore**: a backup of `m8_src` (PostgreSQL 16 objects, 643 KB) verifies (a
  truncated or damaged copy is rejected, no password or host in the archive) and restores into
  `m8_restore` with 100 036 rows; structure and data compares of source and restore are in sync
  on all five engines (SQL Server `datetimeoffset` values keep their offset and 100 ns digits).
- **Live tests** (services): `m8_structure_live` 15, `m8_data_live` 7, `m8_transfer_live` 8
  (including cancel mid-table within 2 s, `continue` mode and closing the source session),
  `m8_backup_live` 5; all passed.
- **Webview smoke** (Linux, `OXBASE_E2E_REQUIRE_M8=1`): data compare of `m8_src` → `m8_tgt`
  showed `m8_data` 2 / 5 / 2; structure compare showed 14 differences, the apply without drops
  ran and the re-compare left only the three target-only tables; a transfer of
  `m8_xfer.m8_types` into a new PostgreSQL schema mapped 24 columns and finished (3 rows); a
  backup of `m8_src` (16 objects, 643 KB, 530 ms) verified OK. Screenshots
  `ui-08-transfer-{wizard,mapping,review,run}-{dark,light}`,
  `ui-08-structure-sync{,-setup}-{dark,light}` and `ui-08-data-sync-{dark,light}`.

Tests at the M8 integration: 1 589 Rust tests passed in the Linux test job of run 41605886760
and on macOS in full-lane run 41608238873 (335 live / perf tests ignored there and run in the
Conformance and perf jobs), 1 468 in the Conformance job, 433 UI tests.
`calamine` is at 0.38.0: an advisory against 0.37.0 (crafted XLS files could panic or exhaust
memory) appeared during the M8 integration; the API oxbase-io uses did not change.

## App data

Profiles live in `connections.json` in the config directory (`OXBASE_CONFIG_DIR` overrides it).
From M2 the app database `oxbase.db` (query history and settings, SQLite with versioned
migrations; from M6 also user snippets and schema-cache snapshots, names and types only) lives in the local data directory: `~/.local/share/oxbase` on Linux,
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
webview, scrolling, page latency, cancel, a table-data cell edit applied and seen after a
reload, import / export (M5), completion (M6), MongoDB and Redis browse / edit / query (M7;
`scripts/e2e-fixtures.sh` starts PostgreSQL, MongoDB and Redis), and data compare, structure
compare and apply, transfer and backup with verify on the PostgreSQL `[m8]` seeds (M8). See [ui/e2e/README.md](ui/e2e/README.md). Locally (needs `webkit2gtk-driver`,
`cargo install tauri-driver --locked --version 2.1.0`, Docker, and `xvfb-run` without a
display):

```sh
(cd ui && pnpm install --frozen-lockfile && pnpm run build)
cargo build -p app-tauri --locked --features tauri/custom-protocol
scripts/e2e-fixtures.sh up
scripts/e2e-smoke.sh            # results in target/e2e (e2e-perf.json, failure screenshot)
scripts/e2e-fixtures.sh down
```

In CI (job **Webview smoke (Linux)**, M4 full-lane run 41298470889; it stays on PostgreSQL)
page requests through Tauri IPC plus Arrow decoding in the webview took p50 2 ms / p95 11 ms /
max 11 ms (the grid's own 7 pages, `window.__oxbasePerf`; 60 random 200-row probe pages: p50
1 ms, p95 2 ms, max 6 ms; limit p95 < 250 ms); the 1 000 000-row query finished in 1.42 s;
Stop ended a streaming query 65 ms after the click; the edit step committed and showed the new
value after a reload.

## CI

- `.github/workflows/ci.yml` has two lanes. **Fast lane**, on pushes to `wip/**` branches:
  Linux only (lint, tests, keychain, UI, Docker conformance, cargo-deny), about 10-15 minutes.
  **Full lane**, on pull requests, pushes to main, weekly and manual runs: fmt + clippy on Linux;
  `cargo test --workspace` on Linux, macOS and Windows; the OS keychain tests
  (`scripts/keychain-tests.sh`) on Linux, macOS and Windows; UI lint, typecheck, test and build;
  the conformance harness and the M1-M8 live tests against the Docker fixtures (MongoDB and
  Redis included, with the Redis cluster and sentinel); the webview
  smoke test (Linux; also on the fast lane of `wip/*-e2e`, `wip/*-app` and `wip/*-integrate`);
  `cargo deny` (advisories, bans, licenses, sources); the `import-perf` job (M5, the 1 GB CSV
  import exit check, `docs/m5-contracts.md` §9, with `OXBASE_REQUIRE_M5_PERF=1`): full lane,
  plus the fast lane of `wip/*-perf` and `wip/*-integrate`; the `insight-perf` job (M6,
  `docs/m6-contracts.md` §5, with `OXBASE_REQUIRE_M6_PERF=1`): completion latency on the
  synthetic 5k-table catalog in the perf and release profiles, the diagnostics (1 MiB) and
  formatter (100 KiB) budgets, then the live 5k-table PostgreSQL schema; full lane, plus the
  fast lane of `wip/*-perf`, `wip/*-complete`, `wip/*-cache` and `wip/*-integrate`. The webview
  smoke test requires the M6 completion step (`OXBASE_E2E_REQUIRE_M6=1`), the M7 MongoDB and
  Redis steps (`OXBASE_E2E_REQUIRE_M7=1`) and the M8 sync, transfer and backup steps
  (`OXBASE_E2E_REQUIRE_M8=1`).
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

### Releases

`.github/workflows/release.yml` publishes a GitHub release when a `v*` tag is pushed:

1. Set the same version in `Cargo.toml` (`[workspace.package] version`) and `ui/package.json`
   (for example `0.7.0`, or `0.7.0-beta.2` for a pre-release) and merge it to main.
2. Tag that commit: `git tag v0.7.0 && git push origin v0.7.0`.
3. The workflow checks the tag against both versions (`scripts/release-check.mjs`), builds the
   bundles with `bundle.yml`, and creates the release with the Windows (`.msi`, `-setup.exe`),
   macOS (`.dmg` for Apple Silicon and Intel) and Linux (`.deb`, `.rpm`, `.AppImage`) installers
   plus `SHA256SUMS.txt`. A tag with a pre-release part becomes a GitHub pre-release.

To test the pipeline without publishing anything, run the Release workflow manually with
`dry_run` ticked: it builds the current version and creates a draft release named
`v<version>-dry-run` (drafts are visible only to maintainers; delete it afterwards). Until the
signing secrets exist, the release notes say the installers are unsigned.

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
