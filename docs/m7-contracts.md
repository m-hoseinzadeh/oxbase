# M7 contracts

Status: frozen for M7 on 2026-11-02 (Design step). M7 exit check (PLAN.md §6): **browse, edit
and query on both engines** (MongoDB and Redis), live in CI. Scope: the MongoDB driver, tree,
document grid with tree / JSON views, find / aggregate builder and mongo-shell subset; the
Redis driver (standalone, cluster, sentinel), key browser, typed value editors, console and
pub/sub. Fixtures: MongoDB 7.0.45 (`[engines.mongodb]`, port 15417; 8.x refuses the CI kernel,
SERVER-121912) and Redis 8.10.3 (`[engines.redis]`, port 15479), plus the M7 additions of §9.

Changes to these contracts go through the Design owner. Streams code against them and must not
change public signatures; adding private helpers, private fields, private modules and tests is
always fine. Rust doc comments on the items named here are part of the contract and are more
detailed than this file (start with the module docs of `oxbase_mongo::{driver, shell, request,
arrow}`, `oxbase_mongo::dialect`, `oxbase_redis::{driver, console, command}`,
`oxbase_redis::dialect`, `oxbase_dialect::{script, document, keyvalue, pubsub}`,
`oxbase_types::{document, keyvalue}` and `oxbase_services::{script, documents, keys, pubsub}`).

## 1. Who owns what

Bodies marked `unimplemented!("m7: <stream>")` belong to that stream. No test may call one
until its stream has filled it. Seven streams start from the Design head in parallel.

| Area | Path | Owner | State at hand-off |
|---|---|---|---|
| Dependencies (`mongodb` 3.9.1, `bson` 3.1.0, `redis` 1.8.1, `rquickjs` 0.14.0, `serde_json` `preserve_order`), `Cargo.lock`, `deny.toml` (BSL-1.0) | root | Design | done (§2.4) |
| `oxbase_types::{document, keyvalue}` (requests, edits, key model, `BinaryText`), `props::{KEY_TYPE, TTL_MS, KEY_COUNT, TRUNCATED, KEY_HEX, DOCUMENT_COUNT}`, `Endpoint::{parse, parse_list}` | `crates/oxbase-types` | Design | done, tested |
| `ResultSource` | `crates/oxbase-driver` | Design | done |
| Roles `ScriptLanguage` (+ `StatementGuard`, `CONFIRMATION_REQUIRED`, `script::{statement, trim_range}`), `DocumentStore`, `KeyValueStore`, `PubSub` / `Subscription` / `PubSubEvent` / `SubscribeRequest`; registry slots and `missing_m7_roles`; `TreeLayout::{with_label, kind_label, folder_label}`; `BulkLoader::{transactional, create_target}`, `BulkMethod::DocumentInsert` | `crates/oxbase-dialect` | Design | done, tested |
| `script::StatementSource` and its use in `QueryService` (split, guards, history redaction, `statement_at_with`), `QueryService::run_source`, `RunOptions::confirm_destructive`, folder labels in `TreeService`; `DocumentService`, `KeyService`, `PubSubService` bodies; the commented registration lines in `drivers.rs` / `dialects.rs` | `crates/oxbase-services` | Design | done (unit-tested; live tests by the streams) |
| Commands of §8, `NoSqlState`, `close_connection` closing subscriptions, `statement_at` for every engine | `crates/app-tauri` | Design (contract) | forwarding bodies done |
| TS payloads and wrappers | `ui/src/api/nosql.ts`, `ui/src/api/types.ts` (`RunOptions.confirmDestructive`), `ui/src/api/io.ts` (`document_insert`) | Design | done, tested |
| `oxbase-mongo`: crate layout, `options` (parsed + tested), `extras`, `dialect/mod.rs` (registration, layout, capabilities), `ShellLimits` / `ShellSession::new` | `crates/oxbase-mongo/src/{lib.rs, options.rs, dialect/mod.rs}`, parts of `shell/mod.rs` | Design | done, tested |
| `oxbase-redis`: crate layout, `options` (parsed + tested), `command::parse_command_line` (done + tested), `extras`, `dialect/mod.rs` | `crates/oxbase-redis/src/{lib.rs, options.rs, command.rs, dialect/mod.rs}` | Design | done, tested |
| MongoDB driver: SOCKS5 dialer, TLS, auth, connect, `execute` dispatch, cancel, streaming; uncomment `oxbase_mongo::register` in `default_drivers` and update the two driver-list tests (`crates/oxbase-services/src/drivers.rs`, `crates/app-tauri/src/backend.rs` `ships_the_m1_and_m4_drivers`) | `crates/oxbase-mongo/src/driver/**`, `crates/oxbase-mongo/tests/driver_live.rs`, those lines | **mongo-driver** | skeleton |
| Redis driver: resolver hook, TLS, auth, RESP2/3, standalone / cluster / sentinel connect, `execute` + reply rendering, cancel; uncomment `oxbase_redis::register` and update the same two tests | `crates/oxbase-redis/src/driver/**`, `crates/oxbase-redis/tests/driver_live.rs`, those lines | **redis-driver** | skeleton |
| MongoDB fetchers, document store (requests as `ResultSource`, `request_text`, `get`, optimistic edits), BSON → Arrow, Extended JSON, import loader; the engine-neutral honouring of `BulkLoader::{transactional, create_target}` in the import service | `crates/oxbase-mongo/src/{dialect/fetch.rs, dialect/documents.rs, dialect/bulk.rs, request.rs, arrow.rs, ejson.rs}` (+ private modules), `crates/oxbase-mongo/tests/{fetch_live, documents_live}.rs`, `crates/oxbase-services/src/import/**` (those hooks only), `crates/oxbase-services/tests/m7_mongo_live.rs` | **mongo** | skeleton |
| Redis fetchers, key-value store (scan / value sources, info, atomic edits), pub/sub, cluster routing of scans | `crates/oxbase-redis/src/dialect/{fetch, keys, pubsub}.rs` (+ private modules), `crates/oxbase-redis/tests/{fetch_live, keys_live, pubsub_live, cluster_live}.rs`, `crates/oxbase-services/tests/m7_redis_live.rs` | **redis** | skeleton |
| Mongo-shell subset (splitter, guards, redaction, QuickJS session) and the Redis console language (splitter, guards, redaction) | `crates/oxbase-mongo/src/shell/**`, `crates/oxbase-redis/src/console.rs`, `crates/oxbase-mongo/tests/shell_live.rs`, `crates/oxbase-redis/tests/console_live.rs` | **shell** | skeleton |
| UI: connection dialog fields, document grid + tree / JSON views, find / aggregate builder, shell and console editors, key browser + typed editors, pub/sub panel, confirmation dialogs; webview smoke steps for both engines | `ui/src` except `ui/src/api/**`, `ui/e2e/**`, `scripts/e2e-*.sh`, the webview smoke job's env line (`OXBASE_E2E_REQUIRE_M7`) | **app** | - |
| Fixtures (TLS on mongodb / redis, SSH targets, seeds, cluster, sentinel, SANs), conformance connection checks and `fetch_list` cells for mongodb and redis, the 7-engine guard; the conformance job lines | `tests/docker/**`, `scripts/{fixtures-certs, ci-fixtures, fixtures-up}.sh`, `crates/oxbase-conformance/**`, `[tls] server_sans` in `tests/fixtures.toml` (merging `[m7.tls] server_sans_add`), the conformance job of `.github/workflows/ci.yml` | **conformance** | - |

