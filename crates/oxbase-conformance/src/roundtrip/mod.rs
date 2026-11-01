//! M3 `roundtrip` and `capabilities` cells for the designer kinds (`Table`, `Column`, `Index`,
//! `ForeignKey`, `Check`; [`DESIGNER_KINDS`]) on every engine that registered the roles.
//!
//! # Round trip
//!
//! Each cell works on its own scratch table(s) named from `tests/fixtures.toml` `[m3]`
//! ([`table_names`]: `<prefix><kind>_<token>`, plus `<prefix>fk_<token>_ref` for foreign keys),
//! in the `[m3.<engine>]` database (and schema), on one session per engine:
//!
//! 1. **create**: the kind's `DdlGenerator::create` (child kinds on a base table that the
//!    `Table` generator created and the round trip verified first) → run → fetch with the M2
//!    `Fetcher` → the kind's `Comparator` finds no difference to the definition the script was
//!    generated from, and neither does the `Table` comparator for the whole table. The base
//!    table has a reserved-word column (`order`) and gets one row (values bound; a quote, a
//!    backslash and non-ASCII text);
//! 2. **alter**: a representative change (table: rename and comment; column: NOT NULL, literal
//!    default, comment, then type, so a `MODIFY` must keep the default and the comment, then a
//!    rename to a reserved word with an embedded quote and non-ASCII text; index: columns and
//!    uniqueness; foreign key: delete action; check: expression) → run → fetch → no difference
//!    to the intended definition, and the difference between the table before and after is
//!    exactly the intended change (same objects, properties and kinds of change), and the row
//!    is still there with the same values;
//! 3. **drop** → run → fetch says `NotFound` (tables: also no longer listed).
//!
//! The comparators must not be blind to what they check: the intended change of every alter
//! must be a non-empty difference (for the `Table` comparator, and for the kind's on a child
//! change), and right after create the `Table` comparator must report a changed type and a
//! changed default of the fetched table, the `ForeignKey` one a changed referenced table.
//!
//! Scripts run with the semantics of the `oxbase_types::script` module docs ([`run_script`]),
//! in one transaction when the engine has transactional DDL and the script is atomic.
//!
//! A change both the designer capabilities and the generator call unsupported is skipped and
//! noted. Only the contract's exceptions may be skipped (comments on SQLite; checks on MySQL
//! before 8.0.16 and MariaDB before 10.2); any other skip leaves the round trip pending, which
//! the required gate rejects. The cell's scratch tables are dropped afterwards whatever
//! happened (`DROP TABLE IF EXISTS`, child before parent, also after a failure, a timeout or a
//! panic), and the cell fails if one is still listed. Leftovers of killed runs whose token is
//! older than `stale_after_secs` are dropped when an engine's run starts.
//!
//! # Capabilities
//!
//! The `capabilities` cell of the same (engine, kind) checks `Capabilities::designer` against
//! what the round trip saw:
//!
//! - every operation the round trip used is supported, and every one the capabilities call
//!   unsupported fails in the generator with `Unsupported` (and the other way round);
//! - `rebuild` operations produce destructive steps;
//! - `transactionalDdl` agrees with the scripts (all main steps transactional, or none) and,
//!   for `Table`, with a live probe (a `CREATE TABLE` rolled back inside a transaction is gone
//!   exactly when DDL is transactional);
//! - `checksEnforced` agrees with a live insert that violates the check (`Check`);
//! - the referential actions used are in `fkActions` (`ForeignKey`).
//!
//! It is pending when the round trip failed before it could check anything.
//!
//! # Pending cells
//!
//! A cell runs once its engine has a driver and registered, in the registry the app uses
//! (`oxbase_services::default_dialects`), the `Table` and the kind's `DdlGenerator`,
//! `Comparator` and `Fetcher`, plus `Quoting` and `Capabilities` ([`missing_roles`]). Until
//! then it stays pending, naming what is missing. [`gate`] with
//! `OXBASE_CONFORMANCE_REQUIRE_M3_ROUNDTRIP=1` turns pending (and anything else but pass) on
//! the M3 engines into failures.

mod cells;
mod exec;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use oxbase_dialect::{DESIGNER_KINDS, DialectRegistry, Fetcher, Quoting};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_services::{ProvidedSecrets, SessionHandle};
use oxbase_store::ProfileAddress;
use oxbase_types::{Engine, ObjectKind, ObjectRef};

