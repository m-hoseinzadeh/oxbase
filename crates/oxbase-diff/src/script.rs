//! Structure sync scripts (`docs/m8-contracts.md` §5.4): the DDL that makes the target look like
//! the source for the selected [`DiffItem`](crate::DiffItem)s, in dependency order. Owner of the
//! bodies marked `unimplemented!("m8: structure-sync")`: the **structure-sync** stream.
//!
//! # Steps
//!
//! Every statement comes from the engine's `DdlGenerator` cells for the item's kind
//! (`create` for `only_in_source`, `alter(target_def, source_def)` for `different`,
//! `drop_object` for `only_in_target` when drops are included), with a `DdlContext` holding the
//! server version, the table as it is now on the target and its triggers (the SQLite rebuild
//! path needs both), and `exact_values` off. A kind whose generator cannot alter in place
//! (views, routines, triggers on most engines) is dropped and re-created, or replaced
//! (`CREATE OR REPLACE`, `CREATE OR ALTER`) where the engine has it; that choice is the cell's.
//!
//! # Order
//!
//! 1. Drops of dependents of anything that changes or goes: triggers, then views and
//!    materialized views (reverse dependency order), then routines that are dropped or
//!    re-created.
//! 2. Foreign keys that change or go (on tables that stay), foreign keys that point at tables
//!    about to be dropped, and foreign keys (of any table that stays) that reference a key the
//!    script changes (a referenced column, the primary key or the unique key they use): the
//!    server refuses to change a referenced key, so they are dropped here and added again in
//!    step 6.
//! 3. Drops of tables only in the target (`include_drops`), children before parents.
//! 4. Sequences to create or alter, then routines that a table's default, check, generated
//!    column or index expression names (the table needs them first).
//! 5. Tables to create (without their foreign keys), parents first; then alters of the
//!    different tables (without foreign-key additions). The SQLite rebuild steps of one table
//!    stay together, with their prepare / finally phases.
//! 6. Foreign keys to add (new tables' and changed ones, and the ones of step 2 added back),
//!    after every table exists. Then drops of sequences only in the target (after the tables
//!    stopped using them in their defaults).
//! 7. Routines and views, materialized views in dependency order (a routine or view after the
//!    routines and views it names), then triggers.
//!
//! Dependencies between views and routines come from a token scan of their SQL for the names of
//! compared objects (`oxbase_sql` tokens; the structure-sync stream) fed to
//! `oxbase_dialect::DependencyGraph`; tables come from foreign keys
//! (`oxbase_dialect::table_dependencies`).
//!
//! # Partial selections
//!
//! A view that reads a table the script changes is dropped and created again even when it is
//! not selected: from the source definition when the view is selected, else from the target's
//! own definition (the script never applies a change nobody selected). A table drop that a
//! foreign key of a table that stays still references is refused (naming both), unless that
//! table's own selected alter drops the key.
//!
//! # Flags
//!
//! [`SyncScript::destructive`] when any step is (drops, narrowing changes, rebuilds);
//! [`SyncScript::sql`] renders a prepare or finally step that only runs on a condition
//! (SQLite restores the pragmas it found) as a comment naming the condition, so a saved script
//! never runs both restores;
//! [`SyncScript::atomic`] when every main step is transactional (`Script::is_atomic`) and the
//! engine runs DDL in transactions (PostgreSQL, SQL Server, SQLite; MySQL / MariaDB DDL commits
//! each statement, and the preview says so: [`SyncScript::transactional_ddl`] false).

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

use oxbase_dialect::compare::{DefaultRules, diff_objects};
use oxbase_dialect::{
    DdlContext, DdlGenerator, DependencyGraph, DialectRegistry, Support, same_table,
    table_dependencies,
};
use oxbase_sql::{SqlDialect, SqlTokenKind, ident_value, tokenize};
use oxbase_types::{
    Engine, Error, ForeignKeyDef, ObjectDef, ObjectKind, ObjectRef, Result, Script, ServerVersion,
    StepPhase, TableDef, TriggerDef,
};
use serde::{Deserialize, Serialize};

use crate::structure::{
    DiffStatus, Pair, SchemaSnapshot, StructureDiff, StructureOptions, item_id, pair_objects,
};

/// A generated structure sync script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncScript {
    pub engine: Engine,
    /// The steps, in execution order of the module docs (phases inside).
    pub script: Script,
    /// The whole script as text, statements ended with the engine's terminator
    /// (`Quoting::script_terminator`), for the preview and for saving.
    pub sql: String,
    /// The item ids the script covers, in step order of their first step.
    pub items: Vec<String>,
    pub destructive: bool,
    /// Runs as one transaction (module docs, *Flags*).
    pub atomic: bool,
    /// The engine runs DDL inside transactions.
    pub transactional_ddl: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl SyncScript {
    /// `true` when there is nothing to run.
    pub fn is_empty(&self) -> bool {
        self.script.is_empty()
    }
}

