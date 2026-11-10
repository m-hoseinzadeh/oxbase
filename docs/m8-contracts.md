# M8 contracts

Status: frozen for M8 on 2026-11-03 (Design step). M8 exit check (PLAN.md §6): **sync between
PG↔PG, MySQL↔MySQL and MSSQL↔MSSQL produces correct scripts; transfer PG→MySQL works**, live in
CI. "Correct scripts" means, per engine pair: the generated structure-sync or data-sync script
runs without errors on the target, a fresh compare of source and target afterwards reports
zero differences for the applied items, and golden-script tests pin the generated SQL. Scope
also covers MariaDB↔MariaDB and SQLite↔SQLite sync, the transfer matrix across the five SQL
engines, and backup / restore on each of them.

Changes to these contracts go through the Design owner. Streams code against them and must not
change public signatures; adding private helpers, private fields, private modules and tests is
always fine. Rust doc comments on the items named here are part of the contract and are more
detailed than this file: start with the module docs of `oxbase_dialect::{typemap, deps, keyset,
compare_objects}`, `oxbase_types::relocate`, `oxbase_transfer::{spec, mapping, cells, convert,
pipeline}`, `oxbase_diff::{structure, script, retarget, data, store}`,
`oxbase_backup::{format, archive, backup, restore}` and `oxbase_services::{transfer, sync,
backup, runs}`.

## 1. Who owns what

Bodies marked `unimplemented!("m8: <stream>")` belong to that stream. No test may call one
until its stream has filled it. Six streams start from the Design head in parallel.

| Area | Path | Owner | State at hand-off |
|---|---|---|---|
| Dependencies (`sha2` 0.10.9 in `[workspace.dependencies]`; `arrow-ipc` feature `zstd` in oxbase-backup; no new crate in `Cargo.lock`), `deny.toml` (unchanged) | root | Design | done (§10) |
| `oxbase_types::relocate` (`Location`, `relocate_ref`, `relocate_def`), M8 `props` (`ARGS` ... `OWNED_BY`) | `crates/oxbase-types` | Design | done, tested |
| Roles and shared machinery: `TypeMapper` / `TypeMapping` / `Fidelity` (`typemap`), `DependencyGraph` / `table_dependencies` / `same_table` (`deps`), `write_key_after` / `write_key_at_most` / `KeyRange` / `ChunkHasher` / `inline_params` (`keyset`), `DefinitionComparator` / `diff_definitions` skeleton (`compare_objects`), `Quoting::script_terminator`, `BulkLoader::{foreign_key_checks, identity_resync}` (defaults), registry slots `register_type_mapper` / `type_mapper` / `register_chunk_hasher` / `chunk_hasher`, `SYNC_KINDS`, `M8_ENGINES`, `missing_m8_roles` | `crates/oxbase-dialect` | Design | done, tested; `diff_definitions` body: **structure-sync** |
| `oxbase-transfer` layout: `spec` (done, tested), `mapping` (types + `representative_types` done; `map_column`, `fidelity_between`, `target_table`, `pair_report` bodies), `cells/{postgres,mysql,sqlite,mssql}.rs` (`TypeMapper` bodies), `convert` (`BatchConverter` bodies), `pipeline` (`plan`, `copy_table` bodies), `register` (done) | `crates/oxbase-transfer/**` | **transfer** | skeleton |
| `oxbase-diff` layout: `structure` (`load_snapshot`, `compare` bodies; ids, counts, options done), `script` (`sync_script` body), `retarget` (`retarget_sql` body) | `crates/oxbase-diff/src/{structure,script,retarget}.rs` (+ private modules), `crates/oxbase-diff/tests/structure*.rs` | **structure-sync** | skeleton |
| `oxbase-diff` data: `data` (`resolve_key`, `row_key` done, tested; `compare_table`, `data_statements` bodies), `store` (`DiffStore::new` / `path` done; `append`, `finish`, `rows`, `read` bodies) | `crates/oxbase-diff/src/{data,store}.rs` (+ private modules), `crates/oxbase-diff/tests/data*.rs` | **data-sync** | skeleton |
| `oxbase-backup`: `format` (manifest, entry names, checksums, validation: done, tested), `archive` (`ArchiveWriter`, `ArchiveReader` bodies), `backup` (`backup` body), `restore` (`restore` body; options done) | `crates/oxbase-backup/**` | **backup** | skeleton |
| Services: `runs::RunRegistry` / `OperationId` (done, tested); `transfer::TransferService`, `sync::{StructureSyncService, DataSyncService}`, `backup::BackupService` (constructors, getters, cancel / `cancel_session` done; operation bodies by the streams below); module registration in `lib.rs`; the commented `oxbase_transfer::register` line in `dialects.rs` | `crates/oxbase-services` | Design | done (payloads unit-tested) |
| `TransferService` bodies (`plan`, `pair_report`, `start`), `crates/oxbase-services/src/transfer/**` (private), `crates/oxbase-services/tests/m8_transfer_live.rs`; uncomment `oxbase_transfer::register` in `dialects.rs` and add the `dialects.rs` test that every SQL engine has a type mapper; the `foreign_key_checks` / `identity_resync` overrides (only those two methods) in `crates/oxbase-{pg,mysql,sqlite,mssql}/src/dialect/bulk.rs` | as listed | **transfer** | skeleton |
| `StructureSyncService` impl block (bodies) in `crates/oxbase-services/src/sync.rs`, `crates/oxbase-services/src/sync/structure.rs` (private, new), `crates/oxbase-services/tests/m8_structure_live.rs`; **per-engine DDL cells** for `View`, `MatView`, `Sequence`, `Function`, `Procedure`, `Trigger`: new files `crates/oxbase-{pg,mysql,sqlite,mssql}/src/dialect/sync_ddl.rs` plus their registration lines (a `DdlGenerator` and a `DefinitionComparator(<engine rules>)` per kind that has a fetcher) in each engine's `dialect/mod.rs`, live tests `crates/oxbase-{pg,mysql,sqlite,mssql}/tests/m8_sync_ddl_live.rs` | as listed | **structure-sync** | - |
| `DataSyncService` impl block (bodies) in `crates/oxbase-services/src/sync.rs`, `crates/oxbase-services/src/sync/data.rs` (private, new), `crates/oxbase-services/tests/m8_data_live.rs`; optional `ChunkHasher` cells `crates/oxbase-<engine>/src/dialect/chunk_hash.rs` + one registration line each | as listed | **data-sync** | - |
| `BackupService` bodies in `crates/oxbase-services/src/backup.rs` (+ `src/backup/**` private), `crates/oxbase-services/tests/m8_backup_live.rs` | as listed | **backup** | skeleton |
| Commands of §8 (`src/sync.rs`), `SyncState`, `close_connection` cancelling M8 runs, `build.rs`, `capabilities/default.json`, `lib.rs` handler list | `crates/app-tauri` | Design (contract) | forwarding bodies done |
| TS payloads and wrappers | `ui/src/api/m8.ts` (+ `m8.test.ts`), `ui/src/api/index.ts` | Design | done, tested |
| UI: transfer wizard, structure sync view, data sync view, backup / restore dialogs (§8.4); webview smoke steps | `ui/src` except `ui/src/api/**`, `ui/e2e/**`, `scripts/e2e-*.sh`, the webview smoke job's env line (`OXBASE_E2E_REQUIRE_M8`) | **app** | - |
| Fixture seeds (§9), conformance cells (structure sync, data sync, transfer matrix, backup round trip) and the M8 guard; the conformance job lines | `tests/docker/**`, `scripts/{ci-fixtures,fixtures-up}.sh` (if needed), `crates/oxbase-conformance/**`, the conformance job of `.github/workflows/ci.yml` | **conformance** | - |
| `tests/fixtures.toml` `[m8]`, PLAN.md, README.md, this file | | Design | done |