pub use exec::{cell_text, exec, first_text, run_script};

use crate::adapter;
use crate::connect::{Ctx, millis};
use crate::fixtures::{Fixtures, M3EngineSeed, ResolvedEngine};
use crate::matrix::{Cell, CellStatus, Check, driver_milestone};
use crate::runner::RunOptions;

/// The engines whose round trip M3 delivers (`docs/m3-contracts.md` §12).
pub const M3_ENGINES: [Engine; 4] = [
    Engine::Postgres,
    Engine::MySql,
    Engine::MariaDb,
    Engine::Sqlite,
];

/// `[m3] roundtrip_prefix` when the fixtures file does not set one.
pub const DEFAULT_PREFIX: &str = "m3_rt_";
/// `[m3] stale_after_secs` when the fixtures file does not set one.
pub const DEFAULT_STALE_AFTER_SECS: u64 = 3600;
/// Length of a scratch-name token (base36: 6 characters of unix seconds, 4 of process id).
pub const TOKEN_LEN: usize = 10;
const SECS_DIGITS: usize = 6;
const PID_DIGITS: usize = 4;
/// The seconds field of a token holds unix seconds modulo this (36^6 s, about 69 years; it
/// wraps on 2038-12-24 and again in 2107), so ages are computed modulo it too ([`token_age`]).
pub const SECS_WRAP: u64 = 36u64.pow(6);

/// The environment variable that makes every M3 cell on the M3 engines required.
pub const REQUIRE_ENV: &str = "OXBASE_CONFORMANCE_REQUIRE_M3_ROUNDTRIP";

/// Time limits of one engine's round-trip run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Opening the session.
    pub open: Duration,
    /// One cell (every create, alter, fetch and drop of one kind).
    pub cell: Duration,
    /// Dropping one cell's scratch tables (and the stale ones at start).
    pub cleanup: Duration,
    /// Closing the session.
    pub close: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            open: Duration::from_secs(60),
            cell: Duration::from_secs(180),
            cleanup: Duration::from_secs(60),
            close: Duration::from_secs(15),
        }
    }
}

/// The result of one `roundtrip` or `capabilities` cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtOutcome {
    /// The fixture name (`[engines.<name>]`).
    pub engine: String,
    pub kind: ObjectKind,
    pub check: Check,
    pub status: CellStatus,
    pub detail: String,
    pub elapsed_ms: Option<u64>,
}

/// The roles a round trip of `kind` on `engine` needs that `registry` does not have (empty =
/// the cell can run): the `Table` and `kind` `DdlGenerator`, `Comparator` and `Fetcher`,
/// `Quoting` and `Capabilities`.
pub fn missing_roles(registry: &DialectRegistry, engine: Engine, kind: ObjectKind) -> Vec<String> {
    let mut out = Vec::new();
    let mut kinds = vec![ObjectKind::Table];
    if kind != ObjectKind::Table {
        kinds.push(kind);
    }
    for k in kinds {
        if registry.ddl(engine, k).is_err() {
            out.push(format!("{k} DdlGenerator"));
        }
        if registry.comparator(engine, k).is_err() {
            out.push(format!("{k} Comparator"));
        }
        if registry.fetcher(engine, k).is_err() {
            out.push(format!("{k} Fetcher"));
        }
    }
    if registry.quoting(engine).is_err() {
        out.push("Quoting".to_owned());
    }
    if registry.capabilities(engine).is_err() {
        out.push("Capabilities".to_owned());
    }
    out
}

/// The designer kinds whose cells can run on `engine`: it has a driver, the kind applies and
/// every role is registered.
pub fn runnable_kinds(registry: &DialectRegistry, engine: Engine) -> Vec<ObjectKind> {
    if driver_milestone(engine).is_some() {
        return Vec::new();
    }
    DESIGNER_KINDS
        .into_iter()
        .filter(|&k| {
            adapter::applies(registry, engine, k) && missing_roles(registry, engine, k).is_empty()
        })
        .collect()
}

/// The planned (pre-run) detail of a `roundtrip` or `capabilities` cell of a designer kind.
pub fn pending_detail(
    registry: &DialectRegistry,
    engine: Engine,
    kind: ObjectKind,
    check: Check,
) -> String {
    let missing = missing_roles(registry, engine, kind);
    match (check, missing.is_empty()) {
        (_, true) => "pending (roles registered; not run)".to_owned(),
        (Check::Capabilities, false) => format!(
            "pending (needs the round trip; missing: {})",
            missing.join(", ")
        ),
        (_, false) => format!("pending (missing: {})", missing.join(", ")),
    }
}

