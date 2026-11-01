# Implementation plan: Rust database client (Navicat-class)

Status: draft, 2026-10-31. Oxbase is a clean-room implementation: it reuses no code, assets or text from other database tools.

## Decisions so far

| Topic | Decision |
|---|---|
| App shell | **Tauri 2.** All logic is in Rust. The UI is TypeScript in a webview. |
| v1 databases | PostgreSQL, MySQL, MariaDB, SQLite, SQL Server, MongoDB, Redis |
| Platforms | Linux, macOS and Windows from day one |
| v1 scope | Core client, sync/transfer/backup, ER modeling, BI dashboards, AI assistant, scheduled jobs |
| Not in v1 | Oracle, team cloud sync and collaboration, PL/SQL debugger, HTTP tunnel |

---

## 1. Architecture

```
┌──────────────── Webview (React + TS) ────────────────┐
│ Monaco editor · Glide Data Grid · dockview layout     │
│ React Flow (ER / query builder / explain) · ECharts   │
└───────────────┬──────────────────────────────────────┘
                │ Tauri commands (control) + Channels (streams)
                │ result pages as Arrow IPC bytes, not JSON
┌───────────────▼──────────── Rust core (headless) ────┐
│ app-tauri   → thin command/event layer, no logic      │
│ services    → sessions, query runs, jobs, docs        │
│ features    → designer, diff/sync, transfer, model,   │
│               bi, ai, import/export                   │
│ dialects    → per-DB Fetcher / DdlGen / Comparator /  │
│               Capabilities / DmlWriter / BulkLoader   │
│ drivers     → pg · mysql · sqlite · mssql · mongo ·   │
│               redis, behind one async trait           │
│ infra       → ssh/tls tunnels, secrets, storage, log  │
└──────────────────────────────────────────────────────┘
        ▲
        └── `oxbase-cli`: the same core with no UI (runs jobs and scheduled tasks)
```

Rules:

1. **The core never depends on Tauri.** Every feature must work from `oxbase-cli` and from tests. `app-tauri` only translates calls.
2. **Arrow `RecordBatch` is the shared row format.** Drivers produce it. The grid, export, import, transfer, data sync and BI all consume it. Parquet, CSV, JSON and XLSX support then sits on one format.
3. **Each database is a matrix of object kind × role** (the main lesson from Navicat). Adding an object kind or a database means filling in table cells, not adding special cases.
4. **Everything is async** (tokio) and cancellable. Every long-running operation takes a `CancellationToken` and reports progress through a channel.

## 2. Cargo workspace

```
crates/
  oxbase-types        Value, DataType, ObjectKind, ObjectRef, ServerVersion, errors
  oxbase-driver       trait Driver/Connection/Session, streaming → RecordBatch, cancel
  oxbase-dialect      traits Fetcher, DdlGenerator, Comparator, Capabilities, DmlWriter, Quoting
  oxbase-pg           tokio-postgres (+ postgres-types)
  oxbase-mysql        mysql_async (MySQL + MariaDB, version-flagged)
  oxbase-sqlite       rusqlite on spawn_blocking (bundled SQLite)
  oxbase-mssql        tiberius (TDS 7.4, no TLS backend) + oxbase's own rustls/ring TLS
  oxbase-mongo        mongodb crate + rquickjs mongo-shell subset
  oxbase-redis        redis crate (cluster, sentinel, TLS, pub/sub)
  oxbase-net          SSH tunnels (russh), TLS config (rustls), tunnel pool
  oxbase-sql          lexer, statement splitter, dialect AST (sqlparser-rs), formatter
  oxbase-insight      completion/diagnostics engine (tree-sitter + schema cache)
  oxbase-schema-cache per-connection identifier cache, incremental refresh
  oxbase-designer     model edits → DDL preview/apply, per dialect
  oxbase-diff         structure compare + data compare/sync, script generation
  oxbase-transfer     source→target pipelines, type mapping across engines
  oxbase-io           import/export: CSV, JSON, XLSX (calamine / rust_xlsxwriter), Parquet, SQL dump
  oxbase-backup       logical backup/restore (own format) + SQL dump
  oxbase-model        ER model documents, reverse/forward engineering, model compare
  oxbase-bi           datasets, chart specs, DataFusion local query engine, dashboards
  oxbase-ai           provider trait: OpenAI-compatible, Anthropic, Gemini, Ollama; prompts
  oxbase-jobs         batch jobs, scheduler, run history, notifications (lettre)
  oxbase-store        app storage: SQLite app DB + versioned JSON documents; keyring secrets
  oxbase-services     orchestration used by both front ends
  oxbase-cli          headless runner
  app-tauri           Tauri 2 shell
ui/                   React + TS + Vite
tests/                conformance suite + docker-compose fixtures
```

