//! The live M8 cells (module docs of [`crate::m8`]): one tokio runtime, the real services over
//! the real drivers, scratch copies of the seed per engine, cells run one after another.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_cast::display::{ArrayFormatter, FormatOptions};
use oxbase_dialect::DialectRegistry;
use oxbase_driver::Connection;
use oxbase_services::backup::{
    BackupContent, BackupEvent, BackupRequest, BackupService, BackupSpec, Compression,
    RestoreEvent, RestoreOptions, RestoreRequest, RestoredStatus,
};
use oxbase_services::sync::{
    DataApplyEvent, DataApplyOptions, DataApplyRequest, DataCompareEvent, DataCompareOptions,
    DataCompareRequest, DataCompareResult, DataScriptOptions, DataSyncService, DiffStatus,
    StructureApplyRequest, StructureCompareRequest, StructureCompareResult, StructureOptions,
    StructureSyncService, TableCompareStatus, TablePair,
};
use oxbase_services::transfer::{
    SourceObject, TableStatus, TransferEvent, TransferOptions, TransferPlan, TransferRequest,
    TransferService, TransferSpec,
};
use oxbase_services::tree::TreeService;
use oxbase_services::{CancellationToken, ProvidedSecrets, SessionHandle};
use oxbase_store::ProfileAddress;
use oxbase_types::{Engine, ObjectKind, ObjectRef};
use tokio::sync::mpsc;

use super::seed::{self, ScratchNames};
use super::{M8Cell, M8Check, M8Options, missing_sync_roles, skeleton};
use crate::connect::{Ctx, millis};
use crate::fixtures::{
    Fixtures, M8EngineSeed, M8Fixture, M8Transfer, ResolvedEngine, in_workspace,
};
use crate::matrix::CellStatus;
use crate::roundtrip::{self, exec};

/// Opening a session, building the scratch copies and checking them, per engine.
const SEED_LIMIT: Duration = Duration::from_secs(600);
/// One sync or backup cell.
const CELL_LIMIT: Duration = Duration::from_secs(600);
/// One transfer cell.
const TRANSFER_LIMIT: Duration = Duration::from_secs(300);
/// Waiting for a run's terminal event (inside a cell).
const RUN_LIMIT: Duration = Duration::from_secs(480);
/// Dropping the scratch containers and closing the sessions.
const CLEANUP_LIMIT: Duration = Duration::from_secs(120);
/// Detail texts are cut to this many characters.
const DETAIL_MAX: usize = 1_500;

/// The rows of the designed objects in a fresh copy: `(table, source rows, target rows)`;
/// `None` = the table is not on that side. `m8_big` takes `[m8] big_rows`.
const SEED_COUNTS: [(&str, Option<u64>, Option<u64>); 13] = [
    ("customers", Some(2), Some(2)),
    ("products", Some(2), Some(2)),
    ("orders", Some(0), Some(0)),
    ("order_audit", Some(0), Some(0)),
    ("order_items", Some(0), None),
    ("order_lines", None, Some(0)),
    ("legacy_notes", None, Some(0)),
    ("MixedCase", Some(0), None),
    ("select", Some(0), Some(0)),
    ("m8_data", Some(20), Some(20)),
    ("m8_big", Some(0), Some(0)),
    ("m8_composite", Some(6), Some(6)),
    ("m8_nokey", Some(2), Some(2)),
];

/// What a cell found.
enum Verdict {
    Pass(String),
    Fail(String),
    Pending(String),
}

type CellResult = Result<String, Verdict>;

fn fail(detail: impl Into<String>) -> Verdict {
    Verdict::Fail(detail.into())
}

/// The services every cell uses, on one connection manager.
struct Services {
    structure: StructureSyncService,
    data: DataSyncService,
    transfer: TransferService,
    backup: BackupService,
}

/// One engine's sessions and scratch containers.
struct Env {
    fixture: ResolvedEngine,
    engine: Engine,
    names: ScratchNames,
    m8: M8Fixture,
    transfer: Option<M8Transfer>,
    /// The sync pair's source / target, the restore target and the transfer target. One
    /// session for all on server engines; one file each on SQLite.
    src: SessionHandle,
    tgt: SessionHandle,
    rst: SessionHandle,
    xfr: SessionHandle,
    c_src: ObjectRef,
    c_tgt: ObjectRef,
    c_rst: ObjectRef,
    c_xfr: ObjectRef,
    /// PostgreSQL: the copy of the transfer set.
    c_xsrc: Option<ObjectRef>,
    /// SQLite files and backup archives; removed at the end.
    dir: PathBuf,
}

impl Env {
    fn sessions(&self) -> Vec<&SessionHandle> {
        let mut out: Vec<&SessionHandle> = Vec::new();
        for s in [&self.src, &self.tgt, &self.rst, &self.xfr] {
            if !out.iter().any(|o| o.id() == s.id()) {
                out.push(s);
            }
        }
        out
    }

    /// `table` of the container named `container` (SQLite: its own file).
    fn q(&self, container: &str, table: &str) -> String {
        seed::qualified(self.engine, container, table)
    }
}

/// Runs every M8 cell of `engines` (see the module docs of [`crate::m8`]).
pub(super) fn run(
    fixtures: &Fixtures,
    engines: &[&ResolvedEngine],
    registry: &DialectRegistry,
    opts: M8Options,
) -> Vec<M8Cell> {
    skeleton::install();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            return super::all_cells(
                engines,
                CellStatus::Fail,
                &format!("cannot start the tokio runtime: {err}"),
            );
        }
    };
    let mut cells = rt.block_on(run_async(fixtures, engines, registry, opts));
    let rank = |name: &str| {
        engines
            .iter()
            .position(|e| e.name == name)
            .unwrap_or(usize::MAX)
    };
    cells.sort_by_key(|c| {
        (
            c.check == M8Check::Transfer,
            rank(&c.engine),
            c.check,
            c.target.as_deref().map_or(0, rank),
        )
    });
    cells
}