static RUN_SEQ: AtomicU32 = AtomicU32::new(0);

/// A fresh scratch-name token: [`TOKEN_LEN`] lowercase base36 characters, the unix seconds
/// (readable back with [`token_secs`]) then the process id (plus a per-process run counter, so
/// two runs in one process and second differ).
pub fn new_token() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let seq = RUN_SEQ.fetch_add(1, Ordering::Relaxed);
    token(
        secs,
        std::process::id().wrapping_add(seq.wrapping_mul(7919)),
    )
}

/// The token of `secs` and `pid` (see [`new_token`]).
pub fn token(secs: u64, pid: u32) -> String {
    let mut out = base36(secs, SECS_DIGITS);
    out.push_str(&base36(u64::from(pid), PID_DIGITS));
    out
}

/// `n` in lowercase base36, exactly `width` digits (zero-padded; higher digits dropped).
fn base36(mut n: u64, width: usize) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = vec![b'0'; width];
    for slot in out.iter_mut().rev() {
        *slot = DIGITS[usize::try_from(n % 36).unwrap_or(0)];
        n /= 36;
    }
    String::from_utf8(out).unwrap_or_default()
}

/// The unix seconds a token was made at, modulo [`SECS_WRAP`].
pub fn token_secs(token: &str) -> Option<u64> {
    if !is_token(token) {
        return None;
    }
    u64::from_str_radix(&token[..SECS_DIGITS], 36).ok()
}

/// How far in the future a token may be (another machine's clock) and still be read as such.
const FUTURE_SLACK_SECS: u64 = 86_400;

/// How many seconds before `now` (unix seconds) `token` was made, modulo [`SECS_WRAP`] so the
/// wrap of the seconds field does not make fresh tokens look old. A token from up to a day in
/// the future (another machine's clock) is `None`: never stale.
pub fn token_age(token: &str, now: u64) -> Option<u64> {
    let secs = token_secs(token)?;
    let age = (now % SECS_WRAP + SECS_WRAP - secs) % SECS_WRAP;
    (age < SECS_WRAP - FUTURE_SLACK_SECS).then_some(age)
}

fn is_token(s: &str) -> bool {
    s.len() == TOKEN_LEN
        && s.bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

/// The token in a scratch table name `name` (`<prefix>...`), if it is one.
pub fn scratch_token<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    name.strip_prefix(prefix)?
        .split('_')
        .find(|part| is_token(part))
}

/// The scratch tables a cell of `kind` may create, in drop order (a referencing table before
/// the table it references): `<prefix><kind>_<token>`, then the extra names of the kind
/// (`..._rn` after a table rename, `..._tx` for the transactional-DDL probe, and
/// `<prefix>fk_<token>_ref` for foreign keys).
pub fn table_names(prefix: &str, token: &str, kind: ObjectKind) -> Vec<String> {
    let main = format!("{prefix}{kind}_{token}");
    match kind {
        ObjectKind::Table => vec![main.clone(), format!("{main}_rn"), format!("{main}_tx")],
        ObjectKind::ForeignKey => vec![main, format!("{prefix}fk_{token}_ref")],
        _ => vec![main],
    }
}

/// The kinds `[m3] roundtrip_kinds` asks for, in [`DESIGNER_KINDS`] order (empty without
/// `[m3]`).
pub fn fixture_kinds(fixtures: &Fixtures) -> Vec<ObjectKind> {
    let wanted = fixtures
        .m3
        .as_ref()
        .and_then(|m3| m3.kinds().ok())
        .unwrap_or_default();
    DESIGNER_KINDS
        .into_iter()
        .filter(|k| wanted.contains(k))
        .collect()
}

/// The parent the scratch tables are listed under: the schema when `[m3.<engine>]` names one,
/// else the database.
pub fn container(seed: &M3EngineSeed) -> Option<ObjectRef> {
    let db = seed.database.as_deref().filter(|d| !d.is_empty())?;
    let database = ObjectRef::database(db);
    Some(match seed.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(schema) => database.child(ObjectKind::Schema, schema),
        None => database,
    })
}

/// The scratch directory a SQLite round trip runs in; removed after the run.
pub fn scratch_dir(e: &ResolvedEngine) -> PathBuf {
    std::env::temp_dir().join(format!(
        "oxbase-conformance-m3-{}-{}",
        e.name,
        std::process::id()
    ))
}