**Integration** (after the streams): uncomment `oxbase_mongo::dialect::register` and
`oxbase_redis::dialect::register` in `crates/oxbase-services/src/dialects.rs` (each registers all
its cells at once, so it waits for the engine's driver, cell and shell streams), extend the
`dialects.rs` tests (`missing_m7_roles` empty for both), set `OXBASE_CONFORMANCE_REQUIRE_M7=1`,
`OXBASE_SERVICES_REQUIRE_M7=1` and `OXBASE_E2E_REQUIRE_M7=1` in CI, and run the shell live tests
(they need both drivers).

**CI lines.** Live tests of `oxbase-mongo` / `oxbase-redis` run only once `-p oxbase-mongo -p
oxbase-redis` are in the conformance job's two cargo lines (build and "Run M1-M5 live tests").
The conformance stream adds both words; the driver streams may add the same two words on their
own branches (a trivial merge), so their live tests run before the conformance stream merges.
The cluster and sentinel fixtures start with the others in `scripts/ci-fixtures.sh up`.

Streams add no crate to `Cargo.lock`: every dependency is already in the two crates'
`Cargo.toml`. A stream that needs another crate adds it with an explicit version to its own
crate, checks the license against `deny.toml` and reports it for hoisting.

## 2. Drivers and libraries (evaluated)

### 2.1 MongoDB: `mongodb` 3.9.1, official (Apache-2.0, MSRV 1.88)

Read from the registry source (`src/runtime/{stream, tls_rustls}.rs`, `src/client/options.rs`,
`Cargo.toml`).

| Requirement | `mongodb` 3.9.1 | Decision |
|---|---|---|
| rustls + ring, no aws-lc / OpenSSL / native-tls | `rustls-tls` = `rustls/ring` + `tokio-rustls/ring` (both `default-features = false`) + `webpki-roots`; `rustls-tls-aws-lc` and `openssl-tls` are separate features. The crate refuses to build without one TLS feature. | `default-features = false, features = ["compat-3-3-0", "bson-3", "rustls-tls", "dns-resolver", "socks5-proxy"]`. `cargo tree` shows no `aws-lc-*`, `openssl*`, `native-tls`; the `deny.toml` bans keep it that way. `webpki-roots` (CDLA-Permissive-2.0) is already allowed by its crate exception. |
| TLS server name vs dial address (M1 §2) | **Fails.** `tls_connect(host, ...)` verifies the host it dials; no override, no custom connector, no DNS hook. Also no chain-only mode (`VerifyCa`), roots only webpki-roots or a CA *file*, client key only from a file. | **In-process SOCKS5 dialer** (driver docs): the `socks5-proxy` hook makes the driver `CONNECT` *by host name*; oxbase's dialer (127.0.0.1:0, per-client random username / password) dials the SSH tunnel for the profile's server endpoint (else the host itself) and runs the TLS handshake with `oxbase_net::tls::client_config` (all of M1 §3: `VerifyCa`, platform roots, inline PEM, keychain client keys) verifying `tls_server_name()`. The driver's own TLS stays off. Like M4 (our own rustls stream), but through the library's proxy hook instead of a vendored patch. |
| SCRAM | SCRAM-SHA-1 and SCRAM-SHA-256 built in (`pbkdf2`, `hmac`, `sha2`), negotiated by default; `MONGODB-X509`, `PLAIN`. | All four offered (§7.1); X.509 uses the dialer's TLS client certificate. |
| Cancel | Dropping a cursor sends `killCursors` in the background; `killOp` is an ordinary command; operations accept a `comment`. | Comment-tagged operations + `$currentOp {ownOps: true}` + `killOp` on another pooled connection (through the dialer, so through SSH), plus dropping the cursor; within 2 s. |
| Streaming | `Cursor` with `batchSize`; `getMore` per batch. | `batchSize` 1000; Arrow per server batch; `maxRows` / disk cap drop the cursor. |
| SRV | `dns-resolver` feature (hickory-resolver 0.26, MIT OR Apache-2.0, `system-config` + `tokio`, no TLS features). | **SRV is in M7** for profiles without SSH (option `srv`); with SSH it is refused (a tunnel reaches one server). |
| Replica sets / direct connection | Topology discovery; `directConnection`. | Host list + `replica_set` option; SSH profiles always `directConnection` (§7.1). |
| Transactions | Need a replica set or sharded cluster. | Not offered in M7 (`begin()` is `Unsupported`; document edits apply one by one, §5.1). |

Alternatives: vendoring `mongodb` with a connector patch (as `tiberius` in M4) was rejected: the
crate is large and the SOCKS hook already gives exactly the dial / name split we need.

### 2.2 Redis: `redis` 1.8.1 (BSD-3-Clause, MSRV 1.88)

`2.0.0-rc.2` exists but is a release candidate; M7 uses the current stable 1.8.1. Read from the
registry source (`src/connection.rs`, `src/client.rs`, `src/io/dns.rs`, `src/aio/**`,
`src/cluster_handling/**`, `src/sentinel.rs`, `src/tls.rs`).

| Requirement | `redis` 1.8.1 | Decision |
|---|---|---|
| rustls + ring | `tls-rustls` = `rustls` with **no** provider feature + `rustls-native-certs`; the config is built with `ClientConfig::builder()` (process default provider). `tls-native-tls` is separate. | `default-features = false, features = ["aio", "tokio-comp", "tokio-rustls-comp", "tls-rustls-insecure", "cluster-async", "sentinel", "acl", "streams"]`; oxbase installs ring as the process default (`oxbase_net::tls::install_default_crypto_provider`) before connecting. |
| TLS server name vs dial address | `AsyncConnectionConfig::set_dns_resolver` / `ClusterClientBuilder::async_dns_resolver`: the client resolves every host through an `AsyncDNSResolver` it is given, while TLS uses the host name. | The client is built for the server host (or `tls.server_name`); the resolver maps that host and port to `dial_endpoint()` (the tunnel). `VerifyFull` therefore works through SSH. |
| TLS modes | `TcpTls { insecure }` (`tls-rustls-insecure`), `danger_accept_invalid_hostnames` (chain only), `TlsCertificates { root_cert: PEM bytes, client_tls: PEM bytes }`. | Mapping table in the driver docs: `Prefer` = TLS then plain fallback, `Require` = insecure, `VerifyCa` = invalid hostnames accepted, `VerifyFull` = full; CA only from `ca_cert` when set, else the platform store (same as M1). |
| Auth / ACL | `RedisConnectionInfo { username, password }`, `HELLO 3 AUTH` / `AUTH`. | Profile user (empty = `default`) and password. |
| RESP3 | `ProtocolVersion::RESP3`, typed replies (`Map`, `Set`, `Double`, `Boolean`, `VerbatimString`, `BigNumber`, `Push`, `Attribute`). | `auto`: RESP3 on 6.0+, else RESP2 (§7.2). |
| Cancel | No in-band cancel in RESP. | `CLIENT UNBLOCK <id> ERROR` (blocking commands) or `SCRIPT KILL` / `FUNCTION KILL` from a side connection; otherwise drop the reply and replace the multiplexed connection (warning notice). Never `CLIENT KILL`. |
| Streaming | Replies arrive whole. | Cursor commands (`SCAN`, `HSCAN`, `SSCAN`, `ZSCAN`), `LRANGE` / `XRANGE` windows of 1000 in the key-value cell; never `KEYS` (§3.2). |
| Cluster | `cluster-async`: slot map, `MOVED` / `ASK`, `node_address_map`, the same resolver hook. | `ClusterClient` over the seeds; scans per master. No SSH (one tunnel reaches one node). |
| Sentinel | `SentinelClient::async_get_client` (master address from the sentinels). | Sentinel mode with `master_name` and optional sentinel auth; no SSH. |
| Pub/sub | RESP3: subscribe on a multiplexed connection with a push sender; RESP2: `aio::PubSub` on a dedicated connection. | A dedicated connection per subscription (§6). |

New licenses in the graph: `xxhash-rust` (BSL-1.0, via redis) — `BSL-1.0` is added to the
`deny.toml` allow-list (OSI-approved, permissive). `crc16` (MIT), `combine` (MIT), `arcstr`,
`async-lock` etc. are on the existing list.

### 2.3 `rquickjs` 0.14.0 (MIT; QuickJS-ng, MIT)

- Builds QuickJS from bundled C with `cc` (no cmake, no NASM); **pre-generated bindings** ship for
  `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`,
  `aarch64-apple-darwin`, `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc`, so no
  `bindgen` / libclang is needed on any CI OS. MSRV 1.87.
- Only the default `std` feature: no `loader` (no module import), no `futures`, no libc
  `std` / `os` modules (QuickJS's host APIs are not compiled in) → a script has no file, network
  or process access. `Runtime::set_memory_limit`, `set_max_stack_size` and
  `set_interrupt_handler` give the sandbox limits (§4.2).
- `Runtime` / `Context` are not `Send` without the `parallel` feature: each shell session owns a
  dedicated thread (shell docs).
- The full 3-OS build is proven by the full CI lane (Linux, macOS, Windows `cargo test`) once
  the crate is compiled there; the fast lane builds it on Linux.

### 2.4 Dependency list (root `[workspace.dependencies]`)

| Crate | Version | Features | License |
|---|---|---|---|
| `mongodb` | 3.9.1 | `compat-3-3-0`, `bson-3`, `rustls-tls`, `dns-resolver`, `socks5-proxy` (no defaults) | Apache-2.0 |
| `bson` | 3.1.0 | `serde`, `serde_json-1`, `chrono-0_4`, `uuid-1` | MIT |
| `redis` | 1.8.1 | see §2.2 (no defaults) | BSD-3-Clause |
| `rquickjs` | 0.14.0 | default (`std`) | MIT |
| `serde_json` | 1.0.154 | **`preserve_order`** (new) | MIT OR Apache-2.0 |

`preserve_order` makes JSON objects keep insertion order everywhere (IndexMap, already in the
graph). MongoDB needs it: `{"b": 1, "a": 1}` is a different sort than `{"a": 1, "b": 1}`, and the
optimistic document check compares field order (§5.1). Code that relied on sorted keys: none
found (struct serialization order is unchanged; equality of JSON values ignores order).

`oxbase-dialect` gains `oxbase-sql` (statements of editor languages) and `tokio` (the pub/sub
sink). `oxbase-services` gains `oxbase-mongo` and `oxbase-redis` (registered at integration).

## 3. Model mapping

### 3.1 MongoDB (`oxbase_mongo::dialect`, `oxbase_dialect::document`)

```text
connection
└─ databases                    (DatabaseScope::Catalog: one client sees every database)
   ├─ Collections               ObjectKind::Table, labelled "Collection" / "Collections"
   │  └─ collection → Indexes   ObjectKind::Index children (`_id_` = PRIMARY_KEY)
   ├─ Views                     ObjectKind::View (definition = pipeline JSON, extras VIEW_ON)
   ├─ Users                     ObjectKind::User (usersInfo; never credentials)
   └─ Roles                     ObjectKind::Role
```

- Refs: database `ObjectRef::database("oxbase_test")`; collection `kind: table, catalog:
  "oxbase_test", name: "m7_docs"`; index `kind: index, catalog, parent: "m7_docs"`.
- No new `ObjectKind`: a collection is a `Table`; the tree shows the engine's label
  (`TreeLayout::with_label`). Applicable kinds: Database, Table, View, Index, User, Role.
- `load` of a collection returns `ObjectDef::Table` with columns inferred from a `$sample`
  (BSON type → `DataType`, `NATIVE_TYPE` = the BSON type or `mixed`), primary key `_id`,
  indexes, and extras (`CAPPED`, `VALIDATOR`, `OPTIONS`, `DOCUMENT_COUNT`, `SIZE_BYTES`; full
  list in `oxbase_mongo::dialect` docs and `oxbase_mongo::extras`).

**Documents as Arrow pages** (contract in `oxbase_dialect::document`):

| BSON seen in a field | Arrow column | `oxbase.bson_type` |
|---|---|---|
| objectId | `Utf8` (24 lowercase hex) | `objectId` |
| string | `Utf8` | `string` |
| int | `Int32` | `int` |
| long, or int + long | `Int64` | `long` |
| double | `Float64` | `double` |
| decimal | `Utf8` (exact text) | `decimal` |
| bool | `Boolean` | `bool` |
| date | `Timestamp(Millisecond, "UTC")` | `date` |
| binData | `Binary` (`oxbase.bson_subtype` when uniform) | `binData` |
| object, array, every other type, any mix | `Utf8` relaxed Extended JSON, `arrow.json` | the type, or `mixed` + `oxbase.bson_types` |

Columns come from the first `sample_size` documents (default 1000; the projection fixes the
order of included fields), `_id` first, then by first appearance. A later value that does not
fit its column is NULL there and counted in one warning notice per result; a field first seen
after the sample is only in the document column. The last column, `$document` (`oxbase.role =
document`, `arrow.json`), holds each row's whole document as canonical Extended JSON text: the
grid hides it, the tree / JSON views and the editors read it (`document_at`).

### 3.2 Redis (`oxbase_redis::dialect`, `oxbase_dialect::keyvalue`)

```text
connection
├─ databases 0 .. N-1           ObjectKind::Database (CONFIG GET databases; a cluster: 0 only)
│  ├─ namespace "m7:"           ObjectKind::Schema, labelled "Namespace" (direct children)
│  │  ├─ namespace "m7:ns:"
│  │  │  └─ key "m7:ns:leaf"    ObjectKind::Key (KEY_TYPE, TTL_MS)
│  │  └─ key "m7:list"
│  └─ key "plain"
└─ Users                        ObjectKind::User (ACL LIST, Redis 6+)
```

- `DatabaseScope::Session`: database `n` is read on a metadata child session opened with
  `ConnectParams::database = n` (the services' existing PostgreSQL path; no cell sends
  `SELECT`).
- Namespaces are `Schema` refs whose `name` is the full prefix with its trailing separator
  (`m7:ns:`); keys are `Key` refs whose `name` is the whole key (non-UTF-8 bytes shown `\xHH`,
  `props::KEY_HEX` = the hex). `schema` = the enclosing namespace. The separator is the profile
  option `namespace_separator` (default `:`; empty = no namespaces). A node's listing scans
  `MATCH <prefix>*` up to `tree_scan_limit` keys (default 100 000) and shows at most 1000
  namespaces and 1000 keys (`TRUNCATED`); the key browser shows everything.
- Cluster nodes are not tree nodes: a cluster has one keyspace (database 0); scans fan out over
  the masters. The database row carries `extras::MODE`.

**Values as Arrow pages** (the typed editors; `oxbase_dialect::keyvalue`): string `value`;
list `index`, `value`; hash `field`, `value`; set `member`; zset `member`, `score`
(`Float64`, score order); stream `id`, `fields` (JSON object); json `value` (JSON). Bytes are
`Binary` with `oxbase.encoding = binary`. The key browser's scan: `key` (`Binary`), `key_text`,
`type`, `ttl_ms`. Values are read in chunks of 1000 (`LRANGE` windows, `HSCAN` / `SSCAN` /
`ZSCAN` cursors, `XRANGE` by id, `GETRANGE` for large strings), so a 200 000-member zset streams in
bounded memory through the M2 result store.

Keys and values cross the IPC as `BinaryText` (`{"encoding": "utf8", "text"}` or
`{"encoding": "hex", "hex"}`).

## 4. Query languages

### 4.1 One run path for every engine (`oxbase_services::script`, done)

- `StatementSource::for_engine(dialects, engine)`: the SQL dialect for SQL engines, else the
  engine's registered `ScriptLanguage`. `QueryService` splits, guards and redacts through it;
  `HistoryRecord::source` (was `dialect`) redacts history; `QueryService::statement_at_with`
  serves `statement_at` for every engine. Nothing else changed in the M2 run path: each
  statement runs as `Connection::execute(text, &[], token)` on the editor's session, with the
  same events, stores, paging, cancel and history.
- `ScriptLanguage::guard` → `StatementGuard::{Allowed, Confirm(why), Blocked(why)}`, checked for
  every statement before the run starts: `Blocked` fails with `InvalidInput`; `Confirm` fails
  with `InvalidInput` whose message starts with `CONFIRMATION_REQUIRED` (`"confirmation
  required: "`) unless `RunOptions::confirm_destructive`. The UI asks and runs again
  (`isConfirmationRequired` in TS).
- `QueryService::run_source(target, description, Arc<dyn ResultSource>, options)` runs
  engine-built streams (find / aggregate requests, key scans, value reads) as a one-statement
  run whose text is `description`; same editor rules and events.
- Statements of non-SQL languages use `oxbase_sql::Statement` with the SQL splitter's range
  rules (`oxbase_dialect::script::{statement, trim_range}` build them).

### 4.2 MongoDB

- **Requests** (the builder): `DocumentQuery::{Find(FindRequest), Aggregate(AggregateRequest)}`
  (`oxbase_types::document`; filter, projection, sort, skip, limit, hint, collation,
  `maxTimeMs`; pipeline, `allowDiskUse`), Extended JSON values with key order kept; the one
  exception: `{$regex, $options}` objects stay the query operator. `document_find` /
  `document_aggregate` run them through `DocumentStore::query` → `run_source`; the history entry
  is the request as shell code (`request_text`). Text typed in the editor whose first non-blank
  character is `{` is also a request (the builder's JSON form).
- **Shell subset** (`oxbase_mongo::shell` docs: the exact list of globals, helpers, results,
  sandbox and splitting rules): `db.<coll>.find / findOne / aggregate / insertOne / insertMany /
  updateOne / updateMany / replaceOne / deleteOne / deleteMany / countDocuments /
  estimatedDocumentCount / distinct / createIndex / getIndexes / dropIndex / drop`,
  `db.getCollectionNames`, `db.getSiblingDB`, `db.runCommand`, `use <db>`, `show dbs /
  collections / users / roles`, helpers `ObjectId`, `ISODate`, `NumberInt`, `NumberLong`,
  `NumberDecimal`, `UUID`, `BinData`, `Timestamp`, `MinKey` / `MaxKey`, `print`, `printjson`,
  `EJSON`. Cursors stream as document result sets; writes report `RowsAffected` and a one-row
  result. Sandbox: bare QuickJS (no `std` / `os`, no loader), 64 MiB memory, 1 MiB stack, 10 s
  CPU per statement (profile options), interrupt on cancel → `Cancelled`, on the CPU limit →
  `Timeout`; globals persist per editor session; one thread per session.
- Splitter: top-level `;` or a newline where the statement is complete (brackets, strings,
  templates, comments, continuation lines starting with `.`, operators); `use` / `show` lines
  alone.
- History redaction: `pwd` / `password` values (`createUser`, `updateUser`, `db.auth`,
  `changeUserPassword`) and `mongodb://user:pass@` passwords → `***`.
- Guards (confirm): `dropDatabase()`, `.drop()`, `dropIndex`, `deleteMany` / `updateMany` with an
  empty filter, `renameCollection(..., true)`.

### 4.3 Redis console

- One command per line; blank lines and `#` / `//` comment lines are not statements. Arguments
  with `redis-cli` quoting (`oxbase_redis::command::parse_command_line`, done: `"..."` with
  `\n \r \t \b \a \\ \" \xHH`, `'...'` with `\'`, binary-safe). `params` (bound values) append
  arguments for internal callers.
- Replies render per RESP type and, for common read commands, with typed columns (table in the
  `oxbase_redis::driver` docs); integer replies of write commands are `RowsAffected`; error
  replies are `Error::Sql { code: <first word> }`.
- Guards: confirm `FLUSHALL`, `FLUSHDB`, `SWAPDB`, `SHUTDOWN`, `DEBUG`, `SCRIPT FLUSH`,
  `FUNCTION FLUSH` / `DELETE`, `CLUSTER RESET` / `FAILOVER` / `FORGET`, `REPLICAOF` /
  `SLAVEOF`, `CONFIG SET` / `REWRITE`, `ACL DELUSER` / `SETUSER`, `KEYS`; block `SUBSCRIBE`,
  `PSUBSCRIBE`, `SSUBSCRIBE` (use the pub/sub panel), `MONITOR`, `SYNC`, `PSYNC`, `QUIT`,
  `RESET`, `HELLO` switching protocol, `CLIENT REPLY`.
- Redaction: `AUTH` passwords, `HELLO ... AUTH`, `MIGRATE ... AUTH / AUTH2`, `ACL SETUSER`
  password rules, `CONFIG SET requirepass | masterauth | masteruser | tls-*-pass` values.
- `MULTI` / `EXEC` typed line by line work (the editor's own session); `begin()` is
  `Unsupported`.

## 5. Editing

### 5.1 Documents (`DocumentEdit`, `DocumentStore::apply`, `document_apply`)

- Edits: `replace {id, previous, document}`, `update {id, changes: [{path, previous, value}]}`
  (`$set` / `$unset`), `insert {document}`, `delete {id, previous?}`. Documents, ids and field
  values are **Extended JSON text** in edits and results, so the UI sends back exactly the text
  it got (JavaScript objects would reorder integer-like keys).
- Optimistic: replace / delete match `{_id, $expr: {$eq: ["$$ROOT", {$literal: previous}]}}`;
  update matches every changed field's previous value (`$exists: false` for missing). Matched 0
  → `conflict`, nothing written.
- No transaction (M7: standalone servers have none): edits run in order, each on its own; the
  first conflict or failure stops the rest (`notRun`); applied edits stay (like M3's
  non-transactional tables). Applied replace / update / insert return the stored document.
- The M3 pending-buffer idea is kept in the UI (a list of `DocumentEdit`s applied with one
  `document_apply`); the M3 `edit_begin` / `edit_apply` commands stay SQL-only.

### 5.2 Keys (`KeyApplyRequest`, `KeyValueStore::apply`, `key_apply`)

- Edits of **one key**: `setString`, `expire` (`PEXPIRE` / `PERSIST`), `rename` (`RENAMENX`, or
  `RENAME` with `overwrite`), `delete` (`UNLINK`), `hashSet` / `hashDelete`, `listPush` /
  `listSet` (with `expected`, checked with `LINDEX`) / `listRemove`, `setAdd` / `setRemove`,
  `zsetAdd` (adds or changes the score) / `zsetRemove`, `streamAdd` / `streamDelete`,
  `jsonSet` (RedisJSON).
- Atomic: `WATCH key` + type / existence checks + `MULTI` ... `EXEC`; all or nothing; a changed
  key or type → `conflict`. `create` requires the key not to exist and starts with a value edit.
- Destructive edits (`delete`, overwriting `rename`) need `confirmDestructive`; `delete` must be
  alone and `rename` last (validated in `oxbase_types` before anything runs).

### 5.3 Import into collections (`BulkLoader` hooks)

`BulkLoader::transactional()` (default `true`) and `create_target()` (default `None`) are new
default methods. MongoDB's loader returns `false` / creates the collection, and uses
`BulkMethod::DocumentInsert` (`insertMany`, ordered, 1000 documents). The mongo stream teaches
the import service (engine-neutral, `crates/oxbase-services/src/import/**`): no transaction for a
non-transactional loader (report what was written, like M3 kept edits), `create_target` instead
of the `DdlGenerator` for mode *create*, and *truncate* / *delete all* / *replace* refused with
`Unsupported` in the plan for such a loader. Export of any MongoDB or Redis result needs nothing
new (Arrow pages; JSON text columns keep `arrow.json`). Not part of the exit check.

### 5.4 Dangerous commands in the browser

The key browser never sends `KEYS` (cursor `SCAN` only) and offers no `FLUSHDB` / `FLUSHALL`;
the console asks for confirmation (§4.3). The document views offer no drop; the shell asks
(§4.2).

## 6. Publish / subscribe (Redis)

- `PubSubService::subscribe(session, SubscribeRequest {channels, patterns})` → the role opens a
  **dedicated connection** like the session's (same resolver, TLS, credentials; through SSH),
  subscribes, and pushes `PubSubEvent`s into a bounded sink (`SINK_CAPACITY` 1024) with
  `try_send`: a full sink drops messages and the next event that fits is preceded by `dropped
  {count}`. Events: `subscribed`, `message {channel, pattern?, payload, receivedAtMs}`,
  `dropped`, `closed {error?}` (last).
- `pubsub_subscribe` forwards the events to a Tauri `Channel<PubSubEvent>`; when sending fails
  (the panel or webview is gone) it unsubscribes. `pubsub_unsubscribe` on panel close;
  `close_connection` closes every subscription of the session.
- `publish(session, channel, payload)` runs `PUBLISH` on the base session and returns the
  receiver count.
- Cluster: classic `SUBSCRIBE` / `PSUBSCRIBE` on one node (broadcast cluster-wide); sharded pub/sub
  is not offered in M7.

## 7. Connection profiles

Profiles keep the M1 shape; engine settings are `options` (strings), parsed by
`MongoOptions::from_params` / `RedisOptions::from_params` (done, tested); unknown keys are
`InvalidInput`. Secrets (password, SSH, client key) stay in the keychain as before; no option
holds a secret.

### 7.1 MongoDB (`oxbase_mongo::options`)

| Field / option | Meaning |
|---|---|
| `host`, `port` | first host (default port 27017), or the SRV name with `srv` |
| `hosts` | more seeds `h:p,h:p` |
| `srv` | `true`: `mongodb+srv://host` (**in M7**, not with SSH, other hosts or a port) |
| `replica_set` | `replicaSet` |
| `auth_source` | default `admin` (`$external` for X.509 / PLAIN) |
| `auth_mechanism` | `SCRAM-SHA-256`, `SCRAM-SHA-1`, `MONGODB-X509`, `PLAIN` (PLAIN needs `VerifyCa` / `VerifyFull`); default negotiated |
| `direct_connection` | default: one host without a replica set; **always** with SSH (one host only) |
| `read_preference` | `primary` (default) ... `nearest` |
| `sample_size`, `shell_memory_mb`, `shell_time_limit_ms` | column inference sample (1000), shell memory (64 MiB), shell CPU per statement (10 s) |
| `user`, `password`, `database`, `tls`, `ssh`, `connect_timeout`, `application_name` | as M1; `database` is the shell's starting `db` |

### 7.2 Redis (`oxbase_redis::options`)

| Field / option | Meaning |
|---|---|
| `host`, `port` | first node (default 6379) |
| `mode` | `standalone` (default), `cluster`, `sentinel` |
| `nodes` | more seeds: cluster nodes, or sentinels |
| `master_name` | sentinel master (required in sentinel mode) |
| `sentinel_auth` | `none` (default) or `same` (the profile's credentials to the sentinels) |
| `protocol` | `auto` (default), `resp2`, `resp3` |
| `namespace_separator` | default `:`; empty: no namespaces |
| `tree_scan_limit` | keys scanned per tree node (100 000) |
| `read_from_replicas` | cluster console reads on replicas (`false`) |
| `user` (ACL; empty = `default`), `password`, `database` (index, `0`; a cluster only 0), `tls`, `ssh` (standalone only), `connect_timeout` | as M1 |

## 8. Tauri commands and TS (`crates/app-tauri/src/nosql.rs`, `ui/src/api/nosql.ts`)

camelCase payloads; core models inside (`ObjectRef`) keep snake_case. Errors are `CommandError
{category, message}`; `requestId` values are cancelled with `cancel_request`. Every command is
in `generate_handler!`, `build.rs` and `capabilities/default.json`.

| Command | Arguments | Returns |
|---|---|---|
| `document_find` | `sessionId`, `editorId`, `request: FindRequest`, `options?`, `onEvent: Channel<RunEvent>`, `requestId?` | `RunId` (events and paging as `query_run`) |
| `document_aggregate` | same with `request: AggregateRequest` | `RunId` |
| `document_at` | `runId`, `resultIndex`, `row` | Extended JSON text of that row's document |
| `document_get` | `sessionId`, `collection: ObjectRef`, `id` (Extended JSON text), `requestId?` | text or `null` |
| `document_apply` | `sessionId`, `collection`, `edits: DocumentEdit[]`, `requestId?` | `DocumentApplyReport` (TS also `documentReplace` / `documentInsert` / `documentDelete`) |
| `key_scan_open` | `sessionId`, `editorId`, `request: KeyScanRequest`, `options?`, `onEvent`, `requestId?` | `RunId` |
| `key_info` | `sessionId`, `key: KeyAddress`, `requestId?` | `KeyInfo` |
| `key_value_open` | `sessionId`, `editorId`, `key`, `options?`, `onEvent`, `requestId?` | `KeyValueOpened {runId, info}` |
| `key_apply` | `sessionId`, `request: KeyApplyRequest`, `requestId?` | `KeyApplyReport` |
| `pubsub_subscribe` | `sessionId`, `request: SubscribeRequest`, `onEvent: Channel<PubSubEvent>`, `requestId?` | subscription id |
| `pubsub_unsubscribe` | `subscriptionId` | `boolean` |
| `pubsub_publish` | `sessionId`, `channel: BinaryText`, `payload: BinaryText`, `requestId?` | receivers |
| `query_run`, `statement_at` (changed: now `State`-backed, every engine) | as M2 | shell / console scripts |

The task's names map as: mongo_find / mongo_aggregate → `document_find` / `document_aggregate`;
redis key scan / get / set / edit → `key_scan_open` / `key_info` + `key_value_open` /
`key_apply`; shell / console runs → `query_run`.

**UI (app stream).** Connection dialog fields of §7 (MongoDB: hosts, SRV, replica set, auth
source and mechanism, read preference; Redis: mode, nodes, master name, sentinel auth, protocol,
separator); document grid (hides `$document`; mixed / JSON cells shown as JSON; Decimal128 as
exact text) with tree and JSON views from `document_at`; find / aggregate builder (filter,
projection, sort with key order, skip, limit; pipeline stages) writing `FindRequest` /
`AggregateRequest`; shell editor (Monaco `javascript`) and console editor (`statement_at`,
`queryRun`); key browser (`key_scan_open` with pattern / type filters, `BinaryText` display),
typed editors per `KeyType` writing `KeyEdit`s; pub/sub panel; confirmation dialogs for
`isConfirmationRequired` errors and destructive key edits. Design system rules apply (tokens,
lucide icons with aria-labels, both themes); existing test ids stay.

## 9. Fixtures (`tests/fixtures.toml` `[m7]`, conformance stream)

### 9.1 TLS and SSH

- MongoDB: `--tlsMode allowTLS` on 27017 with a combined certificate + key PEM
  (`server-combined.pem`, generated next to the [tls] files), `--tlsCAFile` the fixture CA,
  `--tlsAllowConnectionsWithoutCertificates`. No TLS-only user (MongoDB cannot require TLS per
  user): the TLS checks are `VerifyFull` success + `tls_in_use`, and a wrong server name fails.
- Redis: TLS-only port 6380 (`--tls-port`), published on 127.0.0.1:15480, client certificates
  optional; plain 6379 unchanged.
- SANs `mongodb` and `redis` join `[tls] server_sans` and `server_dns_sans` in
  `scripts/fixtures-certs.sh` in one change (`[m7.tls] server_sans_add`; the fixtures self-test
  requires both lists to agree, so they are kept apart until then).
- `[ssh.targets]`: `mongodb:27017`, `redis:6379`, `redis_tls` = `redis:6380`.

### 9.2 Seeds

`[m7.mongodb]` (database `oxbase_test`): `m7_docs` (12 documents covering every mapped BSON
type, an int / long column, a mixed column, a late field), `m7_big` (200 000), `m7_edit` (3 seed
documents; tests write their own), view `m7_view`, five indexes (name, multikey, TTL, unique
partial, `m7_big_g_idx`), user `m7_reader`, role `m7_role`. `[m7.redis]`: one key of every type
(`m7:string`, `m7:binary` with bytes 00 ff fe 0a, `m7:hash`, `m7:list`, `m7:set`, `m7:zset`,
`m7:stream`, `m7:json`), a TTL key, `m7:big:list` and `m7:big:zset` (200 000 each), the
namespace tree `m7:ns:*`, `m7:db1:key` in database 1, ACL user `m7_reader`, pub/sub channel
`m7:news`. Exact shapes are in the file.

### 9.3 Cluster and sentinel (feasible in CI: both specified)

- `[m7.redis_cluster]`: three masters (no replicas) on 127.0.0.1:15501-15503 (bus +10000),
  `network_mode: host` so nodes announce addresses the runner can dial, created once with
  `redis-cli --cluster create ... --cluster-yes`, seeded with 100 keys.
- `[m7.redis_sentinel]`: master 15511, replica 15512, one sentinel 15526 (quorum 1, master
  `oxmaster`, `sentinel auth-pass`), `network_mode: host`.
- Ports checked free with `ss -ltn` on 2026-11-02 (155xx and 255xx). `network_mode: host` works on
  the Linux runners the conformance job uses; if a runner refuses host networking, the
  conformance stream may switch to bridge networking with `cluster-announce-ip 127.0.0.1` and
  published ports equal to the container ports, and must record that in the contract's
  as-built notes.

## 10. Invariants every stream keeps

1. **One TLS stack**: rustls + ring; no stream enables a `mongodb` / `redis` feature that pulls
   aws-lc, OpenSSL or native-tls (`cargo deny` bans fail the build otherwise).
2. **Addressing**: every socket at `dial_endpoint()`; TLS verifies `tls_server_name()`; cluster,
   sentinel, SRV and replica-set discovery are refused with SSH (one tunnel, one server).
3. **No names in code**: MongoDB commands are BSON with names as values; Redis keys are
   arguments, never formatted into a command line by the core.
4. **Bounded memory**: cursors and `SCAN` loops; no `KEYS`; no whole big value in one reply.
5. **Cancel within 2 s** and the session stays usable (M1 §7, M2 §6 rule 6).
6. **Optimistic edits**: a document or key changed since it was read is a conflict, never
   overwritten.
7. **Secrets** never in Debug / Display, logs, errors, panics, history or Tauri responses;
   history redaction for `AUTH` / `ACL SETUSER` / `CONFIG SET requirepass` and mongo
   `pwd` / `password`; the dialer's per-client credentials are random and never logged.
8. **Sandbox**: the shell has no host access beyond the `db` API; memory, stack and CPU limits.
9. **No engine special cases** outside the engine crates: services and the app reach MongoDB and
   Redis only through `DriverRegistry`, `DialectRegistry` roles and the commands of §8.

## 11. Exit-check tests the streams must write

Live tests are `#[ignore]`d and run in the conformance CI job against the Docker fixtures. A
missing fixture is "pending" unless the stream's `REQUIRE` variable is set (CI sets them at
integration).

- **mongo-driver** (`oxbase-mongo/tests/driver_live.rs`): connect plain; `VerifyFull` with the
  fixture CA as `localhost` (`tls_in_use`), wrong server name fails, `VerifyCa` with a wrong name
  passes; SSH (password and key) to `mongodb:27017` with `VerifyFull` for `mongodb`; SCRAM-SHA-256
  and SCRAM-SHA-1 as `oxbase` (auth source admin), wrong password is `Auth` and leaks no marker;
  a `$where` sleep or `$function` loop on `m7_big` cancelled within 2 s (direct and through SSH)
  and the next command works; a 200 000-document find streams all rows; JSON request statements
  through `execute`.
- **redis-driver** (`oxbase-redis/tests/driver_live.rs`): plain, TLS on 15480 (`VerifyFull`,
  wrong name fails, plain to the TLS port fails), SSH to `redis:6379` and `redis_tls`, ACL
  user `m7_reader`, RESP2 and RESP3, `SELECT` via `database`, `BLPOP 0` cancelled within 2 s
  (`CLIENT UNBLOCK`), the session works afterwards; reply rendering of every RESP type and the
  typed shapes; cluster connect and `GET` across slots; sentinel connect to `oxmaster`.
- **mongo** (`oxbase-mongo/tests/{fetch_live, documents_live}.rs`, `oxbase-services/tests/m7_mongo_live.rs`):
  *browse*: the tree lists `oxbase_test` → Collections (`m7_docs`, `m7_big`, `m7_edit`), Views
  (`m7_view`), Users, Roles, and `m7_docs`'s five indexes; `load` of `m7_docs` has `_id` as key
  and the expected column types; *query*: `document_find` of `m7_docs` gives the column table of
  §3.1 (one `Int64` column for `n`, one `mixed` column, `$document` last, `only_late` absent from
  the columns with a sample of 10, `oxbase.bson_type` metadata), `document_at` returns canonical
  Extended JSON; an aggregate with `$group`; `m7_big` 200 000 rows paged at random offsets, cancel
  mid-stream within 2 s; *edit*: insert, update (with a missing-field change), replace, delete on
  `m7_edit` documents of the test; a stale `previous` gives `conflict` and changes nothing.
- **redis** (`oxbase-redis/tests/*_live.rs`, `oxbase-services/tests/m7_redis_live.rs`):
  *browse*: databases 0..15 (1 has `m7:db1:key`), the namespace tree under `m7:` with key types
  and the TTL of `m7:ttl`; *query*: `key_scan_open` with `m7:*` returns every `[m7.redis] keys`
  entry once; `key_value_open` of each type returns its typed columns; `m7:big:list` and
  `m7:big:zset` stream 200 000 rows; *edit*: every `KeyEdit` on `m7:edit:t<token>:*` keys,
  `create`, a type conflict, `delete` without confirmation refused; pub/sub: subscribe to
  `m7:news` and `m7:*`, publish, receive both, unsubscribe, `dropped` counting with a full sink;
  cluster: scan returns the 100 seed keys across the masters; sentinel: read `m7:sentinel`.
- **shell** (unit tests always; live at integration): splitter cases (multi-line chains, `;`,
  `use` / `show`, strings, templates, comments, a JSON request), redaction (`createUser` `pwd`,
  `AUTH`, `ACL SETUSER`, `CONFIG SET requirepass`) with "nothing to redact" borrowed, guards
  (each listed command); the sandbox: a `while(true){}` is interrupted on cancel and by the CPU
  limit, an allocation loop hits the memory limit, `require` / `load` / `process` are undefined.
  Live: `db.m7_docs.countDocuments({})` = 12; `db.m7_big.find({g: 7}).sort({_id: 1}).limit(5)`
  → `_id` 7, 107, 207, 307, 407; `db.m7_big.aggregate([{$group: {_id: null, total: {$sum:
  "$g"}}}])` → 9 900 000; `use` changes `current_database`; console `HGETALL m7:hash`, `ZRANGE
  m7:zset 0 -1 WITHSCORES`, `GET m7:binary` (bytes kept), `FLUSHDB` refused without
  confirmation; history entries redacted.
- **app**: Vitest for the dialog fields, document grid (hidden `$document`, JSON cells), tree /
  JSON views, builder output, key browser and typed editors, pub/sub panel, confirmation flow.
  Webview smoke (Linux, `OXBASE_E2E_REQUIRE_M7=1`): connect to MongoDB, open `m7_docs` (12 rows),
  run `db.m7_docs.countDocuments({})` in the shell; connect to Redis, browse `m7:ns:`, open
  `m7:hash`, run `HGETALL m7:hash` in the console; screenshots in both themes.
- **conformance**: connection checks for mongodb and redis (`connect`, `tls`, `ssh`,
  `tls_over_ssh`, `cancel` within 2 s, `stream_200k` = `m7_big` / `m7:big:list`), `fetch_list`
  cells for MongoDB (database, table, view, index, user, role) and Redis (database, schema, key,
  user); with `OXBASE_CONFORMANCE_REQUIRE_M7=1` all seven engines must pass every applicable
  cell.

## 12. Deviations from PLAN.md and open points

- PLAN §3 "Arrow `RecordBatch` is the shared row format": documents and keys follow it (the
  `$document` column carries the whole document as text so nothing is lost).
- PLAN §5 "Edits in the grid need a primary or unique key": documents always have `_id`; keys are
  their own key. The M3 `edit_begin` / `edit_apply` stay SQL-only; M7 adds `document_apply` and
  `key_apply`.
- No multi-document transactions, no change streams, no sharded pub/sub, no cluster / sentinel /
  SRV through SSH in M7.
- The SOCKS5 dialer means the MongoDB wire protocol crosses loopback in clear between the driver
  and the dialer inside one process (TLS covers dialer → server). Accepted: the listener is
  per-client, loopback-only and password-protected.
- `serde_json` `preserve_order` is now workspace-wide.
