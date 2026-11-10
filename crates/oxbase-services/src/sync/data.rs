//! The data-sync half of [`super::DataSyncService`] (`docs/m8-contracts.md` §6.4): the compare
//! and apply tasks, the kept compare results, script rendering and statement order.
//!
//! - **Compare**: per pair, the source and target tables are read on dedicated work sessions
//!   (children of the base sessions on the pair's databases, or the bases themselves for a
//!   private in-memory database) and compared by `oxbase_diff::data::compare_table`; differing
//!   rows spill into one [`DiffStore`] per compare, kept with the result under the compare id.
//! - **Order**: deletes children-first, then updates, then inserts parents-first, over the
//!   target tables' foreign keys (`oxbase_dialect::table_dependencies`).
//! - **Apply**: the bound statements of `oxbase_diff::data::data_statements` run on a work
//!   session of the target in transactions of `batch_statements`; each must affect exactly one
//!   row (a result set's rows when the plan returns them, else the affected-row count), else
//!   the batch rolls back and the apply stops. A table's inserts are wrapped in the statements
//!   of `oxbase_diff::data::insert_session` (identity inserts, sequence resync), which run on
//!   the same session and change no rows; after a failure the closing ones run anyway (best
//!   effort). Then the applied pairs are compared again. A compare is applied at most once;
//!   another apply needs a fresh compare.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use oxbase_dialect::{DialectRegistry, Quoting, inline_params, table_dependencies};
use oxbase_diff::data::{
    CompareJob, InsertSession, compare_table, data_statements, insert_session, row_key,
};
use oxbase_diff::{DiffStore, RowOp};
use oxbase_driver::{Tx, collect};
use oxbase_types::{
    BoundStatement, Engine, Error, ObjectDef, ObjectKind, Result, RowKey, ServerVersion, TableDef,
    Value,
};
use tokio::sync::mpsc;

use super::{
    DataApplyEvent, DataApplyOptions, DataApplyReport, DataCompareEvent, DataCompareOptions,
    DataCompareRequest, DataCompareResult, DataScript, DataScriptOptions, KEPT_COMPARES,
    TableApplyReport, TableCompareStatus, TableDataDiff, TablePair,
};
use crate::import::{WorkSession, close_work_session, work_session};
use crate::query::ErrorInfo;
use crate::runs::{OperationId, RunTicket};
use crate::{CancellationToken, ConnectionManager, SessionGuard, SessionHandle, SessionId};

/// Events buffered per run (progress is dropped when the receiver lags).
pub(super) const EVENT_BUFFER: usize = 64;
/// Rows read back from the store per batch (script and apply).
const STORE_BATCH_ROWS: usize = 1_000;
/// Progress events of an apply at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// How long closing a work session may hold up the end of a run.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

/// One kept compare: what `script` and `apply` work from.
pub(super) struct DataCompare {
    pub id: OperationId,
    pub engine: Engine,
    /// The target server's version (DML is written for it).
    pub version: ServerVersion,
    pub source_session: SessionId,
    pub target_session: SessionId,
    pub options: DataCompareOptions,
    /// Per request pair.
    pub pairs: Vec<KeptPair>,
    pub result: DataCompareResult,
    pub store: DiffStore,
    /// An apply of this compare started (a second one is refused).
    pub applied: AtomicBool,
}

/// A request pair and what its compare found out.
pub(super) struct KeptPair {
    pub pair: TablePair,
    /// The target table, its key and the compared columns, when the pair was compared.
    pub target: Option<Compared>,
}

/// What scripts and applies of a compared pair need.
pub(super) struct Compared {
    pub table: TableDef,
    pub key: RowKey,
    /// The compared columns (key first): the stored rows' columns.
    pub columns: Vec<String>,
}

impl KeptPair {
    fn compared(&self) -> Option<(&TableDef, &RowKey)> {
        self.target.as_ref().map(|c| (&c.table, &c.key))
    }
}

/// The kept compares of one service, oldest first (at most [`KEPT_COMPARES`]).
#[derive(Default)]
pub(super) struct Kept {
    entries: Mutex<VecDeque<Arc<DataCompare>>>,
}

impl Kept {
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Arc<DataCompare>>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn keep(&self, entry: DataCompare) {
        let mut entries = self.lock();
        entries.retain(|e| e.id != entry.id);
        entries.push_back(Arc::new(entry));
        while entries.len() > KEPT_COMPARES {
            entries.pop_front();
        }
    }

    pub fn get(&self, id: OperationId) -> Result<Arc<DataCompare>> {
        self.lock()
            .iter()
            .find(|e| e.id == id)
            .cloned()
            .ok_or_else(|| {
                Error::not_found(format!(
                    "data compare {id} is no longer kept; compare again"
                ))
            })
    }

    pub fn forget(&self, id: OperationId) -> bool {
        let mut entries = self.lock();
        let before = entries.len();
        entries.retain(|e| e.id != id);
        entries.len() != before
    }
}

/// What the run tasks share with the service.
#[derive(Clone)]
pub(super) struct Ctx {
    pub manager: Arc<ConnectionManager>,
    pub dialects: Arc<DialectRegistry>,
    pub kept: Arc<Kept>,
}