/// Runs the round-trip and capabilities cells of every reachable engine (`reachable[i]`
/// belongs to `engines[i]`). Engines run concurrently. Cells that cannot run produce no
/// outcome, so their planned status stays.
pub fn run_all(
    fixtures: &Fixtures,
    engines: &[ResolvedEngine],
    reachable: &[bool],
    registry: &DialectRegistry,
) -> Vec<RtOutcome> {
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
) -> Vec<RtOutcome> {
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
                    both(
                        e,
                        &runnable_kinds(registry, e.engine),
                        CellStatus::Fail,
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
                    async move { engine_cells(&ctx, registry, &fixtures, &e, limits).await },
                )
            })
            .collect();
        let mut out = Vec::new();
        for (task, e) in tasks.into_iter().zip(&todo) {
            match task.await {
                Ok(cells) => out.extend(cells),
                Err(err) => out.extend(both(
                    e,
                    &runnable_kinds(&registry, e.engine),
                    CellStatus::Fail,
                    &format!("round-trip task panicked: {err}"),
                )),
            }
        }
        out
    })
}

fn outcome(
    e: &ResolvedEngine,
    kind: ObjectKind,
    check: Check,
    status: CellStatus,
    detail: String,
) -> RtOutcome {
    RtOutcome {
        engine: e.name.clone(),
        kind,
        check,
        status,
        detail,
        elapsed_ms: None,
    }
}

/// The same status and detail for the `roundtrip` and `capabilities` cells of every kind.
fn both(e: &ResolvedEngine, kinds: &[ObjectKind], status: CellStatus, why: &str) -> Vec<RtOutcome> {
    kinds
        .iter()
        .flat_map(|&k| {
            [Check::Roundtrip, Check::Capabilities]
                .map(|check| outcome(e, k, check, status, why.to_owned()))
        })
        .collect()
}

/// Every runnable cell of one engine, on one session.
async fn engine_cells(
    ctx: &Ctx,
    registry: Arc<DialectRegistry>,
    fixtures: &Fixtures,
    e: &ResolvedEngine,
    limits: Timeouts,
) -> Vec<RtOutcome> {
    let runnable = runnable_kinds(&registry, e.engine);
    let Some((m3, seed)) = fixtures.m3_seed(&e.name, e.engine) else {
        return both(
            e,
            &runnable,
            CellStatus::Pending,
            &format!(
                "pending (roles registered; tests/fixtures.toml has no [m3.{}])",
                e.name
            ),
        );
    };
    let Some(container) = container(&seed) else {
        return both(
            e,
            &runnable,
            CellStatus::Pending,
            &format!(
                "pending (roles registered; [m3.{}] has no database)",
                e.name
            ),
        );
    };
    let wanted = fixture_kinds(fixtures);
    let (kinds, unlisted): (Vec<ObjectKind>, Vec<ObjectKind>) =
        runnable.into_iter().partition(|k| wanted.contains(k));
    let mut out = both(
        e,
        &unlisted,
        CellStatus::Pending,
        "pending (roles registered; not in [m3] roundtrip_kinds)",
    );
    if kinds.is_empty() {
        return out;
    }
    let prefix = m3
        .roundtrip_prefix
        .clone()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| DEFAULT_PREFIX.to_owned());
    let stale_after = m3.stale_after_secs.unwrap_or(DEFAULT_STALE_AFTER_SECS);

    let scratch = (!e.engine.is_networked()).then(|| scratch_dir(e));
    let opened = tokio::time::timeout(
        limits.open,
        open_session(ctx, e, seed.database.as_deref(), scratch.as_deref()),
    )
    .await
    .unwrap_or_else(|_| Err(format!("did not open within {} s", limits.open.as_secs())));
    match opened {
        Ok(session) => {
            let setup = Setup {
                registry,
                session: session.clone(),
                container,
                prefix,
                stale_after,
            };
            out.extend(run_cells(e, setup, &kinds, limits).await);
            let _ = tokio::time::timeout(limits.close, ctx.mgr.close(session.id())).await;
        }
        Err(why) => out.extend(both(
            e,
            &kinds,
            CellStatus::Fail,
            &format!("cannot open a session: {why}"),
        )),
    }
    if let Some(dir) = scratch {
        let _ = std::fs::remove_dir_all(dir);
    }
    out
}