async fn run_async(
    fixtures: &Fixtures,
    engines: &[&ResolvedEngine],
    registry: &DialectRegistry,
    opts: M8Options,
) -> Vec<M8Cell> {
    let ctx = Ctx::new(fixtures);
    let dialects = Arc::new(registry.clone());
    let tree = Arc::new(TreeService::new(ctx.mgr.clone(), dialects.clone()));
    let services = Arc::new(Services {
        structure: StructureSyncService::new(ctx.mgr.clone(), tree.clone(), dialects.clone()),
        data: DataSyncService::new(ctx.mgr.clone(), tree.clone(), dialects.clone()),
        transfer: TransferService::new(ctx.mgr.clone(), tree.clone(), dialects.clone()),
        backup: BackupService::new(ctx.mgr.clone(), tree, dialects.clone()),
    });

    let mut cells = Vec::new();
    let mut envs: Vec<Option<Arc<Env>>> = Vec::new();
    for &e in engines {
        let started = Instant::now();
        let prepared = tokio::time::timeout(SEED_LIMIT, prepare(&ctx, fixtures, e))
            .await
            .unwrap_or_else(|_| {
                Err(Prepared::Failed(format!(
                    "did not finish within {} s",
                    SEED_LIMIT.as_secs()
                )))
            });
        let elapsed = Some(millis(started.elapsed()));
        let (status, detail, env) = match prepared {
            Ok((env, detail)) => (CellStatus::Pass, detail, Some(Arc::new(env))),
            Err(Prepared::Unreachable(why)) if opts.allow_missing => (
                CellStatus::Skipped,
                format!("fixture unreachable (allowed missing): {why}"),
                None,
            ),
            Err(Prepared::Unreachable(why)) => (
                CellStatus::Fail,
                format!("fixture unreachable: {why}"),
                None,
            ),
            Err(Prepared::Failed(why)) => (CellStatus::Fail, why, None),
        };
        let mut seed_cell = M8Cell::new(e, M8Check::Seed, status, cut(&detail));
        seed_cell.elapsed_ms = elapsed;
        cells.push(seed_cell);
        if env.is_none() {
            let (status, why) = if status == CellStatus::Skipped {
                (CellStatus::Skipped, "fixture unreachable (allowed missing)")
            } else {
                (CellStatus::Fail, "not run: the seed cell failed")
            };
            for check in [M8Check::SyncStructure, M8Check::SyncData, M8Check::Backup] {
                cells.push(M8Cell::new(e, check, status, why));
            }
        }
        envs.push(env);
    }

    // The transfer matrix reads the sources and writes only the transfer targets.
    for (si, s) in engines.iter().enumerate() {
        for (ti, t) in engines.iter().enumerate() {
            if si == ti {
                continue;
            }
            let cell = match (&envs[si], &envs[ti]) {
                (Some(src), Some(tgt)) => {
                    let fut =
                        transfer_cell(services.clone(), dialects.clone(), src.clone(), tgt.clone());
                    let (status, detail, ms) = guarded(TRANSFER_LIMIT, fut).await;
                    let mut c = M8Cell::transfer(s, t, status, detail);
                    c.elapsed_ms = Some(ms);
                    c
                }
                _ => {
                    let skipped = cells.iter().any(|c| {
                        c.check == M8Check::Seed
                            && (c.engine == s.name || c.engine == t.name)
                            && c.status == CellStatus::Skipped
                    });
                    let status = if skipped {
                        CellStatus::Skipped
                    } else {
                        CellStatus::Fail
                    };
                    M8Cell::transfer(s, t, status, "not run: a seed cell failed or was skipped")
                }
            };
            cells.push(cell);
        }
    }

    // Per engine: the backup reads the source and writes the restore container, the data sync
    // changes the target's rows, the structure sync (last) the target's structure.
    for (e, env) in engines.iter().zip(&envs) {
        let Some(env) = env else { continue };
        let runs: [(M8Check, CellFuture); 3] = [
            (
                M8Check::Backup,
                Box::pin(backup_cell(services.clone(), dialects.clone(), env.clone())),
            ),
            (
                M8Check::SyncData,
                Box::pin(data_cell(services.clone(), env.clone())),
            ),
            (
                M8Check::SyncStructure,
                Box::pin(structure_cell(
                    services.clone(),
                    dialects.clone(),
                    env.clone(),
                )),
            ),
        ];
        for (check, fut) in runs {
            let (status, detail, ms) = guarded(CELL_LIMIT, fut).await;
            let mut c = M8Cell::new(e, check, status, detail);
            c.elapsed_ms = Some(ms);
            cells.push(c);
        }
    }

    for env in envs.iter().flatten() {
        let _ = tokio::time::timeout(CLEANUP_LIMIT, cleanup(&ctx, env, opts.keep_scratch)).await;
    }
    cells
}

type CellFuture = std::pin::Pin<Box<dyn Future<Output = Verdict> + Send>>;

/// Runs one cell on its own task with a time limit. A cell that did not pass while an
/// unfinished M8 body panicked is pending ([`skeleton`]).
async fn guarded<F>(limit: Duration, fut: F) -> (CellStatus, String, u64)
where
    F: Future<Output = Verdict> + Send + 'static,
{
    let mark = skeleton::mark();
    let started = Instant::now();
    let mut task = tokio::spawn(fut);
    let verdict = match tokio::time::timeout(limit, &mut task).await {
        Ok(Ok(v)) => v,
        Ok(Err(err)) if err.is_panic() => fail(format!("panicked: {}", panic_text(err))),
        Ok(Err(err)) => fail(format!("the cell task ended: {err}")),
        Err(_) => {
            task.abort();
            fail(format!("did not finish within {} s", limit.as_secs()))
        }
    };
    let streams = skeleton::since(mark);
    let (status, detail) = match verdict {
        Verdict::Pass(d) => (CellStatus::Pass, d),
        Verdict::Pending(d) => (CellStatus::Pending, d),
        Verdict::Fail(d) if !streams.is_empty() => (
            CellStatus::Pending,
            format!(
                "pending (not implemented yet: m8: {}; {d})",
                streams.join(", m8: ")
            ),
        ),
        Verdict::Fail(d) => (CellStatus::Fail, d),
    };
    (status, cut(&detail), millis(started.elapsed()))
}

fn panic_text(err: tokio::task::JoinError) -> String {
    let payload = err.into_panic();
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "(no message)".to_owned())
}

fn cut(text: &str) -> String {
    if text.chars().count() <= DETAIL_MAX {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(DETAIL_MAX).collect();
    out.push_str(" ...");
    out
}

/// Waits for the terminal event of a run. Gives up early when an unfinished M8 body panicked
/// meanwhile (its task may never send one).
async fn terminal<E>(
    rx: &mut mpsc::Receiver<E>,
    is_terminal: impl Fn(&E) -> bool,
    mark: usize,
) -> Result<E, String> {
    let deadline = Instant::now() + RUN_LIMIT;
    loop {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(event)) if is_terminal(&event) => return Ok(event),
            Ok(Some(_)) => {}
            Ok(None) => return Err("the event stream ended without a terminal event".to_owned()),
            Err(_) => {
                if !skeleton::since(mark).is_empty() {
                    return Err("an unfinished M8 body panicked during the run".to_owned());
                }
                if Instant::now() > deadline {
                    return Err(format!(
                        "no terminal event within {} s",
                        RUN_LIMIT.as_secs()
                    ));
                }
            }
        }
    }
}

