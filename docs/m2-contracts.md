# M2 contracts

Status: frozen for M2 on 2026-10-31 (Design step). M2 exit check (PLAN.md §6): run a 1M-row
query and scroll smoothly; cancel mid-stream. Scope: object tree (fetchers for schema, table,
view, index, function, trigger, sequence), query editor, splitter, streaming results grid,
multiple result sets, history, on PostgreSQL, MySQL, MariaDB and SQLite.

Changes to these contracts go through the Design owner. Streams code against them and must not
change public signatures; adding private helpers, private fields and tests is always fine. Rust
doc comments on the items named here are part of the contract and are more detailed than this
file.

## 1. Who owns what

Bodies marked `unimplemented!("m2: <stream>")` belong to that stream. No test may call one
until its stream has filled it.

| Area | Path | Owner | State at hand-off |
|---|---|---|---|
| Object model: `ObjectRef::signature`, `SchemaDef`, `RoutineDef`, `TriggerDef`, `SequenceDef`, `IdentityDef`, new `props::*`, `ObjectKind::plural_display_name` | `crates/oxbase-types` | Design | done, tested |
| `Connection::current_database` (default `None`), mock reports `ConnectParams::database` | `crates/oxbase-driver` | Design | done |
| `TreeLayout`, `ChildGroup`, `DatabaseScope`, registry additions | `crates/oxbase-dialect` | Design | done, tested |
| Splitter API, position helpers, `pick_statement` | `crates/oxbase-sql` | Design | done, tested |
| `split`, `redact_secrets` bodies | `crates/oxbase-sql` | **sql** | skeleton |
| PostgreSQL dialect: `oxbase_pg::dialect::register`, fetchers, layout, capabilities, quoting; `PgConnection::current_database`; `tests/docker/init/postgres/04-m2.sql`; the pg line in `crates/oxbase-services/src/dialects.rs` | `crates/oxbase-pg`, fixtures | **pg** | - |
| MySQL + MariaDB dialect: `oxbase_mysql::dialect::register` (both engines); `current_database`; `tests/docker/init/{mysql,mariadb}/04-m2.sql`; its line in `dialects.rs` | `crates/oxbase-mysql`, fixtures | **mysql** | - |
| SQLite dialect: `oxbase_sqlite::dialect::register`; `current_database` = `main`; `tests/docker/init/sqlite/01-m2.sql`; its line in `dialects.rs` | `crates/oxbase-sqlite`, fixtures | **sqlite** | - |
| `AppDb` (open, migrations, settings) and `SqliteHistoryRepository` bodies | `crates/oxbase-store/src/app_db.rs` | **store** | skeleton |
| `HistoryEntry`, `HistoryQuery`, `HistorySettings`, `HistoryRepository`, `MemoryHistoryRepository`, `app_data_dir`, `MIGRATIONS` v1 | `crates/oxbase-store` | Design | done, tested |
| `ConnectionManager::open_child` + child bookkeeping in `close` / `list_open` / `get`; `TreeService`; `QueryService`; `ResultStore` | `crates/oxbase-services` | **services** | skeleton |
| `SessionHandle::exclusive`, `SessionSummary::{database, parent}`, `default_dialects`, `HistoryService`, `ByteBudget`, `RunDir`, `encode_ipc_stream`, event/payload types | `crates/oxbase-services` | Design | done, tested |
| Conformance harness on `default_dialects()` and the `fetch_list` cells | `crates/oxbase-conformance` | **services** | - |
| Workbench commands, state wiring, capabilities | `crates/app-tauri` | Design (contract) | forwarding bodies done |
| Switch the app to `SqliteHistoryRepository`; the UI: tree panel, Monaco editor, Glide grid, history panel | `crates/app-tauri`, `ui/` | **app** | - |
| TS payload types and command wrappers | `ui/src/api/**` | Design | done, tested |
| Dependencies and lockfiles | root `Cargo.toml`, `Cargo.lock`, `ui/package.json`, `ui/pnpm-lock.yaml` | Design | done (§9) |

Engine streams add `oxbase-dialect` to nothing: it is already a dependency of `oxbase-pg`,
`oxbase-mysql` and `oxbase-sqlite`, so no stream needs to touch `Cargo.lock`. A stream that needs
a new third-party crate adds it with an explicit version to its own crate and reports it for
hoisting.