/// What [`run_cells`] needs besides the kinds.
struct Setup {
    registry: Arc<DialectRegistry>,
    session: SessionHandle,
    container: ObjectRef,
    prefix: String,
    stale_after: u64,
}

/// Opens a session as the fixture user, on the `[m3.<engine>]` database. SQLite gets a fresh
/// file in `scratch` (created here; the caller deletes it).
async fn open_session(
    ctx: &Ctx,
    e: &ResolvedEngine,
    database: Option<&str>,
    scratch: Option<&Path>,
) -> Result<SessionHandle, String> {
    let mut p = ctx.profile(e, false).await?;
    if e.engine.is_networked() {
        if let Some(db) = database {
            p.database = Some(db.to_owned());
        }
    } else {
        let dir =
            scratch.ok_or_else(|| format!("no scratch rule for the file engine {}", e.engine))?;
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
        p.address = ProfileAddress::File {
            path: dir.join("roundtrip.sqlite"),
        };
    }
    ctx.mgr
        .open(&p, &ProvidedSecrets::none(), CancellationToken::new())
        .await
        .map_err(|err| format!("open failed: {err}"))
}

/// Runs the cells of one engine, each followed by its cleanup.
async fn run_cells(
    e: &ResolvedEngine,
    setup: Setup,
    kinds: &[ObjectKind],
    limits: Timeouts,
) -> Vec<RtOutcome> {
    let Setup {
        registry,
        session,
        container,
        prefix,
        stale_after,
    } = setup;
    let version = session.info().server_version.clone();
    let mut out = Vec::new();
    let (quoting, caps) = match (registry.quoting(e.engine), registry.capabilities(e.engine)) {
        (Ok(q), Ok(c)) => (q, c),
        (Err(err), _) | (_, Err(err)) => {
            return both(e, kinds, CellStatus::Pending, &format!("pending ({err})"));
        }
    };
    let table_fetcher = match registry.fetcher(e.engine, ObjectKind::Table) {
        Ok(f) => f,
        Err(err) => return both(e, kinds, CellStatus::Pending, &format!("pending ({err})")),
    };
    let designer = caps.designer(&version);
    let token = new_token();

    // Leftovers of killed runs.
    let stale = tokio::time::timeout(
        limits.cleanup,
        drop_stale(
            session.connection().as_ref(),
            quoting.as_ref(),
            table_fetcher.as_ref(),
            &container,
            &prefix,
            stale_after,
        ),
    )
    .await
    .unwrap_or_else(|_| Err("timed out".to_owned()));
    let mut note = match stale {
        Ok(0) => None,
        Ok(n) => Some(format!("dropped {n} stale scratch table(s) first")),
        Err(err) => Some(format!("could not drop stale scratch tables: {err}")),
    };

    let mut abandoned: Option<ObjectKind> = None;
    for &kind in kinds {
        let started = Instant::now();
        if let Some(stuck) = abandoned {
            out.extend(both(
                e,
                &[kind],
                CellStatus::Fail,
                &format!("not run: the session was given up after the {stuck} cell timed out"),
            ));
            continue;
        }
        if !registry.supports_kind(e.engine, kind, &version) {
            out.extend(both(
                e,
                &[kind],
                CellStatus::NotApplicable,
                &format!("not on server {}", version.raw()),
            ));
            continue;
        }
        let names = table_names(&prefix, &token, kind);
        let refs: Vec<ObjectRef> = names
            .iter()
            .map(|n| container.child(ObjectKind::Table, n.clone()))
            .collect();
        let cancel = CancellationToken::new();
        let env = Arc::new(cells::Env {
            engine: e.engine,
            registry: registry.clone(),
            session: session.clone(),
            version: version.clone(),
            caps: designer.clone(),
            quoting: quoting.clone(),
            container: container.clone(),
            names,
            cancel: cancel.clone(),
        });
        let mut task = tokio::spawn(cells::run(env, kind));
        let report = match tokio::time::timeout(limits.cell, &mut task).await {
            Ok(Ok(r)) => r,
            Ok(Err(join)) => cells::CellReport::failed(&format!("the cell panicked: {join}")),
            Err(_) => {
                cancel.cancel();
                task.abort();
                let _ = task.await;
                let _ = session.connection().cancel_running().await;
                abandoned = Some(kind);
                cells::CellReport::failed(&format!(
                    "did not finish within {} s (cancelled; the later cells of this engine do \
                     not run)",
                    limits.cell.as_secs()
                ))
            }
        };
        // Cleanup always runs, also after a failure, a panic or a timeout.
        let cleaned = tokio::time::timeout(limits.cleanup, async {
            let _guard = session.exclusive().await;
            cleanup(
                session.connection().as_ref(),
                quoting.as_ref(),
                table_fetcher.as_ref(),
                &container,
                &refs,
            )
            .await
        })
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "cleanup did not finish within {} s",
                limits.cleanup.as_secs()
            ))
        });
        let elapsed = Some(millis(started.elapsed()));
        let (mut rt, caps_cell) = report.into_parts();
        if let Err(err) = cleaned {
            rt.0 = CellStatus::Fail;
            rt.1 = format!("{} [cleanup: {err}]", rt.1);
        }
        if let Some(n) = note.take() {
            rt.1 = format!("{} [{n}]", rt.1);
        }
        let mut a = outcome(e, kind, Check::Roundtrip, rt.0, rt.1);
        a.elapsed_ms = elapsed;
        let mut b = outcome(e, kind, Check::Capabilities, caps_cell.0, caps_cell.1);
        b.elapsed_ms = elapsed;
        out.push(a);
        out.push(b);
    }
    out
}

