//! Planning and running a transfer on two connections (`docs/m8-contracts.md` §4.3-§4.5).
//! Owner of the bodies marked `unimplemented!("m8: transfer")`: the **transfer** stream.
//!
//! # Plan ([`plan`])
//!
//! 1. [`TransferSpec::validate`]; the target container must exist on the target connection.
//! 2. Per source object: load its definition through the source engine's fetchers (`Table`,
//!    `View`); for a `Query`, run it with a zero-row limit (`SELECT * FROM (<sql>) q WHERE 1 = 0`
//!    is **not** used: the query runs as typed with the driver's row limit 0 / first batch only)
//!    to read its Arrow schema; map every column ([`crate::mapping::map_column`]) and build the
//!    target table ([`crate::mapping::target_table`]).
//! 3. Per target table: does it exist (the target's `Table` fetcher)? Then check the mode:
//!    `create` refuses an existing table, `append` / `truncate` refuse a missing one and match
//!    columns by name; `replace` drops it. The DDL text (drop, create, truncate) comes from the
//!    target's `DdlGenerator` (`DdlContext::exact_values`) and its `drop_object`, rendered with
//!    `Script::to_sql`.
//! 4. Order: the foreign-key graph of the planned tables (`oxbase_dialect::table_dependencies`,
//!    using the **source** definitions), so parents load before children; cycles are reported
//!    and load in insertion order (their foreign keys come after the load anyway).
//! 5. `runnable` is false (with `blocked` reasons) when a column is `unsupported`, a mode check
//!    failed, a table name collides under the target's case rules, or (source and target on
//!    one server, [`PlanInputs::same_server`]) a target table is one of the source objects.
//!
//! Nothing is written during planning: [`plan`] is also the **dry run** (target DDL and the
//! mapping report). A query source is read inside a transaction that is always rolled back,
//! so a statement that writes leaves nothing behind (where the engine's transactions cover
//! it).
//!
//! # Run ([`copy_table`], called per table by the services in plan order)
//!
//! - The target table's DDL runs first (inside the table's transaction when the engine has
//!   transactional DDL and `TransactionScope::PerTable`; MySQL / MariaDB DDL commits at once).
//! - The source rows stream from the source session (`SELECT` built with
//!   `oxbase_dialect::sql::select_table_data` and the object's filter, or the user's query) as
//!   Arrow batches; each is converted ([`crate::convert::BatchConverter`]), re-chunked to
//!   `TransferOptions::batch_rows` (and the loader's `rows_per_call`) and loaded with the
//!   target engine's `BulkLoader` (`prepare` / `load` / `finish`, identity and foreign-key
//!   hooks). Memory stays bounded: at most one source batch and one load batch per table.
//! - Commits follow `TransactionScope`; `ErrorMode::Continue` drops rows with conversion
//!   issues (reported) and goes on; a server error fails the table.
//! - Progress: `on_progress` at most every 250 ms and after each commit, with cumulative
//!   counters ([`TableProgress`]).
//! - Cancel: the token stops the source stream and the running load statement; the open
//!   transaction rolls back; the report says what stayed ([`TableReport::left_behind`]).
//!
//! After every table: the planned foreign keys ([`TablePlan::foreign_keys`]) run on the target
//! (services), then `BulkLoader::identity_resync` statements, then the row-level security and
//! policies of created tables ([`PreparedTable::row_security`]).

//!
//! # How the run uses its transactions
//!
//! [`copy_table`] opens the table's transaction (when the loader is transactional), runs the
//! [`CopyTable::ddl`] statements inside it, then [`BulkLoader::prepare`] on that transaction
//! (so a loader that looks at the new table, such as SQL Server's `IDENTITY_INSERT`, sees it),
//! then the loads. The services pass only DDL that is undone by a rollback
//! ([`PreparedTable::ddl_in_transaction`]); DDL that commits on its own (MySQL / MariaDB) they
//! run before calling [`copy_table`], and they drop a table the run created when its load
//! fails. Foreign-key checks are switched off (`FkChecks::Disable`) only for tables that
//! existed before the run (new tables get their foreign keys after every table is loaded).

use std::collections::HashSet;
use std::future::Future;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use arrow_array::{BooleanArray, RecordBatch};
use arrow_schema::SchemaRef;
use futures::StreamExt;
use oxbase_dialect::deps::same_table;
use oxbase_dialect::sql::select_table_data;
use oxbase_dialect::{
    BulkLoader, DatabaseScope, DdlContext, DialectRegistry, Fidelity, LoadColumn, LoadSession,
    LoadTarget, TypeMapping, table_dependencies,
};
use oxbase_driver::{CancellationToken, Connection, ResultItem, ResultStream, Tx};
use oxbase_types::{
    BoundStatement, CaseRules, ColumnDef, DataType, Engine, Error, ErrorCategory, Extras,
    ForeignKeyDef, ObjectDef, ObjectKind, ObjectRef, Result, Script, ScriptStep, ServerVersion,
    StepPhase, TableDataQuery, TableDef,
};
use serde::{Deserialize, Serialize};

use crate::cells::target_rules;
use crate::convert::{BatchConverter, RowIssue};
use crate::mapping::{
    ColumnPlan, MappingContext, bound_key_columns, case_rules, constraint_name, fidelity_between,
    map_column, same_container, same_name, target_table, utf16_fidelity,
};
use crate::spec::{
    ErrorMode, FkChecks, IdentityMode, SourceObject, TableMode, TransactionScope, TransferOptions,
    TransferSpec,
};

/// Progress at most this often (plus after each commit).
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// How long a cancelled step may take to stop on its own before it is abandoned.
const CANCEL_GRACE: Duration = Duration::from_millis(800);
/// How long a rollback may take before the transaction is dropped (the server then rolls it
/// back when the session closes).
const ROLLBACK_WAIT: Duration = Duration::from_millis(600);
/// How long `BulkLoader::finish` and switching foreign-key checks back on may take after a
/// failure or a cancel.
const CLEANUP_WAIT: Duration = Duration::from_millis(500);
/// How long the rest of an abandoned source stream is drained (so the session can run the
/// next statement).
const DRAIN_WAIT: Duration = Duration::from_secs(1);
/// What draining, rollback and clean-up may take together once a copy is cancelled (the run
/// reports `Cancelled` within 2 s of the cancel).
const CANCEL_BUDGET: Duration = Duration::from_millis(1_100);

/// The plan of one target table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TablePlan {
    /// Index of the source object in `TransferSpec::objects`.
    pub object: usize,
    /// The source table or view (`None` for a query).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ObjectRef>,
    /// The target table.
    pub target: ObjectRef,
    /// The target table exists now.
    pub exists: bool,
    pub mode: TableMode,
    /// Every source column, in source order.
    pub columns: Vec<ColumnPlan>,
    /// The worst fidelity of the columns that are not skipped.
    pub fidelity: Fidelity,
    /// DDL run before the load (drop, create, truncate), as preview text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ddl: Vec<String>,
    /// Foreign keys added after every table is loaded, as preview text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreign_keys: Vec<String>,
    /// The table's rows, when the source can tell cheaply (catalog estimate; `None` else).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_rows: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// What a transfer will do ([`plan`]; also the dry run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferPlan {
    pub source_engine: Engine,
    pub target_engine: Engine,
    /// In `TransferSpec::objects` order.
    pub tables: Vec<TablePlan>,
    /// Load order: indexes into `tables`, parents before children.
    pub order: Vec<usize>,
    /// Groups of tables whose foreign keys form a cycle (indexes into `tables`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cycles: Vec<Vec<usize>>,
    /// The worst fidelity over every table.
    pub fidelity: Fidelity,
    /// Something is dropped or emptied (`replace`, `truncate`): the UI asks first.
    pub destructive: bool,
    /// The target runs DDL inside transactions (false on MySQL / MariaDB: a failed table can
    /// leave a created table behind; the services drop it).
    pub transactional_ddl: bool,
    /// The plan can run (no unsupported column, no failed mode check).
    pub runnable: bool,
    /// Why not, when `runnable` is false.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// The two connections a plan reads. Both are sessions on the right databases (the services