## 2. Object model (`oxbase-types`)

`ObjectRef { kind, catalog, schema, parent, parent_kind, name, signature }`:

- `catalog` is the **database**: PostgreSQL database, MySQL/MariaDB database (= schema), SQLite
  schema name (`main`, `temp`, attach name). `ObjectRef::database_name()` returns it (or the name of a
  `Database` ref). Fetchers always set it for listed objects.
- `schema` only on PostgreSQL (and SQL Server later).
- `parent` / `parent_kind` for table children (column, index, FK, check, trigger).
- `signature` (new) tells PostgreSQL routine overloads apart: the identity argument types as
  `pg_get_function_identity_arguments` prints them (`integer, text`; `""` for no arguments).
  `None` everywhere else, including MySQL/MariaDB routines (no overloading).

`ObjectDef` variant per kind:

| Kind | Variant | Notes |
|---|---|---|
| Database, Schema | `Schema(SchemaDef)` | owner, comment; charset/collation/encoding in extras |
| Table | `Table(TableDef)` | columns (type, nullability, default, `auto_increment`, `identity`), PK, indexes (without the PK index), FKs, checks, comment |
| Column / Index / ForeignKey / Check | `Column{table,column}` etc. | |
| View, MatView | `View(ViewDef)` | `definition` = query text after `AS` |
| Function, Procedure | `Routine(RoutineDef)` | args (name, mode, native type, default), returns, language, body, full `definition` |
| Trigger | `Trigger(TriggerDef)` | timing, events, `UPDATE OF` columns, level, `WHEN`, PG trigger `function`, inline `body`, full `definition`, enabled |
| Sequence | `Sequence(SequenceDef)` | start/increment/min/max/cache as `i128` (JSON **strings**), cycle, `owned_by` column ref |
| other | `Generic(GenericDef)` | |

Engine-specific facts go in `extras` (`props::DETERMINISTIC`, `VOLATILITY`, `DEFINER`,
`SQL_DATA_ACCESS`, `SECURITY`, `VIEW_ALGORITHM`, `POPULATED`, `TEMPORARY`, `TABLE_TYPE`,
`ENCODING`, `CHARSET`, `COLLATION`, `FILE`, ...). Listing rows (`ObjectSummary.extras`) use
`NATIVE_TYPE` and `PRIMARY_KEY` for columns, `CURRENT` for the session's database, `SIZE_BYTES`,
and the index listing marks the PK-backing index with `PRIMARY_KEY = true` (it *is* listed under
Indexes, while `TableDef.indexes` leaves it out).

## 3. Object tree and fetchers (`oxbase-dialect`, engine crates)

### 3.1 Registration

Each engine crate gets a `dialect` module with `pub fn register(reg: &mut DialectRegistry)`.
`oxbase_mysql::dialect::register` registers **both** `Engine::MySql` and `Engine::MariaDb`
(version and flavour checks inside the cells). `register` must:

1. register a `Fetcher` for every kind in its `TreeLayout` (test:
   `reg.missing_layout_fetchers(engine).is_empty()`);
2. register the `TreeLayout` (`register_tree_layout`);
3. register `Capabilities` (at least `supports_kind`, version-aware) and `Quoting`;
4. mark the kinds the engine does not have: `mark_not_applicable_except(engine, &[...])`.

`oxbase_services::default_dialects()` calls them; the line for each engine is pre-written as a
comment in `crates/oxbase-services/src/dialects.rs`, and each engine stream uncomments only its
own line. Engines that registered nothing keep the provisional M0 applicability table (SQL
Server, MongoDB, Redis in M2). Services resolves `(engine, kind) → Fetcher` with
`DialectRegistry::fetcher(engine, kind)` (`Unsupported` when missing) and filters kinds with
`DialectRegistry::supports_kind(engine, kind, &server_version)`.

### 3.2 Tree shape (`TreeLayout`)

