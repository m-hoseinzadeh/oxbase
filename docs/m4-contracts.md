# M4 contracts

Status: frozen for M4 on 2026-11-01 (Design step). M4 exit check (PLAN.md §6): **conformance
green on 5 engines** (PostgreSQL, MySQL, MariaDB, SQLite, SQL Server). Scope: the SQL Server
driver and its dialect cells (tree, designer, grid editing), and editor, grid and designer
parity in the app. The fixture is SQL Server 2025 (`tests/fixtures.toml` `[engines.mssql]`,
port 15433, user `sa`).

Changes to these contracts go through the Design owner. Streams code against them and must not
change public signatures; adding private helpers, private fields and tests is always fine. Rust
doc comments on the items named here are part of the contract and are more detailed than this
file (start with the module docs of `oxbase_mssql`, `oxbase_mssql::driver`,
`oxbase_mssql::driver::types`, `oxbase_mssql::dialect` and each skeleton file under
`crates/oxbase-mssql/src/dialect/`).

## 1. Who owns what

Bodies marked `unimplemented!("m4: <stream>")` belong to that stream. No test may call one
until its stream has filled it. Six streams start from the Design head in parallel.

| Area | Path | Owner | State at hand-off |
|---|---|---|---|
| `tiberius` 0.13.0 in `[workspace.dependencies]`, every dependency of `oxbase-mssql`, `Cargo.lock`, `deny.toml` | root | Design | done (§10) |
| Crate layout, `options`, `extras`, `MsSqlDriver` / `register` shell, `MsSqlConnection` shell | `crates/oxbase-mssql/src/lib.rs`, `src/driver/mod.rs` (shell) | Design | done |
| Dialect registration, tree layout, `MsSqlCapabilities::{supports, supports_kind}`, `APPLICABLE` | `crates/oxbase-mssql/src/dialect/mod.rs`, `caps.rs` (those items) | Design | done, tested |
| `BracketQuoting` (no database in names, `TOP (n)`, `OFFSET ... FETCH`, bracket lexing), `Quoting::top_clause`, `LexRules::bracket_identifiers` (also for SQLite's `AnsiQuoting`) | `crates/oxbase-dialect` | Design | done, tested (§9) |
| Registration points (commented lines) and the `oxbase-mssql` dependency | `crates/oxbase-services/{Cargo.toml, src/drivers.rs, src/dialects.rs}` | Design | done |
| Fixture names: `[engines.mssql]` TLS notes, `[engines.mssql.tls_user]`, `[tls] server_sans`, `[ssh.targets] mssql`, `[m2.mssql]`, `[m3.mssql]`, `[m4]`, `[m4.mssql]` | `tests/fixtures.toml` | Design | done |
| Driver: connect, TLS, auth, statements, Arrow mapping, parameters, cancel, transactions, errors; uncomment `oxbase_mssql::register(&mut drivers)` in `default_drivers` (and update its unit test) | `crates/oxbase-mssql/src/driver/**` (`conn.rs`, `tls.rs`, `types.rs`, `error.rs`, new files), `crates/oxbase-mssql/tests/driver_live.rs`, that one line + test of `crates/oxbase-services/src/drivers.rs` | **driver** | skeleton |
| Catalog fetchers for every tree kind | `crates/oxbase-mssql/src/dialect/fetch.rs` (+ `fetch/**`), `crates/oxbase-mssql/tests/fetch_live.rs` | **fetch** | skeleton |
| DDL generator, comparator rules, designer capabilities, grid writer | `crates/oxbase-mssql/src/dialect/{ddl*, compare*, caps.rs (designer), dml*}`, `crates/oxbase-mssql/tests/m4_live.rs` | **ddl** | skeleton |
| T-SQL splitter rules and `redact_secrets` for T-SQL | `crates/oxbase-sql` | **sql** | M2 placeholder (`GO` lines) |
| Docker: mssql TLS (`mssql.conf`), TLS-only login, SSH target, init scripts `03-tls-user.sql`, `04-m2.sql`, `05-m3.sql`, `06-m4.sql`; certificate SANs; conformance mssql cells and the 5-engine guard; the CI lines of §8 | `tests/docker/**`, `scripts/fixtures-certs.sh`, `scripts/ci-fixtures.sh`, `scripts/fixtures-up.sh`, `crates/oxbase-conformance`, the conformance job of `.github/workflows/ci.yml` (§8) | **fixtures** | - |
| UI parity: connection dialog fields for SQL Server, designer type lists and options from capabilities, editor T-SQL language and `GO`, read-only reasons; e2e only if cheap | `ui/src` (incl. `ui/src/api/**` for SQL Server payload additions), `crates/app-tauri` | **app** | - |

Integration (after the streams): uncomment `oxbase_mssql::dialect::register(&mut reg)` in
`crates/oxbase-services/src/dialects.rs` (it registers every cell at once, so it waits for both
the fetch and the ddl stream), add the SQL Server lines of the services live tests (§8.3) and
set the 5-engine requirement in CI.

Streams add no crate to `Cargo.lock`: every dependency `oxbase-mssql` needs is already in its
`Cargo.toml` (§10). A stream that needs another crate adds it with an explicit version to its
own crate's `Cargo.toml`, checks the license against `deny.toml` and reports it for hoisting.

**Live tests** go in `crates/oxbase-mssql/tests/*.rs`, `#[ignore]`d, reading
`tests/fixtures.toml`. They run in the conformance CI job only once `-p oxbase-mssql` is in its
two cargo lines (§8.1), a change outside the Design step's paths. Until it lands, no SQL Server
live test runs in CI: the orchestrator should land §8.1 first (two words in `ci.yml`) or let
each of the driver, fetch and ddl streams make that edit on its own branch. The plain (non-TLS)
SQL Server fixture already runs in the conformance job today (`sa`, database `oxbase_test`).

## 2. Driver (`oxbase_mssql::driver`, driver stream)

### 2.1 Driver library: tiberius 0.13.0, evaluated

`tiberius` 0.13.0 (crates.io, 2026-09-25, MIT OR Apache-2.0, MSRV 1.88; the first release after
0.12.3 of 2024-07, so maintained again by prisma) was read from the registry source. Findings:

| Requirement | tiberius 0.13.0 | Decision |
|---|---|---|
| rustls + ring, no aws-lc / OpenSSL / native-tls | **Fails.** Feature `rustls` depends on `tokio-rustls = "0.26.4"` *with default features*, which enable `aws_lc_rs`, and `rustls_tls_stream.rs` falls back to `crypto::aws_lc_rs` when no process provider is installed. `native-tls` and `vendored-openssl` are OpenSSL on Linux. | Built with **no TLS backend** (`default-features = false, features = ["tds73", "chrono"]`). oxbase runs the TLS handshake itself (§2.2) with `oxbase_net::tls::client_config` (rustls 0.23 + ring). `cargo metadata` shows no `aws-lc-*`, `openssl*` or `native-tls` in the graph; the cargo-deny job checks it in CI. |
| TDS 7.4 | Yes (LOGIN7 `SqlServerN` = 7.4). | Default. |
| TDS 8.0 strict | Only with a TLS backend (`tds80` feature = TLS before PRELOGIN). | oxbase does the strict TLS itself (ALPN `tds/8.0`), then lets tiberius speak over it (§2.2). |
| Cancel | `Client::cancel_query`: sends an ATTENTION packet and drains to the `DONE` with the attention bit. Needs `&mut Client`, i.e. after the result stream is dropped. A stream dropped in the middle of a token can leave the token reader out of step. | Attention in-band; if the drain fails or exceeds 1.5 s, close the socket (§2.6). No `KILL` (it ends the whole session). |
| Multiple result sets | Yes: `QueryStream` yields `Metadata` per result set, then rows. | Used. |
| Row counts | Only `ExecuteResult` (no rows) and `CommandStream` (RPC by procedure name) report `DONE` counts; `QueryStream` (SQL batch and `Client::query`) does not. | Parameterized statements go through `Command::new("sp_executesql")` (rows **and** counts). SQL batches report no `RowsAffected` (known gap). |
| `PRINT` / `RAISERROR` (severity <= 10) | Not exposed (the INFO token is decoded and logged at DEBUG, then dropped; upstream `TODO(surface)`). | Known gap: no `Notice` for them in M4. Errors (severity >= 11) are errors. |
| Result metadata | `Column { name, ColumnType }` only: no length, precision, scale, nullability, or the width of `INTN` / `FLTN`. | Arrow mapping by `ColumnType` (§2.5): one `Int64` for integers, decimals as exact text. |
| Types | All SQL Server types decode: integers, `bit`, `decimal` / `numeric` (`Numeric`, i128 + scale), money, floats, date / time family (`tds73`), character types with collation code pages (`encoding_rs`), binary, `uniqueidentifier`, `xml`, `sql_variant`, CLR UDTs (raw bytes). `json` / `vector` (2025) arrive as `nvarchar(max)` / `varchar(max)` because tiberius does not announce the JSON / vector features. | §2.5. |
| Named instances | SQL Browser lookup only with `sql-browser-tokio`, and it dials itself (cannot go through an SSH tunnel). | Not used: a named instance needs a port in M4 (§2.3). |
| Windows / Entra auth | `winauth` (Windows SSPI), `integrated-auth-gssapi` (libgssapi), AAD token. | Not in M4: SQL authentication only. |
| Bulk copy | `Client::bulk_insert` (`INSERT BULK`). | Not in M4 (PLAN §7 risk: tested with M5 import). |

**Alternatives considered.** `mssql-tds` 0.3.0 (Microsoft, MIT) hard-depends on `native-tls`
and, on Linux, `openssl`: excluded by the TLS rule. Writing a TDS client in `oxbase-mssql` (no
third-party protocol code; MS-TDS is a public specification) or carrying a patched tiberius
(exposing INFO and DONE tokens and full column metadata) would close the three known gaps; both
are larger than M4 and are recorded as follow-ups (§11). The driver keeps tiberius behind
`MsSqlConnection`, so either can replace it later without touching other crates.

### 2.2 Transport and TLS

Rules (also in the `oxbase_mssql::driver` module docs):

1. Every socket is opened at `ConnectParams::dial_endpoint()` (M1 §2): the SSH tunnel's local end
   when tunnelled. There is no out-of-band socket (cancel is in-band), so nothing else dials.
2. TLS verifies `ConnectParams::tls_server_name()` (also the SNI), never the dial address;
   `VerifyFull` works through a tunnel. The rustls config comes from
   `oxbase_net::tls::client_config(&params.tls)`; strict mode clones it and sets ALPN `tds/8.0`.
3. **TDS 7.4 negotiation (default).** oxbase writes its own PRELOGIN (`VERSION`, `ENCRYPTION`,
   `INSTOPT` = the instance name or empty, `THREADID`, `MARS = 0`, terminator) and reads the
   server's. With encryption agreed, the TLS handshake runs with each TLS record carried in TDS
   packets of type `0x12` (8-byte header, status `EOM`), the server's in packets of type `0x04`;
   after the handshake TLS records go directly over TCP and all TDS traffic is inside TLS.
   oxbase never offers `ENCRYPT_OFF`, so "login-only encryption" does not occur.
4. The stream handed to `tiberius::Client::connect` (tiberius `Config::encryption(NotSupported)`)
   then swallows tiberius' own PRELOGIN packet and answers it with the server's saved PRELOGIN
   response, its `ENCRYPTION` byte set to `ENCRYPT_NOT_SUP`; tiberius continues with LOGIN7
   inside the TLS session and never knows about it.
5. **TDS 8.0 strict** (`options.encrypt = "strict"`): TLS first, directly on TCP, ALPN
   `tds/8.0`; then the PRELOGIN exchange of rule 3 (with `ENCRYPTION = ENCRYPT_REQ`) inside TLS
   and the replay of rule 4.
6. `TlsMode` → PRELOGIN `ENCRYPTION` and result:

   | `TlsMode` | Sent | Server answers `NOT_SUP` | Server answers `OFF` / `ON` / `REQ` |
   |---|---|---|---|
   | `Disable` | `NOT_SUP` | plaintext | `REQ`: fail, "the server requires encryption"; else plaintext |
   | `Prefer` (default) | `ON` | plaintext, `tls_in_use() = false` | TLS, certificate not checked |
   | `Require` | `ON` | fail | TLS, not checked (`InsecureNoVerification`) |
   | `VerifyCa` | `ON` | fail | TLS, chain and dates checked, any name |
   | `VerifyFull` | `ON` | fail | TLS, chain, dates and name checked |

   Strict accepts `Require`, `VerifyCa`, `VerifyFull`; `Disable` / `Prefer` with strict are
   `InvalidInput`. TLS 1.2 and 1.3 (rustls); servers that only speak TLS 1.0 / 1.1 cannot
   connect with TLS (SQL Server 2017+ supports 1.2). `TlsOptions::client_cert` is refused with
   `Unsupported`.
7. The handshake (TCP, TLS, PRELOGIN, LOGIN7, the session query of §2.3) is bounded by
   `ConnectParams::connect_timeout` (default 15 s) and the `CancellationToken`-free connect
   future can be dropped by the connection manager at any point without leaking a task.

### 2.3 Connection parameters (`ConnectParams` → SQL Server)

| `ConnectParams` | SQL Server |
|---|---|
| `host`, `port` | server endpoint; default port 1433 (`Engine::MsSql.default_port()`) |
| `dial_addr` | where the TCP socket goes (tunnel) |
| `user`, `password` | SQL Server login (`AuthMethod::sql_server`). `user` required; Windows / Entra auth not in M4 |
| `database` | LOGIN7 database; `None` = the login's default database |
| `tls` | §2.2 |
| `application_name` | LOGIN7 application name (default `oxbase`) |
| `connect_timeout` | handshake deadline |
| `options` | `instance`, `encrypt` (`strict` / `negotiate`), `application_intent` (`read_only` / `read_write`), `batch_rows` ([`oxbase_mssql::options`]); any other key is `InvalidInput` naming it |

- **Named instances**: `options.instance` with an explicit port. M4 does not query SQL Browser
  (UDP 1434 cannot cross an SSH tunnel, and it would be a second address to dial); a named
  instance without a port is `InvalidInput` ("give the instance's port"). The name goes in
  PRELOGIN `INSTOPT`; a server that answers "not this instance" is refused.
- After login the driver runs once: `SET TEXTSIZE 2147483647; SELECT DB_NAME(), @@SPID,
  CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)), @@VERSION`. `server_version()` =
  `ServerVersion::parse(ProductVersion)` with `raw` = `@@VERSION`'s first line;
  `current_database()` = `DB_NAME()` (a later `USE` in the session is not tracked, as on MySQL);
  the SPID is kept for logs.
- Session settings: tiberius sets the LOGIN7 ODBC flag, so the server applies the ANSI
  defaults (`QUOTED_IDENTIFIER`, `ANSI_NULLS`, `ANSI_PADDING`, `ANSI_WARNINGS`,
  `CONCAT_NULL_YIELDS_NULL` on), which filtered indexes, indexed views and indexed computed
  columns require. `SET TEXTSIZE 2147483647` above keeps `text` / `ntext` / `(max)` values
  whole. Nothing else is changed: `SET NOCOUNT` stays `OFF`, so DML reports counts.

### 2.4 Statements, results and transactions

- `execute(sql, [])`: a **SQL batch** (`Client::simple_query`). It runs in the session scope:
  `USE`, `SET`, `#temp` tables and `BEGIN TRAN` persist, which the editor needs. The query
  service sends one `GO` batch per `execute` (§6).
- `execute(sql, params)`: **`sp_executesql`** via RPC (`tiberius::Command::new("sp_executesql")`,
  `@stmt` = `sql`, `@params` = `@P1 <decl>, @P2 <decl>, ...` from §2.5, then `@P1..@Pn`). It
  returns rows and per-statement counts. Session-scoped changes made inside it (`SET`, `#temp`
  created inside) end with it; temp tables created earlier in the session are visible.
- Items: `Schema` + `Batch`es per result set, in server order (a batch or a procedure may return
  several); `RowsAffected(n)` for each `DONE` / `DONEINPROC` / `DONEPROC` with the count bit that
  does not end a result set (RPC path only); errors end the stream with
  `Error::Sql { code: Some("<number>"), message }` (message: the server text, plus
  ` (procedure <p>, line <n>)` when sent). A batch with several statements stops at the first
  error the server aborts on; earlier results stay delivered.
- Batches hold at most `BATCH_ROWS` (4096, `options.batch_rows`) rows; no result is collected
  in memory.
- `begin()`: `BEGIN TRANSACTION` (batch); `Tx::commit` / `Tx::rollback`:
  `IF @@TRANCOUNT > 0 COMMIT` / `... ROLLBACK`. A dropped `Tx` is rolled back before the
  connection's next statement. `Tx::execute` follows `Connection::execute`.
- One statement at a time per connection (M1 trait rule); `execute` waits for the previous
  stream to finish or be dropped.

### 2.5 Arrow mapping and parameters

Result columns (full table and reasons in `oxbase_mssql::driver::types`):

| SQL Server | Arrow |
|---|---|
| `bit` | `Boolean` |
| `tinyint`, `smallint`, `int`, `bigint` | `Int64` (tiberius hides the width of nullable integers) |
| `real`, `float` | `Float64` |
| `decimal`, `numeric` | `Utf8`, exact text (scale not in the metadata) |
| `money`, `smallmoney` | `Decimal128(19, 4)` |
| `date` | `Date32` |
| `time(n)` | `Time64(Microsecond)` (100 ns digit truncated) |
| `datetime`, `smalldatetime`, `datetime2(n)` | `Timestamp(Microsecond, None)` (100 ns digit truncated) |
| `datetimeoffset(n)` | `Utf8`, RFC 3339 with the stored offset and 7 digits |
| `char`, `varchar`, `nchar`, `nvarchar` (+ `max`), `text`, `ntext`, `xml`, `sql_variant`, `json`, `vector` | `Utf8` |
| `binary`, `varbinary` (+ `max`), `image`, `rowversion`, `hierarchyid`, `geometry`, `geography`, CLR types | `Binary` |
| `uniqueidentifier` | `FixedSizeBinary(16)` + `ARROW:extension:name = arrow.uuid` |
| untyped `NULL` | `Null` |

Every field is nullable and carries `oxbase.native_type` = the type family (`int`, `decimal`,
`nvarchar`, `datetime2`, `udt`, ...). The grid and the edit service take column types from the
catalog (`TableDef`), not from this metadata.

Parameters (`@params` declarations): `Null` `nvarchar(1)`; `Bool` `bit`; `Int` `bigint`; `UInt`
`bigint` or `decimal(20, 0)`; `Float` `float` (NaN / infinities `InvalidInput`); `Decimal`
`decimal(38, s)`; `Text`, `Json` `nvarchar(max)`; `Bytes` `varbinary(max)`; `Date` `date`;
`Time` `time(7)`; `Timestamp` `datetime2(7)`; `TimestampTz` `datetimeoffset(7)`; `Uuid`
`uniqueidentifier`. A `nvarchar` NULL does not convert implicitly to `varbinary`, `image` or CLR
types, so the grid writer writes the keyword `NULL` (§4.5).

### 2.6 Cancellation (M1 §7, M2 §6 rule 6)

- In-band ATTENTION. `execute` runs its request in a task that owns the client (behind an
  `Arc<tokio::sync::Mutex<_>>`) and feeds a bounded channel (the `ResultStream`, `'static`).
  On the statement's token, on `cancel_running()` (which targets the request in flight when it
  is called), or when the stream is dropped, the task stops reading, drops the tiberius stream,
  calls `Client::cancel_query()` and waits for the `DONE` with the attention bit.