/// The script for the items `selection` of `diff` (empty selection: every item that is not
/// `same`, and `only_in_target` items only with `options.include_drops`). `source` and
/// `target` are the snapshots `diff` was computed from. `InvalidInput` for an unknown id;
/// `Unsupported` (naming the object) when a needed generator is missing.
pub fn sync_script(
    dialects: &DialectRegistry,
    diff: &StructureDiff,
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
    selection: &[String],
    options: &StructureOptions,
) -> Result<SyncScript> {
    let engine = target.engine;
    for id in selection {
        if diff.item(id).is_none() {
            return Err(Error::invalid_input(format!(
                "the compare has no item {id}; compare again"
            )));
        }
    }
    let pairs = pair_objects(source, target, options)?;
    let status_of: HashMap<&str, DiffStatus> = diff
        .items
        .iter()
        .map(|i| (i.id.as_str(), i.status))
        .collect();
    let chosen: Vec<(&Pair, DiffStatus)> = pairs
        .iter()
        .filter_map(|p| {
            let status = *status_of.get(p.id.as_str())?;
            let picked = if selection.is_empty() {
                match status {
                    DiffStatus::Same => false,
                    DiffStatus::OnlyInTarget => options.include_drops,
                    DiffStatus::OnlyInSource | DiffStatus::Different => true,
                }
            } else {
                status != DiffStatus::Same && selection.contains(&p.id)
            };
            picked.then_some((p, status))
        })
        .collect();

    let designer = dialects
        .capabilities(engine)
        .map(|c| c.designer(&target.version))
        .unwrap_or_default();
    let planner = Planner {
        dialects,
        engine,
        options,
        version: &target.version,
        native_fk: designer.add_foreign_key == Support::Native
            && designer.drop_foreign_key == Support::Native
            && dialects.ddl(engine, ObjectKind::ForeignKey).is_ok(),
    };
    let parts = planner.plan(&pairs, &chosen, target)?;

    let mut script = Script::new();
    let mut items: Vec<String> = Vec::new();
    let mut seen: HashSet<(StepPhase, String, Option<String>, Option<String>)> = HashSet::new();
    for part in parts {
        if !part.script.steps.is_empty() && !items.contains(&part.id) {
            items.push(part.id.clone());
        }
        for mut step in part.script.steps {
            if step.phase != StepPhase::Main {
                // One prepare / finally step of a kind is enough (SQLite rebuilds of two tables
                // both switch foreign keys off and restore them).
                let key = (
                    step.phase,
                    step.sql.clone(),
                    step.capture.clone(),
                    step.only_if
                        .as_ref()
                        .map(|c| format!("{}={}", c.capture, c.equals)),
                );
                if !seen.insert(key) {
                    continue;
                }
            }
            if part.recreate {
                // Dropped only to be created again right after the tables changed.
                step.destructive = false;
            }
            script.push_step(step);
        }
        for warning in part.script.warnings {
            if !script.warnings.contains(&warning) {
                script.warnings.push(warning);
            }
        }
    }
    if !designer.transactional_ddl && !script.is_empty() {
        script.warn(format!(
            "{} commits each DDL statement on its own: if a statement fails, the statements \
             before it stay applied",
            engine.display_name()
        ));
    }
    let terminator = dialects
        .quoting(engine)
        .map_or(";", |q| q.script_terminator());
    Ok(SyncScript {
        engine,
        sql: script_text(&script, terminator),
        items,
        destructive: script.is_destructive(),
        atomic: designer.transactional_ddl && script.is_atomic(),
        transactional_ddl: designer.transactional_ddl,
        warnings: script.warnings.clone(),
        script,
    })
}

/// The script as text (module docs, *Flags*): every step with the terminator, a conditional
/// prepare or finally step as a comment that names its condition.
fn script_text(script: &Script, terminator: &str) -> String {
    let mut out = String::new();
    for step in script.ordered() {
        match &step.only_if {
            Some(condition) if step.phase != StepPhase::Main => {
                let one_line: Vec<&str> = step.sql.split_whitespace().collect();
                let _ = writeln!(
                    out,
                    "-- only when {} = {}: {}",
                    condition.capture,
                    condition.equals,
                    one_line.join(" ")
                );
            }
            _ => {
                out.push_str(&step.sql);
                out.push_str(terminator);
                out.push('\n');
            }
        }
    }
    out
}

/// The phases of the module docs, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    /// 1. Triggers, views and routines that go or are re-created.
    DropDependents,
    /// 2. Foreign keys that change or go.
    DropForeignKeys,
    /// 3. Tables only in the target.
    DropTables,
    /// 4. Sequences to create or alter, routines the tables need.
    Sequences,
    /// 5. New tables (parents first), then changed tables.
    Tables,
    /// 6. Foreign keys to add.
    AddForeignKeys,
    /// 6. Sequences only in the target, once no table default uses them.
    DropSequences,
    /// 7. Routines, views, triggers.
    Dependents,
}

/// The script of one item in one phase.
struct Part {
    phase: Phase,
    id: String,
    script: Script,
    /// A drop of an object that is created again later in the script (not destructive).
    recreate: bool,
}

struct Planner<'a> {
    dialects: &'a DialectRegistry,
    engine: Engine,
    options: &'a StructureOptions,
    version: &'a ServerVersion,
    /// Foreign keys are added and dropped with their own statements (else they travel with
    /// their table: SQLite).
    native_fk: bool,
}