/// open them).
#[derive(Clone, Copy)]
pub struct PlanInputs<'a> {
    pub dialects: &'a DialectRegistry,
    pub source: &'a dyn Connection,
    pub target: &'a dyn Connection,
    pub spec: &'a TransferSpec,
    /// Source and target are sessions on one server (the same base session or profile): a
    /// target table that is one of the source objects is refused.
    pub same_server: bool,
}

/// What running one planned table needs besides its [`TablePlan`] (built by [`prepare`] next
/// to the plan, for the services).
#[derive(Debug, Clone)]
pub struct PreparedTable {
    /// The target table: as the run creates it, or the existing one (append / truncate).
    pub definition: TableDef,
    /// DDL steps before the load, in execution order (drop, create, or the truncate).
    pub ddl: Vec<ScriptStep>,
    /// Every DDL step is undone by a rollback: it runs inside the table's transaction
    /// ([`copy_table`]); else the services run it before the copy.
    pub ddl_in_transaction: bool,
    /// The run creates the table (create, replace).
    pub creates: bool,
    /// Drops the table the run creates (clean-up where DDL commits at once).
    pub drop_created: Vec<ScriptStep>,
    /// The planned foreign keys (one script each), added after every table is loaded.
    pub foreign_keys: Vec<Script>,
    /// Row-level security and policies of a table the run creates (the target's table
    /// `alter` from the table as created without them): switched on after every table is
    /// loaded, after the foreign keys and identity resyncs, so a policy that binds the owner
    /// (`FORCE`) neither filters nor refuses the load. `None` when the table has none.
    pub row_security: Option<Script>,
    /// The rows to copy (bound values).
    pub select: BoundStatement,
    /// The load columns (not skipped), in load order.
    pub columns: Vec<LoadColumn>,
    /// Identity / auto-increment columns of the target whose values are loaded (for
    /// `BulkLoader::identity_resync` after the load), with their target names.
    pub identity_columns: Vec<ColumnDef>,
}

/// A plan and what running it needs ([`prepare`]).
#[derive(Debug, Clone)]
pub struct Prepared {
    pub plan: TransferPlan,
    /// In `plan.tables` order.
    pub tables: Vec<PreparedTable>,
}

/// Builds the plan (module docs, *Plan*). Reads catalogs only.
pub async fn plan(inputs: PlanInputs<'_>, cancel: CancellationToken) -> Result<TransferPlan> {
    prepare(inputs, cancel).await.map(|p| p.plan)
}

/// A source object as the plan reads it.
struct SourceTable {
    /// Its definition (a table's fetched one; for views and queries one built from the
    /// result's Arrow schema). `obj` carries the object index as its signature, so every
    /// source is a distinct node of the dependency graph.
    def: TableDef,
    select: BoundStatement,
    /// A fetched table (foreign keys and keys are known).
    is_table: bool,
}