- The stream yields one `Err(Error::Cancelled)` and ends **within 2 s** of the cancel. If the
  drain fails or has not finished after 1.5 s, the driver shuts the socket down; the connection
  is then dead and every later call is `Error::Connection("the connection was closed after a
  cancel")`. The M2 query service replaces a session whose stream does not wind down within its
  1 s grace (as for MySQL), so the editor's next run starts at once either way.
- A cancel never reaches a later request: the attention is sent only while the targeted request
  is in flight, under the client lock.
- Through SSH the attention travels on the session's own socket; nothing else is dialled.
- A server-side timeout or `KILL` from another session is reported as the server's error, not
  `Cancelled`.

### 2.7 Errors and secrets

- `tiberius::error::Error::Server` → `Error::Sql` with the number; login errors 18456, 18452,
  18486, 18487, 18488 → `Error::Auth`; I/O, TLS and protocol errors → `Error::Connection` naming
  the server endpoint; deadlines → `Error::Timeout`.
- The password lives in tiberius' `AuthMethod` (`secrecy`) and in `ConnectParams` (`Secret`);
  `MsSqlConnection`'s `Debug` shows neither; no error, log line or panic contains it (live test
  with a marker password, as in M1 §10).

## 3. Object tree and fetchers (fetch stream)

### 3.1 Layout and scope (done: `oxbase_mssql::dialect::tree_layout`)

