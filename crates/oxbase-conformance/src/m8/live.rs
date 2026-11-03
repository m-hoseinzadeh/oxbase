//! The live M8 cells (module docs of [`crate::m8`]): one tokio runtime, the real services over
//! the real drivers, scratch copies of the seed per engine, cells run one after another.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_cast::display::{ArrayFormatter, FormatOptions};
use oxbase_dialect::{DialectRegistry, Fidelity};
use oxbase_driver::{Connection, cell_value};
use oxbase_services::backup::{
    BackupContent, BackupEvent, BackupRequest, BackupService, BackupSpec, Compression,
    RestoreEvent, RestoreOptions, RestoreRequest, RestoredStatus,
};
use oxbase_services::sync::{
    DataApplyEvent, DataApplyOptions, DataApplyRequest, DataCompareEvent, DataCompareOptions,
    DataCompareRequest, DataCompareResult, DataScriptOptions, DataScriptRequest, DataSyncService,
    DiffStatus, StructureApplyRequest, StructureCompareRequest, StructureCompareResult,
    StructureOptions, StructureScriptRequest, StructureSyncService, TableCompareStatus, TablePair,
};
use oxbase_services::transfer::{
    SourceObject, TableStatus, TransferEvent, TransferOptions, TransferPlan, TransferRequest,
    TransferService, TransferSpec,
};
use oxbase_services::tree::TreeService;
use oxbase_services::{CancellationToken, ProvidedSecrets, SessionHandle, SessionId};
use oxbase_store::ProfileAddress;
use oxbase_types::{Engine, ObjectKind, ObjectRef, Value};
use tokio::sync::mpsc;

use super::seed::{self, ScratchNames};
use super::values::{self, Kind};
use super::{M8Cell, M8Check, M8Options, missing_sync_roles, skeleton};
use crate::connect::{Ctx, millis};
use crate::fixtures::{
    Fixtures, M8EngineSeed, M8Fixture, M8Transfer, ResolvedEngine, in_workspace,
};
use crate::matrix::CellStatus;
use crate::probe::Target;
use crate::roundtrip::{self, exec};

/// Opening the sessions, building the scratch copies and checking them, per engine.
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
/// Bytes inverted in the damaged copy of a backup archive (in its middle, inside table data).
const DAMAGED_BYTES: usize = 16;