```
connection
└─ databases (direct)
   ├─ PostgreSQL: schemas (direct) → folders: Tables, Views, Materialized views, Functions,
   │               Procedures (11+), Sequences
   ├─ MySQL/MariaDB: folders: Tables, Views, Functions, Procedures, Sequences (MariaDB 10.3+)
   └─ SQLite: folders: Tables, Views
table → folders: Columns, Indexes, Foreign keys, Check constraints, Triggers
view  → folders: Columns, Triggers (PostgreSQL, SQLite: INSTEAD OF) / Columns (MySQL, MariaDB)
materialized view (PostgreSQL) → folders: Columns, Indexes
```

`DatabaseScope`: PostgreSQL `Session` (one database per session), MySQL/MariaDB `Catalog`,
SQLite `Attached` (`main`, `temp` when it has objects, attached databases from
`pragma_database_list`, `FILE` extra = the file path).

### 3.3 Fetcher rules

Spelled out on the `Fetcher` trait: catalog queries with **bound parameters only** (no names
interpolated into SQL), query choice by `Connection::server_version()`, the parent convention
(Database ← root; Schema ← Database; schema-level kinds ← Schema on PostgreSQL, ← Database on
MySQL/MariaDB/SQLite; table children ← owning table/view/matview), refs built with
`parent.child(...)`, sorted by name (columns by ordinal), `system = true` for system objects,
`Error::Cancelled` and nothing partial on cancel, `Error::NotFound` from `load` when gone.
SQLite: table-valued pragma functions (`pragma_table_list`, `pragma_table_info(?, ?)`,
`pragma_index_list`, `pragma_foreign_key_list`) take the table *and* schema as parameters.

### 3.4 Which session reads what

- A session from `ConnectionManager::open` is a **base** session; the tree reads through it.
- `Connection::current_database()` (driver): PostgreSQL = connected database (or the user name,
  the server default), MySQL/MariaDB = connect-time default database, SQLite = `main`.
- On `DatabaseScope::Session` engines a node of another database is read on a **metadata
  child** (`ConnectionManager::open_child(base, Some(db))`), cached per (base, database) by
  `TreeService`, closed by `TreeService::forget` or with the base. The child reuses the base's
  resolved parameters and SSH tunnel; no secret is asked again.
- Children are invisible to `list_open`; closing a base closes its children first.
- Every reader holds `SessionHandle::exclusive()` while a statement is open (one statement at a
  time per connection).

## 4. Splitter (`oxbase-sql`)

```rust
pub fn split(dialect: SqlDialect, text: &str) -> Vec<Statement>;
pub fn statement_at(dialect: SqlDialect, text: &str, byte_offset: usize) -> Option<Statement>;
pub fn pick_statement(statements: &[Statement], byte_offset: usize) -> Option<&Statement>;
pub fn redact_secrets(dialect: SqlDialect, sql: &str) -> Cow<'_, str>;
pub fn position_at(text: &str, byte_offset: usize) -> Position;          // 1-based, UTF-16 cols
pub fn byte_offset_from_utf16(text: &str, utf16_offset: usize) -> usize;
pub enum SqlDialect { Postgres, MySql { mariadb: bool }, Sqlite, TSql /* M4 */ }
pub struct Statement { range: Range<usize>, segment_end, start: Position, end: Position,
                       text: String, kind: StatementKind /* Query | Other: a hint */ }
```

Range rules (no leading whitespace or ordinary comments, no terminator, executable `/*! */`
and hint `/*+ */` comments are content, comment-only segments and `DELIMITER` lines produce no
statement) and the per-dialect lexing rules (PostgreSQL dollar quotes, `E''` escapes, nested
block comments, `BEGIN ATOMIC`; MySQL `#` and `-- ` comments, backslash escapes, backticks,
`DELIMITER`, compound `BEGIN ... END` bodies, MariaDB `BEGIN NOT ATOMIC`; SQLite trigger
`BEGIN ... END`, `[ident]`; T-SQL `GO` lines) are in the crate docs and are the sql stream's
acceptance list. `split` never fails: unterminated constructs run to the end.

`statement_at` ownership rule: statement *i* owns byte offsets `(segment_end[i-1],
segment_end[i]]`, so a cursor just after `;` picks the statement before it and a cursor on the
next blank line picks the next statement; past the end picks the last.

## 5. Tauri commands and events (`app-tauri`, `ui/src/api`)

