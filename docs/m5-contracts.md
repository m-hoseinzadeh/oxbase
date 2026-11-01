# M5 contracts

Status: frozen for M5 on 2026-11-01 (Design step). M5 exit check (PLAN.md §6): **a 1 GB CSV
import with progress, and export of any result or table**. Scope: import and export of CSV / TSV,
JSON, XLSX, Parquet and SQL dump (export only), a wizard with mapping and preview, per-engine
bulk loading, and native file dialogs that hand the webview opaque handles instead of paths.

Changes to these contracts go through the Design owner. Streams code against them and must not
change public signatures; adding private helpers, private fields and tests is always fine. Rust
doc comments on the items named here are part of the contract and are more detailed than this
file (start with the module docs of `oxbase_io`, `oxbase_dialect::bulk`,
`oxbase_services::import`, `oxbase_services::export` and `crates/app-tauri/src/files.rs`).

## 1. Who owns what

Bodies marked `unimplemented!("m5: <stream>")` belong to that stream. No test may call one until
its stream has filled it. Six streams start from the Design head in parallel.

| Area | Path | Owner | State at hand-off |
|---|---|---|---|
| New dependencies (§11), `[profile.perf]`, `Cargo.lock`, `deny.toml` | root | Design | done |
| `Connection::as_any`, `Tx::as_any`, `cell_value` (Arrow cell → `Value`) | `crates/oxbase-driver` | Design | done, tested |
| `bulk` module: `BulkLoader`, `BulkLimits`, `BulkMethod`, `LoadTarget` / `LoadColumn` / `LoadSession`, `load_arrow_type`, `bind_value`, `InsertLoader`; registry `register_bulk_loader` / `bulk_loader` (with fallback) / `has_bulk_loader` | `crates/oxbase-dialect` | Design | done, tested |
| `oxbase-io` API: options (+ validation, Parquet codecs), `SourceReader` / `SourceBatch` / `RowIssue` / `ColumnSpec` / `SourcePreview`, `preview` / `open` dispatch, `spawn_reader`, `SinkWriter` / `SinkFile` / `WriterHandle` / `create_writer` / `spawn_writer`, `encoding` (BOM, decoding reader, encoding writer, byte counter), `formula`, `logical_type` / `arrow_type_for` / `file_label`, `sql_dump::default_version` | `crates/oxbase-io/src/{lib,options,source,sink,encoding,formula,types}.rs` | Design | done, tested |
| Type inference; CSV / JSON / XLSX / Parquet **readers** | `crates/oxbase-io/src/{infer,csv_file,json_file,xlsx_file,parquet_file}.rs` (`preview`, `open`, new private files under `src/`), `crates/oxbase-io/tests/read_*.rs` | **read** | skeleton |
| CSV / JSON / XLSX / Parquet / SQL dump **writers** | `crates/oxbase-io/src/{csv_file,json_file,xlsx_file,parquet_file,sql_dump}.rs` (`writer`), `crates/oxbase-io/tests/write_*.rs` | **write** | skeleton |
| Native bulk loaders and their registration; `as_any` overrides | `crates/oxbase-{pg,mysql,sqlite,mssql}/src/**` (new `dialect/bulk*.rs`, the `register` line, `Connection::as_any` / `Tx::as_any`), `crates/oxbase-{pg,mysql,sqlite,mssql}/tests/bulk_live.rs` | **load** | - (fallback `InsertLoader` works today) |
| `ImportService` (`plan`, `start`, `conversion_note`), `ExportService::start`, their live tests | `crates/oxbase-services/src/{import,export}.rs` (+ `import/**`, `export/**`), `crates/oxbase-services/tests/{import,export}_live.rs` | **services** | skeleton (types, constructors, `preview`, `cancel` done) |
| Import / export wizard UI, export from grid and tree; Tauri command bodies stay thin | `ui/src/**` except `ui/src/api/**` (payload additions there only with Design), `crates/app-tauri/src/{files,transfer}.rs` (bug fixes only) | **app** | commands done (§6-7), no UI |
| 1 GB exit check: generator, test, CI job | `crates/oxbase-services/tests/import_perf.rs` (+ `tests/perf/**`), the new `import-perf` job of `.github/workflows/ci.yml`, `scripts/ci-fixtures.sh` (service list argument) | **perf** | - |
| Contract, PLAN, README, fixtures `[m5]`, samples | `docs/m5-contracts.md`, `PLAN.md`, `README.md`, `tests/fixtures.toml`, `tests/data/m5/**` | Design | done |

Streams add no crate to `Cargo.lock`: every dependency they need is in place (§11). A stream that
needs another crate adds it with an explicit version to its own crate's `Cargo.toml`, checks the
license against `deny.toml` and reports it for hoisting.

**Live tests** are `#[ignore]`d and read `tests/fixtures.toml`. The conformance job already runs
`-p oxbase-pg -p oxbase-mysql -p oxbase-sqlite -p oxbase-mssql -p oxbase-services` with
`--include-ignored`, so the load and services live tests run there with no CI change. `oxbase-io`
has no live tests (it needs no database); its tests run in the Linux / macOS / Windows test jobs.

Integration (after the streams): every engine registers its native loader
(`DialectRegistry::has_bulk_loader` true for PostgreSQL, MySQL, MariaDB, SQLite, SQL Server), the
perf job is green on the integration branch, and PLAN / README get the *as built* notes.

## 2. Core additions

### 2.1 Driver (`oxbase-driver`)