```text
connection
└─ databases                 sys.databases
   └─ schemas                sys.schemas of that database
      ├─ Tables              table → Columns, Indexes, Foreign keys, Check constraints, Triggers
      ├─ Views               view  → Columns, Indexes (indexed views), Triggers (INSTEAD OF)
      ├─ Functions           scalar, inline / multi-statement table-valued, CLR
      ├─ Procedures
      ├─ Sequences
      └─ Types               alias and table types (list and load only)
```

`DatabaseScope::Session` (decided): one session reads one database, like PostgreSQL. The tree
reads another database on a metadata child session (`ConnectionManager::open_child(base,
Some(db))`, M2 §3.4), and the editor's run on another database reopens its child (M2 §6). This
keeps every catalog query on the session's own `sys.*` views with bound parameters only: a
three-part name (`[db].sys.tables`) or `USE [db]` would put a name into SQL text, and
`OBJECT_DEFINITION`, `OBJECT_ID` and `COLUMNPROPERTY` only work in the current database.
`BracketQuoting::qualify` writes `[schema].[name]` without the database.

Kinds and decisions:

| Kind | M4 | Notes |
|---|---|---|
| Database, Schema, Table, Column, Index, ForeignKey, Check, View, Function, Procedure, Trigger, Sequence | fetcher, tree | |
| Type | fetcher (list, load as `Generic`), tree folder | alias and table types; CLR types listed with `type_kind = clr` |
| Trigger | DML triggers on tables and views | **DDL triggers** (database / server level) are **not listed** in M4 (no node for them) |
| User, Role, Grant | applicable, no fetcher (pending) | optional in M4; nothing in the tree |
| Synonym | not modelled | needs a new `ObjectKind`; post-M4 |
| MatView, Event, Extension, Key | not applicable | indexed views are views with indexes |

### 3.2 Parents, refs and system objects

- Parents as on PostgreSQL: Database ← root; Schema ← Database; Table, View, Function,
  Procedure, Sequence, Type ← Schema; Column, Index, ForeignKey, Check, Trigger ← the owning
  table or view (`parent_kind` = `View` for view children). `catalog` = database, `schema` =
  schema. Refs built with `parent.child(..)`.