// --- seeding -----------------------------------------------------------------------------

enum Prepared {
    /// The session could not be opened.
    Unreachable(String),
    Failed(String),
}

impl From<String> for Prepared {
    fn from(why: String) -> Self {
        Self::Failed(why)
    }
}

/// Opens the sessions, drops stale scratch containers, builds the scratch copies and checks
/// them (the `seed` cell).
async fn prepare(
    ctx: &Ctx,
    fixtures: &Fixtures,
    e: &ResolvedEngine,
) -> Result<(Env, String), Prepared> {
    let (m8, seed_table) = fixtures
        .m8_seed(&e.name, e.engine)
        .ok_or_else(|| format!("tests/fixtures.toml has no [m8.{}]", e.name))?;
    let m8 = m8.clone();
    let prefix = m8
        .scratch_prefix
        .clone()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| seed::DEFAULT_PREFIX.to_owned());
    let stale_after = m8
        .stale_after_secs
        .unwrap_or(seed::DEFAULT_STALE_AFTER_SECS);
    let token = roundtrip::new_token();
    let names = ScratchNames::new(&prefix, &token);
    let dir = std::env::temp_dir().join(format!("oxbase-conformance-m8-{}-{token}", e.name));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    let transfer = m8.transfer();

    let mut notes = Vec::new();
    let env = if e.engine.is_networked() {
        let database = seed_table
            .database
            .clone()
            .or_else(|| {
                fixtures
                    .engines
                    .get(&e.name)
                    .and_then(|f| f.database.clone())
            })
            .unwrap_or_default();
        let base = open(ctx, e, None).await.map_err(Prepared::Unreachable)?;
        let conn = base.connection().as_ref();
        let dropped = drop_stale(conn, e.engine, &prefix, stale_after).await?;
        if dropped > 0 {
            notes.push(format!("dropped {dropped} stale scratch container(s)"));
        }
        let script_path = seed_table
            .init_sql
            .as_deref()
            .map(in_workspace)
            .ok_or_else(|| format!("[m8.{}] has no init_sql", e.name))?;
        let script = std::fs::read_to_string(&script_path)
            .map_err(|err| format!("cannot read {}: {err}", script_path.display()))?;
        let pair = seed::section(&script, "pair").ok_or_else(|| {
            format!(
                "{} has no `-- m8:pair begin/end` section",
                script_path.display()
            )
        })?;
        run_statements(conn, e.engine, &seed::rename(pair, &names.pair_renames()))
            .await
            .map_err(|err| format!("seeding the pair from {}: {err}", script_path.display()))?;
        for name in [&names.restore, &names.transfer_target] {
            run_sql(conn, &seed::create_container(e.engine, name)).await?;
        }
        let c_xsrc = if let Some(xfer) = seed::section(&script, "xfer") {
            run_statements(
                conn,
                e.engine,
                &seed::rename(xfer, &names.transfer_renames()),
            )
            .await
            .map_err(|err| {
                format!(
                    "seeding the transfer set from {}: {err}",
                    script_path.display()
                )
            })?;
            Some(seed::container(e.engine, &database, &names.transfer_source))
        } else {
            None
        };
        let canonical = check_canonical(conn, e.engine, &seed_table, transfer.as_ref()).await?;
        notes.push(canonical);
        let c = |name: &str| seed::container(e.engine, &database, name);
        Env {
            fixture: e.clone(),
            engine: e.engine,
            c_src: c(&names.source),
            c_tgt: c(&names.target),
            c_rst: c(&names.restore),
            c_xfr: c(&names.transfer_target),
            c_xsrc,
            names,
            m8,
            transfer,
            src: base.clone(),
            tgt: base.clone(),
            rst: base.clone(),
            xfr: base,
            dir,
        }
    } else {
        let script = |key: Option<&str>, what: &str| -> Result<String, String> {
            let path = key
                .map(in_workspace)
                .ok_or_else(|| format!("[m8.{}] has no {what}", e.name))?;
            std::fs::read_to_string(&path)
                .map_err(|err| format!("cannot read {}: {err}", path.display()))
        };
        let src_sql = script(seed_table.source_sql.as_deref(), "source_sql")?;
        let tgt_sql = script(seed_table.target_sql.as_deref(), "target_sql")?;
        let src = open(ctx, e, Some(dir.join("src.sqlite")))
            .await
            .map_err(Prepared::Unreachable)?;
        run_statements(src.connection().as_ref(), e.engine, &src_sql)
            .await
            .map_err(|err| format!("seeding the source file: {err}"))?;
        let tgt = open(ctx, e, Some(dir.join("tgt.sqlite"))).await?;
        run_statements(tgt.connection().as_ref(), e.engine, &tgt_sql)
            .await
            .map_err(|err| format!("seeding the target file: {err}"))?;
        let rst = open(ctx, e, Some(dir.join("rst.sqlite"))).await?;
        let xfr = open(ctx, e, Some(dir.join("xfr.sqlite"))).await?;
        notes.push("source, target, restore and transfer files built".to_owned());
        let main = ObjectRef::database("main");
        Env {
            fixture: e.clone(),
            engine: e.engine,
            names,
            m8,
            transfer,
            src,
            tgt,
            rst,
            xfr,
            c_src: main.clone(),
            c_tgt: main.clone(),
            c_rst: main.clone(),
            c_xfr: main,
            c_xsrc: None,
            dir,
        }
    };
    let counts = check_counts(&env).await?;
    notes.insert(0, counts);
    Ok((env, notes.join("; ")))
}

/// Opens a session as the fixture user; SQLite on the file `file`.
async fn open(
    ctx: &Ctx,
    e: &ResolvedEngine,
    file: Option<PathBuf>,
) -> Result<SessionHandle, String> {
    let mut p = ctx.profile(e, false).await?;
    if let Some(path) = file {
        p.address = ProfileAddress::File { path };
    }
    ctx.mgr
        .open(&p, &ProvidedSecrets::none(), CancellationToken::new())
        .await
        .map_err(|err| format!("open failed: {err}"))
}

async fn run_sql(conn: &dyn Connection, sql: &str) -> Result<oxbase_driver::Collected, String> {
    exec::exec(conn, sql, &[], &CancellationToken::new())
        .await
        .map_err(|err| format!("{err} (statement: {})", first_line(sql)))
}

fn first_line(sql: &str) -> String {
    let line = sql.trim().lines().next().unwrap_or_default();
    cut_to(line, 120)
}