- `Connection::as_any(&self) -> Option<&(dyn Any + Send + Sync)>` and the same on `Tx`, default
  `None`. Engines return `Some(self)` where one of their own cells downcasts (PostgreSQL
  `COPY ... FROM STDIN` through tokio-postgres `copy_in`, SQL Server `bulk_insert`). **Only code in
  the same engine crate may downcast** (PLAN §1 rule 3). PostgreSQL and SQL Server transactions
  are session state, so the `Tx` downcast reaches the same client as the connection's.
- `cell_value(&dyn Array, row) -> Result<Value>`: one Arrow cell as a `Value` (table in the doc
  comment). Used by `InsertLoader` and the SQL dump writer.

### 2.2 Dialect (`oxbase_dialect::bulk`, §4)

`BulkLoader` is an engine-wide role kept next to the `EngineRole`s but **not** in
`EngineRole::ALL`, so the conformance matrix and `missing_designer_roles` do not change.

## 3. `oxbase-io`

### 3.1 Shape

The crate reads and writes **files** only; it knows nothing of sessions, tables or the UI.
Readers and writers are **synchronous** (`SourceReader`, `SinkWriter`) and run on blocking
threads; `spawn_reader` / `spawn_writer` wrap them as async streams / handles with bounded
queues. Everything is in Arrow `RecordBatch`es (PLAN §1 rule 2).

### 3.2 Options (`ReadOptions`, `WriteOptions`)

JSON forms (camelCase fields, serde defaults for every field, so the UI sends only what it
changed):

```json
{"format": "csv", "delimiter": ";", "encoding": "windows1252", "header": true,
 "nullTokens": ["", "\\N"], "formats": {"decimalSeparator": ",", "dateFormat": "%d.%m.%Y"}}
{"format": "json", "layout": "lines"}
{"format": "xlsx", "sheet": "Data", "headerRow": 1}
{"format": "parquet"}
{"format": "parquet", "compression": {"codec": "zstd", "level": 3}, "rowGroupRows": 131072}
{"format": "sql_dump", "target": "mysql", "rowsPerInsert": 500}
```

| Read option | Values (default) |
|---|---|
| CSV `delimiter` / `quote` / `escape` / `comment` | ASCII characters (`,` / `"` / none = doubled quotes / none) |
| CSV `header`, `skipRows`, `trim`, `allowRagged` | (`true`, 0, `false`, `true`) |
| CSV / JSON `encoding` | `auto` (BOM, else UTF-8 if valid, else Windows-1252), `utf8`, `utf16le`, `utf16be`, `windows1252`, `latin1` (true ISO-8859-1; JSON: UTF only) |
| CSV `lineEnding` | `auto` (LF and CRLF), `cr` (also a lone CR) |
| CSV `nullTokens` | texts read as NULL (`[""]`) |
| `formats` (`ValueFormats`) | `dateFormat` / `timeFormat` / `timestampFormat` (chrono strftime; ISO 8601 when absent), `decimalSeparator` (`.` or `,`), `thousandsSeparator`, `trueValues` / `falseValues` |
| JSON `layout` | `auto`, `array`, `lines` |
| XLSX `sheet`, `headerRow` | (first sheet, 1; 0 = no header) |

| Write option | Values (default) |
|---|---|
| CSV `delimiter`, `quote`, `quoteStyle`, `escape`, `header` | (`,`, `"`, `necessary` / `always` / `non_numeric` / `never`, doubled, `true`) |
| CSV `encoding`, `bom`, `lineEnding`, `nullToken` | (`utf8`, `false`, `lf` / `cr_lf`, empty) |
| CSV / XLSX `formulaGuard` | `true` (§3.8) |
| JSON `layout`, `pretty`, `binary`, `explicitNulls` | (`array`, `false`, `base64` / `hex`, `true`) |
| XLSX `sheetName`, `header`, `maxRowsPerSheet`, `dateFormat`, `timestampFormat` | (`Sheet1`, `true`, 1 048 576, `yyyy-mm-dd`, `yyyy-mm-dd hh:mm:ss`) |
| Parquet `compression`, `rowGroupRows` | (`snappy`; `none`, `gzip`, `brotli`, `lz4_raw`, `zstd` with optional `level`; 131 072) |
| SQL dump `target`, `targetVersion`, `table`, `includeCreate`, `includeDrop`, `rowsPerInsert` | (required, per-engine default §3.7, source name or `export`, `true`, `false`, 100) |

