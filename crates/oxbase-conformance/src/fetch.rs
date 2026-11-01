//! M2 `fetch_list` cells: for every (engine, object kind) with a registered [`Fetcher`], list
//! the seed objects of `tests/fixtures.toml` under their parent and load each one.
//!
//! The registry is `oxbase_services::default_dialects()`, the one the app uses, so a cell runs
//! as soon as its engine crate registers a fetcher there; until then it stays pending. A cell
//! passes when, for each [`SeedGroup`] of [`seeds::seed_groups`]:
//!
//! - `list(parent)` succeeds and returns only rows of the cell's kind;
//! - every seed name is listed, with the ref `parent.child(kind, name)` (routine signatures
//!   aside) and `system = false`;
//! - `load` of every listed seed row returns a definition of that kind and name whose JSON
//!   (what the webview receives) parses back to the same definition.
//!
//! - the seed rows come in the order of §3.3 of `docs/m2-contracts.md`: columns by ordinal,
//!   other kinds (databases aside) by name.
//!
//! A kind the live server version does not have ([`DialectRegistry::supports_kind`]) is not
//! applicable, unless the fixtures seed it on that engine: then the version check is wrong and
//! the cell fails. A kind with a fetcher but no seed object stays pending.
//!
//! Every step is bounded ([`Timeouts`]): opening (and seeding) the session, each cell (waiting
//! for the session included) and closing. A cell that times out has its statement cancelled; the
//! later cells of that engine fail without running, since the session may still be busy.
//!
//! [`gate`] decides whether a report's `fetch_list` cells are acceptable (the live test).
//!
//! Server engines are seeded by their Docker init scripts (`[m2.<engine>] init_sql`). SQLite
//! has no container: the harness applies its `init_sql` to a fresh scratch file and attaches a
//! second file as `attached_as` holding `attached_table`, as `[m2.sqlite]` describes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxbase_dialect::{DialectRegistry, Fetcher};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_services::{ProvidedSecrets, SessionHandle};
use oxbase_store::ProfileAddress;
use oxbase_types::{Engine, ObjectDef, ObjectKind, ObjectRef, ObjectSummary, Value};

use crate::adapter;
use crate::connect::{Ctx, millis};
use crate::fixtures::{Fixtures, M2EngineSeed, ResolvedEngine, in_workspace};
use crate::guard::{self, M4_ENGINES};
use crate::matrix::{Cell, CellStatus, Check, driver_milestone};
use crate::seeds::{self, SeedGroup};

/// Upper bound for one cell (waiting for the session, then every list and load of one kind).
pub const CELL_TIMEOUT: Duration = Duration::from_secs(60);
/// Upper bound for opening one engine's session (and seeding it, on SQLite).
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(60);
/// Upper bound for closing one engine's session.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(15);

/// The engines whose `fetch_list` cells M2 delivers (`docs/m2-contracts.md` §10).
pub const M2_ENGINES: [Engine; 4] = [
    Engine::Postgres,
    Engine::MySql,
    Engine::MariaDb,
    Engine::Sqlite,
];

/// Time limits of one engine's `fetch_list` run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub open: Duration,
    pub cell: Duration,
    pub close: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            open: OPEN_TIMEOUT,
            cell: CELL_TIMEOUT,
            close: CLOSE_TIMEOUT,
        }
    }
}

/// The result of one `fetch_list` cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOutcome {
    /// The fixture name (`[engines.<name>]`).
    pub engine: String,
    pub kind: ObjectKind,
    pub status: CellStatus,
    pub detail: String,
    pub elapsed_ms: Option<u64>,
}

/// The kinds whose `fetch_list` cell can run on `engine`: the engine has a driver, the kind
/// applies, and a [`Fetcher`] is registered for it.
pub fn runnable_kinds(registry: &DialectRegistry, engine: Engine) -> Vec<ObjectKind> {
    if driver_milestone(engine).is_some() {
        return Vec::new();
    }
    ObjectKind::ALL
        .into_iter()
        .filter(|&k| adapter::applies(registry, engine, k) && registry.fetcher(engine, k).is_ok())
        .collect()
}

/// The seed groups of `kind` for the fixture `e` (empty when the fixtures seed nothing): the
/// `[m2]` groups, then the `[m4]` ones of an engine with an `[m4.<engine>]` table.
pub fn groups_for(fixtures: &Fixtures, e: &ResolvedEngine, kind: ObjectKind) -> Vec<SeedGroup> {
    let mut groups = fixtures
        .m2_seed(&e.name, e.engine)
        .map(|(m2, seed)| seeds::seed_groups(m2, seed, kind))
        .unwrap_or_default();
    if let Some((m4, seed)) = fixtures.m4_seed(&e.name, e.engine) {
        groups.extend(seeds::m4_groups(m4, &seed, kind));
    }
    groups
}

/// The scratch directory a SQLite fixture is seeded in; removed after the run.
pub fn scratch_dir(e: &ResolvedEngine) -> PathBuf {
    std::env::temp_dir().join(format!(
        "oxbase-conformance-m2-{}-{}",
        e.name,
        std::process::id()
    ))
}

/// Runs the `fetch_list` cells of every reachable engine (`reachable[i]` belongs to
/// `engines[i]`). Engines run concurrently. Cells that cannot run produce no outcome, so their
/// planned status stays.
pub fn run_all(
    fixtures: &Fixtures,
    engines: &[ResolvedEngine],
    reachable: &[bool],
    registry: &DialectRegistry,
) -> Vec<FetchOutcome> {
    run_all_with(fixtures, engines, reachable, registry, Timeouts::default())
}