**Merges.** `crates/oxbase-services/src/sync.rs` is shared by two streams: structure-sync edits
only the `impl StructureSyncService` block, data-sync only the `impl DataSyncService` block
(the payload types between them stay as they are), and each puts its code in its own private
submodule (`sync/structure.rs`, `sync/data.rs`, declared with one `mod` line each at the end of
the module list). Engine crates: each stream adds only its own new file and its own
registration lines (one blank line between them).

**CI lines.** Live tests of `oxbase-transfer`, `oxbase-diff` and `oxbase-backup` run only once
`-p oxbase-transfer -p oxbase-diff -p oxbase-backup` are in the conformance job's two cargo
lines (build, and "Run M1-M5 live tests"). The conformance stream adds them; the other streams
may add the same words on their own branches (a trivial merge) so their live tests run before
the conformance stream merges. The services live tests already run there.

**Integration** (after the streams): `missing_m8_roles` empty for the five SQL engines in a
`dialects.rs` test; set `OXBASE_CONFORMANCE_REQUIRE_M8=1`, `OXBASE_SERVICES_REQUIRE_M8=1` and
(webview job) `OXBASE_E2E_REQUIRE_M8=1`; README / PLAN as-built notes.

## 2. Decisions

| Question | Decision |
|---|---|
| Logical type model | `oxbase_types::DataType` (already the neutral model of fetchers, import, the grid). No second model. |
| Where type mapping lives | A per-engine `TypeMapper` role in `oxbase-dialect`, cells in `oxbase-transfer::cells` (one module per engine; MySQL and MariaDB share one with a flag), registered by `oxbase_transfer::register` from `default_dialects`. Engine crates stay unchanged for mapping. |
| Target DDL | The target's `DdlGenerator::create` with `DdlContext::exact_values` (as M5 import), fed a `TableDef` whose columns carry the mapped `DataType` and, where the mapper says, `native_type`. |
| Row path | Source `SELECT` (M3 `select_table_data` with bound filter values, or the user's query) → Arrow pages → `BatchConverter` → load batches → the target's M5 `BulkLoader`. One batch in memory per side. |
| MongoDB / Redis in transfer | **Not in M8** (both directions): a collection source needs a non-SQL read path and a document target a mapper; the exit check does not need them. `plan` answers `Unsupported` naming the engine. The M7 `DocumentInsert` loader makes a MongoDB target cheap later (one `TypeMapper` cell). |
| Sync engines | Same engine on both sides only (PG↔PG, MySQL↔MySQL, MariaDB↔MariaDB, MSSQL↔MSSQL, SQLite↔SQLite). Different servers or two containers of one server. |
| Structure sync engine work | Shared walk in `oxbase_dialect::compare_objects` + per-engine DDL cells for views, materialized views, sequences, routines and triggers (new `sync_ddl.rs` per engine); tables reuse the M3 cells (SQLite rebuild included). |
| Moving between containers | Model refs: `oxbase_types::relocate_def` (Design, done). SQL text: `oxbase_diff::retarget_sql` (token level). |
| Data compare | Keyset chunks on the source, the same key ranges on the target, in-memory match per chunk; optional per-engine digest skip (`ChunkHasher`). Tables without a key: **refused** unless the user names key columns (full-row compare cannot tell updates from delete + insert and breaks on duplicates). |
| Differences storage | `DiffStore`: Arrow IPC spill files in a private temp dir per compare; scripts and applies read them back (bounded memory). |
| DML | The engine's M3 `DmlWriter` (`RowEdit` per row, bound values); previews through `inline_params` (dump literals). |
| Backup format | One ZIP file (`.oxbackup`): `manifest.json`, `objects/NNNN.json` (definition), `objects/NNNN.sql` (DDL for people), `data/NNNN.arrows` (Arrow IPC stream in the load schema, zstd buffers), SHA-256 per entry. §7.1. |
| Backup vs SQL dump | Separate. The M5 SQL dump stays an export (portable text, any engine); backup is Oxbase's archive restorable into the same engine. Restore never parses SQL. |
| Restore structure | Built with the structure-sync machinery: archive definitions as the source snapshot, the target container's snapshot, `compare` + `sync_script`, run with `execute_script`. |
| Long runs and closing a connection | Every M8 operation registers in a `RunRegistry` with its sessions; `close_connection` calls `SyncState::cancel_session` before closing. |
| Server-held compare results | Compare results stay in the service by `OperationId` (at most 8 per service); scripts and applies name the id. |

## 3. Shared core

### 3.1 Type mapping (`oxbase_dialect::typemap`)

```rust
pub enum Fidelity { Exact, Widened, Narrowed, Unsupported }          // ordered best → worst
pub struct TypeMapping { data_type: DataType, native_type: String, fidelity: Fidelity, reason: Option<String> }
pub trait TypeMapper: Send + Sync {
    fn logical(&self, column: &ColumnDef, version: &ServerVersion) -> DataType;   // default: column.data_type
    fn map(&self, data_type: &DataType, version: &ServerVersion) -> TypeMapping; // total
    fn same_engine(&self, column: &ColumnDef, version: &ServerVersion) -> TypeMapping; // default: native type, exact
}
```

Rules: pure, total, a reason on every non-exact mapping, `native_type` empty unless the
generator's derived type would not match the fidelity. JSON camelCase (`dataType`,
`nativeType`, `fidelity`, `reason`).