/// [`plan`] plus what the run needs (module docs, *Plan*). Reads catalogs only; a query
/// source runs until its result's schema is known.
pub async fn prepare(inputs: PlanInputs<'_>, cancel: CancellationToken) -> Result<Prepared> {
    let PlanInputs {
        dialects,
        source,
        target,
        spec,
        same_server,
    } = inputs;
    spec.validate()?;
    let (se, te) = (source.engine(), target.engine());
    for (engine, role) in [(se, "source"), (te, "target")] {
        if !engine.is_sql() || !dialects.has_type_mapper(engine) {
            return Err(Error::unsupported(format!(
                "{} cannot be a transfer {role} (data transfer copies tables between SQL \
                 engines)",
                engine.display_name()
            )));
        }
    }
    let source_mapper = dialects.type_mapper(se)?;
    let target_mapper = dialects.type_mapper(te)?;
    let sv = source.server_version().clone();
    let tv = target.server_version().clone();
    let ctx = MappingContext {
        source_engine: se,
        source_version: &sv,
        source_mapper: source_mapper.as_ref(),
        target_engine: te,
        target_version: &tv,
        target_mapper: target_mapper.as_ref(),
    };
    check_container(dialects, target, &spec.target, &cancel).await?;
    let target_quoting = dialects.quoting(te)?;
    let terminator = target_quoting.script_terminator();
    let generator = dialects.ddl(te, ObjectKind::Table)?;
    let fk_generator = dialects.ddl(te, ObjectKind::ForeignKey).ok();
    let table_fetcher = dialects.fetcher(te, ObjectKind::Table)?;
    let loader = dialects.bulk_loader(te)?;
    // Without a capabilities role, claim the safe thing: DDL may commit at once.
    let transactional_ddl = dialects
        .capabilities(te)
        .is_ok_and(|caps| caps.designer(&tv).transactional_ddl);
    let rules = ctx.target_rules();
    let attached = dialects
        .tree_layout(te)
        .is_ok_and(|l| l.scope == DatabaseScope::Attached);
    let case = case_rules(te);
    let options = &spec.options;
    let ddl_ctx = DdlContext::new(&tv).with_exact_values();

    let mut warnings: Vec<String> = Vec::new();
    let mut blocked: Vec<String> = Vec::new();

    // 1. The source objects.
    let mut sources: Vec<SourceTable> = Vec::with_capacity(spec.objects.len());
    for (index, object) in spec.objects.iter().enumerate() {
        check(&cancel)?;
        let mut read = read_source(dialects, source, object, &cancel).await?;
        read.def.obj.signature = Some(format!("#{index}"));
        sources.push(read);
    }

    // 2. Per object: columns, target table, mode checks, DDL.
    let mut tables: Vec<TablePlan> = Vec::with_capacity(sources.len());
    let mut prepared: Vec<PreparedTable> = Vec::with_capacity(sources.len());
    for (index, (object, src)) in spec.objects.iter().zip(&sources).enumerate() {
        check(&cancel)?;
        let name = object.target_name().to_owned();
        let target_ref = spec.target.child(ObjectKind::Table, name.clone());
        let mut table_warnings = Vec::new();
        let overrides: Vec<_> = spec
            .overrides
            .iter()
            .filter(|o| o.object == index)
            .collect();
        for o in &overrides {
            if src.def.column(&o.column).is_none() {
                table_warnings.push(format!(
                    "the override of column '{}' names no column of the source",
                    o.column
                ));
            }
        }
        let mut columns = Vec::with_capacity(src.def.columns.len());
        for column in &src.def.columns {
            let described = if src.is_table {
                describe_type(dialects, source, &src.def.obj, column, &cancel).await
            } else {
                column.clone()
            };
            let o = overrides.iter().find(|o| o.column == column.name).copied();
            let mut plan = map_column(&ctx, &described, o);
            if options.identity == IdentityMode::Generate && column.auto_increment {
                plan.skipped = true;
            }
            if ctx.same_engine() && column.generated.is_some() && !plan.skipped {
                plan.skipped = true;
                table_warnings.push(format!(
                    "column {}: computed on the target too, so its values are not copied",
                    column.name
                ));
            }
            columns.push(plan);
        }
        if src.is_table {
            bound_key_columns(&ctx, &src.def, &mut columns);
        }

        // Does the target table exist?
        let existing = match table_fetcher
            .load(target, &target_ref, cancel.clone())
            .await
        {
            Ok(ObjectDef::Table(t)) => Some(t),
            Ok(_) => None,
            Err(e) if e.category() == ErrorCategory::NotFound => None,
            Err(e) => return Err(e),
        };
        let exists = existing.is_some();
        let mode = options.mode;
        let label = target_ref.display_name();
        if same_server
            && spec
                .objects
                .iter()
                .filter_map(SourceObject::source_ref)
                .any(|r| same_object(case, r, &target_ref))
        {
            blocked.push(format!(
                "{label} is a source of this transfer: a table cannot be copied onto itself \
                 (choose another target schema or name)"
            ));
        }
        let mut ddl: Vec<ScriptStep> = Vec::new();
        let mut ddl_preview: Vec<String> = Vec::new();
        let mut drop_created = Vec::new();
        let mut row_security = None;
        let creates = matches!(mode, TableMode::Create | TableMode::Replace);
        let definition = match (mode, &existing) {
            (TableMode::Create, Some(def)) => {
                blocked.push(format!(
                    "{label} already exists on the target: choose replace, append or truncate"
                ));
                def.clone()
            }
            (TableMode::Append | TableMode::Truncate, None) => {
                blocked.push(format!(
                    "{label} does not exist on the target: choose create or replace"
                ));
                TableDef::new(target_ref.clone())
            }
            (TableMode::Append | TableMode::Truncate, Some(def)) => {
                match_existing(&ctx, def, &mut columns, &mut blocked);
                if mode == TableMode::Truncate {
                    let step = truncate_step(&target_quoting.qualify(&target_ref), &name, attached);
                    let step = if transactional_ddl {
                        step
                    } else {
                        step.non_transactional()
                    };
                    ddl_preview.push(format!("{}{terminator}\n", step.sql));
                    ddl.push(step);
                }
                def.clone()
            }
            (TableMode::Create | TableMode::Replace, _) => {
                let (def, table_notes) =
                    match target_table(&ctx, &src.def, target_ref.clone(), &columns, options) {
                        Ok(made) => made,
                        Err(e) => {
                            blocked.push(format!("{label}: {e}"));
                            (TableDef::new(target_ref.clone()), Vec::new())
                        }
                    };
                table_warnings.extend(table_notes);
                if mode == TableMode::Replace && exists {
                    match generator.drop_object(&target_ref, &ddl_ctx) {
                        Ok(script) => {
                            ddl_preview.push(script.to_sql(terminator));
                            table_warnings.extend(script.warnings.iter().cloned());
                            ddl.extend(script.ordered().cloned().map(ScriptStep::destructive));
                        }
                        Err(e) => blocked.push(format!("{label}: {e}")),
                    }
                }
                if !def.columns.is_empty() {
                    // Row-level security (same-engine copies keep it) is switched on after the
                    // load: the table is created without it.
                    let bare = def.without_row_security();
                    let created = bare.as_ref().unwrap_or(&def);
                    match generator.create(&ObjectDef::Table(created.clone()), &ddl_ctx) {
                        Ok(script) => {
                            ddl_preview.push(script.to_sql(terminator));
                            table_warnings.extend(script.warnings.iter().cloned());
                            ddl.extend(script.ordered().cloned());
                        }
                        Err(e) => blocked.push(format!("{label}: {e}")),
                    }
                    if let Some(bare) = bare {
                        match generator.alter(
                            &ObjectDef::Table(bare),
                            &ObjectDef::Table(def.clone()),
                            &ddl_ctx,
                        ) {
                            Ok(script) => {
                                ddl_preview.push(script.to_sql(terminator));
                                table_warnings.extend(script.warnings.iter().cloned());
                                row_security = Some(script);
                            }
                            Err(e) => blocked.push(format!("{label}: {e}")),
                        }
                    }
                    if let Ok(script) = generator.drop_object(&target_ref, &ddl_ctx) {
                        drop_created = script.ordered().cloned().collect();
                    }
                }
                def
            }
        };
        for plan in columns.iter().filter(|p| !p.skipped) {
            if plan.mapping.fidelity == Fidelity::Unsupported {
                blocked.push(format!(
                    "{label}.{}: {}; skip the column or override its type",
                    plan.source,
                    plan.mapping.reason.as_deref().unwrap_or("no target type")
                ));
            }
        }
        for (i, a) in columns.iter().enumerate().filter(|(_, p)| !p.skipped) {
            if columns[..i]
                .iter()
                .filter(|p| !p.skipped)
                .any(|b| same_name(case.columns, &a.target, &b.target))
            {
                blocked.push(format!(
                    "{label}: two columns are named '{}' on {}",
                    a.target,
                    te.display_name()
                ));
            }
        }
        let fidelity = columns
            .iter()
            .filter(|p| !p.skipped)
            .fold(Fidelity::Exact, |worst, p| worst.worst(p.mapping.fidelity));
        let load_columns: Vec<LoadColumn> = columns
            .iter()
            .filter(|p| !p.skipped)
            .map(|p| load_column(&definition, p))
            .collect();
        let identity_columns: Vec<ColumnDef> = definition
            .columns
            .iter()
            .filter(|c| c.auto_increment && load_columns.iter().any(|l| l.name == c.name))
            .cloned()
            .collect();
        let ddl_in_transaction = transactional_ddl
            && ddl
                .iter()
                .all(|s| s.transactional && s.phase == StepPhase::Main);
        if options.fk_checks == FkChecks::Disable && exists && mode != TableMode::Replace {
            if loader.foreign_key_checks(&target_ref, false).is_none() {
                table_warnings.push(format!(
                    "{} cannot switch foreign-key checks off for a session: {label} loads with \
                     its foreign keys checked",
                    te.display_name()
                ));
            } else if !rules.fk_reenable_validates {
                table_warnings.push(format!(
                    "{label} loads with foreign-key checks off, and {} does not check the \
                     loaded rows when they are switched back on: rows without a parent stay",
                    te.display_name()
                ));
            }
        }
        tables.push(TablePlan {
            object: index,
            source: object.source_ref().cloned(),
            target: target_ref,
            exists,
            mode,
            columns,
            fidelity,
            ddl: ddl_preview,
            foreign_keys: Vec::new(),
            estimated_rows: None,
            warnings: table_warnings,
        });
        prepared.push(PreparedTable {
            definition,
            ddl,
            ddl_in_transaction,
            creates,
            drop_created,
            foreign_keys: Vec::new(),
            row_security,
            select: src.select.clone(),
            columns: load_columns,
            identity_columns,
        });
    }

    // 3. Name clashes under the target's case rules.
    for (i, a) in tables.iter().enumerate() {
        for b in &tables[..i] {
            if same_name(case.relations, &a.target.name, &b.target.name) {
                blocked.push(format!(
                    "'{}' and '{}' are the same table on {}",
                    b.target.name,
                    a.target.name,
                    te.display_name()
                ));
            }
        }
    }

    // 4. Foreign keys between the transferred tables, added after the load.
    if options.create_foreign_keys {
        for (i, source_table) in sources.iter().enumerate() {
            if !source_table.is_table || !prepared[i].creates {
                continue;
            }
            for fk in &source_table.def.foreign_keys {
                let fk_label = fk.name.clone().unwrap_or_else(|| fk.columns.join(", "));
                let Some(parent) = sources
                    .iter()
                    .position(|s| s.is_table && same_table(&fk.ref_table, &s.def.obj))
                else {
                    tables[i].warnings.push(format!(
                        "foreign key {fk_label}: not copied ({} is not part of the transfer)",
                        fk.ref_table.display_name()
                    ));
                    continue;
                };
                let Some(mut planned) =
                    planned_foreign_key(fk, &tables[i], &tables[parent], ctx.same_engine())
                else {
                    tables[i].warnings.push(format!(
                        "foreign key {fk_label}: not copied (one of its columns is left out)"
                    ));
                    continue;
                };
                let Some(fk_generator) = &fk_generator else {
                    tables[i].warnings.push(format!(
                        "foreign key {fk_label}: not copied ({} has no foreign-key DDL)",
                        te.display_name()
                    ));
                    continue;
                };
                // Foreign-key names are unique per schema on some engines: new names across
                // engines and next to the source.
                if !ctx.same_engine() || same_container(&source_table.def.obj, &tables[i].target) {
                    planned.name = planned.name.as_deref().map(|n| {
                        constraint_name(&source_table.def.obj.name, &tables[i].target.name, n)
                    });
                }
                let def = ObjectDef::ForeignKey {
                    table: tables[i].target.clone(),
                    foreign_key: planned,
                };
                let fk_ctx = ddl_ctx.with_table(&prepared[i].definition);
                match fk_generator.create(&def, &fk_ctx) {
                    Ok(script) => {
                        tables[i].foreign_keys.push(script.to_sql(terminator));
                        prepared[i].foreign_keys.push(script);
                    }
                    Err(e) => tables[i]
                        .warnings
                        .push(format!("foreign key {fk_label}: not copied ({e})")),
                }
            }
        }
    }
    if options.identity == IdentityMode::Generate
        && sources.iter().any(|s| !s.def.foreign_keys.is_empty())
    {
        warnings.push(
            "identity values are generated anew on the target: foreign keys that point at \
             them no longer match"
                .to_owned(),
        );
    }

    // 5. Load order from the source foreign keys.
    let defs: Vec<TableDef> = sources.iter().map(|s| s.def.clone()).collect();
    let order = table_dependencies(&defs).order();
    let index_of = |r: &ObjectRef| defs.iter().position(|d| d.obj == *r);
    let load_order: Vec<usize> = order.order.iter().filter_map(index_of).collect();
    let cycles: Vec<Vec<usize>> = order
        .cycles
        .iter()
        .map(|group| group.iter().filter_map(index_of).collect())
        .collect();
    if !cycles.is_empty() {
        warnings.push(
            "some tables reference each other: they load in the listed order, and their \
             foreign keys are added after every table is loaded"
                .to_owned(),
        );
    }
    if !transactional_ddl && tables.iter().any(|t| t.mode != TableMode::Append) {
        warnings.push(format!(
            "{} commits each DDL statement at once: a table the transfer created is dropped \
             again when its load fails",
            te.display_name()
        ));
    }
    let fidelity = tables
        .iter()
        .fold(Fidelity::Exact, |worst, t| worst.worst(t.fidelity));
    let destructive = tables
        .iter()
        .any(|t| t.exists && matches!(t.mode, TableMode::Replace | TableMode::Truncate));
    let mut seen = HashSet::new();
    blocked.retain(|b| seen.insert(b.clone()));
    Ok(Prepared {
        plan: TransferPlan {
            source_engine: se,
            target_engine: te,
            tables,
            order: load_order,
            cycles,
            fidelity,
            destructive,
            transactional_ddl,
            runnable: blocked.is_empty(),
            blocked,
            warnings,
        },
        tables: prepared,
    })
}