/// [`run_all`] with explicit time limits. Must not be called from inside a tokio runtime (it
/// starts its own).
pub fn run_all_with(
    fixtures: &Fixtures,
    engines: &[ResolvedEngine],
    reachable: &[bool],
    registry: &DialectRegistry,
    limits: Timeouts,
) -> Vec<FetchOutcome> {
    let todo: Vec<&ResolvedEngine> = engines
        .iter()
        .zip(reachable)
        .filter(|&(e, &up)| up && !runnable_kinds(registry, e.engine).is_empty())
        .map(|(e, _)| e)
        .collect();
    if todo.is_empty() {
        return Vec::new();
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            return todo
                .iter()
                .flat_map(|e| {
                    failed_all(
                        e,
                        &runnable_kinds(registry, e.engine),
                        &format!("cannot start the tokio runtime: {err}"),
                    )
                })
                .collect();
        }
    };
    let registry = Arc::new(registry.clone());
    let fixtures = Arc::new(fixtures.clone());
    rt.block_on(async {
        let tasks: Vec<_> = todo
            .iter()
            .map(|&e| {
                let ctx = Ctx::new(&fixtures);
                let (registry, fixtures, e) = (registry.clone(), fixtures.clone(), e.clone());
                tokio::spawn(
                    async move { engine_cells(&ctx, &registry, &fixtures, &e, limits).await },
                )
            })
            .collect();
        let mut out = Vec::new();
        for (task, e) in tasks.into_iter().zip(&todo) {
            match task.await {
                Ok(cells) => out.extend(cells),
                Err(err) => out.extend(failed_all(
                    e,
                    &runnable_kinds(&registry, e.engine),
                    &format!("fetch task panicked: {err}"),
                )),
            }
        }
        out
    })
}

fn outcome(
    e: &ResolvedEngine,
    kind: ObjectKind,
    status: CellStatus,
    detail: String,
) -> FetchOutcome {
    FetchOutcome {
        engine: e.name.clone(),
        kind,
        status,
        detail,
        elapsed_ms: None,
    }
}

fn failed_all(e: &ResolvedEngine, kinds: &[ObjectKind], why: &str) -> Vec<FetchOutcome> {
    kinds
        .iter()
        .map(|&k| outcome(e, k, CellStatus::Fail, why.to_owned()))
        .collect()
}

/// Every runnable cell of one engine, on one session.
async fn engine_cells(
    ctx: &Ctx,
    registry: &DialectRegistry,
    fixtures: &Fixtures,
    e: &ResolvedEngine,
    limits: Timeouts,
) -> Vec<FetchOutcome> {
    let cells: Vec<(ObjectKind, Vec<SeedGroup>)> = runnable_kinds(registry, e.engine)
        .into_iter()
        .map(|k| (k, groups_for(fixtures, e, k)))
        .collect();
    let seed = fixtures.m2_seed(&e.name, e.engine).map(|(_, s)| s);
    let scratch = (e.engine == Engine::Sqlite).then(|| scratch_dir(e));
    let opened = tokio::time::timeout(limits.open, open_session(ctx, e, seed, scratch.as_deref()))
        .await
        .unwrap_or_else(|_| Err(format!("did not open within {}", human(limits.open))));
    let out = match opened {
        Ok(session) => {
            let out = run_cells(registry, e, &session, cells, limits).await;
            let _ = tokio::time::timeout(limits.close, ctx.mgr.close(session.id())).await;
            out
        }
        // Only the cells that would have run fail; the others keep their planned status (the
        // server version, and so n/a, is unknown without a session).
        Err(why) => cells
            .into_iter()
            .filter(|(_, groups)| !groups.is_empty())
            .map(|(k, _)| {
                outcome(
                    e,
                    k,
                    CellStatus::Fail,
                    format!("cannot open a session: {why}"),
                )
            })
            .collect(),
    };
    if let Some(dir) = scratch {
        let _ = std::fs::remove_dir_all(dir);
    }
    out
}

/// Runs the cells on an open session (see the module docs for timeouts).
async fn run_cells(
    registry: &DialectRegistry,
    e: &ResolvedEngine,
    session: &SessionHandle,
    cells: Vec<(ObjectKind, Vec<SeedGroup>)>,
    limits: Timeouts,
) -> Vec<FetchOutcome> {
    let version = session.info().server_version.clone();
    let mut abandoned: Option<ObjectKind> = None;
    let mut out = Vec::with_capacity(cells.len());
    for (kind, groups) in cells {
        let started = Instant::now();
        let (status, detail) = if !registry.supports_kind(e.engine, kind, &version) {
            if groups.is_empty() {
                (
                    CellStatus::NotApplicable,
                    format!("not on server {}", version.raw()),
                )
            } else {
                (
                    CellStatus::Fail,
                    format!(
                        "supports_kind says server {} has no {kind}, but the fixtures seed {} \
                         on this engine",
                        version.raw(),
                        seed_names(&groups)
                    ),
                )
            }
        } else if groups.is_empty() {
            (
                CellStatus::Pending,
                format!(
                    "pending (Fetcher registered; tests/fixtures.toml [m2] seeds no {kind} on \
                     this engine)"
                ),
            )
        } else if let Some(stuck) = abandoned {
            (
                CellStatus::Fail,
                format!("not run: the session was given up after the {stuck} cell timed out"),
            )
        } else {
            match registry.fetcher(e.engine, kind) {
                Err(err) => (CellStatus::Pending, format!("pending ({err})")),
                Ok(fetcher) => {
                    let cancel = CancellationToken::new();
                    let run = async {
                        let _guard = session.exclusive().await;
                        check_kind(
                            fetcher.as_ref(),
                            session.connection().as_ref(),
                            kind,
                            &groups,
                            &cancel,
                        )
                        .await
                    };
                    match tokio::time::timeout(limits.cell, run).await {
                        Ok(Ok(detail)) => (CellStatus::Pass, detail),
                        Ok(Err(detail)) => (CellStatus::Fail, detail),
                        Err(_) => {
                            // Stops the statement on the server; the session may still be busy,
                            // so the later cells do not use it.
                            cancel.cancel();
                            abandoned = Some(kind);
                            (
                                CellStatus::Fail,
                                format!(
                                    "did not finish within {} (cancelled; the later cells of \
                                     this engine do not run)",
                                    human(limits.cell)
                                ),
                            )
                        }
                    }
                }
            }
        };
        let mut cell = outcome(e, kind, status, detail);
        cell.elapsed_ms = Some(millis(started.elapsed()));
        out.push(cell);
    }
    out
}

