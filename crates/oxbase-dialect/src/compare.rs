//! Structural comparison of tables and their children (M3), shared by every engine's
//! [`Comparator`] cells so property ids and ordering are the same everywhere.
//!
//! An engine registers `RulesComparator(its CompareRules)` for `Table`, `Column`, `Index`,
//! `ForeignKey` and `Check`. Its [`CompareRules`] only normalize what the engine spells its own
//! way (type aliases, default expressions, check expressions, implicit extras); the walk, the
//! matching of children and the order of the result are fixed here.
//!
//! # Order of [`diff_tables`]
//!
//! All differences for the target table `b` (`object` refs are built from `b.obj`, so a renamed
//! table reports its new name):
//!
//! 1. the table: `name` (renamed), `comment`, then extras by property id;
//! 2. columns of `a` missing from `b`, in `a`'s order (`object_removed`, `old` = the column as
//!    JSON);
//! 3. columns of `b` in `b`'s order: added (`object_added`, `new` = JSON) or the property
//!    differences of [`diff_columns`];
//! 4. `position` for every column kept on both sides whose rank among the kept columns changed
//!    (old/new = 0-based rank);
//! 5. `primary_key` on the table (old/new = `{"name"?, "columns"}` JSON);
//! 6. indexes (matched by name): removed in `a`'s order, then `b`'s order, as in 2-3, with
//!    [`diff_indexes`];
//! 7. foreign keys, then 8. checks, the same way. These are matched by name when both sides
//!    are named, else by content (an unnamed SQLite constraint, or a name the server made up),
//!    see [`diff_foreign_keys`] / [`diff_checks`].
//!
//! # Property ids and values
//!
//! | Object | Properties, in order (`oxbase_types::props`) | Value |
//! |---|---|---|
//! | table | `name`, `comment`, extras, `primary_key` | text / text / extra / JSON |
//! | column | `name`, `data_type`, `nullable`, `default`, `auto_increment`, `identity`, `generated`, `collation`, `comment`, extras, `position` | text, text (native type, else the neutral type), bool, text, bool, JSON, JSON, text, text, extra, int |
//! | index | `name`, `columns`, `unique`, `constraint`, `predicate`, extras | JSON list of key parts, bool, bool, text |
//! | foreign key | `name`, `columns`, `ref_table`, `ref_columns`, `on_delete`, `on_update`, extras | JSON lists, text (`schema.table`), snake_case action |
//! | check | `name`, `expression`, extras | text |
//!
//! Absent and empty comments are equal. A collation is compared only when both sides report
//! one. Extras are compared only where both sides have the property (an engine that wants a
//! property compared always reports it, `false` included). A `name` difference is reported
//! only when both sides are named.

use oxbase_types::{
    CheckDef, ColumnDef, DiffKind, Extras, FkAction, ForeignKeyDef, IndexDef, ObjectDef,
    ObjectKind, ObjectRef, PrimaryKeyDef, PropId, PropertyDiff, TableDef, Value, props,
};
use serde::Serialize;

use crate::Comparator;

/// How an engine normalizes what it spells its own way. Every method has a default; engines
/// override what differs.
pub trait CompareRules: Send + Sync {
    /// The type text reported in `data_type` diffs: the native type when set, else the neutral
    /// type's display form.
    fn type_text(&self, c: &ColumnDef) -> String {
        if c.native_type.trim().is_empty() {
            c.data_type.to_string()
        } else {
            normalize_ws(&c.native_type)
        }
    }

    /// Whether two columns have the same type. Default: when either side has no native type,
    /// compare the neutral [`oxbase_types::DataType`]s; else the native types, case-insensitive
    /// with whitespace collapsed.
    fn same_type(&self, old: &ColumnDef, new: &ColumnDef) -> bool {
        if old.native_type.trim().is_empty() || new.native_type.trim().is_empty() {
            old.data_type == new.data_type
        } else {
            normalize_ws(&old.native_type).eq_ignore_ascii_case(&normalize_ws(&new.native_type))
        }
    }

    /// Whether two columns have the same default. Default: both absent; or both present and
    /// [`CompareRules::same_expression`]; or both columns auto-increment and one side has no
    /// default (the engine's generated `nextval(...)` / identity default is an implementation
    /// detail).
    fn same_default(&self, old: &ColumnDef, new: &ColumnDef) -> bool {
        match (&old.default, &new.default) {
            (None, None) => true,
            (Some(before), Some(after)) => self.same_expression(before, after),
            _ => old.auto_increment && new.auto_increment,
        }
    }

