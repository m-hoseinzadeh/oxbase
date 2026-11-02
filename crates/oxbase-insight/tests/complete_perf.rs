//! M6 exit check (PLAN §6, `docs/m6-contracts.md` §5): completion under 50 ms at p95 on a
//! 5 000-table x 20-column schema.
//!
//! Builds the exit-check cache (`SyntheticSpec::exit_check()`), expands every
//! `[[m6.perf.scenarios]]` entry of `tests/fixtures.toml` over 16 pairs of related tables (240
//! cursor positions), runs each scenario `warmup` + `iterations` times through
//! `context_at` + `complete` and prints p50 / p95 / max per scenario and overall. Every
//! position must return at least one item of the scenario's `expect` kind.
//!
//! Run it in a release-like build (the `insight-perf` CI job):
//!
//! ```text
//! OXBASE_REQUIRE_M6_PERF=1 cargo test -p oxbase-insight --profile perf --test complete_perf -- --ignored --nocapture
//! ```
//!
//! With `OXBASE_REQUIRE_M6_PERF=1` a budget overrun (cache build over 2 s, a scenario's p95 over
//! `complete_p95_ms`, a cancellation slower than `cancel_limit_ms`) fails the test; without it
//! the numbers are printed only.
//!
//! Every timing covers the real pipeline from text + offset to the ranked list:
//! `context_at` (the tolerant parser) followed by `complete` (no hand-built contexts).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use oxbase_insight::{
    CancellationToken, CompletionKind, CompletionList, Dialect, SchemaCache, complete, context_at,
};
use oxbase_schema_cache::synthetic::{SyntheticSpec, snapshot};
use oxbase_types::{Engine, ErrorCategory};

/// Table pairs each scenario is expanded over (15 scenarios x 16 = 240 cursor positions).
const PAIRS: usize = 16;
/// The cache build limit (`docs/m6-contracts.md` §5).
const BUILD_LIMIT: Duration = Duration::from_secs(2);

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn fixtures() -> toml::Table {
    let path = root().join("tests/fixtures.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.parse().expect("fixtures.toml parses")
}

fn table<'a>(t: &'a toml::Table, key: &str) -> &'a toml::Table {
    t.get(key)
        .and_then(toml::Value::as_table)
        .unwrap_or_else(|| panic!("fixtures.toml: missing table {key}"))
}

fn integer(t: &toml::Table, key: &str) -> u64 {
    t.get(key)
        .and_then(toml::Value::as_integer)
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or_else(|| panic!("fixtures.toml: missing integer {key}"))
}

fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}

/// One `[[m6.perf.scenarios]]` entry.
struct Scenario {
    name: String,
    text: String,
    expect: CompletionKind,
    repeat: usize,
}

fn scenarios(perf: &toml::Table) -> Vec<Scenario> {
    let list = perf
        .get("scenarios")
        .and_then(toml::Value::as_array)
        .expect("fixtures.toml: [[m6.perf.scenarios]]");
    list.iter()
        .map(|v| {
            let t = v.as_table().expect("a scenario is a table");
            let s = |k: &str| {
                t.get(k)
                    .and_then(toml::Value::as_str)
                    .unwrap_or_else(|| panic!("scenario: missing {k}"))
                    .to_owned()
            };
            let expect: CompletionKind =
                serde_json::from_value(serde_json::Value::String(s("expect")))
                    .expect("expect is a completion kind");
            Scenario {
                name: s("name"),
                text: s("text"),
                expect,
                repeat: t
                    .get("repeat")
                    .and_then(toml::Value::as_integer)
                    .and_then(|r| usize::try_from(r).ok())
                    .unwrap_or(0),
            }
        })
        .collect()
}

/// Pairs (`{table}`, `{table2}`) of relations of the default schema where `{table2}` has a
/// foreign key to `{table}`, spread over the schema. `{table}` has a `created_*` column (the
/// scenarios type `cr` in its select list and select `created_at` from it).
fn related_pairs(cache: &SchemaCache) -> Vec<(String, String)> {
    let snap = cache.snapshot();
    let default = snap.default_schema.clone().unwrap_or_default();
    let schema = snap
        .schemas
        .iter()
        .find(|s| s.name == default)
        .expect("default schema");
    let fits = |name: &str| {
        schema
            .relations
            .iter()
            .any(|r| r.name == name && r.columns.iter().any(|c| c.name.starts_with("created")))
    };
    let mut pairs: Vec<(String, String)> = Vec::new();
    let step = (schema.relations.len() / (PAIRS * 2)).max(1);
    let candidates = schema
        .relations
        .iter()
        .step_by(step)
        .chain(schema.relations.iter());
    for rel in candidates {
        if pairs.len() == PAIRS {
            break;
        }
        let target = rel
            .foreign_keys
            .iter()
            .find(|fk| fk.ref_schema.is_none() && fits(&fk.ref_table));
        if let Some(fk) = target {
            let pair = (fk.ref_table.clone(), rel.name.clone());
            if !pairs.contains(&pair) {
                pairs.push(pair);
            }
        }
    }
    assert_eq!(pairs.len(), PAIRS, "not enough related tables");
    pairs
}