## 3. Core contracts (sketch)

```rust
#[async_trait]
pub trait Connection: Send + Sync {
    fn engine(&self) -> Engine;
    fn server_version(&self) -> &ServerVersion;
    async fn execute(&self, sql: &str, params: &[Value], cancel: CancellationToken)
        -> Result<ResultStream>;            // yields Notice | Schema | RecordBatch | RowsAffected
    async fn cancel_running(&self) -> Result<()>;
    async fn begin(&self) -> Result<Tx>;
}

pub trait Fetcher {                          // one impl per (engine, ObjectKind)
    async fn list(&self, c: &dyn Connection, parent: &ObjectRef, cancel: CancellationToken)
        -> Result<Vec<ObjectSummary>>;
    async fn load(&self, c: &dyn Connection, obj: &ObjectRef, cancel: CancellationToken)
        -> Result<ObjectDef>;
}
pub trait DdlGenerator { fn create(&self, d: &ObjectDef) -> Script;
                         fn alter(&self, old: &ObjectDef, new: &ObjectDef) -> Script;
                         fn drop_object(&self, r: &ObjectRef) -> Script; }
pub trait Comparator   { fn diff(&self, a: &ObjectDef, b: &ObjectDef) -> Vec<PropertyDiff>; }
pub trait Capabilities { fn supports(&self, kind: ObjectKind, prop: PropId, v: &ServerVersion) -> bool; }
pub trait DmlWriter    { fn apply(&self, edits: &[RowEdit], key: &RowKey) -> Script; } // grid write-back
```

Progress (rule 4): a long-running operation takes a `ProgressSink` (a lossy wrapper around a tokio `mpsc::Sender<Progress>`, in `oxbase-driver`) next to its `CancellationToken`, and reports `Progress { stage, done, total, unit, message }` events (in `oxbase-types`) through it.

`ObjectDef` is one typed model per object kind (Table, Column, Index, ForeignKey, Check, View, MatView, Function, Procedure, Trigger, Sequence, Schema, User, Role, Grant, Event, Type, Extension). Database-specific properties go in a typed `extras` map, which is gated by `Capabilities`.

**Conformance suite:** one test matrix runs against every engine in Docker. It checks that a create → fetch → alter → diff → drop round trip is stable for each object kind. This keeps the per-database cells consistent.

## 4. Frontend

| Need | Choice | Why |
|---|---|---|
| SQL editor | Monaco | Folding, multi-cursor, and completion and diagnostics providers that call `oxbase-insight` |
| Result grid | Glide Data Grid | Canvas-rendered and virtualized, handles millions of rows. Pages are pulled on scroll. |
| Layout | dockview | Tabs, splits and floating panes, like Navicat |
| Diagrams | React Flow (@xyflow) | ER modeler, visual query builder, explain plan |
| Charts | ECharts | Covers Navicat's chart types: heat map, tree map, gauge, waterfall and more |
| State | Zustand + TanStack Query | |
| Data transport | `apache-arrow` JS | Decodes Arrow IPC pages sent over Tauri `ipc::Response` |

The server holds the results. A query run keeps a spilled result store (Arrow IPC on disk past N MB) and the grid fetches pages from it by row range. Large result sets never become JSON.

## 5. Security and storage

