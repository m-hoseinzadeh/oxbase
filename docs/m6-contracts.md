# M6 contracts

Status: frozen for M6 on 2026-11-01 (Design step). M6 exit check (PLAN.md §6): **completion
under 50 ms on a 5k-table schema**. Scope: cursor context, schema-aware completion,
diagnostics, hover, a formatter and snippets in the SQL editor, for PostgreSQL, MySQL, MariaDB,
SQLite and SQL Server, backed by a per-connection schema identifier cache.

Changes to these contracts go through the Design owner. Streams code against them and must not
change public signatures; adding private helpers, private fields and tests is always fine. Rust
doc comments on the items named here are part of the contract and are more detailed than this
file (start with the crate docs of `oxbase_insight` and `oxbase_schema_cache`, the module docs
of `oxbase_services::schema_cache` and `oxbase_services::insight`, `oxbase_dialect::catalog`,
`oxbase_dialect::language`, `oxbase_types::catalog`, `oxbase_types::snippet` and the M6 section
of `oxbase_sql`).

## 1. Who owns what

Bodies marked `unimplemented!("m6: <stream>")` belong to that stream. No test may call one until
its stream has filled it. Six streams start from the Design head in parallel.

| Area | Path | Owner | State at hand-off |
|---|---|---|---|
| Token API (`tokenize`, `SqlToken`, `ident_value`), `tokens_equivalent`, `schema_changes`, `utf16_offset_from_byte` | `crates/oxbase-sql/src/{tokens,ddl,lib}.rs` | Design | done, tested |
| `CatalogSnapshot` and friends, `IdentCase` / `CaseRules`; `Snippet`, `SnippetInput`, `validate_snippet_body` | `crates/oxbase-types/src/{catalog,snippet}.rs` | Design | done, tested |
| `CatalogReader` / `CatalogScope` role; `SqlLanguage` data type + `SqlLanguage::standard`; registry `register_catalog_reader` / `catalog_reader` / `register_language` / `language` / `missing_insight_roles` | `crates/oxbase-dialect/src/{catalog,language,registry}.rs` | Design | done, tested |
| `SchemaCache` (index, search, resolution, merge, memory estimate), `fuzzy_score`, `codec`, `synthetic` | `crates/oxbase-schema-cache/**` | Design (cache may tune internals) | done, tested |
| App DB migration 2; `SnippetRepository`, `SchemaCacheRepository` (memory + SQLite) | `crates/oxbase-store/src/{app_db,insight}.rs` | Design | done, tested |
| `oxbase_insight` types: `Dialect`, `Outline` and friends, `CursorContext`, `Completion*`, `Diagnostic*`, `Hover`, `FormatOptions`, `check_format`; built-in snippets | `crates/oxbase-insight/src/{lib,snippets}.rs` + types in every module | Design | done, tested |
| `SnippetService`; `RequestGate`; insight payloads and UTF-16 conversions; `SchemaEvents`, `CacheStatus` & co., `SchemaCacheService::new` / `key_for` | `crates/oxbase-services/src/{snippets,insight,schema_cache}.rs` | Design | done, tested |
| Tauri commands (registration, ACL, capabilities, thin bodies), backend wiring, TS types and wrappers | `crates/app-tauri/{build.rs,capabilities/default.json,src/{insight,backend,lib}.rs}`, `ui/src/api/insight.ts` | Design | done, tested (bodies call services that are still skeletons) |
| Per-engine **catalog readers** + registration; `SchemaCacheService` bodies (memory, persistence, build on a child session, refresh, invalidation, LRU); publishing `SchemaChangeEvent`s from the designer, import and query services; snapshot removal when a profile is deleted; live tests incl. the 5k-table PostgreSQL variant | `crates/oxbase-{pg,mysql,sqlite,mssql}/src/dialect/catalog.rs` (+ the `register` line), `crates/oxbase-services/src/schema_cache.rs` (+ `schema_cache/**`), the publishing call sites in `crates/oxbase-services/src/{designer,import,query,profiles}.rs`, `crates/oxbase-services/tests/schema_cache_live.rs`, `crates/oxbase-services/tests/schema_cache_perf.rs`, `crates/oxbase-schema-cache/src/**` (internals only) | **cache** | skeleton |
| `outline`, `context_at` | `crates/oxbase-insight/src/{outline,context}.rs` (+ private files under `src/parse/`), `crates/oxbase-insight/tests/{context,outline,fuzz}.rs` | **context** | skeleton |
| `complete`; `InsightService` bodies; the synthetic perf test and the `insight-perf` CI job | `crates/oxbase-insight/src/complete.rs` (+ `src/complete/**`), `crates/oxbase-insight/tests/{complete,complete_perf}.rs`, `crates/oxbase-services/src/insight.rs` (+ `insight/**`), the `insight-perf` job of `.github/workflows/ci.yml` | **complete** | skeleton |
| `diagnose`, `hover`; the valid-SQL corpus | `crates/oxbase-insight/src/{diagnose,hover}.rs` (+ `src/diagnose/**`), `crates/oxbase-insight/tests/{diagnose,hover}.rs`, `crates/oxbase-insight/tests/corpus/*.sql` | **diagnose** | skeleton |
| `format`, `format_range`; per-engine **`SqlLanguage` data** + registration | `crates/oxbase-insight/src/format.rs` (+ `src/format/**`), `crates/oxbase-insight/tests/format.rs`, `crates/oxbase-{pg,mysql,sqlite,mssql}/src/dialect/language.rs` (+ the `register` line) | **format** | skeleton (standard data works today) |
| Monaco providers (completion, hover, diagnostics markers, format action, snippet UI), cache status and refresh in the UI, debounce and sequencing, closing an editor; Tauri command bodies stay thin (bug fixes only); webview smoke additions | `ui/src/**` except `ui/src/api/**` (payload additions there only with Design), `crates/app-tauri/src/insight.rs` (bug fixes), `ui/e2e/**`, `scripts/e2e-*.sh` | **app** | commands done (§7), no UI |
| Contract, PLAN, README, fixtures `[m6]` | `docs/m6-contracts.md`, `PLAN.md`, `README.md`, `tests/fixtures.toml` | Design | done |