/// `true` when `a` and `b` name the same table under `case` (a missing database or schema
/// part matches any).
fn same_object(case: CaseRules, a: &ObjectRef, b: &ObjectRef) -> bool {
    let part = |x: &Option<String>, y: &Option<String>| match (x, y) {
        (Some(x), Some(y)) => same_name(case.schemas, x, y),
        _ => true,
    };
    same_name(case.relations, &a.name, &b.name)
        && part(&a.schema, &b.schema)
        && part(&a.catalog, &b.catalog)
}

fn check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// `NotFound` when the target database or schema does not exist (engines without a fetcher
/// for the container kind are not asked). A target that is a database on an engine whose
/// session sees one database (schemas hold the tables) is refused.
async fn check_container(
    dialects: &DialectRegistry,
    target: &dyn Connection,
    container: &ObjectRef,
    cancel: &CancellationToken,
) -> Result<()> {
    let engine = target.engine();
    if container.kind == ObjectKind::Database
        && dialects
            .tree_layout(engine)
            .is_ok_and(|l| l.scope == DatabaseScope::Session)
    {
        return Err(Error::invalid_input(format!(
            "on {} the transfer target is a schema, not a database",
            engine.display_name()
        )));
    }
    let Ok(fetcher) = dialects.fetcher(engine, container.kind) else {
        return Ok(());
    };
    match fetcher.load(target, container, cancel.clone()).await {
        Err(e) if e.category() == ErrorCategory::NotFound => Err(Error::not_found(format!(
            "the target {} {} does not exist",
            if container.kind == ObjectKind::Schema {
                "schema"
            } else {
                "database"
            },
            container.display_name()
        ))),
        Err(e) if e.is_cancelled() => Err(e),
        _ => Ok(()),
    }
}

/// Reads one source object's definition and its `SELECT` (module docs, *Plan* step 2).
async fn read_source(
    dialects: &DialectRegistry,
    source: &dyn Connection,
    object: &SourceObject,
    cancel: &CancellationToken,
) -> Result<SourceTable> {
    let engine = source.engine();
    let quoting = dialects.quoting(engine)?;
    match object {
        SourceObject::Table { table, filter, .. } => {
            let fetcher = dialects.fetcher(engine, ObjectKind::Table)?;
            let ObjectDef::Table(def) = fetcher.load(source, table, cancel.clone()).await? else {
                return Err(Error::internal(format!(
                    "the table fetcher returned something else for {}",
                    table.display_name()
                )));
            };
            let mut query = TableDataQuery::new(table.clone());
            query.filter.clone_from(filter);
            let select = select_table_data(quoting.as_ref(), &query, Some(&def))?;
            Ok(SourceTable {
                def,
                select,
                is_table: true,
            })
        }
        SourceObject::View { view, filter, .. } => {
            let mut probe = TableDataQuery::new(view.clone());
            probe.limit = Some(0);
            let probe = select_table_data(quoting.as_ref(), &probe, None)?;
            let schema = result_schema(source, &probe, cancel).await?;
            let mut query = TableDataQuery::new(view.clone());
            query.filter.clone_from(filter);
            let select = select_table_data(quoting.as_ref(), &query, None)?;
            let mut obj = view.clone();
            obj.kind = ObjectKind::Table;
            Ok(SourceTable {
                def: table_from_schema(obj, &schema),
                select,
                is_table: false,
            })
        }
        SourceObject::Query { sql, target_name } => {
            let select = BoundStatement::new(sql.clone(), Vec::new());
            let schema = query_schema(source, &select, cancel).await?;
            Ok(SourceTable {
                def: table_from_schema(
                    ObjectRef::new(ObjectKind::Table, target_name.clone()),
                    &schema,
                ),
                select,
                is_table: false,
            })
        }
    }
}

