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
//! 2. Foreign keys that change or go (on tables that stay), and foreign keys that point at
//!    tables about to be dropped.
//! 3. Drops of tables only in the target (`include_drops`), children before parents.
//! 4. Sequences to create or alter.
//! 5. Tables to create (without their foreign keys), parents first; then alters of the
//!    different tables (without foreign-key additions). The SQLite rebuild steps of one table
//!    stay together, with their prepare / finally phases.
//! 6. Foreign keys to add (new tables' and changed ones), after every table exists.
//! 7. Routines, then views and materialized views (dependency order: a view that reads
//!    another view comes after it), then triggers.
//!
//! Dependencies between views and routines come from a token scan of their SQL for the names of
//! compared objects (`oxbase_sql` tokens; the structure-sync stream) fed to
//! `oxbase_dialect::DependencyGraph`; tables come from foreign keys
//! (`oxbase_dialect::table_dependencies`).
//!
//! # Flags
//!
//! [`SyncScript::destructive`] when any step is (drops, narrowing changes, rebuilds);
//! [`SyncScript::atomic`] when every main step is transactional (`Script::is_atomic`) and the
//! engine runs DDL in transactions (PostgreSQL, SQL Server, SQLite; MySQL / MariaDB DDL commits
//! each statement, and the preview says so: [`SyncScript::transactional_ddl`] false).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use oxbase_dialect::compare::{DefaultRules, diff_objects};
use oxbase_dialect::{
    DdlContext, DdlGenerator, DependencyGraph, DialectRegistry, Support, table_dependencies,
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
                status != DiffStatus::Same && selection.iter().any(|s| *s == p.id)
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
        sql: script.to_sql(terminator),
        items,
        destructive: script.is_destructive(),
        atomic: designer.transactional_ddl && script.is_atomic(),
        transactional_ddl: designer.transactional_ddl,
        warnings: script.warnings.clone(),
        script,
    })
}

/// The phases of the module docs, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    /// 1. Triggers, views and routines that go or are re-created.
    DropDependents,
    /// 2. Foreign keys that change or go.
    DropForeignKeys,
    /// 3. Tables only in the target, then sequences only in the target.
    DropTables,
    /// 4. Sequences to create or alter.
    Sequences,
    /// 5. New tables (parents first), then changed tables.
    Tables,
    /// 6. Foreign keys to add.
    AddForeignKeys,
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
        // Views created in phase 7: new ones, and dropped ones that stay.
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
            let removed = chosen_views.get(p.id.as_str()) == Some(&DiffStatus::OnlyInTarget);
            if removed {
                continue;
            }
            if let Some(def) = &p.source {
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
        let drop_order = view_order(
            self.engine,
            dropped_views
                .iter()
                .copied()
                .filter_map(|p: &Pair| p.target.as_ref().map(|d| (p, d))),
        );
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
        let mut altered: Vec<(&Pair, TableDef, TableDef, Vec<ForeignKeyDef>)> = Vec::new();
        for &(p, t, s) in &alters {
            if self.native_fk {
                let (before, after, dropped, added) = self.split_fks(t, s);
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
        // ... and sequences only in the target.
        for &(p, status) in &sequences {
            if status == DiffStatus::OnlyInTarget {
                let r = target_ref(p)?;
                push(Phase::DropTables, &p.id, self.drop_object(&r)?, false);
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

        // 5. New tables (parents first, foreign keys later where the engine adds them on
        // their own), then the changed tables.
        let new_tables: Vec<TableDef> = creates.iter().map(|(_, s)| (*s).clone()).collect();
        let create_order = table_dependencies(&new_tables).order();
        let mut fk_adds: Vec<(String, TableDef, ForeignKeyDef)> = Vec::new();
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

        // 7. Routines, views (dependency order), triggers.
        for &(p, status) in &routines {
            match (status, &p.source, &p.target) {
                (DiffStatus::OnlyInSource, Some(s), _) => {
                    push(Phase::Dependents, &p.id, self.create(s)?, false);
                }
                (DiffStatus::Different, Some(s), Some(t)) => {
                    push(Phase::Dependents, &p.id, self.alter(t, s)?, false);
                }
                _ => {}
            }
        }
        for (p, def) in view_order(self.engine, created_views.iter().copied()) {
            push(Phase::Dependents, &p.id, self.create(def)?, false);
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

        parts.sort_by_key(|part| part.phase);
        Ok(parts)
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
    fn split_fks(
        &self,
        target: &TableDef,
        source: &TableDef,
    ) -> (TableDef, TableDef, Vec<String>, Vec<ForeignKeyDef>) {
        let mut kept: Vec<ForeignKeyDef> = Vec::new();
        let mut used = vec![false; source.foreign_keys.len()];
        let mut dropped: Vec<String> = Vec::new();
        for fk in &target.foreign_keys {
            let found = source
                .foreign_keys
                .iter()
                .enumerate()
                .find(|(i, other)| !used[*i] && self.same_fk(&target.obj, fk, other))
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

/// Views in dependency order (a view that reads another one after it), by the names their
/// queries mention.
fn view_order<'a>(
    engine: Engine,
    views: impl Iterator<Item = (&'a Pair, &'a ObjectDef)>,
) -> Vec<(&'a Pair, &'a ObjectDef)> {
    let views: Vec<(&Pair, &ObjectDef)> = views.collect();
    let mut graph = DependencyGraph::new();
    let refs: Vec<ObjectRef> = views.iter().map(|(_, d)| d.object_ref()).collect();
    for r in &refs {
        graph.add(r.clone());
    }
    for (i, (_, def)) in views.iter().enumerate() {
        let ObjectDef::View(v) = def else { continue };
        let names = referenced_names(engine, &v.definition);
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
        .filter_map(|r| refs.iter().position(|x| x == r).map(|i| views[i]))
        .collect()
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
    fn phases_are_ordered() {
        assert!(Phase::DropDependents < Phase::DropForeignKeys);
        assert!(Phase::Tables < Phase::AddForeignKeys);
        assert!(Phase::AddForeignKeys < Phase::Dependents);
        let step = ScriptStep::new("x");
        assert_eq!(step.phase, StepPhase::Main);
    }
}