### 3.2 Moving between containers (`oxbase_types::relocate`, `oxbase_diff::retarget_sql`)

`Location { catalog, schema }` (`of_container`: a `Database` ref is the catalog, a `Schema` ref
the schema in its catalog). `relocate_def(def, from, to)` moves the object's ref, a child's
owning table, FK targets, a trigger's function and a sequence's owner when they live in
`from` (schema equal, catalog equal or absent); refs elsewhere stay. `retarget_sql(engine,
text, from, to)` moves qualified names inside SQL text on the token level (`oxbase_sql`),
keeping quoting and everything else byte for byte.

### 3.3 Dependency order (`oxbase_dialect::deps`)

`DependencyGraph` (`add`, `depends_on`, `order() -> DependencyOrder { order, cycles }`):
dependencies first, nodes visited in insertion order, cycles emitted together and reported,
self-edges ignored, iterative (50 000-node chains tested). `table_dependencies(&[TableDef])`
builds the FK graph (`same_table`: name, schema / catalog where both sides set them).

### 3.4 Keysets and previews (`oxbase_dialect::keyset`)

`write_key_after` / `write_key_at_most` (lexicographic, plain comparisons, every value bound,
NULL refused), `KeyRange { after, upto }.write`, the optional `ChunkHasher` role (one statement
→ one row `(row_count, digest)` for a range), and `inline_params(q, &BoundStatement)` (each
`?` / `$n` / `@Pn` outside strings, quoted identifiers, comments and dollar quotes replaced by
`Quoting::dump_literal`; parameter count checked). `Quoting::script_terminator()`: `";"`,
SQL Server `"\nGO"`.

### 3.5 Bulk loader hooks (`oxbase_dialect::bulk`)

`BulkLoader::foreign_key_checks(table, enabled) -> Option<Vec<String>>` (default `None`;
MySQL / MariaDB / SQLite session statements, SQL Server per-table `NOCHECK` / `WITH CHECK
CHECK`) and `BulkLoader::identity_resync(table, columns) -> Vec<String>` (default empty;
PostgreSQL `setval`). Overrides: transfer stream.

### 3.6 Registration and the M8 guard

`DialectRegistry::missing_m8_roles(engine)`: for each of `M8_ENGINES` (PostgreSQL, MySQL,
MariaDB, SQLite, SQL Server) a `TypeMapper`, and a `DdlGenerator` and a `Comparator` for every
`SYNC_KINDS` kind (table, column, index, foreign key, check, view, materialized view, sequence,
function, procedure, trigger) that has a fetcher. Expected registrations at the end of M8:

| Engine | Sync kinds with fetchers (need D + C) |
|---|---|
| PostgreSQL | designer kinds, view, matview, sequence, function, procedure, trigger |
| MySQL | designer kinds, view, function, procedure, trigger |
| MariaDB | MySQL's, sequence |
| SQL Server | designer kinds, view, sequence, function, procedure, trigger |
| SQLite | designer kinds, view, trigger |

## 4. Data transfer (transfer stream)

### 4.1 Mapping cells (`oxbase_transfer::cells`)

`PgTypeMapper`, `MySqlTypeMapper { mariadb }`, `SqliteTypeMapper`, `MsSqlTypeMapper`; the
required facts per engine are in each module's docs (from the engines' public documentation).
Summary of the PG → MySQL direction (the exit check), as pinned by `[m8.transfer.expected_mysql]`:

| PostgreSQL | MySQL 8.4 | Fidelity |
|---|---|---|
| `boolean` | `tinyint(1)` | exact |
| `smallint` / `integer` / `bigint` / `serial` | `smallint` / `int` / `bigint` / `int AUTO_INCREMENT` | exact |
| `numeric(38,10)` | `decimal(38,10)` | exact |
| `numeric` (unconstrained) | `decimal(65,30)` | narrowed (scale > 30, precision > 65 refused per value) |
| `real` / `double precision` | `float` / `double` | exact |
| `char(10)` / `varchar(255)` / `text` | `char(10)` / `varchar(255)` / `longtext` | exact (utf8mb4) |
| `bytea` | `longblob` | exact |
| `date` / `time(6)` / `timestamp(6)` | `date` / `time(6)` / `datetime(6)` | exact |
| `timestamptz(6)` | `datetime(6)` holding the UTC instant | narrowed (offset dropped) |
| `interval` | `varchar(64)` (PostgreSQL text form) | widened |
| `uuid` | `char(36)` | widened |
| `json` / `jsonb` | `json` | exact (as JSON values) |
| `integer[]` / `text[]` | `json` arrays | widened |
| enum `m8_mood` | `enum('sad','ok','happy')` | exact |