Two streams touch each engine crate: **cache** adds `dialect/catalog.rs`, **format** adds
`dialect/language.rs`; each adds one line to that crate's `register` (merge conflicts there are
trivial). The registry reports both with `DialectRegistry::missing_insight_roles(engine)`, which
must be empty for PostgreSQL, MySQL, MariaDB, SQLite and SQL Server at integration.

Streams add no crate to `Cargo.lock`: M6 needs **no new external dependency** (§10). A stream
that needs one adds it with an explicit version to its own crate's `Cargo.toml`, checks the
license against `deny.toml` and reports it for hoisting.

**Live tests** are `#[ignore]`d and read `tests/fixtures.toml`. The conformance job already runs
`-p oxbase-pg -p oxbase-mysql -p oxbase-sqlite -p oxbase-mssql -p oxbase-services` with
`--include-ignored`, so the catalog reader and cache live tests run there with no CI change.

Integration (after the streams): every engine registers its catalog reader and language data;
the designer, import and query services publish schema changes; the `insight-perf` job is green
with `OXBASE_REQUIRE_M6_PERF=1`; PLAN / README get the *as built* notes.

## 2. Parsing approach and the token API

### 2.1 Evaluation

PLAN.md §2 named tree-sitter for cursor context. The candidates, checked on 2026-11-01:

| | tree-sitter + `tree-sitter-sequel` 0.3.11 (DerekStride grammar) | tree-sitter T-SQL forks (`tree-sitter-sequel-tsql` 0.4.2, `tree-sitter-mssql` 0.2.3) | `sqlparser` 0.63.0 | Hand-written tolerant parser over the `oxbase-sql` lexer |
|---|---|---|---|---|
| License | MIT (+ `tree-sitter` 0.27.1 MIT) | MIT | Apache-2.0 | ours |
| Maintenance | last release 2025-10; 1.6 M downloads | one person each, 8 k / 0.5 k downloads | active (Apache DataFusion) | ours |
| Dialects | one generic grammar with some PostgreSQL / MySQL forms; no `[bracket]` identifiers, no MySQL `#` comments, block comments do not nest, backtick identifiers cannot hold spaces, identifiers limited to `[A-Za-z_À-ſ]` (no Cyrillic, Greek, CJK, Arabic or Persian names) | T-SQL only | PG, MySQL, SQLite, MS SQL dialect flags | exactly the five lexers the splitter already uses (dollar quotes, nested comments, `DELIMITER`, `GO`, `N'...'`, brackets, backticks, any Unicode identifier) |
| Incomplete SQL | generic error recovery: `SELECT a. FROM t` or `SELECT * FROM` becomes an `ERROR` node whose shape depends on the parse table, so the cursor context has to be guessed from error nodes | same | no recovery: the first error aborts the parse | designed for it: clause-level recovery, every prefix of a statement gives a usable outline |
| Size / build | `parser.c` 16.6 MB (674 k lines, 17 329 states) compiled with `cc` on every OS; C code in the process | 2.5 / 1.7 MB crates, same build model | pure Rust, ~0.5 MB | pure Rust, no new dependency |
| Agreement with the splitter | a second lexer that disagrees with ours on comments, quotes and statement ends | same | a second lexer | the same tokens by construction |

**Decision: a tolerant, hand-written parser on top of the `oxbase-sql` lexer** (no tree-sitter,
no `sqlparser` in M6). Reasons, in order: cursor-context accuracy on incomplete SQL (the
normal case) is a property we can design and test directly instead of reverse-engineering error
nodes; one lexer for splitter, insight and formatter, so a string, comment or statement boundary
never means two things; full Unicode identifiers; T-SQL and MySQL quoting the grammar lacks; no
16 MB of generated C to build on three OSes; no new dependency. Insight needs the *structure* of
a statement (clauses, relations, aliases, CTEs, subqueries, references), not a full AST, which
keeps the parser small. `sqlparser` stays an option for later features that need a full AST of
complete statements (AI dialect conversion, M12); it is not used to report errors, because its
gaps in dialect coverage would become false errors (§4.5).

### 2.2 Token API (`oxbase-sql`, done)

- `tokenize(dialect, text) -> Tokens` yields `SqlToken { kind, start, end }` covering the text
  without gaps; kinds `Space`, `LineComment`, `BlockComment`, `ExecComment`, `String { inner_start,
  inner_end }`, `QuotedIdent { quote }`, `Word`, `Punct`; `is_trivia`, `is_terminated`
  (unterminated strings, identifiers and comments run to the end of the text). Same rules as the
  splitter (crate docs, *Dialect rules*); it knows no client directives (`GO`, `DELIMITER` are
  ordinary words), so callers cut statements with `split` first.