/// Checks a compare request before it starts; returns the engine and the target's version.
pub(super) fn check_request(
    ctx: &Ctx,
    request: &DataCompareRequest,
) -> Result<(Engine, ServerVersion)> {
    if request.pairs.is_empty() {
        return Err(Error::invalid_input("choose at least one table to compare"));
    }
    for (i, pair) in request.pairs.iter().enumerate() {
        for (side, obj) in [("source", &pair.source), ("target", &pair.target)] {
            if obj.kind != ObjectKind::Table {
                return Err(Error::invalid_input(format!(
                    "pair {} names no {side} table ({} is a {:?})",
                    i + 1,
                    obj.display_name(),
                    obj.kind
                )));
            }
        }
    }
    let base = |id: SessionId| {
        ctx.manager
            .get(id)
            .ok_or_else(|| Error::not_found(format!("no open session {id}")))
    };
    let source = base(request.source_session)?;
    let target = base(request.target_session)?;
    let (a, b) = (source.summary().engine, target.summary().engine);
    if a != b {
        return Err(Error::invalid_input(format!(
            "data sync compares two connections of the same engine; the source is {a} and the \
             target is {b}"
        )));
    }
    Ok((a, target.info().server_version.clone()))
}

// --- compare ---------------------------------------------------------------------------------

/// The compare task: `Started`, per table `Progress` / `TableFinished`, one terminal event.
#[allow(
    clippy::needless_pass_by_value,
    reason = "a spawned task owns its inputs"
)]
pub(super) async fn compare_task(
    ctx: Ctx,
    ticket: RunTicket,
    request: DataCompareRequest,
    (engine, version): (Engine, ServerVersion),
    store: DiffStore,
    events: mpsc::Sender<DataCompareEvent>,
) {
    let id = ticket.id();
    let token = ticket.token().clone();
    let _ = events
        .send(DataCompareEvent::Started {
            compare_id: id,
            tables: request.pairs.len(),
        })
        .await;
    let progress_events = events.clone();
    let on_progress = move |progress: &oxbase_diff::CompareProgress| {
        let _ = progress_events.try_send(DataCompareEvent::Progress {
            progress: *progress,
        });
    };
    let on_table = |table: &TableDataDiff| {
        let events = events.clone();
        let table = Box::new(table.clone());
        async move {
            let _ = events.send(DataCompareEvent::TableFinished { table }).await;
        }
    };
    let (entry, error) = run_compare(
        &ctx,
        id,
        &request,
        engine,
        version,
        store,
        &token,
        &on_progress,
        on_table,
    )
    .await;
    let result = entry.result.clone();
    ctx.kept.keep(entry);
    let cancelled = token.is_cancelled()
        || result
            .tables
            .iter()
            .any(|t| t.status == TableCompareStatus::Cancelled);
    let terminal = match error {
        Some(e) if !e.is_cancelled() => DataCompareEvent::Failed {
            error: ErrorInfo::from(&e),
            result,
        },
        Some(_) => DataCompareEvent::Cancelled { result },
        None if cancelled => DataCompareEvent::Cancelled { result },
        None => DataCompareEvent::Finished { result },
    };
    let _ = events.send(terminal).await;
    drop(ticket);
}

/// A session a run reads or writes through: a dedicated work session, or a base session the
/// run shares with the tree and editors (a private in-memory database), whose guard the run
/// takes while it uses it.
#[derive(Clone)]
struct Work {
    handle: SessionHandle,
    shared: bool,
}

/// Work sessions of one run, by (base session, database), closed at the end.
struct Sessions<'a> {
    ctx: &'a Ctx,
    open: HashMap<(SessionId, Option<String>), WorkSession>,
}

impl<'a> Sessions<'a> {
    fn new(ctx: &'a Ctx) -> Self {
        Self {
            ctx,
            open: HashMap::new(),
        }
    }

    async fn get(
        &mut self,
        base: SessionId,
        database: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Work> {
        let key = (base, database.map(str::to_owned));
        if let Some(open) = self.open.get(&key) {
            return Ok(Work {
                handle: open.handle.clone(),
                shared: !open.dedicated,
            });
        }
        let opened = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(Error::Cancelled),
            r = work_session(&self.ctx.manager, &self.ctx.dialects, base, database, cancel) => r,
        }?;
        let work = Work {
            handle: opened.handle.clone(),
            shared: !opened.dedicated,
        };
        self.open.insert(key, opened);
        Ok(work)
    }

    async fn close(self) {
        for (_, session) in self.open {
            close_work_session(&self.ctx.manager, session, CLOSE_WAIT).await;
        }
    }
}

/// Takes the guard of a shared session once per run step (see [`Work`]).
async fn guard(
    work: &Work,
    held: &mut Vec<(SessionId, SessionGuard)>,
    cancel: &CancellationToken,
) -> Result<()> {
    if !work.shared || held.iter().any(|(id, _)| *id == work.handle.id()) {
        return Ok(());
    }
    let g = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(Error::Cancelled),
        g = work.handle.exclusive() => g,
    };
    held.push((work.handle.id(), g));
    Ok(())
}

/// Loads table `obj` through the engine's table fetcher.
async fn load_table(
    ctx: &Ctx,
    engine: Engine,
    session: &Work,
    obj: &oxbase_types::ObjectRef,
    cancel: &CancellationToken,
) -> Result<TableDef> {
    let fetcher = ctx.dialects.fetcher(engine, ObjectKind::Table)?;
    match fetcher
        .load(session.handle.connection().as_ref(), obj, cancel.clone())
        .await?
    {
        ObjectDef::Table(t) => Ok(t),
        _ => Err(Error::invalid_input(format!(
            "{} is not a table",
            obj.display_name()
        ))),
    }
}

