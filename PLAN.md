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
│               Capabilities / DmlWriter                │
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
  oxbase-mssql        tiberius (TDS, rustls)
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

Order of work: M0–M3 prove the architecture. M4 and M7 are where the per-database matrix gets tested for real. M8 and M10 reuse the Comparator and DdlGenerator cells and get much cheaper if M3 is done right.

## 7. Main risks

| Risk | Mitigation |
|---|---|
| Scope (this is Navicat parity, a multi-quarter effort) | Strict milestone exit criteria. M10–M12 can move to v1.1 without blocking the core. |
| SQL Server through tiberius: no Windows integrated auth on Linux/macOS, gaps around bulk copy | SQL auth and Azure AD first. Test bulk insert early in M4. |
| Mongo shell compatibility | Support a defined subset (find, aggregate, CRUD, `db.*` helpers) and say so in the docs |
| Autocomplete quality across dialects | tree-sitter for context plus a catalog-driven engine; measure on real schemas |
| Webview differences (WebKitGTK on Linux is the weakest) | Run CI smoke tests on each OS's webview; keep the canvas grid off DOM-heavy paths |
| Type fidelity across engines (transfer/sync) | Per-pair type mapping tables, tested in the conformance suite |

## 8. Open questions

1. ~~Product name~~ **Decided 2026-10-31: Oxbase.** The crate prefix is `oxbase-`, replacing the `dbx` placeholder.
2. ~~License~~ **Decided 2026-10-31: Apache-2.0, open core.** The public repo is Apache-2.0. Paid features (team sync, enterprise SSO and similar) live in a separate private repo that depends on the public crates. Public code must never depend on private code. Dependencies must have licenses compatible with Apache-2.0, enforced by `deny.toml`. Outside contributors sign a **CLA** (decided 2026-10-31), based on the Apache Individual and Corporate CLAs. It grants a broad copyright and patent license with the right to sublicense, so contributions can also ship in the paid edition. The CLA is enforced on pull requests by the CLA Assistant GitHub Action. For now the licensor is Mohammad Hoseinzadeh, and the CLA contact is hoseinzadeh.ashraf@gmail.com. The repo is private at https://github.com/m-hoseinzadeh/oxbase.
3. Team size and target date. These set how many milestones run in parallel.
4. Should Oracle join in v1.1 (rust-oracle needs Oracle Instant Client at runtime)?