/// `DROP TABLE IF EXISTS` for each of `refs` (in order), then checks that none is still listed
/// under `container`.
async fn cleanup(
    conn: &dyn Connection,
    quoting: &dyn Quoting,
    tables: &dyn Fetcher,
    container: &ObjectRef,
    refs: &[ObjectRef],
) -> Result<(), String> {
    let cancel = CancellationToken::new();
    let mut errors = Vec::new();
    for r in refs {
        let sql = format!("DROP TABLE IF EXISTS {}", quoting.qualify(r));
        if let Err(err) = exec(conn, &sql, &[], &cancel).await {
            errors.push(format!("{sql}: {err}"));
        }
    }
    let listed = tables
        .list(conn, container, cancel)
        .await
        .map_err(|err| format!("cannot list the tables after cleanup: {err}"))?;
    let left: Vec<&str> = listed
        .iter()
        .map(|s| s.obj.name.as_str())
        .filter(|n| refs.iter().any(|r| r.name == *n))
        .collect();
    if !left.is_empty() {
        errors.push(format!("still listed after cleanup: {}", left.join(", ")));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Drops every `<prefix>*` table under `container` whose token is older than `stale_after`
/// seconds. Returns how many it dropped.
async fn drop_stale(
    conn: &dyn Connection,
    quoting: &dyn Quoting,
    tables: &dyn Fetcher,
    container: &ObjectRef,
    prefix: &str,
    stale_after: u64,
) -> Result<usize, String> {
    let cancel = CancellationToken::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let listed = tables
        .list(conn, container, cancel.clone())
        .await
        .map_err(|err| format!("cannot list {container}: {err}"))?;
    let mut stale: Vec<&ObjectRef> = listed
        .iter()
        .map(|s| &s.obj)
        .filter(|r| {
            scratch_token(&r.name, prefix)
                .and_then(|t| token_age(t, now))
                .is_some_and(|age| age > stale_after)
        })
        .collect();
    // In reverse name order, referencing tables (`<prefix>foreign_key_*`) come before the
    // tables they reference (`<prefix>fk_*`).
    stale.sort_by(|a, b| b.name.cmp(&a.name));
    let mut dropped = 0;
    let mut errors = Vec::new();
    for r in stale {
        let sql = format!("DROP TABLE IF EXISTS {}", quoting.qualify(r));
        match exec(conn, &sql, &[], &cancel).await {
            Ok(_) => dropped += 1,
            Err(err) => errors.push(format!("{}: {err}", r.name)),
        }
    }
    if errors.is_empty() {
        Ok(dropped)
    } else {
        Err(errors.join("; "))
    }
}

/// Fills the `roundtrip` / `capabilities` cells of one engine from the outcomes of
/// [`run_all`].
pub fn apply(cells: &mut [Cell], outcomes: &[RtOutcome]) {
    for cell in cells
        .iter_mut()
        .filter(|c| matches!(c.check, Check::Roundtrip | Check::Capabilities))
    {
        if let Some(o) = outcomes.iter().find(|o| {
            o.engine == cell.engine && o.kind == cell.object_kind && o.check == cell.check
        }) {
            cell.status = o.status;
            cell.detail = Some(o.detail.clone());
            cell.elapsed_ms = o.elapsed_ms;
        }
    }
}

/// [`gate`] options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GateOptions {
    /// Skipped cells are fine (`OXBASE_CONFORMANCE_ALLOW_MISSING`, or `..._SKIP_ROUNDTRIP`
    /// without `require_m3`; see [`GateOptions::for_run`]).
    pub skipped_ok: bool,
    /// Every `roundtrip` and `capabilities` cell of [`DESIGNER_KINDS`] on every [`M3_ENGINES`]
    /// fixture must pass, every M3 engine must have no `missing_designer_roles`, and the
    /// fixtures must have its `[m3.<engine>]` ([`REQUIRE_ENV`]).
    pub require_m3: bool,
}

impl GateOptions {
    /// The options of a run with `opts`: unreachable fixtures (`allow_missing`) may leave
    /// cells skipped; `OXBASE_CONFORMANCE_SKIP_ROUNDTRIP` may too, but not when `require_m3`
    /// (both set is an error, so the required gate cannot be passed by skipping every cell).
    pub fn for_run(opts: &RunOptions, require_m3: bool) -> Result<Self, String> {
        if require_m3 && opts.skip_roundtrip {
            return Err(format!(
                "{REQUIRE_ENV} and OXBASE_CONFORMANCE_SKIP_ROUNDTRIP are both set: the required \
                 round-trip cells cannot be skipped"
            ));
        }
        Ok(Self {
            skipped_ok: opts.allow_missing || opts.skip_roundtrip,
            require_m3,
        })
    }

    /// `require_m3` from [`REQUIRE_ENV`].
    pub fn require_m3_from_env() -> bool {
        std::env::var(REQUIRE_ENV).is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    }
}

/// Checks the `roundtrip` and `capabilities` cells of a report and returns every problem (empty
/// = acceptable): nothing fails; kinds outside [`DESIGNER_KINDS`] have no runner, so they never
/// pass or fail; with [`GateOptions::require_m3`], see there.
pub fn gate(
    cells: &[Cell],
    fixtures: &Fixtures,
    engines: &[ResolvedEngine],
    registry: &DialectRegistry,
    opts: GateOptions,
) -> Vec<String> {
    let mut problems = Vec::new();
    let mine = cells
        .iter()
        .filter(|c| matches!(c.check, Check::Roundtrip | Check::Capabilities));
    for c in mine {
        let designer = DESIGNER_KINDS.contains(&c.object_kind);
        let bad = match c.status {
            CellStatus::Fail => true,
            CellStatus::Pass => !designer,
            _ => false,
        };
        if bad {
            problems.push(format!(
                "{} {} {}: {} ({})",
                c.engine,
                c.object_kind,
                c.check,
                c.status.label(),
                c.detail.as_deref().unwrap_or("no detail")
            ));
        }
    }
    if !opts.require_m3 {
        return problems;
    }
    for e in engines.iter().filter(|e| M3_ENGINES.contains(&e.engine)) {
        let missing = registry.missing_designer_roles(e.engine);
        if !missing.is_empty() {
            problems.push(format!(
                "{}: missing designer roles in oxbase_services::default_dialects ({REQUIRE_ENV} \
                 is set): {}",
                e.name,
                missing.join(", ")
            ));
        }
        if fixtures.m3_seed(&e.name, e.engine).is_none() {
            problems.push(format!(
                "{}: tests/fixtures.toml has no [m3.{}] ({REQUIRE_ENV} is set)",
                e.name, e.name
            ));
        }
        for kind in DESIGNER_KINDS {
            for check in [Check::Roundtrip, Check::Capabilities] {
                let cell = cells
                    .iter()
                    .find(|c| c.engine == e.name && c.object_kind == kind && c.check == check);
                let ok = cell.is_some_and(|c| {
                    c.status == CellStatus::Pass
                        || (opts.skipped_ok && c.status == CellStatus::Skipped)
                });
                if !ok {
                    problems.push(format!(
                        "{} {kind} {check}: {} ({REQUIRE_ENV} is set: must pass)",
                        e.name,
                        cell.map_or_else(
                            || "no cell".to_owned(),
                            |c| format!(
                                "{} ({})",
                                c.status.label(),
                                c.detail.as_deref().unwrap_or("no detail")
                            )
                        )
                    ));
                }
            }
        }
    }
    problems
}

#[cfg(test)]
mod tests;