fn cut_to(text: &str, n: usize) -> String {
    if text.chars().count() <= n {
        text.to_owned()
    } else {
        format!("{}...", text.chars().take(n).collect::<String>())
    }
}

/// Runs every statement of `script` in order; the error names the failing one.
async fn run_statements(conn: &dyn Connection, engine: Engine, script: &str) -> Result<(), String> {
    let statements = seed::statements(engine, script);
    if statements.is_empty() {
        return Err("the script has no statements".to_owned());
    }
    for (i, sql) in statements.iter().enumerate() {
        run_sql(conn, sql)
            .await
            .map_err(|err| format!("statement {} of {}: {err}", i + 1, statements.len()))?;
    }
    Ok(())
}

/// Every first-column value of `sql` as text.
async fn column(conn: &dyn Connection, sql: &str) -> Result<Vec<String>, String> {
    Ok(rows(conn, sql)
        .await?
        .into_iter()
        .filter_map(|r| r.into_iter().next().flatten())
        .collect())
}

/// Every row of `sql`, every value as display text (`None` = NULL).
async fn rows(conn: &dyn Connection, sql: &str) -> Result<Vec<Vec<Option<String>>>, String> {
    let out = run_sql(conn, sql).await?;
    let options = FormatOptions::default();
    let mut rows = Vec::new();
    for batch in out.result_sets.iter().flat_map(|r| r.batches.iter()) {
        let formatters = batch
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c.as_ref(), &options))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| format!("cannot format the result of `{}`: {err}", first_line(sql)))?;
        for row in 0..batch.num_rows() {
            rows.push(
                batch
                    .columns()
                    .iter()
                    .zip(&formatters)
                    .map(|(c, f)| (!c.is_null(row)).then(|| f.value(row).to_string()))
                    .collect(),
            );
        }
    }
    Ok(rows)
}

async fn count(conn: &dyn Connection, table: &str) -> Result<u64, String> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let values = column(conn, &sql).await?;
    values
        .first()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .ok_or_else(|| format!("`{sql}` returned no count"))
}