    /// Whether two engine-native expressions (defaults, checks, generated columns, index
    /// predicates and expression keys) mean the same. Default: equal after
    /// [`normalize_ws`].
    fn same_expression(&self, old: &str, new: &str) -> bool {
        normalize_ws(old) == normalize_ws(new)
    }

    /// Whether two foreign-key targets name the same table. Default: same name, and same schema
    /// and catalog where both sides set them.
    fn same_table_ref(&self, old: &ObjectRef, new: &ObjectRef) -> bool {
        old.name == new.name
            && opt_match(old.schema.as_deref(), new.schema.as_deref())
            && opt_match(old.catalog.as_deref(), new.catalog.as_deref())
    }

    /// Whether extra `prop` of objects of `kind` is compared at all (default: yes). Engines
    /// return `false` for facts that are not part of a definition (sizes, OIDs, sequence names
    /// the server made up).
    fn compare_extra(&self, kind: ObjectKind, prop: &PropId) -> bool {
        let _ = (kind, prop);
        true
    }
}

/// [`CompareRules`] with every default.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultRules;

impl CompareRules for DefaultRules {}

/// A [`Comparator`] for `Table`, `Column`, `Index`, `ForeignKey` and `Check` cells that runs
/// [`diff_objects`] with the engine's rules.
#[derive(Debug, Clone, Copy, Default)]
pub struct RulesComparator<R>(pub R);

impl<R: CompareRules> Comparator for RulesComparator<R> {
    fn diff(&self, old: &ObjectDef, new: &ObjectDef) -> Vec<PropertyDiff> {
        diff_objects(old, new, &self.0)
    }
}

/// Trims and collapses every run of whitespace outside `'...'` and `"..."` to one space.
pub fn normalize_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut quote: Option<char> = None;
    let mut pending_space = false;
    for c in text.trim().chars() {
        if quote.is_none() && c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        match quote {
            Some(q) if c == q => quote = None,
            None if c == '\'' || c == '"' => quote = Some(c),
            _ => {}
        }
        out.push(c);
    }
    out
}

fn opt_match(old: Option<&str>, new: Option<&str>) -> bool {
    match (old, new) {
        (Some(before), Some(after)) => before == after,
        _ => true,
    }
}

fn json<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).map_or(Value::Null, Value::Json)
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn added(object: ObjectRef, new: Value) -> PropertyDiff {
    let mut d = PropertyDiff::object_added(object);
    d.new = Some(new);
    d
}

fn removed(object: ObjectRef, old: Value) -> PropertyDiff {
    let mut d = PropertyDiff::object_removed(object);
    d.old = Some(old);
    d
}

fn changed(out: &mut Vec<PropertyDiff>, object: &ObjectRef, prop: PropId, old: Value, new: Value) {
    out.push(PropertyDiff {
        object: object.clone(),
        prop: Some(prop),
        kind: DiffKind::Changed,
        old: Some(old),
        new: Some(new),
    });
}

/// `Changed` when both present and different, `Added` / `Removed` when one side is absent.
fn optional(
    out: &mut Vec<PropertyDiff>,
    object: &ObjectRef,
    prop: PropId,
    old: Option<Value>,
    new: Option<Value>,
) {
    if old != new {
        out.push(PropertyDiff::property(object.clone(), prop, old, new));
    }
}

fn comment(out: &mut Vec<PropertyDiff>, object: &ObjectRef, old: Option<&str>, new: Option<&str>) {
    let old = old.filter(|c| !c.is_empty());
    let new = new.filter(|c| !c.is_empty());
    optional(out, object, props::COMMENT, old.map(text), new.map(text));
}

fn name(out: &mut Vec<PropertyDiff>, object: &ObjectRef, old: Option<&str>, new: Option<&str>) {
    if let (Some(before), Some(after)) = (old, new)
        && before != after
    {
        changed(out, object, props::NAME, text(before), text(after));
    }
}

fn extras(
    out: &mut Vec<PropertyDiff>,
    object: &ObjectRef,
    kind: ObjectKind,
    old: &Extras,
    new: &Extras,
    rules: &dyn CompareRules,
) {
    for prop in old.changed_keys(new) {
        if !rules.compare_extra(kind, prop) {
            continue;
        }
        if let (Some(before), Some(after)) = (old.get(prop), new.get(prop)) {
            changed(out, object, prop.clone(), before.clone(), after.clone());
        }
    }
}

