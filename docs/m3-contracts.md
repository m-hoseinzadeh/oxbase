# M3 contracts

Status: frozen for M3 on 2026-10-31 (Design step); deviations accepted at integration are marked
**As built**. M3 exit check (PLAN.md §6): **round-trip
conformance passes for table, column, index, FK and check** on PostgreSQL, MySQL, MariaDB and
SQLite. Scope: grid editing (`DmlWriter`), filter and sort builder, form / JSON views, and the
table designer with DDL preview for the three SQL engines (MySQL with MariaDB as a flavour).
SQL Server is M4.

Changes to these contracts go through the Design owner. Streams code against them and must not
change public signatures; adding private helpers, private fields and tests is always fine. Rust
doc comments on the items named here are part of the contract and are more detailed than this
file (start with the module docs of `oxbase_types::script`, `oxbase_types::filter`,
`oxbase_dialect::{sql, compare, dml}`, `oxbase_designer`, `oxbase_services::{edit, designer,
convert}`).

## 1. Who owns what

Bodies marked `unimplemented!("m3: <stream>")` belong to that stream. No test may call one until
its stream has filled it. Seven streams start from the Design head in parallel.

| Area | Path | Owner | State at hand-off |
|---|---|---|---|
| `Script` / `ScriptStep` (phases, captures, checks), `BoundStatement`, `Filter` / `SortKey` / `TableDataQuery`, `Rename`, `props::CONSTRAINT`, `RowKey::validate` (empty insert allowed) | `crates/oxbase-types` | Design | done, tested |
| `Quoting` M3 hooks, `sql` (`SqlWriter`, `check_expression`, filter / sort / table-data SELECT), `compare` (`CompareRules`, `RulesComparator`, `diff_*`), `DdlContext`, `DmlTarget` / `DmlPlan` / key helpers, `DesignerCapabilities` / `Support`, new trait signatures, `DESIGNER_KINDS`, `missing_designer_roles` | `crates/oxbase-dialect` | Design | done, tested |
| `TableDesign` model, `from_def` / `to_def` / `renames` / `apply_renames`, payload types | `crates/oxbase-designer` | Design | done, tested |
| `validate`, `preview`, `render_script` | `crates/oxbase-designer` | **designer** | skeleton |
| Conformance `roundtrip` cells (and `OXBASE_CONFORMANCE_REQUIRE_M3_ROUNDTRIP`, the one-line env addition in the conformance CI step) | `crates/oxbase-conformance`, that line of `.github/workflows/ci.yml` | **designer** | - |
| PostgreSQL cells: `DdlGenerator` + `RulesComparator(PgRules)` for the 5 kinds, `DmlWriter`, `PgCapabilities::designer`; `tests/docker/init/postgres/05-m3.sql` | `crates/oxbase-pg`, its fixture | **pg** | - |
| MySQL + MariaDB cells (same list, both engines), driver `CLIENT_FOUND_ROWS`; `tests/docker/init/{mysql,mariadb}/05-m3.sql` | `crates/oxbase-mysql`, its fixtures | **mysql** | - |
| SQLite cells (same list) including the table rebuild; `tests/docker/init/sqlite/02-m3.sql` | `crates/oxbase-sqlite`, its fixture | **sqlite** | - |
| `EditService`, `DesignerService` bodies, `convert`, `QueryService::run_bound` / `run_session`, `execute_script` | `crates/oxbase-services` | **services** | skeleton |
| `EditService::new`, `DesignerService::new` / `capabilities`, `PendingChanges` helpers, payload types, `InputKind::for_type`, `QueryService::run_state` | `crates/oxbase-services` | Design | done, tested |
| Commands `table_data_open`, `edit_begin`, `edit_apply`, `row_get_form`, `designer_load`, `designer_preview`, `designer_apply`, `capabilities_get` (registration, `build.rs`, capabilities, state) | `crates/app-tauri` | Design (contract) | forwarding bodies done |
| UI: filter / sort builder, editable grid, form and JSON views, table designer, DDL preview | `ui/src` except `ui/src/api` | **app** | - |
| TS payload types, wrappers, pending-changes helpers | `ui/src/api/**` | Design | done, tested |
| Webview smoke test job and script (§9) | new job in `.github/workflows/ci.yml`, `ui/e2e/**`, `scripts/e2e-*.sh` | **e2e** | - |
| Dependencies and lockfiles | root `Cargo.toml`, `Cargo.lock`, `ui/package.json`, `ui/pnpm-lock.yaml` | Design | done (§11) |