/// A table definition from a result's Arrow schema (views and queries): one column per field,
/// typed with `oxbase_io::logical_type`, no native types, no keys.
fn table_from_schema(obj: ObjectRef, schema: &SchemaRef) -> TableDef {
    let mut def = TableDef::new(obj);
    def.columns = schema
        .fields()
        .iter()
        .map(|f| {
            let mut c = ColumnDef::new(f.name().clone(), oxbase_io::logical_type(f));
            c.nullable = f.is_nullable();
            c
        })
        .collect();
    def
}

/// The Arrow schema of the first result set of `statement` on `connection`. The statement is
/// cancelled once the schema is known and the rest of its stream drained.
async fn result_schema(
    connection: &dyn Connection,
    statement: &BoundStatement,
    cancel: &CancellationToken,
) -> Result<SchemaRef> {
    let token = cancel.child_token();
    let stream = connection
        .execute(&statement.sql, &statement.params, token.clone())
        .await?;
    first_schema(stream, &token).await
}

/// [`result_schema`] of a user's query, inside a transaction that is rolled back whatever
/// happens: planning (the dry run) must not change data, and the query may write.
async fn query_schema(
    connection: &dyn Connection,
    statement: &BoundStatement,
    cancel: &CancellationToken,
) -> Result<SchemaRef> {
    let tx = cancellable(cancel, connection.begin()).await?;
    let token = cancel.child_token();
    let result = async {
        let stream = tx
            .execute(&statement.sql, &statement.params, token.clone())
            .await?;
        first_schema(stream, &token).await
    }
    .await;
    let _ = tokio::time::timeout(ROLLBACK_WAIT, tx.rollback()).await;
    result
}

/// The schema of the first result set of `stream`; then cancels it (`token`) and drains the
/// rest.
async fn first_schema(mut stream: ResultStream, token: &CancellationToken) -> Result<SchemaRef> {
    let mut found = None;
    while let Some(item) = stream.next().await {
        match item? {
            ResultItem::Schema(schema) => {
                found = Some(schema);
                break;
            }
            ResultItem::Batch(batch) => {
                found = Some(batch.schema());
                break;
            }
            ResultItem::Notice(_) | ResultItem::RowsAffected(_) => {}
        }
    }
    token.cancel();
    drain(stream).await;
    found.ok_or_else(|| {
        Error::invalid_input("the source statement returns no rows (it must be a SELECT)")
    })
}

/// Reads the rest of an abandoned stream (bounded), so its session can run the next statement.
async fn drain(stream: ResultStream) {
    drain_for(stream, DRAIN_WAIT).await;
}

/// [`drain`] for at most `wait`.
async fn drain_for(mut stream: ResultStream, wait: Duration) {
    let _ = tokio::time::timeout(wait, async {
        while let Some(item) = stream.next().await {
            if item.is_err() {
                break;
            }
        }
    })
    .await;
}

/// `column` with the extras of its engine-specific type merged in (an enum's labels, a domain's
/// base type), when the source engine fetches type objects; else `column` as it is. The
/// engine's `TypeMapper::logical` reads them.
async fn describe_type(
    dialects: &DialectRegistry,
    source: &dyn Connection,
    table: &ObjectRef,
    column: &ColumnDef,
    cancel: &CancellationToken,
) -> ColumnDef {
    let mut out = column.clone();
    if !matches!(column.data_type, DataType::Other { .. }) {
        return out;
    }
    let Ok(fetcher) = dialects.fetcher(source.engine(), ObjectKind::Type) else {
        return out;
    };
    let (schema, name) = split_type_name(&column.native_type, table.schema.as_deref());
    if name.is_empty() {
        return out;
    }
    let mut r = ObjectRef::new(ObjectKind::Type, name);
    r.catalog.clone_from(&table.catalog);
    r.schema = schema;
    if let Ok(def) = fetcher.load(source, &r, cancel.clone()).await {
        for (prop, value) in def.extras().iter() {
            if out.extras.get(prop).is_none() {
                out.extras.insert(prop.clone(), value.clone());
            }
        }
    }
    out
}

/// `schema.name` (each part optionally double-quoted) → (schema, name); a bare name lives in
/// `default_schema`.
fn split_type_name(native: &str, default_schema: Option<&str>) -> (Option<String>, String) {
    let text = native.trim();
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                current.push('"');
            }
            '"' => quoted = !quoted,
            '.' if !quoted => parts.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    parts.push(current);
    let name = parts.pop().unwrap_or_default();
    let schema = parts.pop().or_else(|| default_schema.map(str::to_owned));
    (schema, name)
}

/// Append / truncate into `existing`: every copied column must exist (matched by target name
/// under the target's column case rules); its type there decides the mapping.
fn match_existing(
    ctx: &MappingContext<'_>,
    existing: &TableDef,
    columns: &mut [ColumnPlan],
    blocked: &mut Vec<String>,
) {
    let case = case_rules(ctx.target_engine).columns;
    for plan in columns.iter_mut().filter(|p| !p.skipped) {
        let Some(column) = existing
            .columns
            .iter()
            .find(|c| same_name(case, &plan.target, &c.name))
        else {
            blocked.push(format!(
                "{} has no column '{}'",
                existing.obj.display_name(),
                plan.target
            ));
            continue;
        };
        plan.target.clone_from(&column.name);
        let data_type = ctx.target_mapper.logical(column, ctx.target_version);
        let (fidelity, reason) = fidelity_between(&plan.source_type, &data_type);
        plan.mapping = TypeMapping {
            data_type,
            native_type: column.native_type.clone(),
            fidelity,
            reason,
        };
        utf16_fidelity(ctx, &plan.source_type, &mut plan.mapping);
    }
}

/// `TRUNCATE TABLE q`, or `DELETE FROM q` on engines whose databases are attached files (no
/// `TRUNCATE` statement). Destructive.
fn truncate_step(qualified: &str, name: &str, attached: bool) -> ScriptStep {
    let sql = if attached {
        format!("DELETE FROM {qualified}")
    } else {
        format!("TRUNCATE TABLE {qualified}")
    };
    ScriptStep::new(sql)
        .described(format!("Empty table {name}"))
        .destructive()
}

/// The load column of `plan` in `definition` (the target table).
fn load_column(definition: &TableDef, plan: &ColumnPlan) -> LoadColumn {
    match definition.column(&plan.target) {
        Some(c) => LoadColumn {
            name: c.name.clone(),
            data_type: plan.mapping.data_type.clone(),
            native_type: c.native_type.clone(),
            nullable: c.nullable,
        },
        None => LoadColumn {
            name: plan.target.clone(),
            data_type: plan.mapping.data_type.clone(),
            native_type: plan.mapping.native_type.clone(),
            nullable: true,
        },
    }
}