/// Compares every pair of `request` in order into `store`. Returns the kept compare (with the
/// tables done so far) and the error that stopped the compare, if any.
#[allow(
    clippy::too_many_arguments,
    reason = "one call site per run kind; the arguments are the run's context"
)]
async fn run_compare<F, Fut>(
    ctx: &Ctx,
    id: OperationId,
    request: &DataCompareRequest,
    engine: Engine,
    version: ServerVersion,
    mut store: DiffStore,
    token: &CancellationToken,
    on_progress: &(dyn Fn(&oxbase_diff::CompareProgress) + Send + Sync),
    on_table: F,
) -> (DataCompare, Option<Error>)
where
    F: Fn(&TableDataDiff) -> Fut,
    Fut: Future<Output = ()>,
{
    let started = Instant::now();
    let mut pairs: Vec<KeptPair> = request
        .pairs
        .iter()
        .map(|p| KeptPair {
            pair: p.clone(),
            target: None,
        })
        .collect();
    let mut tables = Vec::with_capacity(request.pairs.len());
    let mut error = None;
    let mut sessions = Sessions::new(ctx);
    for (index, pair) in request.pairs.iter().enumerate() {
        if token.is_cancelled() {
            error = Some(Error::Cancelled);
            break;
        }
        match compare_pair(
            ctx,
            &mut sessions,
            &mut store,
            index,
            pair,
            request,
            engine,
            &version,
            token,
            on_progress,
        )
        .await
        {
            Ok((diff, target)) => {
                pairs[index].target = target;
                on_table(&diff).await;
                let stop = diff.status == TableCompareStatus::Cancelled;
                tables.push(diff);
                if stop {
                    error = Some(Error::Cancelled);
                    break;
                }
            }
            Err(e) => {
                error = Some(e);
                break;
            }
        }
    }
    sessions.close().await;
    if let Err(e) = store.finish() {
        error.get_or_insert(e);
    }
    let result = DataCompareResult {
        compare_id: id,
        tables,
        elapsed_ms: elapsed_ms(started),
    };
    (
        DataCompare {
            id,
            engine,
            version,
            source_session: request.source_session,
            target_session: request.target_session,
            options: request.options.clone(),
            pairs,
            result,
            store,
            applied: AtomicBool::new(false),
        },
        error,
    )
}