Golden tests: for every ordered pair of the five engines (25 pairs, same-engine included),
`pair_report` over `representative_types()` (30 logical types) is pinned in
`crates/oxbase-transfer/tests/golden/<source>_<target>.txt` (one line per type: logical,
native, fidelity, reason).

### 4.2 Spec (`oxbase_transfer::spec`, done)

`TransferSpec { objects: Vec<SourceObject>, target: ObjectRef (Database | Schema), options:
TransferOptions, overrides: Vec<ColumnOverride> }`. `SourceObject::{Table {table,
targetName?, filter?}, View {view, targetName?, filter?}, Query {sql, targetName}}`.
`TransferOptions { mode: create | replace | append | truncate, batchRows (10 000), transaction:
per_table | per_batch, errors: stop | continue, identity: keep | generate, fkChecks: keep |
disable, createIndexes (true), createForeignKeys (true), maxReportedErrors (1000) }`.
`ColumnOverride { object, column, skip?, targetName?, dataType?, nativeType? }`.
`validate()` checks the shape without a connection.

### 4.3 Plan / dry run (`oxbase_transfer::pipeline::plan`)

Catalog reads only. Per object: definition (or the query's Arrow schema), column plans
(`map_column`), target table (`target_table`), existence and mode checks, DDL preview text
(`DdlGenerator` + `Script::to_sql(script_terminator)`), FK statements for after the load,
load order (`table_dependencies` of the source definitions; cycles reported). `TransferPlan {
sourceEngine, targetEngine, tables: [TablePlan {object, source?, target, exists, mode,
columns: [ColumnPlan {source, sourceType, sourceNative?, target, mapping, overridden,
skipped}], fidelity, ddl, foreignKeys, estimatedRows?, warnings}], order, cycles, fidelity,
destructive, transactionalDdl, runnable, blocked, warnings }`.

Rules: same engine keeps native types and everything engine-native (relocated); across
engines drop engine-native text the target cannot parse (non-literal defaults, collations,
generated expressions, checks, expression / partial indexes, extras) with a warning each;
`append` / `truncate` match existing columns by name and compare against their types
(`fidelity_between`); a plan with an `unsupported` column, a failed mode check or a name clash
under the target's case rules is not `runnable`.

### 4.4 Run (`copy_table`, `BatchConverter`)

Per table on dedicated sessions: DDL (in the table's transaction where DDL is transactional
and `per_table`), `BulkLoader::prepare`, FK checks off if asked and possible, stream →
convert → re-chunk → `load`, commits per `TransactionScope`, `finish`, identity resync. Row
issues: `stop` fails the table at the first; `continue` drops and reports them (first
`maxReportedErrors`). Server errors fail the table. What a failure leaves is stated in
`TableReport::left_behind` (MySQL / MariaDB: the services drop a table the run created).
Progress at most every 250 ms. Cancel within 2 s.

### 4.5 Service (`oxbase_services::transfer`)

`TransferRequest { sourceSession, targetSession, spec, confirmDestructive }`; `plan`,
`pair_report`, `start → TransferHandle { id, events }`, `cancel`, `cancel_session`.
`TransferEvent` (`type`-tagged camelCase): `started {transferId, plan}`, `tableStarted
{table}`, `progress {progress: TableProgress}`, `rowError {table, issue}`, `tableFinished
{report: TableReport}`, then one of `finished {report: TransferReport}`, `failed {error,
report}`, `cancelled {report}`.

### 4.6 Not in M8

MongoDB / Redis endpoints (§2), cross-engine views / routines / triggers (only tables and
their keys and indexes travel across engines; same-engine transfers copy table structure, not
other objects: use structure sync for that), resumable transfers.

## 5. Structure sync (structure-sync stream)

### 5.1 Compare (`oxbase_diff::structure`)

`StructureOptions { kinds (default table, view, matview, sequence, function, procedure,
trigger), includeDrops (false), compareCollations (true), compareComments (true),
compareOwnership (false), ignoreCase (false), exclude: [ObjectRef] }`.
`load_snapshot(dialects, connection, container, options, on_object, cancel) ->
SchemaSnapshot { engine, version, container, objects, skipped }` through the engine's
fetchers. `compare(dialects, source, target, options) -> StructureDiff { engine, source,
target, items: [DiffItem], warnings }`; `DiffItem { id, kind, name, parent?, status:
only_in_source | only_in_target | different | same, source?, target?, properties:
[PropertyDiff] (going from target to source), sourceDdl?, targetDdl? }`. Ids: `item_id`
(`table:orders`, `trigger:orders.orders_audit`, `function:order_total(p_order integer)` on
PostgreSQL). Sorted by kind rank, then name. `StructureDiff::is_in_sync(include_drops)` is the
re-compare check.

Steps: relocate (and `retarget_sql`) every source definition into the target container;
match by kind + name (+ parent, + signature), exact or `ignoreCase`; `Comparator::diff(target,
source)`; filter properties per options; both sides' `CREATE` text per item.

### 5.2 Objects other than tables (`oxbase_dialect::compare_objects`, engine `sync_ddl.rs`)

`DefinitionComparator<R: CompareRules>` / `diff_definitions` with fixed property ids and order
(module docs: view `definition`, `columns`, `comment`, extras; routine `args`, `returns`,
`language`, `body`, `comment`, extras; trigger `timing`, `events`, `update_columns`, `level`,
`condition`, `function`, `body`, `enabled`, `comment`, extras; sequence `data_type`, `start`,
`increment`, `min_value`, `max_value`, `cache`, `cycle`, `owned_by`, `comment`, extras).
Per-engine `DdlGenerator` cells for those kinds: `create` from the model (views: `CREATE VIEW
q AS <definition>`; routines and triggers: the fetched `definition` text with names retargeted,
or built from the model where the engine's definition is not a full statement), `drop_object`
in the engine's syntax (`DROP FUNCTION f(<signature>)`, `DROP TRIGGER t ON tbl` on PostgreSQL,
`DROP TRIGGER [s].[t]` on SQL Server, ...), `alter` = `CREATE OR REPLACE` / `CREATE OR ALTER`
where the engine has it, else drop + create (steps flagged; MySQL / MariaDB non-transactional).
MySQL / MariaDB statements carry no `DEFINER`, so the applying account becomes the definer, also
on an alter with `compareOwnership` off; the script warns for every trigger, and every view or
routine whose `SQL SECURITY` is not `INVOKER`, whose definer is not kept. A restore lists the
same warnings in its report.
Round trip like M3: an object fetched right after `create` / `alter` compares equal to the
definition the script came from.

### 5.3 Matching rules that keep scripts correct

- Column drops inside a changed table are part of making the table equal and are always in
  its script (destructive: the apply needs confirmation); `includeDrops` concerns whole objects
  that exist only in the target.
- No rename detection: a renamed object is a drop plus a create (the UI shows both items).
- Triggers belong to their table: excluding or dropping a table excludes / drops its triggers.
- A view or routine that depends on a changed table is re-created after the table changes
  when the engine invalidates it (SQL Server `sp_refreshview` is not used; the cell re-creates).

### 5.4 Script (`oxbase_diff::script::sync_script`)

`SyncScript { engine, script: Script, sql, items, destructive, atomic, transactionalDdl,
warnings }`; order: dependents' drops (triggers, views, routines), changed / doomed FKs, table
drops (children first), sequences, table creates (parents first, no FKs) and alters, FK adds,
routines, views (dependency order), triggers. Golden tests: per engine, the script of the
`[m8]` pair with every item selected and drops on is pinned in
`crates/oxbase-diff/tests/golden/structure_<engine>.sql` (generated from hand-built snapshots
in a unit test, so the pin does not need Docker; the script of the definitions loaded live is
not pinned, see §13).

### 5.5 Service (`oxbase_services::sync::StructureSyncService`)

`compare(StructureCompareRequest {sourceSession, source, targetSession, target, options},
cancel) -> StructureCompareResult {compareId, diff, counts, skipped, elapsedMs}`;
`script(StructureScriptRequest {compareId, selection}) -> SyncScript`;
`apply(StructureApplyRequest {compareId, selection, confirmDestructive}, cancel) ->
StructureApplyReport {script, outcome: ScriptOutcome, recompare, inSync}`; `forget`,
`cancel_session`. Apply: refuse destructive without confirmation; refuse when a selected
target object changed since the compare; `execute_script` (atomic where possible); schema
events; re-compare.

## 6. Data sync (data-sync stream)

### 6.1 Keys (`oxbase_diff::data::resolve_key`, done)

Primary key, else the first unique key on NOT NULL plain columns, else refused ("choose key
columns"); `KeyChoice::{Primary, Unique {name}, Columns {columns}}`; user-named columns are
checked for duplicates while comparing.

### 6.2 Compare (`compare_table`)

`TablePair { source, target, key?, columns? }`, `DataCompareOptions { chunkRows (10 000),
sampleRows (20), useChunkHash (true) }`. Keyset chunks on the source; the same `(after, upto]`
on the target; final `(last, +∞)` on the target; optional digest skip; exact value match
(module docs); inserts / updates / deletes counted, sampled as text and stored in the
`DiffStore`. `TableDataDiff { pair, source, target, status: compared | refused {reason} |
failed {message} | cancelled, key, columns, counts: RowCounts {sourceRows, targetRows, same,
inserts, updates, deletes}, samples: [RowSample {op, key, source?, target?, changed}],
chunksSkipped, warnings, elapsedMs }`.

### 6.3 Store (`oxbase_diff::store::DiffStore`)

Per (pair, `RowOp`) an Arrow IPC stream file in a private temp dir (deleted on drop):
inserts / updates hold key + compared columns of the source row, deletes the key.

### 6.4 Script and apply

`data_statements(dialects, engine, version, target_table, key, op, rows)` → bound statements
through the engine's `DmlWriter`. Order: deletes (children first), updates, inserts (parents
first). `DataScriptOptions { inserts (true), updates (true), deletes (false), pairs, previewLimit
(1000) }` → `DataScript { engine, statements, totalStatements, truncated, destructive, warnings }`.
`DataApplyOptions { script, batchStatements (1000), confirmDeletes }`: transactions of
`batchStatements`, each statement must affect exactly one row (else the batch rolls back and
the apply stops), then a re-compare of the applied pairs (`DataApplyReport { tables:
[TableApplyReport], statements, committedBatches, leftBehind, recompare?, inSync, elapsedMs }`).

Service (`DataSyncService`): `start_compare(DataCompareRequest {sourceSession,
targetSession, pairs, options}) → DataRunHandle<DataCompareEvent>` (`started {compareId,
tables}`, `progress {progress}`, `tableFinished {table}`, one of `finished {result}` / `failed
{error, result}` / `cancelled {result}`); `script(DataScriptRequest {compareId, options},
cancel)`; `start_apply(DataApplyRequest {compareId, options}) →
DataRunHandle<DataApplyEvent>` (`started {applyId, totalStatements}`, `progress
{statementsDone, totalStatements, pair, elapsedMs}`, `recomparing`, one terminal with
`report`); `forget`, `cancel`, `cancel_session`.

## 7. Backup and restore (backup stream)

### 7.1 Format (`oxbase_backup::format`, done)

ZIP (zip64 when large), entries `manifest.json` (deflate), `objects/NNNN.json` (ObjectDef,
deflate), `objects/NNNN.sql` (CREATE text, deflate), `data/NNNN.arrows` (Arrow IPC stream in
the load schema, zstd buffer compression, stored). `Manifest { format: "oxbase-backup",
formatVersion: 1, oxbaseVersion, createdAt, engine, serverVersion, source: {container},
content: structure_and_data | structure_only | data_only (1.2: tables only, rows restored into
existing tables, nothing created), compression: {codec: none | zstd}, objects:
[ManifestObject {index, kind, ref, definition, ddl?, data?: {entry, rows, columns}}], order,
entries: {name: {size, sha256}}, warnings, ...unknown }`. Never: hosts, ports, users,
passwords, SSH / TLS settings, profile names, local paths. Readers refuse newer format
versions (`Unsupported`) and malformed manifests (`validate`: unique indexes, `order` a
permutation, entry names of the fixed shapes only, every named entry listed).

### 7.2 Backup

`BackupSpec { container, objects (empty: all), content, compression }`. Snapshot via
`oxbase_diff::load_snapshot`, order like the sync script, definitions + DDL, table rows
(generated columns left out) converted to the load schema, one consistent read transaction per
table (or per backup where the engine allows), temp file renamed on success
(`oxbase_io::SinkFile`). `BackupProgress`, `BackupReport { manifest, bytes, elapsedMs }`.

### 7.3 Restore

`RestoreOptions { container (existing), content: structure_and_data | structure_only |
data_only, objects (indexes), dropExisting, truncateBeforeLoad, verifyFirst (true),
confirmDestructive, batchRows? }`. Same engine only. Verify first; structure through
`compare` + `sync_script` + `execute_script` (selected objects; drops only with
`dropExisting`; an existing different object without `dropExisting` is an error); data per
table in manifest order through the `BulkLoader`, one transaction per table. `RestoreReport {
objects: [RestoredObject {index, object, status: created | replaced | kept | loaded | failed |
not_run, rows, error?}], rows, leftBehind, warnings, elapsedMs }`. M8 does not create the target
database / schema.

### 7.4 Verify and list

`ArchiveReader::verify` recomputes every SHA-256 and table row count → `VerifyReport { ok,
entriesChecked, bytesChecked, problems: [EntryCheck {entry, ok, problem?}] }`; `list` returns
the validated manifest.

Service (`BackupService`): `start_backup(BackupRequest {session, spec}, destination,
overwrite) → BackupRunHandle<BackupEvent>` (`started {backupId, objects}`, `progress
{progress}`, `finished {report}` / `failed {error, elapsedMs}` / `cancelled {elapsedMs}`);
`start_restore(RestoreRequest {session, options}, source) → BackupRunHandle<RestoreEvent>`
(`started {restoreId, manifest}`, `progress`, terminal with `report`); `verify(path, cancel)`;
`list(path, cancel)`; `cancel`; `cancel_session`.

## 8. Tauri commands, TS and UI

### 8.1 Commands (`crates/app-tauri/src/sync.rs`, `ui/src/api/m8.ts`)

camelCase payloads; core models inside keep snake_case. Errors are `CommandError {category,
message}`. Every command is in `generate_handler!`, `build.rs` and
`capabilities/default.json`.

| Command | Arguments | Returns |
|---|---|---|
| `transfer_plan` | `request: TransferRequest`, `requestId?` | `TransferPlan` (dry run) |
| `transfer_pair_report` | `sourceSession`, `targetSession` | `PairReport` |
| `transfer_run` | `request`, `onEvent: Channel<TransferEvent>` | `OperationId` |
| `transfer_cancel` | `transferId` | `boolean` |
| `structure_compare` | `request: StructureCompareRequest`, `requestId?` | `StructureCompareResult` |
| `structure_script` | `request: StructureScriptRequest` | `SyncScript` |
| `structure_apply` | `request: StructureApplyRequest`, `requestId?` | `StructureApplyReport` |
| `compare_forget` | `compareId` | `boolean` |
| `data_compare` | `request: DataCompareRequest`, `onEvent: Channel<DataCompareEvent>` | `OperationId` |
| `data_script` | `request: DataScriptRequest`, `requestId?` | `DataScript` |
| `data_apply` | `request: DataApplyRequest`, `onEvent: Channel<DataApplyEvent>` | `OperationId` |
| `data_cancel` | `operationId` | `boolean` |
| `backup_run` | `file` (from `file_pick_save`, used up), `request: BackupRequest`, `onEvent: Channel<BackupEvent>` | `OperationId` |
| `backup_restore` | `file` (from `file_pick_open`), `request: RestoreRequest`, `onEvent: Channel<RestoreEvent>` | `OperationId` |
| `backup_verify` | `file`, `requestId?` | `VerifyReport` |
| `backup_list` | `file`, `requestId?` | `Manifest` |
| `backup_cancel` | `operationId` | `boolean` |

The task's names map 1:1; `transfer_pair_report`, `compare_forget`, `data_cancel` and
`backup_cancel` are additions. File dialogs use `BACKUP_FILTERS` (`.oxbackup`).

### 8.2 Long runs

`oxbase_services::runs::RunRegistry` (`register`, `register_with(token)`, `cancel`,
`cancel_session`, `RunTicket` unregisters on drop). Request-style operations register with
their request token. `close_connection` calls `SyncState::cancel_session(session)` first.

### 8.3 Events

Every run ends with exactly one terminal event (`finished`, `failed`, `cancelled`;
`isTerminalM8Event` in TS); progress events are lossy (`try_send`), every other event is
awaited.

### 8.4 UI (app stream)

Design system rules apply (tokens only, lucide icons with `aria-label`s, both themes, schema
before table names via `qualifiedName`); existing test ids stay.

- **Transfer wizard**: source (session, objects from the tree with filters, or a query) →
  target (session, database / schema) → objects (target names, modes) → mapping (per table, per
  column: source type, target type, a lossiness badge `exact` / `widened` / `narrowed` /
  `unsupported` with the reason as tooltip, override type / name / skip) → options (§4.2) →
  review (`transfer_plan`: DDL in Monaco read-only, order, warnings, destructive confirmation)
  → progress (per table rows / bytes / errors, cancel) → report.
- **Structure sync view**: pick both sides and options, compare; side-by-side tree of items
  grouped by kind with status icons and checkboxes (only-in-source, only-in-target, different;
  `same` hidden by default); per item a DDL diff (`sourceDdl` vs `targetDdl`, Monaco diff) and
  the property list; script preview in Monaco (`structure_script`, selection-driven); apply with
  a confirmation that names destructive steps and says when the engine's DDL is not
  transactional; then the re-compare result (expected: in sync).
- **Data sync view**: pairs (auto-matched by name, key chosen or refused with the reason),
  compare with progress; per table counts and sample rows (insert / update / delete tabs,
  changed cells highlighted); script preview (inserts / updates on, deletes off by default);
  apply with confirmation (deletes need their own), progress, re-compare result.
- **Backup / restore dialogs**: backup (container, objects, content, compression; save dialog;
  progress; report); restore (open dialog, `backup_list` manifest view, verify, target
  container, content, objects, drop existing / truncate with confirmation, progress, report);
  verify alone.
- **Webview smoke** (Linux, `OXBASE_E2E_REQUIRE_M8=1`): PostgreSQL `m8_src` → `m8_tgt`
  structure compare shows the `[m8] structure_items`, apply all but drops, re-compare shows no
  differences for them; data compare of `m8_data` shows 2 / 5 / 2 counts; a transfer of
  `m8_xfer.m8_types` into a new PostgreSQL schema runs to `finished`; a backup of `m8_src`
  verifies `ok`; screenshots in both themes.

## 9. Fixtures (`tests/fixtures.toml` `[m8]`, conformance stream)

`[m8]` names the objects and expectations; the conformance stream writes the seeds:
`tests/docker/init/postgres/06-m8.sql`, `mysql/06-m8.sql`, `mariadb/06-m8.sql`,
`mssql/07-m8.sql` (run by the entrypoint wrapper like `06-m4.sql`) and the SQLite scripts
`sqlite/03-m8-src.sql` / `04-m8-tgt.sql`. Ports and containers stay as they are.

| Engine | Source | Target | Restore into | Transfer |
|---|---|---|---|---|
| PostgreSQL | schema `m8_src` of `oxbase_test` | schema `m8_tgt` | schema `m8_restore` | source schema `m8_xfer` |
| MySQL | database `m8_src` | database `m8_tgt` | `m8_restore` | target database `m8_xfer_target` |
| MariaDB | `m8_src` | `m8_tgt` | `m8_restore` | `m8_xfer_target` |
| SQL Server | schema `m8_src` of `oxbase_test` | schema `m8_tgt` | schema `m8_restore` | target schema `m8_xfer_target` |
| SQLite | file from `03-m8-src.sql` | file from `04-m8-tgt.sql` | an empty file | - |

MySQL / MariaDB seeds create the databases as root and grant the fixture user `oxbase` all
privileges on them (plus `CREATE ROUTINE`, `TRIGGER`). The designed structure and data
differences, the expected counts and the PG transfer set with the expected MySQL column types
are spelled out in the `[m8]` comments and keys of `tests/fixtures.toml` (one source of
truth).

## 10. Dependencies

| Crate | Version | Where | License | Lockfile |
|---|---|---|---|---|
| `sha2` | 0.10.9 | workspace, oxbase-backup | MIT OR Apache-2.0 | already present (rustls / russh / mongodb) |
| `arrow-ipc` feature `zstd` | 60.1.0 | oxbase-backup | Apache-2.0 | `zstd` 0.14.3 already present (parquet) |
| `zip` | 9.2.1 (`deflate`) | oxbase-backup (workspace dep since M5) | MIT | present |

`deny.toml` is unchanged. A stream that needs another crate adds it with an explicit version to
its own crate, checks the license against `deny.toml` and reports it for hoisting.

## 11. Invariants every stream keeps

1. **No engine special cases** in `oxbase-transfer` (outside `cells`), `oxbase-diff`,
   `oxbase-backup`, services or the app: engine behaviour comes from registry roles
   (`TypeMapper`, `Fetcher`, `DdlGenerator`, `Comparator`, `DmlWriter`, `BulkLoader`,
   `Quoting`, `Capabilities`, `ChunkHasher`).
2. **Nothing writes without an explicit apply**: plans, compares, scripts, previews, verify and
   list read only. Every script is previewable before it runs.
3. **Destructive steps are flagged and confirmed**: drops, column drops, narrowing changes,
   rebuilds, truncates, data deletes, `replace` mode, restore over existing objects; the
   services refuse them without the request's confirmation flag.
4. **Transactions where the engine has them**: PostgreSQL, SQL Server and SQLite run DDL
   scripts atomically; MySQL / MariaDB DDL commits per statement and every preview / report
   says so.
5. **Arrow pages, bounded memory**: transfer, data compare, backup and restore stream; no
   whole table in memory; differences spill to disk.
6. **Cancel within 2 s**, everything async with progress; closing a connection cancels its runs.
7. **Values bound, names quoted**: DML and key ranges bind every value; previews render literals
   with `dump_literal` only for display / saving.
8. **Secrets** never in Debug / Display, logs, errors, panics, history, backup files,
   scripts or Tauri responses; manifests carry names only.
9. **Determinism**: diff items, script steps, plans and manifests are ordered deterministically
   (golden tests depend on it).
10. **Clean room**: type and syntax facts from the engines' public documentation only.

## 12. Exit-check tests the streams must write

Live tests are `#[ignore]`d and run in the conformance CI job against the Docker fixtures. A
missing fixture or seed is "pending" unless the stream's `REQUIRE` variable is set
(`OXBASE_SERVICES_REQUIRE_M8`, `OXBASE_CONFORMANCE_REQUIRE_M8`; CI sets them at integration).

- **transfer**: unit: every mapper total over `representative_types()`, reasons on non-exact
  mappings, the 25 golden pair reports, `fidelity_between` table, `map_column` overrides,
  `target_table` across engines (dropped defaults / checks warned), `BatchConverter` per
  Arrow → load type pair (casts, text forms, range / length / NOT NULL issues), `plan` over the
  mock driver (order, cycles, blocked plans). Live (`oxbase-services/tests/m8_transfer_live.rs`):
  **PG → MySQL** of `[m8.transfer] tables` with the tables listed child first: plan fidelities
  equal `[m8.transfer.expected_mysql]`, `information_schema.COLUMNS.COLUMN_TYPE` equals the
  expected native types, every value reads back equal (rules in `[m8.transfer]`), FKs exist
  after the load, `m8_parent` loaded before `m8_child`; PG → PG (another schema), MySQL →
  PostgreSQL, SQL Server → MySQL and SQLite → PostgreSQL of `m8_data` (row count and checksum
  of the values' text forms); `continue` mode skips a bad row and reports it; cancel mid-table
  within 2 s with `left_behind` true; closing the source session cancels a running transfer.
- **structure-sync**: unit: `diff_definitions` per kind and property; `retarget_sql` on each
  engine's dialect (quoted / unquoted / strings / comments untouched); `sync_script` order on
  hand-built snapshots (drops of dependents first, FK adds last, SQLite rebuild steps grouped)
  and the golden scripts per engine; engine `sync_ddl` cells: create / alter / drop SQL per kind
  and round trip (live, per engine). Live (`m8_structure_live.rs`, per engine PG, MySQL,
  MariaDB, SQL Server, SQLite): compare `m8_src` / `m8_tgt` reports exactly `[m8]
  structure_items` (minus `skip_items`) with the designed statuses; the script of every item
  with drops runs without error; a re-compare is in sync; without drops `legacy_notes` and
  `order_lines` stay and the re-compare reports only them; a stale apply (target changed after
  the compare) is refused; a destructive apply without confirmation is refused.
- **data-sync**: unit: keyset predicates, `DiffStore` round trip (100 000 rows in bounded
  memory), `data_statements` per op per engine writer, value equality rules. Live
  (`m8_data_live.rs`, per engine): `m8_data` 2 / 5 / 2, `m8_big` 1 / 1 / 1 with chunks of
  10 000 (and 8 chunks skipped where a `ChunkHasher` is registered: the designed update
  and delete fall in two different chunks),
  `m8_composite` 0 / 1 / 0, `m8_nokey` refused; the script preview renders the expected
  statements (golden per engine for `m8_data`); apply with deletes confirmed, then the
  re-compare is in sync; deletes without confirmation are refused; cancel within 2 s.
- **backup**: unit: archive write / read round trip in memory, verify catches a flipped byte,
  a missing entry and a wrong row count, newer format refused, no connection details in the
  manifest. Live (`m8_backup_live.rs`, per engine): back up `m8_src`, list and verify, restore
  into `m8_restore` (structure and data), a structure compare of `m8_src` vs `m8_restore` is in
  sync and a data compare of every data table is in sync; data-only restore with truncate;
  restore of selected objects; a cancelled backup leaves no file.
- **app**: Vitest for the wizard steps, mapping badges and overrides, the structure tree and
  selection, DDL diff and script preview wiring, data sync counts / samples / script, backup and
  restore dialogs, confirmations. Webview smoke steps of §8.4.
- **conformance**: cells `sync_structure` (per engine: script applies, re-compare in sync),
  `sync_data` (per engine: same), `transfer` (matrix: each of the 5 SQL engines as source into
  each other engine for `m8_data`; PG → MySQL with the `m8_types` set required), `backup`
  (round trip per engine); with `OXBASE_CONFORMANCE_REQUIRE_M8=1` every cell of the five SQL
  engines must pass, nothing pending.

## 13. Deviations from PLAN.md and open points

- PLAN §2 lists "SQL dump" under `oxbase-backup`; it stays in `oxbase-io` (M5 export) and the
  backup has its own format (§2).
- Transfer between engines carries tables (columns, keys, indexes, foreign keys), not views,
  routines or triggers; MongoDB / Redis transfer is deferred.
- Restore needs an existing database / schema (no `CREATE DATABASE` / `SCHEMA` cells in M8).
- Data compare across collations: both sides of a sync are the same engine; a key column whose
  collation differs between the two sides is refused by the plan (it would order and compare
  differently).
- The per-engine `ChunkHasher` is optional; engines without one compare every row.
- The structure-sync goldens pin the hand-built `[m8]` snapshots only. The script generated
  from the fixture definitions loaded live is not pinned as text (the scratch copies' names
  and the defaults / properties each server reports back make it differ in detail); the live
  tests (`m8_structure_live`, conformance `sync_structure`) prove it instead: it runs without
  errors and the re-compare reports zero differences.