/// One cursor position of a scenario.
struct Position {
    text: String,
    offset: usize,
}

fn expand(s: &Scenario, (table, table2): &(String, String), schema: &str) -> Position {
    let fill = |t: &str| {
        t.replace("{table2}", table2)
            .replace("{table}", table)
            .replace("{schema}", schema)
    };
    let filler = fill("SELECT id, created_at\nFROM {table}\nWHERE id > 10;\n");
    let mut text = filler.repeat(s.repeat);
    let body = fill(&s.text);
    let at = body.find("<|>").expect("scenario text has a <|> cursor");
    let offset = text.len() + at;
    text.push_str(&body.replace("<|>", ""));
    Position { text, offset }
}

fn kind_matches(item: CompletionKind, expect: CompletionKind) -> bool {
    item == expect || (expect == CompletionKind::Table && item == CompletionKind::View)
}

fn percentile(sorted: &[Duration], pct: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (sorted.len() * pct).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[test]
#[ignore = "release-build latency check; the insight-perf CI job runs it"]
fn completion_latency_on_the_exit_check_cache() {
    let fixtures = fixtures();
    let perf = table(table(&fixtures, "m6"), "perf");
    let budget = Duration::from_millis(integer(perf, "complete_p95_ms"));
    let cancel_limit = Duration::from_millis(integer(perf, "cancel_limit_ms"));
    let warmup = usize::try_from(integer(perf, "warmup")).unwrap_or(20);
    let iterations = usize::try_from(integer(perf, "iterations")).unwrap_or(200);
    let limit = usize::try_from(integer(perf, "limit")).unwrap_or(200);
    let require = flag("OXBASE_REQUIRE_M6_PERF");
    // Over a latency budget (enforced with OXBASE_REQUIRE_M6_PERF=1) / wrong results (always).
    let mut over_budget: Vec<String> = Vec::new();
    let mut wrong: Vec<String> = Vec::new();

    let started = Instant::now();
    let cache = SchemaCache::new(snapshot(&SyntheticSpec::exit_check()));
    let build = started.elapsed();
    let snap = cache.snapshot();
    println!(
        "exit-check cache: {} relations, {} columns, {} routines; built in {:.1} ms; ~{} MiB",
        snap.relation_count(),
        snap.column_count(),
        snap.schemas.iter().map(|s| s.routines.len()).sum::<usize>(),
        ms(build),
        cache.heap_bytes() / (1024 * 1024)
    );
    if build > BUILD_LIMIT {
        over_budget.push(format!(
            "cache build took {:.1} ms (limit {} ms)",
            ms(build),
            BUILD_LIMIT.as_millis()
        ));
    }

    let mut dialect = Dialect::builtin(Engine::Postgres).expect("PostgreSQL dialect");
    dialect.case_rules = cache.case_rules();
    println!("pipeline: oxbase_insight::context_at + complete (release-like build)");
    let snippets = oxbase_insight::snippets::builtin_snippets();
    let pairs = related_pairs(&cache);
    let other_schema = snap
        .schemas
        .iter()
        .map(|s| s.name.as_str())
        .find(|s| Some(*s) != snap.default_schema.as_deref())
        .expect("a second schema")
        .to_owned();
    let scenarios = scenarios(perf);

    let run = |p: &Position, token: &CancellationToken| -> CompletionList {
        let ctx = context_at(&dialect, &p.text, p.offset);
        complete(&ctx, &dialect, Some(&cache), &snippets, limit, token).expect("completion")
    };

    let token = CancellationToken::new();
    let mut all: Vec<Duration> = Vec::new();
    let mut positions_total = 0;
    println!(
        "{:<22} {:>9} {:>9} {:>9} {:>6}",
        "scenario", "p50 ms", "p95 ms", "max ms", "items"
    );
    for s in &scenarios {
        let positions: Vec<Position> = pairs
            .iter()
            .map(|pair| expand(s, pair, &other_schema))
            .collect();
        positions_total += positions.len();
        // Every position must offer the expected kind.
        let mut items = 0;
        for p in &positions {
            let list = run(p, &token);
            items = items.max(list.items.len());
            if !list.items.iter().any(|i| kind_matches(i.kind, s.expect)) {
                let first: Vec<&str> = list
                    .items
                    .iter()
                    .take(5)
                    .map(|i| i.label.as_str())
                    .collect();
                wrong.push(format!(
                    "{}: no {:?} item at {:?} (first items: {first:?})",
                    s.name,
                    s.expect,
                    &p.text[p.text.len().saturating_sub(120)..]
                ));
            }
        }
        for i in 0..warmup {
            std::hint::black_box(run(&positions[i % positions.len()], &token));
        }
        let mut samples = Vec::with_capacity(iterations);
        for i in 0..iterations {
            let p = &positions[i % positions.len()];
            let t = Instant::now();
            std::hint::black_box(run(p, &token));
            samples.push(t.elapsed());
        }
        samples.sort_unstable();
        let (p50, p95) = (percentile(&samples, 50), percentile(&samples, 95));
        let max = samples.last().copied().unwrap_or_default();
        println!(
            "{:<22} {:>9.3} {:>9.3} {:>9.3} {:>6}",
            s.name,
            ms(p50),
            ms(p95),
            ms(max),
            items
        );
        if p95 > budget {
            over_budget.push(format!(
                "{}: p95 {:.3} ms over the {} ms budget",
                s.name,
                ms(p95),
                budget.as_millis()
            ));
        }
        all.extend(samples);
    }
    all.sort_unstable();
    println!(
        "overall: {} cursor positions, {} runs; p50 {:.3} ms, p95 {:.3} ms, max {:.3} ms (budget p95 {} ms)",
        positions_total,
        all.len(),
        ms(percentile(&all, 50)),
        ms(percentile(&all, 95)),
        ms(all.last().copied().unwrap_or_default()),
        budget.as_millis()
    );
    assert!(positions_total >= 200, "at least 200 cursor positions");

    // Cancellation: a request cancelled while it runs returns `Cancelled` quickly.
    let fuzzy = scenarios
        .iter()
        .find(|s| s.name == "from-fuzzy")
        .or_else(|| scenarios.first())
        .expect("a scenario");
    let position = expand(fuzzy, &pairs[0], &other_schema);
    let cancel_after = cancel_latency(&position, &dialect, &cache);
    println!(
        "cancellation: returned {:.3} ms after cancel (limit {} ms)",
        ms(cancel_after),
        cancel_limit.as_millis()
    );
    if cancel_after > cancel_limit {
        over_budget.push(format!(
            "cancellation took {:.3} ms (limit {} ms)",
            ms(cancel_after),
            cancel_limit.as_millis()
        ));
    }
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let err = complete(
        &context_at(&dialect, &position.text, position.offset),
        &dialect,
        Some(&cache),
        &[],
        limit,
        &cancelled,
    )
    .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Cancelled);

    assert_eq!(wrong, Vec::<String>::new(), "completion failures");
    if over_budget.is_empty() {
        println!("M6 completion latency: within budget");
    } else if require {
        panic!(
            "M6 completion latency over budget:\n{}",
            over_budget.join("\n")
        );
    } else {
        println!(
            "M6 completion latency over budget (not enforced without OXBASE_REQUIRE_M6_PERF=1):\n{}",
            over_budget.join("\n")
        );
    }
}

