//! Structure compare (`docs/m8-contracts.md` §5.1-§5.3): two schemas of the **same engine**,
//! read through the engine's fetchers into [`SchemaSnapshot`]s and compared with its
//! `Comparator` cells into a [`StructureDiff`]. Owner of the bodies marked
//! `unimplemented!("m8: structure-sync")`: the **structure-sync** stream.
//!
//! # Snapshots ([`load_snapshot`])
//!
//! Every object of [`StructureOptions::kinds`] under the container (a `Schema` ref on
//! PostgreSQL / SQL Server, a `Database` ref on MySQL / MariaDB / SQLite): `list` then `load`
//! through the engine's `Fetcher`s (system objects skipped). Tables are loaded whole (columns,
//! keys, indexes, foreign keys, checks); triggers are listed per table and view. An object
//! that fails to load (permissions, an unsupported feature) is skipped with a reason
//! ([`SchemaSnapshot::skipped`]) instead of failing the compare. Kinds the engine has no
//! fetcher for, or does not have at its version (`Capabilities::supports_kind`), are skipped
//! silently.
//!
//! # Matching and comparing ([`compare`])
//!
//! 1. Every source definition is moved to the target container
//!    (`oxbase_types::relocate_def`) and the names inside its SQL text are moved with
//!    [`crate::retarget_sql`], so a view `SELECT ... FROM m8_src.t` compares equal to
//!    `SELECT ... FROM m8_tgt.t`.
//! 2. Objects are matched by kind and name (and parent for triggers, signature for
//!    PostgreSQL routines); exactly, or ignoring case with [`StructureOptions::ignore_case`]
//!    (the target's spelling is kept). Excluded objects ([`StructureOptions::exclude`], by
//!    source or target ref) are left out of the diff.
//! 3. Matched pairs go through the engine's `Comparator` for their kind (`diff(target,
//!    source)`: going **from the target to the source**, because the script makes the target
//!    look like the source). Property differences the options switch off are removed: collations
//!    (`collation`), comments (`comment`), ownership (`owner`, `definer`).
//! 4. Every item gets both sides' `CREATE` text for the side-by-side DDL view (the engine's
//!    `DdlGenerator::create`, rendered with the engine's terminator), so the UI never builds
//!    DDL itself.
//!
//! # Determinism
//!
//! Items are sorted by kind rank (table, view, materialized view, sequence, function,
//! procedure, trigger), then name, then parent; [`DiffItem::id`] is stable across compares of
//! the same schemas (`<kind>:<parent>.<name>(<signature>)`, target-side names), so a selection
//! survives a re-compare.

use std::collections::HashMap;

use oxbase_dialect::compare::{DefaultRules, diff_objects};
use oxbase_dialect::compare_objects::diff_definitions;
use oxbase_dialect::{DdlContext, DialectRegistry, Fetcher};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    Engine, Error, ErrorCategory, Extras, Location, ObjectDef, ObjectKind, ObjectRef, PropId,
    PropertyDiff, Result, ServerVersion, props, relocate_def,
};
use serde::{Deserialize, Serialize};

use crate::retarget::retarget_sql;

/// The top-level kinds a structure compare reads by default (their children come with them:
/// a table's columns, keys, indexes, foreign keys and checks; triggers per table).
pub const DEFAULT_KINDS: [ObjectKind; 7] = [
    ObjectKind::Table,
    ObjectKind::View,
    ObjectKind::MatView,
    ObjectKind::Sequence,
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Trigger,
];

/// What a compare reads and how it compares. Every field has a default (camelCase JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StructureOptions {
    /// Top-level kinds to compare (default [`DEFAULT_KINDS`]).
    pub kinds: Vec<ObjectKind>,
    /// Scripts drop objects that exist only in the target (off by default; drops are
    /// destructive and need confirmation to apply).
    pub include_drops: bool,
    pub compare_collations: bool,
    pub compare_comments: bool,
    /// Owners, definers and security principals (off by default: two servers rarely share
    /// them).
    pub compare_ownership: bool,
    /// Match names ignoring case (for engines or schemas whose names were created with a
    /// different case).
    pub ignore_case: bool,
    /// Objects to leave out (source- or target-side refs; a table excludes its triggers too).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<ObjectRef>,
}