/// Drops the scratch containers older than `stale_after` seconds; returns how many.
async fn drop_stale(
    conn: &dyn Connection,
    engine: Engine,
    prefix: &str,
    stale_after: u64,
) -> Result<usize, String> {
    let Some(list) = seed::list_containers(engine, prefix) else {
        return Ok(0);
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let stale: Vec<String> = column(conn, &list)
        .await?
        .into_iter()
        .filter(|n| seed::is_stale(n, prefix, now, stale_after))
        .collect();
    let refs: Vec<&str> = stale.iter().map(String::as_str).collect();
    for sql in seed::drop_containers(engine, &refs) {
        run_sql(conn, &sql).await?;
    }
    Ok(stale.len())
}

/// The canonical seed of the Docker init exists: its source data table holds 20 rows (and,
/// on PostgreSQL, the transfer set its rows).
async fn check_canonical(
    conn: &dyn Connection,
    engine: Engine,
    seed_table: &M8EngineSeed,
    transfer: Option<&M8Transfer>,
) -> Result<String, String> {
    let source = seed_table
        .source_schema
        .as_deref()
        .or(seed_table.source_database.as_deref())
        .unwrap_or(seed::CANONICAL_SOURCE);
    let n = count(conn, &seed::qualified(engine, source, "m8_data"))
        .await
        .map_err(|err| format!("canonical seed (Docker init) missing: {err}"))?;
    if n != 20 {
        return Err(format!(
            "canonical {source}.m8_data has {n} rows, expected 20"
        ));
    }
    let mut out = format!("canonical {source} seeded");
    if let (Some(schema), Some(t)) = (seed_table.transfer_schema.as_deref(), transfer)
        && let Some(types) = t.types_table.as_deref()
    {
        let n = count(conn, &seed::qualified(engine, schema, types)).await?;
        if Some(n) != t.types_rows {
            return Err(format!(
                "canonical {schema}.{types} has {n} rows, expected {:?}",
                t.types_rows
            ));
        }
        out.push_str(&format!(", {schema}.{types} {n} rows"));
    }
    Ok(out)
}

/// The scratch copies hold the designed tables with the designed row counts.
async fn check_counts(env: &Env) -> Result<String, String> {
    let big = env.m8.big_rows.unwrap_or(100_000);
    let mut problems = Vec::new();
    let mut checked = 0;
    for (table, src, tgt) in SEED_COUNTS {
        for (want, session, container) in [
            (src, &env.src, &env.names.source),
            (tgt, &env.tgt, &env.names.target),
        ] {
            let Some(mut want) = want else { continue };
            if table == "m8_big" {
                want = big;
            }
            match count(session.connection().as_ref(), &env.q(container, table)).await {
                Ok(n) if n == want => checked += 1,
                Ok(n) => problems.push(format!("{container}.{table}: {n} rows, expected {want}")),
                Err(err) => problems.push(err),
            }
        }
    }
    if problems.is_empty() {
        Ok(format!("{checked} scratch tables hold the designed rows"))
    } else {
        Err(format!("scratch seed: {}", problems.join("; ")))
    }
}

async fn cleanup(ctx: &Ctx, env: &Env, keep: bool) {
    if !keep && env.engine.is_networked() {
        let conn = env.src.connection().as_ref();
        for sql in seed::drop_containers(env.engine, &env.names.all()) {
            if let Err(err) = run_sql(conn, &sql).await {
                eprintln!(
                    "[m8 conformance] {}: cleanup failed: {err}",
                    env.fixture.name
                );
            }
        }
    }
    for s in env.sessions() {
        let _ = ctx.mgr.close(s.id()).await;
    }
    if !keep {
        let _ = std::fs::remove_dir_all(&env.dir);
    }
}

// --- structure sync ----------------------------------------------------------------------

async fn compare_structure(
    services: &Services,
    request: &StructureCompareRequest,
) -> Result<StructureCompareResult, Verdict> {
    services
        .structure
        .compare(request, CancellationToken::new())
        .await
        .map_err(|err| fail(format!("structure compare failed: {err}")))
}

/// The ids of the items that are not `same` (or only in the target when drops are off).
fn differing(result: &StructureCompareResult, include_drops: bool) -> Vec<String> {
    result
        .diff
        .items
        .iter()
        .filter(|i| match i.status {
            DiffStatus::Same => false,
            DiffStatus::OnlyInTarget => include_drops,
            DiffStatus::OnlyInSource | DiffStatus::Different => true,
        })
        .map(|i| format!("{} ({:?})", i.id, i.status))
        .collect()
}

async fn structure_cell(
    services: Arc<Services>,
    dialects: Arc<DialectRegistry>,
    env: Arc<Env>,
) -> Verdict {
    let missing = missing_sync_roles(&dialects, env.engine);
    if !missing.is_empty() {
        return Verdict::Pending(format!(
            "pending (structure-sync roles missing: {})",
            missing.join(", ")
        ));
    }
    match structure_run(&services, &env).await {
        Ok(d) => Verdict::Pass(d),
        Err(v) => v,
    }
}

async fn structure_run(services: &Services, env: &Env) -> CellResult {
    let request = StructureCompareRequest {
        source_session: env.src.id(),
        source: env.c_src.clone(),
        target_session: env.tgt.id(),
        target: env.c_tgt.clone(),
        options: StructureOptions {
            include_drops: true,
            ..StructureOptions::default()
        },
    };
    let first = compare_structure(services, &request).await?;
    let seed_table = env
        .m8
        .engine(&env.fixture.name)
        .or_else(|| env.m8.engine(env.engine.as_str()))
        .unwrap_or_default();
    let expected = seed::expected_items(
        &env.m8.structure_items,
        &seed_table.skip_items,
        env.m8.function.as_deref(),
        seed_table.function_signature.as_deref(),
    );
    let not_reported: Vec<String> = expected
        .iter()
        .filter(|id| {
            first
                .diff
                .item(id.as_str())
                .is_none_or(|i| i.status == DiffStatus::Same)
        })
        .cloned()
        .collect();
    if !not_reported.is_empty() {
        let seen: Vec<String> = differing(&first, true);
        return Err(fail(format!(
            "the compare does not report these designed differences: {}; it reported: {}",
            not_reported.join(", "),
            seen.join(", ")
        )));
    }
    let to_apply = differing(&first, true);
    let report = services
        .structure
        .apply(
            &StructureApplyRequest {
                compare_id: first.compare_id,
                selection: Vec::new(),
                confirm_destructive: true,
            },
            CancellationToken::new(),
        )
        .await
        .map_err(|err| fail(format!("structure apply failed: {err}")))?;
    let _ = services.structure.forget(first.compare_id);
    if !report.outcome.committed {
        let errors: Vec<String> = report
            .outcome
            .steps
            .iter()
            .filter_map(|s| {
                s.error
                    .as_ref()
                    .map(|e| format!("step {}: {}", s.index, e.message))
            })
            .collect();
        return Err(fail(format!(
            "the sync script did not commit: {}\n--- script ---\n{}",
            errors.join("; "),
            report.script.sql
        )));
    }
    let _ = services.structure.forget(report.recompare.compare_id);
    if !report.in_sync {
        return Err(fail(format!(
            "the apply's re-compare is not in sync: {}",
            differing(&report.recompare, true).join(", ")
        )));
    }
    let again = compare_structure(services, &request).await?;
    let _ = services.structure.forget(again.compare_id);
    if !again.diff.is_in_sync(true) {
        return Err(fail(format!(
            "a fresh compare after the apply still differs: {}",
            differing(&again, true).join(", ")
        )));
    }
    Ok(format!(
        "{} designed items reported; applied {} items ({} script statements, destructive: {}, \
         atomic: {}); re-compare and a fresh compare in sync ({} items same)",
        expected.len(),
        to_apply.len(),
        report.script.script.steps.len(),
        report.script.destructive,
        report.script.atomic,
        again.diff.counts().same
    ))
}

// --- data sync ---------------------------------------------------------------------------

fn pairs(from: &ObjectRef, to: &ObjectRef, tables: &[String]) -> Vec<TablePair> {
    tables
        .iter()
        .map(|t| TablePair {
            source: from.child(ObjectKind::Table, t.as_str()),
            target: to.child(ObjectKind::Table, t.as_str()),
            key: None,
            columns: None,
        })
        .collect()
}

/// Runs a data compare to its end.
async fn compare_data(
    services: &Services,
    source: &SessionHandle,
    target: &SessionHandle,
    pairs: Vec<TablePair>,
) -> Result<DataCompareResult, Verdict> {
    let mark = skeleton::mark();
    let mut handle = services
        .data
        .start_compare(DataCompareRequest {
            source_session: source.id(),
            target_session: target.id(),
            pairs,
            options: DataCompareOptions::default(),
        })
        .await
        .map_err(|err| fail(format!("data compare refused: {err}")))?;
    match terminal(&mut handle.events, DataCompareEvent::is_terminal, mark).await {
        Ok(DataCompareEvent::Finished { result }) => Ok(result),
        Ok(DataCompareEvent::Failed { error, .. }) => {
            Err(fail(format!("data compare failed: {}", error.message)))
        }
        Ok(other) => Err(fail(format!("data compare ended with {other:?}"))),
        Err(err) => Err(fail(format!("data compare: {err}"))),
    }
}

/// The keyed data tables (those with expected counts) in `[m8] data_tables` order.
fn keyed_tables(m8: &M8Fixture) -> Vec<String> {
    m8.data_tables
        .iter()
        .filter(|t| m8.data_expected.contains_key(t.as_str()))
        .cloned()
        .collect()
}

/// Every compared table is in sync; `Err` lists the others.
fn all_in_sync(result: &DataCompareResult, tables: &[String]) -> Result<(), String> {
    let bad: Vec<String> = result
        .tables
        .iter()
        .filter(|t| t.status != TableCompareStatus::Compared || !t.counts.in_sync())
        .map(|t| {
            format!(
                "{}: {:?} {}/{}/{}",
                tables.get(t.pair).map_or("?", String::as_str),
                t.status,
                t.counts.inserts,
                t.counts.updates,
                t.counts.deletes
            )
        })
        .collect();
    if bad.is_empty() && result.tables.len() == tables.len() {
        Ok(())
    } else {
        Err(bad.join("; "))
    }
}

async fn data_cell(services: Arc<Services>, env: Arc<Env>) -> Verdict {
    match data_run(&services, &env).await {
        Ok(d) => Verdict::Pass(d),
        Err(v) => v,
    }
}

async fn data_run(services: &Services, env: &Env) -> CellResult {
    let tables = env.m8.data_tables.clone();
    if tables.is_empty() {
        return Err(Verdict::Pending(
            "pending ([m8] data_tables is empty)".to_owned(),
        ));
    }
    let result = compare_data(
        services,
        &env.src,
        &env.tgt,
        pairs(&env.c_src, &env.c_tgt, &tables),
    )
    .await?;
    let mut problems = Vec::new();
    let mut seen = Vec::new();
    for t in &result.tables {
        let name = tables.get(t.pair).map_or("?", String::as_str);
        match env.m8.data_expected.get(name) {
            Some(want) => {
                let got = [t.counts.inserts, t.counts.updates, t.counts.deletes];
                if t.status != TableCompareStatus::Compared || want.as_slice() != got.as_slice() {
                    problems.push(format!(
                        "{name}: {:?} {}/{}/{}, expected {:?}",
                        t.status, got[0], got[1], got[2], want
                    ));
                } else {
                    seen.push(format!(
                        "{name} {}/{}/{} ({} chunks skipped)",
                        got[0], got[1], got[2], t.chunks_skipped
                    ));
                }
            }
            None => match &t.status {
                TableCompareStatus::Refused { reason } => {
                    seen.push(format!("{name} refused ({reason})"));
                }
                other => problems.push(format!("{name}: {other:?}, expected refused (no key)")),
            },
        }
    }
    if result.tables.len() != tables.len() {
        problems.push(format!(
            "{} table results for {} pairs",
            result.tables.len(),
            tables.len()
        ));
    }
    if !problems.is_empty() {
        let _ = services.data.forget(result.compare_id);
        return Err(fail(format!("data compare: {}", problems.join("; "))));
    }

    let mark = skeleton::mark();
    let mut handle = services
        .data
        .start_apply(DataApplyRequest {
            compare_id: result.compare_id,
            options: DataApplyOptions {
                script: DataScriptOptions {
                    deletes: true,
                    ..DataScriptOptions::default()
                },
                confirm_deletes: true,
                ..DataApplyOptions::default()
            },
        })
        .await
        .map_err(|err| fail(format!("data apply refused: {err}")))?;
    let report = match terminal(&mut handle.events, DataApplyEvent::is_terminal, mark).await {
        Ok(DataApplyEvent::Finished { report }) => report,
        Ok(DataApplyEvent::Failed { error, report }) => {
            return Err(fail(format!(
                "data apply failed: {} (left behind: {})",
                error.message, report.left_behind
            )));
        }
        Ok(other) => return Err(fail(format!("data apply ended with {other:?}"))),
        Err(err) => return Err(fail(format!("data apply: {err}"))),
    };
    let _ = services.data.forget(result.compare_id);
    if let Some(r) = &report.recompare {
        let _ = services.data.forget(r.compare_id);
    }
    if !report.in_sync {
        return Err(fail(format!(
            "the apply's re-compare is not in sync ({} statements, table errors: {:?})",
            report.statements,
            report
                .tables
                .iter()
                .filter_map(|t| t.error.clone())
                .collect::<Vec<_>>()
        )));
    }
    let keyed = keyed_tables(&env.m8);
    let again = compare_data(
        services,
        &env.src,
        &env.tgt,
        pairs(&env.c_src, &env.c_tgt, &keyed),
    )
    .await?;
    let _ = services.data.forget(again.compare_id);
    all_in_sync(&again, &keyed).map_err(|why| {
        fail(format!(
            "a fresh compare after the apply still differs: {why}"
        ))
    })?;
    Ok(format!(
        "{}; applied {} statements in {} batches; re-compare and a fresh compare in sync",
        seen.join(", "),
        report.statements,
        report.committed_batches
    ))
}

// --- backup / restore --------------------------------------------------------------------

async fn backup_cell(
    services: Arc<Services>,
    dialects: Arc<DialectRegistry>,
    env: Arc<Env>,
) -> Verdict {
    let path = env.dir.join(format!("{}.oxbackup", env.names.token));
    let out = backup_run(&services, &dialects, &env, &path).await;
    let _ = std::fs::remove_file(&path);
    match out {
        Ok(d) => Verdict::Pass(d),
        Err(v) => v,
    }
}

async fn backup_run(
    services: &Services,
    dialects: &DialectRegistry,
    env: &Env,
    path: &std::path::Path,
) -> CellResult {
    let mark = skeleton::mark();
    let mut handle = services
        .backup
        .start_backup(
            BackupRequest {
                session: env.src.id(),
                spec: BackupSpec {
                    container: env.c_src.clone(),
                    objects: Vec::new(),
                    content: BackupContent::StructureAndData,
                    compression: Compression::Zstd,
                },
            },
            path,
            true,
        )
        .await
        .map_err(|err| fail(format!("backup refused: {err}")))?;
    let backup = match terminal(&mut handle.events, BackupEvent::is_terminal, mark).await {
        Ok(BackupEvent::Finished { report }) => report,
        Ok(BackupEvent::Failed { error, .. }) => {
            return Err(fail(format!("backup failed: {}", error.message)));
        }
        Ok(other) => return Err(fail(format!("backup ended with {other:?}"))),
        Err(err) => return Err(fail(format!("backup: {err}"))),
    };
    if !path.is_file() {
        return Err(fail(format!(
            "the backup reported success but {} is missing",
            path.display()
        )));
    }
    let manifest = services
        .backup
        .list(path, CancellationToken::new())
        .await
        .map_err(|err| fail(format!("list failed: {err}")))?;
    if manifest.objects.len() != backup.manifest.objects.len() {
        return Err(fail(format!(
            "list shows {} objects, the backup wrote {}",
            manifest.objects.len(),
            backup.manifest.objects.len()
        )));
    }
    let verify = services
        .backup
        .verify(path, CancellationToken::new())
        .await
        .map_err(|err| fail(format!("verify failed: {err}")))?;
    if !verify.ok {
        return Err(fail(format!(
            "verify found problems: {:?}",
            verify.problems
        )));
    }

    let mark = skeleton::mark();
    let mut handle = services
        .backup
        .start_restore(
            RestoreRequest {
                session: env.rst.id(),
                options: RestoreOptions::into_container(env.c_rst.clone()),
            },
            path,
        )
        .await
        .map_err(|err| fail(format!("restore refused: {err}")))?;
    let restore = match terminal(&mut handle.events, RestoreEvent::is_terminal, mark).await {
        Ok(RestoreEvent::Finished { report }) => report,
        Ok(RestoreEvent::Failed { error, report }) => {
            return Err(fail(format!(
                "restore failed: {} (left behind: {})",
                error.message, report.left_behind
            )));
        }
        Ok(other) => return Err(fail(format!("restore ended with {other:?}"))),
        Err(err) => return Err(fail(format!("restore: {err}"))),
    };
    let failed: Vec<String> = restore
        .objects
        .iter()
        .filter(|o| matches!(o.status, RestoredStatus::Failed | RestoredStatus::NotRun))
        .map(|o| {
            format!(
                "{} {:?}: {}",
                o.object.display_name(),
                o.status,
                o.error.as_deref().unwrap_or("")
            )
        })
        .collect();
    if !failed.is_empty() {
        return Err(fail(format!(
            "restore left objects out: {}",
            failed.join("; ")
        )));
    }

    // Row counts first: they need no other stream.
    let mut problems = Vec::new();
    for table in env
        .m8
        .data_tables
        .iter()
        .map(String::as_str)
        .chain(["customers", "products"])
    {
        let src = count(
            env.src.connection().as_ref(),
            &env.q(&env.names.source, table),
        )
        .await;
        let rst = count(
            env.rst.connection().as_ref(),
            &env.q(&env.names.restore, table),
        )
        .await;
        match (src, rst) {
            (Ok(a), Ok(b)) if a == b => {}
            (a, b) => problems.push(format!("{table}: source {a:?}, restored {b:?}")),
        }
    }
    if !problems.is_empty() {
        return Err(fail(format!(
            "restored rows differ: {}",
            problems.join("; ")
        )));
    }

    let missing = missing_sync_roles(dialects, env.engine);
    if !missing.is_empty() {
        return Err(Verdict::Pending(format!(
            "pending (backup, list, verify and restore ran, row counts equal; the structure \
             compare needs the structure-sync roles: {})",
            missing.join(", ")
        )));
    }
    let request = StructureCompareRequest {
        source_session: env.src.id(),
        source: env.c_src.clone(),
        target_session: env.rst.id(),
        target: env.c_rst.clone(),
        options: StructureOptions {
            include_drops: true,
            ..StructureOptions::default()
        },
    };
    let compared = compare_structure(services, &request).await?;
    let _ = services.structure.forget(compared.compare_id);
    if !compared.diff.is_in_sync(true) {
        return Err(fail(format!(
            "structure compare source / restore differs: {}",
            differing(&compared, true).join(", ")
        )));
    }
    let keyed = keyed_tables(&env.m8);
    let data = compare_data(
        services,
        &env.src,
        &env.rst,
        pairs(&env.c_src, &env.c_rst, &keyed),
    )
    .await?;
    let _ = services.data.forget(data.compare_id);
    all_in_sync(&data, &keyed)
        .map_err(|why| fail(format!("data compare source / restore differs: {why}")))?;
    Ok(format!(
        "{} objects, {} bytes; verify ok ({} entries); restored {} rows; structure and data \
         compares source / restore in sync",
        backup.manifest.objects.len(),
        backup.bytes,
        verify.entries_checked,
        restore.rows
    ))
}

// --- transfer ----------------------------------------------------------------------------

/// `column: fidelity (reason)` of every non-exact, not skipped column of the plan.
fn lossiness(plan: &TransferPlan) -> String {
    let lossy: Vec<String> = plan
        .tables
        .iter()
        .flat_map(|t| t.columns.iter())
        .filter(|c| !c.skipped && fidelity(&c.mapping.fidelity) != "exact")
        .map(|c| {
            format!(
                "{} {} -> {}: {} ({})",
                c.source,
                c.source_native,
                c.mapping.native_type,
                fidelity(&c.mapping.fidelity),
                c.mapping.reason.as_deref().unwrap_or("no reason given")
            )
        })
        .collect();
    if lossy.is_empty() {
        "every column exact".to_owned()
    } else {
        format!("lossy: {}", lossy.join("; "))
    }
}

fn fidelity(f: &impl std::fmt::Debug) -> String {
    format!("{f:?}").to_ascii_lowercase()
}

async fn run_transfer(services: &Services, request: TransferRequest) -> Result<(), Verdict> {
    let mark = skeleton::mark();
    let mut handle = services
        .transfer
        .start(request)
        .await
        .map_err(|err| fail(format!("transfer refused: {err}")))?;
    let report = match terminal(&mut handle.events, TransferEvent::is_terminal, mark).await {
        Ok(TransferEvent::Finished { report }) => report,
        Ok(TransferEvent::Failed { error, report }) => {
            let tables: Vec<String> = report
                .tables
                .iter()
                .filter_map(|t| {
                    t.error
                        .as_ref()
                        .map(|e| format!("table {}: {e} ({})", t.table, t.left_behind))
                })
                .collect();
            return Err(fail(format!(
                "transfer failed: {}; {}",
                error.message,
                tables.join("; ")
            )));
        }
        Ok(other) => return Err(fail(format!("transfer ended with {other:?}"))),
        Err(err) => return Err(fail(format!("transfer: {err}"))),
    };
    let bad: Vec<String> = report
        .tables
        .iter()
        .filter(|t| !matches!(t.status, TableStatus::Done) || !t.issues.is_empty())
        .map(|t| {
            format!(
                "table {} {:?} ({} issues): {}",
                t.table,
                t.status,
                t.issues.len(),
                t.error.as_deref().unwrap_or("")
            )
        })
        .collect();
    if bad.is_empty() {
        Ok(())
    } else {
        Err(fail(format!(
            "transfer finished with problems: {}",
            bad.join("; ")
        )))
    }
}

async fn transfer_cell(
    services: Arc<Services>,
    dialects: Arc<DialectRegistry>,
    src: Arc<Env>,
    tgt: Arc<Env>,
) -> Verdict {
    let no_mapper: Vec<&str> = [src.engine, tgt.engine]
        .into_iter()
        .filter(|&e| !dialects.has_type_mapper(e))
        .map(Engine::as_str)
        .collect();
    if !no_mapper.is_empty() {
        return Verdict::Pending(format!(
            "pending (no TypeMapper registered for {})",
            no_mapper.join(", ")
        ));
    }
    match transfer_run(&services, &src, &tgt).await {
        Ok(d) => Verdict::Pass(d),
        Err(v) => v,
    }
}

/// The exit-check pair: PostgreSQL → MySQL.
fn is_required(src: Engine, tgt: Engine) -> bool {
    src == Engine::Postgres && tgt == Engine::MySql
}

async fn transfer_run(services: &Services, src: &Env, tgt: &Env) -> CellResult {
    let required = is_required(src.engine, tgt.engine);
    let target_name = format!("m8_data_{}", src.engine.as_str());
    let request = TransferRequest {
        source_session: src.src.id(),
        target_session: tgt.xfr.id(),
        spec: TransferSpec {
            objects: vec![SourceObject::Table {
                table: src.c_src.child(ObjectKind::Table, "m8_data"),
                target_name: Some(target_name.clone()),
                filter: None,
            }],
            target: tgt.c_xfr.clone(),
            options: TransferOptions::default(),
            overrides: Vec::new(),
        },
        confirm_destructive: false,
    };
    let plan = services
        .transfer
        .plan(&request, CancellationToken::new())
        .await
        .map_err(|err| fail(format!("plan failed: {err}")))?;
    let lossy = lossiness(&plan);
    if !plan.runnable {
        let precise = !plan.blocked.is_empty() && plan.blocked.iter().all(|b| !b.trim().is_empty());
        let why = format!("plan not runnable: {}; {lossy}", plan.blocked.join("; "));
        return if required || !precise {
            Err(fail(why))
        } else {
            Ok(why)
        };
    }
    run_transfer(services, request).await?;
    let select = |env: &Env, container: &str, table: &str| {
        format!(
            "SELECT {}, {}, {} FROM {} ORDER BY {}",
            seed::quote(env.engine, "id"),
            seed::quote(env.engine, "name"),
            seed::quote(env.engine, "note"),
            env.q(container, table),
            seed::quote(env.engine, "id"),
        )
    };
    let want = rows(
        src.src.connection().as_ref(),
        &select(src, &src.names.source, "m8_data"),
    )
    .await
    .map_err(fail)?;
    let got = rows(
        tgt.xfr.connection().as_ref(),
        &select(tgt, &tgt.names.transfer_target, &target_name),
    )
    .await
    .map_err(fail)?;
    if want != got {
        let first = want
            .iter()
            .zip(&got)
            .find(|(a, b)| a != b)
            .map(|(a, b)| format!("first difference: source {a:?}, target {b:?}"))
            .unwrap_or_default();
        return Err(fail(format!(
            "read back differs: source {} rows, target {} rows; {first}",
            want.len(),
            got.len()
        )));
    }
    let mut detail = format!(
        "{} rows read back equal (id, name, note); {lossy}",
        want.len()
    );
    if required {
        let types = transfer_types(services, src, tgt).await?;
        detail.push_str("; ");
        detail.push_str(&types);
    }
    Ok(detail)
}

/// PostgreSQL → MySQL of the `[m8.transfer]` set (tables listed child first): plan fidelities
/// and `COLUMN_TYPE`s as `[m8.transfer.expected_mysql]`, rows, load order and foreign keys.
async fn transfer_types(services: &Services, src: &Env, tgt: &Env) -> Result<String, Verdict> {
    let Some(set) = src.transfer.clone() else {
        return Err(fail("tests/fixtures.toml has no [m8.transfer]"));
    };
    let Some(container) = &src.c_xsrc else {
        return Err(fail("the PostgreSQL seed has no `-- m8:xfer` section"));
    };
    let types = set
        .types_table
        .clone()
        .unwrap_or_else(|| "m8_types".to_owned());
    // Listed child first, so the plan has to reorder them.
    let listed: Vec<String> = set.tables.iter().rev().cloned().collect();
    let request = TransferRequest {
        source_session: src.src.id(),
        target_session: tgt.xfr.id(),
        spec: TransferSpec {
            objects: listed
                .iter()
                .map(|t| SourceObject::Table {
                    table: container.child(ObjectKind::Table, t.as_str()),
                    target_name: None,
                    filter: None,
                })
                .collect(),
            target: tgt.c_xfr.clone(),
            options: TransferOptions::default(),
            overrides: Vec::new(),
        },
        confirm_destructive: false,
    };
    let plan = services
        .transfer
        .plan(&request, CancellationToken::new())
        .await
        .map_err(|err| fail(format!("[m8.transfer] plan failed: {err}")))?;
    if !plan.runnable {
        return Err(fail(format!(
            "[m8.transfer] plan not runnable: {}",
            plan.blocked.join("; ")
        )));
    }
    let position = |name: &str| {
        listed
            .iter()
            .position(|t| t == name)
            .and_then(|i| plan.order.iter().position(|&o| o == i))
    };
    if let (Some(parent), Some(child)) = (position("m8_parent"), position("m8_child"))
        && parent > child
    {
        return Err(fail("[m8.transfer] plan loads m8_child before m8_parent"));
    }
    let mut problems = Vec::new();
    let types_index = listed.iter().position(|t| *t == types);
    let planned: BTreeMap<&str, String> = types_index
        .and_then(|i| plan.tables.get(i))
        .map(|t| {
            t.columns
                .iter()
                .map(|c| (c.source.as_str(), fidelity(&c.mapping.fidelity)))
                .collect()
        })
        .unwrap_or_default();
    for (column, want) in &set.expected_mysql {
        let want_fidelity = want.get(1).map(String::as_str).unwrap_or_default();
        match planned.get(column.as_str()) {
            Some(f) if f == want_fidelity => {}
            other => problems.push(format!(
                "plan {column}: {other:?}, expected {want_fidelity}"
            )),
        }
    }
    run_transfer(services, request).await?;
    let conn = tgt.xfr.connection().as_ref();
    let schema = tgt.names.transfer_target.replace('\'', "''");
    let column_types: BTreeMap<String, String> = rows(
        conn,
        &format!(
            "SELECT CAST(COLUMN_NAME AS CHAR), CAST(COLUMN_TYPE AS CHAR) \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = '{schema}' AND TABLE_NAME = '{}'",
            types.replace('\'', "''")
        ),
    )
    .await
    .map_err(fail)?
    .into_iter()
    .filter_map(|r| match r.as_slice() {
        [Some(name), Some(ty)] => Some((name.clone(), ty.clone())),
        _ => None,
    })
    .collect();
    for (column, want) in &set.expected_mysql {
        let want_type = want.first().map(String::as_str).unwrap_or_default();
        match column_types.get(column) {
            Some(ty) if ty.eq_ignore_ascii_case(want_type) => {}
            other => problems.push(format!(
                "COLUMN_TYPE {column}: {other:?}, expected {want_type}"
            )),
        }
    }
    let n = count(conn, &tgt.q(&tgt.names.transfer_target, &types))
        .await
        .map_err(fail)?;
    if Some(n) != set.types_rows {
        problems.push(format!("{types}: {n} rows, expected {:?}", set.types_rows));
    }
    let fks = column(
        conn,
        &format!(
            "SELECT CAST(CONSTRAINT_NAME AS CHAR) FROM information_schema.REFERENTIAL_CONSTRAINTS \
             WHERE CONSTRAINT_SCHEMA = '{schema}' AND TABLE_NAME = 'm8_child' \
             AND REFERENCED_TABLE_NAME = 'm8_parent'"
        ),
    )
    .await
    .map_err(fail)?;
    if fks.is_empty() {
        problems.push("no foreign key m8_child -> m8_parent after the load".to_owned());
    }
    if problems.is_empty() {
        Ok(format!(
            "[m8.transfer] {} tables: {} columns with the expected MySQL types and fidelities, \
             {n} rows, m8_parent loaded before m8_child, foreign key present",
            listed.len(),
            set.expected_mysql.len()
        ))
    } else {
        Err(fail(format!("[m8.transfer]: {}", problems.join("; "))))
    }
}