The app stream may add `data-testid` attributes and the perf hook of §9 anywhere in `ui/src`;
the e2e stream only reads them. A stream that needs a new third-party crate adds it with an
explicit version to its own crate and reports it for hoisting.

## 2. Core types (`oxbase-types`)

- **`Script { steps: Vec<ScriptStep>, warnings }`** (was `statements: Vec<Statement>`).
  `ScriptStep { sql, description, destructive, transactional (default true), phase:
  prepare | main | finally, check: none | expect_no_rows, capture?, only_if? }` with builders
  (`ScriptStep::new(sql).described(..).destructive().non_transactional().in_phase(..)
  .expect_no_rows().capture(..).only_if(..)`). `Script::ordered()` is execution order;
  `Script::is_atomic()` = every main step transactional. Execution semantics (prepare outside
  the transaction, main in one transaction when atomic, finally always; captures and
  conditions) are in the `oxbase_types::script` module docs and are what
  `oxbase_services::designer::execute_script` implements.
- **`BoundStatement { sql, params: Vec<Value> }`**: SQL with the engine's placeholders and the
  bound values. Grid DML and table-data queries only travel in this form.
- **`Filter`** (`and` / `or` / `not` / `condition {column, op, values}` / `rawSql {sql}`),
  **`FilterOp`** (`eq ne lt le gt ge like notLike contains notContains startsWith endsWith in
  notIn isNull isNotNull between notBetween`), **`SortKey {column, descending, nulls?}`**,
  **`TableDataQuery {table, filter?, sort, limit?}`**. camelCase, `type`-tagged. Limits:
  depth 32, 1000 conditions, 1000 `in` values; no NULL values in comparisons; text values for
  pattern operators. Semantics in the module docs (empty groups add nothing, `ne` does not
  match NULLs, pattern operators escape `%`/`_`, raw SQL is checked lexically).
- **`Rename {kind, old, new}`**: designer renames (table, column, index, FK, check).
- `RowKey::validate` now accepts an insert with no values (a row of defaults).
- `props::CONSTRAINT` (`"constraint"`): an index is a declared `UNIQUE` constraint.

## 3. Dialect roles (`oxbase-dialect`)

### 3.1 Registration and resolution

Each engine's existing `dialect::register` additionally registers (M3), for its engine(s):

1. a `DdlGenerator` and `RulesComparator(<Engine>Rules)` for every kind of `DESIGNER_KINDS`
   (`Table`, `Column`, `Index`, `ForeignKey`, `Check`);
2. its `DmlWriter` (`register_dml_writer`);
3. `Capabilities::designer(&ServerVersion) -> DesignerCapabilities` in its existing
   capabilities.

Test: `reg.missing_designer_roles(engine).is_empty()` for every M3 engine (and
`oxbase_services::default_dialects()` must keep it empty; the services stream adds that
assertion). Services resolve `dialects.ddl(engine, Table)`, `comparator(engine, Table)`,
`quoting(engine)`, `capabilities(engine)` (designer) and `dml_writer(engine)` (edits); a missing
role is `Unsupported`, shown by the UI as read-only. No engine special case lives outside the
engine crates.

### 3.2 `Quoting` hooks (all with defaults)

| Hook | Default | PostgreSQL (`PgQuoting`) | MySQL/MariaDB (`BacktickQuoting`) | SQLite (`AnsiQuoting`) |
|---|---|---|---|---|
| `placeholder(n)` | `?` | `$n` | `?` | `?` |
| `text_cast(expr)` | `CAST(expr AS TEXT)` | same | `CAST(expr AS CHAR)` | same |
| `nulls_ordering()` | true | true | false (emulated) | true |
| `like_specials()` | `%_` | same | same | same |
| `limit_clause(n)` | `LIMIT n` | same | same | same |
| `lex_rules()` | none | dollar quotes, `E''` | backslash escapes, `#` comments | none |
| `qualify(obj)` | catalog.schema.name | **no catalog** (one database per session) | catalog.name | catalog.name |

### 3.3 `DdlGenerator`

```rust
fn create(&self, d: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script>;
fn alter(&self, old: &ObjectDef, new: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script>;
fn drop_object(&self, r: &ObjectRef, ctx: &DdlContext<'_>) -> Result<Script>;
// DdlContext { version, table: Option<&TableDef>, triggers: &[TriggerDef], renames: &[Rename] }
```