/// Foreign key `fk` of `child` re-aimed at `parent` with target column names; `None` when one
/// of its columns is not copied.
fn planned_foreign_key(
    fk: &ForeignKeyDef,
    child: &TablePlan,
    parent: &TablePlan,
    same_engine: bool,
) -> Option<ForeignKeyDef> {
    let target_of = |plans: &[ColumnPlan], name: &str| {
        plans
            .iter()
            .find(|p| p.source == name && !p.skipped)
            .map(|p| p.target.clone())
    };
    let columns: Option<Vec<String>> = fk
        .columns
        .iter()
        .map(|c| target_of(&child.columns, c))
        .collect();
    let ref_columns: Option<Vec<String>> = fk
        .ref_columns
        .iter()
        .map(|c| target_of(&parent.columns, c))
        .collect();
    Some(ForeignKeyDef {
        name: fk.name.clone(),
        columns: columns?,
        ref_table: parent.target.clone(),
        ref_columns: ref_columns?,
        on_delete: fk.on_delete,
        on_update: fk.on_update,
        extras: if same_engine {
            fk.extras.clone()
        } else {
            Extras::new()
        },
    })
}

/// Cumulative counters of one table (lossy progress: consumers must not rely on every event).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableProgress {
    /// Index into `TransferPlan::tables`.
    pub table: usize,
    pub rows_read: u64,
    pub rows_written: u64,
    pub rows_committed: u64,
    pub rows_skipped: u64,
    /// Bytes of the source batches read (Arrow memory size).
    pub bytes: u64,
    pub errors: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_rows: Option<u64>,
    pub elapsed_ms: u64,
}

/// How a table ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableStatus {
    Done,
    Failed,
    Cancelled,
    /// Not started (an earlier table failed with `ErrorMode::Stop`, or the transfer was
    /// cancelled before it).
    NotRun,
}

/// The outcome of one table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableReport {
    pub table: usize,
    pub target: ObjectRef,
    pub status: TableStatus,
    pub progress: TableProgress,
    /// Reported row issues (at most `TransferOptions::max_reported_errors`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<RowIssue>,
    /// More issues happened than the report keeps.
    #[serde(default)]
    pub issues_truncated: bool,
    /// Why it failed (the error message; never connection details).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What the target table holds now, in words ("rolled back: the table is as before",
    /// "3 of 5 batches committed (30 000 rows)", "the created table was dropped").
    pub left_behind: String,
}

/// The outcome of a whole transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferReport {
    /// In `TransferPlan::tables` order.
    pub tables: Vec<TableReport>,
    /// Foreign keys added after the load.
    pub foreign_keys_added: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub elapsed_ms: u64,
}

/// Everything [`copy_table`] needs for one table.
#[derive(Clone, Copy)]
pub struct CopyTable<'a> {
    pub plan: &'a TablePlan,
    /// The DDL steps to run first (from the plan's generator call; executed in order).
    pub ddl: &'a [BoundStatement],
    /// A dedicated source session.
    pub source: &'a dyn Connection,
    /// The rows to copy (`SELECT` with bound values).
    pub select: &'a BoundStatement,
    /// A dedicated target session (the loader's `prepare` changes session settings).
    pub target: &'a dyn Connection,
    pub target_version: &'a ServerVersion,
    pub loader: &'a dyn BulkLoader,
    /// The load columns (not skipped), in load order.
    pub columns: &'a [LoadColumn],
    pub options: &'a TransferOptions,
}

/// Copies one table (module docs, *Run*). Returns its report (also for a failed or cancelled
/// table: `Err` only when the report itself cannot be made, e.g. the target session is gone
/// before anything ran).
///
/// The `ddl` statements run inside the table's first transaction (module docs, *How the run
/// uses its transactions*).
pub async fn copy_table(
    job: CopyTable<'_>,
    on_progress: &(dyn Fn(&TableProgress) + Send + Sync),
    cancel: CancellationToken,
) -> Result<TableReport> {
    let started = Instant::now();
    let mut run = Copier {
        job,
        on_progress,
        cancel,
        started,
        last_progress: Instant::now(),
        progress: TableProgress {
            table: job.plan.object,
            total_rows: job.plan.estimated_rows,
            ..TableProgress::default()
        },
        issues: Vec::new(),
        issues_truncated: false,
        tx: None,
        ddl_in_open_tx: false,
        ddl_committed: false,
        pending: Vec::new(),
        loader_prepared: false,
        fk_off: false,
        fk_restore_error: None,
        deadline: None,
        stream: Mutex::new(None),
    };
    let source_token = run.cancel.child_token();
    let outcome = run.run(&source_token).await;
    if outcome.is_err() {
        if run.cancel.is_cancelled() {
            // Clean-up after a cancel is bounded as a whole.
            run.deadline = Some(Instant::now() + CANCEL_BUDGET);
        }
        // Stop the source statement and read what is left of it, so the source session can
        // run the next table's statement.
        source_token.cancel();
        let wait = run.wait(DRAIN_WAIT);
        if let Some(stream) = run
            .stream
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            drain_for(stream, wait).await;
        }
        run.abort().await;
    }
    run.clean_up(outcome.is_err()).await;
    Ok(run.report(&outcome))
}

/// The state of one [`copy_table`] run.
struct Copier<'a> {
    job: CopyTable<'a>,
    on_progress: &'a (dyn Fn(&TableProgress) + Send + Sync),
    cancel: CancellationToken,
    started: Instant,
    last_progress: Instant,
    progress: TableProgress,
    issues: Vec<RowIssue>,
    issues_truncated: bool,
    tx: Option<Box<dyn Tx>>,
    /// The DDL ran inside the open transaction.
    ddl_in_open_tx: bool,
    /// The DDL is in the table for good (its transaction committed).
    ddl_committed: bool,
    /// Converted rows waiting for a load call.
    pending: Vec<RecordBatch>,
    loader_prepared: bool,
    /// Foreign-key checks are off on the target session.
    fk_off: bool,
    /// Switching foreign-key checks back on failed (the table report says so).
    fk_restore_error: Option<String>,
    /// After a cancel: when the clean-up must be over ([`CANCEL_BUDGET`]).
    deadline: Option<Instant>,
    /// The source rows (in a mutex only so the copier is `Sync`: a stream is `Send` but not
    /// `Sync`, and the copier is borrowed across awaits; it is only ever used through
    /// `&mut self`, so the lock is never contended).
    stream: Mutex<Option<ResultStream>>,
}