impl Planner<'_> {
    fn ddl(&self, kind: ObjectKind, r: &ObjectRef) -> Result<Arc<dyn DdlGenerator>> {
        self.dialects.ddl(self.engine, kind).map_err(|_| {
            Error::unsupported(format!(
                "{} has no DDL generator for {} {}",
                self.engine.display_name(),
                kind.display_name().to_lowercase(),
                r.display_name()
            ))
        })
    }

    fn ctx(&self) -> DdlContext<'_> {
        DdlContext::new(self.version)
    }

    fn create(&self, def: &ObjectDef) -> Result<Script> {
        let r = def.object_ref();
        self.ddl(def.kind(), &r)?.create(def, &self.ctx())
    }

    fn drop_object(&self, r: &ObjectRef) -> Result<Script> {
        self.ddl(r.kind, r)?.drop_object(r, &self.ctx())
    }

    fn alter(&self, old: &ObjectDef, new: &ObjectDef) -> Result<Script> {
        let r = new.object_ref();
        self.ddl(new.kind(), &r)?.alter(old, new, &self.ctx())
    }

    /// Two foreign keys are the same constraint (names, where both have one, and every
    /// compared property).
    fn same_fk(&self, table: &ObjectRef, old: &ForeignKeyDef, new: &ForeignKeyDef) -> bool {
        if old.name.is_some() && new.name.is_some() && old.name != new.name {
            return false;
        }
        let a = ObjectDef::ForeignKey {
            table: table.clone(),
            foreign_key: old.clone(),
        };
        let b = ObjectDef::ForeignKey {
            table: table.clone(),
            foreign_key: new.clone(),
        };
        match self
            .dialects
            .comparator(self.engine, ObjectKind::ForeignKey)
        {
            Ok(comparator) => comparator.diff(&a, &b).is_empty(),
            Err(_) => diff_objects(&a, &b, &DefaultRules).is_empty(),
        }
    }

    fn plan(
        &self,
        pairs: &[Pair],
        chosen: &[(&Pair, DiffStatus)],
        target: &SchemaSnapshot,
    ) -> Result<Vec<Part>> {
        let mut parts: Vec<Part> = Vec::new();
        let mut push = |phase: Phase, id: &str, script: Script, recreate: bool| {
            parts.push(Part {
                phase,
                id: id.to_owned(),
                script,
                recreate,
            });
        };
        let of_kinds = |kinds: &[ObjectKind]| {
            chosen
                .iter()
                .filter(|(p, _)| kinds.contains(&p.kind))
                .copied()
                .collect::<Vec<_>>()
        };
        let tables = of_kinds(&[ObjectKind::Table]);
        let views = of_kinds(&[ObjectKind::View, ObjectKind::MatView]);
        let sequences = of_kinds(&[ObjectKind::Sequence]);
        let routines = of_kinds(&[ObjectKind::Function, ObjectKind::Procedure]);
        let triggers = of_kinds(&[ObjectKind::Trigger]);

        // Names of what changes or goes, for the dependents.
        let mut changed_tables: HashSet<String> = HashSet::new();
        let mut dropped_tables: HashSet<String> = HashSet::new();
        for &(p, status) in &tables {
            match status {
                DiffStatus::Different => {
                    changed_tables.insert(p.name.to_lowercase());
                }
                DiffStatus::OnlyInTarget => {
                    changed_tables.insert(p.name.to_lowercase());
                    dropped_tables.insert(p.name.to_lowercase());
                }
                _ => {}
            }
        }
        let mut changed_routines: HashSet<String> = HashSet::new();
        for &(p, status) in &routines {
            if matches!(status, DiffStatus::Different | DiffStatus::OnlyInTarget) {
                changed_routines.insert(p.name.to_lowercase());
            }
        }
        let mut notes: Vec<String> = Vec::new();
        if !changed_tables.is_empty() {
            self.note_views_out_of_scope(&mut notes);
        }

        // Triggers dropped in phase 1 and created in phase 7.
        let mut trigger_drops: Vec<&Pair> = Vec::new();
        let mut trigger_creates: Vec<&Pair> = Vec::new();
        for &(p, status) in &triggers {
            match status {
                DiffStatus::OnlyInSource => trigger_creates.push(p),
                DiffStatus::Different => {
                    trigger_drops.push(p);
                    trigger_creates.push(p);
                }
                DiffStatus::OnlyInTarget => trigger_drops.push(p),
                DiffStatus::Same => {}
            }
        }

        // Views: the chosen ones, plus every target view that reads something that changes
        // or goes (it is dropped first and created again after the change).
        let chosen_views: HashMap<&str, DiffStatus> =
            views.iter().map(|(p, s)| (p.id.as_str(), *s)).collect();
        let target_views: Vec<&Pair> = pairs
            .iter()
            .filter(|p| {
                matches!(p.kind, ObjectKind::View | ObjectKind::MatView) && p.target.is_some()
            })
            .collect();
        let reads: HashMap<&str, HashSet<String>> = target_views
            .iter()
            .filter_map(|p| match &p.target {
                Some(ObjectDef::View(v)) => {
                    Some((p.id.as_str(), referenced_names(self.engine, &v.definition)))
                }
                _ => None,
            })
            .collect();
        let mut dropped_views: Vec<&Pair> = Vec::new();
        let mut gone: HashSet<String> = changed_tables.clone();
        gone.extend(changed_routines.iter().cloned());
        for &p in &target_views {
            if matches!(
                chosen_views.get(p.id.as_str()),
                Some(DiffStatus::Different | DiffStatus::OnlyInTarget)
            ) {
                dropped_views.push(p);
                gone.insert(p.name.to_lowercase());
            }
        }
        loop {
            let mut grew = false;
            for &p in &target_views {
                if dropped_views.iter().any(|d| d.id == p.id) {
                    continue;
                }
                let hit = reads
                    .get(p.id.as_str())
                    .is_some_and(|names| names.iter().any(|n| gone.contains(n)));
                if hit {
                    dropped_views.push(p);
                    gone.insert(p.name.to_lowercase());
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        // Views created in phase 7: new ones, and dropped ones that stay (from the source
        // definition when the view is selected, else from the target's own).
        let mut created_views: Vec<(&Pair, &ObjectDef)> = Vec::new();
        for &(p, status) in &views {
            if status == DiffStatus::OnlyInSource
                && let Some(def) = &p.source
            {
                created_views.push((p, def));
            }
        }
        let mut warnings_parts: Vec<(String, String)> = Vec::new();
        for &p in &dropped_views {
            let selected = chosen_views.get(p.id.as_str());
            if selected == Some(&DiffStatus::OnlyInTarget) {
                continue;
            }
            if selected.is_some()
                && let Some(def) = &p.source
            {
                created_views.push((p, def));
            } else if let Some(def) = &p.target {
                let reads_dropped = reads
                    .get(p.id.as_str())
                    .is_some_and(|names| names.iter().any(|n| dropped_tables.contains(n)));
                if reads_dropped {
                    warnings_parts.push((
                        p.id.clone(),
                        format!(
                            "{} {} reads a table the script drops; it is dropped too",
                            p.kind.display_name(),
                            p.name
                        ),
                    ));
                } else {
                    created_views.push((p, def));
                }
            }
        }
        // Triggers on dropped views go with them: create them again unless they go for good.
        let handled: HashSet<&str> = trigger_drops
            .iter()
            .chain(&trigger_creates)
            .map(|p| p.id.as_str())
            .collect();
        let recreated_view_names: HashSet<String> = created_views
            .iter()
            .filter(|(p, _)| p.target.is_some())
            .map(|(p, _)| p.name.clone())
            .collect();
        let mut view_trigger_creates: Vec<&Pair> = Vec::new();
        for p in pairs {
            if p.kind == ObjectKind::Trigger
                && p.target.is_some()
                && !handled.contains(p.id.as_str())
                && p.parent
                    .as_ref()
                    .is_some_and(|v| recreated_view_names.contains(v))
            {
                view_trigger_creates.push(p);
            }
        }

        // 1. Dependents' drops: triggers, views (dependents first), routines.
        for p in &trigger_drops {
            let r = target_ref(p)?;
            let recreate = trigger_creates.iter().any(|c| c.id == p.id);
            push(
                Phase::DropDependents,
                &p.id,
                self.drop_object(&r)?,
                recreate,
            );
        }
        let drop_entries: Vec<(&Pair, &ObjectDef)> = dropped_views
            .iter()
            .copied()
            .filter_map(|p: &Pair| p.target.as_ref().map(|d| (p, d)))
            .collect();
        let drop_order = dependency_order(self.engine, &drop_entries);
        for (p, _) in drop_order.into_iter().rev() {
            let r = target_ref(p)?;
            let recreate = chosen_views.get(p.id.as_str()) != Some(&DiffStatus::OnlyInTarget)
                && created_views.iter().any(|(c, _)| c.id == p.id);
            push(
                Phase::DropDependents,
                &p.id,
                self.drop_object(&r)?,
                recreate,
            );
        }
        for &(p, status) in &routines {
            if status == DiffStatus::OnlyInTarget {
                let r = target_ref(p)?;
                push(Phase::DropDependents, &p.id, self.drop_object(&r)?, false);
            }
        }

        // Tables.
        let target_triggers: Vec<&TriggerDef> = target
            .objects
            .iter()
            .filter_map(|d| match d {
                ObjectDef::Trigger(t) => Some(t),
                _ => None,
            })
            .collect();
        let target_tables: Vec<&TableDef> = target
            .objects
            .iter()
            .filter_map(|d| match d {
                ObjectDef::Table(t) => Some(t),
                _ => None,
            })
            .collect();
        let dropped_trigger_ids: HashSet<&str> =
            trigger_drops.iter().map(|p| p.id.as_str()).collect();

        let mut creates: Vec<(&Pair, &TableDef)> = Vec::new();
        let mut alters: Vec<(&Pair, &TableDef, &TableDef)> = Vec::new();
        let mut drops: Vec<(&Pair, &TableDef)> = Vec::new();
        for &(p, status) in &tables {
            match (status, &p.source, &p.target) {
                (DiffStatus::OnlyInSource, Some(ObjectDef::Table(s)), _) => creates.push((p, s)),
                (DiffStatus::Different, Some(ObjectDef::Table(s)), Some(ObjectDef::Table(t))) => {
                    alters.push((p, t, s));
                }
                (DiffStatus::OnlyInTarget, _, Some(ObjectDef::Table(t))) => drops.push((p, t)),
                _ => {}
            }
        }
        let is_dropped = |t: &TableDef| drops.iter().any(|(_, d)| d.obj == t.obj);
        let alter_of = |t: &TableDef| alters.iter().find(|(_, a, _)| a.obj == t.obj).copied();

        // A table drop that a foreign key of a table that stays still needs is refused.
        for &t in &target_tables {
            if is_dropped(t) {
                continue;
            }
            for fk in &t.foreign_keys {
                let Some((_, parent)) = drops
                    .iter()
                    .find(|(_, d)| d.obj != t.obj && same_table(&fk.ref_table, &d.obj))
                else {
                    continue;
                };
                let dropped_by_alter = alter_of(t).is_some_and(|(_, before, after)| {
                    !after
                        .foreign_keys
                        .iter()
                        .any(|other| self.same_fk(&before.obj, fk, other))
                });
                if !dropped_by_alter {
                    return Err(Error::invalid_input(format!(
                        "table {} cannot be dropped: foreign key {} of table {} references it; \
                         select table {} too, or leave the drop out",
                        parent.obj.name,
                        fk.name.as_deref().unwrap_or("(unnamed)"),
                        t.obj.name,
                        t.obj.name
                    )));
                }
            }
        }

        // 2. Foreign keys that change or go; 3. table drops (children first).
        let doomed: Vec<TableDef> = drops.iter().map(|(_, t)| (*t).clone()).collect();
        let doomed_order = table_dependencies(&doomed).order();
        if self.native_fk {
            for cycle in &doomed_order.cycles {
                for table_ref in cycle {
                    let Some((p, t)) = drops.iter().find(|(_, t)| t.obj == *table_ref) else {
                        continue;
                    };
                    for fk in &t.foreign_keys {
                        let Some(name) = &fk.name else { continue };
                        if cycle
                            .iter()
                            .any(|c| *c != t.obj && c.name == fk.ref_table.name)
                        {
                            let script = self.drop_fk(t, name)?;
                            push(Phase::DropForeignKeys, &p.id, script, false);
                        }
                    }
                }
            }
        }
        // Foreign keys that reference a key the script changes: dropped, added back in 6.
        let mut forced: HashMap<String, Vec<String>> = HashMap::new();
        let mut fk_adds: Vec<(String, TableDef, ForeignKeyDef)> = Vec::new();
        if self.native_fk {
            for &t in &target_tables {
                if is_dropped(t) {
                    continue;
                }
                for fk in &t.foreign_keys {
                    let Some(name) = &fk.name else { continue };
                    let Some((parent, before, after)) = alters
                        .iter()
                        .find(|(_, before, _)| same_table(&fk.ref_table, &before.obj))
                    else {
                        continue;
                    };
                    if !key_changes(before, after, &fk.ref_columns) {
                        continue;
                    }
                    if alter_of(t).is_some() {
                        // The table's own alter drops and adds it (`split_fks`).
                        forced
                            .entry(t.obj.name.clone())
                            .or_default()
                            .push(name.clone());
                    } else {
                        let script = self.drop_fk(t, name)?;
                        push(Phase::DropForeignKeys, &parent.id, script, false);
                        fk_adds.push((parent.id.clone(), t.clone(), fk.clone()));
                    }
                }
            }
        }
        let mut altered: Vec<(&Pair, TableDef, TableDef, Vec<ForeignKeyDef>)> = Vec::new();
        for &(p, t, s) in &alters {
            if self.native_fk {
                let no_force = Vec::new();
                let force = forced.get(&t.obj.name).unwrap_or(&no_force);
                let (before, after, dropped, added) = self.split_fks(t, s, force);
                for name in dropped {
                    let script = self.drop_fk(t, &name)?;
                    push(Phase::DropForeignKeys, &p.id, script, false);
                }
                altered.push((p, before, after, added));
            } else {
                altered.push((p, t.clone(), s.clone(), Vec::new()));
            }
        }
        for table_ref in doomed_order.order.iter().rev() {
            if let Some((p, t)) = drops.iter().find(|(_, t)| t.obj == *table_ref) {
                push(Phase::DropTables, &p.id, self.drop_object(&t.obj)?, false);
            }
        }
        // Sequences only in the target go once the tables no longer use them (phase 6).
        for &(p, status) in &sequences {
            if status == DiffStatus::OnlyInTarget {
                let r = target_ref(p)?;
                push(Phase::DropSequences, &p.id, self.drop_object(&r)?, false);
            }
        }

        // 4. Sequences.
        for &(p, status) in &sequences {
            match (status, &p.source, &p.target) {
                (DiffStatus::OnlyInSource, Some(s), _) => {
                    push(Phase::Sequences, &p.id, self.create(s)?, false);
                }
                (DiffStatus::Different, Some(s), Some(t)) => {
                    push(Phase::Sequences, &p.id, self.alter(t, s)?, false);
                }
                _ => {}
            }
        }
        // ... and the routines a table's expressions call (created before the tables).
        let table_names: HashSet<String> = creates
            .iter()
            .map(|(_, s)| *s)
            .chain(alters.iter().map(|(_, _, s)| *s))
            .flat_map(|s| expression_names(self.engine, s))
            .collect();
        let early = |p: &Pair| table_names.contains(&p.name.to_lowercase());
        let routine_script = |p: &Pair, status: DiffStatus| -> Result<Option<Script>> {
            match (status, &p.source, &p.target) {
                (DiffStatus::OnlyInSource, Some(s), _) => self.create(s).map(Some),
                (DiffStatus::Different, Some(s), Some(t)) => self.alter(t, s).map(Some),
                _ => Ok(None),
            }
        };
        for &(p, status) in &routines {
            if early(p)
                && let Some(script) = routine_script(p, status)?
            {
                push(Phase::Sequences, &p.id, script, false);
            }
        }

        // 5. New tables (parents first, foreign keys later where the engine adds them on
        // their own), then the changed tables.
        let new_tables: Vec<TableDef> = creates.iter().map(|(_, s)| (*s).clone()).collect();
        let create_order = table_dependencies(&new_tables).order();
        for table_ref in &create_order.order {
            let Some((p, s)) = creates.iter().find(|(_, s)| s.obj == *table_ref) else {
                continue;
            };
            if self.native_fk {
                let mut bare = (*s).clone();
                let fks = std::mem::take(&mut bare.foreign_keys);
                let script = self.create(&ObjectDef::Table(bare.clone()))?;
                push(Phase::Tables, &p.id, script, false);
                for fk in fks {
                    fk_adds.push((p.id.clone(), bare.clone(), fk));
                }
            } else {
                push(
                    Phase::Tables,
                    &p.id,
                    self.create(&ObjectDef::Table((*s).clone()))?,
                    false,
                );
            }
        }
        for (p, before, after, added) in altered {
            let kept: Vec<TriggerDef> = target_triggers
                .iter()
                .filter(|t| {
                    t.obj.parent.as_deref() == Some(before.obj.name.as_str())
                        && !dropped_trigger_ids.contains(item_id(&t.obj).as_str())
                })
                .map(|t| (*t).clone())
                .collect();
            let ctx = self.ctx().with_table(&before).with_triggers(&kept);
            let ddl = self.ddl(ObjectKind::Table, &before.obj)?;
            let script = ddl.alter(
                &ObjectDef::Table(before.clone()),
                &ObjectDef::Table(after.clone()),
                &ctx,
            )?;
            push(Phase::Tables, &p.id, script, false);
            for fk in added {
                fk_adds.push((p.id.clone(), after.clone(), fk));
            }
        }

        // 6. Foreign keys to add.
        for (id, table, fk) in fk_adds {
            let def = ObjectDef::ForeignKey {
                table: table.obj.clone(),
                foreign_key: fk,
            };
            let ddl = self.ddl(ObjectKind::ForeignKey, &table.obj)?;
            let ctx = self.ctx().with_table(&table);
            push(Phase::AddForeignKeys, &id, ddl.create(&def, &ctx)?, false);
        }

        // 7. Routines and views (dependency order), triggers.
        let mut late: Vec<(&Pair, &ObjectDef)> = Vec::new();
        let mut late_scripts: HashMap<&str, Script> = HashMap::new();
        for &(p, status) in &routines {
            if early(p) {
                continue;
            }
            if let Some(script) = routine_script(p, status)? {
                if let Some(def) = p.source.as_ref() {
                    late.push((p, def));
                }
                late_scripts.insert(p.id.as_str(), script);
            }
        }
        late.extend(created_views.iter().copied());
        for (p, def) in dependency_order(self.engine, &late) {
            let script = match late_scripts.remove(p.id.as_str()) {
                Some(script) => script,
                None => self.create(def)?,
            };
            push(Phase::Dependents, &p.id, script, false);
        }
        for p in trigger_creates.iter().chain(&view_trigger_creates) {
            if let Some(def) = p.source.as_ref().or(p.target.as_ref()) {
                push(Phase::Dependents, &p.id, self.create(def)?, false);
            }
        }
        for (id, warning) in warnings_parts {
            let mut script = Script::new();
            script.warn(warning);
            push(Phase::Dependents, &id, script, false);
        }

        // Objects dropped and created again lose what was granted on them.
        let recreated = !dropped_views.is_empty()
            || trigger_creates.iter().any(|p| p.target.is_some())
            || routines.iter().any(|(_, s)| *s == DiffStatus::Different);
        if recreated {
            notes.push(
                "views, routines and triggers that are dropped and created again lose the \
                 privileges granted on them (and a materialized view its indexes); grant them \
                 again after the apply"
                    .to_owned(),
            );
        }
        if !notes.is_empty() {
            // Warnings only: no steps, so no item.
            let mut script = Script::new();
            for note in notes {
                script.warn(note);
            }
            push(Phase::DropDependents, "", script, false);
        }

        parts.sort_by_key(|part| part.phase);
        Ok(parts)
    }

    /// A warning when tables change while views that may read them are not compared (left
    /// out of the kinds, or excluded): those are neither dropped nor created again.
    fn note_views_out_of_scope(&self, notes: &mut Vec<String>) {
        let view_kinds: Vec<ObjectKind> = [ObjectKind::View, ObjectKind::MatView]
            .into_iter()
            .filter(|k| {
                self.dialects.supports_kind(self.engine, *k, self.version)
                    && self.dialects.fetcher(self.engine, *k).is_ok()
            })
            .collect();
        let left_out = view_kinds.iter().any(|k| !self.options.kinds.contains(k))
            || self
                .options
                .exclude
                .iter()
                .any(|e| view_kinds.contains(&e.kind));
        if left_out {
            notes.push(
                "views are not all compared (kinds or exclusions): a view that reads a changed \
                 table is not dropped and created again, so the server may refuse the change or \
                 keep the view as it was"
                    .to_owned(),
            );
        }
    }

    /// `ALTER TABLE ... DROP` of foreign key `name` of `table`.
    fn drop_fk(&self, table: &TableDef, name: &str) -> Result<Script> {
        let r = table.obj.child(ObjectKind::ForeignKey, name);
        let ctx = self.ctx().with_table(table);
        self.ddl(ObjectKind::ForeignKey, &r)?.drop_object(&r, &ctx)
    }

    /// The foreign keys of a changed table, split for engines that add and drop them on their
    /// own: the table as it is once the changed and removed (named) keys are dropped, the
    /// table to alter it into (with the kept keys only), the names to drop, the keys to add.
    /// `force`: names of target keys to drop and add again although they stay the same (they
    /// reference a key the script changes).
    fn split_fks(
        &self,
        target: &TableDef,
        source: &TableDef,
        force: &[String],
    ) -> (TableDef, TableDef, Vec<String>, Vec<ForeignKeyDef>) {
        let mut kept: Vec<ForeignKeyDef> = Vec::new();
        let mut used = vec![false; source.foreign_keys.len()];
        let mut dropped: Vec<String> = Vec::new();
        for fk in &target.foreign_keys {
            let forced = fk.name.as_ref().is_some_and(|n| force.contains(n));
            let found = source
                .foreign_keys
                .iter()
                .enumerate()
                .find(|(i, other)| !forced && !used[*i] && self.same_fk(&target.obj, fk, other))
                .map(|(i, _)| i);
            match (found, &fk.name) {
                (Some(i), _) => {
                    used[i] = true;
                    kept.push(fk.clone());
                }
                (None, Some(name)) => dropped.push(name.clone()),
                // An unnamed key cannot be dropped on its own: the table alter drops it.
                (None, None) => kept.push(fk.clone()),
            }
        }
        let added: Vec<ForeignKeyDef> = source
            .foreign_keys
            .iter()
            .zip(&used)
            .filter(|(_, was_used)| !**was_used)
            .map(|(fk, _)| fk.clone())
            .collect();
        let mut before = target.clone();
        before
            .foreign_keys
            .retain(|fk| fk.name.as_ref().is_none_or(|n| !dropped.contains(n)));
        let mut after = source.clone();
        after.foreign_keys = before
            .foreign_keys
            .iter()
            .filter(|fk| fk.name.is_some())
            .cloned()
            .collect();
        (before, after, dropped, added)
    }
}

/// The target-side ref of an item.
fn target_ref(p: &Pair) -> Result<ObjectRef> {
    p.target
        .as_ref()
        .map(ObjectDef::object_ref)
        .ok_or_else(|| Error::internal(format!("item {} has no target object", p.id)))
}

/// Views and routines in dependency order (one that names another comes after it), by the
/// names their SQL mentions; otherwise in the given order.
fn dependency_order<'a>(
    engine: Engine,
    entries: &[(&'a Pair, &'a ObjectDef)],
) -> Vec<(&'a Pair, &'a ObjectDef)> {
    let mut graph = DependencyGraph::new();
    let refs: Vec<ObjectRef> = entries.iter().map(|(_, d)| d.object_ref()).collect();
    for r in &refs {
        graph.add(r.clone());
    }
    for (i, (_, def)) in entries.iter().enumerate() {
        let names = definition_names(engine, def);
        for (j, other) in refs.iter().enumerate() {
            if i != j && names.contains(&other.name.to_lowercase()) {
                graph.depends_on(&refs[i], other);
            }
        }
    }
    graph
        .order()
        .order
        .iter()
        .filter_map(|r| refs.iter().position(|x| x == r).map(|i| entries[i]))
        .collect()
}

/// The names the SQL text of a view or routine mentions.
fn definition_names(engine: Engine, def: &ObjectDef) -> HashSet<String> {
    match def {
        ObjectDef::View(v) => referenced_names(engine, &v.definition),
        ObjectDef::Routine(r) => {
            let mut names = HashSet::new();
            for text in [r.body.as_deref(), r.definition.as_deref()]
                .into_iter()
                .flatten()
            {
                names.extend(referenced_names(engine, text));
            }
            names
        }
        _ => HashSet::new(),
    }
}

/// The names a table's expressions mention (defaults, checks, generated columns, index
/// expressions and predicates): the routines it calls are among them.
fn expression_names(engine: Engine, table: &TableDef) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut add = |text: &str| names.extend(referenced_names(engine, text));
    for c in &table.columns {
        if let Some(d) = &c.default {
            add(d);
        }
        if let Some(g) = &c.generated {
            add(&g.expression);
        }
    }
    for check in &table.checks {
        add(&check.expression);
    }
    for index in &table.indexes {
        if let Some(p) = &index.predicate {
            add(p);
        }
        for part in index.columns.iter().filter(|c| c.is_expression) {
            add(&part.name);
        }
    }
    names
}