Rules (also on the trait): identifiers through `Quoting`; comments and literal defaults through
`Quoting::quote_str`; expressions from the model verbatim (user-typed ones were checked with
`sql::check_expression` by the designer); one step per statement with a description;
`destructive` for drops, narrowing type changes and rebuilds; `transactional = false` for
statements that commit implicitly. Order of an `alter`: renames; drops of changed / removed FKs,
checks and indexes; column drops, changes, adds; primary key; new / changed indexes, checks, FKs;
comments. Empty script when nothing changed. Unsupported things are `Error::Unsupported` naming
them, never silently skipped (informational properties may be skipped with a warning).

Per engine:

- **PostgreSQL**: everything native and transactional (`ALTER TABLE ... ADD / DROP / RENAME
  COLUMN`, `ALTER COLUMN ... TYPE ... USING col::type`, `SET / DROP NOT NULL`, `SET / DROP
  DEFAULT`, `ADD / DROP CONSTRAINT`, `CREATE / DROP INDEX`, `COMMENT ON`), identity (10+),
  stored generated columns (12+), `INCLUDE` (11+). A type change that may fail or truncate is
  destructive. **As built:** sessions start with `standard_conforming_strings=on`, and literals that contain
  a backslash are written as `E'...'`, so a script means the same with the setting off. Index
  operator classes, key collations, storage parameters, tablespaces and `DEFERRABLE` unique
  constraints are fetched as index extras and kept when an index is recreated.
- **MySQL / MariaDB**: every step `transactional = false` (implicit commit); column changes with
  `MODIFY` / `CHANGE` carrying the full column definition (type, nullability, default,
  auto-increment, comment, collation); `RENAME COLUMN` on MySQL 8.0+ / MariaDB 10.5+, else
  `CHANGE`; `AFTER` / `FIRST` for positions; checks enforced on MySQL 8.0.16+ and MariaDB 10.2+
  (else `checks_enforced = false` and a warning); no `SET DEFAULT` FK action on InnoDB. Dropping
  an FK's index needs the FK gone first. **As built:** when a check, generated column, expression
  default or expression key uses a renamed column, MySQL refuses a separate `RENAME COLUMN`, so
  every column rename becomes `CHANGE COLUMN old new <definition>` in the main `ALTER`, after
  the drops of changed checks; a new expression that still names a renamed column's old name
  is `InvalidInput`. Character set, collation, `ON UPDATE` and `INVISIBLE` absent from the new
  definition are kept; an explicit `Value::Null` extra (`INVISIBLE: false`) removes them.
- **SQLite**: native `ALTER TABLE ... RENAME TO`, `RENAME COLUMN` (3.25+), `ADD COLUMN` (only a
  column SQLite accepts there: no PK / UNIQUE, constant default, NOT NULL needs a default),
  `DROP COLUMN` (3.35+, only a plain column: not PK, unique, indexed, in an FK, check, view,
  trigger or generated column), `CREATE / DROP INDEX`. Everything else is the **table rebuild**
  (§6.4). No comments (`table_comment` / `column_comment` unsupported).

### 3.4 `Comparator` and `compare`

**As built:** PostgreSQL registers `PgComparator`, which first fills in PostgreSQL's defaults
for flags the server always reports (`not_valid`, `match_type`, `nulls_not_distinct`,
`include_columns`, ...) on both sides (`oxbase_pg::dialect::normalize_def`) and then runs the same
shared `diff_objects` walk with `PgRules`, so ids and order are unchanged. MySQL, MariaDB and
SQLite register `RulesComparator`.

Engines register `RulesComparator(<Engine>Rules)` and only override `CompareRules`
(`type_text`, `same_type`, `same_default`, `same_expression`, `same_table_ref`,
`compare_extra`) for what they spell their own way: type aliases (`int4` / `integer`,
`character varying(20)`, MySQL display widths), default spellings (`'x'::text`, MySQL quoting),
check text as the server rewrites it (`CHECK ((qty >= 0))`, `` (`qty` >= 0) ``), implicit
extras. The order and property ids of the result are fixed by `oxbase_dialect::compare`
(module docs: table props, removed columns, columns in new order, positions, primary key,
indexes, foreign keys, checks; FKs and checks matched by name, else by content). **Round-trip
invariant:** the definition fetched right after running `create` / `alter` must compare equal
(empty diff) to the definition the script was generated from.

### 3.5 `Capabilities::designer`