- `ident_value(token_text)` unquotes a quoted identifier (`"a""b"` → `a"b`, `[x]]y]` → `x]y`).
- `utf16_offset_from_byte` complements `byte_offset_from_utf16`.
- `tokens_equivalent` and `schema_changes`: §4.6 and §3.5.

## 3. Schema cache

### 3.1 Snapshot model (`oxbase_types::catalog`)

`CatalogSnapshot { format, engine, server_version, database, databases, default_schema,
search_path, case_rules, schemas, read_at_ms, truncated }`; `CatalogSchema { name, system,
relations, routines, sequences, types }`; `CatalogRelation { name, kind (table, view, mat_view,
foreign, temporary), columns, foreign_keys, comment }`; `CatalogColumn { name, native_type,
nullable, primary_key }`; `CatalogForeignKey { columns, ref_schema, ref_table, ref_columns }`;
`CatalogRoutine { name, kind (function, procedure, aggregate), args, returns }`. Engines without
schemas have one `CatalogSchema` named after the database (MySQL, MariaDB) or `main` plus each
attached database (SQLite). `normalize()` sorts by name; columns keep ordinal order.

Identifier case (`IdentCase`, per name class in `CaseRules { schemas, relations, columns,
routines }`):

| Engine | Rule (default; the reader refines it) |
|---|---|
| PostgreSQL | `FoldLower` everywhere: unquoted names fold to lower case, quoted ones match exactly |
| MySQL / MariaDB | databases and tables `Sensitive` when `lower_case_table_names = 0`, `Insensitive` for 1 and 2; columns and routines `Insensitive` |
| SQLite | `Insensitive` (ASCII; quoted or not) |
| SQL Server | `Insensitive` with a `_CI_` database collation (default), `Sensitive` with `_CS_` |

Matching for *completion* is always case-insensitive (typing `ord` offers `Orders`); the rules
decide *resolution* (`o.` → which table) and *quoting* (`Dialect::quote_name`: `"Orders"` on
PostgreSQL, `Orders` on SQL Server).

### 3.2 Catalog readers (`oxbase_dialect::catalog`, cache stream)