- Passwords, SSH keys and AI keys go in the OS keychain (`keyring` crate). Config files hold only a key ID.
- The app DB (SQLite) holds connections, groups, query history, job runs and the schema-cache snapshot.
- Documents (saved queries, models, dashboards, job definitions) are versioned JSON with `#[serde(flatten)] unknown: Map`, so newer fields survive older builds (Navicat's "forward compatible" pattern).
- AI: schema and data are sent only after the user turns it on for that connection, and the app shows exactly what is sent. Local providers (Ollama) are first-class.
- No telemetry by default.
- Edits in the grid need a primary or unique key. Otherwise the grid is read-only and the UI says why.

## 6. Milestones

Every milestone ships as a working app on all three platforms through CI. The "Done when" column is the exit check.

| # | Milestone | Done when |
|---|---|---|
| M0 | Workspace, Tauri shell, CI matrix (Linux/macOS/Windows), Docker fixtures for all 7 engines, conformance harness | Signed dev builds on all 3 OSes; harness runs empty against all engines |
| M1 | Driver trait + **pg, mysql/mariadb, sqlite**; SSH tunnel and TLS; connection manager with keychain | Connect through SSH and TLS to each engine; cancel works |
| M2 | Object tree (Fetchers for schema, table, view, index, function, trigger, sequence); query editor; splitter; streaming results grid; multiple result sets; history | Run a 1M-row query and scroll smoothly; cancel mid-stream |
| M3 | Grid editing (DmlWriter), filter and sort builder, form/JSON views; **table designer** with DDL preview for the 3 SQL engines | Round-trip conformance passes for table, column, index, FK and check |
| M4 | **mssql** driver + dialect cells; editor, grid and designer parity | Conformance green on 5 engines |
| M5 | Import/export: CSV, JSON, XLSX, Parquet, SQL dump; wizard with mapping and preview | 1 GB CSV import with progress; export of any result or table |
| M6 | Code insight: tree-sitter context, schema-aware completion, diagnostics, formatter, snippets | Completion under 50 ms on a 5k-table schema |
| M7 | **mongo** (document grid, tree/JSON view, find/aggregate builder, shell subset via rquickjs) and **redis** (key browser, typed editors, console, cluster/sentinel, pub/sub) | Browse, edit and query on both engines |
| M8 | Data transfer (cross-engine type mapping), **structure sync**, **data sync**, backup/restore | Sync between PG↔PG, MySQL↔MySQL and MSSQL↔MSSQL produces correct scripts; transfer PG→MySQL works |
| M9 | Jobs: batch jobs, scheduler (in-app + `oxbase-cli` for OS cron/Task Scheduler), email notifications, run logs | Scheduled export runs headless on all 3 OSes |
| M10 | **ER modeling**: logical/physical model, reverse/forward engineering, model compare, export PNG/PDF/SQL | Reverse-engineer a 200-table DB, edit it, then forward-engineer the change |
| M11 | **BI**: datasets (query/table/file) through DataFusion, ~15 chart types, dashboards, parameters, auto refresh | Build a dashboard from 2 connections with a filter |
| M12 | **AI**: chat with schema attach, explain/optimize/fix SQL, dialect conversion, natural language → SQL, "fill with AI" | Works with OpenAI, Anthropic, Gemini and Ollama |
| M13 | Hardening: crash reporting (opt-in), perf passes, accessibility, i18n scaffolding, installers, auto-update (Tauri updater), docs | Release candidate |

**M1 as built (notes, 2026-10-31).** The exit check passed on Linux against the Docker fixtures
(see README, *Conformance harness*). Where the build differs from the text above:

- Connection profiles are stored in one versioned JSON file in the platform config directory
  (`oxbase-store::FileProfileRepository`), not yet in the SQLite app DB of §5. Secrets are in the
  OS keychain as planned.
- The conformance harness gained a second table of per-engine connection checks (plain, TLS
  VerifyFull, SSH, TLS over SSH, cancel within 2 s, 200k-row streaming). The object-kind matrix
  of §3 stays pending until the dialect cells of M2.
- SSH tunnels are shared only between requests with the same SSH endpoint, user, target,
  host-key policy and credential, so a stricter profile never reuses a tunnel checked more
  loosely. Details: `docs/m1-contracts.md` §11.
- `mysql_async` trusts its bundled webpki roots (not the platform store) when no CA file is
  set.
- Saving profiles with their keychain secrets is in `oxbase-services::profiles` (used by the app;
  usable by the CLI, which has no connection commands yet). A stored database or SSH password stays with the destination it was saved for: an
  edit that changes the server, user, TLS or SSH route must retype it.
- The OS keychain backend is proven by its own tests (`scripts/keychain-tests.sh`, CI job
  `keychain` on Linux, macOS and Windows; Linux uses a private gnome-keyring), not by the Docker
  conformance run, which uses an in-memory secret store.

**M2 contracts (Design step, 2026-10-31).** `docs/m2-contracts.md` fixes the object tree
(per-engine `TreeLayout` data and `DatabaseScope` in `oxbase-dialect`; PostgreSQL reads other
databases through child sessions), the typed models for schemas, routines, triggers and
sequences, the `oxbase-sql` splitter API, query runs with a spilling result store
(`oxbase-services::query` / `results`), query history in a SQLite app DB (`oxbase-store`), and the
Tauri commands and channel events. Each query editor runs on its own child session, so a long
query never blocks the object tree.

**M2 as built (notes, 2026-10-31).** The exit check passed in the CI conformance job (Linux,
Docker fixtures; numbers in the README, *Conformance harness*). On each of PostgreSQL, MySQL,
MariaDB and SQLite, a 1 000 000-row query streamed in 0.9-2.3 s (two CI runs) into a result
store with an 8 MiB budget, spilling to disk. Random 200-row pages took under 0.3 ms at p95.
A cancel after about 250 000 rows ended the run within about 1 s, and the rows already
received stayed pageable. All 42 `fetch_list` cells pass. Where the build differs from the text above or from
`docs/m2-contracts.md`:

- Platforms: the full CI lane (Rust tests and OS keychain tests on Linux, macOS and Windows)
  and the Bundle workflow (Tauri builds for Linux x86_64, macOS x86_64 and aarch64, Windows
  x86_64) passed on `milestone/m2` at a0ee957 (manual runs 41232475071 and 41232477171). An
  earlier full-lane run found one Windows-only test failure (a test opened a directory with
  `File::open` to set its time), fixed before that run. The bundles are built but not
  launched: the Monaco worker, the canvas grid and the binary `query_page` responses in WebKit
  and WebView2 have no automated smoke test yet, and the page latency through Tauri IPC plus
  Arrow decoding in the webview is not measured (the 1M-row test times `QueryService::page`
  in-process).
- "Scroll smoothly" is proven by page latency in the core and by UI tests that check the grid
  asks only for the visible row ranges. There is no automated in-webview scrolling benchmark
  yet. That belongs with the webview smoke tests of §7.
- Only the `fetch_list` checks of the object-kind matrix run. `roundtrip` and `capabilities`
  stay pending until the DdlGenerator and Comparator cells of M3. The `[m2]` seed names are
  the acceptance list. CI now requires every M2 engine to register its tree layout on every
  lane (`OXBASE_CONFORMANCE_REQUIRE_M2_FETCH=1`).
- PostgreSQL: the catalog queries keep separate versions for 10, 11+ and 18, but CI runs only
  PostgreSQL 18.6. The older versions are checked by unit tests of the query text. Table
  listings carry no `SIZE_BYTES`, because `pg_total_relation_size` waits on table locks. New
  extras outside the contract: `NOT_ENFORCED`, `EXCLUSION` and `NULLS_FIRST`.
- MySQL/MariaDB: session temporary tables are not listed. Events have a fetcher but no tree
  folder. MariaDB sequences can be listed on 10.3+ but loaded only on 11.5+. Trigger
  definitions leave out `FOLLOWS`/`PRECEDES`. After a mid-stream cancel the driver needs
  several seconds to finish the killed stream (an earlier CI run measured 8-9 s before the
  editor's next `SELECT 1` could start on that session). The query service no longer waits for
  it: when the stopped stream has not wound down within 1 s, the editor's next run opens a new
  session at once, closes the old one in the background and warns that its temporary tables,
  settings and open transaction are gone. The live test requires that next `SELECT 1` within
  2 s on every engine. In CI run 41231834815 it took 3 ms on MySQL and MariaDB (new session),
  455 ms on PostgreSQL and 1 ms on SQLite (same session).
- The query service's unit tests no longer carry a stand-in splitter or redactor. They use
  `oxbase_sql::split` and `redact_secrets`.
- The Glide Data Grid is pinned to `6.0.4-alpha24`, the first line that declares React 19
  (`docs/m2-contracts.md` §9).

**M3 contracts (Design step, 2026-10-31).** `docs/m3-contracts.md` fixes the M3 contracts:
DDL scripts as `ScriptStep`s with phases, transactionality, destructive flags and
capture/condition steps (for the SQLite table rebuild); the `DdlGenerator` (with a
`DdlContext`), `Comparator` (one shared structural comparison, `oxbase_dialect::compare`, with
per-engine normalization rules and fixed property ids), `Capabilities::designer` and
`DmlWriter` (bound parameters, key-only match, `RETURNING` or a read-back query) roles and
their registration; the typed filter and sort model and the table-data `SELECT` with bound
values; the edit service (editable key or a read-only reason, a UI-held pending-changes buffer
applied in one transaction where every update and delete must hit exactly one row); the table
designer model, preview and apply; the Tauri commands; the `[m3]` fixtures; and the webview
smoke test that closes the M2 gap (Linux, `tauri-driver`, full lane).

**M3 as built (notes, 2026-11-01).** The exit check passed in the CI conformance job (Linux,
Docker fixtures, run 41254616693; numbers in the README, *Conformance harness*): `roundtrip` and
`capabilities` pass for table, column, index, foreign key and check on PostgreSQL 18.6, MySQL
8.4.13, MariaDB 11.8.9 and SQLite 3.54.1 (40 of 40 cells), and CI now requires them
(`OXBASE_CONFORMANCE_REQUIRE_M3_ROUNDTRIP=1`). Each round trip keeps a seeded row through every
alter and uses a reserved-word column and a renamed column with a quote and a non-ASCII
character. The services edit and designer live tests run against every engine's real cells
(`OXBASE_SERVICES_REQUIRE_M3_CELLS=1`). The M2 gap is closed on Linux: the webview smoke test
drives the debug app through WebKitGTK (`tauri-driver`), and page latency through IPC plus
Arrow decoding in the webview was p95 11 ms; it also edits a cell, applies and sees it after a
reload. On the same code (`milestone/m3` at 58b0545), the full CI lane (Rust tests: 665 passed
on Linux and macOS, 664 on Windows; OS keychain tests on all three; conformance; webview smoke)
and the Bundle workflow (Linux x86_64, macOS aarch64 and x86_64, Windows x86_64) passed in
manual runs 41255149525 and 41255151166. Where the build differs from the text above or from
`docs/m3-contracts.md` (the contract file marks each deviation *As built*):

- PostgreSQL registers its own `PgComparator` (PostgreSQL defaults filled in on both sides, then
  the shared walk) instead of `RulesComparator(PgRules)`. Sessions now pin
  `standard_conforming_strings=on`, and generated DDL writes backslash literals as `E'...'`.
- MySQL / MariaDB: when a check, generated column, expression default or expression key uses a
  renamed column, renames move into the main `ALTER` as `CHANGE COLUMN` (MySQL refuses the
  separate rename). Removing character set, collation, `ON UPDATE` or `INVISIBLE` needs an
  explicit null extra; an absent one keeps the old value.
- SQLite rebuild: copies values without `CAST` (casts to NUMERIC-affinity types lose data),
  runs renames natively after the copy so SQLite rewrites triggers and views, guards against
  triggers missing from the context, and checks foreign keys database-wide filtered to the
  table. Clauses the model cannot hold (`ON CONFLICT`, key `COLLATE`) make a rebuild
  `Unsupported`.
- Designer validation checks names in two groups (indexes; constraints), so MySQL's FK-backing
  index may share its FK's name, and treats names that differ only in case as duplicates on
  every engine (no per-engine case flag yet).
- Webview smoke: Linux only. No Windows job (the PostgreSQL fixture is a Linux container) and
  macOS has no WebDriver for WKWebView. Integration found one bug no unit test could: Glide
  reports the overlay editor's cell in grid columns (row markers included), so edits opened on
  the next column; fixed in `ui/src/workbench/results/gridCoords.ts`.
- Not modelled yet: a PRIMARY KEY's own `DEFERRABLE` on PostgreSQL (lost if the designer
  changes the key's columns); identity / auto-increment changes on an existing column have no
  capability of their own (the UI gates them on the type-change capability).

**M4 contracts (Design step, 2026-11-01).** `docs/m4-contracts.md` fixes the SQL Server
contracts. Driver: `tiberius` 0.13.0 for TDS 7.4, built without a TLS backend, because its
`rustls` feature pulls aws-lc (tokio-rustls default features, hard-coded aws-lc provider) and its
other backends are OpenSSL; `oxbase-mssql` runs the TLS handshake itself with rustls + ring (TLS
records inside PRELOGIN packets for TDS 7.4, direct TLS with ALPN `tds/8.0` for TDS 8.0 strict)
and replays the server's PRELOGIN answer to tiberius. Cancel is an in-band attention (works
through SSH), with the socket closed when the drain does not finish within 1.5 s. Accepted gaps
of tiberius: no `PRINT` messages, no row counts for plain batches, coarse result metadata
(integers as `Int64`, decimals as text). SQL Server reads one database per session
(`DatabaseScope::Session`, like PostgreSQL), names are `[schema].[name]`; the tree adds a Types
folder and indexes under views; DDL is transactional, defaults are named constraints,
renames use `sp_rename`, IDENTITY cannot change on an existing column; the grid writer returns
one row per affected row through `OUTPUT ... INTO` a table variable, so triggers and `NOCOUNT`
cannot break the "exactly one row" check. Fixtures gain TLS on the mssql container, a TLS-only
login enforced by a logon trigger, the SSH target `mssql:1433` and the `[m2.mssql]`,
`[m3.mssql]` and `[m4]` names. Six streams (driver, fetch, ddl, sql, fixtures, app) fill the
skeleton of `crates/oxbase-mssql`.

**M4 as built (notes, 2026-11-01).** The exit check passed in the CI conformance job (Linux,
Docker fixtures, with `OXBASE_CONFORMANCE_REQUIRE_M4=1`, the 5-engine guard; numbers in the
README, *Conformance harness*): on PostgreSQL 18.6, MySQL 8.4.13, MariaDB 11.8.9, SQLite 3.54.1
and SQL Server 2025 (17.0.5035) every M1 connection check passes (27 of 27 applicable; SQL
Server: plain, TLS `VerifyFull` as the TLS-only login, SSH, TLS over SSH, cancel, 200k-row
stream), every `fetch_list` cell passes (55; SQL Server 13: database, schema, table, column,
index, foreign key, check, view, function, procedure, trigger, sequence, type) and `roundtrip`
and `capabilities` pass for table, column, index, foreign key and check (50 of 50 cells). The
services live tests run on SQL Server too: a 1 000 000-row query streamed in 3.5-4.4 s (two
runs) with random 200-row pages at p95 0.20 ms, a cancel after about 200 000 rows ended the run
in about 1 s and the editor's next `SELECT 1` took under 100 ms (on a new session), and the
edit and designer cycles pass with SQL Server's own grid writer and DDL cells. On the same code
(`milestone/m4` at d47cb5d), the full CI lane (Rust tests: 766 passed on Linux and macOS, 765 on
Windows; OS keychain tests on all three; conformance; webview smoke, IPC plus Arrow decoding p95
11 ms) and the Bundle workflow (Linux x86_64, macOS aarch64 and x86_64, Windows x86_64) passed in
manual runs 41298470889 and 41298472236. Where the build differs from the text above or from
`docs/m4-contracts.md` (its §13 lists every deviation):

- tiberius gaps as planned: no `PRINT` messages (the editor says so for SQL Server), no row
  counts for plain batches, coarse result metadata. tiberius itself decodes `money` through
  `f64`; oxbase carries a vendored copy (`vendor/tiberius`, one patched function) that decodes
  `money` / `smallmoney` exactly.
- `Tx::commit` fails (error 50000) when the server already ended the transaction, instead of
  reporting a commit; `begin()` still nests inside an editor's own `BEGIN TRAN`.
- After a cancel mid-stream SQL Server behaves like MySQL / MariaDB in the query service: the
  attention drain of a streaming result takes longer than the service's 1 s wait, so the next
  run gets a new session and the warning about lost `#temp` tables and settings.
- The SQL Server fixture enforces the TLS-only login with a logon trigger that reads
  `sys.dm_exec_connections` and runs `sqlservr` as root inside the container.
- The webview smoke test stays on PostgreSQL; the SQL Server UI (connection dialog, designer
  types and options, PRINT note) is covered by Vitest only.

**M5 contracts (Design step, 2026-11-01).** `docs/m5-contracts.md` fixes the import / export
contracts. `oxbase-io` reads and writes files only, synchronously behind async adapters with
bounded queues: CSV / TSV (`csv` + `encoding_rs`: UTF-8, UTF-16, Windows-1252, true Latin-1,
BOMs), JSON (array streamed element by element, NDJSON; nested values as JSON text), XLSX
(`calamine` streaming cell reader; `rust_xlsxwriter` in constant-memory mode with sheet
splitting at 1 048 576 rows and formula-injection protection), Parquet (`parquet` 60, the
workspace's arrow major; Snappy, gzip, Brotli, LZ4 and zstd) and SQL dumps (export only: the
target engine's `DdlGenerator` and `Quoting` literals). Readers infer types over a sample and
produce batches in one canonical Arrow type per logical type; unreadable cells are NULL plus a
row issue. Writers write a temporary file in the destination directory and rename it on
success. A new per-engine role `BulkLoader` (`oxbase_dialect::bulk`, outside the conformance
matrix) loads those batches: PostgreSQL `COPY FROM STDIN` through tokio-postgres (reached with
the new `Connection::as_any`), MySQL / MariaDB multi-row parameterized inserts (no `LOAD DATA
LOCAL INFILE`: off by default on the server and it lets the server pick client files), SQLite
prepared inserts, SQL Server TDS bulk load; batch sizes follow each engine's parameter limit
(65 535, 65 535, 32 766, 2 100); a generic multi-row `INSERT` loader is the fallback for every
engine. `oxbase-services` gains `ImportService` (preview, plan, run on a dedicated child session
with target modes create / append / truncate / delete-all / replace, error and transaction
policies, a per-engine table of what a cancel leaves behind, cancel within 2 s) and
`ExportService` (a table with filter and sort, one query, or every stored row of a result,
spilled ones included). The webview never sends file paths: `file_pick_open` / `file_pick_save`
show native dialogs from Rust and return opaque handles that the import and export commands
take. The exit check is a CI job `import-perf` that generates a 1 GB CSV on the runner and
imports it into PostgreSQL and MySQL with checksum, progress, memory and cancel assertions. Six
streams (read, write, load, services, app, perf) fill the skeletons.

**M5 as built (notes, 2026-11-01).** The exit check passed in the CI job `import-perf` (Linux,
`--profile perf`, PostgreSQL and MySQL fixtures, `OXBASE_REQUIRE_M5_PERF=1`; full-lane run
41328475066 on `milestone/m5` at 2434d20): a generated 1 238 093 427-byte CSV of 10 000 000 rows
was imported into PostgreSQL 18.6 with `COPY` in 60.7 s (164 742 rows/s, 20.4 MB/s, 22 MiB peak
RSS growth) and into MySQL 8.4.13 with multi-row inserts in 91.9 s (108 845 rows/s, 13.5 MB/s,
87 MiB), the checksum matched on both, progress events were at most 498 ms apart, and a cancel
at about 25% ended the import 1 ms / 152 ms after the call with exactly the committed rows (2.6
/ 2.5 million) left, as the report said. "Export of any result or table": the services live
tests export a filtered and sorted table, a query and a 300 000-row spilled result, and round-trip
a table through CSV, JSON, XLSX and Parquet on all five SQL engines (SQL dumps run back on
PostgreSQL and SQLite, and each engine's own dump on PostgreSQL, MySQL, MariaDB and SQL Server
in `oxbase-io`'s live test); the webview smoke test imports a CSV and exports a result grid's
query through the app (debug-only `--e2e-file-dir` hook, required with
`OXBASE_E2E_REQUIRE_M5=1`). All five SQL engines register a native `BulkLoader` (unit-tested).
The full CI lane (Rust tests: 949 passed on Linux and macOS, 948 on Windows; OS keychain tests on
all three; webview smoke; import-perf) and the Bundle workflow (Linux x86_64, macOS aarch64 and
x86_64, Windows x86_64, run 41328476718) passed; that conformance run failed once on a SQLite
cancel test that expected its own rollback to succeed after SQLite had already rolled the
interrupted transaction back (test fixed). Where the build differs from the text above or from
`docs/m5-contracts.md` (its §13 lists every deviation):

- SQL Server loads with typed, parameterized multi-row `INSERT`s (2 098 parameters, 1 000 rows
  per statement; about 7 000 rows/s for 100 000 rows of every type in a debug build), not TDS
  bulk load: tiberius needs exact per-column wire types and a half-sent bulk stream cannot be
  cancelled in-band. SQLite also uses multi-row inserts (32 766 parameters).
- SQLite `decimal` columns of tables the import creates get `NUMERIC` affinity, which keeps
  about 15 significant digits; mapping them to `TEXT` is still open.
- The import sends a heartbeat `progress` event during a slow load call, commit or read (the
  first PostgreSQL run had 2 s gaps during commits). Text that is a plain number (SQL Server
  decimals and unconstrained PostgreSQL `numeric` arrive as text) never gets the formula guard.
  XLSX keeps times to the millisecond by cutting, not rounding. A SQL dump written under another
  table name renames the source's key, index and constraint names.
- CSV `trim` also trims quoted fields (the parser does not report quoting). With line ending
  `cr`, comment lines still end only at LF.
- The 1 GB check runs on PostgreSQL and MySQL only; at the SQL Server rates measured in CI
  (3 900 to 7 000 rows/s) the same file would take about 25 to 45 minutes.

Order of work: M0–M3 prove the architecture. M4 and M7 are where the per-database matrix gets tested for real. M8 and M10 reuse the Comparator and DdlGenerator cells and get much cheaper if M3 is done right.

## 7. Main risks

| Risk | Mitigation |
|---|---|
| Scope (this is Navicat parity, a multi-quarter effort) | Strict milestone exit criteria. M10–M12 can move to v1.1 without blocking the core. |
| SQL Server through tiberius: no Windows integrated auth on Linux/macOS, gaps around bulk copy; no TLS backend we can use (aws-lc / OpenSSL), no INFO messages, no batch row counts, coarse result metadata (M4 evaluation) | SQL auth first (Entra later). oxbase does the TDS TLS itself (rustls + ring). The INFO / count / metadata gaps are accepted for M4; a TDS client of our own or upstream changes replace tiberius behind `MsSqlConnection` later. Bulk insert is tested with M5 import. |
| Mongo shell compatibility | Support a defined subset (find, aggregate, CRUD, `db.*` helpers) and say so in the docs |
| Autocomplete quality across dialects | tree-sitter for context plus a catalog-driven engine; measure on real schemas |
| Webview differences (WebKitGTK on Linux is the weakest) | Run CI smoke tests on each OS's webview; keep the canvas grid off DOM-heavy paths |
| Type fidelity across engines (transfer/sync) | Per-pair type mapping tables, tested in the conformance suite |

## 8. Open questions

1. ~~Product name~~ **Decided 2026-10-31: Oxbase.** The crate prefix is `oxbase-`, replacing the `dbx` placeholder.
2. ~~License~~ **Decided 2026-10-31: Apache-2.0, open core.** The public repo is Apache-2.0. Paid features (team sync, enterprise SSO and similar) live in a separate private repo that depends on the public crates. Public code must never depend on private code. Dependencies must have licenses compatible with Apache-2.0, enforced by `deny.toml`. Outside contributors sign a **CLA** (decided 2026-10-31), based on the Apache Individual and Corporate CLAs. It grants a broad copyright and patent license with the right to sublicense, so contributions can also ship in the paid edition. The CLA is enforced on pull requests by the CLA Assistant GitHub Action. For now the licensor is Mohammad Hoseinzadeh, and the CLA contact is hoseinzadeh.ashraf@gmail.com. The repo is private at https://github.com/m-hoseinzadeh/oxbase.
3. Team size and target date. These set how many milestones run in parallel.
4. Should Oracle join in v1.1 (rust-oracle needs Oracle Instant Client at runtime)?