/// Differences between two definitions. `Table`, `Column`, `Index`, `ForeignKey` and `Check`
/// pairs are compared structurally (the functions of this module, objects named from `new`);
/// any other pair, or two different variants, yields one `definition` difference when they are
/// not equal.
pub fn diff_objects(
    old: &ObjectDef,
    new: &ObjectDef,
    rules: &dyn CompareRules,
) -> Vec<PropertyDiff> {
    match (old, new) {
        (ObjectDef::Table(before), ObjectDef::Table(after)) => diff_tables(before, after, rules),
        (
            ObjectDef::Column { column: before, .. },
            ObjectDef::Column {
                table,
                column: after,
            },
        ) => diff_columns(table, before, after, rules),
        (
            ObjectDef::Index { index: before, .. },
            ObjectDef::Index {
                table,
                index: after,
            },
        ) => diff_indexes(table, before, after, rules),
        (
            ObjectDef::ForeignKey {
                foreign_key: before,
                ..
            },
            ObjectDef::ForeignKey {
                table,
                foreign_key: after,
            },
        ) => diff_foreign_keys(table, before, after, rules),
        (
            ObjectDef::Check { check: before, .. },
            ObjectDef::Check {
                table,
                check: after,
            },
        ) => diff_checks(table, before, after, rules),
        _ if old == new => Vec::new(),
        _ => vec![PropertyDiff::property(
            new.object_ref(),
            props::DEFINITION,
            Some(json(old)),
            Some(json(new)),
        )],
    }
}

/// Differences between two tables, in the order of the module docs.
pub fn diff_tables(old: &TableDef, new: &TableDef, rules: &dyn CompareRules) -> Vec<PropertyDiff> {
    let mut out = Vec::new();
    let table = &new.obj;
    name(
        &mut out,
        table,
        Some(old.obj.name.as_str()),
        Some(new.obj.name.as_str()),
    );
    comment(
        &mut out,
        table,
        old.comment.as_deref(),
        new.comment.as_deref(),
    );
    extras(
        &mut out,
        table,
        ObjectKind::Table,
        &old.extras,
        &new.extras,
        rules,
    );

    // Columns.
    for col in &old.columns {
        if new.column(&col.name).is_none() {
            out.push(removed(
                table.child(ObjectKind::Column, &col.name),
                json(col),
            ));
        }
    }
    for col in &new.columns {
        match old.column(&col.name) {
            None => out.push(added(table.child(ObjectKind::Column, &col.name), json(col))),
            Some(before) => out.extend(diff_columns(table, before, col, rules)),
        }
    }
    let kept_old: Vec<&str> = old
        .columns
        .iter()
        .filter(|col| new.column(&col.name).is_some())
        .map(|col| col.name.as_str())
        .collect();
    let kept_new: Vec<&str> = new
        .columns
        .iter()
        .filter(|col| old.column(&col.name).is_some())
        .map(|col| col.name.as_str())
        .collect();
    for (rank_new, col) in kept_new.iter().enumerate() {
        let rank_old = kept_old
            .iter()
            .position(|other| other == col)
            .unwrap_or(rank_new);
        if rank_old != rank_new {
            changed(
                &mut out,
                &table.child(ObjectKind::Column, *col),
                props::POSITION,
                rank_value(rank_old),
                rank_value(rank_new),
            );
        }
    }

    // Primary key.
    if !same_pk(old.primary_key.as_ref(), new.primary_key.as_ref()) {
        out.push(PropertyDiff::property(
            table.clone(),
            props::PRIMARY_KEY,
            old.primary_key.as_ref().map(json),
            new.primary_key.as_ref().map(json),
        ));
    }

    // Indexes, by name.
    for idx in &old.indexes {
        if !new.indexes.iter().any(|other| other.name == idx.name) {
            out.push(removed(
                table.child(ObjectKind::Index, &idx.name),
                json(idx),
            ));
        }
    }
    for idx in &new.indexes {
        match old.indexes.iter().find(|other| other.name == idx.name) {
            None => out.push(added(table.child(ObjectKind::Index, &idx.name), json(idx))),
            Some(before) => out.extend(diff_indexes(table, before, idx, rules)),
        }
    }

    // Foreign keys and checks: by name, else by content.
    let fks = pair(
        &old.foreign_keys,
        &new.foreign_keys,
        |fk| fk.name.as_deref(),
        |before, after| same_fk_target(before, after, rules),
    );
    emit_pairs(
        &mut out,
        table,
        ObjectKind::ForeignKey,
        &fks,
        |fk| fk.name.as_deref(),
        |before, after| diff_foreign_keys(table, before, after, rules),
    );
    let checks = pair(
        &old.checks,
        &new.checks,
        |check| check.name.as_deref(),
        |before, after| rules.same_expression(&before.expression, &after.expression),
    );
    emit_pairs(
        &mut out,
        table,
        ObjectKind::Check,
        &checks,
        |check| check.name.as_deref(),
        |before, after| diff_checks(table, before, after, rules),
    );
    out
}