/// The seed names of `groups`, for messages.
fn seed_names(groups: &[SeedGroup]) -> String {
    groups
        .iter()
        .flat_map(|g| g.names.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `60 s`, or `250 ms` below whole seconds.
fn human(d: Duration) -> String {
    if d.subsec_millis() == 0 {
        format!("{} s", d.as_secs())
    } else {
        format!("{} ms", millis(d))
    }
}

/// Opens a session as the fixture user. For SQLite the session is on a fresh database file in
/// `scratch` (created here; the caller deletes it), seeded by the harness.
async fn open_session(
    ctx: &Ctx,
    e: &ResolvedEngine,
    seed: Option<&M2EngineSeed>,
    scratch: Option<&Path>,
) -> Result<SessionHandle, String> {
    let mut p = ctx.profile(e, false).await?;
    if e.engine.is_networked() {
        return ctx
            .mgr
            .open(&p, &ProvidedSecrets::none(), CancellationToken::new())
            .await
            .map_err(|err| format!("open failed: {err}"));
    }
    let Some(dir) = scratch.filter(|_| e.engine == Engine::Sqlite) else {
        return Err(format!("no seeding rule for the file engine {}", e.engine));
    };
    let seed = seed.ok_or_else(|| format!("no [m2.{}] in the fixtures file", e.name))?;
    let script_path = seed
        .init_sql
        .as_deref()
        .map(in_workspace)
        .ok_or_else(|| format!("[m2.{}] has no init_sql", e.name))?;
    let script = std::fs::read_to_string(&script_path)
        .map_err(|err| format!("cannot read {}: {err}", script_path.display()))?;
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)
        .map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    let db = seed.database.as_deref().unwrap_or("main");
    p.address = ProfileAddress::File {
        path: dir.join(format!("{db}.sqlite")),
    };
    let s = ctx
        .mgr
        .open(&p, &ProvidedSecrets::none(), CancellationToken::new())
        .await
        .map_err(|err| format!("open failed: {err}"))?;
    if let Err(err) = seed_sqlite(s.connection().as_ref(), &script, seed, dir).await {
        let _ = ctx.mgr.close(s.id()).await;
        return Err(format!("seeding {}: {err}", script_path.display()));
    }
    Ok(s)
}

/// Applies the SQLite seed script and attaches the second database (`[m2.sqlite]`).
async fn seed_sqlite(
    conn: &dyn Connection,
    script: &str,
    seed: &M2EngineSeed,
    dir: &Path,
) -> Result<(), String> {
    exec(conn, script, &[]).await?;
    if let Some(alias) = seed.attached_as.as_deref() {
        let file = dir.join(format!("{alias}.sqlite"));
        exec(
            conn,
            &format!("ATTACH DATABASE ?1 AS {}", quote_ident(alias)),
            &[Value::Text(file.display().to_string())],
        )
        .await?;
        if let Some(table) = seed.attached_table.as_deref() {
            exec(
                conn,
                &format!(
                    "CREATE TABLE {}.{} (id INTEGER PRIMARY KEY)",
                    quote_ident(alias),
                    quote_ident(table)
                ),
                &[],
            )
            .await?;
        }
    }
    Ok(())
}

/// An ANSI-quoted identifier (fixture names only; never user input).
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

async fn exec(conn: &dyn Connection, sql: &str, params: &[Value]) -> Result<(), String> {
    let stream = conn
        .execute(sql, params, CancellationToken::new())
        .await
        .map_err(|err| err.to_string())?;
    oxbase_driver::collect(stream)
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// Checks one `fetch_list` cell against its seed groups (see the module docs). `Ok` carries a
/// one-line summary, `Err` the first problem.
/// `cancel` is passed to every `list` and `load`.
pub async fn check_kind(
    fetcher: &dyn Fetcher,
    conn: &dyn Connection,
    kind: ObjectKind,
    groups: &[SeedGroup],
    cancel: &CancellationToken,
) -> Result<String, String> {
    let mut parts = Vec::with_capacity(groups.len());
    for g in groups {
        let parent = describe(&g.parent);
        let rows = fetcher
            .list(conn, &g.parent, cancel.clone())
            .await
            .map_err(|err| format!("list under {parent}: {err}"))?;
        if let Some(r) = rows.iter().find(|r| r.obj.kind != kind) {
            return Err(format!(
                "list of {kind} under {parent} returned {} (a {})",
                r.obj, r.obj.kind
            ));
        }
        let missing: Vec<&str> = g
            .names
            .iter()
            .filter(|n| !rows.iter().any(|r| &r.obj.name == *n))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "{} not listed under {parent} (listed: {})",
                missing.join(", "),
                preview(&rows)
            ));
        }
        check_order(kind, &g.names, &rows).map_err(|err| format!("{err} under {parent}"))?;
        let mut loaded = 0usize;
        for name in &g.names {
            let want = normalized(&g.parent.child(kind, name.clone()));
            for row in rows.iter().filter(|r| &r.obj.name == name) {
                if normalized(&row.obj) != want {
                    return Err(format!(
                        "{name} is listed as {:?}, expected {:?} (parent.child)",
                        row.obj, want
                    ));
                }
                if row.system {
                    return Err(format!("{} is flagged as a system object", row.obj));
                }
                let def = fetcher
                    .load(conn, &row.obj, cancel.clone())
                    .await
                    .map_err(|err| format!("load {}: {err}", row.obj))?;
                check_def(&def, kind, name).map_err(|err| format!("load {}: {err}", row.obj))?;
                loaded += 1;
            }
        }
        parts.push(format!(
            "{} under {parent}: listed, {loaded} loaded ({} rows)",
            g.names.join(", "),
            rows.len()
        ));
    }
    Ok(parts.join("; "))
}