/// One pair: sessions, both definitions, `compare_table`. A table that cannot be loaded is
/// refused (in the result); `Err` only for a broken store or a cancel before the compare.
#[allow(
    clippy::too_many_arguments,
    reason = "one call site; the arguments are the run's context"
)]
async fn compare_pair(
    ctx: &Ctx,
    sessions: &mut Sessions<'_>,
    store: &mut DiffStore,
    index: usize,
    pair: &TablePair,
    request: &DataCompareRequest,
    engine: Engine,
    version: &ServerVersion,
    token: &CancellationToken,
    on_progress: &(dyn Fn(&oxbase_diff::CompareProgress) + Send + Sync),
) -> Result<(TableDataDiff, Option<Compared>)> {
    let started = Instant::now();
    let refused = |reason: String| TableDataDiff {
        pair: index,
        source: pair.source.clone(),
        target: pair.target.clone(),
        status: TableCompareStatus::Refused { reason },
        key: Vec::new(),
        columns: Vec::new(),
        counts: oxbase_diff::RowCounts::default(),
        samples: Vec::new(),
        chunks_skipped: 0,
        warnings: Vec::new(),
        elapsed_ms: elapsed_ms(started),
    };
    let source = match sessions
        .get(request.source_session, pair.source.database_name(), token)
        .await
    {
        Ok(s) => s,
        Err(e) if e.is_cancelled() => return Err(e),
        Err(e) => return Ok((refused(format!("cannot open the source: {e}")), None)),
    };
    let target = match sessions
        .get(request.target_session, pair.target.database_name(), token)
        .await
    {
        Ok(s) => s,
        Err(e) if e.is_cancelled() => return Err(e),
        Err(e) => return Ok((refused(format!("cannot open the target: {e}")), None)),
    };
    let mut held = Vec::new();
    guard(&source, &mut held, token).await?;
    guard(&target, &mut held, token).await?;
    let source_def = match load_table(ctx, engine, &source, &pair.source, token).await {
        Ok(t) => t,
        Err(e) if e.is_cancelled() => return Err(e),
        Err(e) => return Ok((refused(format!("source table: {e}")), None)),
    };
    let target_def = match load_table(ctx, engine, &target, &pair.target, token).await {
        Ok(t) => t,
        Err(e) if e.is_cancelled() => return Err(e),
        Err(e) => return Ok((refused(format!("target table: {e}")), None)),
    };
    let job = CompareJob {
        dialects: &ctx.dialects,
        engine,
        version,
        pair: index,
        source: source.handle.connection().as_ref(),
        target: target.handle.connection().as_ref(),
        source_table: &source_def,
        target_table: &target_def,
        choice: pair.key.as_ref(),
        columns: pair.columns.as_deref(),
        options: &request.options,
    };
    let diff = compare_table(job, store, on_progress, token.clone()).await?;
    drop(held);
    let kept = (diff.status == TableCompareStatus::Compared).then(|| Compared {
        key: row_key(&target_def, &diff.key, pair.key.as_ref()),
        columns: diff.columns.clone(),
        table: target_def,
    });
    Ok((diff, kept))
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// --- order and script ------------------------------------------------------------------------

/// The (pair, op) groups a script or apply runs, in order: deletes children-first, updates,
/// inserts parents-first. Only compared pairs of the selection; ops the options switch off
/// are left out.
fn groups(entry: &DataCompare, options: &DataScriptOptions) -> Result<Vec<(usize, RowOp)>> {
    for &p in &options.pairs {
        if p >= entry.pairs.len() {
            return Err(Error::invalid_input(format!(
                "the compare has no pair {p} (it has {})",
                entry.pairs.len()
            )));
        }
    }
    let selected: Vec<usize> = (0..entry.pairs.len())
        .filter(|p| options.pairs.is_empty() || options.pairs.contains(p))
        .filter(|&p| entry.pairs[p].target.is_some())
        .collect();
    // Parents first over the target tables' foreign keys.
    let defs: Vec<TableDef> = selected
        .iter()
        .filter_map(|&p| entry.pairs[p].compared().map(|(t, _)| t.clone()))
        .collect();
    let order = table_dependencies(&defs).order();
    let mut parents_first: Vec<usize> = Vec::with_capacity(selected.len());
    for obj in &order.order {
        for &p in &selected {
            if !parents_first.contains(&p)
                && entry.pairs[p]
                    .compared()
                    .is_some_and(|(t, _)| &t.obj == obj)
            {
                parents_first.push(p);
            }
        }
    }
    for &p in &selected {
        if !parents_first.contains(&p) {
            parents_first.push(p);
        }
    }
    let mut out = Vec::new();
    if options.deletes {
        out.extend(parents_first.iter().rev().map(|&p| (p, RowOp::Delete)));
    }
    if options.updates {
        out.extend(selected.iter().map(|&p| (p, RowOp::Update)));
    }
    if options.inserts {
        out.extend(parents_first.iter().map(|&p| (p, RowOp::Insert)));
    }
    Ok(out)
}

/// The rows the groups hold.
fn total_rows(entry: &DataCompare, groups: &[(usize, RowOp)]) -> u64 {
    groups.iter().map(|&(p, op)| entry.store.rows(p, op)).sum()
}

/// The session statements around the inserts of each insert group with rows (by pair).
fn insert_sessions(
    ctx: &Ctx,
    entry: &DataCompare,
    groups: &[(usize, RowOp)],
) -> Result<HashMap<usize, InsertSession>> {
    let mut out = HashMap::new();
    for &(p, op) in groups {
        if op != RowOp::Insert || entry.store.rows(p, op) == 0 {
            continue;
        }
        let Some(compared) = entry.pairs[p].target.as_ref() else {
            continue;
        };
        let session = insert_session(
            &ctx.dialects,
            entry.engine,
            &compared.table,
            &compared.columns,
        )?;
        if !session.is_empty() {
            out.insert(p, session);
        }
    }
    Ok(out)
}

/// Every statement a script or apply of `groups` runs: the rows plus the insert sessions.
fn total_statements(
    entry: &DataCompare,
    groups: &[(usize, RowOp)],
    sessions: &HashMap<usize, InsertSession>,
) -> u64 {
    let extra: usize = sessions.values().map(InsertSession::len).sum();
    total_rows(entry, groups) + u64::try_from(extra).unwrap_or(u64::MAX)
}

/// Warnings every script and apply of `entry` carries.
fn script_warnings(
    ctx: &Ctx,
    entry: &DataCompare,
    options: &DataScriptOptions,
) -> Result<Vec<String>> {
    let writer = ctx.dialects.dml_writer(entry.engine)?;
    let mut warnings = Vec::new();
    for (p, kept) in entry.pairs.iter().enumerate() {
        if !(options.pairs.is_empty() || options.pairs.contains(&p)) {
            continue;
        }
        let Some((table, _)) = kept.compared() else {
            warnings.push(format!(
                "{} was not compared; it is not in the script",
                kept.pair.target.display_name()
            ));
            continue;
        };
        let name = table.obj.display_name();
        let deletes = entry.store.rows(p, RowOp::Delete);
        if deletes > 0 && !options.deletes {
            warnings.push(format!(
                "{name}: {deletes} rows only in the target are kept (deletes are off)"
            ));
        }
        if !writer.transactional(table) {
            warnings.push(format!(
                "{name}: the table's storage cannot roll back; statements that ran before a \
                 failure stay applied"
            ));
        }
    }
    Ok(warnings)
}

/// The session statements around a script preview, each ended with `terminator`: the engine's
/// SQL dump session ([`Quoting::dump_session`]). The preview inlines its values with
/// `Quoting::dump_literal`, whose escaping is right only in that session (PostgreSQL
/// `standard_conforming_strings = on`; MySQL / MariaDB `sql_mode` without
/// `NO_BACKSLASH_ESCAPES` and `SET NAMES utf8mb4`), so a script copied into a client whose
/// session differs would read a backslash in a value differently and could end a string
/// early. `zoned`: a value is a timestamp with a time zone.
fn script_session(quoting: &dyn Quoting, zoned: bool) -> (Vec<String>, Vec<String>) {
    let terminator = quoting.script_terminator();
    let session = quoting.dump_session(zoned);
    let end = |statements: Vec<String>| -> Vec<String> {
        statements
            .into_iter()
            .map(|s| format!("{s}{terminator}"))
            .collect()
    };
    (end(session.prologue), end(session.epilogue))
}

/// Renders the script preview of `entry`.
pub(super) fn script(
    ctx: &Ctx,
    entry: &DataCompare,
    options: &DataScriptOptions,
    cancel: &CancellationToken,
) -> Result<DataScript> {
    let groups = groups(entry, options)?;
    let quoting = ctx.dialects.quoting(entry.engine)?;
    let terminator = quoting.script_terminator();
    let sessions = insert_sessions(ctx, entry, &groups)?;
    let total = total_statements(entry, &groups, &sessions);
    let limit = options.preview_limit;
    let mut statements = Vec::new();
    let zoned = Cell::new(false);
    let render = |statement: &BoundStatement| -> Result<String> {
        if statement
            .params
            .iter()
            .any(|v| matches!(v, Value::TimestampTz(_)))
        {
            zoned.set(true);
        }
        let text = inline_params(quoting.as_ref(), statement)?;
        Ok(format!("{text}{terminator}"))
    };
    'groups: for &(p, op) in &groups {
        if statements.len() >= limit {
            break;
        }
        let Some((table, key)) = entry.pairs[p].compared() else {
            continue;
        };
        let session = sessions.get(&p).filter(|_| op == RowOp::Insert);
        for statement in session.map_or(&[][..], |s| s.before.as_slice()) {
            if statements.len() >= limit {
                break 'groups;
            }
            statements.push(render(statement)?);
        }
        for batch in entry.store.read(p, op, STORE_BATCH_ROWS)? {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let batch = batch?;
            for statement in data_statements(
                &ctx.dialects,
                entry.engine,
                &entry.version,
                table,
                key,
                op,
                &batch,
            )? {
                if statements.len() >= limit {
                    break 'groups;
                }
                statements.push(render(&statement)?);
            }
        }
        for statement in session.map_or(&[][..], |s| s.after.as_slice()) {
            if statements.len() >= limit {
                break 'groups;
            }
            statements.push(render(statement)?);
        }
    }
    let (prologue, epilogue) = script_session(quoting.as_ref(), zoned.get());
    let destructive = options.deletes
        && groups
            .iter()
            .any(|&(p, op)| op == RowOp::Delete && entry.store.rows(p, op) > 0);
    Ok(DataScript {
        engine: entry.engine,
        truncated: u64::try_from(statements.len()).unwrap_or(u64::MAX) < total,
        prologue,
        statements,
        epilogue,
        total_statements: total,
        destructive,
        warnings: script_warnings(ctx, entry, options)?,
    })
}