`DesignerCapabilities` (camelCase JSON) says per engine and version what the designer offers:
`Support::{native, rebuild, unsupported}` per operation (rename table, add / drop / rename
column, type, nullability, default, comments, collation, reorder, primary key, indexes, FKs,
checks), flags (`transactionalDdl`, identity, generated columns, partial / expression /
descending indexes, prefix lengths, `checksEnforced`, deferrable), lists (`indexMethods`,
`fkActions`), `maxIdentifierLength`, and human `notes`. `Default` is read-only. The generator
is the authority: anything `unsupported` here fails there with `Unsupported`.

### 3.6 `DmlWriter`

```rust
fn plan(&self, edit: &RowEdit, target: &DmlTarget<'_>) -> Result<DmlPlan>;
// DmlTarget { table: &TableDef, key: &RowKey, version, read_back: &[String] }
// DmlPlan   { write: BoundStatement, returning: bool, read_back: Option<BoundStatement> }
```

- Every value bound (`SqlWriter::param`); identifiers quoted; `UPDATE` / `DELETE` match by key
  only (`write_key_predicate`); generated and `GENERATED ALWAYS` identity columns never written
  (`InvalidInput`); an insert with no values = defaults (`DEFAULT VALUES`, MySQL `() VALUES
  ()`).
- **Read-back strategy:** PostgreSQL and SQLite: `RETURNING <read_back>` on insert / update
  (`returning = true`). MariaDB 10.5+: `INSERT ... RETURNING`, update via read-back query.
  MySQL: read-back query after the write; for a new row whose auto-increment key was not given,
  `WHERE <auto_inc_col> = LAST_INSERT_ID()`. An update that changes key values reads back by the
  new values. `read_back = None` when the row cannot be found again.
- Values the driver cannot bind for the column's type (PostgreSQL arrays, enums, intervals,
  domains, `Other` types) arrive as `Value::Text`; the writer casts the placeholder
  (PostgreSQL `CAST($n::text AS <native_type>)`).
- **Optimistic concurrency** (services enforce): one transaction per apply; affected rows =
  rows returned when `returning`, else the summed `RowsAffected`; anything but 1 is a
  conflict and rolls everything back. The **mysql** stream sets `CLIENT_FOUND_ROWS` on every
  connection so an `UPDATE` reports matched rows (a no-op update counts 1), with a live test.

## 4. Filter, sort and table data (`oxbase_dialect::sql`)

`select_table_data(quoting, &TableDataQuery, Option<&TableDef>) -> Result<BoundStatement>`:
`SELECT * FROM <qualified> [WHERE <filter>] [ORDER BY <sort>] [LIMIT n]`. Conditions render as
`col <op> ?`, `col [NOT] LIKE ? ESCAPE '!'` (literal matches: `%`, `_`, `!` escaped with `!`;
pattern operators on known non-text columns compare `text_cast(col)`), `col [NOT] IN (?, ...)`,
`(col [NOT] BETWEEN ? AND ?)`, `col IS [NOT] NULL`; groups parenthesized; `rawSql` as `(sql)`
after `check_expression` (single expression: no `;`, no comments, terminated strings, balanced
parentheses, engine lexing). `NULLS FIRST/LAST` native or emulated with `CASE WHEN col IS NULL
...`. With the table definition, unknown columns are `InvalidInput`. Implemented and tested.

`EditService::open_table_data` (services) loads the definition (`TreeService::load`), builds the
statement and starts it with `QueryService::run_bound` on the UI's editor id, with
`RunOptions::database = table.catalog` and `record_history = false`. Paging, events, cancel and
close are the M2 run path (`query_page` etc.). The UI sends filter values as `{type: "text"}`
as typed; services may convert them with the column type (`convert::parse_input`) before
binding.

## 5. Grid editing (`oxbase_services::edit`, `convert`)

- **Editable results:** only runs started by `open_table_data` (M3). `edit_begin` returns
  `Editability {runId, resultIndex, table, editable, readOnlyReason?, key?, columns:
  EditColumn[]}`. Key: the primary key, else the first unique index / constraint whose columns
  are all NOT NULL plain columns and which has no predicate; every key column must be in the
  result. Read-only reasons are the constants of `edit::reasons` (not table data, no key, view,
  key not in result, engine). Columns: generated and `GENERATED ALWAYS` identity columns are
  read-only cells with a reason; `InputKind` from the column type.
- **Pending changes** (UI buffer, sent whole): `PendingChanges {edits: [update {row, cells} |
  insert {cells} | delete {row}]}`, `CellEdit {column, input: null | default | text {text}}`.
  At most one update or delete per row; a cell edit replaces that column's input; deleting a
  row drops its update; a deleted row cannot be edited (`PendingChanges` helpers in Rust,
  `ui/src/api/pending.ts` in TS: same rules, tested on both sides).