Command wrapper payloads are camelCase; core models inside them (`ObjectRef`, `ObjectSummary`,
`ObjectDef`, `Value`, `DataType`) keep their snake_case serde form. Every command rejects with
`CommandError { category, message }`. TS mirrors: `ui/src/api/types.ts`; wrappers:
`ui/src/api/workbench.ts`.

| Command | Arguments | Returns |
|---|---|---|
| `tree_children` | `sessionId`, `node: TreeNodeRef`, `options?: { showSystem }`, `requestId?` | `TreeNode[]` |
| `object_load` | `sessionId`, `object: ObjectRef`, `requestId?` | `ObjectDef` |
| `query_run` | `sessionId`, `editorId`, `script`, `options?: RunOptions`, `onEvent: Channel<RunEvent>` | `RunId` |
| `query_page` | `runId`, `resultIndex`, `offset`, `limit` | **raw bytes**: Arrow IPC stream (`tauri::ipc::Response`) |
| `query_cancel` | `runId` | `boolean` (was running) |
| `query_close` | `runId` | `void` |
| `query_close_editor` | `editorId` | `void` |
| `statement_at` | `engine`, `text`, `offset` (UTF-16 units, Monaco `getOffsetAt`) | `StatementSpan \| null` |
| `history_list` | `query?: HistoryQuery` | `HistoryEntry[]` newest first |
| `history_delete` | `ids: string[]` | number removed |
| `history_clear` | `profileId?` | number removed |
| `history_get_settings` / `history_set_settings` | - / `settings` | `HistorySettings` / `void` |

`requestId` values are cancelled with the M1 `cancel_request`. `TreeNodeRef` is
`{type:"connection"} | {type:"folder", parent, kind} | {type:"object", object}`; `TreeNode` is
`{ node, key, label, expandable, summary? }`.

`RunEvent` (`{ "type": ..., camelCase fields }`), in order:
`started {runId, statements: StatementSpan[]}` → per statement `statementStarted` →
`resultSet {resultIndex, statementIndex, statementRange, columns}` → `rows {resultIndex,
totalSoFar}`* → `resultDone {resultIndex, totalRows, truncated}`, `notice` anywhere →
`statementDone {statementIndex, rowsAffected?, elapsedMs}` or `statementFailed {statementIndex,
error, elapsedMs}` → exactly one terminal `finished | failed | cancelled {elapsedMs}`.
`resultIndex` counts result sets across the run (a statement can have several, e.g. MySQL
`CALL`).