`CatalogReader::read(conn, &CatalogScope, cancel) -> CatalogSnapshot`, registered per engine
with `register_catalog_reader`. A full read makes a **fixed number of catalog queries**
independent of the table count (never one per table), with bound parameters for scope names.
`CatalogScope { schemas, relations, include_system, max_columns }` (default: everything but
system schemas, 400 000 columns). Sources (suggested; the engine's documented catalogs):

- PostgreSQL: `pg_namespace`; `pg_class` (`relkind` `r p v m f`, `S` for sequences);
  `pg_attribute` with `format_type`; `pg_constraint` (`p` keys, `f` foreign keys via `conkey` /
  `confkey`); `pg_proc` with `pg_get_function_identity_arguments` / `pg_get_function_result`
  and `prokind` (11+; `proisagg` before); `pg_type` (`typtype` `e d c` without table row types);
  `current_schemas(false)` for the search path; `pg_database` (`datallowconn`) for databases.
- MySQL / MariaDB: `information_schema.SCHEMATA`, `TABLES`, `COLUMNS` (`COLUMN_KEY = 'PRI'`),
  `KEY_COLUMN_USAGE` (`REFERENCED_TABLE_NAME`), `ROUTINES` + `PARAMETERS`; MariaDB 10.3+
  sequences (`TABLE_TYPE = 'SEQUENCE'`); `@@lower_case_table_names`.
- SQLite: `pragma_database_list`; per database one query joining `sqlite_schema` with
  `pragma_table_xinfo(name)` and one with `pragma_foreign_key_list(name)` (table-valued
  functions, so still a fixed number of queries); no routines.
- SQL Server: `sys.schemas`, `sys.objects` (`U`, `V`; `FN IF TF` functions, `P` procedures),
  `sys.columns` with `TYPE_NAME` and length / precision / scale, `sys.index_columns` of the
  primary key, `sys.foreign_key_columns`, `sys.parameters`, `sys.sequences`, `sys.types`
  (`is_user_defined`), `SCHEMA_NAME()`, `DATABASEPROPERTYEX(DB_NAME(), 'Collation')`,
  `sys.databases` (`HAS_DBACCESS`).

### 3.3 Persistence (`oxbase-store`, done)

App DB migration 2 adds `snippets` and `schema_cache_snapshots(profile_id, database_name,
format, saved_at_ms, snapshot BLOB, PRIMARY KEY (profile_id, database_name))`.
`SchemaCacheRepository { load, save, delete(profile, Option<database>) }` with SQLite and memory
implementations; `StoredSnapshot`'s `Debug` omits the bytes. The bytes are
`oxbase_schema_cache::codec::encode` (format 1: the snapshot's JSON); `decode` rejects another
`CatalogSnapshot::FORMAT` (the service then drops it and reads the catalog again). Snapshots over
`codec::MAX_SNAPSHOT_BYTES` (64 MiB) are not persisted. A profile's snapshots are deleted with
the profile (cache stream: `profiles.rs` calls `SchemaCacheService::forget_profile`). Snapshots
hold names, types and comments only, never secrets or data.

### 3.4 Index and memory (`oxbase-schema-cache`, done)

`SchemaCache::new(snapshot)` builds an immutable index (`Arc`, cheap clone) with a process-wide
`generation`. `search(&NameQuery { pattern, mode: Prefix | Fuzzy, kinds, schema, relation, limit
})` returns `NameHit { target, score }` best first (prefix: 1 000 for an exact match, else
900 minus the extra characters; fuzzy: `fuzzy_score`, prefix and word starts first);
`resolve_schema`, `resolve_relation` (search path first, PostgreSQL semantics; `db.schema.name`
checked against the snapshot's database), `relations_named` (lenient, any schema),
`resolve_routine`, `resolve_column`, `name_of(target)`, `merge(partial, scope)` (a new cache with
the scoped schemas / relations replaced or removed), `heap_bytes()`.

Memory budget: `heap_bytes() < MEMORY_BUDGET_BYTES` (64 MiB) for the exit-check catalog (5 000
relations x 20 columns, 500 routines; unit test `the_exit_check_catalog_fits_the_memory_budget`).
`synthetic::snapshot(&SyntheticSpec)` builds deterministic catalogs of any size for tests.

### 3.5 Service, invalidation and events (`oxbase_services::schema_cache`, cache stream)

`CacheKey { profileId, database }`. `SchemaCacheService::new(manager, dialects, repo, events,
config)` (done); `key_for(session, database)` (done); `cache(&key)` (memory, else disk, never
network; a few ms once in memory); `ensure(session, database)` (background build when missing or
older than `stale_after` = 15 min; returns at once); `refresh(session, database, RefreshScope,
cancel)` (waits; joins a running build of the key); `status(session, database)`;
`invalidate(&event)`; `forget_profile(profile)`. Builds run on their own child session
(`ConnectionManager::open_child`) on the requested database, one per key at a time, and never
block editors or the tree. At most `max_caches` = 16 caches in memory (LRU; snapshots stay on
disk). `CacheStatus { key, state: missing|ready|stale|failed, source: disk|live, building,
generation, readAt, relationCount, columnCount, routineCount, truncated, lastBuildMs, lastError }`.

Invalidation hooks publish `SchemaChangeEvent { session, database, source: designer|import|query,
changes: Vec<oxbase_sql::SchemaChange> }` on the shared `SchemaEvents` bus (tokio broadcast,
capacity 256, done):

| Where | When | `changes` |
|---|---|---|
| `DesignerService::apply` | after a successful apply | `Create` / `Alter` of the table (and `Rename` with the new name when renamed) |
| `ImportService` run | after it created the target table (mode create / replace) | `Create` of the table |
| `QueryService` | after each statement that succeeded and for which `oxbase_sql::schema_changes(dialect, text)` is not empty | that list (an empty-name change means "refresh everything") |

The cache service subscribes; relation changes refresh only those relations (a partial read
merged with `SchemaCache::merge`), anything else (schemas, routines, types, `Other`, empty names,
a lagged receiver) refreshes the whole database. Refreshes of one key coalesce for 300 ms. The
services get the bus through a setter the cache stream adds (`with_schema_events`), wired in
`crates/app-tauri/src/backend.rs` (the bus is created there today).

`schema_changes` (done) is lexical and best effort: `CREATE` / `ALTER` / `DROP` / `RENAME` /
`sp_rename` with modifiers (`OR REPLACE`, `OR ALTER`, `TEMP`, `UNIQUE`, `MATERIALIZED`, MySQL
`DEFINER = ...`), `IF [NOT] EXISTS`, `DROP TABLE a, b`, `RENAME TABLE a TO b`, `ALTER ... RENAME
TO c`; scanning stops after a routine, trigger or event (its body runs later). Not detected:
`SELECT ... INTO`, dynamic SQL; the editor's refresh button covers those.

## 4. Insight engine (`oxbase-insight`)

All offsets are bytes; the services convert to and from UTF-16 (§7). Every function is a pure,
synchronous function of its inputs (no I/O, no global state) and **never panics** on any input
(fuzz test, §9). `Dialect { engine, sql, language, quoting, version, case_rules }` comes from
`Dialect::from_registry` (the app) or `Dialect::builtin(engine)` (tests: standard language data
and the engine's usual quoting); `quote_name` / `quote_path` quote only where needed.

### 4.1 Outline (context stream)

`outline(dialect, text, statement_range) -> Outline { range, class, queries, refs }`.
`StatementClass` from the leading keywords (after `EXPLAIN` / `EXPLAIN ANALYZE` and T-SQL `SET`
prefixes). `QueryBlock { range, parent, ctes, relations, output, star, clauses }` for the
statement's main query, each CTE body and each subquery; `ScopeRelation { source: Named |
Cte | Derived | Function | Values, alias, column_aliases, range }`; `NameRef { parts, ranges,
role: relation|column|function|procedure|type|schema|database|definition|unknown, query, clause
}`; helpers `query_at`, `ref_at`, `visible_relations`, `visible_ctes` (done). Must recognise in
every dialect: `WITH [RECURSIVE]` with column lists; `SELECT [DISTINCT|ALL|TOP (n)]` lists with
aliases (`AS x`, bare `x`, T-SQL `x = expr`); `FROM` lists with commas, every join form (`[INNER]
| LEFT | RIGHT | FULL [OUTER] | CROSS | NATURAL JOIN`, `CROSS / OUTER APPLY`, `LATERAL`),
`ON` / `USING`; derived tables and `VALUES` lists with alias column lists; table functions;
`WHERE`, `GROUP BY`, `HAVING`, `WINDOW`, `QUALIFY`-free, `ORDER BY`, `LIMIT / OFFSET / FETCH`;
set operations; `INSERT INTO t [(cols)] VALUES | SELECT`, `ON CONFLICT ... DO UPDATE SET`, `ON
DUPLICATE KEY UPDATE`, `RETURNING` / `OUTPUT`; `UPDATE t [AS a] SET ... [FROM ...] WHERE`,
MySQL multi-table `UPDATE a JOIN b`; `DELETE FROM t [USING ...]`, MySQL `DELETE a FROM a JOIN b`;
`MERGE INTO t USING s ON ... WHEN`; `CREATE TABLE name (...)`, `CREATE VIEW name AS query`,
`CREATE INDEX name ON t (cols)`, `ALTER TABLE t ADD | DROP | ALTER | RENAME ...`, `DROP ... name`;
`CALL` / `EXEC[UTE]`; `USE db`. Quoted identifiers keep `quoted = true`; parameters (`$1`, `?`,
`:name`, `@var`) and literals are not references. Unknown constructs are skipped to the next
clause keyword at the same parenthesis depth.

### 4.2 Cursor context (context stream)

`context_at(dialect, text, offset) -> CursorContext { offset, statement, class, clause, word,
prefix, quote, qualifier, expected, keywords, scope, ctes, outline }`, with `column_sources(case)`
(done). The statement is the one `oxbase_sql::pick_statement` picks. Under 5 ms for a
10 000-token statement (release). Required readings (`|` = cursor; the context tests cover each
on every dialect where the syntax exists, at least 80 cases):

| Text | `expected` (first) | Notes |
|---|---|---|
| `|` (empty or a new statement) | Keyword, Snippet | `keywords` = statement starters |
| `SELECT |` / `SELECT a, |` | Column, Function, Keyword | scope from the `FROM` after the cursor too |
| `SELECT t.| FROM orders t` | Column | `qualifier = [t]`, `column_sources` = `orders` |
| `SELECT s.|` with no `FROM` | Relation, Column | `qualifier = [s]` may be a schema |
| `FROM |`, `JOIN |`, `UPDATE |`, `INSERT INTO |`, `DELETE FROM |`, `TRUNCATE |` | Relation, Schema | CTE names count as relations |
| `FROM sales.|` | Relation | `qualifier = [sales]` |
| `FROM db.sales.|` | Relation | three parts on PostgreSQL / SQL Server |
| `JOIN b ON |` / `WHERE |` / `GROUP BY |` / `ORDER BY |` / `HAVING |` | Column, Function, Keyword | |
| `FROM orders o |` | Keyword | `keywords` = `WHERE JOIN ... GROUP ORDER LIMIT` (alias already given) |
| `FROM orders |` | Keyword | an alias may be typed: no names offered |
| `INSERT INTO t (|` | Column | of `t` only |
| `UPDATE t SET |` | Column | of `t` only |
| `CAST(x AS |`, column definition type position | Type | |
| `CALL |`, `EXEC |` | Procedure | |
| `CREATE TABLE |`, `AS x|` (alias), `WITH x|` (CTE name) | (nothing) | a new name |
| inside `'...'`, a comment, a dollar body | (nothing) | |
| `SELECT "Ord|` | Column, … | `quote = Some(b'"')`, `prefix = "Ord"`, `word` starts at the quote |
| `WITH c AS (SELECT a FROM t) SELECT c.| FROM c` | Column | `ctes[0].columns = [a]` |
| `SELECT (SELECT x.| FROM t2 x) FROM t1` | Column | inner scope first, then `t1` |

`word` covers the identifier or keyword being typed (including an opening quote) and extends
past the cursor to the end of that word; `prefix` is the typed part before the cursor, unquoted.

### 4.3 Completion (complete stream)

`complete(ctx, dialect, cache, snippets, limit, cancel) -> CompletionList { items, incomplete }`
with `Completion { label, kind, insert_text, is_snippet, detail, documentation, sort_key,
filter_text, replace }`. Rules:

- **Sources** per expected kind: columns of `column_sources` (resolved through
  `SchemaCache::resolve_relation`, else `relations_named`; CTE / derived columns from the
  outline); relations of the search-path schemas, then other schemas (inserted qualified),
  then CTE names; schemas and databases; built-in functions of `dialect.language` (filtered by
  `version`) and cached routines; types (language + cached types); keywords
  (`ctx.keywords` first, then any keyword of the dialect only at statement level); snippets that
  apply to the engine at statement starts; `JOIN` items from foreign keys after `JOIN |`
  (`orders o ON o.customer_id = c.id`).
- **Matching**: case-insensitive prefix first; fuzzy (`fuzzy_score`) fills up to `limit` when
  the prefix finds fewer. A typed prefix inside quotes matches exactly that text.
- **Ranking** (`sort_key`, byte order): group, then score descending, then shorter label, then
  label. Groups: items of the most likely expected kind first (columns in a select list,
  relations after `FROM`), aliases and CTEs right after them, then functions, then keywords,
  then snippets; an exact match of the prefix ranks first in its group.
- **Insert texts**: names through `dialect.quote_name(name, rule)` with the cache's
  `case_rules` (schema-qualified when outside the search path); functions as `name($0)` with
  `is_snippet` (or `name()` for zero-argument ones); types with parameters as `varchar(${1:n})`;
  keywords in upper case unless the typed prefix is lower case; snippets their body.
- **Details**: column `type · nullable / not null · PK / FK → table`; relation `table · schema`;
  function `name(args) → returns`; documentation from comments and summaries.
- `incomplete = true` when more matched than `limit` or when a cache build is pending.
- Checks `cancel` between sources and every 4 096 names; returns `Error::Cancelled` once set.

### 4.4 Language data and hover

`SqlLanguage { engine, keywords, reserved, functions: [FunctionInfo { name, args, returns, kind,
min_args, max_args, summary, since }], types: [TypeInfo { name, params, summary }], ident_chars,
default_schema }` (static data; rules in `oxbase_dialect::language`, checked by `validate`).
The **format** stream writes one per engine (`oxbase_pg::dialect::language`, ... MySQL and
MariaDB may share one with `since` gates) from each engine's public documentation: every keyword
and reserved word of the engine's keyword appendix, the built-in functions users call (at least
the string, numeric, date / time, aggregate, window, JSON, conditional and conversion families;
PostgreSQL ≥ 250, MySQL ≥ 200, SQLite ≥ 80 including the JSON and math functions, SQL Server ≥
150), and every built-in data type. Summaries are one sentence written for Oxbase. Clean room:
no list is copied from another tool.

`hover(dialect, text, offset, cache) -> Option<Hover { range, markdown }>` (diagnose stream):
relation (kind, qualified name, comment, columns with type / null / PK / FK), column (type,
nullability, keys, owning relation), routine (signature), built-in function (signature and
summary), CTE or alias (what it stands for). Keywords get none.

### 4.5 Diagnostics (diagnose stream)

`diagnose(dialect, text, cache, cancel) -> Vec<Diagnostic { range, severity, code, message }>`.
**No false-positive errors**: `Diagnostic::new` caps severity at `DiagnosticCode::max_severity`:

| Code | Max | Raised when |
|---|---|---|
| `unterminated-string` / `-identifier` / `-comment` | error | the token runs to the end of the text (`SqlToken::is_terminated`) |
| `unbalanced-parenthesis` | error | `)` without `(`, or `(` not closed, within one statement (strings and comments excluded) |
| `unknown-relation`, `unknown-schema` | warning | not in the cache; **hint** unless the cache is `ready`, complete (not truncated) and the name is not a CTE, alias, temporary table (`#t`, `TEMP`), created or renamed earlier in the same script, or in another database / an attached database |
| `unknown-column` | warning | none of the resolved relations has it; hint when a relation in scope is unresolved, a derived table selects `*`, or the name may be a select-list alias (`ORDER BY`, MySQL `GROUP BY` / `HAVING`) |
| `ambiguous-column` | warning | unqualified and two or more resolved relations in scope have it |
| `wrong-argument-count` | warning | a built-in function with no overload accepting that count (and no cached routine of that name) |
| `unknown-function` | hint | neither built in nor cached |
| `missing-where` | warning | `UPDATE` / `DELETE` of a table without `WHERE` (not for `TRUNCATE`) |

Without a cache only lexical findings and `missing-where` appear. Messages never quote string
literal contents. Results are sorted by range start.

### 4.6 Formatter (format stream)

`format(dialect, text, &FormatOptions) -> String` and `format_range(dialect, text, range,
options) -> TextEdit` (whole statements overlapping the range, re-indented at the first
statement's column). `FormatOptions { indentWidth 4, useTabs false, keywordCase upper,
maxLineLength 100, leadingCommas false, blankLineBetweenStatements true }` (`validate`: indent
1-8, line 40-400). Layout: one clause per line, lists broken one item per line past the line
limit, subqueries and CTE bodies indented, `JOIN ... ON` on its own line, `CASE` arms indented,
comments kept with the token they precede (a line comment always ends its line), `GO` /
`DELIMITER` lines kept as they are, dollar-quoted and other routine bodies untouched,
statements separated as the options say.

**Invariant** (`check_format`, done): the output must lex to the same tokens as the input apart
from whitespace and the case of keywords (`oxbase_sql::tokens_equivalent`: same tokens one for
one, comments byte-identical, words may change ASCII case only when the language lists them as
keywords, touching operators / qualifiers / literal prefixes stay touching, same statement
count). `format` runs the check before returning and fails with `Error::Internal` when it does
not hold; the editor then keeps the text. The formatter re-cases a word only where the outline
reads it as a keyword (a column named `date` or `user` keeps its spelling).

## 5. Latency contract and perf tests

- **Budget**: `context_at` + `complete` together, from text + offset to the ranked list, at most
  **50 ms at p95** for every scenario of `[m6.perf]` on the exit-check cache
  (`SyntheticSpec::exit_check()`: 5 000 relations x 20 columns, 500 routines, 4 schemas), in a
  `--profile perf` build. `diagnose` at most 100 ms for a 1 MiB script; `format` at most 200 ms
  for a 100 KiB script. A cancelled completion returns within 5 ms.
- **Non-blocking**: requests never wait for the network (§3.5); work runs on the blocking pool;
  `RequestGate` (done) admits only the newest request per (editor, kind) and cancels the older
  one's token.
- **UI rules** (app stream; constants in `ui/src/api/insight.ts`): completion has no debounce
  (Monaco triggers on typing, `.` and Ctrl+Space; stale answers are dropped with
  `InsightSequencer.isStale`); diagnostics run 400 ms after the last edit and when the cache
  generation changes; hover uses Monaco's own delay; formatting only on command. The status bar
  shows the cache state; while `building`, the UI polls `schema_cache_status` every 2 s.
- **Synthetic perf test** (complete stream): `crates/oxbase-insight/tests/complete_perf.rs`,
  `#[ignore]`; builds the exit-check cache (reports the build time, limit 2 s), runs each
  `[m6.perf.scenarios]` entry `warmup` + `iterations` times, prints p50 / p95 / max per scenario,
  checks every scenario returns at least one item of its `expect` kind, and with
  `OXBASE_REQUIRE_M6_PERF=1` fails above `complete_p95_ms`.
- **Live variant** (cache stream): `crates/oxbase-services/tests/schema_cache_perf.rs`,
  `#[ignore]`, PostgreSQL fixture only: creates `[m6.perf] tables` tables of
  `columns_per_table` columns in schema `[m6.perf] schema` (one `DO` block; reused when present
  and younger than `stale_after_secs`), builds the cache through `SchemaCacheService::refresh`
  (limit `build_limit_ms`, reported), reloads it from the app DB (limit `reload_limit_ms`), then
  runs the scenarios through `InsightService::complete` (limit `complete_p95_ms`).
- **CI job** `insight-perf` (complete stream; Linux; full lane plus the fast lane of
  `wip/*-perf`, `wip/*-complete`, `wip/*-cache` and `wip/*-integrate`): `cargo test -p
  oxbase-insight --profile perf --locked --test complete_perf -- --ignored --nocapture`, then the
  PostgreSQL fixture (`scripts/ci-fixtures.sh postgres`) and `cargo test -p oxbase-services
  --profile perf --locked --test schema_cache_perf -- --ignored --nocapture`, both with
  `OXBASE_REQUIRE_M6_PERF=1`.
- **Through IPC** (app stream): the webview smoke test times 50 `insight_complete` calls on the
  e2e PostgreSQL database and reports p95 (limit 150 ms, IPC and JSON included).

## 6. Snippets

`Snippet { id, name, prefix, body, description, engines, builtin }` (camelCase), bodies in
Monaco snippet syntax (`$1`, `${1:default}`, `${1|a,b|}`, `$0`, variables, `\$` escapes) checked
by `validate_snippet_body`; `SnippetInput { id?, name, prefix, body, description?, engines? }`
with `validate` (name 1-100, prefix 1-40 of `A-Z a-z 0-9 _ -`, body 1-10 000, description
≤ 500, SQL engines only). Built-in set (`oxbase_insight::snippets::builtin_snippets`, 19 written
for Oxbase, ids `builtin:<slug>`; each prefix at most once per engine): select, first rows
(`LIMIT` / `TOP`), count per group, insert, update, delete, inner / left join, CTE, `CASE`,
`EXISTS`, create table, create index, upsert (`ON CONFLICT` / `ON DUPLICATE KEY`), transaction
(two spellings), PostgreSQL `DO` block. User snippets in the app DB (`SnippetRepository`).
`SnippetService { list(engine?) (user first, then built-in, filtered), save, delete }` (done;
built-in ones cannot be deleted).

## 7. Tauri commands and TS (`ui/src/api/insight.ts`)

| Command | Arguments (camelCase) | Returns |
|---|---|---|
| `insight_complete` | `request: InsightRequest`, `limit: number \| null`, `requestId` | `CompletionResponse` |
| `insight_diagnose` | `request`, `requestId` | `DiagnosticsResponse` |
| `insight_hover` | `request` | `HoverResponse \| null` |
| `insight_close_editor` | `editorId` | `void` |
| `format_sql` | `request: FormatRequest` | `FormatResponse` |
| `schema_cache_refresh` | `sessionId`, `database: string \| null`, `scope: RefreshScope \| null`, `requestId` | `CacheStatus` |
| `schema_cache_status` | `sessionId`, `database` | `CacheStatus` |
| `snippet_list` | `engine: Engine \| null` | `Snippet[]` |
| `snippet_save` | `snippet: SnippetInput` | `Snippet` |
| `snippet_delete` | `id` | `boolean` |

- `InsightRequest { editorId, seq, engine, sessionId | null, database | null, text, offset }`
  (offset in UTF-16 units). `seq` grows per (editor, kind); `InsightSequencer` hands them out.
- `CompletionResponse { seq, superseded, items: CompletionItem[], incomplete, cache, elapsedUs }`;
  `CompletionItem { label, kind, insertText, isSnippet, detail, documentation, sortText,
  filterText, range: {start, end} }` (UTF-16). `DiagnosticsResponse { seq, superseded,
  diagnostics: [{ range, severity: hint|warning|error, code, message }], cache, elapsedUs }`.
  `HoverResponse { range, markdown }`. `FormatRequest { engine, text, range | null, options |
  null }` → `FormatResponse { range, text }` (replace `range` of the sent text).
- `CacheStatus` as §3.5; `lastError` is a core error `{ category, message? }`.
  `RefreshScope = {type: "full"} | {type: "schemas", names} | {type: "relations", targets:
  [{schema?, name}]}`.
- A superseded request is an ordinary answer (`superseded: true`), not an error. `requestId`
  cancels with the existing `cancel_request`. Errors are the usual `{category, message}`. Each
  command has an `allow-<command>` permission (`build.rs`, `capabilities/default.json`).
- The app's Monaco provider maps `kind` to `CompletionItemKind`, `isSnippet` to
  `InsertAsSnippet`, `severity` to marker severities (error / warning / hint, hints with a
  dotted underline), and closes editors with `insight_close_editor` next to `query_close_editor`.

## 8. Invariants

1. **Tolerant**: `outline`, `context_at`, `complete`, `diagnose`, `hover`, `format`,
   `format_range` and `schema_changes` never panic and never fail on any text (format may
   return an error, never a wrong text). Incomplete SQL is the normal case.
2. **No false-positive errors**: only lexical certainties are errors (§4.5); every valid
   statement of the corpus produces no error and no warning without a cache, and no error with
   the matching cache.
3. **Formatter token equivalence**: every `format` / `format_range` result passes
   `check_format`; formatting is idempotent (`format(format(x)) == format(x)`).
4. **Latency**: §5 budgets; insight requests never wait on the network or on another editor's
   query.
5. **Cancellation**: a newer request of an editor cancels the older one; `requestId` cancels;
   cancelled work returns `Cancelled` and leaves no state behind.
6. **Cache invalidation**: a schema change made through Oxbase (designer, import, editor DDL)
   reaches the cache within 2 s of the change finishing; a stale cache is still served (marked
   `stale`) while it refreshes; a failed build keeps the previous names.
7. **One lexer**: insight, formatter and splitter use the `oxbase-sql` tokens only.
8. **Data, not special cases**: keywords, functions, types, identifier characters and case
   rules come from `SqlLanguage` / `CaseRules`; insight code has no `match engine`.
9. **Secrets**: snapshots, completions, diagnostics, hovers and logs hold names and types only;
   diagnostic messages never quote string literals.

## 9. Exit-check tests

| Test | Where | Stream |
|---|---|---|
| token API, `tokens_equivalent`, `schema_changes` | `oxbase-sql` unit tests | Design (done) |
| case rules, snapshot, snippets validation | `oxbase-types` unit tests | Design (done) |
| language data rules, registry fallbacks, `missing_insight_roles` | `oxbase-dialect` unit + `tests/registry.rs` | Design (done) |
| index, resolution, merge, codec, synthetic, memory budget | `oxbase-schema-cache` unit tests | Design (done) |
| snippet and snapshot repositories, migration 2 | `oxbase-store` unit tests | Design (done) |
| gate, payloads, snippet service, events | `oxbase-services` unit tests | Design (done) |
| command argument mapping, wiring | `app-tauri` unit tests; `ui/src/api/insight.test.ts` | Design (done) |
| per-engine catalog reader live test: seeded `[m6.<engine>]` objects appear with columns, PK, FK, routines, sequences, types, case rules; partial scope; cancel | `crates/oxbase-{pg,mysql,sqlite,mssql}/tests/catalog_live.rs` | cache |
| cache service live test: build, persist, reload without network, invalidation from designer apply / import create / editor DDL within 2 s, `forget_profile` | `crates/oxbase-services/tests/schema_cache_live.rs` | cache |
| 5k-table live perf | `crates/oxbase-services/tests/schema_cache_perf.rs` | cache |
| outline and context tables (≥ 80 cases over the five dialects, §4.2) | `crates/oxbase-insight/tests/{outline,context}.rs` | context |
| fuzz: every prefix, every single-token deletion and random cuts of the corpus through every insight function, no panic | `crates/oxbase-insight/tests/fuzz.rs` | context |
| completion behaviour (sources, ranking, quoting per dialect, cancel) | `crates/oxbase-insight/tests/complete.rs` | complete |
| synthetic perf, `insight-perf` job | `crates/oxbase-insight/tests/complete_perf.rs`, `.github/workflows/ci.yml` | complete |
| `InsightService` (gate, cache states, UTF-16 conversion end to end) | `crates/oxbase-services/src/insight` tests | complete |
| diagnostics policy and the valid-SQL corpus (≥ 150 statements per dialect; zero errors and warnings without a cache) | `crates/oxbase-insight/tests/diagnose.rs`, `tests/corpus/*.sql` | diagnose |
| hover | `crates/oxbase-insight/tests/hover.rs` | diagnose |
| formatter: corpus formatted with every option combination passes `check_format`, idempotence, `format_range` | `crates/oxbase-insight/tests/format.rs` | format |
| `SqlLanguage::validate` empty and coverage minimums per engine | engine crate unit tests | format |
| Monaco providers (mapping, stale answers dropped, debounce with fake timers, snippet UI, cache status) | `ui/src/workbench/editor/*.test.ts(x)` | app |
| webview smoke: completion after `SELECT * FROM ` shows a fixture table; IPC timing | `ui/e2e/**` | app |

## 10. Dependencies

No new external crate. New internal edges: `oxbase-insight` → `oxbase-types`, `oxbase-sql`,
`oxbase-dialect`, `oxbase-schema-cache`, `tokio-util`, `serde`; `oxbase-schema-cache` →
`oxbase-types`, `oxbase-sql`, `oxbase-dialect`, `serde_json`; `oxbase-services` →
`oxbase-insight`, `oxbase-schema-cache`. `deny.toml` and the UI dependencies are unchanged.

## 11. Fixtures (`tests/fixtures.toml` `[m6]`)

`[m6]` holds the scratch prefix (`m6_t_`) for live tests, `[m6.<engine>]` the database / schema
each engine's live tests use, `[m6.perf]` the 5k-table live variant and the latency limits, and
`[[m6.perf.scenarios]]` the cursor texts shared by the synthetic and the live perf tests (`<|>`
marks the cursor; `{table}` / `{table2}` are replaced by two relations of the cache that have a
foreign key between them; `{schema}` by a schema other than the default one).

## 12. Deviations from PLAN.md

- PLAN §2 and §6 named tree-sitter for context; M6 uses the tolerant parser of §2.1 instead.
  `oxbase-sql` gains no `sqlparser` AST in M6.
- The formatter lives in `oxbase-insight` (PLAN §2 listed it under `oxbase-sql`); `oxbase-sql`
  holds its safety check.