fn rank_value(rank: usize) -> Value {
    i64::try_from(rank).map_or(Value::Null, Value::Int)
}

fn same_pk(old: Option<&PrimaryKeyDef>, new: Option<&PrimaryKeyDef>) -> bool {
    match (old, new) {
        (None, None) => true,
        (Some(before), Some(after)) => {
            before.columns == after.columns
                && (before.name.is_none() || after.name.is_none() || before.name == after.name)
        }
        _ => false,
    }
}

/// Matched children: the `old` items without a match, then `new`'s items in order with theirs.
struct Pairs<'a, T> {
    removed: Vec<&'a T>,
    targets: Vec<(Option<&'a T>, &'a T)>,
}

/// Pairs `old` and `new` items: first equal names (both named), then, for what is left, the
/// first unmatched item with the same content where at least one side is unnamed.
fn pair<'a, T>(
    old: &'a [T],
    new: &'a [T],
    name_of: impl Fn(&T) -> Option<&str>,
    same_content: impl Fn(&T, &T) -> bool,
) -> Pairs<'a, T> {
    let mut used = vec![false; old.len()];
    let mut matched: Vec<Option<usize>> = vec![None; new.len()];
    for (pos, item) in new.iter().enumerate() {
        if let Some(wanted) = name_of(item)
            && let Some(found) =
                (0..old.len()).find(|&cand| !used[cand] && name_of(&old[cand]) == Some(wanted))
        {
            used[found] = true;
            matched[pos] = Some(found);
        }
    }
    for (pos, item) in new.iter().enumerate() {
        if matched[pos].is_some() {
            continue;
        }
        if let Some(found) = (0..old.len()).find(|&cand| {
            !used[cand]
                && (name_of(&old[cand]).is_none() || name_of(item).is_none())
                && same_content(&old[cand], item)
        }) {
            used[found] = true;
            matched[pos] = Some(found);
        }
    }
    Pairs {
        removed: old
            .iter()
            .zip(&used)
            .filter(|(_, was_used)| !**was_used)
            .map(|(item, _)| item)
            .collect(),
        targets: new
            .iter()
            .zip(matched)
            .map(|(item, found)| (found.map(|idx| &old[idx]), item))
            .collect(),
    }
}

fn emit_pairs<T: Serialize>(
    out: &mut Vec<PropertyDiff>,
    table: &ObjectRef,
    kind: ObjectKind,
    pairs: &Pairs<'_, T>,
    name_of: impl Fn(&T) -> Option<&str>,
    diff: impl Fn(&T, &T) -> Vec<PropertyDiff>,
) {
    for item in &pairs.removed {
        let item: &T = item;
        out.push(removed(
            table.child(kind, name_of(item).unwrap_or("")),
            json(item),
        ));
    }
    for (before, after) in &pairs.targets {
        let after: &T = after;
        match before {
            None => out.push(added(
                table.child(kind, name_of(after).unwrap_or("")),
                json(after),
            )),
            Some(prev) => {
                let prev: &T = prev;
                out.extend(diff(prev, after));
            }
        }
    }
}

fn same_fk_target(old: &ForeignKeyDef, new: &ForeignKeyDef, rules: &dyn CompareRules) -> bool {
    old.columns == new.columns
        && old.ref_columns == new.ref_columns
        && rules.same_table_ref(&old.ref_table, &new.ref_table)
}