/// Checks an apply before it starts; returns the number of statements it runs.
pub(super) fn check_apply(
    ctx: &Ctx,
    entry: &DataCompare,
    options: &DataApplyOptions,
) -> Result<u64> {
    let groups = groups(entry, &options.script)?;
    let deletes: u64 = groups
        .iter()
        .filter(|(_, op)| *op == RowOp::Delete)
        .map(|&(p, op)| entry.store.rows(p, op))
        .sum();
    if options.script.deletes && !options.confirm_deletes {
        return Err(Error::invalid_input(format!(
            "the script deletes {deletes} rows from the target; confirm the deletes to apply it"
        )));
    }
    if options.batch_statements == 0 {
        return Err(Error::invalid_input("batchStatements must be at least 1"));
    }
    if entry.applied.load(Ordering::SeqCst) {
        return Err(Error::invalid_input(format!(
            "data compare {} was already applied; compare again before another apply",
            entry.id
        )));
    }
    let sessions = insert_sessions(ctx, entry, &groups)?;
    Ok(total_statements(entry, &groups, &sessions))
}

/// Marks `entry` applied; `InvalidInput` when another apply got there first.
pub(super) fn mark_applied(entry: &DataCompare) -> Result<()> {
    if entry.applied.swap(true, Ordering::SeqCst) {
        return Err(Error::invalid_input(format!(
            "data compare {} was already applied; compare again before another apply",
            entry.id
        )));
    }
    Ok(())
}

// --- apply -----------------------------------------------------------------------------------

/// The open transaction of an apply and what it holds so far.
struct Batch {
    session: SessionId,
    tx: Box<dyn Tx>,
    statements: u64,
    /// Rows per pair in this transaction: (inserted, updated, deleted).
    pending: HashMap<usize, (u64, u64, u64)>,
}

/// What stopped an apply.
enum ApplyStop {
    Cancelled,
    Failed(Error),
}