impl Default for StructureOptions {
    fn default() -> Self {
        Self {
            kinds: DEFAULT_KINDS.to_vec(),
            include_drops: false,
            compare_collations: true,
            compare_comments: true,
            compare_ownership: false,
            ignore_case: false,
            exclude: Vec::new(),
        }
    }
}

/// An object a snapshot could not load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedObject {
    pub object: ObjectRef,
    pub reason: String,
}

/// Every compared object of one container, as the server reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaSnapshot {
    pub engine: Engine,
    pub version: ServerVersion,
    /// The container (`Schema` or `Database` ref).
    pub container: ObjectRef,
    /// Top-level definitions (tables whole; triggers separately), in fetcher order.
    pub objects: Vec<ObjectDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedObject>,
}

/// Reads `container` on `connection` (module docs, *Snapshots*). `on_object` is called after
/// each loaded object with (loaded so far, listed so far) for progress.
pub async fn load_snapshot(
    dialects: &DialectRegistry,
    connection: &dyn Connection,
    container: &ObjectRef,
    options: &StructureOptions,
    on_object: &(dyn Fn(u64, u64) + Send + Sync),
    cancel: CancellationToken,
) -> Result<SchemaSnapshot> {
    let engine = connection.engine();
    let version = connection.server_version().clone();
    let mut kinds: Vec<ObjectKind> = Vec::new();
    for kind in &options.kinds {
        if *kind != ObjectKind::Trigger && !kinds.contains(kind) {
            kinds.push(*kind);
        }
    }
    let triggers = if options.kinds.contains(&ObjectKind::Trigger)
        && dialects.supports_kind(engine, ObjectKind::Trigger, &version)
    {
        dialects.fetcher(engine, ObjectKind::Trigger).ok()
    } else {
        None
    };

    let mut listed: Vec<ObjectRef> = Vec::new();
    for kind in kinds {
        if !dialects.supports_kind(engine, kind, &version) {
            continue;
        }
        let Ok(fetcher) = dialects.fetcher(engine, kind) else {
            continue;
        };
        check(&cancel)?;
        let summaries = match fetcher.list(connection, container, cancel.clone()).await {
            Ok(summaries) => summaries,
            // A kind this server version does not have.
            Err(e) if e.category() == ErrorCategory::Unsupported => continue,
            Err(e) => return Err(e),
        };
        listed.extend(
            summaries
                .into_iter()
                .filter(|s| !s.system && !is_excluded(&s.obj, options))
                .map(|s| s.obj),
        );
    }

    let total = u64::try_from(listed.len()).unwrap_or(u64::MAX);
    let mut objects = Vec::with_capacity(listed.len());
    let mut skipped = Vec::new();
    let mut loaded = 0_u64;
    for obj in listed {
        check(&cancel)?;
        let fetcher = dialects.fetcher(engine, obj.kind)?;
        match fetcher.load(connection, &obj, cancel.clone()).await {
            // A sequence owned by a column (serial, identity) comes and goes with its table.
            Ok(ObjectDef::Sequence(q)) if q.owned_by.is_some() => {}
            Ok(def) => {
                let owner = def.object_ref();
                let has_triggers = matches!(
                    def.kind(),
                    ObjectKind::Table | ObjectKind::View | ObjectKind::MatView
                );
                objects.push(def);
                if has_triggers && let Some(trigger_fetcher) = &triggers {
                    load_triggers(
                        trigger_fetcher.as_ref(),
                        connection,
                        &owner,
                        options,
                        &mut objects,
                        &mut skipped,
                        &cancel,
                    )
                    .await?;
                }
            }
            Err(e) if e.is_cancelled() => return Err(e),
            Err(e) => skipped.push(SkippedObject {
                object: obj,
                reason: e.to_string(),
            }),
        }
        loaded += 1;
        on_object(loaded, total);
    }
    check(&cancel)?;
    Ok(SchemaSnapshot {
        engine,
        version,
        container: container.clone(),
        objects,
        skipped,
    })
}