/// Differences between two versions of one column of `table` (object named from `new`).
pub fn diff_columns(
    table: &ObjectRef,
    old: &ColumnDef,
    new: &ColumnDef,
    rules: &dyn CompareRules,
) -> Vec<PropertyDiff> {
    let mut out = Vec::new();
    let obj = table.child(ObjectKind::Column, &new.name);
    name(
        &mut out,
        &obj,
        Some(old.name.as_str()),
        Some(new.name.as_str()),
    );
    if !rules.same_type(old, new) {
        changed(
            &mut out,
            &obj,
            props::DATA_TYPE,
            text(&rules.type_text(old)),
            text(&rules.type_text(new)),
        );
    }
    if old.nullable != new.nullable {
        changed(
            &mut out,
            &obj,
            props::NULLABLE,
            Value::Bool(old.nullable),
            Value::Bool(new.nullable),
        );
    }
    if !rules.same_default(old, new) {
        out.push(PropertyDiff::property(
            obj.clone(),
            props::DEFAULT,
            old.default.as_deref().map(text),
            new.default.as_deref().map(text),
        ));
    }
    if old.auto_increment != new.auto_increment {
        changed(
            &mut out,
            &obj,
            props::AUTO_INCREMENT,
            Value::Bool(old.auto_increment),
            Value::Bool(new.auto_increment),
        );
    }
    let same_identity = match (&old.identity, &new.identity) {
        (None, None) => true,
        (Some(before), Some(after)) => {
            before.always == after.always
                && (before.start.is_none() || after.start.is_none() || before.start == after.start)
                && (before.increment.is_none()
                    || after.increment.is_none()
                    || before.increment == after.increment)
        }
        _ => false,
    };
    if !same_identity {
        out.push(PropertyDiff::property(
            obj.clone(),
            props::IDENTITY,
            old.identity.as_ref().map(json),
            new.identity.as_ref().map(json),
        ));
    }
    let same_generated = match (&old.generated, &new.generated) {
        (None, None) => true,
        (Some(before), Some(after)) => {
            before.stored == after.stored
                && rules.same_expression(&before.expression, &after.expression)
        }
        _ => false,
    };
    if !same_generated {
        out.push(PropertyDiff::property(
            obj.clone(),
            props::GENERATED,
            old.generated.as_ref().map(json),
            new.generated.as_ref().map(json),
        ));
    }
    if let (Some(before), Some(after)) = (&old.collation, &new.collation)
        && before != after
    {
        changed(&mut out, &obj, props::COLLATION, text(before), text(after));
    }
    comment(
        &mut out,
        &obj,
        old.comment.as_deref(),
        new.comment.as_deref(),
    );
    extras(
        &mut out,
        &obj,
        ObjectKind::Column,
        &old.extras,
        &new.extras,
        rules,
    );
    out
}

/// Differences between two versions of one index of `table` (object named from `new`).
pub fn diff_indexes(
    table: &ObjectRef,
    old: &IndexDef,
    new: &IndexDef,
    rules: &dyn CompareRules,
) -> Vec<PropertyDiff> {
    let mut out = Vec::new();
    let obj = table.child(ObjectKind::Index, &new.name);
    name(
        &mut out,
        &obj,
        Some(old.name.as_str()),
        Some(new.name.as_str()),
    );
    let same_columns = old.columns.len() == new.columns.len()
        && old.columns.iter().zip(&new.columns).all(|(before, after)| {
            before.is_expression == after.is_expression
                && before.descending == after.descending
                && before.length == after.length
                && if before.is_expression {
                    rules.same_expression(&before.name, &after.name)
                } else {
                    before.name == after.name
                }
        });
    if !same_columns {
        changed(
            &mut out,
            &obj,
            props::COLUMNS,
            json(&old.columns),
            json(&new.columns),
        );
    }
    if old.unique != new.unique {
        changed(
            &mut out,
            &obj,
            props::UNIQUE,
            Value::Bool(old.unique),
            Value::Bool(new.unique),
        );
    }
    if old.is_constraint != new.is_constraint {
        changed(
            &mut out,
            &obj,
            props::CONSTRAINT,
            Value::Bool(old.is_constraint),
            Value::Bool(new.is_constraint),
        );
    }
    let same_predicate = match (&old.predicate, &new.predicate) {
        (None, None) => true,
        (Some(before), Some(after)) => rules.same_expression(before, after),
        _ => false,
    };
    if !same_predicate {
        out.push(PropertyDiff::property(
            obj.clone(),
            props::PREDICATE,
            old.predicate.as_deref().map(text),
            new.predicate.as_deref().map(text),
        ));
    }
    extras(
        &mut out,
        &obj,
        ObjectKind::Index,
        &old.extras,
        &new.extras,
        rules,
    );
    out
}

