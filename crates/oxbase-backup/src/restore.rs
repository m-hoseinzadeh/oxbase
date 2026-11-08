//! Restore into the same engine (`docs/m8-contracts.md` §7.3), by the **backup** stream.
//!
//! # Steps ([`restore`])
//!
//! 1. Open the archive (`ArchiveReader::open`: manifest validated); refuse another engine
//!    (`InvalidInput`: "the backup is from PostgreSQL, the target is MySQL"); verify the
//!    checksums of the selected entries first unless [`RestoreOptions::verify_first`] is off.
//! 2. **Plan** (reads only): every definition is moved from the backed-up container to the
//!    target (`relocate_def`, SQL text through `oxbase_diff::retarget_sql`); a definition that
//!    does not then lie in the target container is refused (a damaged or forged archive never
//!    writes elsewhere). The target's objects of the same kind and name are found through the
//!    engine's fetchers.
//! 3. **Structure** (content `structure_and_data` / `structure_only`). Deviation from the
//!    contract text (`load_snapshot` + `compare` + `sync_script`): the restore plans its DDL
//!    itself from the archive's definitions, with the engine's `Fetcher`, `Comparator` and
//!    `DdlGenerator` cells, because a restore only ever creates or drops whole objects. An
//!    object that exists and is not dropped ([`RestoreOptions::drop_existing`]) is kept when it
//!    compares equal and refused when it differs ("restore with drop existing"). Before the
//!    rows: drops of replaced objects (dependents first), then creates in the archive's
//!    creation order, tables without the foreign keys that point at themselves or at a table
//!    created later. After the rows: those foreign keys, then materialized views, triggers and
//!    every view or routine that names one of them (`objects::after_data`). Each part runs as
//!    one script through the services' `execute_script` (one transaction where the engine has
//!    transactional DDL). The target container itself must exist.
//! 4. **Data** (content `structure_and_data` / `data_only`). A table that existed and is not
//!    dropped must be empty unless [`RestoreOptions::truncate_before_load`] (it is emptied with
//!    `DELETE`, every such table in one transaction, children first) or
//!    [`RestoreOptions::append_rows`] is set; otherwise the restore is refused before it writes
//!    anything. Then per selected table in creation order (parents first): its batches stream
//!    from the archive into the engine's `BulkLoader` (they are already in the load schema),
//!    one transaction per table, foreign-key checks off where the engine allows
//!    (`BulkLoader::foreign_key_checks`) for existing tables on a foreign-key cycle or with a
//!    foreign key to themselves, identity values kept (`BulkLoader::prepare` /
//!    `identity_resync`).
//! 5. **Sequences**: the archive does not hold a sequence's position, so each sequence this
//!    restore created restarts after the highest restored value of the columns whose default
//!    draws from it (`ALTER SEQUENCE ... RESTART WITH`, SQL standard).
//! 6. Report per object (created, replaced, kept, loaded, failed) and what a failure or cancel
//!    left: structure steps follow the script rules (rolled back where atomic); data per table
//!    (a failed table rolls back; earlier tables stay).
//!
//! Destructive restores (drop existing, truncate) need the caller's confirmation
//! ([`RestoreOptions::confirm_destructive`]); the services refuse without it.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arrow_array::RecordBatch;
use arrow_schema::{DataType as ArrowType, Schema};
use futures::StreamExt;
use oxbase_dialect::bulk::load_arrow_type;
use oxbase_dialect::deps::same_table;
use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{
    BulkLoader, DdlContext, DialectRegistry, LoadColumn, LoadSession, LoadTarget,
    table_dependencies,
};
use oxbase_driver::{CancellationToken, Connection, ResultItem, Tx, cell_value};
use oxbase_types::{
    BoundStatement, DataType, Engine, Error, Location, ObjectDef, ObjectKind, ObjectRef, Result,
    Script, SequenceDef, TableDef, Value, relocate_def,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::archive::ArchiveReader;
use crate::backup::{elapsed_ms, until_cancelled};
use crate::format::{Manifest, ManifestObject};
use crate::objects::{after_data, late_order, mentions, retarget_text};

/// What a restore writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreContent {
    #[default]
    StructureAndData,
    StructureOnly,
    DataOnly,
}

/// How to restore. Every field but `container` has a default (camelCase JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreOptions {
    /// The existing database or schema to restore into (may differ from the backed-up one).
    pub container: ObjectRef,
    #[serde(default)]
    pub content: RestoreContent,
    /// Manifest object indexes to restore (empty: all).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<u32>,
    /// Drop selected objects that exist in the target before creating them. Destructive.
    #[serde(default)]
    pub drop_existing: bool,
    /// Empty existing tables before loading (data only). Destructive.
    #[serde(default)]
    pub truncate_before_load: bool,
    /// Add the rows to existing tables that already hold rows (without it, such a restore is
    /// refused: keyless tables would get duplicate rows, keyed ones fail part way).
    #[serde(default)]
    pub append_rows: bool,
    /// Check the selected entries' checksums before writing anything.
    #[serde(default = "yes")]
    pub verify_first: bool,
    /// The user confirmed a destructive restore.
    #[serde(default)]
    pub confirm_destructive: bool,
    /// Rows per load call (default: the loader's own `rows_per_call`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_rows: Option<usize>,
}

fn yes() -> bool {
    true
}

impl RestoreOptions {
    /// Restore everything into `container` with the defaults.
    pub fn into_container(container: ObjectRef) -> Self {
        Self {
            container,
            content: RestoreContent::StructureAndData,
            objects: Vec::new(),
            drop_existing: false,
            truncate_before_load: false,
            append_rows: false,
            verify_first: true,
            confirm_destructive: false,
            batch_rows: None,
        }
    }

    /// `true` when the restore can destroy existing data.
    pub fn is_destructive(&self) -> bool {
        self.drop_existing || self.truncate_before_load
    }
}

/// Cumulative restore progress.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreProgress {
    /// `verify`, `structure` or `data`.
    pub phase: String,
    pub objects_done: u32,
    pub objects_total: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<ObjectRef>,
    pub rows: u64,
    pub rows_total: u64,
    pub elapsed_ms: u64,
}

/// What happened to one object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoredStatus {
    Created,
    /// Dropped and created again.
    Replaced,
    /// Existed and was equal: left as it is.
    Kept,
    /// Rows loaded into an existing table (data only).
    Loaded,
    Failed,
    NotRun,
}

/// One object's outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoredObject {
    pub index: u32,
    /// The object as created in the target container.
    pub object: ObjectRef,
    pub status: RestoredStatus,
    #[serde(default)]
    pub rows: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The outcome of a restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreReport {
    pub objects: Vec<RestoredObject>,
    pub rows: u64,
    /// What a failure or cancel left behind, in words.
    pub left_behind: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub elapsed_ms: u64,
}