- A ref whose `catalog` differs from `Connection::current_database()` is `InvalidInput` ("open
  a session on <db>"); the database listing is server-wide.
- `system = true`: databases with `database_id <= 4` (`master`, `tempdb`, `model`, `msdb`);
  schemas `sys`, `INFORMATION_SCHEMA`, `guest` and those with `schema_id >= 16384` (fixed
  database-role schemas); objects with `is_ms_shipped = 1` (and their children).
- `Database` summaries: `CURRENT` (the session's database), `SIZE_BYTES` only if cheap (omit
  otherwise), extras `STATE` (databases not `ONLINE` are listed; opening one fails with the
  server's error).
- Sorting: by name (binary order of `sys` names is fine); columns by `column_id`.

### 3.3 Definitions (`ObjectDef` per kind and extras)

`oxbase_mssql::extras` holds the SQL Server extra ids; generic ones are `oxbase_types::props`.

- **Database** (`SchemaDef`, `obj.kind = Database`): `owner` (`SUSER_SNAME(owner_sid)`), comment
  (database `MS_Description`), extras `COLLATION`, `COMPATIBILITY_LEVEL` (int),
  `RECOVERY_MODEL`, `STATE`.
- **Schema** (`SchemaDef`): owner (principal name), comment.
- **Table** (`TableDef`):
  - columns in `column_id` order: `native_type` as written in DDL (`int`, `varchar(20)`,
    `nvarchar(max)`, `decimal(12,2)`, `datetime2(6)`, `varbinary(max)`; alias types as
    `schema.name`, unquoted), `data_type` (§3.4), `nullable`,
    `default` = the default constraint's definition as stored (`((1))`, `(N'new')`,
    `(getdate())`) with `extras::DEFAULT_CONSTRAINT` = its name, `collation` only when it differs
    from the database default, `comment` (`MS_Description`);
  - identity: `auto_increment = true`, `identity = Some(IdentityDef { always: true, start: seed,
    increment })` (SQL Server identity values cannot be written without `IDENTITY_INSERT`, so
    they behave like `GENERATED ALWAYS`); `extras::NOT_FOR_REPLICATION` when set;
  - computed: `generated = Some(GeneratedColumn { expression: <definition as stored>, stored:
    is_persisted })`;
  - `rowversion`: `native_type = "rowversion"`, `data_type = Binary { length: Some(8) }`,
    `extras::ROW_VERSION = true`;
  - temporal period columns: `extras::PERIOD` = `row_start` / `row_end`, `extras::HIDDEN`;
    `extras::SPARSE`;
  - `primary_key` with its constraint name (clustering: see below);
  - `indexes` (not the PK): unique constraints (`is_constraint = true`) and indexes with
    `columns` (key columns in key order, `descending`), `unique`, `predicate` = filter
    definition as stored (`([code] IS NOT NULL)`), extras `CLUSTERED` (bool), `INDEX_KIND`,
    `INCLUDE_COLUMNS` (JSON array, `is_included_column`), `FILL_FACTOR` (when not 0),
    `DISABLED`, `IGNORE_DUP_KEY`, `DATA_COMPRESSION` (when not `NONE`). Columnstore, XML and
    spatial indexes are listed with their `INDEX_KIND` and no key columns as appropriate.
  - The primary key's clustering: `TableDef.extras[CLUSTERED]` = whether the PK is clustered
    (absent when there is no PK). The comparator treats an absent value as `true` (SQL Server's
    default for a PK when no clustered index exists).
  - `foreign_keys`: name, columns, `ref_table` (catalog, schema, name), `ref_columns`,
    `on_delete` / `on_update` (`NoAction`, `Cascade`, `SetNull`, `SetDefault`), extras
    `NOT_TRUSTED`, `NOT_FOR_REPLICATION`, `DISABLED`;
  - `checks`: name, `expression` = definition as stored (`([qty]>=(0))`), extras `NOT_TRUSTED`,
    `DISABLED`, `NOT_FOR_REPLICATION`;
  - `comment` (`MS_Description`); extras `TEMPORAL_TYPE`, `HISTORY_TABLE`, `MEMORY_OPTIMIZED`,
    `DATA_COMPRESSION`, `TABLE_TYPE` (`graph_node` / `graph_edge` / `ledger` when so).
- **Column / Index / ForeignKey / Check** (`ObjectDef::Column` etc.): the same items, with the
  owning table ref. Index listings include the PK-backing index with `PRIMARY_KEY = true`
  (M2 §2), indexed-view indexes under views.
- **View** (`ViewDef`): `definition` = the text after `AS` of the module definition (the
  fetcher strips `CREATE VIEW ... AS`, keeping comments inside the body), `columns`, comment,
  extras `CHECK_OPTION` (`cascaded` when `WITH CHECK OPTION`), `SCHEMA_BINDING`, `ENCRYPTED`.
- **Function / Procedure** (`RoutineDef`): `args` from `sys.parameters` (name **with** the `@`,
  as T-SQL writes it; `mode` `In`, or `InOut` for `OUTPUT`; `native_type`; `default` `None`:
  the catalog does not keep T-SQL parameter defaults), `returns` (the scalar type, or `TABLE`
  for both kinds of table-valued function), `language` = `SQL` or `CLR`, `body` = `None`,
  `definition` = full module text, comment, extras `FUNCTION_KIND`, `SCHEMA_BINDING`,
  `ENCRYPTED`, `EXECUTE_AS`, `READONLY_ARGS`. No `signature` (no overloading).
- **Trigger** (`TriggerDef`): DML triggers only; `timing` `After` or `InsteadOf` (`FOR` =
  `After`), `events` from `sys.trigger_events`, `level` `Statement`, `update_columns` empty,
  `condition` `None`, `function` `None`, `definition` = full text, `enabled` = `!is_disabled`,
  extras `ENCRYPTED`, `EXECUTE_AS`, `NOT_FOR_REPLICATION`.
- **Sequence** (`SequenceDef`): start, increment, min, max, cache (`None` for `NO CACHE`),
  cycle, the type (`int`, `bigint`, `decimal(p,0)`, ...) as `props::DATA_TYPE` text in extras,
  no `owned_by`.
- **Type** (`GenericDef`): `definition` = a `CREATE TYPE` statement the fetcher writes from the
  catalog (alias: `CREATE TYPE [s].[t] FROM varchar(20) NOT NULL`; table: `CREATE TYPE [s].[t]
  AS TABLE (...)` with columns, keys and checks), extras `TYPE_KIND`, `BASE_TYPE`, `NOT_NULL`.

### 3.4 Logical types (`DataType` of catalog columns)

| Native | `DataType` |
|---|---|
| `bit` | `Bool` |
| `tinyint` | `Int { bits: 8, unsigned: true }` |
| `smallint` / `int` / `bigint` | `Int { 16 / 32 / 64, unsigned: false }` |
| `real` / `float(n)` | `Float { 32 }` for `real` / `float(1..24)`, else `Float { 64 }` |
| `decimal(p,s)`, `numeric(p,s)` | `Decimal { Some(p), Some(s) }` |
| `money` / `smallmoney` | `Decimal { Some(19), Some(4) }` / `Decimal { Some(10), Some(4) }` |
| `char(n)` / `nchar(n)` | `Char { Some(n) }` |
| `varchar(n)` / `nvarchar(n)` | `VarChar { Some(n) }` (characters, not bytes) |
| `varchar(max)`, `nvarchar(max)`, `text`, `ntext` | `Text` |
| `binary(n)` | `Binary { Some(n) }` |
| `varbinary(n)` | `VarBinary { Some(n) }` |
| `varbinary(max)`, `image` | `VarBinary { None }` |
| `date` | `Date` |
| `time(n)` | `Time { Some(n) }` |
| `datetime` / `smalldatetime` / `datetime2(n)` | `Timestamp { Some(3) / Some(0) / Some(n) }` |
| `datetimeoffset(n)` | `TimestampTz { Some(n) }` |
| `uniqueidentifier` | `Uuid` |
| `json` (2025) | `Json { binary: true }` |
| `xml` | `Other { "xml" }` |
| `vector(n)` (2025) | `Other { "vector" }` (values as JSON array text) |
| `rowversion` / `timestamp` | `Binary { Some(8) }` + `extras::ROW_VERSION` |
| `sql_variant`, `hierarchyid`, `geometry`, `geography`, CLR types | `Other { <name> }` |
| alias types | the base type's `DataType`; `native_type` = `schema.alias` |

Grid editing then follows M3 `convert`: `Other` types are edited as text and converted by the
server (implicit `nvarchar` → `xml`, `hierarchyid`, `geometry`, ... conversion).

### 3.5 Fetcher SQL rules

- `sys.*` catalog views of the session's database, `@P1..` parameters (`nvarchar` for names),
  never `INFORMATION_SCHEMA` for definitions (it truncates and misses extras).
- One query per `list`; `load` may use a few (columns, indexes, keys, checks, comments) but
  checks the token between them.
- Expressions and definitions are returned exactly as stored (`sys.default_constraints`,
  `sys.check_constraints`, `sys.computed_columns`, `sys.indexes.filter_definition`,
  `sys.sql_modules.definition`).
- Version gates: `sys.tables.temporal_type` and `history_table_id` (2016+, always present
  on 2017+), `is_memory_optimized`, `ledger_type` (2022+), `json` / `vector` type names
  (2025). Unknown version (`0.0.0`) counts as current.

## 4. Designer, comparison and grid writes (ddl stream)

### 4.1 Generator rules (`MsSqlDdl`)

On top of the M3 `DdlGenerator` rules (one step per statement, descriptions, order of an
`alter`, `Unsupported` instead of silent skips):

- **Transactions:** SQL Server DDL is transactional, so every step is `transactional` and
  scripts are atomic (`DesignerCapabilities::transactional_ddl = true`); the designer runs
  them in one transaction.
- **Names:** `[schema].[name]` via `BracketQuoting`; constraint and index names unqualified.
  Literals via `quote_str` (`N'...'`). Identifiers up to 128 characters.
- **Renames** (first, from `DdlContext::renames`): `EXEC sp_rename N'<[s].[t]>', N'<new>'`
  (table), `N'<[s].[t].[c]>', N'<new>', N'COLUMN'`, `N'<[s].[t].[ix]>', N'<new>', N'INDEX'`,
  `N'<[s].[constraint]>', N'<new>', N'OBJECT'` (PK, unique, FK, check, default constraints).
  The first argument is the quoted name passed as a string literal.
- **Columns:** `ALTER TABLE [s].[t] ADD [c] <type> [COLLATE x] [NULL|NOT NULL] [CONSTRAINT
  [df] DEFAULT (<expr>)] [IDENTITY(seed, inc)]`; `ALTER TABLE ... DROP COLUMN [c]` (after
  dropping its default constraint, indexes, keys and checks that use it); `ALTER TABLE ...
  ALTER COLUMN [c] <type> [COLLATE x] NULL|NOT NULL` always restates the nullability.
- **ALTER COLUMN limits** the generator must handle by drop + recreate around the alter, in
  the same script: the column's default constraint (except when only the length / precision /
  scale grows), indexes and unique / primary keys that contain it, foreign keys on it or
  referencing it, checks that use it, computed columns that reference it (dropped and re-added:
  destructive). Not alterable at all: `rowversion`, computed and period columns (`Unsupported`
  unless dropped and re-added explicitly), `text` / `ntext` / `image` to anything other than
  their `(max)` successors.