fn action(fk_action: FkAction) -> Value {
    match serde_json::to_value(fk_action) {
        Ok(serde_json::Value::String(snake)) => Value::Text(snake),
        _ => Value::Null,
    }
}

fn table_ref_text(target: &ObjectRef) -> Value {
    match &target.schema {
        Some(schema) => Value::Text(format!("{schema}.{}", target.name)),
        None => text(&target.name),
    }
}

/// Differences between two versions of one foreign key of `table` (object named from `new`, or
/// from `old` when `new` is unnamed).
pub fn diff_foreign_keys(
    table: &ObjectRef,
    old: &ForeignKeyDef,
    new: &ForeignKeyDef,
    rules: &dyn CompareRules,
) -> Vec<PropertyDiff> {
    let mut out = Vec::new();
    let label = new.name.as_deref().or(old.name.as_deref()).unwrap_or("");
    let obj = table.child(ObjectKind::ForeignKey, label);
    name(&mut out, &obj, old.name.as_deref(), new.name.as_deref());
    if old.columns != new.columns {
        changed(
            &mut out,
            &obj,
            props::COLUMNS,
            json(&old.columns),
            json(&new.columns),
        );
    }
    if !rules.same_table_ref(&old.ref_table, &new.ref_table) {
        changed(
            &mut out,
            &obj,
            props::REF_TABLE,
            table_ref_text(&old.ref_table),
            table_ref_text(&new.ref_table),
        );
    }
    if old.ref_columns != new.ref_columns {
        changed(
            &mut out,
            &obj,
            props::REF_COLUMNS,
            json(&old.ref_columns),
            json(&new.ref_columns),
        );
    }
    if old.on_delete != new.on_delete {
        changed(
            &mut out,
            &obj,
            props::ON_DELETE,
            action(old.on_delete),
            action(new.on_delete),
        );
    }
    if old.on_update != new.on_update {
        changed(
            &mut out,
            &obj,
            props::ON_UPDATE,
            action(old.on_update),
            action(new.on_update),
        );
    }
    extras(
        &mut out,
        &obj,
        ObjectKind::ForeignKey,
        &old.extras,
        &new.extras,
        rules,
    );
    out
}