/// Runs a structure script on the restore's target session with the `oxbase_types::script`
/// semantics; `Ok(true)` when it committed. The services pass `execute_script` on a dedicated
/// child session.
pub type ScriptRunner =
    dyn Fn(Script) -> futures::future::BoxFuture<'static, Result<bool>> + Send + Sync;

/// The session and registry a restore writes through. The structure script runs through
/// `run_script` (the services pass `execute_script` on a dedicated child session), so this
/// crate needs no services dependency.
#[derive(Clone, Copy)]
pub struct RestoreJob<'a> {
    pub dialects: &'a DialectRegistry,
    /// A dedicated session on the target database (structure reads, loads).
    pub target: &'a dyn Connection,
    pub options: &'a RestoreOptions,
}

/// Restores the archive at `source` (module docs). `run_script` executes a structure script on
/// the target session with the `oxbase_types::script` semantics and returns whether it
/// committed.
pub async fn restore(
    job: RestoreJob<'_>,
    source: &Path,
    run_script: &ScriptRunner,
    on_progress: &(dyn Fn(&RestoreProgress) + Send + Sync),
    cancel: CancellationToken,
) -> Result<RestoreReport> {
    let outcome = restore_run(job, source, run_script, on_progress, cancel).await;
    match outcome.error {
        Some(e) => Err(e),
        None => Ok(outcome.report),
    }
}

/// What [`restore_run`] did: the report (also after a failure or cancel) and the error that
/// stopped it.
#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    pub report: RestoreReport,
    /// `None` when the restore finished; `Error::Cancelled` when it was cancelled.
    pub error: Option<Error>,
}

/// [`restore`] with the report kept when it fails (the services send it with the `failed` and
/// `cancelled` events).
pub async fn restore_run(
    job: RestoreJob<'_>,
    source: &Path,
    run_script: &ScriptRunner,
    on_progress: &(dyn Fn(&RestoreProgress) + Send + Sync),
    cancel: CancellationToken,
) -> RestoreOutcome {
    let mut run = Restore {
        job,
        source: source.to_path_buf(),
        run_script,
        on_progress,
        cancel,
        started: Instant::now(),
        progress: RestoreProgress::default(),
        last_report: None,
        objects: Vec::new(),
        warnings: Vec::new(),
        wrote: false,
        container: job.options.container.display_name(),
        replaced: BTreeSet::new(),
        emptied: Vec::new(),
    };
    let error = run.run().await.err();
    let error = match error {
        Some(_) if run.cancel.is_cancelled() => Some(Error::Cancelled),
        other => other,
    };
    let left_behind = run.left_behind(error.as_ref());
    let rows = run.objects.iter().map(|o| o.rows).sum();
    RestoreOutcome {
        report: RestoreReport {
            objects: run.objects,
            rows,
            left_behind,
            warnings: run.warnings,
            elapsed_ms: elapsed_ms(run.started),
        },
        error,
    }
}

/// Progress events at most this often while rows load.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// What the blocking open step read.
struct Opened {
    manifest: Manifest,
    /// The selected objects in creation order, with their definitions.
    selected: Vec<(ManifestObject, ObjectDef)>,
}

/// One object of the restore, as planned.
struct Planned {
    /// Position in `Restore::objects`.
    slot: usize,
    manifest: ManifestObject,
    /// The definition moved to the target container.
    def: ObjectDef,
    /// The object exists in the target now.
    existing: Option<ObjectRef>,
    /// Created (or dropped and created) by this restore.
    create: bool,
    /// Created after the rows (`objects::after_data`).
    after_data: bool,
}

impl Planned {
    /// The table or object as it is (or will be) in the target.
    fn target_ref(&self) -> ObjectRef {
        self.existing
            .clone()
            .unwrap_or_else(|| self.def.object_ref())
    }
}

struct Restore<'a> {
    job: RestoreJob<'a>,
    source: PathBuf,
    run_script: &'a ScriptRunner,
    on_progress: &'a (dyn Fn(&RestoreProgress) + Send + Sync),
    cancel: CancellationToken,
    started: Instant,
    progress: RestoreProgress,
    last_report: Option<Instant>,
    objects: Vec<RestoredObject>,
    warnings: Vec<String>,
    /// Something was written to the target.
    wrote: bool,
    container: String,
    /// Slots of objects that existed and are dropped and created again.
    replaced: BTreeSet<usize>,
    /// Tables emptied before the load.
    emptied: Vec<String>,
}

fn check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