- **Defaults** are named constraints. New: `CONSTRAINT [DF_<table>_<column>] DEFAULT (<expr>)`
  (name truncated to 128; `extras::DEFAULT_CONSTRAINT` of the new column wins when set).
  Changed or removed: `ALTER TABLE ... DROP CONSTRAINT [<old name>]` (from the old column's
  `DEFAULT_CONSTRAINT`), then `ALTER TABLE ... ADD CONSTRAINT [df] DEFAULT (<expr>) FOR [c]`.
  A literal default from the designer (`defaultLiteral`) is quoted with `quote_str`.
- **Identity:** cannot be added to or removed from an existing column, and seed / increment
  cannot change: `Unsupported` ("SQL Server cannot add or remove IDENTITY on an existing
  column; add a new column or recreate the table"). A new column may be `IDENTITY` when the
  table has none.
- **Computed columns:** `[c] AS (<expr>) [PERSISTED]`; any change is drop + add (destructive:
  the column moves to the end).
- **Primary key:** `ALTER TABLE ... ADD CONSTRAINT [pk] PRIMARY KEY [CLUSTERED|NONCLUSTERED]
  (...)`; a change is drop (by name) + add. Foreign keys of other tables that reference it
  block the drop: the server's error is reported ("drop the referencing foreign keys first",
  as on PostgreSQL); the generator does not drop other tables' keys. New PK names default to
  `PK_<table>`.
- **Indexes:** `CREATE [UNIQUE] [CLUSTERED|NONCLUSTERED] INDEX [ix] ON [s].[t] ([c] ASC|DESC,
  ...) [INCLUDE ([c], ...)] [WHERE <predicate>]`; unique constraints: `ALTER TABLE ... ADD
  CONSTRAINT [uq] UNIQUE [CLUSTERED|NONCLUSTERED] (...)`; drop: `DROP INDEX [ix] ON [s].[t]` /
  `ALTER TABLE ... DROP CONSTRAINT [uq]`. Expression keys and prefix lengths: `Unsupported`.
  Columnstore / XML / spatial / hash indexes: kept; changing one is `Unsupported`.
- **Foreign keys:** `ALTER TABLE ... ADD CONSTRAINT [fk] FOREIGN KEY (...) REFERENCES
  [s].[t] (...) ON DELETE ... ON UPDATE ...`; actions `NO ACTION`, `CASCADE`, `SET NULL`,
  `SET DEFAULT`; `Restrict` is `Unsupported`. The server's "may cause cycles or multiple
  cascade paths" error is reported as is.
- **Checks:** `ALTER TABLE ... ADD CONSTRAINT [ck] CHECK (<expr>)` (`WITH CHECK`, the
  default).
- **Comments:** extended property `MS_Description`:
  `EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'...', @level0type =
  N'SCHEMA', @level0name = N's', @level1type = N'TABLE', @level1name = N't' [, @level2type =
  N'COLUMN', @level2name = N'c']`; `sp_updateextendedproperty` when one exists,
  `sp_dropextendedproperty` to remove.
- **Drop:** `DROP TABLE [s].[t]`; children via `ALTER TABLE ... DROP ...` / `DROP INDEX`.
- **Not in M4** (`Unsupported`): reordering columns; changing temporal (system-versioned),
  memory-optimized, graph or ledger tables (they load read-only in the designer); partitioning;
  data compression changes; sparse / column sets; `NOT FOR REPLICATION` changes.
- Destructive: drops, narrowing type changes (shorter length, smaller precision / scale,
  `nvarchar` → `varchar`, `datetime2(n)` with smaller `n`, ...), computed-column re-adds.

### 4.2 Context

`DdlContext::table` is the table as on the server (the designer passes `original`); the
generator reads the old default-constraint names and the indexes / keys / checks that block an
`ALTER COLUMN` from it. `DdlContext::triggers` is not needed (no rebuilds).

### 4.3 Comparator (`MsSqlRules`)

Registered as `RulesComparator(MsSqlRules)` for every designer kind (the stream may replace
`compare::comparator()` with its own `Comparator` that normalizes first, like PostgreSQL). Rules
in `compare.rs` docs: type spellings (`nvarchar(max)` / `nvarchar(-1)`, `float` / `float(53)`,
`real` / `float(24)`, `decimal` / `decimal(18,0)`, `rowversion` / `timestamp`, `sysname` /
`nvarchar(128)`, case), default and check text as stored (redundant parentheses, brackets,
keyword and built-in function case), extras compared only when both sides have them (default
constraint names, `CLUSTERED` of the PK defaulting to `true`), `IdentityDef` start / increment
compared. Round-trip invariant (M3 §3.4) holds on SQL Server.

### 4.4 Designer capabilities (`caps::designer`, expected values)

`transactionalDdl: true`, `createTable` / `dropTable: true`; `renameTable`, `tableComment`,
`addColumn`, `dropColumn`, `renameColumn`, `alterColumnType`, `alterColumnNullability`,
`alterColumnDefault`, `columnComment`, `columnCollation`, `changePrimaryKey`, `addIndex`,
`dropIndex`, `addForeignKey`, `dropForeignKey`, `addCheck`, `dropCheck`: `native`;
`reorderColumns: unsupported`; `autoIncrement: false`, `identity: true`, `generatedStored:
true`, `generatedVirtual: true`; `uniqueConstraints`, `partialIndexes` (filtered),
`descendingIndexes`: `true`; `expressionIndexes`, `indexPrefixLengths`, `deferrableConstraints`:
`false`; `indexMethods: []` (clustering is the `clustered` property: `Capabilities::supports(
Index, CLUSTERED)` is true); `fkActions: [no_action, cascade, set_null, set_default]`;
`checksEnforced: true`; `maxIdentifierLength: 128` (characters). `notes` (in this order):

1. "SQL Server runs the whole change in one transaction: if any statement fails, nothing is
   changed."
2. "SQL Server cannot reorder columns: new columns are added at the end."
3. "IDENTITY cannot be added to or removed from an existing column."
4. "Changing a column's type drops and recreates the default, indexes, keys and checks that use
   it."
5. "Renames use sp_rename: views, procedures and scripts that use the old name are not
   updated."

The same for every supported version (2017+); an unknown version counts as current.

### 4.5 Grid writer (`MsSqlDmlWriter`)

Every plan is one batch with `returning = true` whose single result set has **one row per
affected row** (full shape in `dml.rs` docs):

```sql
SET NOCOUNT ON;
DECLARE @k TABLE ([id] int);
UPDATE [s].[t] SET [name] = @P1 OUTPUT INSERTED.[id] INTO @k WHERE [id] = @P2;
SELECT t.[id], t.[name], ... FROM [s].[t] AS t JOIN @k AS k ON t.[id] = k.[id];
SET NOCOUNT OFF;
```