/// The triggers of table or view `owner` into `objects` (failures into `skipped`).
async fn load_triggers(
    fetcher: &dyn Fetcher,
    connection: &dyn Connection,
    owner: &ObjectRef,
    options: &StructureOptions,
    objects: &mut Vec<ObjectDef>,
    skipped: &mut Vec<SkippedObject>,
    cancel: &CancellationToken,
) -> Result<()> {
    let summaries = match fetcher.list(connection, owner, cancel.clone()).await {
        Ok(summaries) => summaries,
        Err(e) if e.is_cancelled() => return Err(e),
        Err(e) if e.category() == ErrorCategory::Unsupported => return Ok(()),
        Err(e) => {
            skipped.push(SkippedObject {
                object: owner.child(ObjectKind::Trigger, ""),
                reason: format!("the triggers of {} could not be listed: {e}", owner.name),
            });
            return Ok(());
        }
    };
    for summary in summaries {
        if summary.system || is_excluded(&summary.obj, options) {
            continue;
        }
        check(cancel)?;
        match fetcher.load(connection, &summary.obj, cancel.clone()).await {
            Ok(def) => objects.push(def),
            Err(e) if e.is_cancelled() => return Err(e),
            Err(e) => skipped.push(SkippedObject {
                object: summary.obj,
                reason: e.to_string(),
            }),
        }
    }
    Ok(())
}

fn check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// `true` when `r` is one of `options.exclude` (by kind, name, owning table and signature;
/// container parts are not compared, so a source- or a target-side ref both work; names folded
/// with [`StructureOptions::ignore_case`]), or a trigger of an excluded table or view.
fn is_excluded(r: &ObjectRef, options: &StructureOptions) -> bool {
    let same_name = |a: &str, b: &str| {
        if options.ignore_case {
            a.to_lowercase() == b.to_lowercase()
        } else {
            a == b
        }
    };
    let same_opt = |a: Option<&str>, b: Option<&str>| match (a, b) {
        (None, _) => true,
        (Some(a), Some(b)) => same_name(a, b),
        (Some(_), None) => false,
    };
    options.exclude.iter().any(|e| {
        let same = e.kind == r.kind
            && same_name(&e.name, &r.name)
            && same_opt(e.parent.as_deref(), r.parent.as_deref())
            && same_opt(e.signature.as_deref(), r.signature.as_deref());
        let owner = r.kind == ObjectKind::Trigger
            && matches!(
                e.kind,
                ObjectKind::Table | ObjectKind::View | ObjectKind::MatView
            )
            && r.parent.as_deref().is_some_and(|p| same_name(p, &e.name));
        same || owner
    })
}

/// Where an object is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStatus {
    /// Only in the source: the script creates it.
    OnlyInSource,
    /// Only in the target: the script drops it when drops are included.
    OnlyInTarget,
    /// In both, with differences: the script alters (or re-creates) it.
    Different,
    /// In both, equal.
    Same,
}

/// One compared object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffItem {
    /// Stable id (module docs, *Determinism*); what selections name.
    pub id: String,
    pub kind: ObjectKind,
    /// Display name (target side when both exist).
    pub name: String,
    /// The owning table or view of a trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub status: DiffStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ObjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ObjectRef>,
    /// What differs (status `different`), going from the target to the source, in the
    /// comparator's fixed order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub properties: Vec<PropertyDiff>,
    /// The source object's `CREATE` text (relocated to the target container).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ddl: Option<String>,
    /// The target object's `CREATE` text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_ddl: Option<String>,
}

/// Item counts by status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffCounts {
    pub only_in_source: u32,
    pub only_in_target: u32,
    pub different: u32,
    pub same: u32,
}

/// The result of [`compare`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructureDiff {
    pub engine: Engine,
    pub source: ObjectRef,
    pub target: ObjectRef,
    /// Sorted (module docs, *Determinism*); `same` items included so the tree can show them.
    pub items: Vec<DiffItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl StructureDiff {
    /// Counts by status.
    pub fn counts(&self) -> DiffCounts {
        let mut c = DiffCounts::default();
        for item in &self.items {
            match item.status {
                DiffStatus::OnlyInSource => c.only_in_source += 1,
                DiffStatus::OnlyInTarget => c.only_in_target += 1,
                DiffStatus::Different => c.different += 1,
                DiffStatus::Same => c.same += 1,
            }
        }
        c
    }

    /// `true` when nothing differs: every item is `same`, or only in the target while drops
    /// are not compared (`include_drops` off). This is the after-apply re-compare check.
    pub fn is_in_sync(&self, include_drops: bool) -> bool {
        self.items.iter().all(|i| match i.status {
            DiffStatus::Same => true,
            DiffStatus::OnlyInTarget => !include_drops,
            DiffStatus::OnlyInSource | DiffStatus::Different => false,
        })
    }

    /// The item with `id`.
    pub fn item(&self, id: &str) -> Option<&DiffItem> {
        self.items.iter().find(|i| i.id == id)
    }
}