- **Apply** (`edit_apply`): validate everything first without touching the database (row
  exists, not deleted, column editable, input parses, NOT NULL, key known); plan each edit with
  the `DmlWriter`; run on the run's own session (`QueryService::run_session` +
  `SessionHandle::exclusive`) inside **one transaction**, in buffer order; affected ≠ 1 is
  `conflict`; first failure or conflict rolls back everything (that edit `failed` /
  `conflict`, earlier ones `rolledBack`, later ones `notRun`); after commit every edit is
  `applied` with its row read back as edit texts. Refused while the run is still running.
  Applied values overlay the immutable result store (later edits, forms, keys use them);
  inserted rows get row numbers after the stored rows; deleted rows refuse later edits. State
  is dropped when the run is closed.
- **Values** (`convert`, grammar table in its module docs): the grid edits text; NULL is
  `CellInput::Null`, an empty text is an empty string (never NULL); booleans `true/false`
  (`t/f`, `1/0`, `yes/no` read); integers range-checked for their width; decimals exact text;
  floats incl. `NaN` / `±Infinity`; dates `YYYY-MM-DD`; times `HH:MM:SS[.ffffff]`; timestamps
  `YYYY-MM-DD HH:MM:SS[.ffffff]` (`T` accepted); timestamptz RFC 3339 (no offset = UTC); uuid;
  JSON validated; bytes `0x` hex (also `\x` hex and `base64:` read); enums by label; anything
  else as text cast by the writer.
- **Form / JSON view** (`row_get_form`): `FormRow {row, fields: [{column, text?}], json}` for any
  run (read-only too), including overlay values; `NotFound` for deleted or unknown rows.
- **Secrets:** cell values are user data, not secrets, but they are never logged; history
  records nothing for table-data runs (`record_history = false`) and never parameter values.

## 6. Table designer (`oxbase-designer`, `oxbase_services::designer`)

### 6.1 Model