/// The apply task: `Started`, `Progress`, `Recomparing`, one terminal event.
#[allow(
    clippy::needless_pass_by_value,
    reason = "a spawned task owns its inputs"
)]
pub(super) async fn apply_task(
    ctx: Ctx,
    ticket: RunTicket,
    entry: Arc<DataCompare>,
    options: DataApplyOptions,
    total: u64,
    events: mpsc::Sender<DataApplyEvent>,
) {
    let started = Instant::now();
    let token = ticket.token().clone();
    let _ = events
        .send(DataApplyEvent::Started {
            apply_id: ticket.id(),
            total_statements: total,
        })
        .await;
    let groups = groups(&entry, &options.script).unwrap_or_default();
    let mut applied: Vec<usize> = groups.iter().map(|&(p, _)| p).collect();
    applied.sort_unstable();
    applied.dedup();
    let mut report = DataApplyReport {
        tables: applied
            .into_iter()
            .map(|p| TableApplyReport {
                pair: p,
                target: entry.pairs[p].pair.target.clone(),
                inserted: 0,
                updated: 0,
                deleted: 0,
                error: None,
            })
            .collect(),
        statements: 0,
        committed_batches: 0,
        left_behind: String::new(),
        recompare: None,
        in_sync: false,
        elapsed_ms: 0,
    };
    let mut sessions = Sessions::new(&ctx);
    let outcome = run_apply(
        &ctx,
        &entry,
        &options,
        &groups,
        total,
        &token,
        &mut sessions,
        &mut report,
        &events,
        started,
    )
    .await;
    sessions.close().await;
    let terminal = match outcome {
        Ok(()) => {
            report.left_behind = format!(
                "every statement was applied ({} in {} transactions)",
                report.statements, report.committed_batches
            );
            let _ = events.send(DataApplyEvent::Recomparing).await;
            match recompare(&ctx, &entry, &options, &groups, &token).await {
                Ok(result) => {
                    report.in_sync = in_sync(&result, &options.script);
                    report.recompare = Some(result);
                    report.elapsed_ms = elapsed_ms(started);
                    DataApplyEvent::Finished { report }
                }
                Err(e) if e.is_cancelled() => {
                    report.elapsed_ms = elapsed_ms(started);
                    DataApplyEvent::Cancelled { report }
                }
                Err(e) => {
                    report.elapsed_ms = elapsed_ms(started);
                    DataApplyEvent::Failed {
                        error: ErrorInfo::from(&e),
                        report,
                    }
                }
            }
        }
        Err(stop) => {
            report.left_behind = left_behind(&ctx, &entry, &report);
            report.elapsed_ms = elapsed_ms(started);
            match stop {
                ApplyStop::Cancelled => DataApplyEvent::Cancelled { report },
                ApplyStop::Failed(e) => DataApplyEvent::Failed {
                    error: ErrorInfo::from(&e),
                    report,
                },
            }
        }
    };
    let _ = events.send(terminal).await;
    drop(ticket);
}

fn left_behind(ctx: &Ctx, entry: &DataCompare, report: &DataApplyReport) -> String {
    let mut text = if report.committed_batches == 0 {
        "nothing was changed: the open transaction was rolled back".to_owned()
    } else {
        format!(
            "{} statements in {} committed transactions stay applied; the open transaction was \
             rolled back",
            report.statements, report.committed_batches
        )
    };
    if let Ok(writer) = ctx.dialects.dml_writer(entry.engine) {
        let lossy: Vec<String> = report
            .tables
            .iter()
            .filter_map(|t| entry.pairs[t.pair].compared())
            .filter(|(table, _)| !writer.transactional(table))
            .map(|(table, _)| table.obj.display_name())
            .collect();
        if !lossy.is_empty() {
            let _ = write!(
                text,
                " (writes to {} cannot be rolled back and stay applied)",
                lossy.join(", ")
            );
        }
    }
    text
}

/// `true` when every pair of the re-compare was compared and has no differences of the
/// kinds that were applied.
fn in_sync(result: &DataCompareResult, options: &DataScriptOptions) -> bool {
    result.tables.iter().all(|t| {
        t.status == TableCompareStatus::Compared
            && (!options.inserts || t.counts.inserts == 0)
            && (!options.updates || t.counts.updates == 0)
            && (!options.deletes || t.counts.deletes == 0)
    })
}

/// Compares the applied pairs again (a new kept compare).
async fn recompare(
    ctx: &Ctx,
    entry: &DataCompare,
    options: &DataApplyOptions,
    groups: &[(usize, RowOp)],
    token: &CancellationToken,
) -> Result<DataCompareResult> {
    let mut applied: Vec<usize> = groups.iter().map(|&(p, _)| p).collect();
    applied.sort_unstable();
    applied.dedup();
    if applied.is_empty() {
        applied = (0..entry.pairs.len())
            .filter(|p| options.script.pairs.is_empty() || options.script.pairs.contains(p))
            .collect();
    }
    let request = DataCompareRequest {
        source_session: entry.source_session,
        target_session: entry.target_session,
        pairs: applied
            .iter()
            .map(|&p| entry.pairs[p].pair.clone())
            .collect(),
        options: entry.options.clone(),
    };
    let id = OperationId::new();
    let store = DiffStore::new(None)?;
    let (kept, error) = run_compare(
        ctx,
        id,
        &request,
        entry.engine,
        entry.version.clone(),
        store,
        token,
        &|_: &oxbase_diff::CompareProgress| {},
        |_: &TableDataDiff| std::future::ready(()),
    )
    .await;
    let result = kept.result.clone();
    ctx.kept.keep(kept);
    match error {
        Some(e) => Err(e),
        None => Ok(result),
    }
}