/// Compares two snapshots of the same engine (module docs, *Matching and comparing*).
/// `InvalidInput` when the engines differ.
pub fn compare(
    dialects: &DialectRegistry,
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
    options: &StructureOptions,
) -> Result<StructureDiff> {
    let pairs = pair_objects(source, target, options)?;
    let engine = target.engine;
    let terminator = dialects
        .quoting(engine)
        .map_or(";", |q| q.script_terminator());
    let mut warnings: Vec<String> = duplicate_warnings(source, options);
    for warning in duplicate_warnings(target, options) {
        if !warnings.contains(&warning) {
            warnings.push(warning);
        }
    }
    let mut items = Vec::with_capacity(pairs.len());
    for p in &pairs {
        let (status, properties) = match (&p.source, &p.target) {
            (Some(s), Some(t)) => {
                let found = match dialects.comparator(engine, p.kind) {
                    Ok(comparator) => comparator.diff(t, s),
                    Err(e) => {
                        let warning = format!("{e}; compared with the default rules");
                        if !warnings.contains(&warning) {
                            warnings.push(warning);
                        }
                        default_diff(t, s)
                    }
                };
                let properties = filter_properties(found, options);
                if properties.is_empty() {
                    (DiffStatus::Same, properties)
                } else {
                    (DiffStatus::Different, properties)
                }
            }
            (Some(_), None) => (DiffStatus::OnlyInSource, Vec::new()),
            (None, Some(_)) => (DiffStatus::OnlyInTarget, Vec::new()),
            (None, None) => continue,
        };
        items.push(DiffItem {
            id: p.id.clone(),
            kind: p.kind,
            name: p.name.clone(),
            parent: p.parent.clone(),
            status,
            source: p.source_ref.clone(),
            target: p.target.as_ref().map(ObjectDef::object_ref),
            properties,
            source_ddl: create_text(
                dialects,
                engine,
                &target.version,
                p.source.as_ref(),
                terminator,
            ),
            target_ddl: create_text(
                dialects,
                engine,
                &target.version,
                p.target.as_ref(),
                terminator,
            ),
        });
    }
    Ok(StructureDiff {
        engine,
        source: source.container.clone(),
        target: target.container.clone(),
        items,
        warnings,
    })
}

/// With [`StructureOptions::ignore_case`]: one warning per set of objects of a snapshot whose
/// names differ only in case (only the first of them is matched; the others are reported as
/// only on their side).
fn duplicate_warnings(snapshot: &SchemaSnapshot, options: &StructureOptions) -> Vec<String> {
    if !options.ignore_case {
        return Vec::new();
    }
    let mut seen: HashMap<Key, String> = HashMap::new();
    let mut out = Vec::new();
    for def in &snapshot.objects {
        if !options.kinds.contains(&def.kind()) {
            continue;
        }
        let r = def.object_ref();
        let shown = item_id(&r);
        match seen.get(&key_of(&r, true)) {
            Some(first) => out.push(format!(
                "{first} and {shown} of {} differ only in case: matching ignores case, so only \
                 {first} is matched",
                snapshot.container.name
            )),
            None => {
                seen.insert(key_of(&r, true), shown);
            }
        }
    }
    out
}

/// The comparison without an engine comparator: the shared walks with default rules.
fn default_diff(old: &ObjectDef, new: &ObjectDef) -> Vec<PropertyDiff> {
    match old {
        ObjectDef::Table(_)
        | ObjectDef::Column { .. }
        | ObjectDef::Index { .. }
        | ObjectDef::ForeignKey { .. }
        | ObjectDef::Check { .. } => diff_objects(old, new, &DefaultRules),
        _ => diff_definitions(&DefaultRules, old, new),
    }
}

/// Removes the property differences the options switch off.
fn filter_properties(found: Vec<PropertyDiff>, options: &StructureOptions) -> Vec<PropertyDiff> {
    found
        .into_iter()
        .filter(|d| match &d.prop {
            Some(p) if *p == props::COLLATION || *p == props::CHARSET => options.compare_collations,
            Some(p) if *p == props::COMMENT => options.compare_comments,
            Some(p) if *p == props::OWNER || *p == props::DEFINER => options.compare_ownership,
            _ => true,
        })
        .collect()
}