impl Copier<'_> {
    fn load_target(&self) -> LoadTarget<'_> {
        LoadTarget {
            table: &self.job.plan.target,
            columns: self.job.columns,
            version: self.job.target_version,
        }
    }

    /// Rows per `BulkLoader::load` call (and per commit with `TransactionScope::PerBatch`).
    fn call_rows(&self) -> usize {
        let limits = self.job.loader.limits(self.job.target_version);
        self.job
            .options
            .batch_rows
            .max(1)
            .min(limits.rows_per_call.max(1))
    }

    async fn run(&mut self, source_token: &CancellationToken) -> Result<()> {
        let job = self.job;
        check(&self.cancel)?;
        // 1. The source rows, before anything changes on the target.
        let select = job.select;
        let stream = cancellable(
            &self.cancel,
            job.source
                .execute(&select.sql, &select.params, source_token.clone()),
        )
        .await?;
        *self
            .stream
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner) = Some(stream);
        let mut schema = None;
        let mut first: Option<RecordBatch> = None;
        while schema.is_none() {
            match self.next().await {
                None => break,
                Some(Err(e)) => return Err(e),
                Some(Ok(ResultItem::Schema(s))) => schema = Some(s),
                Some(Ok(ResultItem::Batch(b))) => {
                    schema = Some(b.schema());
                    first = Some(b);
                }
                Some(Ok(_)) => {}
            }
        }
        let Some(schema) = schema else {
            return Err(Error::invalid_input(
                "the source statement returns no rows (it must be a SELECT)",
            ));
        };
        let rules = target_rules(job.target.engine(), job.target_version);
        let converter =
            BatchConverter::new(schema, &job.plan.columns, job.columns.to_vec())?.with_rules(rules);

        // 2. Foreign-key checks off for a table that existed before (new tables get their
        //    foreign keys after the load).
        if job.options.fk_checks == FkChecks::Disable
            && job.plan.exists
            && job.plan.mode != TableMode::Replace
            && let Some(statements) = job.loader.foreign_key_checks(&job.plan.target, false)
        {
            self.fk_off = true;
            for sql in statements {
                self.step(&BoundStatement::new(sql, Vec::new()), false)
                    .await?;
            }
        }

        // 3. The transaction, the DDL in it, the loader's session settings.
        self.begin().await?;
        for statement in job.ddl {
            self.step(statement, true).await?;
            self.ddl_in_open_tx = true;
        }
        {
            let session = LoadSession::new(job.target, self.tx.as_deref());
            let target = self.load_target();
            let prepare = job.loader.prepare(session, &target, self.cancel.clone());
            cancellable(&self.cancel, prepare).await?;
        }
        self.loader_prepared = true;

        // 4. The rows.
        if let Some(batch) = first {
            self.batch(&converter, &batch).await?;
        }
        loop {
            match self.next().await {
                None => break,
                Some(Err(e)) => return Err(e),
                Some(Ok(ResultItem::Batch(batch))) => self.batch(&converter, &batch).await?,
                Some(Ok(ResultItem::Schema(_))) => {
                    return Err(Error::invalid_input(
                        "the source statement returns more than one result set",
                    ));
                }
                Some(Ok(_)) => {}
            }
        }
        *self
            .stream
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner) = None;
        self.flush(true).await?;
        self.commit().await?;
        self.report_progress(true);
        Ok(())
    }

    /// The next item of the source stream (`Cancelled` once the run is cancelled).
    async fn next(&mut self) -> Option<Result<ResultItem>> {
        let stream = self
            .stream
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()?;
        next_item(stream, &self.cancel).await
    }

    /// Runs one statement on the target: in the open transaction (`in_tx`) or on the session.
    async fn step(&self, statement: &BoundStatement, in_tx: bool) -> Result<()> {
        let tx = if in_tx { self.tx.as_deref() } else { None };
        let session = LoadSession::new(self.job.target, tx);
        cancellable(&self.cancel, session.run(statement, self.cancel.clone()))
            .await
            .map(|_| ())
    }

    async fn begin(&mut self) -> Result<()> {
        if !self.job.loader.transactional() {
            return Ok(());
        }
        let tx = cancellable(&self.cancel, self.job.target.begin()).await?;
        self.tx = Some(tx);
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        if let Some(tx) = self.tx.take() {
            cancellable(&self.cancel, tx.commit()).await?;
        }
        self.progress.rows_committed = self.progress.rows_written;
        if self.ddl_in_open_tx {
            self.ddl_in_open_tx = false;
            self.ddl_committed = true;
        }
        Ok(())
    }

    /// `limit`, cut to what is left of the cancel budget after a cancel.
    fn wait(&self, limit: Duration) -> Duration {
        match self.deadline {
            Some(deadline) => limit.min(deadline.saturating_duration_since(Instant::now())),
            None => limit,
        }
    }

    /// Rolls the open transaction back (bounded).
    async fn abort(&mut self) {
        let wait = self.wait(ROLLBACK_WAIT);
        if let Some(tx) = self.tx.take() {
            let _ = tokio::time::timeout(wait, tx.rollback()).await;
        }
        self.ddl_in_open_tx = false;
    }

    /// `BulkLoader::finish` (best effort after a failure) and foreign-key checks back on
    /// (a failure is kept for the report: the constraints may stay off, or the loaded rows
    /// broke them).
    async fn clean_up(&mut self, failed: bool) {
        let token = CancellationToken::new();
        let limit = if failed {
            CLEANUP_WAIT
        } else {
            Duration::from_secs(30)
        };
        if self.loader_prepared {
            let session = LoadSession::new(self.job.target, None);
            let target = self.load_target();
            let wait = self.wait(limit);
            let _ = tokio::time::timeout(
                wait,
                self.job.loader.finish(session, &target, token.clone()),
            )
            .await;
        }
        if self.fk_off
            && let Some(statements) = self
                .job
                .loader
                .foreign_key_checks(&self.job.plan.target, true)
        {
            let session = LoadSession::new(self.job.target, None);
            // Switching the checks back on matters even after a failure: at least a moment.
            let wait = self.wait(limit).max(Duration::from_millis(200));
            for sql in statements {
                let statement = BoundStatement::new(sql, Vec::new());
                let error = match tokio::time::timeout(wait, session.run(&statement, token.clone()))
                    .await
                {
                    Ok(Ok(_)) => continue,
                    Ok(Err(e)) => e.to_string(),
                    Err(_) => "it took too long".to_owned(),
                };
                self.fk_restore_error = Some(format!(
                    "switching foreign-key checks back on for {} failed: {error}",
                    self.job.plan.target.display_name()
                ));
                break;
            }
        }
    }

    /// One source batch: convert, apply the error mode, queue, load full calls.
    async fn batch(&mut self, converter: &BatchConverter, batch: &RecordBatch) -> Result<()> {
        let rows = batch.num_rows();
        let first_row = self.progress.rows_read + 1;
        self.progress.rows_read += u64::try_from(rows).unwrap_or(u64::MAX);
        self.progress.bytes += u64::try_from(batch.get_array_memory_size()).unwrap_or(u64::MAX);
        let converted = converter.convert(batch, first_row)?;
        let mut kept = converted.batch;
        if !converted.issues.is_empty() {
            self.progress.errors += u64::try_from(converted.issues.len()).unwrap_or(u64::MAX);
            if self.job.options.errors == ErrorMode::Stop {
                let first = converted.issues[0].clone();
                let message = match &first.column {
                    Some(column) => {
                        format!("row {}, column '{column}': {}", first.row, first.message)
                    }
                    None => format!("row {}: {}", first.row, first.message),
                };
                self.record(converted.issues);
                return Err(Error::invalid_input(message));
            }
            self.record(converted.issues);
            let mut keep = vec![true; rows];
            for row in &converted.bad_rows {
                if let Some(k) = keep.get_mut(*row) {
                    *k = false;
                }
            }
            self.progress.rows_skipped +=
                u64::try_from(converted.bad_rows.len()).unwrap_or(u64::MAX);
            kept = arrow_select::filter::filter_record_batch(&kept, &BooleanArray::from(keep))
                .map_err(|e| Error::internal(format!("dropping rows with issues failed: {e}")))?;
        }
        if kept.num_rows() > 0 {
            self.pending.push(kept);
        }
        self.flush(false).await?;
        self.report_progress(false);
        Ok(())
    }

    /// Keeps issues in the report (up to the cap).
    fn record(&mut self, issues: Vec<RowIssue>) {
        let cap = self.job.options.max_reported_errors;
        for issue in issues {
            if self.issues.len() < cap {
                self.issues.push(issue);
            } else {
                self.issues_truncated = true;
            }
        }
    }

    /// Loads full calls of queued rows (every queued row with `all`).
    async fn flush(&mut self, all: bool) -> Result<()> {
        let call_rows = self.call_rows();
        loop {
            let queued: usize = self.pending.iter().map(RecordBatch::num_rows).sum();
            if queued == 0 || (!all && queued < call_rows) {
                return Ok(());
            }
            let combined = if self.pending.len() == 1 {
                self.pending.remove(0)
            } else {
                let schema = self.pending[0].schema();
                let combined = arrow_select::concat::concat_batches(&schema, &self.pending)
                    .map_err(|e| Error::internal(format!("combining load batches failed: {e}")))?;
                self.pending.clear();
                combined
            };
            let take = combined.num_rows().min(call_rows);
            if take < combined.num_rows() {
                self.pending
                    .push(combined.slice(take, combined.num_rows() - take));
            }
            let chunk = combined.slice(0, take);
            self.load(&chunk).await?;
        }
    }

    /// One `BulkLoader::load` call, then a commit with `TransactionScope::PerBatch`.
    async fn load(&mut self, chunk: &RecordBatch) -> Result<()> {
        let job = self.job;
        {
            let session = LoadSession::new(job.target, self.tx.as_deref());
            let target = self.load_target();
            let load = job
                .loader
                .load(session, &target, chunk, self.cancel.clone());
            cancellable(&self.cancel, load).await?;
        }
        self.progress.rows_written += u64::try_from(chunk.num_rows()).unwrap_or(u64::MAX);
        if self.tx.is_none() {
            // Written for good (a loader without transactions).
            self.progress.rows_committed = self.progress.rows_written;
        }
        if job.options.transaction == TransactionScope::PerBatch {
            self.commit().await?;
            self.report_progress(true);
            self.begin().await?;
        }
        Ok(())
    }

    fn report_progress(&mut self, force: bool) {
        if !force && self.last_progress.elapsed() < PROGRESS_EVERY {
            return;
        }
        self.last_progress = Instant::now();
        self.progress.elapsed_ms = millis(self.started);
        (self.on_progress)(&self.progress);
    }

    fn report(&mut self, outcome: &Result<()>) -> TableReport {
        self.progress.elapsed_ms = millis(self.started);
        let (mut status, mut error) = match outcome {
            Ok(()) => (TableStatus::Done, None),
            Err(e) if e.is_cancelled() || self.cancel.is_cancelled() => {
                (TableStatus::Cancelled, None)
            }
            Err(e) => (TableStatus::Failed, Some(e.to_string())),
        };
        let mut left_behind = self.left_behind(outcome.is_ok());
        if let Some(fk) = &self.fk_restore_error {
            // The rows are in; the table's foreign keys may be off or broken by them.
            left_behind = format!(
                "{left_behind}; its foreign-key checks may still be off, or loaded rows break \
                 them: check the table's constraints"
            );
            if status == TableStatus::Done {
                status = TableStatus::Failed;
                error = Some(fk.clone());
            }
        }
        TableReport {
            table: self.job.plan.object,
            target: self.job.plan.target.clone(),
            status,
            progress: self.progress.clone(),
            issues: std::mem::take(&mut self.issues),
            issues_truncated: self.issues_truncated,
            error,
            left_behind,
        }
    }

    /// What the target table holds now, in words.
    fn left_behind(&self, ok: bool) -> String {
        let plan = self.job.plan;
        let table = plan.target.display_name();
        let committed = self.progress.rows_committed;
        if ok {
            return match plan.mode {
                TableMode::Append => format!("{committed} rows were added to {table}"),
                _ => format!("{table} holds the {committed} copied rows"),
            };
        }
        if committed > 0 {
            return format!(
                "{committed} rows of committed batches stay in {table}; the rest was rolled back"
            );
        }
        if self.ddl_committed || self.job.ddl.is_empty() {
            return match plan.mode {
                TableMode::Append => format!("nothing was added: {table} is as it was"),
                TableMode::Truncate if self.ddl_committed => {
                    format!("{table} was emptied and holds no copied rows")
                }
                TableMode::Truncate => format!("rolled back: {table} is as it was"),
                TableMode::Create | TableMode::Replace => {
                    format!("{table} exists and holds no copied rows")
                }
            };
        }
        match plan.mode {
            TableMode::Replace if plan.exists => {
                format!("rolled back: the old {table} is as it was")
            }
            TableMode::Create | TableMode::Replace => {
                "rolled back: no table was created".to_owned()
            }
            TableMode::Truncate | TableMode::Append => {
                format!("rolled back: {table} is as it was")
            }
        }
    }
}

