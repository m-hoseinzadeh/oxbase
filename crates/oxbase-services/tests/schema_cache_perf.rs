//! M6: the 5k-table live variant of the exit check (`docs/m6-contracts.md` §5), PostgreSQL only.
//!
//! Creates `[m6.perf] tables` tables of `columns_per_table` columns (each with a foreign key to
//! the previous one) in schema `[m6.perf] schema`, reused while younger than
//! `[m6] stale_after_secs`; builds the cache through `SchemaCacheService::refresh` and reloads
//! it from an app database file, printing the build time, the reload time, the index memory and
//! the snapshot size, and times name lookups on the live cache. With `OXBASE_REQUIRE_M6_PERF=1`
//! (the `insight-perf` job, `--profile perf`) the build and reload limits are enforced; the
//! memory budget always is.
//!
//! The completion scenarios through `InsightService::complete` belong to the complete stream
//! and are added at integration.
//!
//! `#[ignore]`d: `cargo test -p oxbase-services --test schema_cache_perf -- --ignored --nocapture`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_array::Array as _;
use oxbase_driver::collect;
use oxbase_net::ssh::RusshTunnelProvider;
use oxbase_schema_cache::{MEMORY_BUDGET_BYTES, MatchMode, NameKind, NameQuery, SchemaCache};
use oxbase_services::schema_cache::{
    CacheSource, CacheState, RefreshScope, SchemaCacheConfig, SchemaCacheService, SchemaEvents,
};
use oxbase_services::{
    CancellationToken, ConnectionManager, ProvidedSecrets, default_dialects, default_drivers,
};
use oxbase_store::{
    AppDb, ConnectionProfile, MemorySecretStore, ProfileAddress, SchemaCacheRepository, Secret,
    SecretRef, SecretSlot, SecretStore, SqliteSchemaCacheRepository, TlsMode, TlsSettings,
};
use oxbase_types::{Engine, Value};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn fixtures() -> toml::Value {
    let path = std::env::var_os("OXBASE_FIXTURES")
        .map(PathBuf::from)
        .map_or_else(|| root().join("tests/fixtures.toml"), |p| root().join(p));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    toml::Value::Table(text.parse().expect("fixtures.toml parses"))
}

fn s(t: &toml::Value, key: &str) -> String {
    t[key]
        .as_str()
        .unwrap_or_else(|| panic!("fixtures.toml: missing {key}"))
        .to_owned()
}

fn n(t: &toml::Value, key: &str) -> u64 {
    t[key]
        .as_integer()
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or_else(|| panic!("fixtures.toml: missing number {key}"))
}

fn report(line: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr().lock(), "[m6 perf] {line}");
}