- **Why:** `OUTPUT` without `INTO` is refused on tables with enabled triggers; `DONE` counts
  and `@@ROWCOUNT` include or depend on trigger statements and `SET NOCOUNT`; counting the rows
  of the final result set is exact in every case. The read-back runs after the triggers, so it
  shows what they changed (`updated_at` stamps).
- **Insert:** `INSERT INTO [s].[t] ([c], ...) OUTPUT INSERTED.<key> INTO @k VALUES (@P1, ...)`
  or `... OUTPUT ... INTO @k DEFAULT VALUES`; generated keys (identity, `NEWID()` defaults)
  come back through `@k`. **Delete:** `DELETE FROM ... OUTPUT DELETED.<key> INTO @k WHERE
  <key>; SELECT k.* FROM @k AS k`.
- `@k` declares the key columns with their `native_type` (alias types allowed: the session is
  on the table's database). Key columns of `text`, `ntext`, `image`, `xml`, CLR or `(max)` types
  cannot be keys: `Unsupported`.
- Never written (`InvalidInput` naming the column): identity (`always`), computed,
  `ROW_VERSION`, `PERIOD` columns. Insert leaves them out.
- `Value::Null` is the keyword `NULL`, never a parameter; every other value is a parameter.
- **Optimistic concurrency** is by key, as in M3 (every update / delete must hit exactly one
  row, else the apply rolls back). `rowversion` is not used as a version check in M4 (the edit
  model carries no original values); its columns are read-only (writer refuses them; the edit
  service does not yet mark them read-only up front, see §11).
- `transactional()` keeps the default `true`.

## 5. Fixtures (fixtures stream)

### 5.1 TLS on the mssql container

- `scripts/fixtures-certs.sh`: add `mssql` to the server certificate's DNS SANs (matches
  `[tls] server_sans`); keep RSA 2048 keys (SQL Server on Linux needs RSA).
- compose: mount `target/fixtures/tls/{server.pem,server-key.pem,ca.pem}` read-only and an
  `mssql.conf` (new file under `tests/docker/init/mssql/` or `tests/docker/tls/`) with
  `[network] tlscert = /etc/oxbase-tls/server.pem`, `tlskey = /etc/oxbase-tls/server-key.pem`,
  `tlsprotocols = 1.2,1.3`, `forceencryption = 0`. The entrypoint wrapper copies the key to a
  file owned by the `mssql` user (uid 10001, mode 0600) and the conf to
  `/var/opt/mssql/mssql.conf` before starting `sqlservr` (`OXBASE_TLS_OWNER` pattern of the
  other engines). Add `mssql` to `material_services` in `fixtures-up.sh`.
- `forceencryption` stays `0` so plain and `Disable` connections work; the TLS-only login of
  5.2 proves enforcement. With the fixture CA, `VerifyFull` uses server name `localhost`
  (direct) or `mssql` (through the bastion); `VerifyCa` with any name.
- TDS 8.0 strict: SQL Server 2025 accepts it on the same port with this certificate
  (`[engines.mssql] tds8_strict = true`); the driver live test connects strict with
  `VerifyFull`.

### 5.2 TLS-only login

SQL Server cannot require TLS per login, and `forceencryption = 1` would require it for every
login. `03-tls-user.sql` creates login and user `oxbase_tls` (password as in `fixtures.toml`,
`CHECK_POLICY = OFF`, db_owner in `oxbase_test`) and a server LOGON trigger:

```sql
CREATE TRIGGER oxbase_tls_only ON ALL SERVER FOR LOGON AS
BEGIN
    IF ORIGINAL_LOGIN() = N'oxbase_tls'
       AND CONVERT(nvarchar(10), CONNECTIONPROPERTY('encrypt_option')) <> N'TRUE'
        ROLLBACK;
END;
```

`CONNECTIONPROPERTY` needs no `VIEW SERVER STATE`. A plain connection as `oxbase_tls` fails
with error 17892 (logon failed because of a trigger), which the driver reports as
`Error::Sql { code: Some("17892") }`; the conformance TLS cell only expects "`Disable` as
`oxbase_tls` fails".

### 5.3 Seeds

The image has no init-script directory; the existing entrypoint wrapper
(`tests/docker/init/mssql/entrypoint.sh`) applies `/oxbase-init/*.sql` in name order once and
marks `oxbase_test` seeded with an extended property. New scripts: `03-tls-user.sql`,
`04-m2.sql` (`[m2.mssql]`: schema `m2`, the M2 shape with the differences noted there, plus
database `oxbase_m2_other` with `dbo.m2_other_t`), `05-m3.sql` (`[m3.mssql]`: schema `m3`,
edit tables and the audit trigger), `06-m4.sql` (`[m4]`: schema `m4`). Each script must be
re-runnable on a fresh `oxbase_test` only (the wrapper drops a partial seed) and use `GO`
between batches that must be first in a batch (`CREATE VIEW` / `FUNCTION` / `PROCEDURE` /
`TRIGGER` / `TYPE`).

`m4.m4_types` row `id = 1` (T-SQL literals; the driver live test asserts the Arrow values):

| Column | Literal | Arrow value |
|---|---|---|
| `c_tinyint` | `255` | `Int64 255` |
| `c_smallint` | `-32768` | `-32768` |
| `c_int` | `2147483647` | `2147483647` |
| `c_bigint` | `-9223372036854775808` | `i64::MIN` |
| `c_bit` | `1` | `true` |
| `c_decimal` | `12345678901234.5678` | `"12345678901234.5678"` |
| `c_numeric` | `1234567890123456789012345678.0123456789` | same text |
| `c_money` | `922337203685477.5807` | `Decimal128(19,4)` 922337203685477.5807 |
| `c_smallmoney` | `-214748.3648` | -214748.3648 |
| `c_float` | `1.5E300` | `1.5e300` |
| `c_real` | `3.25` | `3.25` |
| `c_date` | `'0001-01-01'` | `Date32` 0001-01-01 |
| `c_time` | `'23:59:59.1234567'` | 23:59:59.123456 |
| `c_datetime` | `'2026-01-15T10:20:30.997'` | 2026-01-15 10:20:30.997 |
| `c_datetime2` | `'9999-12-31T23:59:59.9999999'` | 9999-12-31 23:59:59.999999 |
| `c_smalldatetime` | `'2026-01-15T10:21:00'` | 2026-01-15 10:21:00 |
| `c_dto` | `'2026-01-15T10:20:30.1234567+03:30'` | `"2026-01-15T10:20:30.1234567+03:30"` |
| `c_char` | `'abc'` | `"abc       "` (padded to 10) |
| `c_varchar` | `'café'` (database collation `SQL_Latin1_General_CP1_CI_AS`) | `"café"` |
| `c_varchar_max` | `REPLICATE(CAST('x' AS varchar(max)), 9000)` | 9000 × `x` |
| `c_nchar` | `N'☃'` | `"☃"` + 9 spaces |
| `c_nvarchar` | `N'Grüße 漢字 😀'` | same |
| `c_nvarchar_max` | `REPLICATE(CAST(N'ab' AS nvarchar(max)), 5000)` | 10000 characters |
| `c_text` / `c_ntext` | `'old text'` / `N'old ntext'` | same |
| `c_binary` | `0x0102` | `01 02` + 14 zero bytes |
| `c_varbinary` | `0xCAFEBABE` | 4 bytes |
| `c_varbinary_max` | `CAST(REPLICATE(CAST(0xAB AS varbinary(max)), 10000) AS varbinary(max))` | 10000 × `0xAB` |
| `c_image` | `0xFFD8FF` | 3 bytes |
| `c_guid` | `'6F9619FF-8B86-D011-B42D-00C04FC964FF'` | uuid `6f9619ff-8b86-d011-b42d-00c04fc964ff` |
| `c_xml` | `N'<a b="1"/>'` | the server's serialization (`<a b="1" />`) |
| `c_json` | `N'{"a":1,"b":[true,null]}'` | text equal as JSON |
| `c_vector` | `'[1,2,3]'` | text: a JSON array equal to `[1,2,3]` as numbers |
| `c_variant` | `CAST(42 AS int)` | `"42"` |
| `c_hierarchyid` | `hierarchyid::Parse('/1/2/')` | non-empty bytes; `CAST(c_hierarchyid AS nvarchar(100))` = `/1/2/` |
| `c_geometry` | `geometry::STGeomFromText('POINT(1 2)', 0)` | non-empty bytes |
| `c_geography` | `geography::Point(47.65, -122.34, 4326)` | non-empty bytes |
| `c_rowversion` | (automatic) | 8 bytes |