/// The script changes a key that foreign keys referencing `columns` of the table use: one of
/// those columns (besides its default and comment), the primary key over them, or a unique
/// index over them.
fn key_changes(before: &TableDef, after: &TableDef, columns: &[String]) -> bool {
    let same_set = |a: &[String]| a.len() == columns.len() && a.iter().all(|c| columns.contains(c));
    let column_changes =
        columns
            .iter()
            .any(|name| match (before.column(name), after.column(name)) {
                (Some(old), Some(new)) => {
                    let (mut old, mut new) = (old.clone(), new.clone());
                    for c in [&mut old, &mut new] {
                        c.comment = None;
                        c.default = None;
                    }
                    old != new
                }
                _ => true,
            });
    let pk_changes = before.primary_key != after.primary_key
        && before
            .primary_key
            .as_ref()
            .is_some_and(|pk| same_set(pk.columns.as_slice()));
    let unique_changes = before.indexes.iter().any(|index| {
        let index_columns: Vec<String> = index.columns.iter().map(|c| c.name.clone()).collect();
        index.unique
            && same_set(index_columns.as_slice())
            && after.indexes.iter().find(|i| i.name == index.name) != Some(index)
    });
    column_changes || pk_changes || unique_changes
}

/// The identifiers a piece of SQL mentions, lower-cased (a token scan: strings and comments
/// are skipped).
fn referenced_names(engine: Engine, text: &str) -> HashSet<String> {
    let Some(dialect) = SqlDialect::for_engine(engine) else {
        return HashSet::new();
    };
    tokenize(dialect, text)
        .filter_map(|t| match t.kind {
            SqlTokenKind::Word => Some(t.text(text).to_lowercase()),
            SqlTokenKind::QuotedIdent { .. } => Some(ident_value(t.text(text)).to_lowercase()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::ScriptStep;

    #[test]
    fn names_come_from_identifiers_only() {
        let names = referenced_names(
            Engine::Postgres,
            "SELECT a FROM \"Orders\" o JOIN s.items -- customers\n WHERE x = 'products'",
        );
        assert!(names.contains("orders") && names.contains("items") && names.contains("s"));
        assert!(!names.contains("customers") && !names.contains("products"));
    }

    #[test]
    fn conditional_restores_are_comments_in_the_text() {
        let mut script = Script::new();
        script
            .push_step(
                ScriptStep::new("PRAGMA foreign_keys")
                    .in_phase(StepPhase::Prepare)
                    .capture("fk"),
            )
            .push_step(ScriptStep::new("CREATE TABLE t (a int)"))
            .push_step(
                ScriptStep::new("PRAGMA foreign_keys\n  = ON")
                    .in_phase(StepPhase::Finally)
                    .only_if("fk", "1"),
            );
        assert_eq!(
            script_text(&script, ";"),
            "PRAGMA foreign_keys;\nCREATE TABLE t (a int);\n-- only when fk = 1: PRAGMA foreign_keys = ON\n"
        );
    }

    #[test]
    fn key_changes_follow_the_referenced_columns() {
        use oxbase_types::{ColumnDef, DataType, IndexColumn, IndexDef, PrimaryKeyDef};
        let r = ObjectRef::table(None, Some("s"), "t");
        let mut before = TableDef::new(r.clone());
        before.columns = vec![
            ColumnDef::new("id", DataType::INT32),
            ColumnDef::new("code", DataType::INT32),
            ColumnDef::new("note", DataType::Text),
        ];
        before.primary_key = Some(PrimaryKeyDef {
            name: None,
            columns: vec!["id".into()],
        });
        let id = ["id".to_owned()];
        let code = ["code".to_owned()];
        // Another column, a comment or a default: no.
        let mut after = before.clone();
        after.columns[2].data_type = DataType::INT32;
        after.columns[0].comment = Some("key".into());
        after.columns[0].default = Some("1".into());
        assert!(!key_changes(&before, &after, &id));
        // The referenced column's type: yes.
        after.columns[0].data_type = DataType::INT64;
        assert!(key_changes(&before, &after, &id));
        // The primary key over the referenced columns: yes.
        let mut after = before.clone();
        after.primary_key = Some(PrimaryKeyDef {
            name: Some("pk".into()),
            columns: vec!["id".into()],
        });
        assert!(key_changes(&before, &after, &id));
        assert!(!key_changes(&before, &after, &code));
        // A unique index over them that changes: yes.
        let mut before = before.clone();
        before.indexes.push(IndexDef {
            name: "code_key".into(),
            columns: vec![IndexColumn::column("code")],
            unique: true,
            is_constraint: true,
            predicate: None,
            extras: oxbase_types::Extras::new(),
        });
        let mut after = before.clone();
        after.indexes[0].is_constraint = false;
        assert!(key_changes(&before, &after, &code));
        assert!(!key_changes(&before, &before, &code));
    }

    #[test]
    fn phases_are_ordered() {
        assert!(Phase::DropDependents < Phase::DropForeignKeys);
        assert!(Phase::Tables < Phase::AddForeignKeys);
        assert!(Phase::AddForeignKeys < Phase::DropSequences);
        assert!(Phase::DropSequences < Phase::Dependents);
        let step = ScriptStep::new("x");
        assert_eq!(step.phase, StepPhase::Main);
    }
}