`validate()` on every options type rejects what cannot work (non-ASCII delimiter, delimiter =
quote, decimal separator = delimiter, encoding `auto` for writing, BOM with a single-byte
encoding, sheet names over 31 characters or with `[]:*?/\`, rows per sheet outside 2..=1 048 576,
Parquet levels out of range, `rowsPerInsert` outside 1..=10 000, SQL dumps for MongoDB / Redis).
`preview`, `open` and `create_writer` call it first.

### 3.3 Reading

1. `preview(path, &ReadOptions, PreviewLimits, &cancel) -> SourcePreview`: reads at most
   `sample_rows` (1000) rows or `sample_bytes` (8 MiB), infers types (§3.4), returns columns
   (`index`, `name`, `dataType`, `nullable`, `warnings`), the first `display_rows` (100) rows as
   display text, `sampledRows`, `complete`, `totalBytes`, `detected` (encoding, BOM, delimiter,
   JSON lines, sheets and the sheet read, Parquet row count) and file-level `warnings`.
2. The caller turns the preview into `ColumnSpec { name, dataType }`s, one per source column in
   source order (`ColumnSpec::from_preview`, then overrides).
3. `open(path, &ReadOptions, &[ColumnSpec], batch_rows) -> Box<dyn SourceReader>`. Every batch is
   in `ColumnSpec::schema(specs)`: each column has the **canonical load type** of its logical type
   (`arrow_type_for` = `oxbase_dialect::bulk::load_arrow_type`, §4.1), all fields nullable.
4. `SourceReader::next_batch() -> Result<Option<SourceBatch>>`; a `SourceBatch` carries the batch,
   `first_row` (1-based data row number), `issues: Vec<RowIssue>` and `bytes_read`. A cell that
   cannot be read as its column's type is **NULL in the batch and listed as a `RowIssue`**
   (`row`, `batchRow`, `column`, `message` with at most 200 characters of the value). A
   malformed *file* (broken JSON, a zip that is not a workbook) is an `Err` (`InvalidInput`
   naming the position) and ends the reader.
5. `spawn_reader(reader, cancel, capacity)`: a stream with at most `capacity` batches buffered;
   one `Err(Cancelled)` after the token fires (checked before each batch).

Per-format rules are in the module docs of `csv_file`, `json_file`, `xlsx_file` and
`parquet_file` (header names made unique, ragged rows, nested JSON as JSON text, XLSX cell types
and cached formula values, Parquet casts, progress per format, memory exceptions).

### 3.4 Type inference (`infer::TypeInference`)

Order for text values: `bool` (`true` / `false` only) → `int(64)` (no leading zeros) →
`decimal(p, s)` (≤ 38 digits) → `float(64)` → `date` / `timestamp` / `timestamp_tz` / `time` →
`uuid` → `json` (`{` / `[` that parses) → `text`. All-NULL columns are `text`. `nullable` is
`false` only when the sample was the whole file and had no NULL. Typed sources report through
`observe_typed` and widen (`int` + `float` → `float`, anything + text → text). Warnings name
values that broke a narrower type and long texts (> 255 characters). Full rules: module docs.

### 3.5 Writing

- `create_writer(destination, overwrite, schema, &WriteOptions, &WriteContext)`: validates the
  options, then creates a `SinkFile`. **Fails before creating anything** when the destination
  exists and `overwrite` is false, or is a folder.
- `SinkFile`: a temporary file `.oxbase-<random>.part` **in the destination's directory**;
  `commit()` flushes, `fsync`s and renames it over the destination (`persist`, or
  `persist_noclobber` without `overwrite`); `abort()` or dropping deletes it. A failed or
  cancelled export never leaves a partial file and never damages an existing one.
- `SinkWriter`: `write(&batch)`, `rows_written()`, `bytes_written()`, `finish() -> SinkReport`
  (`rows`, `bytes`, `sheets`, `warnings`), `abort()`. After a `write` error the writer is done.
- `spawn_writer(writer, capacity) -> WriterHandle`: `write(batch).await` (bounded queue; returns
  the writer's error once it failed), `finish().await`, `abort().await`, live counters.
- `WriteContext { dialects, table, source_engine }`: what the SQL dump needs (§3.7).

Per-format rules: module docs of the writer files (CSV value text forms, JSON value forms
including exact decimals and 64-bit integers, XLSX typed cells, 32 767-character cell limit,
sheet splitting, constant-memory mode, Parquet types preserved and codec choice).

### 3.6 Encodings (`encoding`)

`detect_bom`, `bom_bytes`, `resolve(Auto, sample)`; `DecodingReader` (any supported encoding →
UTF-8; malformed bytes become U+FFFD and `had_replacements()` turns true, so the reader warns
once); `EncodingWriter` (UTF-8 → UTF-8 / UTF-16LE / UTF-16BE / Windows-1252 / Latin-1; an
unmappable character fails with `InvalidData` naming it); `CountingReader` (shared byte counter
for progress). Windows-1252 and Latin-1 differ in 0x80-0x9F (`€` vs U+0080); `encoding_rs`
treats the `latin1` label as Windows-1252, so `latin1` is decoded by oxbase itself.

### 3.7 SQL dump (`sql_dump`)

Export only (to import a dump, open it in the query editor and run it). `CREATE TABLE` from the
**target** engine's `DdlGenerator::create` (`DdlContext::new(version)`), `DROP` from
`drop_object` when asked, then multi-row `INSERT`s whose values are `Quoting::literal` of
`cell_value` (escaping is the target dialect's: PostgreSQL `E'...'` and `'\x..'::bytea`, MySQL
backslashes, SQL Server `N'...'` and `0x..`). Version: `targetVersion` parsed, else
`default_version(engine)` (PostgreSQL 18, MySQL 8.4, MariaDB 11.8, SQLite 3.45, SQL Server 17).
A table export uses its `TableDef` (keys, indexes, checks, comments); across engines the
engine-specific parts (native types, defaults, collations, generated expressions, extras) are
cleared and foreign keys dropped, with a `--` note. A query or result export builds the table
from the Arrow schema with `logical_type`. SQL Server inserts go in at most 1000-row `VALUES`
lists and `SET IDENTITY_INSERT` wraps them when an identity column is written. Statements end
with `;`, the file is UTF-8 with `\n` line ends and starts with a comment naming the target
(never connection details).

### 3.8 Formula injection (`formula`)

Text that starts with `=`, `+`, `-`, `@`, tab or CR is a formula to a spreadsheet. With
`formulaGuard` (default on): CSV writes it with a leading `'` (`guard_text`); XLSX writes it as a
string cell with the quote-prefix format (`needs_guard`). Data is never written with
`write_formula`. Numbers are never guarded. The guard is an option because the CSV change is
visible to non-spreadsheet consumers.

## 4. Bulk loading (`oxbase_dialect::bulk`, load stream)

### 4.1 The role

```rust
#[async_trait]
pub trait BulkLoader: Send + Sync {
    fn method(&self, version: &ServerVersion) -> BulkMethod;
    fn limits(&self, version: &ServerVersion) -> BulkLimits;
    async fn prepare(&self, session: LoadSession<'_>, target: &LoadTarget<'_>, cancel) -> Result<()>; // default: nothing
    async fn finish(&self, session: LoadSession<'_>, target: &LoadTarget<'_>, cancel) -> Result<()>;  // default: nothing
    async fn load(&self, session: LoadSession<'_>, target: &LoadTarget<'_>, batch: &RecordBatch,
                  cancel: CancellationToken) -> Result<u64>;
}
```

- **Load schema.** Batches arrive in `LoadTarget::schema()`: one column per `LoadColumn { name,
  data_type, native_type, nullable }` in order, with `load_arrow_type(data_type)`: `bool` →
  `Boolean`; `int` → `Int64` (unsigned 64-bit → `UInt64`); `float` → `Float64`; `decimal` →
  `Utf8` exact text; `char` / `varchar` / `text` / `enum` / `interval` / `array` / `other` / `json`
  → `Utf8`; `binary` / `varbinary` → `Binary`; `date` → `Date32`; `time` → `Time64(µs)`;
  `timestamp` → `Timestamp(µs)`; `timestamp_tz` → `Timestamp(µs, "+00:00")` (UTC instant);
  `uuid` → `FixedSizeBinary(16)`. `check_batch` verifies it. The import service converts and
  checks values (range, length, NOT NULL) **before** the loader; what remains is the server's
  own refusal.
- **One call per batch**, all rows or an error. The service re-chunks to about
  `limits.rows_per_call` rows and calls inside **its** transaction (`LoadSession { connection,
  tx }`); a loader never begins or commits. `prepare` / `finish` run once on the import's
  dedicated session, outside transactions, for session settings.
- **Values** are bound parameters (`bind_value`: decimal text → `Value::Decimal`, JSON text →
  `Value::Json`) or travel through the engine's bulk protocol; identifiers are quoted by the
  engine's `Quoting`. Never SQL literals.
- **Cancel**: the token is checked before each statement or protocol message; a cancelled load
  returns `Error::Cancelled` and the service rolls back.

### 4.2 Batch-size rules (`BulkLimits`)

`rows_per_statement(columns) = clamp(max_params / columns, 1, max_rows_per_statement)`; a
statement also stays under `max_statement_bytes` (estimated from the bound data, 8 bytes
overhead per value).

| Engine | Parameter limit | `max_params` used | Rows per statement | Bytes per statement |
|---|---|---|---|---|
| PostgreSQL | 65 535 (16-bit count) | 65 535 | 10 000 | - |
| MySQL / MariaDB | 65 535 placeholders | 65 535 | 10 000 | 16 MiB (`max_allowed_packet` default 64 MiB) |
| SQLite | 32 766 (`SQLITE_MAX_VARIABLE_NUMBER`) | 32 766 | 10 000 | - |
| SQL Server | 2 100 per RPC | 2 098 (`sp_executesql` uses 2) | 1 000 (`VALUES` limit) | - |

`rows_per_call` defaults to 50 000 for every engine.

### 4.3 Per-engine loaders

| Engine | Required method | Notes |
|---|---|---|
| PostgreSQL | `copy_text` (`copy_binary` optional) | `COPY <qualified> (<quoted cols>) FROM STDIN` through `tokio_postgres::Client::copy_in` on the session's client (`as_any` on the connection or the `Tx`). Text format: `\N` for NULL; `\\`, `\t`, `\n`, `\r` escaped; booleans `t` / `f`; `bytea` as `\\x<hex>`; timestamps ISO with `+00` for `timestamp_tz`; decimals and JSON as their text. Works for every type the server can parse from text (domains, enums, arrays). Cancel: drop the sink (the server aborts the `COPY`) and cancel the statement. |
| MySQL / MariaDB | `multi_row_insert` | `InsertLoader` with `BulkLimits::for_engine`. `prepare`: `SET time_zone = '+00:00'` so `TIMESTAMP` columns take UTC instants. **No `LOAD DATA LOCAL INFILE`**: it needs `local_infile=ON` on the server (off by default since MySQL 8.0) and a client that lets the *server* name the file to send, which a malicious or compromised server can abuse to read client files; it may come later as an opt-in method whose handler only streams the import's own rows and only when the server reports `local_infile`. Multi-row inserts reach about 100 000-200 000 rows/s on the CI fixture, enough for the exit check. Non-transactional storage engines (`MyISAM`, ...) keep rows of a failed call (`DmlWriter::transactional` says which); the import report says so. |
| SQLite | `prepared_insert` or `multi_row_insert` | Inside the service's transaction (`BEGIN` on the worker); one prepared `INSERT` reused for every row of the call (worker-side, `rusqlite` cached statement) or `InsertLoader` with 32 766 parameters. |
| SQL Server | `tds_bulk_load` (else `multi_row_insert`) | tiberius `bulk_insert_columns` (`INSERT BULK`) on the session's client inside the transaction; rows converted to the column types tiberius reports. If a type cannot go through bulk load (`sql_variant`, CLR types), fall back to `InsertLoader` for that table (method reported). Identity columns: `prepare` sets `IDENTITY_INSERT ON` (bulk: `KEEP_IDENTITY`), `finish` turns it off. |

### 4.4 Registration

`DialectRegistry::register_bulk_loader(engine, loader)` in each engine's `dialect::register`
(load stream). `bulk_loader(engine)` returns it, or **falls back** to an `InsertLoader` over the
engine's `Quoting` with `BulkLimits::for_engine(engine)` (so import works before the load stream
lands); `Unsupported` for engines without `Quoting`. `has_bulk_loader(engine)` is true only for a
registered loader; the load stream's tests assert it for its engines.

### 4.5 Type fidelity per engine (load stream tests, §12)

A round trip *file → import → export → file* must keep, per engine: integers (64-bit exact,
unsigned 64-bit on MySQL / MariaDB), decimals exactly (no float on the way: canonical text),
booleans (MySQL / MariaDB `tinyint(1)`, SQLite integers, SQL Server `bit`), dates, times with
microseconds (SQL Server `datetime2(6)`, MySQL `datetime(6)`), `timestamp_tz` as the same instant
(PostgreSQL `timestamptz`; MySQL `TIMESTAMP` via the UTC session; SQL Server `datetimeoffset` at
`+00:00`; SQLite text), UUIDs (PostgreSQL `uuid`, SQL Server `uniqueidentifier`, others text /
`binary(16)` as declared), binary data, JSON (PostgreSQL `jsonb` normalizes key order and
spacing: compared as parsed JSON), and text with quotes, backslashes, tabs, CR / LF, NUL-free
Unicode including astral characters (MySQL / MariaDB need `utf8mb4` columns; SQL Server
`nvarchar`).

## 5. Import and export services (`oxbase-services`, services stream)

### 5.1 Import pipeline

`ImportService::preview(&ImportSource, cancel)` → `plan(&ImportSource, &ImportRequest, cancel)`
→ `start(ImportSource, ImportRequest) -> ImportHandle { id, events }` → `cancel(id)`.
`ImportSource { path, options }` never leaves Rust. `ImportRequest` (JSON camelCase):

```json
{
  "options": {"format": "csv"},
  "columns": [{"name": "id", "dataType": {"type": "int", "bits": 64, "unsigned": false}}],
  "mapping": [{"source": 0, "target": "id"}],
  "target": {"session": "<uuid>", "table": {"kind": "table", "schema": "public", "name": "t"},
             "mode": "append"},
  "primaryKey": [],
  "conversion": {"trimText": false, "emptyTextAsNull": false, "truncateText": false},
  "errors": {"type": "skip", "maxErrors": 100},
  "transaction": {"type": "batches", "rows": 100000}
}
```

Plan checks (all before anything changes): every mapped target exists once (create / replace:
the new table from the mapping, `native_type` empty so the target `DdlGenerator` derives types,
primary key from `primaryKey`); no generated column is written; identity `ALWAYS` columns are
refused, `BY DEFAULT` / auto-increment / SQL Server identity allowed; every NOT NULL column
without a default is mapped; every type pair has a conversion (`conversion_note`: anything →
text / json; text → anything by parsing; int / float / decimal with range checks; date →
timestamp; timestamp → date with a note; timestamp ↔ timestamp_tz in UTC; bool ↔ int; uuid ↔
text; binary ↔ hex text; no conversion → `InvalidInput` naming the column). The `ImportPlan`
lists the DDL, columns with conversion notes, the bulk method, the resolved transaction policy,
`destructive` and warnings.

Run: on a **dedicated child session** (`open_child` on the table's database, closed at the end);
target-mode DDL; `oxbase_io::open` + `spawn_reader` (4 batches buffered); per batch: conversion
options, cast to the load schema with checks, row issues (reader + conversion) through the error
policy (`filter_record_batch` drops skipped rows), re-chunk to `rows_per_call`,
`BulkLoader::load` in the current transaction, commit per policy.

### 5.2 Target modes, transactions, what a cancel leaves

`TransactionPolicy::Auto` = one transaction up to 64 MiB of file, else commits every 100 000
rows; `Single`; `Batches { rows }` (commit at the end of the loader call that crosses each
multiple).

| Mode | Single transaction, on cancel / failure | Commit every N, on cancel / failure |
|---|---|---|
| `append` | nothing inserted | committed batches stay (`rowsCommitted`) |
| `truncate` | PostgreSQL / SQL Server / SQLite: rolled back. MySQL / MariaDB: `TRUNCATE` committed, table **empty** | table holds the committed batches |
| `delete_all` | rolled back (`DELETE` in the transaction) | table holds the committed batches |
| `create` | PostgreSQL / SQL Server / SQLite: `CREATE` rolled back. MySQL / MariaDB: service **drops** the table it created | service **drops** the table it created |
| `replace` | PostgreSQL / SQL Server / SQLite: old table intact. MySQL / MariaDB: old table **gone**, new one dropped | old table **gone**, new one dropped |

`ImportReport.leftBehind` states which case happened; `tableCreated` / `tableDropped` too.
`destructive` plans (truncate on MySQL / MariaDB, replace outside one transaction) need a
confirmation in the UI. **Cancel ends the import with `Cancelled` within 2 s.**

### 5.3 Errors, progress, events

Row issues: `stop` fails at the first; `skip { maxErrors }` skips and records (row, column,
message) and fails past `maxErrors`. At most 1000 errors in the report (`errorsTruncated`), the
first 100 also as `rowError` events. Server errors fail the import whatever the policy, naming
the source row range of the failing call. Events (`{"type": ...}` camelCase): `started
{importId, totalBytes, plan}`, `progress {bytesRead, totalBytes, rowsRead, rowsInserted,
rowsCommitted, rowsSkipped, elapsedMs}` at most every 250 ms and after each commit (lossy),
`rowError {error}`, `committed {rowsCommitted}`, then exactly one of `finished {report}`,
`failed {error, report}`, `cancelled {report}`.

### 5.4 Export

`ExportService::start(ExportRequest, ExportDestination { path, overwrite }) -> ExportHandle`,
`cancel(id)`. Sources (`{"type": ...}`): `table {session, query: TableDataQuery}` (the M3 filter
and sort with bound values, streamed on a dedicated child session; the `TableDef` goes to the
writer), `query {session, sql, database?}` (exactly one statement, first result set, dedicated
child session, no history), `result {runId, resultIndex}` (every row of the run's
`ResultStore`, **spilled rows included**, paged with `QueryService::page`; `InvalidInput` while
the run is running; a truncated result is exported as kept and flagged `sourceTruncated`). Rows
stream into a `WriterHandle`; the file appears only on success. Events: `started {exportId,
totalRows?}`, `progress {rows, bytesWritten, totalRows?, elapsedMs}`, then `finished {report}`,
`failed {error, elapsedMs}` or `cancelled {elapsedMs}` (within 2 s; no file left).
`ExportReport { exportId, fileName, rows, bytes, sheets?, warnings?, sourceTruncated, elapsedMs }`.

## 6. File handles (`crates/app-tauri/src/files.rs`)

- `file_pick_open { filters, title? } -> PickedFile | null` and `file_pick_save { defaultName,
  filters, title? } -> PickedFile | null` show the **native dialog from Rust**
  (`tauri_plugin_dialog::DialogExt`, `pick_file` / `save_file` with a one-shot channel), so the
  webview gets **no new dialog, fs or shell permission**. `file_release { handle } -> bool`.
- `PickedFile { handle, name, access: "read" | "write", size?, exists }`: `name` is the base
  name only; the path stays in `FilesState`.
- Read handles are reusable (preview, plan, run); write handles are used up by `export_run`. A
  write handle may overwrite only if the file existed when picked (the save dialog asked); a file
  that appears later is never overwritten. At most 64 handles (oldest dropped); memory only.
- The webview chooses only filters (1-16 extensions of ASCII letters / digits, or `*`), the
  title, and a save dialog's suggested **base name** (no `/`, `\`, `:`, control characters,
  `.` or `..`).
- Error messages name files by base name only (`oxbase_io::file_label`).

## 7. Tauri commands and TS (`ui/src/api/io.ts`)

| Command | Arguments (camelCase) | Returns |
|---|---|---|
| `file_pick_open` | `filters: FileFilter[]`, `title: string \| null` | `PickedFile \| null` |
| `file_pick_save` | `defaultName`, `filters`, `title` | `PickedFile \| null` |
| `file_release` | `handle` | `boolean` |
| `import_preview` | `file`, `options: ReadOptions`, `requestId` | `SourcePreview` |
| `import_plan` | `file`, `request: ImportRequest`, `requestId` | `ImportPlan` |
| `import_run` | `file`, `request`, `onEvent: Channel<ImportEvent>` | `ImportId` |
| `import_cancel` | `importId` | `boolean` |
| `export_run` | `file` (write handle), `request: ExportRequest`, `onEvent: Channel<ExportEvent>` | `ExportId` |
| `export_cancel` | `exportId` | `boolean` |

`requestId` cancels a preview or plan with the existing `cancel_request`. Errors are the usual
`{category, message}`. Each command has an `allow-<command>` permission (`build.rs`,
`capabilities/default.json`). TS wrappers: `filePickOpen`, `filePickSave`, `fileRelease`,
`importPreview`, `importPlan`, `importRun`, `importCancel`, `exportRun`, `exportCancel`,
`filtersFor(format)`, `isTerminalTransferEvent`; types mirror the Rust payloads
(`ui/src/api/io.test.ts`).

## 8. Fixtures (`tests/fixtures.toml` `[m5]`) and sample files

No seed objects and no init scripts: tests create scratch tables `m5_t_<case>_<token>` in
`[m5.<engine>]` (PostgreSQL `oxbase_test.public`, MySQL / MariaDB `oxbase_test`, SQL Server
`oxbase_test.dbo`, SQLite a temp file), drop them in a guard, and drop stale ones (> 1 h).
`roundtrip_rows = 10000`.

Committed samples in `tests/data/m5/` (all under 200 KB, `.gitattributes` `-text`):
`basic.csv` (quotes, embedded delimiters / quotes / LF / CRLF, Unicode with an emoji, NULLs,
formula-looking text), `crlf.csv`, `bom_utf8.csv`, `utf16le_bom.csv`, `utf16be_bom.csv`,
`windows1252.csv`, `latin1.csv`, `empty.csv`, `header_only.csv`, `ragged.csv`,
`duplicate_headers.csv`, `no_header.csv`, `semicolon_decimal_comma.csv`, `null_tokens.csv`,
`basic.tsv`, `wide.csv` (300 columns), `long_field.csv` (100 000-character field),
`unterminated_quote.csv`, `array.json` (nested objects / arrays, a 2^53 + 1 integer, a key that
appears late), `lines.ndjson`, `empty_array.json`, `not_objects.json`, `utf16le_bom.json`,
`truncated.json`. `[m5.files."<name>"]` holds the expected rows, columns, types, values, issues
and encodings per file. XLSX and Parquet samples are built at runtime (`[m5.generated]`:
dates, booleans, formulas with cached values, an error cell, decimals, timestamps with a zone,
lists, binary, NULLs).

## 9. Exit check: 1 GB CSV import (perf stream)

### 9.1 Generator (deterministic, in the test crate)

Rows `i = 1..=rows` (`[m5.perf] rows = 10 000 000`), header
`id,code,qty,amount,price,active,born,at,note`, written with the `csv` crate (`QuoteStyle::
Necessary`, LF), UTF-8:

| Column | Value | Target type (create mode) |
|---|---|---|
| `id` | `i` | `int(64)` primary key |
| `code` | `C` + base-36 of `(i * 7919) % 36^8`, zero-padded to 8 | `varchar(16)` |
| `qty` | empty (NULL) when `i % 17 == 0`, else `(i * 31) % 10007 - 5000` | `int(32)` |
| `amount` | `((i * 7907) % 10 000 000) / 100` with 2 decimals, negative when `i % 11 == 0` | `decimal(12, 2)` |
| `price` | `(i % 1000) as f64 / 7.0` in Rust's shortest form | `float(64)` |
| `active` | `true` when `i % 3 == 0`, else `false` | `bool` |
| `born` | 1970-01-01 + `(i * 37) % 20 000` days | `date` |
| `at` | 2000-01-01 00:00:00 + `i * 7` s + `i % 1 000 000` µs, `YYYY-MM-DD HH:MM:SS.ffffff` | `timestamp(6)` |
| `note` | empty (NULL) when `i % 13 == 0`, else fragment `i % 8` + ` #` + `i`: plain ASCII; with `,`; with `"`; with LF; with CRLF; `naïve café – 日本語 🚀`; starting with `=`; 60 × `x` | `text` (SQL Server `nvarchar(max)`, MySQL `utf8mb4`) |

The generator returns the six checksum numbers (count, Σ id, Σ qty, count of notes, Σ note length
in code points, or UTF-16 units for SQL Server, Σ amount in cents) and the file size; the test
fails when the file is under `min_bytes` (1 000 000 000). The perf stream may adjust fragment
lengths to land between 1.0 and 1.3 GB, keeping the checksum definitions.

### 9.2 Assertions (per engine of `[m5.perf] engines`: PostgreSQL and MySQL mandatory)

1. **Import** into `m5_perf` (create mode, `Batches { rows: 100 000 }`, `ErrorPolicy::Stop`)
   finishes with `Finished`; `rowsRead == rowsCommitted == rows`, no row errors.
2. **Checksum**: `[m5.perf.checksum].<engine>` returns exactly the generator's numbers.
3. **Progress**: `bytesRead` and the row counters never decrease; consecutive `progress` events
   at most 1.5 s apart or at most 1% of the bytes apart; the last one before `Finished`
   reports every byte.
4. **Memory**: Linux `VmRSS` grows at most 512 MiB over its value right before the import.
5. **Cancel**: a second import (append into a pre-created `m5_perf_cancel`, `Batches { 100 000 }`)
   is cancelled once `bytesRead` passes 25%; `Cancelled` arrives within 2000 ms of
   `ImportService::cancel`; the table then holds exactly `report.rowsCommitted` rows (> 0), which
   is the documented leftover of §5.2.
6. The test prints rows/s, MB/s, the peak RSS growth, the progress gap and the cancel latency
   per engine (the integration step copies them into the README).

### 9.3 Where it runs

A new CI job **`import-perf`** (perf stream, `.github/workflows/ci.yml`), not the conformance
job: it needs a release-like build (`--profile perf`: release without LTO, so the 1 GB parse is
not measured at opt-level 0) and 10-15 minutes the fast lane should not pay on every push. It
runs on the **full lane** and on the fast lane of `wip/*-perf` and `wip/*-integrate` branches
(like the webview smoke job). Steps: build `cargo test -p oxbase-services --profile perf --test
import_perf --no-run`; `scripts/fixtures-certs.sh`; start **only** the `postgres` and `mysql`
fixtures (`scripts/ci-fixtures.sh up postgres mysql`, a service-list argument the perf stream
adds); run with `OXBASE_PERF_DIR=${{ runner.temp }}/m5-perf` and `--include-ignored
--test-threads 1`; delete the CSV; tear the fixtures down. Disk: about 1.2 GB CSV + about 2.5 GB
of table, index and WAL / redo per engine on a runner with about 14 GB free; tables are dropped
after each engine. Time budget: 30 minutes (timeout 45).

## 10. Invariants every stream keeps

1. **Bounded memory**: stream everything; a reader holds one batch plus fixed buffers, a writer
   one batch plus its format's buffers (Parquet row group, XLSX constant-memory row), the import
   at most `reader_capacity` + 1 batches, the export `writer_capacity` + 1. Documented floors:
   XLSX shared-strings table; one JSON element; one CSV field (capped at 64 MiB).
2. **Cancel**: every long operation takes a `CancellationToken`; import and export end within 2 s
   of a cancel with their terminal event.
3. **Cleanup**: an export leaves no file on failure or cancel and never damages an existing one
   (`SinkFile`); an import leaves the table as §5.2 says, and the report says it.
4. **Formula injection** protection on by default for CSV and XLSX output (§3.8).
5. **Handles, not paths**: no command takes a path from the webview; dialogs run in Rust; no new
   webview permission (§6).
6. **Encoding fidelity**: decoding never fails silently (replacements are warned about);
   encoding fails on an unmappable character instead of writing `?`; BOMs only when asked.
7. **Type fidelity**: decimals stay exact text end to end, 64-bit integers stay integers, instants
   stay instants (§4.5); JSON writers write decimals and big integers without going through
   `f64`.
8. **Quoting and literals**: identifiers through the engine's `Quoting`; values bound or sent
   by the bulk protocol; SQL dump literals escaped by the target dialect.
9. **Secrets and paths**: no secret in any message, report or file; messages name files by base
   name only.
10. **No engine special cases outside engine crates**: per-engine behaviour comes from
    `BulkLoader`, `DdlGenerator`, `Quoting` and `Capabilities`.

## 11. Dependencies (added by Design; licenses checked)

All in `[workspace.dependencies]`, used by `oxbase-io` (and the services / app through it):

| Crate | Version | License | Notes |
|---|---|---|---|
| `arrow-json` | 60.1.0 | Apache-2.0 | JSON writer; same arrow major as `arrow-array` 60 |
| `arrow-cast` | 60.1.0 | Apache-2.0 | display / cast helpers for writers and Parquet reading |
| `parquet` | 60.1.0 | Apache-2.0 | `default-features = false`, features `arrow`, `snap`, `lz4`, `zstd`, `brotli`, `flate2-rust_backend` |
| `csv` | 1.4.0 | Unlicense OR MIT | CSV / TSV reader and writer |
| `encoding_rs` | 0.8.44 | (Apache-2.0 OR MIT) AND BSD-3-Clause | UTF-16 and Windows-1252 |
| `calamine` | 0.37.0 | MIT | feature `dates`; streaming `worksheet_cells_reader` |
| `rust_xlsxwriter` | 0.102.0 | MIT OR Apache-2.0 | features `constant_memory`, `chrono` |

New transitive crates and licenses: `snap` (BSD-3-Clause, pure Rust), `lz4_flex` (MIT, pure
Rust), `zstd` / `zstd-safe` / `zstd-sys` (BSD-3-Clause; C compiled with `cc`, builds on Linux,
macOS and Windows; `jobserver` MIT OR Apache-2.0), `brotli` (already in the graph), `flate2` with
the `miniz_oxide` backend (already in the graph), `zip` 9.2.1 (MIT, `deflate` only),
`zopfli` (Apache-2.0), `typed-path`, `codepage`, `atoi`, `atoi_simd`, `debug_unsafe`,
`fast-float2`, `lexical-*`, `seq-macro` (MIT and / or Apache-2.0), `ryu` (Apache-2.0 OR BSL-1.0),
`csv-core` (Unlicense OR MIT), `arrow-ord` (Apache-2.0). No OpenSSL, native-tls or aws-lc;
`deny.toml` needs no change (cargo-deny bans and licenses green in CI). Workspace-internal:
`oxbase-dialect` gains `arrow-array`, `arrow-schema`, `futures`; `oxbase-driver` gains `chrono`,
`uuid`; `oxbase-services` depends on `oxbase-io`; `app-tauri` on `oxbase-io` and `tokio`.
`[profile.perf]` (release without LTO) is for §9.

## 12. Exit-check tests the streams must write

- **read** (`crates/oxbase-io/tests/read_*.rs`, every OS): every `[m5.files.*]` expectation
  (rows, columns, names, types, values, issues, encodings, BOM, errors); generated XLSX and
  Parquet samples; preview limits (`sample_rows`, `sample_bytes`, `display_rows`); a reader
  over a 50 000-row generated CSV never holds more than one batch (`batch_rows`) and reports
  monotonic `bytes_read` ending at the file size; type inference rules of §3.4 as unit tests;
  overrides (a column forced to `text`, an `int` column with a bad value → row issue).
- **write** (`crates/oxbase-io/tests/write_*.rs`): every format round-trips a batch with every
  canonical type and NULLs back through the readers (decimals exact, 2^53 + 1 exact, Unicode,
  CR / LF / quotes in text); CSV encodings and BOM; formula guard on and off (CSV and XLSX);
  XLSX sheet splitting with a small `maxRowsPerSheet`; Parquet with each codec; SQL dump for
  every target engine (text checked; the services stream runs the PostgreSQL and SQLite dumps
  against a database); abort leaves no file; existing destination untouched on failure.
- **load** (`crates/oxbase-<engine>/tests/bulk_live.rs`, conformance job): `has_bulk_loader`;
  a 100 000-row load of every canonical type into a scratch table and read back equal (§4.5);
  limits respected (a 2 000-column-param batch on SQL Server, a 70 000-parameter batch on
  MySQL split); cancel mid-load returns `Cancelled` and the rolled-back transaction leaves no
  row; a constraint violation fails the call.
- **services** (`crates/oxbase-services/tests/{import,export}_live.rs`, conformance job, all five
  engines): every target mode × single / batches with cancel and failure, asserting §5.2 per
  engine; error policies; plan rejections; export of a table (filter + sort), a query and a
  spilled result (a 300 000-row run with an 8 MiB memory budget) to every format, read back
  equal; export cancel leaves no file; overwrite refused without `overwrite`.
- **app** (Vitest): the wizard (format options, preview grid, mapping with skips and types,
  target mode with the destructive confirmation, progress, report) and export from the grid
  and the tree, against mocked commands; the webview smoke test gains an export of a result to
  CSV and an import of `tests/data/m5/basic.csv` only if cheap (dialogs cannot be driven by
  WebDriver; a test-only handle injection is not allowed in release builds).
- **perf** (§9): `crates/oxbase-services/tests/import_perf.rs`, job `import-perf`.

## 13. Deviations (as built)

None yet. Streams record deviations from this file here, with the reason.