fn require_perf() -> bool {
    std::env::var("OXBASE_REQUIRE_M6_PERF").is_ok_and(|v| v == "1")
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The p50 / p95 / max of `samples` in microseconds.
fn percentiles(samples: &mut [Duration]) -> (u128, u128, u128) {
    samples.sort();
    let at = |q: usize| samples[(samples.len() * q / 100).min(samples.len() - 1)].as_micros();
    (
        at(50),
        at(95),
        samples.last().map_or(0, Duration::as_micros),
    )
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

async fn exec(c: &dyn oxbase_services::Connection, sql: &str, params: &[Value]) -> Vec<Vec<Value>> {
    let collected = collect(
        c.execute(sql, params, CancellationToken::new())
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}")),
    )
    .await
    .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let mut out = Vec::new();
    for set in collected.result_sets {
        for batch in &set.batches {
            for row in 0..batch.num_rows() {
                out.push(
                    batch
                        .columns()
                        .iter()
                        .map(|col| {
                            let a = col.as_ref();
                            if a.is_null(row) {
                                Value::Null
                            } else if let Some(t) =
                                a.as_any().downcast_ref::<arrow_array::StringArray>()
                            {
                                Value::Text(t.value(row).to_owned())
                            } else if let Some(i) =
                                a.as_any().downcast_ref::<arrow_array::Int64Array>()
                            {
                                Value::Int(i.value(row))
                            } else {
                                Value::Null
                            }
                        })
                        .collect(),
                );
            }
        }
    }
    out
}

/// Creates (or reuses) the perf schema. Returns `true` when it was created now.
async fn ensure_schema(
    c: &dyn oxbase_services::Connection,
    schema: &str,
    tables: u64,
    columns: u64,
    stale_after: i64,
) -> bool {
    let spec = format!("m6 perf {tables}x{columns}");
    let rows = exec(
        c,
        "SELECT pg_catalog.obj_description(n.oid, 'pg_namespace') AS comment, \
                (SELECT count(*) FROM pg_catalog.pg_class c \
                 WHERE c.relnamespace = n.oid AND c.relkind = 'r')::int8 AS tables \
         FROM pg_catalog.pg_namespace n WHERE n.nspname = $1",
        &[Value::Text(schema.to_owned())],
    )
    .await;
    if let Some(row) = rows.first()
        && let (Value::Text(comment), Value::Int(count)) = (&row[0], &row[1])
        && let Some((made, rest)) = comment.split_once(' ')
        && rest == spec
        && u64::try_from(*count).is_ok_and(|c| c == tables)
        && made
            .parse::<i64>()
            .is_ok_and(|t| now_secs().saturating_sub(t) < stale_after)
    {
        report(&format!("reusing schema {schema} ({tables} tables)"));
        return false;
    }
    let started = Instant::now();
    // The schema name comes from tests/fixtures.toml; it is quoted with %I below.
    let q = schema.replace('\'', "''");
    exec(
        c,
        &format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE",
            schema.replace('"', "\"\"")
        ),
        &[],
    )
    .await;
    exec(
        c,
        &format!("CREATE SCHEMA \"{}\"", schema.replace('"', "\"\"")),
        &[],
    )
    .await;
    // Batches of 250 tables: one transaction each keeps the lock table small.
    let mut from = 0;
    while from < tables {
        let to = (from + 250).min(tables) - 1;
        let block = format!(
            "DO $m6$ DECLARE i int; j int; cols text; \
             names text[] := ARRAY['customer', 'orders', 'invoice', 'product', 'shipment']; \
             types text[] := ARRAY['integer', 'numeric(12,2)', 'varchar(40)', 'boolean', 'date']; \
             BEGIN FOR i IN {from}..{to} LOOP \
               cols := 'id bigint PRIMARY KEY, parent_id bigint' || CASE WHEN i > 0 THEN \
                 format(' REFERENCES %I.%I (id)', '{q}', \
                        format('t%s_%s', lpad((i - 1)::text, 4, '0'), names[1 + (i - 1) % 5])) \
                 ELSE '' END || ', created_at timestamptz, name text'; \
               FOR j IN 5..{columns} LOOP \
                 cols := cols || format(', c%s %s', lpad(j::text, 2, '0'), types[1 + j % 5]); \
               END LOOP; \
               EXECUTE format('CREATE TABLE %I.%I (%s)', '{q}', \
                              format('t%s_%s', lpad(i::text, 4, '0'), names[1 + i % 5]), cols); \
             END LOOP; END $m6$"
        );
        exec(c, &block, &[]).await;
        from = to + 1;
    }
    exec(
        c,
        &format!(
            "COMMENT ON SCHEMA \"{}\" IS '{} {spec}'",
            schema.replace('"', "\"\""),
            now_secs()
        ),
        &[],
    )
    .await;
    report(&format!(
        "created schema {schema}: {tables} tables x {columns} columns in {} ms",
        ms(started.elapsed())
    ));
    true
}

/// Times `query` on `cache` (`iterations` runs after `warmup`).
fn time_lookup(
    cache: &SchemaCache,
    name: &str,
    query: &NameQuery<'_>,
    warmup: u64,
    iterations: u64,
) {
    for _ in 0..warmup {
        let _ = cache.search(query);
    }
    let mut samples = Vec::new();
    let mut hits = 0;
    for _ in 0..iterations {
        let started = Instant::now();
        hits = cache.search(query).len();
        samples.push(started.elapsed());
    }
    let (p50, p95, max) = percentiles(&mut samples);
    report(&format!(
        "lookup {name}: {hits} hits, p50 {p50} us, p95 {p95} us, max {max} us"
    ));
}