impl Restore<'_> {
    fn report(&mut self, force: bool) {
        let now = Instant::now();
        if !force && self.last_report.is_some_and(|t| now - t < PROGRESS_EVERY) {
            return;
        }
        self.last_report = Some(now);
        self.progress.elapsed_ms = elapsed_ms(self.started);
        (self.on_progress)(&self.progress);
    }

    fn phase(&mut self, phase: &str) {
        phase.clone_into(&mut self.progress.phase);
        self.progress.current = None;
        self.report(true);
    }

    async fn run(&mut self) -> Result<()> {
        let options = self.job.options;
        if options.is_destructive() && !options.confirm_destructive {
            return Err(Error::invalid_input(
                "this restore drops or empties existing objects; confirm it first",
            ));
        }
        if !matches!(
            options.container.kind,
            ObjectKind::Database | ObjectKind::Schema
        ) {
            return Err(Error::invalid_input(format!(
                "a restore goes into a database or a schema, not into a {}",
                options.container.kind
            )));
        }
        let engine = self.job.target.engine();
        let opened = self.open(engine).await?;
        self.progress.rows = 0;
        let manifest = opened.manifest;
        let with_structure =
            options.content != RestoreContent::DataOnly && manifest.content.has_structure();
        let with_data =
            options.content != RestoreContent::StructureOnly && manifest.content.has_rows();
        if options.content != RestoreContent::StructureOnly && !manifest.content.has_rows() {
            self.warnings
                .push("the backup holds no table rows; only the structure was restored".into());
        }
        if !manifest.content.has_structure() {
            if options.content == RestoreContent::StructureOnly {
                return Err(Error::invalid_input(
                    "the backup holds table rows only, no structure: restore it as data only",
                ));
            }
            if options.content == RestoreContent::StructureAndData {
                self.warnings.push(
                    "the backup holds table rows only: they were loaded into the existing \
                     tables, and no definition was created or changed"
                        .into(),
                );
            }
        }

        // Plan: move every definition to the target container, find what exists there.
        let from = Location::of_container(&manifest.source.container);
        let to = Location::of_container(&options.container);
        let moved = from != to;
        let mut planned = Vec::with_capacity(opened.selected.len());
        let mut lister = Lister::default();
        self.phase("structure");
        for (mo, mut def) in opened.selected {
            check(&self.cancel)?;
            relocate_def(&mut def, &from, &to);
            let object = def.object_ref();
            if !to.contains(&object) {
                return Err(Error::invalid_input(format!(
                    "the backup's object {} does not lie in the backed-up container {}; nothing \
                     was restored",
                    object.display_name(),
                    manifest.source.container.display_name()
                )));
            }
            if moved && self.job.dialects.ddl(engine, def.kind()).is_ok() {
                retarget_text(engine, &mut def, &from, &to);
            }
            let existing = lister
                .find(
                    self.job.dialects,
                    self.job.target,
                    &options.container,
                    &object,
                    &self.cancel,
                )
                .await?;
            self.objects.push(RestoredObject {
                index: mo.index,
                object,
                status: RestoredStatus::NotRun,
                rows: 0,
                error: None,
            });
            planned.push(Planned {
                slot: self.objects.len() - 1,
                manifest: mo,
                def,
                existing,
                create: false,
                after_data: false,
            });
        }
        self.progress.objects_total = u32::try_from(planned.len()).unwrap_or(u32::MAX);
        self.progress.rows_total = if with_data {
            planned
                .iter()
                .filter_map(|p| p.manifest.data.as_ref())
                .map(|d| d.rows)
                .sum()
        } else {
            0
        };

        let (pre, post) = if with_structure {
            self.plan_structure(engine, &mut planned).await?
        } else {
            self.plan_data_only(&planned)?;
            (Script::new(), Script::new())
        };
        if with_data && !options.truncate_before_load && !options.append_rows {
            self.refuse_filled_tables(&planned).await?;
        }

        // Structure before the rows.
        if !pre.is_empty() {
            check(&self.cancel)?;
            let slots: Vec<usize> = planned
                .iter()
                .filter(|p| p.create && !p.after_data)
                .map(|p| p.slot)
                .collect();
            self.script(pre, &slots).await?;
        }
        for p in planned.iter().filter(|p| !p.create) {
            if self.objects[p.slot].status == RestoredStatus::NotRun && with_structure {
                self.objects[p.slot].status = RestoredStatus::Kept;
            }
        }

        // Rows, table by table in creation order (parents first).
        if with_data {
            self.phase("data");
            let tables: Vec<TableDef> = planned
                .iter()
                .filter_map(|p| match &p.def {
                    ObjectDef::Table(t) => Some(t.clone()),
                    _ => None,
                })
                .collect();
            let cycles: Vec<ObjectRef> = table_dependencies(&tables)
                .order()
                .cycles
                .into_iter()
                .flatten()
                .collect();
            // (table, its rows reference each other: foreign-key checks off while emptying).
            let to_empty: Vec<(ObjectRef, bool)> = planned
                .iter()
                .filter(|p| !p.create && options.truncate_before_load && p.manifest.data.is_some())
                .filter_map(|p| match &p.def {
                    ObjectDef::Table(t) => Some((
                        p.target_ref(),
                        cycles.contains(&t.obj) || self_referencing(t),
                    )),
                    _ => None,
                })
                .collect();
            if !to_empty.is_empty() {
                check(&self.cancel)?;
                self.empty_tables(&to_empty).await?;
            }
            for p in &planned {
                let ObjectDef::Table(table) = &p.def else {
                    continue;
                };
                if p.manifest.data.is_none() {
                    continue;
                }
                check(&self.cancel)?;
                // Tables this restore created get their foreign keys to themselves and to later
                // tables after the load; existing tables on a cycle or with a foreign key to
                // themselves load with the checks off where the engine allows it.
                let checks_off =
                    !p.create && (cycles.contains(&table.obj) || self_referencing(table));
                self.progress.current = Some(table.obj.clone());
                self.report(true);
                match self.load_table(p, table, checks_off).await {
                    Ok(rows) => {
                        let o = &mut self.objects[p.slot];
                        o.rows = rows;
                        if !p.create {
                            o.status = RestoredStatus::Loaded;
                        }
                    }
                    Err(e) => {
                        let o = &mut self.objects[p.slot];
                        o.status = RestoredStatus::Failed;
                        o.error = Some(message(&e));
                        return Err(e);
                    }
                }
                self.progress.objects_done += 1;
                self.report(true);
            }
            self.restart_sequences(&planned).await?;
        }

        // Structure after the rows: deferred foreign keys, materialized views, triggers and
        // what names them.
        if !post.is_empty() {
            self.phase("structure");
            check(&self.cancel)?;
            let slots: Vec<usize> = planned
                .iter()
                .filter(|p| p.create && p.after_data)
                .map(|p| p.slot)
                .collect();
            self.script(post, &slots).await?;
        }
        self.progress.current = None;
        self.progress.objects_done = self.progress.objects_total;
        self.report(true);
        Ok(())
    }

    /// Opens the archive on a blocking thread: manifest, engine check, selection,
    /// verification, definitions. Nothing is written before this succeeds.
    async fn open(&mut self, engine: Engine) -> Result<Opened> {
        let options = self.job.options.clone();
        let path = self.source.clone();
        let cancel = self.cancel.clone();
        let checked = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&checked);
        if options.verify_first {
            self.phase("verify");
        }
        let mut task = tokio::task::spawn_blocking(move || {
            open_archive(&path, &options, engine, &cancel, &counter)
        });
        let mut tick = tokio::time::interval(PROGRESS_EVERY);
        let stop = self.cancel.clone();
        loop {
            tokio::select! {
                biased;
                // The blocking task polls the same token and stops on its own.
                () = stop.cancelled() => return Err(Error::Cancelled),
                joined = &mut task => {
                    return joined.map_err(|_| Error::internal("reading the backup stopped unexpectedly"))?;
                }
                _ = tick.tick() => {
                    // Verification progress: bytes checked, shown as rows of the phase.
                    self.progress.rows = checked.load(Ordering::Relaxed);
                    self.report(false);
                }
            }
        }
    }

    /// Plans the structure scripts: drops of replaced objects and creates before the rows,
    /// deferred foreign keys, materialized views and triggers after them.
    async fn plan_structure(
        &mut self,
        engine: Engine,
        planned: &mut [Planned],
    ) -> Result<(Script, Script)> {
        let options = self.job.options;
        let dialects = self.job.dialects;
        let target = self.job.target;
        let version = target.server_version().clone();
        let ctx = DdlContext::new(&version);

        // Existing objects: replaced, kept when equal, refused when different.
        let mut differ = Vec::new();
        for p in planned.iter_mut() {
            match &p.existing {
                None => p.create = true,
                Some(_) if options.drop_existing => {
                    p.create = true;
                    self.replaced.insert(p.slot);
                }
                Some(existing) => {
                    check(&self.cancel)?;
                    let current = dialects
                        .fetcher(engine, p.def.kind())?
                        .load(target, existing, self.cancel.clone())
                        .await?;
                    let equal = dialects
                        .comparator(engine, p.def.kind())
                        .is_ok_and(|c| c.diff(&current, &p.def).is_empty());
                    if !equal {
                        differ.push(existing.display_name());
                    }
                }
            }
        }
        if !differ.is_empty() {
            return Err(Error::invalid_input(format!(
                "these objects exist in {} and differ from the backup: {}; restore with drop \
                 existing to replace them",
                self.container,
                differ.join(", ")
            )));
        }

        // Generators first, so a missing one stops the restore before anything is written.
        for p in planned.iter().filter(|p| p.create) {
            if dialects.ddl(engine, p.def.kind()).is_err() {
                return Err(Error::unsupported(format!(
                    "Oxbase cannot create {} objects on {} yet ({}); leave it out of the \
                     restore",
                    p.def.kind(),
                    engine.display_name(),
                    p.def.object_ref().display_name()
                )));
            }
        }

        let mut pre = Script::new();
        let mut post = Script::new();
        // Drops of replaced objects, dependents first.
        for p in planned.iter().rev() {
            let (true, Some(existing)) = (p.create, &p.existing) else {
                continue;
            };
            let generator = dialects.ddl(engine, p.def.kind())?;
            let current = match p.def.kind() {
                ObjectKind::Table => match dialects
                    .fetcher(engine, ObjectKind::Table)?
                    .load(target, existing, self.cancel.clone())
                    .await?
                {
                    ObjectDef::Table(t) => Some(t),
                    _ => None,
                },
                _ => None,
            };
            let drop_ctx = match &current {
                Some(t) => ctx.with_table(t),
                None => ctx,
            };
            pre.append(generator.drop_object(existing, &drop_ctx)?);
        }

        // What waits until after the rows (module docs, step 3).
        let created: Vec<&ObjectDef> = planned
            .iter()
            .filter(|p| p.create)
            .map(|p| &p.def)
            .collect();
        let late = after_data(&created);
        let mut flags = late.iter();
        for p in planned.iter_mut().filter(|p| p.create) {
            p.after_data = flags.next().copied().unwrap_or(false);
        }

        // Tables being created, in order: their foreign keys to themselves and to tables
        // created later wait until after the rows (cycles, and loads in parent-first order).
        let created_tables: Vec<ObjectRef> = planned
            .iter()
            .filter(|p| p.create && p.def.kind() == ObjectKind::Table)
            .map(|p| p.def.object_ref())
            .collect();
        let mut deferred: Vec<(TableDef, Vec<oxbase_types::ForeignKeyDef>)> = Vec::new();
        for p in planned.iter().filter(|p| p.create) {
            let generator = dialects.ddl(engine, p.def.kind())?;
            match &p.def {
                ObjectDef::Table(t) => {
                    let position = created_tables
                        .iter()
                        .position(|r| *r == t.obj)
                        .unwrap_or(usize::MAX);
                    let mut now = t.clone();
                    // `>=`: a foreign key to the table itself waits too (rows may point at rows
                    // loaded after them).
                    let (later, keep): (Vec<_>, Vec<_>) =
                        t.foreign_keys.iter().cloned().partition(|fk| {
                            created_tables
                                .iter()
                                .position(|r| same_table(&fk.ref_table, r))
                                .is_some_and(|q| q >= position)
                        });
                    now.foreign_keys = keep;
                    pre.append(generator.create(&ObjectDef::Table(now.clone()), &ctx)?);
                    if !later.is_empty() {
                        deferred.push((now, later));
                    }
                }
                _ if p.after_data => {}
                def => {
                    pre.append(generator.create(def, &ctx)?);
                }
            }
        }
        if !deferred.is_empty() {
            let generator = dialects.ddl(engine, ObjectKind::ForeignKey)?;
            for (mut table, fks) in deferred {
                for fk in fks {
                    let script = generator.create(
                        &ObjectDef::ForeignKey {
                            table: table.obj.clone(),
                            foreign_key: fk.clone(),
                        },
                        &ctx.with_table(&table),
                    )?;
                    post.append(script);
                    table.foreign_keys.push(fk);
                }
            }
        }
        let deferred: Vec<&ObjectDef> = planned
            .iter()
            .filter(|p| p.create && p.after_data)
            .map(|p| &p.def)
            .collect();
        for i in late_order(&deferred) {
            let def = deferred[i];
            let generator = dialects.ddl(engine, def.kind())?;
            post.append(generator.create(def, &ctx)?);
        }
        Ok((pre, post))
    }

    /// Data only: every selected table must exist in the target.
    fn plan_data_only(&self, planned: &[Planned]) -> Result<()> {
        let missing: Vec<String> = planned
            .iter()
            .filter(|p| p.def.kind() == ObjectKind::Table && p.existing.is_none())
            .map(|p| p.def.object_ref().display_name())
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(Error::invalid_input(format!(
                "these tables do not exist in {}: {}; restore their structure too",
                self.container,
                missing.join(", ")
            )))
        }
    }

    /// Runs one structure script; marks `slots` created / replaced, or failed.
    async fn script(&mut self, script: Script, slots: &[usize]) -> Result<()> {
        let run = (self.run_script)(script);
        let result = until_cancelled(&self.cancel, run).await;
        match result {
            Ok(_) => {
                self.wrote = true;
                for &slot in slots {
                    self.objects[slot].status = if self.replaced.contains(&slot) {
                        RestoredStatus::Replaced
                    } else {
                        RestoredStatus::Created
                    };
                }
                Ok(())
            }
            Err(e) => {
                // The script may have run in part (engines whose DDL commits per statement).
                self.wrote = true;
                for &slot in slots {
                    self.objects[slot].status = RestoredStatus::Failed;
                    self.objects[slot].error = Some(message(&e));
                }
                Err(e)
            }
        }
    }

    /// Runs the loader's statements that switch foreign-key checks for `tables` off (`enabled`
    /// false) or on, outside any transaction. Switching on is best effort (warnings).
    async fn foreign_key_checks(&mut self, tables: &[&ObjectRef], enabled: bool) -> Result<()> {
        let connection = self.job.target;
        let loader = self.job.dialects.bulk_loader(connection.engine())?;
        let plain = LoadSession::new(connection, None);
        for table in tables {
            for sql in loader
                .foreign_key_checks(table, enabled)
                .unwrap_or_default()
            {
                let statement = BoundStatement::new(sql, Vec::new());
                if enabled {
                    if let Err(e) = plain.run(&statement, CancellationToken::new()).await {
                        self.warnings.push(format!(
                            "switching foreign-key checks back on for {} failed: {e}",
                            table.display_name()
                        ));
                    }
                } else {
                    plain.run(&statement, self.cancel.clone()).await?;
                }
            }
        }
        Ok(())
    }

    /// Empties `tables` (in creation order; the flag: rows of the table reference each other)
    /// with `DELETE`, children first, in one transaction, foreign-key checks off for flagged
    /// tables where the engine allows it.
    async fn empty_tables(&mut self, tables: &[(ObjectRef, bool)]) -> Result<()> {
        let relaxed: Vec<&ObjectRef> = tables.iter().filter(|(_, r)| *r).map(|(t, _)| t).collect();
        if !relaxed.is_empty() {
            self.foreign_key_checks(&relaxed, false).await?;
        }
        let result = self.delete_rows(tables).await;
        if !relaxed.is_empty() {
            self.foreign_key_checks(&relaxed, true).await?;
        }
        result
    }

    async fn delete_rows(&mut self, tables: &[(ObjectRef, bool)]) -> Result<()> {
        let connection = self.job.target;
        let quoting = self.job.dialects.quoting(connection.engine())?;
        let tx = until_cancelled(&self.cancel, connection.begin()).await?;
        let session = LoadSession::new(connection, Some(tx.as_ref()));
        let mut failed = None;
        for (table, _) in tables.iter().rev() {
            let mut w = SqlWriter::new(quoting.as_ref());
            w.push("DELETE FROM ").qualified(table);
            let statement = w.finish();
            if let Err(e) = session.run(&statement, self.cancel.clone()).await {
                failed = Some(e);
                break;
            }
        }
        if let Some(e) = failed {
            if let Ok(Err(rollback)) =
                tokio::time::timeout(Duration::from_secs(5), tx.rollback()).await
            {
                self.warnings.push(format!(
                    "rolling back the emptying of the tables failed: {rollback}"
                ));
            }
            return Err(e);
        }
        tx.commit().await?;
        self.wrote = true;
        self.emptied = tables.iter().map(|(t, _)| t.display_name()).collect();
        Ok(())
    }

    /// Loads one table's rows (module docs, step 3); returns the rows loaded.
    async fn load_table(&mut self, p: &Planned, table: &TableDef, checks_off: bool) -> Result<u64> {
        let dialects = self.job.dialects;
        let connection = self.job.target;
        let engine = connection.engine();
        let version = connection.server_version().clone();
        // The table as found in the target (the definition's ref when this restore created it).
        let table_ref = p.target_ref();
        let data = p.manifest.data.as_ref().ok_or_else(|| {
            Error::internal(format!(
                "{} has no rows in the backup",
                table.obj.display_name()
            ))
        })?;
        let columns = data
            .columns
            .iter()
            .map(|name| {
                table
                    .column(name)
                    .map(|c| LoadColumn {
                        name: c.name.clone(),
                        data_type: c.data_type.clone(),
                        native_type: c.native_type.clone(),
                        nullable: c.nullable,
                    })
                    .ok_or_else(|| {
                        Error::invalid_input(format!(
                            "the backup's rows of {} name a column '{name}' its definition does \
                             not have",
                            table.obj.display_name()
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let path = self.source.clone();
        let index = p.manifest.index;
        let stored =
            tokio::task::spawn_blocking(move || ArchiveReader::open(&path)?.table_schema(index))
                .await
                .map_err(|e| Error::internal(format!("reading the backup failed: {e}")))??;
        let columns = stored_text_forms(columns, &stored);
        let target = LoadTarget {
            table: &table_ref,
            columns: &columns,
            version: &version,
        };
        let loader = dialects.bulk_loader(engine)?;
        let plain = LoadSession::new(connection, None);
        until_cancelled(
            &self.cancel,
            loader.prepare(plain, &target, self.cancel.clone()),
        )
        .await?;
        let result = self
            .load_rows(loader.as_ref(), &target, p.manifest.index, checks_off)
            .await;
        let finish = loader.finish(plain, &target, CancellationToken::new());
        match tokio::time::timeout(Duration::from_secs(5), finish).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => self.warnings.push(format!(
                "ending the load of {} failed: {e}",
                table.obj.display_name()
            )),
            Err(_) => self.warnings.push(format!(
                "ending the load of {} took too long",
                table.obj.display_name()
            )),
        }
        if checks_off && let Some(statements) = loader.foreign_key_checks(&table_ref, true) {
            for sql in statements {
                if let Err(e) = plain
                    .run(
                        &BoundStatement::new(sql, Vec::new()),
                        CancellationToken::new(),
                    )
                    .await
                {
                    self.warnings.push(format!(
                        "switching foreign-key checks back on for {} failed: {e}",
                        table.obj.display_name()
                    ));
                }
            }
        }
        let rows = result?;
        // Identity and auto-increment counters follow the loaded values. The rows are committed
        // by now: a failure here is a warning, not a failed table.
        let generated: Vec<_> = table
            .columns
            .iter()
            .filter(|c| c.auto_increment && data.columns.contains(&c.name))
            .cloned()
            .collect();
        if !generated.is_empty() {
            for sql in loader.identity_resync(&table_ref, &generated) {
                if let Err(e) = plain
                    .run(&BoundStatement::new(sql, Vec::new()), self.cancel.clone())
                    .await
                {
                    if e.is_cancelled() {
                        return Err(e);
                    }
                    self.warnings.push(format!(
                        "the rows of {} were restored, but moving its identity counter past them \
                         failed: {e}; new rows may get keys that already exist",
                        table_ref.display_name()
                    ));
                    break;
                }
            }
        }
        Ok(rows)
    }

    /// The table's transaction: optional `DELETE`, the batches, commit (or roll back).
    async fn load_rows(
        &mut self,
        loader: &dyn BulkLoader,
        target: &LoadTarget<'_>,
        index: u32,
        checks_off: bool,
    ) -> Result<u64> {
        let connection = self.job.target;
        let plain = LoadSession::new(connection, None);
        if checks_off && let Some(statements) = loader.foreign_key_checks(target.table, false) {
            for sql in statements {
                plain
                    .run(&BoundStatement::new(sql, Vec::new()), self.cancel.clone())
                    .await?;
            }
        }
        let tx = if loader.transactional() {
            Some(until_cancelled(&self.cancel, connection.begin()).await?)
        } else {
            None
        };
        let result = self
            .load_batches(loader, target, tx.as_deref(), index)
            .await;
        match (result, tx) {
            (Ok(rows), Some(tx)) => {
                tx.commit().await?;
                self.wrote = true;
                Ok(rows)
            }
            (Ok(rows), None) => {
                self.wrote = true;
                Ok(rows)
            }
            (Err(e), Some(tx)) => {
                if let Ok(Err(rollback)) =
                    tokio::time::timeout(Duration::from_secs(5), tx.rollback()).await
                {
                    self.warnings.push(format!(
                        "rolling back the load of {} failed: {rollback}",
                        target.table.display_name()
                    ));
                }
                Err(e)
            }
            (Err(e), None) => {
                self.wrote = true;
                Err(e)
            }
        }
    }

    async fn load_batches(
        &mut self,
        loader: &dyn BulkLoader,
        target: &LoadTarget<'_>,
        tx: Option<&dyn Tx>,
        index: u32,
    ) -> Result<u64> {
        let connection = self.job.target;
        let session = LoadSession::new(connection, tx);
        let (sender, mut receiver) = mpsc::channel::<Result<RecordBatch>>(2);
        let path = self.source.clone();
        // Ends on its own once `receiver` is dropped.
        let _reader = tokio::task::spawn_blocking(move || read_table(&path, index, &sender));
        let per_call = self
            .job
            .options
            .batch_rows
            .unwrap_or_else(|| loader.limits(target.version).rows_per_call)
            .max(1);
        let mut rows = 0_u64;
        loop {
            let next = tokio::select! {
                biased;
                () = self.cancel.cancelled() => return Err(Error::Cancelled),
                next = receiver.recv() => next,
            };
            let batch = match next {
                None => return Ok(rows),
                Some(item) => item?,
            };
            target.check_batch(&batch)?;
            let mut offset = 0;
            while offset < batch.num_rows() {
                let len = per_call.min(batch.num_rows() - offset);
                let chunk = batch.slice(offset, len);
                rows += loader
                    .load(session, target, &chunk, self.cancel.clone())
                    .await?;
                offset += len;
                self.progress.rows += u64::try_from(len).unwrap_or(u64::MAX);
                self.report(false);
            }
        }
    }

    /// The first cell of the first row `statement` returns on the target session (reads the
    /// whole result).
    async fn first_cell(&self, statement: &BoundStatement) -> Result<Option<Value>> {
        let session = LoadSession::new(self.job.target, None);
        let mut stream = until_cancelled(
            &self.cancel,
            session.execute(&statement.sql, &statement.params, self.cancel.clone()),
        )
        .await?;
        let mut first = None;
        loop {
            let item = tokio::select! {
                biased;
                () = self.cancel.cancelled() => return Err(Error::Cancelled),
                item = stream.next() => item,
            };
            match item {
                None => return Ok(first),
                Some(Err(e)) => return Err(e),
                Some(Ok(ResultItem::Batch(batch))) => {
                    if first.is_none() && batch.num_rows() > 0 && batch.num_columns() > 0 {
                        first = Some(cell_value(batch.column(0).as_ref(), 0)?);
                    }
                }
                Some(Ok(_)) => {}
            }
        }
    }

    /// Refuses (before anything is written) to add rows to existing tables that already hold
    /// some (module docs, step 4).
    async fn refuse_filled_tables(&self, planned: &[Planned]) -> Result<()> {
        let quoting = self.job.dialects.quoting(self.job.target.engine())?;
        let mut filled = Vec::new();
        for p in planned {
            if p.create || p.def.kind() != ObjectKind::Table || p.manifest.data.is_none() {
                continue;
            }
            let Some(existing) = &p.existing else {
                continue;
            };
            check(&self.cancel)?;
            let top = quoting.top_clause(1);
            let mut w = SqlWriter::new(quoting.as_ref());
            w.push("SELECT ");
            if let Some(top) = &top {
                w.push(top).push(" ");
            }
            w.push("1 FROM ").qualified(existing);
            if top.is_none() {
                w.push(" ").push(&quoting.limit_clause(1));
            }
            if self.first_cell(&w.finish()).await?.is_some() {
                filled.push(existing.display_name());
            }
        }
        if filled.is_empty() {
            return Ok(());
        }
        Err(Error::invalid_input(format!(
            "these tables in {} already hold rows: {}; nothing was restored. Empty them first \
             (empty tables before loading), replace them (drop existing), or choose to add the \
             rows to them",
            self.container,
            filled.join(", ")
        )))
    }

    /// Restarts each sequence this restore created after the highest restored value of the
    /// columns whose default draws from it (module docs, step 5). Failures are warnings: the
    /// rows are in place.
    async fn restart_sequences(&mut self, planned: &[Planned]) -> Result<()> {
        let engine = self.job.target.engine();
        let Ok(quoting) = self.job.dialects.quoting(engine) else {
            return Ok(());
        };
        let tables: Vec<(ObjectRef, &TableDef)> = planned
            .iter()
            .filter(|p| p.manifest.data.is_some())
            .filter_map(|p| match &p.def {
                ObjectDef::Table(t) => Some((p.target_ref(), t)),
                _ => None,
            })
            .collect();
        for p in planned {
            let ObjectDef::Sequence(seq) = &p.def else {
                continue;
            };
            let users: Vec<(&ObjectRef, &str)> = tables
                .iter()
                .flat_map(|(r, t)| {
                    t.columns
                        .iter()
                        .filter(|c| {
                            !c.auto_increment
                                && c.default
                                    .as_deref()
                                    .is_some_and(|d| mentions(d, &seq.obj.name))
                        })
                        .map(move |c| (r, c.name.as_str()))
                })
                .collect();
            if users.is_empty() || seq.increment == 0 {
                continue;
            }
            let name = p.target_ref().display_name();
            if !p.create {
                self.warnings.push(format!(
                    "the sequence {name} was kept as it is; check that its next value is past \
                     the restored rows"
                ));
                continue;
            }
            check(&self.cancel)?;
            let mut edge: Option<i128> = None;
            for (table, column) in users {
                let mut w = SqlWriter::new(quoting.as_ref());
                w.push(if seq.increment > 0 {
                    "SELECT MAX("
                } else {
                    "SELECT MIN("
                })
                .ident(column)
                .push(") FROM ")
                .qualified(table);
                match self.first_cell(&w.finish()).await {
                    Ok(value) => {
                        if let Some(v) = value.as_ref().and_then(integer) {
                            edge = Some(match edge {
                                None => v,
                                Some(e) if seq.increment > 0 => e.max(v),
                                Some(e) => e.min(v),
                            });
                        }
                    }
                    Err(e) if e.is_cancelled() => return Err(e),
                    Err(e) => {
                        self.warnings.push(format!(
                            "the next value of the sequence {name} could not be worked out: {e}"
                        ));
                        edge = None;
                        break;
                    }
                }
            }
            let Some(next) = edge.and_then(|e| restart_value(seq, e)) else {
                continue;
            };
            let beyond = if seq.increment > 0 {
                seq.max_value.is_some_and(|m| next > m)
            } else {
                seq.min_value.is_some_and(|m| next < m)
            };
            if beyond {
                self.warnings.push(format!(
                    "the restored rows reach the end of the sequence {name}; it was left at its \
                     start value"
                ));
                continue;
            }
            let mut w = SqlWriter::new(quoting.as_ref());
            w.push("ALTER SEQUENCE ")
                .qualified(&p.target_ref())
                .push(&format!(" RESTART WITH {next}"));
            let statement = w.finish();
            let session = LoadSession::new(self.job.target, None);
            match session.run(&statement, self.cancel.clone()).await {
                Ok(_) => self.wrote = true,
                Err(e) if e.is_cancelled() => return Err(e),
                Err(e) => self.warnings.push(format!(
                    "the sequence {name} could not be moved past the restored rows: {e}; new \
                     rows may get values that already exist"
                )),
            }
        }
        Ok(())
    }

    /// In words: what the target holds now (module docs, step 4).
    fn left_behind(&self, error: Option<&Error>) -> String {
        if !self.wrote {
            return format!("nothing was written to {}", self.container);
        }
        let mut parts = Vec::new();
        if !self.emptied.is_empty() {
            parts.push(format!("emptied {}", self.emptied.join(", ")));
        }
        let created: Vec<String> = self
            .objects
            .iter()
            .filter(|o| matches!(o.status, RestoredStatus::Created | RestoredStatus::Replaced))
            .map(|o| o.object.display_name())
            .collect();
        if !created.is_empty() {
            parts.push(format!(
                "{} holds the restored objects {}",
                self.container,
                created.join(", ")
            ));
        }
        let loaded: Vec<String> = self
            .objects
            .iter()
            .filter(|o| o.rows > 0 && o.status != RestoredStatus::Failed)
            .map(|o| format!("{} ({} rows)", o.object.display_name(), o.rows))
            .collect();
        if !loaded.is_empty() {
            parts.push(format!("rows were loaded into {}", loaded.join(", ")));
        }
        if let Some(failed) = self
            .objects
            .iter()
            .find(|o| o.status == RestoredStatus::Failed)
        {
            parts.push(format!(
                "the step of {} failed and its transaction was rolled back where the engine \
                 allows it (MySQL and MariaDB keep DDL that ran)",
                failed.object.display_name()
            ));
        } else if error.is_some_and(Error::is_cancelled) {
            parts.push(
                "the restore was cancelled; the step that was running was rolled back".into(),
            );
        }
        let not_run = self
            .objects
            .iter()
            .filter(|o| o.status == RestoredStatus::NotRun)
            .count();
        if error.is_some() && not_run > 0 {
            parts.push(format!("{not_run} objects were not restored"));
        }
        parts.join("; ")
    }
}

/// `true` when `table` has a foreign key to itself.
fn self_referencing(table: &TableDef) -> bool {
    table
        .foreign_keys
        .iter()
        .any(|fk| same_table(&fk.ref_table, &table.obj))
}

/// An integer cell (`MAX` / `MIN` results: integers, decimals or text by engine).
fn integer(value: &Value) -> Option<i128> {
    match value {
        Value::Int(i) => Some(i128::from(*i)),
        Value::UInt(u) => Some(i128::from(*u)),
        Value::Decimal(t) | Value::Text(t) => {
            let t = t.trim();
            t.parse().ok().or_else(|| {
                let (whole, fraction) = t.split_once('.')?;
                fraction
                    .bytes()
                    .all(|b| b == b'0')
                    .then(|| whole.parse().ok())
                    .flatten()
            })
        }
        _ => None,
    }
}

/// Where sequence `seq` restarts when the restored columns that draw from it reach `edge` (their
/// highest value, the lowest for a descending sequence): `None` when its start is already past
/// them.
fn restart_value(seq: &SequenceDef, edge: i128) -> Option<i128> {
    let next = edge.checked_add(seq.increment)?;
    let past_start = if seq.increment > 0 {
        next > seq.start
    } else {
        next < seq.start
    };
    past_start.then_some(next)
}

fn message(e: &Error) -> String {
    match e {
        Error::Cancelled => "cancelled".to_owned(),
        other => other.to_string(),
    }
}

/// The blocking part of the open step (`Restore::open`).
fn open_archive(
    path: &Path,
    options: &RestoreOptions,
    engine: Engine,
    cancel: &CancellationToken,
    checked: &AtomicU64,
) -> Result<Opened> {
    let mut reader = ArchiveReader::open(path)?;
    let manifest = reader.manifest().clone();
    if manifest.engine != engine {
        return Err(Error::invalid_input(format!(
            "the backup is from {}, the target is {}",
            manifest.engine.display_name(),
            engine.display_name()
        )));
    }
    let all: BTreeSet<u32> = manifest.objects.iter().map(|o| o.index).collect();
    let selected: BTreeSet<u32> = if options.objects.is_empty() {
        all
    } else {
        if let Some(unknown) = options.objects.iter().find(|i| !all.contains(i)) {
            return Err(Error::invalid_input(format!(
                "the backup has no object {unknown}"
            )));
        }
        options.objects.iter().copied().collect()
    };
    if options.verify_first {
        let with_data = options.content != RestoreContent::StructureOnly;
        let report = reader.verify_objects(
            &selected,
            with_data,
            &mut |bytes| checked.store(bytes, Ordering::Relaxed),
            &|| cancel.is_cancelled(),
        )?;
        if !report.ok {
            return Err(Error::invalid_input(format!(
                "the backup failed verification, nothing was restored: {}",
                report.problem_lines().join("; ")
            )));
        }
    }
    let mut out = Vec::with_capacity(selected.len());
    for index in &manifest.order {
        if !selected.contains(index) {
            continue;
        }
        check(cancel)?;
        let object = manifest
            .object(*index)
            .cloned()
            .ok_or_else(|| Error::invalid_input(format!("the backup has no object {index}")))?;
        let def = reader.definition(*index)?;
        if def.kind() != object.kind {
            return Err(Error::invalid_input(format!(
                "the backup's object {index} is a {}, its definition a {}",
                object.kind,
                def.kind()
            )));
        }
        out.push((object, def));
    }
    Ok(Opened {
        manifest,
        selected: out,
    })
}

/// The reader thread of one table: batches into `sender` until the entry ends, an error, or
/// the receiver goes away.
/// `columns` with the types the backup stored them as: a column kept as the server's text
/// (`backup::keep_text_forms`, a time-zone-aware column whose driver reads it as text) loads
/// as text, and the server parses it back into the column's type.
fn stored_text_forms(mut columns: Vec<LoadColumn>, stored: &Schema) -> Vec<LoadColumn> {
    for (column, field) in columns.iter_mut().zip(stored.fields()) {
        if matches!(field.data_type(), ArrowType::Utf8)
            && load_arrow_type(&column.data_type) != ArrowType::Utf8
        {
            column.data_type = DataType::Text;
        }
    }
    columns
}

fn read_table(path: &Path, index: u32, sender: &mpsc::Sender<Result<RecordBatch>>) {
    let mut reader = match ArchiveReader::open(path) {
        Ok(r) => r,
        Err(e) => {
            let _ = sender.blocking_send(Err(e));
            return;
        }
    };
    let rows = match reader.table_rows(index) {
        Ok(rows) => rows,
        Err(e) => {
            let _ = sender.blocking_send(Err(e));
            return;
        }
    };
    for item in rows {
        let stop = item.is_err();
        if sender.blocking_send(item).is_err() || stop {
            return;
        }
    }
}

/// Finds objects in the target container, listing each (kind, parent) once.
#[derive(Default)]
struct Lister {
    listed: HashMap<(ObjectKind, ObjectRef), Vec<ObjectRef>>,
}

impl Lister {
    /// The target's object with `object`'s kind and name (and signature where both have one).
    async fn find(
        &mut self,
        dialects: &DialectRegistry,
        connection: &dyn Connection,
        container: &ObjectRef,
        object: &ObjectRef,
        cancel: &CancellationToken,
    ) -> Result<Option<ObjectRef>> {
        let parent = if object.kind.is_table_child() {
            let Some(owner) = object.owner() else {
                return Ok(None);
            };
            // The owner must exist before its children are listed.
            match self
                .find_under(dialects, connection, container.clone(), &owner, cancel)
                .await?
            {
                Some(owner) => owner,
                None => return Ok(None),
            }
        } else {
            container.clone()
        };
        self.find_under(dialects, connection, parent, object, cancel)
            .await
    }

    /// The object of `object`'s kind and name directly under `parent`.
    async fn find_under(
        &mut self,
        dialects: &DialectRegistry,
        connection: &dyn Connection,
        parent: ObjectRef,
        object: &ObjectRef,
        cancel: &CancellationToken,
    ) -> Result<Option<ObjectRef>> {
        let key = (object.kind, parent);
        if !self.listed.contains_key(&key) {
            let fetcher = dialects.fetcher(connection.engine(), object.kind)?;
            let refs = fetcher
                .list(connection, &key.1, cancel.clone())
                .await?
                .into_iter()
                .map(|s| s.obj)
                .collect();
            self.listed.insert(key.clone(), refs);
        }
        Ok(self.listed.get(&key).and_then(|refs| {
            refs.iter()
                .find(|r| {
                    r.name == object.name
                        && (r.signature.is_none()
                            || object.signature.is_none()
                            || r.signature == object.signature)
                })
                .cloned()
        }))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn columns_stored_as_text_load_as_text() {
        use arrow_schema::{Field, TimeUnit};
        let tz = DataType::TimestampTz { precision: Some(7) };
        let columns = vec![
            LoadColumn::new("id", DataType::INT64),
            LoadColumn::new("kept", tz.clone()),
            LoadColumn::new("instant", tz.clone()),
        ];
        let stored = Schema::new(vec![
            Field::new("id", ArrowType::Int64, false),
            Field::new("kept", ArrowType::Utf8, true),
            Field::new(
                "instant",
                ArrowType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into())),
                true,
            ),
        ]);
        let out = stored_text_forms(columns, &stored);
        let types: Vec<_> = out.iter().map(|c| c.data_type.clone()).collect();
        assert_eq!(types, [DataType::INT64, DataType::Text, tz]);
    }

    #[test]
    fn options_default_to_a_verified_non_destructive_restore() {
        let o: RestoreOptions =
            serde_json::from_str(r#"{"container":{"kind":"database","name":"m8_restore"}}"#)
                .unwrap();
        assert_eq!(
            o,
            RestoreOptions::into_container(ObjectRef::database("m8_restore"))
        );
        assert!(o.verify_first && !o.is_destructive());
        let json = serde_json::to_value(&o).unwrap();
        assert_eq!(json["content"], "structure_and_data");
        assert!(json.get("objects").is_none());
    }

    #[test]
    fn sequences_restart_past_the_restored_values() {
        let mut seq =
            SequenceDef::new(ObjectRef::schema(None, "s").child(ObjectKind::Sequence, "q"));
        seq.start = 1000;
        seq.increment = 1;
        assert_eq!(restart_value(&seq, 999), None, "the start is already past");
        assert_eq!(restart_value(&seq, 1000), Some(1001));
        seq.increment = 10;
        assert_eq!(restart_value(&seq, 2000), Some(2010));
        seq.start = -1;
        seq.increment = -1;
        assert_eq!(restart_value(&seq, -5), Some(-6));
        assert_eq!(restart_value(&seq, 3), None);
        assert_eq!(integer(&Value::Int(7)), Some(7));
        assert_eq!(integer(&Value::Decimal("12.000".into())), Some(12));
        assert_eq!(integer(&Value::Text(" 42 ".into())), Some(42));
        assert_eq!(integer(&Value::Text("1.5".into())), None);
        assert_eq!(integer(&Value::Null), None);
    }

    #[test]
    fn self_references_are_seen() {
        let mut t = TableDef::new(ObjectRef::table(Some("db"), Some("s"), "categories"));
        assert!(!self_referencing(&t));
        t.foreign_keys.push(oxbase_types::ForeignKeyDef {
            name: None,
            columns: vec!["parent_id".into()],
            ref_table: ObjectRef::table(None, Some("s"), "categories"),
            ref_columns: vec!["id".into()],
            on_delete: oxbase_types::FkAction::NoAction,
            on_update: oxbase_types::FkAction::NoAction,
            extras: oxbase_types::Extras::default(),
        });
        assert!(self_referencing(&t));
    }
}