Every command is registered in `generate_handler!`, listed in `build.rs` and granted in
`capabilities/default.json`. The app stream must verify at runtime that the `Channel` messages
arrive (Tauri's internal channel fetch command is not an app permission).

## 6. Query runs and the result store (`oxbase-services::query`, `::results`)

Full rules in the module docs; the invariants every implementation and test must hold:

1. **Editor sessions.** Each `EditorId` has its own child session; one run at a time per editor
   (second is `InvalidInput`); a run on another `database` reopens the child on it (ignored on
   `Attached`); a new run closes the editor's earlier runs unless `keepPrevious`.
2. **Order.** Statements run in script order on one session; events follow §5.
3. **Bounded memory.** Batch data per run ≤ `memory_budget_bytes` (default 64 MiB) plus one batch
   in flight plus ≤ 4 cached decoded segments; past the budget, a store spills its in-memory
   batches to an Arrow IPC *file* segment in the run's `run-*` directory. Events use a bounded
   channel (default 256); `rows` events are lossy, all others are awaited.
4. **Paging while streaming.** `page(run, result, offset, limit)` returns rows clipped to what
   has arrived, as one batch with the result's schema, at any time; rows never change once
   stored; `limit` is clamped to `max_page_rows` (50 000).
5. **Disk cap.** All runs share `disk_cap_bytes` (default 8 GiB). At the cap (or `maxRows`) the
   statement's stream is dropped, its result `truncated`, a warning `notice` emitted, and the run
   goes on.
6. **Cancel.** `query_cancel` ends the run with `cancelled` within **2 s**, the server-side
   statement is cancelled (driver contract, M1 §7), rows received so far stay pageable and their
   results are marked truncated. The editor's next run does not wait for the driver: when the
   stopped stream did not wind down within the 1 s stop grace (MySQL/MariaDB keep killing and
   draining a large streaming result for several seconds), that run opens a new session on the
   same database right away, closes the old one in the background and emits a warning `notice`
   (temporary tables, settings and an open transaction of the old session are gone); otherwise
   it reuses the session. Later statements of the *same* run keep the session.
7. **Cleanup.** `query_close` / `query_close_editor` / dropping the service delete the run
   directories; startup removes `run-*` directories older than 24 h. Nothing is written outside
   `QueryConfig::spill_dir` (default `<temp>/oxbase-results`).
8. **No JSON rows.** Rows reach the UI only as Arrow IPC bytes from `query_page`; events carry
   counts and column metadata, never values.

## 7. History (`oxbase-store`, `oxbase-services::history`)

- App DB: `oxbase.db` in `app_data_dir()` (`OXBASE_DATA_DIR` overrides; Linux
  `~/.local/share/oxbase`, macOS `~/Library/Application Support/dev.oxbase.app`, Windows
  `%LOCALAPPDATA%\oxbase`), rusqlite bundled, WAL, `schema_migrations` table, append-only
  `MIGRATIONS` (v1: `settings`, `query_history` with times in Unix ms).
- One `HistoryEntry { id (UUID v7), profileId, database?, sql, startedAt, durationMs, status:
  ok|error|cancelled, rows?, error? }` per statement that started.
- `HistoryRepository`: `append`, `list(HistoryQuery { profileId?, search?, before?, limit })`
  newest first (search = ASCII case-insensitive substring of `sql`, literal, `LIKE` escaped),
  `delete(ids)`, `clear(profileId?)`, `prune(keep)`, `settings`, `save_settings`.
- **Secrets.** SQL text can contain secrets (`CREATE USER ... PASSWORD '...'`, `IDENTIFIED BY`,
  connection strings). `HistoryService::record` always passes the SQL and the error text through
  `oxbase_sql::redact_secrets` (rule in its docs: string literals after `PASSWORD`,
  `IDENTIFIED [WITH plugin] BY`, `MASTER_PASSWORD`, `SOURCE_PASSWORD`, `SECRET`, `KEY` become
  `'***'`; `password=`/`pwd=` values inside literals become `***`), truncates (64 KiB SQL, 4 KiB
  error) and writes nothing when `HistorySettings.enabled` is off. Repositories never log SQL.
  Redaction is best effort; turning history off is the guarantee.
- Retention: `max_entries` (default 10 000; 0 = unlimited), pruned every 64 appends and on
  settings change.

## 8. Fixtures (`tests/fixtures.toml` `[m2]`)

Names only; each dialect stream writes its engine's init SQL at `[m2.<engine>].init_sql`:
tables `m2_parent` (PK, unique `code`, check `m2_parent_code_chk`, comment "M2 parent table"),
`m2_child` (PK, FK `m2_child_parent_fk` ON DELETE CASCADE, check `m2_child_qty_chk`, index
`m2_child_parent_idx`), view `m2_view`, trigger `m2_trg` on `m2_child`, function `m2_fn(n)`,
procedure `m2_proc` (pg, mysql, mariadb), sequence `m2_seq` (pg, mariadb), PostgreSQL schema `m2`,
matview `m2_mview`, trigger function `m2_trg_fn` and a second database `oxbase_m2_other` with
`public.m2_other_t`; MySQL/MariaDB objects directly in database `oxbase_test`; SQLite seed
applied to a temp file plus an attached `m2aux` database with `m2_aux_t`.

## 9. Dependencies (added by Design; licenses checked)

Rust (workspace): `arrow-select` 60.1.0 (Apache-2.0, already in the graph through `arrow-ipc`);
`tempfile` 3.27.0 now also a normal dependency of `oxbase-services`; `rusqlite` 0.41.1 (bundled)
and `chrono` added to `oxbase-store`; `oxbase-dialect` to the three engine crates; `oxbase-sql`,
`oxbase-dialect`, arrow crates, `futures`, `async-trait`, `serde_json`, `tempfile` to
`oxbase-services`. No new TLS stack: `cargo tree -i openssl-sys` / `aws-lc-sys` stay empty. The
splitter needs no helper crate.

UI (`ui/package.json`, exact versions, all MIT; dompurify via monaco is MPL-2.0 OR Apache-2.0):

| Package | Version | Why / compatibility |
|---|---|---|
| `monaco-editor` | 0.57.0 | Bundled locally (the CSP has no CDN). ESM only. |
| `@monaco-editor/react` | 4.7.0 | Peer `react ^19`. Must load the local copy: `loader.config({ monaco })`. |
| `@glideapps/glide-data-grid` | 6.0.4-alpha24 | Canvas grid. The stable 6.0.3 (Feb 2024) declares React ≤ 18 only; the `beta` line declares React 19 (`^16.12 \|\| 17 \|\| 18 \|\| 19`) with the same API, so it is pinned exactly. Its peers are installed: |
| `lodash` | 4.18.1 | glide peer |
| `marked` | 16.4.2 | glide peer (`^16.0.10`) |
| `react-responsive-carousel` | 3.2.23 | glide peer (image overlay) |

Monaco + Vite (for the app stream; CSP is `script-src 'self'`, workers count as scripts):

```ts
import * as monaco from "monaco-editor/editor";               // core API, no languages
import "monaco-editor/languages/definitions/pgsql/register";   // + mysql, sql
import EditorWorker from "monaco-editor/editor/editor.worker?worker";
import { loader } from "@monaco-editor/react";
self.MonacoEnvironment = { getWorker: () => new EditorWorker() };
loader.config({ monaco });                                      // never the CDN default
```

Only the editor worker is needed (no TS/JSON/CSS language services). Vite emits it and the
codicon font as same-origin files; no `blob:` or `data:` script source is needed. Glide needs
its stylesheet `@glideapps/glide-data-grid/dist/index.css`.

## 10. Exit-check tests the streams must write

Live tests are `#[ignore]`d and run in the conformance CI job against the Docker fixtures.

- **sql**: unit tests for every rule in the crate docs (each dialect: quotes, comments,
  dollar quotes, `E''`, nested comments, `BEGIN ATOMIC`, `DELIMITER`, compound bodies, SQLite
  trigger bodies, `GO`), ranges/positions with multi-byte text, comment-only segments,
  unterminated input, `statement_at` at the boundaries of §4; `redact_secrets` for each keyword
  and connection strings, plus "nothing to redact" returns `Cow::Borrowed`; a 1 MB script splits
  in well under 100 ms.
- **pg / mysql / sqlite**: live tests that list every `[m2.<engine>].objects` entry under its
  parent and `load` it with the asserted shape of §8 (columns, PK, FK actions, check, index,
  comment, trigger timing/events, routine args/returns, sequence numbers); routine overloads on
  PostgreSQL get distinct signatures; `system` objects are flagged; a pre-cancelled token returns
  `Cancelled`; a missing object is `NotFound`; `missing_layout_fetchers` is empty; PostgreSQL
  lists `oxbase_m2_other`; SQLite lists `main` and the attached `m2aux`. A unit test (or a SQL
  text review) shows no fetcher builds SQL from names.
- **store**: migrations apply once and are idempotent on reopen (tempdir); a newer recorded
  version opens; history round trip, search with `%`/`_` literal, `before` paging, delete, clear
  per profile, prune, settings persistence.
- **services**: with `MockDriver`: `open_child` reuses params and tunnel, is hidden from
  `list_open`, closes with its base; `TreeService` folder/direct groups, `show_system`,
  unsupported kinds hidden, metadata child per other database on `Session` scope; query run event
  order for multi-statement scripts and multiple result sets; `max_rows` truncation; a result
  larger than a small memory budget spills to files and pages correctly *while* the stream
  continues; disk cap truncates; cancel ends with `cancelled` and keeps rows; `close` and drop
  delete the run directory; one-run-per-editor; history recorded per statement (redacted).
  Live (conformance job): on each engine, a **1M-row** query (`[m2] large_result_rows`) streams
  into a store whose memory budget forces spilling, random pages across the range are correct,
  memory stays within budget; cancel mid-stream ends within 2 s with rows pageable. Conformance:
  `fetch_list` cells pass for the M2 kinds of PostgreSQL, MySQL, MariaDB and SQLite.
- **app**: Rust tests for argument mapping of the commands that do not need a live session;
  UI tests (Vitest) for the tree panel, editor run/run-current/cancel, grid paging requests
  (only visible ranges are fetched), and the history panel (search, delete, clear, off switch).