`TableDesign {original?: TableDef, table: ObjectRef, columns: DesignColumn[], primaryKey?,
indexes / foreignKeys / checks: DesignItem<T>[], comment?, extras}`; every item has `origin?`
(index in the original's list; absent = new). `DesignColumn.defaultLiteral` is a plain value
the dialect quotes (`Quoting::quote_str`), `column.default` an engine-native expression.
`TableDesign::from_def`, `new_table`, `renames()` (from origins), `to_def(quoting)` and
`apply_renames` are implemented and tested. The UI sends `original` back unchanged.

### 6.2 Validation (`oxbase_designer::validate`, designer stream)

Issues `{severity: error | warning, path, message}` in design order. Errors: empty or
over-long names (`maxIdentifierLength`), duplicate column / index / constraint names (engine
case rules), no columns, PK / index / FK columns that do not exist, duplicate origins or origins
out of range, FK column count ≠ referenced count, FK action not in `fkActions`, identity /
auto-increment on a non-integer column, any user expression (default, check, generated, index
predicate, expression key) failing `sql::check_expression`, any change whose capability is
`unsupported`. **As built:** names are checked in two groups (indexes: primary key + indexes;
constraints: primary key, unique constraints, FKs, checks), so MySQL's FK-backing index may
share its FK's name; only names the design gives (new or renamed) are checked, and names that
differ only in case are errors on every engine (no per-engine case flag yet). Warnings:
unenforced checks, rebuilds (data is copied), a NOT NULL column added
without a default to a table that may have rows, a type change that may truncate.

### 6.3 Preview and apply

`DesignPreview {diffs, script, sql, atomic, destructive, issues, notes}`: diffs from the
`Comparator` between `apply_renames(original)` and `to_def`, prefixed by one `name` diff per
rename (a new table: one `added` diff); script from `create` or `alter(original, new, ctx)`
with `DdlContext {table: original, triggers, renames}`; `atomic = transactionalDdl &&
script.is_atomic()`; when not atomic the notes say a failure leaves earlier statements applied
(MySQL / MariaDB); `sql = render_script(..)` with step descriptions as comments and `BEGIN` /
`COMMIT` when atomic. Error issues: empty script.

`DesignerService::apply` (services): refuse with `InvalidInput` on error issues, on a
destructive script without `confirmDestructive`, on an empty script, or when the table on the
server differs from `original` (comparator diff not empty: "the table changed since it was
loaded; reload it"); then `execute_script` on a **dedicated child session** of the table's
catalog (closed afterwards; base session only for private in-memory SQLite), and reload into a
new `TableDesign` (also after a failed non-atomic script). Report: `ScriptOutcome {committed,
atomic, steps: [{index, status: done | failed | skipped | rolledBack | notRun, error?,
elapsedMs}], warnings}` + `design?`.

### 6.4 SQLite table rebuild (sqlite stream)

The documented 12-step procedure as one script (all main steps destructive):

```text
prepare  PRAGMA foreign_keys                           capture "fk"
prepare  PRAGMA foreign_keys = OFF                      (no-op inside a transaction, so outside)
main     CREATE TABLE "<schema>"."<tmp>" (new definition)    tmp = "<name>__oxbase_new"
main     INSERT INTO "<tmp>" (<kept columns>) SELECT <kept / renamed columns> FROM "<name>"
main     DROP TABLE "<name>"
main     ALTER TABLE "<tmp>" RENAME TO "<name>"
main     CREATE INDEX ... (every index of the new definition)
main     CREATE TRIGGER ... (every trigger of DdlContext::triggers, from its definition)
main     PRAGMA foreign_key_check("<name>")             expect no rows, only if fk = "1"
finally  PRAGMA foreign_keys = ON                       only if fk = "1"
```

Rules: the main steps run in one transaction (SQLite DDL is transactional), so any failure
leaves the original table untouched; foreign keys are off during the copy so `DROP TABLE` does
not cascade; enforcement is restored only if it was on; views that reference the table are not
dropped (SQLite does not drop them with the table), and the stream must prove with a live test
that a view on the table still works after a rebuild (use `PRAGMA legacy_alter_table` with the
same capture / restore pattern if the rename needs it). New columns get their default (or
NULL); dropped columns are not copied; a type change copies with `CAST`.

**As built:** values are copied as they are (`INSERT ... SELECT "col"`, never `CAST`: a cast to a
NUMERIC-affinity type such as `DATE` or `DECIMAL` loses data); the new column's affinity converts
only what it can without loss, and a `STRICT` table rejects the rest (the script rolls back). The
new table is created under the current names; renames run natively after the rebuild
(`legacy_alter_table = OFF`), so SQLite itself rewrites triggers, views and child foreign keys
(before 3.25 a column rename is copied and refused when a trigger uses it). A first main step
fails when the table has a trigger missing from `DdlContext::triggers`. The foreign-key check
covers the whole database filtered to rows where the table is child or parent
(`pragma_foreign_key_check(NULL, '<db>')`, 3.16+; the per-table pragma before). Clauses the
model cannot hold (`ON CONFLICT`, `COLLATE` on key columns) are recorded in the `REBUILD_LOSES`
extra and make a rebuild `Unsupported`. Swapping two column names is `Unsupported`.

## 7. Tauri commands and TS (`app-tauri`, `ui/src/api`)

camelCase payloads; core models inside them keep snake_case (as in M2). Errors are
`CommandError {category, message}`. `requestId` values are cancelled with `cancel_request`.
TS types in `ui/src/api/types.ts`, wrappers in `data.ts` / `designer.ts`, buffer helpers in
`pending.ts`, tests in `m3.test.ts`.

| Command | Arguments | Returns |
|---|---|---|
| `table_data_open` | `sessionId`, `editorId`, `request: TableDataQuery`, `options?: RunOptions`, `onEvent: Channel<RunEvent>`, `requestId?` | `RunId` (events and paging as `query_run`) |
| `edit_begin` | `runId`, `requestId?` | `Editability` |
| `edit_apply` | `runId`, `changes: PendingChanges`, `requestId?` | `ApplyReport {committed, outcomes: EditOutcome[]}` |
| `row_get_form` | `runId`, `resultIndex`, `row` | `FormRow` |
| `designer_load` | `sessionId`, `table: ObjectRef`, `requestId?` | `TableDesign` |
| `designer_preview` | `sessionId`, `design: TableDesign`, `requestId?` | `DesignPreview` |
| `designer_apply` | `sessionId`, `design`, `options?: {confirmDestructive}`, `requestId?` | `DesignApplyReport` |
| `capabilities_get` | `sessionId` | `EngineCapabilities {engine, serverVersion, designer, gridEditing, nullsOrdering}` |

There is no `edit_discard`: the buffer lives in the UI. New tables start from
`newTableDesign(ref)` in TS. Every command is in `generate_handler!`, `build.rs` and
`capabilities/default.json`; a core panic becomes an `internal` error (`isolated`).

## 8. Fixtures (`tests/fixtures.toml` `[m3]`)

Round-trip scratch tables `m3_rt_<kind>_<token>` (token = 10 base36 characters from time and
pid; unique per run), created and dropped by each cell in a guard that also runs on failure;
leftovers older than `stale_after_secs` are dropped at start. Editable seed tables per engine
(shapes in the file): `m3_edit_pk` (generated integer PK, unique `code`, text, decimal,
boolean, date, timestamp, JSON, bytes, stored generated `total`; seed rows 1-3),
`m3_edit_uq` (no PK, NOT NULL unique `code`), `m3_edit_nokey` (no usable key: read-only),
`m3_edit_view` (read-only). PostgreSQL objects in schema `m3`, MySQL / MariaDB in
`oxbase_test`, SQLite in a fresh temp file. Live edit tests never change seed rows 1-3.

## 9. Webview smoke test (e2e stream)

Closes the M2 gap (PLAN.md, *M2 as built*): nothing launched the built app.

- **Job** `Webview smoke (Linux)` in `ci.yml`: runs on the **full lane** (pull requests,
  `milestone/*` PRs, pushes to main, schedule, manual) and on the fast lane only for branches
  `wip/*-e2e` and `wip/*-app`. Linux, `ubuntu-latest`, `timeout-minutes: 40`. Windows
  (`msedgedriver` matching the runner's WebView2) is an optional full-lane job with
  `continue-on-error: true` in M3. macOS is skipped: WKWebView has no WebDriver.
- **Build:** `pnpm install --frozen-lockfile && pnpm run build` in `ui/`, then a debug app that
  serves `ui/dist` (not the dev server): `cargo build -p app-tauri --locked --features
  tauri/custom-protocol` (what `tauri build --debug --no-bundle` does).
- **Driver:** `tauri-driver` (installed with `cargo install tauri-driver --locked --version
  <pinned>`, license checked) in front of `WebKitWebDriver` (apt `webkit2gtk-driver`), under
  `xvfb-run`. The client is a dependency-free Node script (`ui/e2e/smoke.mjs`) speaking W3C
  WebDriver over `fetch` to `127.0.0.1:4444`; no new npm package.
- **Database:** the PostgreSQL fixture only (`scripts/ci-fixtures.sh up` or a postgres-only
  variant), plain TCP. The app runs with `OXBASE_DATA_DIR` and `XDG_CONFIG_HOME` pointing at a
  temp dir holding a `connections.json` profile whose password is **prompt** (no keychain in
  the job); the script types the fixture password into the prompt. No test-only backdoors in
  the app.
- **Hooks (app stream provides):** `data-testid` on the connection list, connect button,
  password prompt, editor, run / cancel buttons, result grid, row counter and status bar; and
  `window.__oxbasePerf.pages: {offset, limit, ipcMs, decodeMs}[]`, filled by the grid's page
  fetcher (cheap, always on).
- **Asserts:** the window loads and the status bar shows `app_info`'s version; connect works;
  `SELECT g, md5(g::text) FROM generate_series(1, 1000000) g` runs, `Channel` events arrive
  (the row counter grows to 1 000 000 and the result is done); `query_page` bytes decode in the
  webview (the first cell shows `1`); scrolling to the end and back loads pages; page latency
  through IPC + decode is recorded and p95 reported in the job summary (fail above 250 ms);
  cancel of a running large query ends with "cancelled" within 2 s. When the M3 UI lands:
  open `m3.m3_edit_pk` table data, edit a cell, apply, see it after reopen.
- **Artifacts:** screenshot and the WebDriver log on failure, `e2e-perf.json` always.

**As built:** the edit step is mandatory from M3 (`OXBASE_E2E_REQUIRE_EDIT=1` in CI; a skip
fails). Hooks it uses beyond the list above: `section.table-data` with `aria-label="Table
<schema>.<name>"`, `apply-edits`, `table-data-status` (contains `pending` while edits are
pending), `apply-errors`, `read-only-banner`, `cell-editor` with `.value-input`, the tab's
**Refresh** button, the Grid / Form tabs (`role=tab`), and double-click on a tree table opening
its data tab. The job also runs on the fast lane of `wip/*-integrate`. No Windows job: the
PostgreSQL fixture is a Linux container that Windows runners cannot start (waived for M3).
`tauri-driver` 2.1.0 and `WebKitWebDriver` have no verbose log, so failures upload
`failure.png` and `failure-dom.html` instead.

## 10. Invariants every stream keeps

1. **Quoting:** every identifier through the engine's `Quoting`; no hand-made quotes.
2. **Bound values:** DML and table-data queries bind every value (`BoundStatement`); no value is
   ever formatted into SQL. DDL literals (comments, literal defaults) go through
   `quote_str`, with tests for quotes, backslashes, unicode and reserved words.
3. **User SQL is one expression:** raw filters and designer expressions pass
   `check_expression`; the preview shows exactly what runs.
4. **Optimistic concurrency:** each update / delete affects exactly one row or the whole apply
   rolls back.
5. **Transactional apply:** grid edits always in one transaction; DDL in one transaction when
   the engine allows (PostgreSQL, SQLite), else step by step with the preview saying so.
6. **Destructive changes** are flagged per step and need `confirmDestructive`.
7. **SQLite rebuild safety:** §6.4; never leaves foreign keys off.
8. **Read-only reasons** are explicit; only table data with a key is editable.
9. **Secrets** never appear in Debug / Display, logs, errors, panics, history or Tauri
   responses; table-data runs record no history.
10. **Core never depends on Tauri;** no engine special cases outside engine crates.

## 11. Dependencies

No new third-party crate or npm package in the Design step. Rust: `serde` and `serde_json` became
normal dependencies of `oxbase-dialect`; `oxbase-designer` depends on `oxbase-types`,
`oxbase-dialect`, `serde`; `oxbase-services` and `app-tauri` depend on `oxbase-designer`. A stream
that needs a crate (for example `base64` for the bytes grammar) adds it to its own crate with an
explicit version, checks the license against `deny.toml` and reports it for hoisting.

## 12. Exit-check tests the streams must write

Live tests are `#[ignore]`d and run in the conformance CI job against the Docker fixtures.

- **pg / mysql / sqlite** (each, for each of its engines): `missing_designer_roles` empty;
  generator unit tests per kind (create / alter / drop, quoting of reserved words, quotes,
  backslashes and unicode in names, comments and literal defaults, order of an alter,
  destructive and transactional flags, unsupported → `Unsupported`); `CompareRules` tests
  (aliases, defaults, check rewrites); `DmlWriter` tests (bound values only, key predicate,
  RETURNING / read-back per version, generated columns refused, defaults-only insert);
  `designer()` per version. Live: create → fetch → compare-empty → alter → fetch →
  compare-empty → drop for each of the 5 kinds on the `[m3]` scratch names; DML plans run
  against `m3_edit_*` with exactly-one-row counts (mysql: a no-op update counts 1). sqlite:
  rebuild keeps rows, indexes, triggers and a dependent view, keeps / restores
  `foreign_keys`, and rolls back completely when `foreign_key_check` finds a violation.
- **designer:** `validate` cases of §6.2; `preview` with toy generators (renames reported as
  `name` diffs, not drop + add; atomic / destructive flags; notes for non-atomic engines);
  `render_script`. Conformance: `roundtrip` cells for Table, Column, Index, ForeignKey, Check
  on postgres, mysql, mariadb, sqlite pass; `OXBASE_CONFORMANCE_REQUIRE_M3_ROUNDTRIP=1` makes a
  pending or failing one fail the job; scratch tables are gone afterwards.
- **services:** with `MockDriver`: `run_bound` events / paging / history (no values recorded);
  `open_table_data` builds the bound SELECT; editability and every read-only reason; apply:
  validation-only failure touches nothing, conflict (0 and 2 rows) rolls back, per-edit
  statuses, overlay used by later edits and forms, refused while running; `convert` round trips
  for every grammar row; `execute_script` phases, captures, conditions, `expect_no_rows`,
  finally after failure, non-atomic stop. Live (each engine): open `m3_edit_pk` filtered and
  sorted; insert / update / delete through `EditService` with read-back (generated `total`,
  generated key); conflict when another session deleted the row; `m3_edit_uq` editable by
  `code`; `m3_edit_nokey` and `m3_edit_view` read-only; designer load / preview / apply / reload
  on a scratch table, conflict when the table changed in between.
- **app:** Rust argument-mapping tests stay green; Vitest for the filter / sort builder
  (produces valid `Filter` JSON, raw SQL clearly marked), editable grid (buffer rules, NULL vs
  empty, read-only reason shown, conflict display), form / JSON view, designer (origins kept,
  preview shown, destructive confirmation, non-atomic warning).
- **e2e:** the smoke job of §9 green on the full lane; latency numbers in the job summary.