/// How long a running completion takes to return after its token is cancelled (the worst of
/// 20 tries, each cancelling at a different moment).
fn cancel_latency(position: &Position, dialect: &Dialect, cache: &SchemaCache) -> Duration {
    let mut worst = Duration::ZERO;
    for attempt in 0..20_u64 {
        let token = CancellationToken::new();
        let started = Arc::new(AtomicBool::new(false));
        let worker = {
            let token = token.clone();
            let started = Arc::clone(&started);
            let dialect = dialect.clone();
            let cache = cache.clone();
            let text = position.text.clone();
            let offset = position.offset;
            std::thread::spawn(move || {
                started.store(true, Ordering::SeqCst);
                loop {
                    let ctx = context_at(&dialect, &text, offset);
                    match complete(&ctx, &dialect, Some(&cache), &[], 200, &token) {
                        Err(e) if e.category() == ErrorCategory::Cancelled => {
                            return Instant::now();
                        }
                        Err(e) => panic!("completion failed: {e}"),
                        Ok(_) => {}
                    }
                }
            })
        };
        while !started.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_micros(300 * (attempt + 1)));
        let cancelled_at = Instant::now();
        token.cancel();
        let returned_at = worker.join().expect("worker thread");
        worst = worst.max(returned_at.saturating_duration_since(cancelled_at));
    }
    worst
}