#[tokio::test]
#[ignore = "needs the postgres fixture"]
async fn pg_5k_tables_build_reload_and_memory() {
    let doc = fixtures();
    let m6 = &doc["m6"];
    let perf = &m6["perf"];
    let pg = &doc["engines"]["postgres"];
    let schema = s(perf, "schema");
    let tables = n(perf, "tables");
    let columns = n(perf, "columns_per_table");
    let stale_after = i64::try_from(n(m6, "stale_after_secs")).unwrap_or(i64::MAX);

    let secrets = Arc::new(MemorySecretStore::new());
    let mut profile = ConnectionProfile::new(
        "m6 perf",
        Engine::Postgres,
        ProfileAddress::Network {
            host: s(pg, "host"),
            port: pg["port"].as_integer().and_then(|p| u16::try_from(p).ok()),
        },
    );
    profile.user = Some(s(pg, "user"));
    profile.database = Some(s(&m6["postgres"], "database"));
    let secret = profile.secret_key(SecretSlot::Password);
    secrets
        .set(&secret, &Secret::new(s(pg, "password")))
        .await
        .unwrap();
    profile.password = Some(SecretRef::Keychain { key: secret });
    profile.tls = TlsSettings {
        mode: TlsMode::Disable,
        ..TlsSettings::default()
    };
    let manager = Arc::new(ConnectionManager::new(
        default_drivers(),
        secrets,
        Arc::new(RusshTunnelProvider::new()),
    ));
    let base = manager
        .open(&profile, &ProvidedSecrets::none(), CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"));
    ensure_schema(
        base.connection().as_ref(),
        &schema,
        tables,
        columns,
        stale_after,
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let app_db = AppDb::open(dir.path().join("app.sqlite")).await.unwrap();
    let repo: Arc<dyn SchemaCacheRepository> =
        Arc::new(SqliteSchemaCacheRepository::new(app_db.clone()));
    let dialects = Arc::new(default_dialects());
    let caches = SchemaCacheService::new(
        Arc::clone(&manager),
        Arc::clone(&dialects),
        Arc::clone(&repo),
        SchemaEvents::new(),
        SchemaCacheConfig::default(),
    );

    // Build.
    let started = Instant::now();
    let status = caches
        .refresh(
            base.id(),
            None,
            RefreshScope::Full,
            CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("build: {e}"));
    let build = started.elapsed();
    assert_eq!(status.state, CacheState::Ready);
    assert!(!status.truncated);
    let key = caches.key_for(base.id(), None).unwrap();
    let cache = caches.cache(&key).await.expect("cache");
    let perf_schema = cache
        .snapshot()
        .schemas
        .iter()
        .find(|x| x.name == schema)
        .expect("the perf schema");
    let expected_tables = usize::try_from(tables).unwrap();
    assert_eq!(perf_schema.relations.len(), expected_tables);
    let perf_columns: usize = perf_schema.relations.iter().map(|r| r.columns.len()).sum();
    assert_eq!(
        perf_columns,
        expected_tables * usize::try_from(columns).unwrap()
    );
    assert!(
        perf_schema
            .relations
            .iter()
            .skip(1)
            .all(|r| r.foreign_keys.len() == 1),
        "every table but the first references the previous one"
    );
    let heap = cache.heap_bytes();
    let stored = repo
        .load(key.profile_id, &key.database)
        .await
        .unwrap()
        .expect("persisted");
    report(&format!(
        "build: {} relations, {} columns, {} routines in {} ms (limit {} ms); index {:.1} MiB \
         (budget {} MiB); snapshot {:.1} MiB",
        status.relation_count,
        status.column_count,
        status.routine_count,
        ms(build),
        n(perf, "build_limit_ms"),
        // Display only.
        f64::from(u32::try_from(heap / 1024).unwrap_or(u32::MAX)) / 1024.0,
        MEMORY_BUDGET_BYTES / (1024 * 1024),
        f64::from(u32::try_from(stored.bytes.len() / 1024).unwrap_or(u32::MAX)) / 1024.0,
    ));
    assert!(
        heap < MEMORY_BUDGET_BYTES,
        "index {heap} bytes over the budget"
    );

    // Reload from the app database (a restart): no catalog read.
    drop(caches);
    let reloaded = SchemaCacheService::new(
        Arc::clone(&manager),
        dialects,
        Arc::new(SqliteSchemaCacheRepository::new(app_db)),
        SchemaEvents::new(),
        SchemaCacheConfig::default(),
    );
    let started = Instant::now();
    let again = reloaded.cache(&key).await.expect("reloaded");
    let reload = started.elapsed();
    assert_eq!(
        again.snapshot().relation_count(),
        cache.snapshot().relation_count()
    );
    let status = reloaded.status(base.id(), None).await.unwrap();
    assert_eq!(status.source, Some(CacheSource::Disk));
    report(&format!(
        "reload from the app database: {} ms (limit {} ms)",
        ms(reload),
        n(perf, "reload_limit_ms")
    ));
    let cached = Instant::now();
    let _ = reloaded.cache(&key).await.expect("in memory");
    report(&format!(
        "cache() once in memory: {} us",
        cached.elapsed().as_micros()
    ));

    // Name lookups on the live index (the cache side of a completion request).
    let (warmup, iterations) = (n(perf, "warmup"), n(perf, "iterations"));
    let relations = [NameKind::Relation];
    let columns_kind = [NameKind::Column];
    time_lookup(
        &again,
        "relation prefix 't04'",
        &NameQuery::prefix("t04", &relations),
        warmup,
        iterations,
    );
    time_lookup(
        &again,
        "relation fuzzy 'cstord'",
        &NameQuery {
            pattern: "cstord",
            mode: MatchMode::Fuzzy,
            kinds: &relations,
            schema: None,
            relation: None,
            limit: 200,
        },
        warmup,
        iterations,
    );
    time_lookup(
        &again,
        "column prefix 'c1' (every relation)",
        &NameQuery {
            limit: 200,
            ..NameQuery::prefix("c1", &columns_kind)
        },
        warmup,
        iterations,
    );
    report("completion scenarios through InsightService::complete: added by the complete stream");

    if require_perf() {
        assert!(
            ms(build) <= n(perf, "build_limit_ms"),
            "build took {} ms",
            ms(build)
        );
        assert!(
            ms(reload) <= n(perf, "reload_limit_ms"),
            "reload took {} ms",
            ms(reload)
        );
    }
    manager.close_all().await.unwrap();
}