/// Differences between two versions of one check of `table` (object named from `new`, or from
/// `old` when `new` is unnamed).
pub fn diff_checks(
    table: &ObjectRef,
    old: &CheckDef,
    new: &CheckDef,
    rules: &dyn CompareRules,
) -> Vec<PropertyDiff> {
    let mut out = Vec::new();
    let label = new.name.as_deref().or(old.name.as_deref()).unwrap_or("");
    let obj = table.child(ObjectKind::Check, label);
    name(&mut out, &obj, old.name.as_deref(), new.name.as_deref());
    if !rules.same_expression(&old.expression, &new.expression) {
        changed(
            &mut out,
            &obj,
            props::EXPRESSION,
            text(&old.expression),
            text(&new.expression),
        );
    }
    extras(
        &mut out,
        &obj,
        ObjectKind::Check,
        &old.extras,
        &new.extras,
        rules,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{DataType, IdentityDef, IndexColumn};
    use pretty_assertions::assert_eq;

    fn base() -> TableDef {
        let mut table = TableDef::new(ObjectRef::table(Some("db"), Some("public"), "orders"));
        let mut id = ColumnDef::new("id", DataType::INT64).not_null();
        id.native_type = "bigint".into();
        id.auto_increment = true;
        id.default = Some("nextval('orders_id_seq'::regclass)".into());
        table.columns.push(id);
        let mut customer = ColumnDef::new("customer", DataType::INT32);
        customer.native_type = "integer".into();
        table.columns.push(customer);
        let mut note = ColumnDef::new("note", DataType::Text);
        note.native_type = "text".into();
        table.columns.push(note);
        table.primary_key = Some(PrimaryKeyDef {
            name: Some("orders_pkey".into()),
            columns: vec!["id".into()],
        });
        table.indexes.push(IndexDef {
            name: "orders_customer_idx".into(),
            columns: vec![IndexColumn::column("customer")],
            unique: false,
            is_constraint: false,
            predicate: None,
            extras: Extras::new(),
        });
        table.foreign_keys.push(ForeignKeyDef {
            name: None,
            columns: vec!["customer".into()],
            ref_table: ObjectRef::table(None, Some("public"), "customers"),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::NoAction,
            on_update: FkAction::NoAction,
            extras: Extras::new(),
        });
        table.checks.push(CheckDef {
            name: Some("orders_customer_chk".into()),
            expression: "customer > 0".into(),
            extras: Extras::new(),
        });
        table
    }

    #[test]
    fn identical_and_equivalent_tables_have_no_diff() {
        let old = base();
        assert_eq!(diff_tables(&old, &old, &DefaultRules), []);
        let mut new = base();
        // Whitespace in expressions, an empty comment, an unreported collation, a server-made
        // FK name, the generated serial default and identity details the spec leaves out.
        new.checks[0].expression = "customer   >  0".into();
        new.comment = Some(String::new());
        new.columns[2].collation = Some("C".into());
        new.foreign_keys[0].name = Some("orders_customer_fkey".into());
        new.columns[0].default = None;
        new.columns[1].native_type = "INTEGER".into();
        new.extras.insert(props::UNLOGGED, false);
        assert_eq!(diff_tables(&old, &new, &DefaultRules), []);
        // The neutral type decides when a native type is missing.
        new.columns[1].native_type = String::new();
        assert_eq!(diff_tables(&old, &new, &DefaultRules), []);
    }

    #[test]
    fn changes_come_in_the_documented_order() {
        let old = base();
        let mut new = base();
        new.obj.name = "orders2".into();
        new.comment = Some("Orders".into());
        new.columns.remove(2); // drop note
        new.columns[1].nullable = false;
        new.columns[1].native_type = "bigint".into();
        new.columns.push(ColumnDef::new(
            "total",
            DataType::Decimal {
                precision: Some(10),
                scale: Some(2),
            },
        ));
        new.columns.swap(0, 1); // customer before id
        new.primary_key = Some(PrimaryKeyDef {
            name: None,
            columns: vec!["id".into(), "customer".into()],
        });
        new.indexes[0].unique = true;
        new.foreign_keys[0].on_delete = FkAction::Cascade;
        new.checks[0].expression = "customer >= 0".into();
        let diffs = diff_tables(&old, &new, &DefaultRules);
        let shape: Vec<(ObjectKind, String, Option<&str>, DiffKind)> = diffs
            .iter()
            .map(|diff| {
                (
                    diff.object.kind,
                    diff.object.name.clone(),
                    diff.prop.as_ref().map(PropId::as_str),
                    diff.kind,
                )
            })
            .collect();
        let expected = [
            (
                ObjectKind::Table,
                "orders2",
                Some("name"),
                DiffKind::Changed,
            ),
            (
                ObjectKind::Table,
                "orders2",
                Some("comment"),
                DiffKind::Added,
            ),
            (ObjectKind::Column, "note", None, DiffKind::Removed),
            (
                ObjectKind::Column,
                "customer",
                Some("data_type"),
                DiffKind::Changed,
            ),
            (
                ObjectKind::Column,
                "customer",
                Some("nullable"),
                DiffKind::Changed,
            ),
            (ObjectKind::Column, "total", None, DiffKind::Added),
            (
                ObjectKind::Column,
                "customer",
                Some("position"),
                DiffKind::Changed,
            ),
            (
                ObjectKind::Column,
                "id",
                Some("position"),
                DiffKind::Changed,
            ),
            (
                ObjectKind::Table,
                "orders2",
                Some("primary_key"),
                DiffKind::Changed,
            ),
            (
                ObjectKind::Index,
                "orders_customer_idx",
                Some("unique"),
                DiffKind::Changed,
            ),
            (
                ObjectKind::ForeignKey,
                "",
                Some("on_delete"),
                DiffKind::Changed,
            ),
            (
                ObjectKind::Check,
                "orders_customer_chk",
                Some("expression"),
                DiffKind::Changed,
            ),
        ]
        .map(|(kind, obj, prop, diff)| (kind, obj.to_owned(), prop, diff));
        assert_eq!(shape, expected);
        let text: Vec<String> = diffs.iter().map(ToString::to_string).collect();
        assert_eq!(
            text[0],
            "~ table:db.public.orders2 .name: orders -> orders2"
        );
        assert_eq!(text[1], "+ table:db.public.orders2 .comment: Orders");
        assert_eq!(
            text[3],
            "~ column:db.public.orders2.customer .data_type: integer -> bigint"
        );
        assert_eq!(
            text[6],
            "~ column:db.public.orders2.customer .position: 1 -> 0"
        );
        assert_eq!(
            text[10],
            "~ foreign_key:db.public.orders2 .on_delete: no_action -> cascade"
        );
        assert!(matches!(diffs[2].old, Some(Value::Json(_))));
        assert!(matches!(diffs[5].new, Some(Value::Json(_))));
        assert_eq!(diffs[7].new, Some(Value::Int(1)));
    }

    #[test]
    fn children_added_and_removed_by_name_or_content() {
        let old = base();
        let mut new = base();
        new.indexes[0].name = "orders_customer_idx2".into();
        new.checks[0].name = None; // unnamed with the same expression: same check
        new.foreign_keys[0].ref_columns = vec!["code".into()]; // other content: replaced
        let diffs = diff_tables(&old, &new, &DefaultRules);
        let shape: Vec<(ObjectKind, &str, DiffKind)> = diffs
            .iter()
            .map(|diff| (diff.object.kind, diff.object.name.as_str(), diff.kind))
            .collect();
        assert_eq!(
            shape,
            [
                (ObjectKind::Index, "orders_customer_idx", DiffKind::Removed),
                (ObjectKind::Index, "orders_customer_idx2", DiffKind::Added),
                (ObjectKind::ForeignKey, "", DiffKind::Removed),
                (ObjectKind::ForeignKey, "", DiffKind::Added),
            ]
        );
        assert!(matches!(diffs[1].new, Some(Value::Json(_))));
    }

    #[test]
    fn column_details() {
        let table = ObjectRef::table(None, None, "t");
        let mut old = ColumnDef::new("c", DataType::INT64);
        old.identity = Some(IdentityDef {
            always: true,
            start: Some(1),
            increment: None,
        });
        old.auto_increment = true;
        let mut new = old.clone();
        new.identity = Some(IdentityDef {
            always: true,
            start: None,
            increment: Some(1),
        });
        assert_eq!(diff_columns(&table, &old, &new, &DefaultRules), []);
        if let Some(identity) = new.identity.as_mut() {
            identity.always = false;
        }
        new.default = Some("0".into());
        new.comment = Some("count".into());
        new.extras.insert(props::CHARSET, "utf8mb4");
        old.extras.insert(props::CHARSET, "latin1");
        let diffs = diff_columns(&table, &old, &new, &DefaultRules);
        let names: Vec<&str> = diffs
            .iter()
            .filter_map(|diff| diff.prop.as_ref().map(PropId::as_str))
            .collect();
        // The default is not compared: both columns auto-increment.
        assert_eq!(names, ["identity", "comment", "charset"]);

        let mut text_col = ColumnDef::new("c", DataType::Text);
        text_col.default = Some("'x'".into());
        let mut renamed = text_col.clone();
        renamed.name = "d".into();
        renamed.default = None;
        let diffs = diff_columns(&table, &text_col, &renamed, &DefaultRules);
        assert_eq!(diffs[0].prop, Some(props::NAME));
        assert_eq!(diffs[1].prop, Some(props::DEFAULT));
        assert_eq!(diffs[1].kind, DiffKind::Removed);
        assert_eq!(diffs[0].object.name, "d");
    }

    #[test]
    fn other_kinds_compare_whole_definitions() {
        let view = |definition: &str| {
            ObjectDef::View(oxbase_types::ViewDef {
                obj: ObjectRef::new(ObjectKind::View, "v"),
                definition: definition.into(),
                columns: vec![],
                comment: None,
                extras: Extras::new(),
            })
        };
        assert_eq!(
            diff_objects(&view("SELECT 1"), &view("SELECT 1"), &DefaultRules),
            []
        );
        let diffs = diff_objects(&view("SELECT 1"), &view("SELECT 2"), &DefaultRules);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].prop, Some(props::DEFINITION));
        let cmp = RulesComparator(DefaultRules);
        let table = ObjectDef::Table(base());
        assert_eq!(cmp.diff(&table, &table), []);
    }

    #[test]
    fn whitespace_normalization_keeps_quoted_text() {
        assert_eq!(normalize_ws("  a  >\n\t 1 "), "a > 1");
        assert_eq!(normalize_ws("x = 'a   b'  "), "x = 'a   b'");
        assert_eq!(normalize_ws("\"my  col\"  IS NULL"), "\"my  col\" IS NULL");
    }
}