/// The `CREATE` text of `def` for the side-by-side view (`None` when the engine cannot write
/// one).
fn create_text(
    dialects: &DialectRegistry,
    engine: Engine,
    version: &ServerVersion,
    def: Option<&ObjectDef>,
    terminator: &str,
) -> Option<String> {
    let def = def?;
    let ddl = dialects.ddl(engine, def.kind()).ok()?;
    let script = ddl.create(def, &DdlContext::new(version)).ok()?;
    Some(script.to_sql(terminator))
}

/// One compared object: the source definition (moved into the target container, its SQL text
/// retargeted and the switched-off properties taken from the target) and the target's.
#[derive(Debug, Clone)]
pub(crate) struct Pair {
    pub id: String,
    pub kind: ObjectKind,
    pub name: String,
    pub parent: Option<String>,
    /// Where the source object lives (in the source container).
    pub source_ref: Option<ObjectRef>,
    pub source: Option<ObjectDef>,
    pub target: Option<ObjectDef>,
}

/// Rank of a kind in the item order (module docs, *Determinism*).
pub(crate) fn kind_rank(kind: ObjectKind) -> usize {
    match kind {
        ObjectKind::Table => 0,
        ObjectKind::View => 1,
        ObjectKind::MatView => 2,
        ObjectKind::Sequence => 3,
        ObjectKind::Function => 4,
        ObjectKind::Procedure => 5,
        ObjectKind::Trigger => 6,
        _ => 7,
    }
}

/// Matching key: kind, owning table, name, signature (folded when ignoring case).
type Key = (ObjectKind, String, String, String);

fn key_of(r: &ObjectRef, ignore_case: bool) -> Key {
    let fold = |s: &str| {
        if ignore_case {
            s.to_lowercase()
        } else {
            s.to_owned()
        }
    };
    (
        r.kind,
        fold(r.parent.as_deref().unwrap_or("")),
        fold(&r.name),
        fold(r.signature.as_deref().unwrap_or("")),
    )
}

/// Every compared object of both snapshots, matched and sorted (module docs).
pub(crate) fn pair_objects(
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
    options: &StructureOptions,
) -> Result<Vec<Pair>> {
    if source.engine != target.engine {
        return Err(Error::invalid_input(format!(
            "a structure compare needs two databases of the same engine, not {} and {}",
            source.engine.display_name(),
            target.engine.display_name()
        )));
    }
    let engine = target.engine;
    let from = Location::of_container(&source.container);
    let to = Location::of_container(&target.container);
    let wanted = |def: &ObjectDef| {
        options.kinds.contains(&def.kind()) && !is_excluded(&def.object_ref(), options)
    };

    let mut by_key: HashMap<Key, usize> = HashMap::new();
    let targets: Vec<&ObjectDef> = target.objects.iter().filter(|d| wanted(d)).collect();
    for (i, def) in targets.iter().enumerate() {
        by_key
            .entry(key_of(&def.object_ref(), options.ignore_case))
            .or_insert(i);
    }
    let mut used = vec![false; targets.len()];
    let mut pairs = Vec::new();
    for def in source.objects.iter().filter(|d| wanted(d)) {
        let source_ref = def.object_ref();
        let mut moved = def.clone();
        relocate_def(&mut moved, &from, &to);
        retarget_def(engine, &mut moved, &from, &to);
        let moved_ref = moved.object_ref();
        if is_excluded(&moved_ref, options) {
            continue;
        }
        let found = by_key
            .get(&key_of(&moved_ref, options.ignore_case))
            .copied()
            .filter(|&i| !used[i]);
        let target_def = found.map(|i| {
            used[i] = true;
            targets[i].clone()
        });
        if let Some(t) = &target_def {
            take_switched_off(&mut moved, t, options);
            // The target's spelling wins when matching ignores case.
            rename_like(&mut moved, &t.object_ref());
        }
        let shown = target_def
            .as_ref()
            .map_or_else(|| moved.object_ref(), ObjectDef::object_ref);
        pairs.push(Pair {
            id: item_id(&shown),
            kind: shown.kind,
            name: shown.name.clone(),
            parent: shown.parent.clone(),
            source_ref: Some(source_ref),
            source: Some(moved),
            target: target_def,
        });
    }
    for (i, def) in targets.iter().enumerate() {
        if used[i] {
            continue;
        }
        let r = def.object_ref();
        pairs.push(Pair {
            id: item_id(&r),
            kind: r.kind,
            name: r.name.clone(),
            parent: r.parent.clone(),
            source_ref: None,
            source: None,
            target: Some((*def).clone()),
        });
    }
    pairs.sort_by(|a, b| {
        (kind_rank(a.kind), &a.name, &a.parent, &a.id).cmp(&(
            kind_rank(b.kind),
            &b.name,
            &b.parent,
            &b.id,
        ))
    });
    Ok(pairs)
}