### 5.4 SSH bastion

`[ssh.targets] mssql = mssql:1433`: the bastion reaches the container by its compose service
name; the TLS-over-SSH cell verifies name `mssql` (SAN added in 5.1).

## 6. SQL text (`oxbase-sql`, sql stream)

`SqlDialect::TSql` (`Engine::MsSql`) rules, the sql stream's acceptance list:

- **Batches:** a line holding only `GO` (case-insensitive), optionally preceded by spaces or
  tabs, optionally followed by a count and a `--` comment, ends a batch. `GO` inside strings,
  quoted identifiers or comments, or with other text on the line (`GOTO x`, `SELECT 1 GO`), is
  not a separator. The separator line is not part of any statement. **One statement = one
  batch**: `;` never splits (T-SQL batches are sent whole; `CREATE PROCEDURE` must be alone in
  its batch).
- `GO <n>` (repeat count): parsed and accepted; in M4 the batch runs **once** (the query
  service has no repeat; the count is a documented gap, §11).
- Lexing: `'...'` and `N'...'` strings with `''` doubling (no backslash escapes); `[...]`
  identifiers with `]]` doubling; `"..."` identifiers; `--` line comments; `/* ... */` block
  comments that **nest**.
- `StatementKind` hint: `Query` for batches whose first keyword is `SELECT` / `WITH` /
  `EXEC[UTE]` / `VALUES`, else `Other` (best effort).
- `redact_secrets` for T-SQL: string literals (`'...'` and `N'...'`) after `PASSWORD =`
  (`CREATE` / `ALTER LOGIN`, `CREATE USER ... WITH PASSWORD`), `SECRET =` (`CREATE DATABASE
  SCOPED CREDENTIAL`, `CREATE CREDENTIAL`), `IDENTITY = ... , SECRET =`, `ENCRYPTION BY
  PASSWORD =`, `DECRYPTION BY PASSWORD =` become `N'***'` / `'***'`; `password=` / `pwd=`
  inside connection-string literals (`OPENROWSET`, linked servers) become `***`.
- Tests: every rule above, multi-byte text, a 1 MB script well under 100 ms (M2 §10).

## 7. App parity (app stream)

- **Connection dialog** (SQL Server): host, port (1433), database (optional: "login default"),
  user, password, TLS mode (default `prefer`), "TDS 8.0 strict encryption" checkbox
  (`options.encrypt = strict`; only with Require / VerifyCa / VerifyFull), instance name
  (`options.instance`; needs a port, shown as a hint), application intent (read-only
  checkbox), SSH as for the other engines. No Windows / Entra auth fields in M4.
- **Editor:** Monaco language `sql` for `mssql` (Monaco's built-in `sql` definition is T-SQL
  flavoured: brackets, `GO`); "run current statement" uses `statement_at` with the T-SQL
  dialect, so the current `GO` batch runs. Show that `PRINT` output is not captured (a short
  note in the messages area when the engine is SQL Server) rather than nothing.
- **Grid:** values arrive per §2.5; `datetimeoffset` and decimals as text; binary as hex.
  Read-only cells from the edit service (identity, computed); `rowversion` and period columns
  are refused by the writer with a clear error until the services mark them (§11).
- **Designer:** type list for SQL Server (from a static list in the UI: `bit, tinyint,
  smallint, int, bigint, decimal(p,s), numeric(p,s), money, smallmoney, real, float, date,
  time(n), datetime, datetime2(n), smalldatetime, datetimeoffset(n), char(n), varchar(n|max),
  nchar(n), nvarchar(n|max), binary(n), varbinary(n|max), uniqueidentifier, xml, json` (2025+,
  from `serverVersion`), `sql_variant`, `hierarchyid`, `geometry`, `geography`); options from
  `capabilities_get` (`designer`: identity instead of auto-increment, computed persisted or
  not, filtered-index predicate, no expression keys, no prefix lengths, no deferrable) and, for
  options the designer capabilities do not carry (clustered indexes, included columns), from
  the engine (`mssql`): a "Clustered" checkbox on indexes and the primary key and an
  "Included columns" list, written as the `clustered` / `include_columns` extras; notes shown
  with the preview.
- **Tree:** the `Types` folder; database nodes of non-online databases show the server's
  error when expanded.
- **e2e:** only if cheap: the smoke test stays on PostgreSQL in M4.

## 8. Conformance and CI (fixtures stream)

### 8.1 CI lines

In `.github/workflows/ci.yml`, conformance job: add `-p oxbase-mssql` to the `--no-run` build
line and to the "Run M1/M2/M3 live tests" line (rename it to M1-M4). Add
`OXBASE_CONFORMANCE_REQUIRE_M4: "1"` to the harness step once the integrator has uncommented
the dialect registration.

### 8.2 Conformance (`crates/oxbase-conformance`)

- `matrix::driver_milestone`: SQL Server no longer pending once its driver is registered.
- `connect.rs`: `long_statement(MsSql)` = `WAITFOR DELAY '00:00:30'`; `rows_statement(MsSql)` =
  `SELECT value AS n, CONVERT(char(32), HASHBYTES('MD5', CAST(value AS varchar(10))), 2) AS h
  FROM GENERATE_SERIES(1, 200000)`; TLS cells with `tls_user` and the CA; SSH cells to
  `mssql:1433`.
- `M2_ENGINES` / `M3_ENGINES` (or a new `M4_ENGINES`) include `Engine::MsSql`;
  `OXBASE_CONFORMANCE_REQUIRE_M4=1` makes every SQL Server `connect`, `fetch_list`,
  `roundtrip` and `capabilities` cell required (the **5-engine guard**: all five engines must
  pass M1-M3 cells, nothing pending).
- Round-trip cells on SQL Server use `[m3.mssql]` schema `m3`; scratch cleanup lists
  `m3_rt_%` tables of schema `m3` through the Table fetcher.

### 8.3 Services live tests (integration)

`crates/oxbase-services/src/query/live_tests.rs`: `large_result("mssql", Engine::MsSql)`
(`SELECT value AS g FROM GENERATE_SERIES(1, 1000000)`), `cancel_mid_stream`, `multi_statement`
(`SELECT 1 AS a; SELECT 2 AS b` in one batch: two result sets), `history_records` (`CREATE
LOGIN ... WITH PASSWORD = N'marker'` redacted); `edit/live_tests.rs` and
`designer/live_tests.rs`: the `mssql` rows. These files are not owned by an M4 stream; the
integrator adds the rows (they only call the generic paths).

## 9. Dialect changes made by Design (done, tested)

- `BracketQuoting::qualify` drops the catalog (`[schema].[name]`, columns
  `[schema].[table].[col]`), like `PgQuoting`.
- `Quoting::top_clause(n) -> Option<String>` (default `None`); `BracketQuoting` returns
  `TOP (n)`, and `sql::select_table_data` writes `SELECT TOP (n) * FROM ...` instead of a
  trailing limit. `BracketQuoting::limit_clause` is `OFFSET 0 ROWS FETCH NEXT n ROWS ONLY`
  (valid only after `ORDER BY`).
- `LexRules::bracket_identifiers`: `check_expression` skips `[...]` (with `]]`) on
  `BracketQuoting` and on `AnsiQuoting` (SQLite also reads `[...]` as an identifier; before
  this, `[a'] ; DROP TABLE t --'` passed the SQLite check as one string).
- Unchanged and already right for T-SQL: `@Pn` placeholders, `N'...'` literals, `0x..` bytes,
  `1` / `0` booleans, `CAST(x AS nvarchar(max))` text casts, emulated `NULLS FIRST/LAST`, `[`
  as a `LIKE` special (escaped with `!`).

## 10. Dependencies