/// One statement of an apply: a row statement (`op`, must affect one row) or a session
/// statement around inserts (`op` `None`).
#[derive(Clone, Copy)]
struct Step<'a> {
    pair: usize,
    op: Option<RowOp>,
    table: &'a TableDef,
}

/// The running state of an apply: the open transaction, counts and progress.
struct Applier<'a> {
    report: &'a mut DataApplyReport,
    events: &'a mpsc::Sender<DataApplyEvent>,
    token: &'a CancellationToken,
    open: Option<Batch>,
    batch_size: u64,
    done: u64,
    total: u64,
    last_progress: Instant,
    started: Instant,
    /// What the failing statement was, for the table's error.
    current: Option<String>,
}

impl Applier<'_> {
    /// Runs `statement` on `session` inside the open transaction (begun on first use, committed
    /// every `batch_size` statements or when the session changes).
    async fn run(
        &mut self,
        session: &Work,
        statement: &BoundStatement,
        step: Step<'_>,
    ) -> Result<()> {
        if self.token.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let other_session = self
            .open
            .as_ref()
            .is_some_and(|b| b.session != session.handle.id());
        if other_session && let Some(batch) = self.open.take() {
            commit(batch, self.report).await?;
        }
        if self.open.is_none() {
            self.open = Some(Batch {
                session: session.handle.id(),
                tx: session.handle.connection().begin().await?,
                statements: 0,
                pending: HashMap::new(),
            });
        }
        let Some(batch) = self.open.as_mut() else {
            return Err(Error::internal("no open transaction"));
        };
        let number = self.done + 1;
        self.current = Some(match step.op {
            Some(op) => format!(
                "statement {number} ({} {})",
                op_words(op),
                step.table.obj.display_name()
            ),
            None => format!(
                "statement {number} (a session statement for the inserts into {})",
                step.table.obj.display_name()
            ),
        });
        match step.op {
            Some(op) => {
                execute_one(batch.tx.as_ref(), statement, self.token, step.table, number).await?;
                let counts = batch.pending.entry(step.pair).or_default();
                match op {
                    RowOp::Insert => counts.0 += 1,
                    RowOp::Update => counts.1 += 1,
                    RowOp::Delete => counts.2 += 1,
                }
            }
            None => {
                collect(
                    batch
                        .tx
                        .execute(&statement.sql, &statement.params, self.token.clone())
                        .await?,
                )
                .await?;
            }
        }
        self.current = None;
        batch.statements += 1;
        self.done += 1;
        if batch.statements >= self.batch_size
            && let Some(batch) = self.open.take()
        {
            commit(batch, self.report).await?;
        }
        if self.last_progress.elapsed() >= PROGRESS_EVERY {
            self.last_progress = Instant::now();
            let _ = self.events.try_send(DataApplyEvent::Progress {
                statements_done: self.done,
                total_statements: self.total,
                pair: step.pair,
                elapsed_ms: elapsed_ms(self.started),
            });
        }
        Ok(())
    }
}

fn op_words(op: RowOp) -> &'static str {
    match op {
        RowOp::Insert => "an insert into",
        RowOp::Update => "an update of",
        RowOp::Delete => "a delete from",
    }
}

/// Runs every statement of `groups` (module docs, *Apply*).
#[allow(
    clippy::too_many_arguments,
    reason = "one call site; the arguments are the run's context"
)]
async fn run_apply(
    ctx: &Ctx,
    entry: &DataCompare,
    options: &DataApplyOptions,
    groups: &[(usize, RowOp)],
    total: u64,
    token: &CancellationToken,
    sessions: &mut Sessions<'_>,
    report: &mut DataApplyReport,
    events: &mpsc::Sender<DataApplyEvent>,
    started: Instant,
) -> std::result::Result<(), ApplyStop> {
    let mut held = Vec::new();
    let mut current: Option<usize> = None;
    // The closing statements of an insert group whose opening ones ran (run after a failure).
    let mut closing: Option<(Work, Vec<BoundStatement>)> = None;
    let mut applier = Applier {
        report: &mut *report,
        events,
        token,
        open: None,
        batch_size: u64::try_from(options.batch_statements.max(1)).unwrap_or(u64::MAX),
        done: 0,
        total,
        last_progress: Instant::now(),
        started,
        current: None,
    };
    let result: Result<()> = async {
        let wrapping = insert_sessions(ctx, entry, groups)?;
        for &(p, op) in groups {
            let Some((table, key)) = entry.pairs[p].compared() else {
                continue;
            };
            current = Some(p);
            let session = sessions
                .get(
                    entry.target_session,
                    entry.pairs[p].pair.target.database_name(),
                    token,
                )
                .await?;
            guard(&session, &mut held, token).await?;
            let around = wrapping.get(&p).filter(|_| op == RowOp::Insert);
            let plain = Step {
                pair: p,
                op: None,
                table,
            };
            if let Some(around) = around {
                closing = Some((session.clone(), around.after.clone()));
                for statement in &around.before {
                    applier.run(&session, statement, plain).await?;
                }
            }
            let row = Step {
                pair: p,
                op: Some(op),
                table,
            };
            for batch in entry.store.read(p, op, STORE_BATCH_ROWS)? {
                let statements = data_statements(
                    &ctx.dialects,
                    entry.engine,
                    &entry.version,
                    table,
                    key,
                    op,
                    &batch?,
                )?;
                for statement in &statements {
                    applier.run(&session, statement, row).await?;
                }
            }
            if let Some(around) = around {
                for statement in &around.after {
                    applier.run(&session, statement, plain).await?;
                }
                closing = None;
            }
        }
        if let Some(batch) = applier.open.take() {
            commit(batch, applier.report).await?;
        }
        Ok(())
    }
    .await;
    if let Some(batch) = applier.open.take()
        && let Err(e) = batch.tx.rollback().await
    {
        tracing::debug!(category = %e.category(), "rolling back a data sync batch failed");
    }
    if result.is_err()
        && let Some((session, statements)) = closing.take()
    {
        // Best effort, outside any transaction: undo session settings (identity inserts).
        for statement in &statements {
            let ran = async {
                collect(
                    session
                        .handle
                        .connection()
                        .execute(&statement.sql, &statement.params, CancellationToken::new())
                        .await?,
                )
                .await
            };
            if let Err(e) = tokio::time::timeout(CLOSE_WAIT, ran)
                .await
                .unwrap_or(Err(Error::Cancelled))
            {
                tracing::debug!(category = %e.category(), "a closing data sync statement failed");
            }
        }
    }
    drop(held);
    let failing = applier.current.take();
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.is_cancelled() || token.is_cancelled() => Err(ApplyStop::Cancelled),
        Err(e) => {
            if let Some(t) = current.and_then(|p| report.tables.iter_mut().find(|t| t.pair == p)) {
                t.error = Some(table_error(failing.as_deref(), &e));
            }
            Err(ApplyStop::Failed(e))
        }
    }
}