/// Gives a source definition matched ignoring case the target's names (so the script alters
/// the target's object, not a new one).
fn rename_like(def: &mut ObjectDef, target: &ObjectRef) {
    let set = |r: &mut ObjectRef| {
        r.name.clone_from(&target.name);
        r.parent.clone_from(&target.parent);
        r.signature.clone_from(&target.signature);
    };
    match def {
        ObjectDef::Table(t) => set(&mut t.obj),
        ObjectDef::View(v) => set(&mut v.obj),
        ObjectDef::Routine(r) => set(&mut r.obj),
        ObjectDef::Trigger(t) => set(&mut t.obj),
        ObjectDef::Sequence(q) => set(&mut q.obj),
        ObjectDef::Generic(g) => set(&mut g.obj),
        _ => {}
    }
}

/// Moves the names inside the engine-native SQL text of `def` from `from` to `to`.
fn retarget_def(engine: Engine, def: &mut ObjectDef, from: &Location, to: &Location) {
    let fix = |s: &mut String| {
        let moved = retarget_sql(engine, s.as_str(), from, to);
        *s = moved;
    };
    match def {
        ObjectDef::Table(t) => {
            for c in &mut t.columns {
                // A user-defined type of the source schema (`m8_src.mood`).
                fix(&mut c.native_type);
                if let Some(d) = &mut c.default {
                    fix(d);
                }
                if let Some(g) = &mut c.generated {
                    fix(&mut g.expression);
                }
            }
            for check in &mut t.checks {
                fix(&mut check.expression);
            }
            for index in &mut t.indexes {
                if let Some(p) = &mut index.predicate {
                    fix(p);
                }
                for part in &mut index.columns {
                    if part.is_expression {
                        fix(&mut part.name);
                    }
                }
            }
        }
        ObjectDef::View(v) => fix(&mut v.definition),
        ObjectDef::Routine(r) => {
            for text in [&mut r.body, &mut r.definition, &mut r.returns]
                .into_iter()
                .flatten()
            {
                fix(text);
            }
            for arg in &mut r.args {
                fix(&mut arg.native_type);
                if let Some(d) = &mut arg.default {
                    fix(d);
                }
            }
            if let Some(signature) = &mut r.obj.signature {
                fix(signature);
            }
        }
        ObjectDef::Trigger(t) => {
            for text in [&mut t.body, &mut t.definition, &mut t.condition]
                .into_iter()
                .flatten()
            {
                fix(text);
            }
        }
        _ => {}
    }
}