- `tiberius` 0.13.0 (`default-features = false`, `tds73`, `chrono`) in
  `[workspace.dependencies]`. New crates in `Cargo.lock` (all MIT and / or Apache-2.0, except
  `encoding_rs`: `(Apache-2.0 OR MIT) AND BSD-3-Clause`, allowed): `tiberius`,
  `tiberius-macros`, `asynchronous-codec`, `connection-string`, `encoding_rs`, `futures-timer`,
  `pretty-hex`, `secrecy`, `simdutf8`, `core_detect`, `multiversion_no_op`. No TLS stack:
  `aws-lc-*`, `openssl*` and `native-tls` stay out of the graph (cargo-deny in CI).
- `oxbase-mssql` depends on `oxbase-types`, `oxbase-driver`, `oxbase-dialect`, `oxbase-net`,
  `tiberius`, `rustls`, `tokio-rustls` (ring), `tokio-util` (`compat`), `tokio`, `futures`,
  `async-trait`, `arrow-array`, `arrow-schema`, `bytes`, `chrono`, `uuid`, `serde_json`,
  `tracing`; dev: `pretty_assertions`, `tokio`, `toml`. `oxbase-services` depends on
  `oxbase-mssql`.
- `deny.toml` unchanged: the licenses above are already allowed.

## 11. Known gaps and follow-ups (accepted for M4)

1. No `PRINT` / low-severity `RAISERROR` messages (tiberius drops INFO tokens).
2. No row counts for SQL batches (tiberius' batch stream drops DONE counts); parameterized
   statements have them.
3. Coarse result metadata: one integer type, decimals as text.
   Follow-up for 1-3: a TDS client in `oxbase-mssql` (MS-TDS is public), or upstream changes to
   tiberius (it marks INFO and metadata as `TODO(surface)`).
4. `GO <n>` runs the batch once.
5. Named instances need a port (no SQL Browser); no Windows / Entra authentication; no client
   certificates.
6. `rowversion` and period columns are refused by the writer but not marked read-only by the
   edit service (`oxbase_services::edit` has no engine-neutral "server-maintained column" flag;
   a one-line follow-up reading `extras::ROW_VERSION` / `PERIOD`, or a typed flag on
   `ColumnDef`, post-M4). No row-version concurrency check.
7. DDL triggers, synonyms, users / roles / permissions are not in the tree.
8. Reordering columns and changing temporal / memory-optimized / graph / ledger tables are
   unsupported in the designer.

## 12. Exit-check tests the streams must write

Live tests are `#[ignore]`d and run in the conformance CI job against the Docker fixtures.

- **driver** (`crates/oxbase-mssql/tests/driver_live.rs` + unit tests): PRELOGIN encode /
  decode and the replay adapter on an in-memory duplex; option parsing (unknown key, strict
  with Prefer, instance without port); live: plain connect as `sa` (`tls_in_use` false with
  `Disable`), `Prefer` (true), `VerifyFull` with the CA and name `localhost` (true), wrong name
  fails, `VerifyCa` with a wrong name passes, strict + `VerifyFull`, `oxbase_tls` with
  `Disable` fails and with `VerifyFull` works; `current_database`, `server_version` 17.x;
  `m4_types` row 1 decoded exactly as §5.3 and row 2 all NULL; every `Value` kind round-trips as
  a parameter (`SELECT @P1`); multiple result sets of `m4.m4_messages`; `RowsAffected` on the
  RPC path; a batch with `#temp` then a second `execute` sees it; `USE oxbase_m2_other` then
  `DB_NAME()`; cancel of `WAITFOR DELAY '00:00:30'` by token and by `cancel_running` within 2 s
  and the next `SELECT 1` works (or the connection reports closed); a 200k-row stream; begin /
  commit / rollback / dropped Tx; the marker-password leak check.
- **fetch** (`tests/fetch_live.rs`): every `[m2.mssql].objects` entry listed under its parent
  and loaded with the asserted shape (M2 §8, with the SQL Server differences); `m4` objects:
  identity seed / increment, computed persisted / not, rowversion, named defaults, filtered
  unique index with predicate, included columns, clustered flags, temporal extras and history
  table, indexed view index under the view, view trigger, inline / multi-statement TVFs
  (`function_kind`), alias and table types, the DDL trigger not listed; system objects flagged;
  `oxbase_m2_other` listed and read through a session on it; a ref of another database is
  `InvalidInput`; pre-cancelled token is `Cancelled`; missing object `NotFound`;
  `missing_layout_fetchers` empty; a review / unit test that no query formats a name.
- **ddl** (`tests/m4_live.rs` + unit tests): generator unit tests per kind (create / alter /
  drop, bracket quoting of reserved words, `]` and unicode in names, `N''` literals, comments
  via extended properties, default-constraint drop + add, ALTER COLUMN with its blockers dropped
  and recreated, identity changes `Unsupported`, order of an alter, destructive flags);
  `MsSqlRules` tests (type spellings, parenthesized defaults and checks); `designer()` values of
  §4.4; `DmlWriter` plans (bound values, `NULL` keyword, `@k` declaration, refused columns,
  defaults-only insert); live: create → fetch → compare-empty → alter → fetch → compare-empty →
  drop for the 5 kinds on `[m3]` scratch names in schema `m3`; DML plans against `m3_edit_pk`
  (with its audit trigger) count exactly one row for insert / update / delete and read back
  the generated id and `total`; a conflict (row deleted by another session) counts 0.
- **sql**: §6 tests.
- **fixtures**: the stack starts healthy with TLS; `03`-`06` scripts apply on a fresh volume;
  conformance cells for SQL Server pass; `OXBASE_CONFORMANCE_REQUIRE_M4=1` set in CI after
  integration; the fixture-file test covers `[m2.mssql]`, `[m3.mssql]`, `[m4]`.
- **app**: Vitest for the SQL Server dialog fields (strict only with verifying modes, instance
  hint), the editor language mapping (`mssql` → `sql`), designer type list and identity
  option, the PRINT note.

## 13. As built (integration, 2026-11-01)

The streams were merged on `milestone/m4`; CI passed with the 5-engine guard on
(`OXBASE_CONFORMANCE_REQUIRE_M4=1`). Where the build differs from the sections above:

- **§2.4 commit.** `Tx::commit` runs `IF @@TRANCOUNT = 0 RAISERROR(..., 16, 1) ELSE COMMIT
  TRANSACTION`: a transaction the server already ended (deadlock 1205, `XACT_ABORT`, a
  trigger's `ROLLBACK`) makes the commit fail with error 50000 instead of reporting a commit
  that did not happen. `begin()` still nests inside a `BEGIN TRAN` the editor opened on the same
  session (that commit only decrements `@@TRANCOUNT`).
- **§2.4 / §11 row counts.** tiberius does not expose the `DONE_COUNT` bit, so the RPC path may
  still report `RowsAffected(0)` for control-of-flow or failed statements; the zero that directly
  follows a result set (the `DONEPROC` after `EXEC proc @P1`) is skipped. Plain batches report no
  counts (gap 2): the services multi-statement test sees `rows_affected = None` for every T-SQL
  batch.
- **§5.3 money.** tiberius 0.13 decodes `money` through `f64`, so values above about 9.0e11 lose
  their last digits: `c_money = 922337203685477.5807` is checked with a tolerance of 2048 units
  of the fourth decimal (`driver_live`). Exact money needs the follow-up of §11 (a patched or own
  TDS decoder).
- **§5.1 / §5.2 fixture.** `CONNECTIONPROPERTY` has no `encrypt_option`, so the logon trigger
  reads `sys.dm_exec_connections` (`WITH EXECUTE AS N'sa'`; `ORIGINAL_LOGIN()` still names the
  connecting login). The container runs `sqlservr` as root (compose `user: root`) with a
  root-owned 0600 copy of the key instead of dropping to uid 10001. `tlsprotocols = 1.2,1.3` is
  set as specified.
- **§12 ddl re-fetch.** `tests/m4_live.rs` reads every table back through the registered `Table`
  fetcher (its stand-in catalog loader is gone), so generator, fetcher and comparator are tested
  together.
- **§8.3 services.** `query::live_tests` runs the T-SQL multi-statement script as one statement
  per `GO` batch with a `#temp` table across batches and a batch of two `SELECT`s (two result
  sets of one statement); history redacts `ALTER LOGIN ... WITH PASSWORD = N'...'`. The edit and
  designer live tests use schema `dbo` of `oxbase_test`. After a cancel mid-stream the query
  service gives the driver 1 s to finish the attention drain of a streaming `GENERATE_SERIES`
  and then runs the editor's next statement on a new session (as on MySQL / MariaDB).