/// The rows of the designed objects in a fresh copy: `(table, source rows, target rows)`;
/// `None` = the table is not on that side. `m8_big` takes `[m8] big_rows`.
const SEED_COUNTS: [(&str, Option<u64>, Option<u64>); 13] = [
    ("customers", Some(2), Some(2)),
    ("products", Some(2), Some(2)),
    ("orders", Some(2), Some(2)),
    ("order_audit", Some(2), Some(2)),
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

/// The designed data differences, checked through sums of a column: `(table, column, source
/// sum, target sum)` (`m8_data` ids 1..20 vs 1..18 + 21, 22; `m8_big` without 50000 and with
/// 100001 for `big_rows` = 100 000; `m8_composite` qty of (2, 'b') 4 vs 40).
const SEED_SUMS: [(&str, &str, u64, u64); 3] = [
    ("m8_data", "id", 210, 214),
    ("m8_big", "id", 5_000_050_000, 5_000_100_001),
    ("m8_composite", "qty", 21, 57),
];

/// The tables whose rows the restore and the structure sync must keep, besides `[m8]
/// data_tables`.
const KEPT_TABLES: [&str; 4] = ["customers", "products", "orders", "order_audit"];

/// What a cell found.
enum Verdict {
    Pass(String),
    Fail(String),
    Pending(String),
    /// Not applicable to this pair, for a precise reason (transfer cells only).
    NotApplicable(String),
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

impl Services {
    /// Cancels every run of every service on `sessions` (a cell that timed out).
    fn cancel_sessions(&self, sessions: &[SessionId]) {
        for &s in sessions {
            self.structure.cancel_session(s);
            self.data.cancel_session(s);
            self.transfer.cancel_session(s);
            self.backup.cancel_session(s);
        }
    }
}

/// One engine's sessions and scratch containers.
struct Env {
    fixture: ResolvedEngine,
    engine: Engine,
    names: ScratchNames,
    m8: M8Fixture,
    transfer: Option<M8Transfer>,
    /// The sync pair's source / target, the restore target and the transfer target: one
    /// session each (on server engines all of the fixture profile; on SQLite one file each).
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
    /// Texts no backup archive may contain (the fixture password, the host), with labels.
    secrets: Vec<(&'static str, String)>,
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

    fn session_ids(&self) -> Vec<SessionId> {
        self.sessions().into_iter().map(SessionHandle::id).collect()
    }

    /// `table` of the container named `container` (SQLite: its own file).
    fn q(&self, container: &str, table: &str) -> String {
        seed::qualified(self.engine, container, table)
    }

    /// `SELECT <columns> FROM <container>.<table> ORDER BY <order>`.
    fn select(
        &self,
        container: &str,
        table: &str,
        columns: &[(&str, Kind)],
        order: &[&str],
    ) -> String {
        let quote = |c: &&str| seed::quote(self.engine, c);
        format!(
            "SELECT {} FROM {} ORDER BY {}",
            columns
                .iter()
                .map(|(c, _)| quote(c))
                .collect::<Vec<_>>()
                .join(", "),
            self.q(container, table),
            order.iter().map(quote).collect::<Vec<_>>().join(", ")
        )
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
        let prepared = prepare(&ctx, fixtures, e).await;
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
            let cell = if let (Some(src), Some(tgt)) = (&envs[si], &envs[ti]) {
                let cancel = CancellationToken::new();
                let sessions = vec![src.src.id(), tgt.xfr.id()];
                let fut = transfer_cell(
                    services.clone(),
                    dialects.clone(),
                    src.clone(),
                    tgt.clone(),
                    cancel.clone(),
                );
                let (status, detail, ms) =
                    guarded(TRANSFER_LIMIT, &services, &sessions, &cancel, fut).await;
                let mut c = M8Cell::transfer(s, t, status, detail);
                c.elapsed_ms = Some(ms);
                c
            } else {
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
            };
            cells.push(cell);
        }
    }

    // Per engine: the backup reads the source and writes the restore container, the data sync
    // changes the target's rows, the structure sync (last) the target's structure.
    for (e, env) in engines.iter().zip(&envs) {
        let Some(env) = env else { continue };
        let sessions = env.session_ids();
        for check in [M8Check::Backup, M8Check::SyncData, M8Check::SyncStructure] {
            let cancel = CancellationToken::new();
            let fut: CellFuture = match check {
                M8Check::Backup => Box::pin(backup_cell(
                    services.clone(),
                    dialects.clone(),
                    env.clone(),
                    cancel.clone(),
                )),
                M8Check::SyncData => {
                    Box::pin(data_cell(services.clone(), env.clone(), cancel.clone()))
                }
                _ => Box::pin(structure_cell(
                    services.clone(),
                    dialects.clone(),
                    env.clone(),
                    cancel.clone(),
                )),
            };
            let (status, detail, ms) =
                guarded(CELL_LIMIT, &services, &sessions, &cancel, fut).await;
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

/// Runs one cell on its own task with a time limit; on timeout the task is aborted, `cancel`
/// is cancelled and every run on `sessions` is cancelled, so nothing keeps writing while the
/// next cell runs. A cell that did not pass while an unfinished M8 body panicked is pending
/// ([`skeleton`]).
async fn guarded<F>(
    limit: Duration,
    services: &Services,
    sessions: &[SessionId],
    cancel: &CancellationToken,
    fut: F,
) -> (CellStatus, String, u64)
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
            cancel.cancel();
            services.cancel_sessions(sessions);
            fail(format!(
                "did not finish within {} s (its runs were cancelled)",
                limit.as_secs()
            ))
        }
    };
    let streams = skeleton::since(mark);
    let (status, detail) = match verdict {
        Verdict::Pass(d) => (CellStatus::Pass, d),
        Verdict::Pending(d) => (CellStatus::Pending, d),
        Verdict::NotApplicable(d) => (CellStatus::NotApplicable, d),
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

/// Waits for the terminal event of a run; returns it and how many events came before it.
/// Gives up early when an unfinished M8 body panicked meanwhile (its task may never send one);
/// on giving up `stop` cancels the run.
async fn terminal<E>(
    rx: &mut mpsc::Receiver<E>,
    is_terminal: impl Fn(&E) -> bool,
    mark: usize,
    stop: impl FnOnce(),
) -> Result<(E, usize), String> {
    let deadline = Instant::now() + RUN_LIMIT;
    let mut before = 0usize;
    let why = loop {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(event)) if is_terminal(&event) => return Ok((event, before)),
            Ok(Some(_)) => before += 1,
            Ok(None) => break "the event stream ended without a terminal event".to_owned(),
            Err(_) => {
                if !skeleton::since(mark).is_empty() {
                    break "an unfinished M8 body panicked during the run".to_owned();
                }
                if Instant::now() > deadline {
                    break format!(
                        "no terminal event within {} s (the run was cancelled)",
                        RUN_LIMIT.as_secs()
                    );
                }
            }
        }
    };
    stop();
    Err(why)
}

/// A run that finished must have reported at least its start before the end.
fn require_events(what: &str, before: usize) -> Result<(), Verdict> {
    if before == 0 {
        Err(fail(format!(
            "the {what} sent its terminal event without a Started / Progress event before it"
        )))
    } else {
        Ok(())
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

/// What [`build`] opened or created so far (cleaned up when it fails).
#[derive(Default)]
struct Opened {
    sessions: Vec<SessionHandle>,
    /// The scratch names (networked engines: containers may exist) and the temp dir.
    scratch: Option<(ScratchNames, PathBuf)>,
}

/// Opens the sessions, drops stale scratch containers, builds the scratch copies and checks
/// them (the `seed` cell). On a failure (or timeout) the sessions it opened are closed and
/// the scratch containers it may have created are dropped.
async fn prepare(
    ctx: &Ctx,
    fixtures: &Fixtures,
    e: &ResolvedEngine,
) -> Result<(Env, String), Prepared> {
    let mut opened = Opened::default();
    let out = tokio::time::timeout(SEED_LIMIT, build(ctx, fixtures, e, &mut opened))
        .await
        .unwrap_or_else(|_| {
            Err(Prepared::Failed(format!(
                "did not finish within {} s",
                SEED_LIMIT.as_secs()
            )))
        });
    match out {
        Ok((env, detail)) => {
            // The seeding session (its `USE` changed its default database) is not kept.
            let kept = env.session_ids();
            for s in &opened.sessions {
                if !kept.contains(&s.id()) {
                    let _ = ctx.mgr.close(s.id()).await;
                }
            }
            Ok((env, detail))
        }
        Err(err) => {
            let _ = tokio::time::timeout(CLEANUP_LIMIT, abandon(ctx, e, &opened)).await;
            Err(err)
        }
    }
}

/// Best-effort cleanup of a failed [`build`].
async fn abandon(ctx: &Ctx, e: &ResolvedEngine, opened: &Opened) {
    if let (Some(first), Some((names, _))) = (opened.sessions.first(), &opened.scratch)
        && e.engine.is_networked()
    {
        for sql in seed::drop_containers(e.engine, &names.all()) {
            if let Err(err) = run_sql(first.connection().as_ref(), &sql).await {
                eprintln!(
                    "[m8 conformance] {}: dropping the scratch containers failed: {err}",
                    e.name
                );
            }
        }
    }
    for s in &opened.sessions {
        let _ = ctx.mgr.close(s.id()).await;
    }
    if let Some((_, dir)) = &opened.scratch {
        let _ = std::fs::remove_dir_all(dir);
    }
}

async fn build(
    ctx: &Ctx,
    fixtures: &Fixtures,
    e: &ResolvedEngine,
    opened: &mut Opened,
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
    opened.scratch = Some((names.clone(), dir.clone()));
    let transfer = m8.transfer();
    let secrets = secrets_of(fixtures, e);

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
        // Seeding runs on its own session (the MySQL seed's `USE` changes it), closed after.
        let seeder = open(ctx, e, None).await.map_err(Prepared::Unreachable)?;
        opened.sessions.push(seeder.clone());
        let conn = seeder.connection().as_ref();
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
        // One session per role, so a service that mixes up source and target sessions shows.
        let src = open_role(ctx, e, opened).await?;
        let tgt = open_role(ctx, e, opened).await?;
        let rst = open_role(ctx, e, opened).await?;
        let xfr = open_role(ctx, e, opened).await?;
        notes.push("source, target, restore and transfer sessions separate".to_owned());
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
            src,
            tgt,
            rst,
            xfr,
            dir,
            secrets,
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
        opened.sessions.push(src.clone());
        run_statements(src.connection().as_ref(), e.engine, &src_sql)
            .await
            .map_err(|err| format!("seeding the source file: {err}"))?;
        let tgt = open(ctx, e, Some(dir.join("tgt.sqlite"))).await?;
        opened.sessions.push(tgt.clone());
        run_statements(tgt.connection().as_ref(), e.engine, &tgt_sql)
            .await
            .map_err(|err| format!("seeding the target file: {err}"))?;
        let rst = open(ctx, e, Some(dir.join("rst.sqlite"))).await?;
        opened.sessions.push(rst.clone());
        let xfr = open(ctx, e, Some(dir.join("xfr.sqlite"))).await?;
        opened.sessions.push(xfr.clone());
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
            secrets,
        }
    };
    let counts = check_counts(&env).await?;
    notes.insert(0, counts);
    Ok((env, notes.join("; ")))
}

/// Opens one more session of the fixture profile (a role of [`Env`]).
async fn open_role(
    ctx: &Ctx,
    e: &ResolvedEngine,
    opened: &mut Opened,
) -> Result<SessionHandle, String> {
    let s = open(ctx, e, None).await?;
    opened.sessions.push(s.clone());
    Ok(s)
}

/// The fixture password and (networked) host, with what they are: texts a backup archive must
/// not contain.
fn secrets_of(fixtures: &Fixtures, e: &ResolvedEngine) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if let Some(pw) = fixtures
        .engines
        .get(&e.name)
        .and_then(|f| f.password.clone())
        .filter(|p| !p.is_empty())
    {
        out.push(("the fixture password", pw));
    }
    if let Target::Tcp { host, .. } = &e.target
        && host.len() >= 4
    {
        out.push(("the fixture host", host.clone()));
    }
    out
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
    exec(conn, sql, &[], &CancellationToken::new())
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

/// Every row of `sql` as typed values (compared with [`values::differences`]).
async fn typed_rows(conn: &dyn Connection, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    let out = run_sql(conn, sql).await?;
    let mut rows = Vec::new();
    for batch in out.result_sets.iter().flat_map(|r| r.batches.iter()) {
        for row in 0..batch.num_rows() {
            let mut values = Vec::with_capacity(batch.num_columns());
            for c in batch.columns() {
                values.push(cell_value(c.as_ref(), row).map_err(|err| {
                    format!("cannot read a value of `{}`: {err}", first_line(sql))
                })?);
            }
            rows.push(values);
        }
    }
    Ok(rows)
}

async fn count(conn: &dyn Connection, table: &str) -> Result<u64, String> {
    scalar(conn, &format!("SELECT COUNT(*) FROM {table}")).await
}

/// The first value of `sql` as a whole number.
async fn scalar(conn: &dyn Connection, sql: &str) -> Result<u64, String> {
    let values = column(conn, sql).await?;
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
        let _ = write!(out, ", {schema}.{types} {n} rows");
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
    for (table, col, src, tgt) in SEED_SUMS {
        if table == "m8_big" && big != 100_000 {
            continue;
        }
        for (want, session, container) in [
            (src, &env.src, &env.names.source),
            (tgt, &env.tgt, &env.names.target),
        ] {
            let sql = format!(
                "SELECT SUM({}) FROM {}",
                seed::quote(env.engine, col),
                env.q(container, table)
            );
            match scalar(session.connection().as_ref(), &sql).await {
                Ok(n) if n == want => checked += 1,
                Ok(n) => problems.push(format!(
                    "{container}.{table}: SUM({col}) {n}, expected {want}"
                )),
                Err(err) => problems.push(err),
            }
        }
    }
    if problems.is_empty() {
        Ok(format!(
            "{checked} row counts and sums of the scratch copies as designed"
        ))
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

// --- independent value checks ------------------------------------------------------------

/// One side of a value comparison: a session and a container on it.
#[derive(Clone, Copy)]
struct Side<'a> {
    session: &'a SessionHandle,
    container: &'a str,
}

/// The rows of `table` on both sides, read in `order` and compared column by column
/// ([`values::differences`]); one problem per differing column.
async fn compare_rows(
    env: &Env,
    a: Side<'_>,
    b: Side<'_>,
    table: &str,
    columns: &[(&str, Kind)],
    order: &[&str],
) -> Vec<String> {
    let want = read_side(env, a, table, columns, order).await;
    let got = read_side(env, b, table, columns, order).await;
    match (want, got) {
        (Ok(want), Ok(got)) => values::differences(columns, &want, &got)
            .into_iter()
            .map(|d| format!("{table} {d}"))
            .collect(),
        (Err(err), _) | (_, Err(err)) => vec![format!("{table}: {err}")],
    }
}

/// The rows of `table` on one side, in `order`.
async fn read_side(
    env: &Env,
    side: Side<'_>,
    table: &str,
    columns: &[(&str, Kind)],
    order: &[&str],
) -> Result<Vec<Vec<Value>>, String> {
    let sql = env.select(side.container, table, columns, order);
    typed_rows(side.session.connection().as_ref(), &sql).await
}

/// The data pairs read back on both sides compare equal: `m8_data` and `m8_composite` value by
/// value, `m8_big` by its count and two sums. Independent of the data-sync compare.
async fn compare_data_tables(env: &Env, a: Side<'_>, b: Side<'_>) -> Vec<String> {
    let mut problems = compare_rows(env, a, b, "m8_data", &values::M8_DATA, &["id"]).await;
    problems.extend(
        compare_rows(
            env,
            a,
            b,
            "m8_composite",
            &values::M8_COMPOSITE,
            &["tenant", "code"],
        )
        .await,
    );
    let q = |c: &str| seed::quote(env.engine, c);
    for aggregate in [
        ("COUNT(*)", None),
        ("SUM", Some("id")),
        ("SUM", Some("val")),
    ] {
        let expr = match aggregate {
            (f, Some(col)) => format!("{f}({})", q(col)),
            (f, None) => f.to_owned(),
        };
        let mut got = Vec::new();
        for side in [a, b] {
            let sql = format!("SELECT {expr} FROM {}", env.q(side.container, "m8_big"));
            got.push(scalar(side.session.connection().as_ref(), &sql).await);
        }
        match (&got[0], &got[1]) {
            (Ok(x), Ok(y)) if x == y => {}
            (x, y) => problems.push(format!("m8_big {expr}: {x:?} vs {y:?}")),
        }
    }
    problems
}

// --- structure sync ----------------------------------------------------------------------

async fn compare_structure(
    services: &Services,
    request: &StructureCompareRequest,
    cancel: &CancellationToken,
) -> Result<StructureCompareResult, Verdict> {
    services
        .structure
        .compare(request, cancel.clone())
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

/// The status the seeds design for a `[m8] structure_items` id: the sequence, the routine and
/// the trigger exist only in the source, so do `order_items` and `MixedCase`; `order_lines` and
/// `legacy_notes` only in the target; every other table and the view differ.
fn designed_status(id: &str) -> DiffStatus {
    let (kind, name) = id.split_once(':').unwrap_or(("", id));
    match (kind, name) {
        ("sequence" | "function" | "procedure" | "trigger", _)
        | (_, "order_items" | "MixedCase") => DiffStatus::OnlyInSource,
        (_, "order_lines" | "legacy_notes") => DiffStatus::OnlyInTarget,
        _ => DiffStatus::Different,
    }
}

/// Whether the engine runs DDL in transactions (the structure script must then be atomic).
fn transactional_ddl(engine: Engine) -> bool {
    !matches!(engine, Engine::MySql | Engine::MariaDb)
}

async fn structure_cell(
    services: Arc<Services>,
    dialects: Arc<DialectRegistry>,
    env: Arc<Env>,
    cancel: CancellationToken,
) -> Verdict {
    let missing = missing_sync_roles(&dialects, env.engine);
    if !missing.is_empty() {
        return Verdict::Pending(format!(
            "pending (structure-sync roles missing: {})",
            missing.join(", ")
        ));
    }
    match structure_run(&services, &env, &cancel).await {
        Ok(d) => Verdict::Pass(d),
        Err(v) => v,
    }
}

/// The kept columns of the target's seeded rows (customers, products, orders) and the
/// audit row count, read before and after the sync.
async fn kept_rows(env: &Env) -> Result<Vec<(String, Vec<Vec<Value>>)>, String> {
    let conn = env.tgt.connection().as_ref();
    let mut out = Vec::new();
    for (table, columns) in [
        ("customers", &values::CUSTOMERS_KEPT[..]),
        ("products", &values::PRODUCTS_KEPT[..]),
        ("orders", &values::ORDERS[..]),
    ] {
        let sql = env.select(&env.names.target, table, columns, &["id"]);
        out.push((table.to_owned(), typed_rows(conn, &sql).await?));
    }
    let audit = count(conn, &env.q(&env.names.target, "order_audit")).await?;
    out.push((
        "order_audit".to_owned(),
        vec![vec![Value::Int(i64::try_from(audit).unwrap_or(i64::MAX))]],
    ));
    Ok(out)
}

async fn structure_run(services: &Services, env: &Env, cancel: &CancellationToken) -> CellResult {
    let before = kept_rows(env).await.map_err(fail)?;
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
    let first = compare_structure(services, &request, cancel).await?;
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
    let wrong: Vec<String> = expected
        .iter()
        .filter_map(|id| {
            let want = designed_status(id);
            match first.diff.item(id.as_str()) {
                Some(i) if i.status == want => None,
                Some(i) => Some(format!("{id}: {:?}, designed {want:?}", i.status)),
                None => Some(format!("{id}: not reported, designed {want:?}")),
            }
        })
        .collect();
    if !wrong.is_empty() {
        let _ = services.structure.forget(first.compare_id);
        return Err(fail(format!(
            "the compare does not report the designed differences: {}; it reported: {}",
            wrong.join(", "),
            differing(&first, true).join(", ")
        )));
    }
    let to_apply = differing(&first, true);
    let extra: Vec<String> = to_apply
        .iter()
        .filter(|d| !expected.iter().any(|id| d.starts_with(&format!("{id} ("))))
        .cloned()
        .collect();
    // The preview of what the apply runs: every differing item, drops flagged destructive.
    let preview = services
        .structure
        .script(&StructureScriptRequest {
            compare_id: first.compare_id,
            selection: Vec::new(),
        })
        .map_err(|err| fail(format!("structure script failed: {err}")))?;
    if preview.is_empty() || preview.sql.trim().is_empty() || !preview.destructive {
        return Err(fail(format!(
            "the script preview is empty or not flagged destructive (it drops objects): \
             destructive {}, {} steps",
            preview.destructive,
            preview.script.steps.len()
        )));
    }
    let ddl_in_tx = transactional_ddl(env.engine);
    if preview.transactional_ddl != ddl_in_tx || preview.atomic != ddl_in_tx {
        return Err(fail(format!(
            "the script preview says transactional_ddl {} and atomic {}; on {} both must be {}",
            preview.transactional_ddl,
            preview.atomic,
            env.engine.as_str(),
            ddl_in_tx
        )));
    }
    // Data safety: a destructive script without confirmation is refused.
    if services
        .structure
        .apply(
            &StructureApplyRequest {
                compare_id: first.compare_id,
                selection: Vec::new(),
                confirm_destructive: false,
            },
            cancel.clone(),
        )
        .await
        .is_ok()
    {
        return Err(fail(
            "the apply ran a destructive script without confirm_destructive",
        ));
    }
    let report = services
        .structure
        .apply(
            &StructureApplyRequest {
                compare_id: first.compare_id,
                selection: Vec::new(),
                confirm_destructive: true,
            },
            cancel.clone(),
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
    let again = compare_structure(services, &request, cancel).await?;
    let _ = services.structure.forget(again.compare_id);
    if !again.diff.is_in_sync(true) {
        return Err(fail(format!(
            "a fresh compare after the apply still differs: {}",
            differing(&again, true).join(", ")
        )));
    }
    // The target's rows survive (a rebuild must copy them by name, not lose or shift them).
    let after = kept_rows(env).await.map_err(fail)?;
    let mut lost = Vec::new();
    for ((table, want), (_, got)) in before.iter().zip(&after) {
        let columns: &[(&str, Kind)] = match table.as_str() {
            "customers" => &values::CUSTOMERS_KEPT,
            "products" => &values::PRODUCTS_KEPT,
            "orders" => &values::ORDERS,
            _ => &[("rows", Kind::Int)],
        };
        lost.extend(
            values::differences(columns, want, got)
                .into_iter()
                .map(|d| format!("{table} {d}")),
        );
    }
    if !lost.is_empty() {
        return Err(fail(format!(
            "the sync changed the target's rows: {}",
            lost.join("; ")
        )));
    }
    Ok(format!(
        "{} designed items reported with their designed status{}; unconfirmed apply refused; \
         applied {} items ({} script statements, destructive: {}, atomic: {}); re-compare and \
         a fresh compare in sync ({} items same); customers, products, orders and order_audit \
         rows kept",
        expected.len(),
        if extra.is_empty() {
            String::new()
        } else {
            format!(" (also differing: {})", extra.join(", "))
        },
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
    let id = handle.id;
    match terminal(
        &mut handle.events,
        DataCompareEvent::is_terminal,
        mark,
        || {
            services.data.cancel(id);
        },
    )
    .await
    {
        Ok((DataCompareEvent::Finished { result }, before)) => {
            require_events("data compare", before)?;
            Ok(result)
        }
        Ok((DataCompareEvent::Failed { error, .. }, _)) => {
            Err(fail(format!("data compare failed: {}", error.message)))
        }
        Ok((other, _)) => Err(fail(format!("data compare ended with {other:?}"))),
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

async fn data_cell(services: Arc<Services>, env: Arc<Env>, cancel: CancellationToken) -> Verdict {
    match data_run(&services, &env, &cancel).await {
        Ok(d) => Verdict::Pass(d),
        Err(v) => v,
    }
}

/// Starts a data apply of `compare_id` with deletes and waits for its end.
async fn apply_data(
    services: &Services,
    compare_id: oxbase_services::runs::OperationId,
    confirm_deletes: bool,
) -> Result<(DataApplyEvent, usize), Verdict> {
    let mark = skeleton::mark();
    let mut handle = services
        .data
        .start_apply(DataApplyRequest {
            compare_id,
            options: DataApplyOptions {
                script: DataScriptOptions {
                    deletes: true,
                    ..DataScriptOptions::default()
                },
                confirm_deletes,
                ..DataApplyOptions::default()
            },
        })
        .await
        .map_err(|err| fail(format!("data apply refused: {err}")))?;
    let id = handle.id;
    terminal(
        &mut handle.events,
        DataApplyEvent::is_terminal,
        mark,
        || {
            services.data.cancel(id);
        },
    )
    .await
    .map_err(|err| fail(format!("data apply: {err}")))
}

async fn data_run(services: &Services, env: &Env, cancel: &CancellationToken) -> CellResult {
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

    // The preview of every statement the apply runs (one per differing row).
    let want_statements: u64 = env.m8.data_expected.values().flatten().sum();
    let preview = services
        .data
        .script(
            &DataScriptRequest {
                compare_id: result.compare_id,
                options: DataScriptOptions {
                    deletes: true,
                    ..DataScriptOptions::default()
                },
            },
            cancel.clone(),
        )
        .await
        .map_err(|err| fail(format!("data script failed: {err}")))?;
    if preview.total_statements != want_statements
        || preview.statements.is_empty()
        || !preview.destructive
    {
        return Err(fail(format!(
            "the data script preview has {} statements ({} rendered, destructive {}), \
             expected {want_statements} with deletes flagged",
            preview.total_statements,
            preview.statements.len(),
            preview.destructive
        )));
    }

    // Data safety: deletes without confirmation are refused (before or at the start).
    let unconfirmed = apply_data(services, result.compare_id, false).await;
    let refused = match &unconfirmed {
        Err(_) => true,
        Ok((DataApplyEvent::Failed { report, .. }, _)) => {
            report.statements == 0 && report.committed_batches == 0
        }
        Ok(_) => false,
    };
    if !refused {
        let shown = unconfirmed
            .map(|(event, _)| format!("{event:?}"))
            .unwrap_or_default();
        return Err(fail(format!(
            "an apply with deletes but without confirm_deletes was not refused: {shown}"
        )));
    }

    let report = match apply_data(services, result.compare_id, true).await? {
        (DataApplyEvent::Finished { report }, before) => {
            require_events("data apply", before)?;
            report
        }
        (DataApplyEvent::Failed { error, report }, _) => {
            return Err(fail(format!(
                "data apply failed: {} (left behind: {})",
                error.message, report.left_behind
            )));
        }
        (other, _) => return Err(fail(format!("data apply ended with {other:?}"))),
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
    // Independent of the service's compare: read both sides back and compare the values.
    let differ = compare_data_tables(
        env,
        Side {
            session: &env.src,
            container: &env.names.source,
        },
        Side {
            session: &env.tgt,
            container: &env.names.target,
        },
    )
    .await;
    if !differ.is_empty() {
        return Err(fail(format!(
            "after the apply the target's values differ from the source's: {}",
            differ.join("; ")
        )));
    }
    Ok(format!(
        "{}; script preview {want_statements} statements; apply without confirm_deletes \
         refused; applied {} statements in {} batches; re-compare and a fresh compare in sync; \
         m8_data, m8_composite and m8_big read back equal",
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
    cancel: CancellationToken,
) -> Verdict {
    let path = env.dir.join(format!("{}.oxbackup", env.names.token));
    let out = backup_run(&services, &dialects, &env, &path, &cancel).await;
    let _ = std::fs::remove_file(&path);
    match out {
        Ok(d) => Verdict::Pass(d),
        Err(v) => v,
    }
}

/// Whether `haystack` contains `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// The archive's bytes, raw and every ZIP entry decompressed, hold none of `secrets`. `Ok`
/// says what was scanned.
fn scan_archive(path: &Path, secrets: &[(&str, String)]) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|err| format!("cannot read the archive: {err}"))?;
    let leak = |data: &[u8], place: &str| -> Result<(), String> {
        match secrets.iter().find(|(_, s)| contains(data, s.as_bytes())) {
            Some((label, _)) => Err(format!("the archive ({place}) contains {label}")),
            None => Ok(()),
        }
    };
    leak(&bytes, "raw bytes")?;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes))
        .map_err(|err| format!("the archive is not a readable ZIP file: {err}"))?;
    let mut scanned = 0usize;
    let mut unreadable = Vec::new();
    for i in 0..zip.len() {
        let mut entry = match zip.by_index(i) {
            Ok(entry) => entry,
            Err(err) => {
                unreadable.push(format!("#{i} ({err})"));
                continue;
            }
        };
        let name = String::from_utf8_lossy(entry.name_raw()).into_owned();
        let mut data = Vec::new();
        if let Err(err) = std::io::Read::read_to_end(&mut entry, &mut data) {
            unreadable.push(format!("{name} ({err})"));
            continue;
        }
        leak(&data, &name)?;
        scanned += 1;
    }
    let mut out =
        format!("no password or host in the archive ({scanned} entries scanned decompressed)");
    if !unreadable.is_empty() {
        let _ = write!(out, ", not scanned: {}", unreadable.join(", "));
    }
    Ok(out)
}

/// A truncated copy and a copy with [`DAMAGED_BYTES`] inverted bytes in its middle must not
/// verify (an error or `ok: false`; a panic fails the cell).
async fn verify_damaged(
    services: &Services,
    path: &Path,
    dir: &Path,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|err| format!("cannot read the archive: {err}"))?;
    let half = bytes.len() / 2;
    let mut flipped = bytes.clone();
    let end = (half + DAMAGED_BYTES).min(flipped.len());
    for b in &mut flipped[half..end] {
        *b = !*b;
    }
    let copies = [("truncated", bytes[..half].to_vec()), ("damaged", flipped)];
    let mut problems = Vec::new();
    for (what, data) in copies {
        let copy = dir.join(format!("{what}.oxbackup"));
        std::fs::write(&copy, &data).map_err(|err| format!("cannot write {what}: {err}"))?;
        let verdict = services.backup.verify(&copy, cancel.clone()).await;
        let _ = std::fs::remove_file(&copy);
        if let Ok(report) = verdict
            && report.ok
        {
            problems.push(format!("verify accepted the {what} copy of the archive"));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

async fn backup_run(
    services: &Services,
    dialects: &DialectRegistry,
    env: &Env,
    path: &Path,
    cancel: &CancellationToken,
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
    let id = handle.id;
    let backup = match terminal(&mut handle.events, BackupEvent::is_terminal, mark, || {
        services.backup.cancel(id);
    })
    .await
    {
        Ok((BackupEvent::Finished { report }, before)) => {
            require_events("backup", before)?;
            report
        }
        Ok((BackupEvent::Failed { error, .. }, _)) => {
            return Err(fail(format!("backup failed: {}", error.message)));
        }
        Ok((other, _)) => return Err(fail(format!("backup ended with {other:?}"))),
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
        .list(path, cancel.clone())
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
        .verify(path, cancel.clone())
        .await
        .map_err(|err| fail(format!("verify failed: {err}")))?;
    if !verify.ok {
        return Err(fail(format!(
            "verify found problems: {:?}",
            verify.problems
        )));
    }
    verify_damaged(services, path, &env.dir, cancel)
        .await
        .map_err(fail)?;
    let scanned = scan_archive(path, &env.secrets).map_err(fail)?;

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
    let id = handle.id;
    let restore = match terminal(&mut handle.events, RestoreEvent::is_terminal, mark, || {
        services.backup.cancel(id);
    })
    .await
    {
        Ok((RestoreEvent::Finished { report }, before)) => {
            require_events("restore", before)?;
            report
        }
        Ok((RestoreEvent::Failed { error, report }, _)) => {
            return Err(fail(format!(
                "restore failed: {} (left behind: {})",
                error.message, report.left_behind
            )));
        }
        Ok((other, _)) => return Err(fail(format!("restore ended with {other:?}"))),
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

    // Row counts and values first: they need no other stream. order_audit catches a trigger
    // that fired during the data load (duplicated audit rows).
    let mut problems = Vec::new();
    for table in env
        .m8
        .data_tables
        .iter()
        .map(String::as_str)
        .chain(KEPT_TABLES)
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
    let source = Side {
        session: &env.src,
        container: &env.names.source,
    };
    let restored = Side {
        session: &env.rst,
        container: &env.names.restore,
    };
    problems.extend(compare_data_tables(env, source, restored).await);
    problems.extend(compare_rows(env, source, restored, "orders", &values::ORDERS, &["id"]).await);
    if !problems.is_empty() {
        return Err(fail(format!(
            "restored rows differ: {}",
            problems.join("; ")
        )));
    }

    let missing = missing_sync_roles(dialects, env.engine);
    if !missing.is_empty() {
        return Err(Verdict::Pending(format!(
            "pending (backup, list, verify, damaged copies and restore ran, rows read back \
             equal; the structure compare needs the structure-sync roles: {})",
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
    let compared = compare_structure(services, &request, cancel).await?;
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
        "{} objects, {} bytes; verify ok ({} entries), truncated and damaged copies rejected; \
         {scanned}; restored {} rows (counts incl. orders / order_audit and values equal); \
         structure and data compares source / restore in sync",
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
        .filter(|c| !c.skipped && c.mapping.fidelity != Fidelity::Exact)
        .map(|c| {
            format!(
                "{} {} -> {}: {} ({})",
                c.source,
                c.source_native,
                c.mapping.native_type,
                fidelity(c.mapping.fidelity),
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

fn fidelity(f: Fidelity) -> String {
    format!("{f:?}").to_ascii_lowercase()
}

async fn run_transfer(services: &Services, request: TransferRequest) -> Result<(), Verdict> {
    let mark = skeleton::mark();
    let mut handle = services
        .transfer
        .start(request)
        .await
        .map_err(|err| fail(format!("transfer refused: {err}")))?;
    let id = handle.id;
    let report = match terminal(&mut handle.events, TransferEvent::is_terminal, mark, || {
        services.transfer.cancel(id);
    })
    .await
    {
        Ok((TransferEvent::Finished { report }, before)) => {
            require_events("transfer", before)?;
            report
        }
        Ok((TransferEvent::Failed { error, report }, _)) => {
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
        Ok((other, _)) => return Err(fail(format!("transfer ended with {other:?}"))),
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
    cancel: CancellationToken,
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
    match transfer_run(&services, &src, &tgt, &cancel).await {
        Ok(d) => Verdict::Pass(d),
        Err(v) => v,
    }
}

/// The exit-check pair: PostgreSQL → MySQL.
fn is_required(src: Engine, tgt: Engine) -> bool {
    src == Engine::Postgres && tgt == Engine::MySql
}

/// A plan that cannot run is acceptable (not applicable) only when it blocks on columns the
/// mapper marks unsupported, each with a reason; anything else is a failure.
fn blocked_verdict(plan: &TransferPlan, required: bool) -> Verdict {
    let unsupported: Vec<(String, Option<String>)> = plan
        .tables
        .iter()
        .flat_map(|t| t.columns.iter())
        .filter(|c| !c.skipped && c.mapping.fidelity == Fidelity::Unsupported)
        .map(|c| {
            (
                format!("{} {}", c.source, c.source_native),
                c.mapping.reason.clone().filter(|r| !r.trim().is_empty()),
            )
        })
        .collect();
    let why = format!(
        "plan not runnable: {}; {}",
        plan.blocked.join("; "),
        lossiness(plan)
    );
    let precise = !unsupported.is_empty() && unsupported.iter().all(|(_, r)| r.is_some());
    if required || !precise {
        return fail(format!(
            "{why} (a plan may only block on columns marked unsupported with a reason)"
        ));
    }
    Verdict::NotApplicable(format!(
        "not applicable: unsupported {}",
        unsupported
            .iter()
            .map(|(c, r)| format!("{c} ({})", r.as_deref().unwrap_or("")))
            .collect::<Vec<_>>()
            .join("; ")
    ))
}

async fn transfer_run(
    services: &Services,
    src: &Env,
    tgt: &Env,
    cancel: &CancellationToken,
) -> CellResult {
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
        .plan(&request, cancel.clone())
        .await
        .map_err(|err| fail(format!("plan failed: {err}")))?;
    let lossy = lossiness(&plan);
    if !plan.runnable {
        return Err(blocked_verdict(&plan, required));
    }
    run_transfer(services, request).await?;
    // Every column read back: exact mappings must match exactly (after normalizing the
    // engines' spellings, values.rs); lossy ones are listed.
    let columns = values::M8_DATA;
    let want = typed_rows(
        src.src.connection().as_ref(),
        &src.select(&src.names.source, "m8_data", &columns, &["id"]),
    )
    .await
    .map_err(fail)?;
    let got = typed_rows(
        tgt.xfr.connection().as_ref(),
        &tgt.select(&tgt.names.transfer_target, &target_name, &columns, &["id"]),
    )
    .await
    .map_err(fail)?;
    let planned: BTreeMap<&str, Fidelity> = plan
        .tables
        .first()
        .map(|t| {
            t.columns
                .iter()
                .map(|c| (c.source.as_str(), c.mapping.fidelity))
                .collect()
        })
        .unwrap_or_default();
    let mut hard = Vec::new();
    let mut listed = Vec::new();
    for d in values::differences(&columns, &want, &got) {
        let planned_fidelity = d
            .column
            .as_deref()
            .and_then(|c| planned.get(c).copied())
            .unwrap_or(Fidelity::Exact);
        if planned_fidelity == Fidelity::Exact {
            hard.push(d.to_string());
        } else {
            listed.push(format!("{d} (planned {})", fidelity(planned_fidelity)));
        }
    }
    if !hard.is_empty() {
        return Err(fail(format!(
            "read back differs where the plan says exact: {}; {lossy}",
            hard.join("; ")
        )));
    }
    let mut detail = format!(
        "{} rows read back, every column compared ({}); {lossy}",
        want.len(),
        if listed.is_empty() {
            "all equal".to_owned()
        } else {
            format!("lossy as planned: {}", listed.join("; "))
        }
    );
    if required {
        let types = transfer_types(services, src, tgt, cancel).await?;
        detail.push_str("; ");
        detail.push_str(&types);
    }
    Ok(detail)
}

/// PostgreSQL → MySQL of the `[m8.transfer]` set (tables listed child first): plan fidelities
/// and `COLUMN_TYPE`s as `[m8.transfer.expected_mysql]`, the `m8_types` values, the row counts,
/// the load order and the foreign keys (`m8_tree` references itself).
async fn transfer_types(
    services: &Services,
    src: &Env,
    tgt: &Env,
    cancel: &CancellationToken,
) -> Result<String, Verdict> {
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
        .plan(&request, cancel.clone())
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
    match (position("m8_parent"), position("m8_child")) {
        (Some(parent), Some(child)) if parent < child => {}
        (Some(_), Some(_)) => {
            return Err(fail("[m8.transfer] plan loads m8_child before m8_parent"));
        }
        (parent, child) => {
            return Err(fail(format!(
                "[m8.transfer] plan order {:?} leaves out m8_parent ({parent:?}) or m8_child \
                 ({child:?})",
                plan.order
            )));
        }
    }
    let mut problems = Vec::new();
    let types_index = listed.iter().position(|t| *t == types);
    let planned: BTreeMap<&str, String> = types_index
        .and_then(|i| plan.tables.get(i))
        .map(|t| {
            t.columns
                .iter()
                .map(|c| (c.source.as_str(), fidelity(c.mapping.fidelity)))
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
    // The m8_types values, column by column ([m8.transfer.expected_mysql] comment rules).
    let type_columns: Vec<(&str, Kind)> = std::iter::once("id")
        .chain(
            set.expected_mysql
                .keys()
                .map(String::as_str)
                .filter(|c| *c != "id"),
        )
        .map(|c| (c, values::types_kind(c)))
        .collect();
    let want_rows = typed_rows(
        src.src.connection().as_ref(),
        &src.select(&src.names.transfer_source, &types, &type_columns, &["id"]),
    )
    .await
    .map_err(fail)?;
    let got_rows = typed_rows(
        conn,
        &tgt.select(&tgt.names.transfer_target, &types, &type_columns, &["id"]),
    )
    .await
    .map_err(fail)?;
    if Some(u64::try_from(got_rows.len()).unwrap_or(u64::MAX)) != set.types_rows {
        problems.push(format!(
            "{types}: {} rows, expected {:?}",
            got_rows.len(),
            set.types_rows
        ));
    }
    problems.extend(
        values::differences(&type_columns, &want_rows, &got_rows)
            .into_iter()
            .map(|d| format!("{types} {d}")),
    );
    // Every table's rows arrived.
    let mut counted = Vec::new();
    for table in &listed {
        let a = count(
            src.src.connection().as_ref(),
            &src.q(&src.names.transfer_source, table),
        )
        .await
        .map_err(fail)?;
        let b = count(conn, &tgt.q(&tgt.names.transfer_target, table))
            .await
            .map_err(fail)?;
        if a == b {
            counted.push(format!("{table} {b}"));
        } else {
            problems.push(format!("{table}: {b} rows, source {a}"));
        }
    }
    for (child, parent) in [("m8_child", "m8_parent"), ("m8_tree", "m8_tree")] {
        let fks = column(
            conn,
            &format!(
                "SELECT CAST(CONSTRAINT_NAME AS CHAR) \
                 FROM information_schema.REFERENTIAL_CONSTRAINTS \
                 WHERE CONSTRAINT_SCHEMA = '{schema}' AND TABLE_NAME = '{child}' \
                 AND REFERENCED_TABLE_NAME = '{parent}'"
            ),
        )
        .await
        .map_err(fail)?;
        if fks.is_empty() {
            problems.push(format!("no foreign key {child} -> {parent} after the load"));
        }
    }
    if problems.is_empty() {
        Ok(format!(
            "[m8.transfer] {} tables: {} columns with the expected MySQL types and fidelities, \
             {types} values read back equal, rows {}, m8_parent loaded before m8_child, foreign \
             keys m8_child -> m8_parent and m8_tree -> m8_tree present",
            listed.len(),
            set.expected_mysql.len(),
            counted.join(", ")
        ))
    } else {
        Err(fail(format!("[m8.transfer]: {}", problems.join("; "))))
    }
}