/// The order rule of `docs/m2-contracts.md` §3.3, checked on the seed rows: columns come in
/// ordinal order (`names` is that order), other kinds sorted by name (compared without case, as
/// the servers' default collations do; the seed names avoid the cases where collations disagree).
/// Databases are exempt: SQLite lists `main`, `temp`, then attached databases.
pub fn check_order(
    kind: ObjectKind,
    names: &[String],
    rows: &[ObjectSummary],
) -> Result<(), String> {
    let mut listed: Vec<&str> = rows
        .iter()
        .map(|r| r.obj.name.as_str())
        .filter(|n| names.iter().any(|s| s == n))
        .collect();
    // Overloads of one routine are one name.
    listed.dedup();
    match kind {
        ObjectKind::Database => Ok(()),
        ObjectKind::Column => {
            if listed.iter().copied().eq(names.iter().map(String::as_str)) {
                Ok(())
            } else {
                Err(format!(
                    "columns are not in ordinal order: listed {}, expected {}",
                    listed.join(", "),
                    names.join(", ")
                ))
            }
        }
        _ => {
            let mut sorted = listed.clone();
            sorted.sort_by_key(|n| n.to_lowercase());
            if sorted == listed {
                Ok(())
            } else {
                Err(format!(
                    "rows are not sorted by name: listed {}",
                    listed.join(", ")
                ))
            }
        }
    }
}

/// Whether a report's `fetch_list` cells are acceptable; [`gate`] options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GateOptions {
    /// Skipped cells are fine (`OXBASE_CONFORMANCE_ALLOW_MISSING` or `..._SKIP_FETCH`).
    pub skipped_ok: bool,
    /// Every fixture of an [`M2_ENGINES`] engine must have a registered tree layout and an
    /// `[m2.<engine>]` seed (`OXBASE_CONFORMANCE_REQUIRE_M2_FETCH=1`, set by the full CI lane),
    /// so a dialect registration left out of `oxbase_services::default_dialects` fails.
    pub require_m2: bool,
    /// The same for every [`M4_ENGINES`] fixture, SQL Server included (the 5-engine guard,
    /// `OXBASE_CONFORMANCE_REQUIRE_M4=1`, [`crate::guard`]).
    pub require_m4: bool,
}

impl GateOptions {
    /// The engines that must have a layout and a seed, and the variable that asked for it.
    fn required(&self) -> Option<(&'static [Engine], &'static str)> {
        let m4: &'static [Engine] = &M4_ENGINES;
        let m2: &'static [Engine] = &M2_ENGINES;
        if self.require_m4 {
            Some((m4, guard::REQUIRE_ENV))
        } else if self.require_m2 {
            Some((m2, REQUIRE_M2_ENV))
        } else {
            None
        }
    }
}

/// The environment variable that makes every M2 engine's tree layout and seed required.
pub const REQUIRE_M2_ENV: &str = "OXBASE_CONFORMANCE_REQUIRE_M2_FETCH";

/// Checks the `fetch_list` cells of a report and returns every problem (empty = acceptable):
///
/// - a cell with a registered Fetcher and a seed object passes; nothing fails;
/// - n/a only where nothing is seeded (else the version check is wrong: see the module docs);
/// - every kind of a registered [`TreeLayout`] passes or is n/a: pending means the Fetcher or
///   the seed object is missing;
/// - an engine with a registered layout has at least one passing cell;
/// - with [`GateOptions::require_m2`], every [`M2_ENGINES`] fixture has a layout and a seed.
///
/// [`TreeLayout`]: oxbase_dialect::TreeLayout
pub fn gate(
    cells: &[Cell],
    fixtures: &Fixtures,
    engines: &[ResolvedEngine],
    registry: &DialectRegistry,
    opts: GateOptions,
) -> Vec<String> {
    let mut problems = Vec::new();
    for e in engines {
        let layout = registry.tree_layout(e.engine).ok().map(|l| l.kinds());
        let runnable = runnable_kinds(registry, e.engine);
        let mine: Vec<&Cell> = cells
            .iter()
            .filter(|c| c.check == Check::FetchList && c.engine == e.name)
            .collect();
        for c in &mine {
            let kind = c.object_kind;
            let must_run = runnable.contains(&kind) && !groups_for(fixtures, e, kind).is_empty();
            let in_layout = layout.as_ref().is_some_and(|k| k.contains(&kind));
            let ok = match c.status {
                CellStatus::Pass => must_run,
                CellStatus::Fail => false,
                CellStatus::Pending => !must_run && !in_layout,
                CellStatus::NotApplicable => !must_run,
                CellStatus::Skipped => opts.skipped_ok,
            };
            if !ok {
                problems.push(format!(
                    "{} {kind} fetch_list: {} ({}; Fetcher and seed: {must_run}, tree layout \
                     kind: {in_layout})",
                    e.name,
                    c.status.label(),
                    c.detail.as_deref().unwrap_or("no detail")
                ));
            }
        }
        let skipped = opts.skipped_ok && mine.iter().any(|c| c.status == CellStatus::Skipped);
        if layout.is_some() && !skipped && !mine.iter().any(|c| c.status == CellStatus::Pass) {
            problems.push(format!(
                "{}: no fetch_list cell passed, but {} registers a tree layout",
                e.name, e.engine
            ));
        }
        if let Some((required, env)) = opts.required()
            && required.contains(&e.engine)
        {
            if layout.is_none() {
                problems.push(format!(
                    "{}: {} registers no tree layout in oxbase_services::default_dialects \
                     ({env} is set)",
                    e.name, e.engine
                ));
            }
            if fixtures.m2_seed(&e.name, e.engine).is_none() {
                problems.push(format!(
                    "{}: tests/fixtures.toml has no [m2.{}] ({env} is set)",
                    e.name, e.name
                ));
            }
        }
    }
    problems
}

/// A parent for messages: `the server` for the root, else the ref's display form.
fn describe(parent: &ObjectRef) -> String {
    if parent.is_root() {
        "the server".to_owned()
    } else {
        parent.to_string()
    }
}

/// A ref without its routine signature, with an unset `parent_kind` of a table child read as
/// `Table` (what [`ObjectRef::owner`] does).
fn normalized(r: &ObjectRef) -> ObjectRef {
    let mut r = r.clone();
    r.signature = None;
    if r.parent.is_some() && r.parent_kind.is_none() {
        r.parent_kind = Some(ObjectKind::Table);
    }
    r
}