fn millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Runs `fut` to its end, or, once `cancel` fires, gives it [`CANCEL_GRACE`] to stop and
/// returns `Cancelled`.
async fn cancellable<T>(
    cancel: &CancellationToken,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::pin!(fut);
    tokio::select! {
        biased;
        result = &mut fut => result,
        () = cancel.cancelled() => {
            let _ = tokio::time::timeout(CANCEL_GRACE, &mut fut).await;
            Err(Error::Cancelled)
        }
    }
}

/// The next item of `stream`, or `Cancelled` once `cancel` fires.
async fn next_item(
    stream: &mut ResultStream,
    cancel: &CancellationToken,
) -> Option<Result<ResultItem>> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Some(Err(Error::Cancelled)),
        item = stream.next() => item,
    }
}

/// Runs `steps` (a DDL or foreign-key script) on `connection` with the `oxbase_types::script`
/// semantics: prepare steps first, the main steps in one transaction when `atomic` (else each
/// on its own), finally steps whatever happened. For the services' DDL outside a table's
/// transaction and the foreign keys after the load.
pub async fn run_steps(
    connection: &dyn Connection,
    steps: &[ScriptStep],
    atomic: bool,
    cancel: &CancellationToken,
) -> Result<()> {
    let by_phase = |phase: StepPhase| steps.iter().filter(move |s| s.phase == phase);
    let mut result = Ok(());
    for step in by_phase(StepPhase::Prepare) {
        result = run_sql(connection, None, &step.sql, cancel).await;
        if result.is_err() {
            break;
        }
    }
    if result.is_ok() {
        if atomic {
            match cancellable(cancel, connection.begin()).await {
                Err(e) => result = Err(e),
                Ok(tx) => {
                    for step in by_phase(StepPhase::Main) {
                        result = run_sql(connection, Some(tx.as_ref()), &step.sql, cancel).await;
                        if result.is_err() {
                            break;
                        }
                    }
                    if result.is_ok() {
                        result = tx.commit().await;
                    } else {
                        let _ = tokio::time::timeout(ROLLBACK_WAIT, tx.rollback()).await;
                    }
                }
            }
        } else {
            for step in by_phase(StepPhase::Main) {
                result = run_sql(connection, None, &step.sql, cancel).await;
                if result.is_err() {
                    break;
                }
            }
        }
    }
    let always = CancellationToken::new();
    for step in by_phase(StepPhase::Finally) {
        let _ = tokio::time::timeout(
            CLEANUP_WAIT * 10,
            run_sql(connection, None, &step.sql, &always),
        )
        .await;
    }
    result
}

/// Runs `sql` (no parameters) through `tx` or on `connection`, to its end.
async fn run_sql(
    connection: &dyn Connection,
    tx: Option<&dyn Tx>,
    sql: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let statement = BoundStatement::new(sql, Vec::new());
    let session = LoadSession::new(connection, tx);
    cancellable(cancel, session.run(&statement, cancel.clone()))
        .await
        .map(|_| ())
}

/// `true` when `steps` can run inside one transaction (every main step transactional).
pub fn atomic_steps(steps: &[ScriptStep]) -> bool {
    steps
        .iter()
        .filter(|s| s.phase == StepPhase::Main)
        .all(|s| s.transactional)
}