/// Takes the properties the options do not compare from the target, so they neither show as
/// differences nor get changed by a script.
fn take_switched_off(source: &mut ObjectDef, target: &ObjectDef, options: &StructureOptions) {
    let mut extras_to_copy: Vec<PropId> = Vec::new();
    if !options.compare_ownership {
        extras_to_copy.extend([props::OWNER, props::DEFINER]);
    }
    // A column's character set travels with its collation (MySQL / MariaDB keep it in the
    // column's extras): both come from the target, so a script never mixes them.
    let collation_extras = [props::COLLATION, props::CHARSET];
    if !options.compare_collations {
        extras_to_copy.extend(collation_extras.iter().cloned());
    }
    match (source, target) {
        (ObjectDef::Table(s), ObjectDef::Table(t)) => {
            if !options.compare_comments {
                s.comment.clone_from(&t.comment);
            }
            for column in &mut s.columns {
                let Some(other) = t.column(&column.name) else {
                    continue;
                };
                if !options.compare_comments {
                    column.comment.clone_from(&other.comment);
                }
                if !options.compare_collations {
                    column.collation.clone_from(&other.collation);
                    copy_extras(&mut column.extras, &other.extras, &collation_extras);
                }
            }
            copy_extras(&mut s.extras, &t.extras, &extras_to_copy);
        }
        (ObjectDef::View(s), ObjectDef::View(t)) => {
            if !options.compare_comments {
                s.comment.clone_from(&t.comment);
            }
            copy_extras(&mut s.extras, &t.extras, &extras_to_copy);
        }
        (ObjectDef::Routine(s), ObjectDef::Routine(t)) => {
            if !options.compare_comments {
                s.comment.clone_from(&t.comment);
            }
            copy_extras(&mut s.extras, &t.extras, &extras_to_copy);
        }
        (ObjectDef::Trigger(s), ObjectDef::Trigger(t)) => {
            if !options.compare_comments {
                s.comment.clone_from(&t.comment);
            }
            copy_extras(&mut s.extras, &t.extras, &extras_to_copy);
        }
        (ObjectDef::Sequence(s), ObjectDef::Sequence(t)) => {
            if !options.compare_comments {
                s.comment.clone_from(&t.comment);
            }
            copy_extras(&mut s.extras, &t.extras, &extras_to_copy);
        }
        _ => {}
    }
}

fn copy_extras(into: &mut Extras, from: &Extras, which: &[PropId]) {
    for prop in which {
        match from.get(prop) {
            Some(value) => {
                into.insert(prop.clone(), value.clone());
            }
            None => {
                into.remove(prop);
            }
        }
    }
}

/// The stable id of an object (module docs, *Determinism*): `<kind>:<parent>.<name>` with
/// `(<signature>)` for routines that have one; names unquoted.
pub fn item_id(r: &ObjectRef) -> String {
    let mut id = format!("{}:", r.kind);
    if let Some(parent) = &r.parent {
        id.push_str(parent);
        id.push('.');
    }
    id.push_str(&r.name);
    if let Some(signature) = &r.signature {
        id.push('(');
        id.push_str(signature);
        id.push(')');
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn item(id: &str, status: DiffStatus) -> DiffItem {
        DiffItem {
            id: id.into(),
            kind: ObjectKind::Table,
            name: id.into(),
            parent: None,
            status,
            source: None,
            target: None,
            properties: Vec::new(),
            source_ddl: None,
            target_ddl: None,
        }
    }

    #[test]
    fn ids_counts_and_sync_state() {
        let t = ObjectRef::table(Some("db"), Some("s"), "orders");
        assert_eq!(item_id(&t), "table:orders");
        assert_eq!(
            item_id(&t.child(ObjectKind::Trigger, "audit")),
            "trigger:orders.audit"
        );
        let f = ObjectRef::schema(Some("db"), "s")
            .child(ObjectKind::Function, "f")
            .with_signature("integer");
        assert_eq!(item_id(&f), "function:f(integer)");

        let mut diff = StructureDiff {
            engine: Engine::Postgres,
            source: ObjectRef::schema(Some("db"), "src"),
            target: ObjectRef::schema(Some("db"), "tgt"),
            items: vec![
                item("table:a", DiffStatus::Same),
                item("table:b", DiffStatus::OnlyInTarget),
            ],
            warnings: Vec::new(),
        };
        assert!(diff.is_in_sync(false));
        assert!(!diff.is_in_sync(true));
        diff.items.push(item("table:c", DiffStatus::Different));
        assert!(!diff.is_in_sync(false));
        assert_eq!(
            diff.counts(),
            DiffCounts {
                only_in_source: 0,
                only_in_target: 1,
                different: 1,
                same: 1
            }
        );
        assert!(diff.item("table:c").is_some());
        let json = serde_json::to_value(&diff).unwrap();
        assert_eq!(json["items"][1]["status"], "only_in_target");
        assert_eq!(serde_json::from_value::<StructureDiff>(json).unwrap(), diff);
    }

    #[test]
    fn options_default_to_every_kind_without_drops() {
        let o: StructureOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(o, StructureOptions::default());
        assert_eq!(o.kinds.len(), 7);
        assert!(!o.include_drops && o.compare_comments && !o.compare_ownership);
        let json = serde_json::to_value(&o).unwrap();
        assert_eq!(json["includeDrops"], false);
        assert!(json.get("exclude").is_none());
    }
}