/// Checks a loaded definition: its kind and name, and that its JSON parses back unchanged.
pub fn check_def(def: &ObjectDef, kind: ObjectKind, name: &str) -> Result<(), String> {
    if def.kind() != kind {
        return Err(format!("returned a {} definition", def.kind()));
    }
    if def.name() != name {
        return Err(format!("returned the definition of {:?}", def.name()));
    }
    let json = serde_json::to_value(def).map_err(|err| format!("does not serialize: {err}"))?;
    let back: ObjectDef = serde_json::from_value(json.clone())
        .map_err(|err| format!("its JSON does not parse back: {err}"))?;
    let again =
        serde_json::to_value(&back).map_err(|err| format!("does not serialize again: {err}"))?;
    if again != json {
        return Err("its JSON does not round-trip".to_owned());
    }
    Ok(())
}

/// Up to ten listed names, for error messages.
fn preview(rows: &[ObjectSummary]) -> String {
    const MAX: usize = 10;
    let mut names: Vec<&str> = rows.iter().take(MAX).map(|r| r.obj.name.as_str()).collect();
    if rows.len() > MAX {
        names.push("...");
    }
    if names.is_empty() {
        "nothing".to_owned()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use oxbase_driver::mock::MockDriver;
    use oxbase_driver::{ConnectParams, Driver as _};
    use std::sync::Mutex;

    use arrow_array::{Array as _, StringArray};
    use oxbase_dialect::{ChildGroup, DatabaseScope, TreeLayout};
    use oxbase_types::{Error, Extras, Result, SchemaDef, ViewDef};

    use super::*;
    use crate::matrix::plan_engine_in;

    /// Lists fixed rows and loads a view (or schema) definition of the requested ref.
    struct Fake {
        rows: Vec<ObjectSummary>,
        /// Overrides the loaded definition's name.
        load_name: Option<&'static str>,
        fail_list: bool,
    }

    impl Fake {
        fn new(rows: Vec<ObjectSummary>) -> Self {
            Self {
                rows,
                load_name: None,
                fail_list: false,
            }
        }
    }

    #[async_trait]
    impl Fetcher for Fake {
        async fn list(
            &self,
            _c: &dyn Connection,
            _parent: &ObjectRef,
            _cancel: CancellationToken,
        ) -> Result<Vec<ObjectSummary>> {
            if self.fail_list {
                return Err(Error::unsupported("boom"));
            }
            Ok(self.rows.clone())
        }

        async fn load(
            &self,
            _c: &dyn Connection,
            obj: &ObjectRef,
            _cancel: CancellationToken,
        ) -> Result<ObjectDef> {
            let mut obj = obj.clone();
            if let Some(n) = self.load_name {
                n.clone_into(&mut obj.name);
            }
            Ok(def_of(obj))
        }
    }

    /// A view definition for a view ref, else a schema/database one.
    fn def_of(obj: ObjectRef) -> ObjectDef {
        if obj.kind == ObjectKind::View {
            ObjectDef::View(ViewDef {
                obj,
                definition: "SELECT 1".to_owned(),
                columns: Vec::new(),
                comment: None,
                extras: Extras::default(),
            })
        } else {
            ObjectDef::Schema(SchemaDef::new(obj))
        }
    }

    /// Reads the catalog of a live SQLite session: databases under the root, views under a
    /// database. So a run with it sees exactly what the harness seeded and attached.
    struct SqliteCatalog;

    #[async_trait]
    impl Fetcher for SqliteCatalog {
        async fn list(
            &self,
            c: &dyn Connection,
            parent: &ObjectRef,
            cancel: CancellationToken,
        ) -> Result<Vec<ObjectSummary>> {
            let (kind, sql, params) = if parent.is_root() {
                (
                    ObjectKind::Database,
                    "SELECT name FROM pragma_database_list ORDER BY seq",
                    Vec::new(),
                )
            } else {
                (
                    ObjectKind::View,
                    "SELECT name FROM pragma_table_list WHERE schema = ?1 AND type = 'view' \
                     ORDER BY name",
                    vec![Value::Text(parent.name.clone())],
                )
            };
            let got = oxbase_driver::collect(c.execute(sql, &params, cancel).await?).await?;
            let mut rows = Vec::new();
            for batch in got.result_sets.iter().flat_map(|s| &s.batches) {
                let names = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| Error::unsupported("name column is not text"))?;
                for name in names.iter().flatten() {
                    rows.push(ObjectSummary::new(parent.child(kind, name)));
                }
            }
            Ok(rows)
        }

        async fn load(
            &self,
            _c: &dyn Connection,
            obj: &ObjectRef,
            _cancel: CancellationToken,
        ) -> Result<ObjectDef> {
            Ok(def_of(obj.clone()))
        }
    }

    /// Never finishes a list; keeps the token it got so the test can see it cancelled.
    #[derive(Default)]
    struct Hang {
        token: Mutex<Option<CancellationToken>>,
    }

    #[async_trait]
    impl Fetcher for Hang {
        async fn list(
            &self,
            _c: &dyn Connection,
            _parent: &ObjectRef,
            cancel: CancellationToken,
        ) -> Result<Vec<ObjectSummary>> {
            *self.token.lock().unwrap() = Some(cancel);
            std::future::pending().await
        }

        async fn load(
            &self,
            _c: &dyn Connection,
            _obj: &ObjectRef,
            _cancel: CancellationToken,
        ) -> Result<ObjectDef> {
            std::future::pending().await
        }
    }

    async fn check(
        f: &dyn Fetcher,
        c: &dyn Connection,
        kind: ObjectKind,
        groups: &[SeedGroup],
    ) -> std::result::Result<String, String> {
        check_kind(f, c, kind, groups, &CancellationToken::new()).await
    }

    async fn conn() -> Box<dyn Connection> {
        MockDriver::new(Engine::Postgres)
            .connect(&ConnectParams::new(Engine::Postgres))
            .await
            .unwrap()
    }

    fn db() -> ObjectRef {
        ObjectRef::database("oxbase_test")
    }

    fn views(names: &[&str]) -> Vec<ObjectSummary> {
        names
            .iter()
            .map(|n| ObjectSummary::new(db().child(ObjectKind::View, *n)))
            .collect()
    }

    fn group(names: &[&str]) -> Vec<SeedGroup> {
        vec![SeedGroup {
            parent: db(),
            names: names.iter().map(|&n| n.to_owned()).collect(),
        }]
    }

    #[tokio::test]
    async fn passes_when_every_seed_is_listed_and_loads() {
        let c = conn().await;
        let f = Fake::new(views(&["a_view", "m2_view"]));
        let detail = check(&f, c.as_ref(), ObjectKind::View, &group(&["m2_view"]))
            .await
            .unwrap();
        assert_eq!(
            detail,
            "m2_view under database:oxbase_test: listed, 1 loaded (2 rows)"
        );
    }

    #[tokio::test]
    async fn databases_list_under_the_root() {
        let c = conn().await;
        let rows = vec![ObjectSummary::new(
            ObjectRef::root().child(ObjectKind::Database, "oxbase_test"),
        )];
        let groups = vec![SeedGroup {
            parent: ObjectRef::root(),
            names: vec!["oxbase_test".to_owned()],
        }];
        let detail = check(&Fake::new(rows), c.as_ref(), ObjectKind::Database, &groups)
            .await
            .unwrap();
        assert!(
            detail.starts_with("oxbase_test under the server: listed"),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn fails_on_a_missing_seed() {
        let c = conn().await;
        let f = Fake::new(views(&["other"]));
        let err = check(&f, c.as_ref(), ObjectKind::View, &group(&["m2_view"]))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            "m2_view not listed under database:oxbase_test (listed: other)"
        );
    }

    #[tokio::test]
    async fn fails_on_a_ref_outside_the_parent_convention() {
        let c = conn().await;
        // No catalog: the fetcher did not build the ref with `parent.child`.
        let f = Fake::new(vec![ObjectSummary::new(ObjectRef::new(
            ObjectKind::View,
            "m2_view",
        ))]);
        let err = check(&f, c.as_ref(), ObjectKind::View, &group(&["m2_view"]))
            .await
            .unwrap_err();
        assert!(err.contains("expected"), "{err}");
    }

    #[tokio::test]
    async fn fails_on_system_rows_wrong_kinds_bad_loads_and_list_errors() {
        let c = conn().await;
        let mut rows = views(&["m2_view"]);
        rows[0].system = true;
        let err = check(
            &Fake::new(rows),
            c.as_ref(),
            ObjectKind::View,
            &group(&["m2_view"]),
        )
        .await
        .unwrap_err();
        assert!(err.contains("system object"), "{err}");

        let rows = vec![ObjectSummary::new(db().child(ObjectKind::Table, "t"))];
        let err = check(
            &Fake::new(rows),
            c.as_ref(),
            ObjectKind::View,
            &group(&["t"]),
        )
        .await
        .unwrap_err();
        assert!(err.contains("(a table)"), "{err}");

        let mut f = Fake::new(views(&["m2_view"]));
        f.load_name = Some("other");
        let err = check(&f, c.as_ref(), ObjectKind::View, &group(&["m2_view"]))
            .await
            .unwrap_err();
        assert!(err.contains("definition of \"other\""), "{err}");

        let mut f = Fake::new(Vec::new());
        f.fail_list = true;
        let err = check(&f, c.as_ref(), ObjectKind::View, &group(&["m2_view"]))
            .await
            .unwrap_err();
        assert!(
            err.starts_with("list under database:oxbase_test: "),
            "{err}"
        );
    }

    #[test]
    fn routine_signatures_and_implicit_table_parents_are_normalized() {
        let table = ObjectRef::table(Some("db"), Some("s"), "t");
        let mut col = table.child(ObjectKind::Column, "c");
        col.parent_kind = None;
        assert_eq!(normalized(&col), table.child(ObjectKind::Column, "c"));
        let f = ObjectRef::schema(Some("db"), "s")
            .child(ObjectKind::Function, "f")
            .with_signature("n integer");
        assert_eq!(f.signature.as_deref(), Some("n integer"));
        assert_eq!(normalized(&f).signature, None);
    }

    #[test]
    fn check_def_rejects_the_wrong_kind() {
        let def = ObjectDef::Schema(SchemaDef::new(ObjectRef::database("d")));
        assert!(check_def(&def, ObjectKind::Database, "d").is_ok());
        assert_eq!(
            check_def(&def, ObjectKind::Schema, "d").unwrap_err(),
            "returned a database definition"
        );
    }

    const NO_KINDS: Vec<ObjectKind> = Vec::new();

    #[test]
    fn engines_without_a_driver_or_fetchers_run_nothing() {
        let reg = adapter::provisional_registry();
        for engine in Engine::ALL {
            assert_eq!(runnable_kinds(&reg, engine), NO_KINDS, "{engine}");
        }
        let mut reg = adapter::provisional_registry();
        reg.register_fetcher(
            Engine::Redis,
            ObjectKind::Key,
            Arc::new(Fake::new(Vec::new())),
        );
        reg.register_fetcher(
            Engine::Sqlite,
            ObjectKind::View,
            Arc::new(Fake::new(Vec::new())),
        );
        // Redis has no driver before M7.
        assert_eq!(runnable_kinds(&reg, Engine::Redis), NO_KINDS);
        assert_eq!(runnable_kinds(&reg, Engine::Sqlite), [ObjectKind::View]);
    }

    #[test]
    fn quotes_fixture_identifiers() {
        assert_eq!(quote_ident("m2aux"), "\"m2aux\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    /// A seed script with a multi-statement trigger body (SQLite's parser splits it).
    const SEED_SQL: &str = "CREATE TABLE t_parent (id INTEGER PRIMARY KEY, code TEXT NOT NULL);
CREATE TABLE t_child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES t_parent (id));
CREATE VIEW m2_view AS SELECT c.id, p.code FROM t_child c JOIN t_parent p ON p.id = c.parent_id;
CREATE TRIGGER t_trg BEFORE INSERT ON t_child FOR EACH ROW WHEN NEW.id < 0
BEGIN
    SELECT RAISE(ABORT, 'negative; id');
END;
";

    /// A fixtures file with one SQLite fixture `name` seeded by `script` (written to `dir`).
    fn sqlite_fixture(dir: &Path, name: &str, script: &str) -> (Fixtures, Vec<ResolvedEngine>) {
        let script_path = dir.join(format!("{name}.sql"));
        std::fs::write(&script_path, script).unwrap();
        let text = format!(
            "schema_version = 1
[engines.{name}]
kind = \"sqlite\"
path = \"target/conformance/{name}-unused.sqlite\"
[m2]
tables = [\"t_parent\", \"t_child\"]
view = \"m2_view\"
trigger = \"t_trg\"
trigger_table = \"t_child\"
[m2.{name}]
init_sql = '{}'
database = \"main\"
attached_as = \"m2aux\"
attached_table = \"m2_aux_t\"
objects = [\"t_parent\", \"t_child\", \"m2_view\", \"t_trg\"]
",
            script_path.display()
        );
        let fx = Fixtures::parse(&text, name).unwrap();
        let engines = fx.resolve().unwrap();
        (fx, engines)
    }

    /// The provisional registry plus the given SQLite fetchers.
    fn sqlite_registry(fetchers: Vec<(ObjectKind, Arc<dyn Fetcher>)>) -> DialectRegistry {
        let mut reg = adapter::provisional_registry();
        for (kind, f) in fetchers {
            reg.register_fetcher(Engine::Sqlite, kind, f);
        }
        reg
    }

    fn find(out: &[FetchOutcome], kind: ObjectKind) -> &FetchOutcome {
        out.iter()
            .find(|o| o.kind == kind)
            .unwrap_or_else(|| panic!("no {kind} outcome in {out:?}"))
    }

    /// The whole path on a real SQLite session: scratch file, seed script, ATTACH, list and
    /// load through the session, close, scratch directory removed.
    #[test]
    fn run_all_seeds_sqlite_and_passes_the_cells() {
        let tmp = tempfile::tempdir().unwrap();
        let (fx, engines) = sqlite_fixture(tmp.path(), "lite_ok", SEED_SQL);
        let catalog: Arc<dyn Fetcher> = Arc::new(SqliteCatalog);
        let reg = sqlite_registry(vec![
            (ObjectKind::Database, catalog.clone()),
            (ObjectKind::View, catalog),
        ]);
        let out = run_all(&fx, &engines, &[true], &reg);
        assert_eq!(out.len(), 2, "{out:?}");
        for o in &out {
            assert_eq!(o.status, CellStatus::Pass, "{o:?}");
            assert!(o.elapsed_ms.is_some(), "{o:?}");
        }
        assert!(
            find(&out, ObjectKind::Database)
                .detail
                .starts_with("main, m2aux under the server: listed, 2 loaded"),
            "{out:?}"
        );
        assert!(
            find(&out, ObjectKind::View)
                .detail
                .starts_with("m2_view under database:main: listed, 1 loaded"),
            "{out:?}"
        );
        assert!(!scratch_dir(&engines[0]).exists());
        // Unreachable engines run nothing.
        assert_eq!(run_all(&fx, &engines, &[false], &reg), Vec::new());
    }

    #[test]
    fn a_failing_seed_script_fails_the_seeded_cells_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (fx, engines) = sqlite_fixture(tmp.path(), "lite_bad", "CREATE TABLE broken (;");
        let catalog: Arc<dyn Fetcher> = Arc::new(SqliteCatalog);
        // Index has a fetcher but no seed: it keeps its planned status (no outcome).
        let reg = sqlite_registry(vec![
            (ObjectKind::Database, catalog.clone()),
            (ObjectKind::View, catalog.clone()),
            (ObjectKind::Index, catalog),
        ]);
        let out = run_all(&fx, &engines, &[true], &reg);
        assert_eq!(out.len(), 2, "{out:?}");
        for o in &out {
            assert_eq!(o.status, CellStatus::Fail, "{o:?}");
            assert!(
                o.detail.starts_with("cannot open a session: seeding "),
                "{o:?}"
            );
        }
        assert!(!scratch_dir(&engines[0]).exists());
    }

    #[test]
    fn a_timed_out_cell_is_cancelled_and_stops_the_engine() {
        let tmp = tempfile::tempdir().unwrap();
        let (fx, engines) = sqlite_fixture(tmp.path(), "lite_hang", SEED_SQL);
        let hang = Arc::new(Hang::default());
        let hanging: Arc<dyn Fetcher> = hang.clone();
        let catalog: Arc<dyn Fetcher> = Arc::new(SqliteCatalog);
        let reg = sqlite_registry(vec![
            (ObjectKind::Database, hanging),
            (ObjectKind::View, catalog),
        ]);
        let limits = Timeouts {
            cell: Duration::from_millis(300),
            ..Timeouts::default()
        };
        let out = run_all_with(&fx, &engines, &[true], &reg, limits);
        let db = find(&out, ObjectKind::Database);
        assert_eq!(db.status, CellStatus::Fail);
        assert!(
            db.detail.starts_with("did not finish within 300 ms"),
            "{db:?}"
        );
        let view = find(&out, ObjectKind::View);
        assert_eq!(view.status, CellStatus::Fail);
        assert_eq!(
            view.detail,
            "not run: the session was given up after the database cell timed out"
        );
        let token = hang.token.lock().unwrap().clone().expect("list was called");
        assert!(token.is_cancelled());
        assert!(!scratch_dir(&engines[0]).exists());
    }

    fn rows(names: &[&str]) -> Vec<ObjectSummary> {
        names
            .iter()
            .map(|n| ObjectSummary::new(db().child(ObjectKind::Table, *n)))
            .collect()
    }

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|&n| n.to_owned()).collect()
    }

    #[test]
    fn order_rules() {
        let cols = strings(&["id", "code", "note"]);
        assert!(check_order(ObjectKind::Column, &cols, &rows(&["id", "code", "note"])).is_ok());
        // Other columns may sit between the seed columns.
        assert!(
            check_order(
                ObjectKind::Column,
                &cols,
                &rows(&["id", "x", "code", "note"])
            )
            .is_ok()
        );
        assert_eq!(
            check_order(ObjectKind::Column, &cols, &rows(&["code", "id", "note"])).unwrap_err(),
            "columns are not in ordinal order: listed code, id, note, expected id, code, note"
        );
        let tables = strings(&["m2_parent", "m2_child"]);
        // Only the seed rows are compared; case is ignored.
        assert!(
            check_order(
                ObjectKind::Table,
                &tables,
                &rows(&["Z", "m2_child", "a", "m2_parent"])
            )
            .is_ok()
        );
        assert_eq!(
            check_order(
                ObjectKind::Table,
                &tables,
                &rows(&["m2_parent", "m2_child"])
            )
            .unwrap_err(),
            "rows are not sorted by name: listed m2_parent, m2_child"
        );
        // Overloads list one name several times in a row.
        let fns = strings(&["m2_fn", "m2_trg_fn"]);
        assert!(
            check_order(
                ObjectKind::Function,
                &fns,
                &rows(&["m2_fn", "m2_fn", "m2_trg_fn"])
            )
            .is_ok()
        );
        // Databases keep the engine's order (SQLite: main, temp, attached).
        let dbs = strings(&["main", "m2aux"]);
        assert!(check_order(ObjectKind::Database, &dbs, &rows(&["main", "m2aux"])).is_ok());
    }

    #[tokio::test]
    async fn check_kind_reports_order_errors_with_the_parent() {
        let c = conn().await;
        let f = Fake::new(views(&["m2_view_b", "m2_view_a"]));
        let err = check(
            &f,
            c.as_ref(),
            ObjectKind::View,
            &group(&["m2_view_a", "m2_view_b"]),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err,
            "rows are not sorted by name: listed m2_view_b, m2_view_a under database:oxbase_test"
        );
    }

    /// The planned cells of `e`, with the given statuses for some kinds.
    fn cells(
        reg: &DialectRegistry,
        e: &ResolvedEngine,
        set: &[(ObjectKind, CellStatus)],
    ) -> Vec<Cell> {
        let mut cells = plan_engine_in(reg, &e.name, e.engine);
        for c in cells.iter_mut().filter(|c| c.check == Check::FetchList) {
            if let Some(&(_, s)) = set.iter().find(|(k, _)| *k == c.object_kind) {
                c.status = s;
            }
        }
        cells
    }

    fn layout_registry() -> DialectRegistry {
        let catalog: Arc<dyn Fetcher> = Arc::new(SqliteCatalog);
        let mut reg = sqlite_registry(vec![
            (ObjectKind::Database, catalog.clone()),
            (ObjectKind::View, catalog),
        ]);
        reg.register_tree_layout(
            Engine::Sqlite,
            TreeLayout::new(DatabaseScope::Attached)
                .with_root(&[ChildGroup::direct(ObjectKind::Database)])
                .with_children(
                    ObjectKind::Database,
                    &[ChildGroup::folder(ObjectKind::View)],
                ),
        );
        reg
    }

    #[test]
    fn gate_accepts_passing_layout_kinds_and_rejects_the_rest() {
        use CellStatus as S;
        use ObjectKind as K;
        let tmp = tempfile::tempdir().unwrap();
        let (fx, engines) = sqlite_fixture(tmp.path(), "lite_gate", SEED_SQL);
        let e = &engines[0];
        let reg = layout_registry();
        let opts = GateOptions::default();
        let problems = |set: &[(K, S)], opts: GateOptions| {
            gate(&cells(&reg, e, set), &fx, &engines, &reg, opts)
        };

        let all_pass = [(K::Database, S::Pass), (K::View, S::Pass)];
        assert_eq!(problems(&all_pass, opts), Vec::<String>::new());
        assert_eq!(
            problems(
                &all_pass,
                GateOptions {
                    require_m2: true,
                    ..opts
                }
            ),
            Vec::<String>::new()
        );

        // A seeded kind reported n/a means a wrong version check.
        let p = problems(&[(K::Database, S::Pass), (K::View, S::NotApplicable)], opts);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].starts_with("lite_gate view fetch_list: n/a"), "{p:?}");

        // A layout kind left pending (no Fetcher, or nothing seeded) fails.
        let p = problems(&[(K::Database, S::Pass)], opts);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("tree layout kind: true"), "{p:?}");

        // Skipped cells only with skipped_ok; then no pass is needed either.
        let skipped = [(K::Database, S::Skipped), (K::View, S::Skipped)];
        assert_eq!(problems(&skipped, opts).len(), 3);
        let ok = GateOptions {
            skipped_ok: true,
            ..opts
        };
        assert_eq!(problems(&skipped, ok), Vec::<String>::new());
    }

    #[test]
    fn gate_requires_a_layout_for_m4_engines_when_guarded() {
        let tmp = tempfile::tempdir().unwrap();
        let (fx, engines) = sqlite_fixture(tmp.path(), "lite_m4", SEED_SQL);
        let reg = adapter::provisional_registry();
        let cells = plan_engine_in(&reg, "lite_m4", Engine::Sqlite);
        let p = gate(
            &cells,
            &fx,
            &engines,
            &reg,
            GateOptions {
                require_m4: true,
                ..GateOptions::default()
            },
        );
        assert_eq!(
            p,
            [
                "lite_m4: sqlite registers no tree layout in oxbase_services::default_dialects \
              (OXBASE_CONFORMANCE_REQUIRE_M4 is set)"
            ]
        );
    }

    #[test]
    fn gate_requires_a_layout_for_m2_engines_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let (fx, engines) = sqlite_fixture(tmp.path(), "lite_req", SEED_SQL);
        // Nothing registered: every cell stays pending, which is fine by default.
        let reg = adapter::provisional_registry();
        let cells = plan_engine_in(&reg, "lite_req", Engine::Sqlite);
        let opts = GateOptions::default();
        assert_eq!(
            gate(&cells, &fx, &engines, &reg, opts),
            Vec::<String>::new()
        );
        let p = gate(
            &cells,
            &fx,
            &engines,
            &reg,
            GateOptions {
                require_m2: true,
                ..opts
            },
        );
        assert_eq!(
            p,
            [
                "lite_req: sqlite registers no tree layout in oxbase_services::default_dialects \
              (OXBASE_CONFORMANCE_REQUIRE_M2_FETCH is set)"
            ]
        );
    }
}