/// The table error of a failed apply: which statement, the server's message, and for a
/// foreign-key failure why the order may matter.
fn table_error(failing: Option<&str>, e: &Error) -> String {
    let message = ErrorInfo::from(e).message;
    let mut text = match failing {
        Some(what) => format!("{what}: {message}"),
        None => message,
    };
    if text.to_ascii_lowercase().contains("foreign key") {
        text.push_str(
            " (statements run as deletes, then updates, then inserts: a row that refers to a \
             row this sync inserts later, or a row still referred to by a row it updates later, \
             fails its foreign key; apply the tables in separate steps)",
        );
    }
    text
}

/// Commits `batch` and moves its counts into `report`.
async fn commit(batch: Batch, report: &mut DataApplyReport) -> Result<()> {
    let Batch {
        tx,
        statements,
        pending,
        ..
    } = batch;
    tx.commit().await?;
    report.statements += statements;
    report.committed_batches += 1;
    for (pair, (i, u, d)) in pending {
        if let Some(t) = report.tables.iter_mut().find(|t| t.pair == pair) {
            t.inserted += i;
            t.updated += u;
            t.deleted += d;
        }
    }
    Ok(())
}

/// Runs one statement; it must affect exactly one row.
async fn execute_one(
    tx: &dyn Tx,
    statement: &BoundStatement,
    token: &CancellationToken,
    table: &TableDef,
    number: u64,
) -> Result<()> {
    let collected = collect(
        tx.execute(&statement.sql, &statement.params, token.clone())
            .await?,
    )
    .await?;
    let affected: u64 = if collected.result_sets.is_empty() {
        collected.rows_affected.iter().sum()
    } else {
        collected
            .result_sets
            .iter()
            .map(|set| u64::try_from(set.num_rows()).unwrap_or(u64::MAX))
            .sum()
    };
    if affected == 1 {
        Ok(())
    } else {
        Err(Error::invalid_input(format!(
            "statement {number} on {} affected {affected} rows instead of 1 (the target \
             changed since the compare); its transaction was rolled back and the apply stopped",
            table.obj.display_name()
        )))
    }
}

#[cfg(test)]
mod tests {
    use oxbase_dialect::{AnsiQuoting, BacktickQuoting, BracketQuoting, PgQuoting};
    use pretty_assertions::assert_eq;

    use super::*;

    /// Audit lead 4: the preview inlines `dump_literal` values, so it carries the dump session
    /// that makes them read back as written.
    #[test]
    fn script_previews_carry_the_dump_session() {
        let (prologue, epilogue) = script_session(&PgQuoting, false);
        assert_eq!(prologue, ["SET standard_conforming_strings = on;"]);
        assert_eq!(epilogue, Vec::<String>::new());

        let (prologue, epilogue) = script_session(&BacktickQuoting, false);
        assert!(
            prologue.iter().any(|s| s.contains("NO_BACKSLASH_ESCAPES")),
            "{prologue:?}"
        );
        assert_eq!(
            prologue.last().map(String::as_str),
            Some("SET NAMES utf8mb4;")
        );
        assert_eq!(
            epilogue.last().map(String::as_str),
            Some("SET sql_mode = @oxbase_sql_mode;")
        );
        assert!(!prologue.iter().any(|s| s.contains("time_zone")));
        let (zoned, _) = script_session(&BacktickQuoting, true);
        assert!(
            zoned.iter().any(|s| s == "SET time_zone = '+00:00';"),
            "{zoned:?}"
        );

        assert_eq!(script_session(&AnsiQuoting, true), (Vec::new(), Vec::new()));
        assert_eq!(
            script_session(&BracketQuoting, true),
            (Vec::new(), Vec::new())
        );
    }
}
