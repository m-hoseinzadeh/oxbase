//! Structural comparison of views, materialized views, routines, triggers and sequences (M8,
//! `docs/m8-contracts.md` §5.2), shared by every engine's [`Comparator`] cells for those kinds,
//! the way [`crate::compare`] is shared for tables.
//!
//! Structure sync compares two servers of the **same engine**, so engine-native text (a view
//! query, a routine body, a trigger condition) is compared as the server reports it, after
//! [`CompareRules::same_expression`] (whitespace collapsed by default) and after the caller has
//! moved names inside it from the source container to the target (`oxbase_diff::retarget_sql`).
//!
//! # Property ids and order (fixed)
//!
//! | Object | Properties, in order (`oxbase_types::props`) | Value |
//! |---|---|---|
//! | view, materialized view | `definition`, `columns`, `comment`, extras | text, JSON list, text |
//! | function, procedure | `args`, `returns`, `language`, `body` (or `definition` when the engine reports no body), `comment`, extras | JSON list of `RoutineArg`, text, text, text |
//! | trigger | `timing`, `events`, `update_columns`, `level`, `condition`, `function`, `body` (or `definition`), `enabled`, `comment`, extras | snake_case text, JSON list, JSON list, text, text, `schema.name` text, text, bool, text |
//! | sequence | `data_type`, `start`, `increment`, `min_value`, `max_value`, `cache`, `cycle`, `owned_by`, `comment`, extras | text, decimal text (×5), bool, `table.column` text, text |
//!
//! Every difference is a [`PropertyDiff::property`] on the compared object's own ref (from `b`).
//! Absent and empty comments are equal; extras follow the [`crate::compare`] rules (compared
//! only where both sides have them and [`CompareRules::compare_extra`] allows it). Two
//! definitions of different kinds, or a kind outside this module, compare as one `definition`
//! difference (never a panic).
//!
//! Owner of the body: the **structure-sync** stream.

use oxbase_types::{
    Extras, ObjectDef, ObjectKind, ObjectRef, PropId, PropertyDiff, RoutineArg, RoutineDef,
    SequenceDef, TriggerDef, TriggerLevel, TriggerTiming, Value, ViewDef, props,
};
use serde::Serialize;

use crate::Comparator;
use crate::compare::{CompareRules, normalize_ws};

/// A [`Comparator`] for `View`, `MatView`, `Function`, `Procedure`, `Trigger` and `Sequence`
/// cells that runs [`diff_definitions`] with the engine's rules.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefinitionComparator<R>(pub R);

impl<R: CompareRules> Comparator for DefinitionComparator<R> {
    fn diff(&self, a: &ObjectDef, b: &ObjectDef) -> Vec<PropertyDiff> {
        diff_definitions(&self.0, a, b)
    }
}

/// Differences going from `a` (old) to `b` (new) for the kinds of the module docs, in the fixed
/// property order. Empty when equivalent.
pub fn diff_definitions(
    rules: &dyn CompareRules,
    a: &ObjectDef,
    b: &ObjectDef,
) -> Vec<PropertyDiff> {
    let mut out = Vec::new();
    match (a, b) {
        (ObjectDef::View(old), ObjectDef::View(new)) if old.obj.kind == new.obj.kind => {
            diff_views(&mut out, rules, old, new);
        }
        (ObjectDef::Routine(old), ObjectDef::Routine(new)) if old.obj.kind == new.obj.kind => {
            diff_routines(&mut out, rules, old, new);
        }
        (ObjectDef::Trigger(old), ObjectDef::Trigger(new)) => {
            diff_triggers(&mut out, rules, old, new);
        }
        (ObjectDef::Sequence(old), ObjectDef::Sequence(new)) => {
            diff_sequences(&mut out, rules, old, new);
        }
        _ if a == b => {}
        _ => out.push(PropertyDiff::property(
            b.object_ref(),
            props::DEFINITION,
            Some(json(a)),
            Some(json(b)),
        )),
    }
    out
}

fn diff_views(out: &mut Vec<PropertyDiff>, rules: &dyn CompareRules, old: &ViewDef, new: &ViewDef) {
    let object = &new.obj;
    if !rules.same_expression(&old.definition, &new.definition) {
        changed(
            out,
            object,
            props::DEFINITION,
            text(&old.definition),
            text(&new.definition),
        );
    }
    if old.columns != new.columns {
        changed(
            out,
            object,
            props::COLUMNS,
            json(&old.columns),
            json(&new.columns),
        );
    }
    comment(out, object, old.comment.as_deref(), new.comment.as_deref());
    extras(out, object, &old.extras, &new.extras, rules);
}

fn diff_routines(
    out: &mut Vec<PropertyDiff>,
    rules: &dyn CompareRules,
    old: &RoutineDef,
    new: &RoutineDef,
) {
    let object = &new.obj;
    if !same_args(rules, &old.args, &new.args) {
        changed(out, object, props::ARGS, json(&old.args), json(&new.args));
    }
    same_words(
        out,
        object,
        props::RETURNS,
        old.returns.as_deref(),
        new.returns.as_deref(),
    );
    same_words(
        out,
        object,
        props::LANGUAGE,
        old.language.as_deref(),
        new.language.as_deref(),
    );
    body_or_definition(
        out,
        rules,
        object,
        (old.body.as_deref(), old.definition.as_deref()),
        (new.body.as_deref(), new.definition.as_deref()),
    );
    comment(out, object, old.comment.as_deref(), new.comment.as_deref());
    extras(out, object, &old.extras, &new.extras, rules);
}

fn diff_triggers(
    out: &mut Vec<PropertyDiff>,
    rules: &dyn CompareRules,
    old: &TriggerDef,
    new: &TriggerDef,
) {
    let object = &new.obj;
    if old.timing != new.timing {
        changed(
            out,
            object,
            props::TIMING,
            text(timing_text(old.timing)),
            text(timing_text(new.timing)),
        );
    }
    let mut old_events = old.events.clone();
    let mut new_events = new.events.clone();
    old_events.sort();
    old_events.dedup();
    new_events.sort();
    new_events.dedup();
    if old_events != new_events {
        changed(
            out,
            object,
            props::EVENTS,
            json(&old_events),
            json(&new_events),
        );
    }
    if old.update_columns != new.update_columns {
        changed(
            out,
            object,
            props::UPDATE_COLUMNS,
            json(&old.update_columns),
            json(&new.update_columns),
        );
    }
    if old.level != new.level {
        changed(
            out,
            object,
            props::LEVEL,
            text(level_text(old.level)),
            text(level_text(new.level)),
        );
    }
    expression(
        out,
        rules,
        object,
        props::CONDITION,
        old.condition.as_deref(),
        new.condition.as_deref(),
    );
    let old_function = old.function.as_ref().map(function_text);
    let new_function = new.function.as_ref().map(function_text);
    if old_function != new_function {
        out.push(PropertyDiff::property(
            object.clone(),
            props::FUNCTION,
            old_function.map(Value::Text),
            new_function.map(Value::Text),
        ));
    }
    body_or_definition(
        out,
        rules,
        object,
        (old.body.as_deref(), old.definition.as_deref()),
        (new.body.as_deref(), new.definition.as_deref()),
    );
    if old.enabled != new.enabled {
        changed(
            out,
            object,
            props::ENABLED,
            Value::Bool(old.enabled),
            Value::Bool(new.enabled),
        );
    }
    comment(out, object, old.comment.as_deref(), new.comment.as_deref());
    extras(out, object, &old.extras, &new.extras, rules);
}

fn diff_sequences(
    out: &mut Vec<PropertyDiff>,
    rules: &dyn CompareRules,
    old: &SequenceDef,
    new: &SequenceDef,
) {
    let object = &new.obj;
    same_words(
        out,
        object,
        props::DATA_TYPE,
        old.data_type.as_deref(),
        new.data_type.as_deref(),
    );
    number(out, object, props::START, Some(old.start), Some(new.start));
    number(
        out,
        object,
        props::INCREMENT,
        Some(old.increment),
        Some(new.increment),
    );
    number(out, object, props::MIN_VALUE, old.min_value, new.min_value);
    number(out, object, props::MAX_VALUE, old.max_value, new.max_value);
    number(out, object, props::CACHE, old.cache, new.cache);
    if old.cycle != new.cycle {
        changed(
            out,
            object,
            props::CYCLE,
            Value::Bool(old.cycle),
            Value::Bool(new.cycle),
        );
    }
    let old_owner = old.owned_by.as_ref().map(owner_text);
    let new_owner = new.owned_by.as_ref().map(owner_text);
    if old_owner != new_owner {
        out.push(PropertyDiff::property(
            object.clone(),
            props::OWNED_BY,
            old_owner.map(Value::Text),
            new_owner.map(Value::Text),
        ));
    }
    comment(out, object, old.comment.as_deref(), new.comment.as_deref());
    extras(out, object, &old.extras, &new.extras, rules);
}

/// Arguments are equal when names, modes, types (case and whitespace folded) and defaults
/// ([`CompareRules::same_expression`]) are.
fn same_args(rules: &dyn CompareRules, old: &[RoutineArg], new: &[RoutineArg]) -> bool {
    old.len() == new.len()
        && old.iter().zip(new).all(|(a, b)| {
            a.name == b.name
                && a.mode == b.mode
                && same_word_text(&a.native_type, &b.native_type)
                && match (&a.default, &b.default) {
                    (None, None) => true,
                    (Some(x), Some(y)) => rules.same_expression(x, y),
                    _ => false,
                }
        })
}

/// Type names, languages: whitespace collapsed, case-insensitive.
fn same_word_text(a: &str, b: &str) -> bool {
    normalize_ws(a).eq_ignore_ascii_case(&normalize_ws(b))
}

fn same_words(
    out: &mut Vec<PropertyDiff>,
    object: &ObjectRef,
    prop: PropId,
    old: Option<&str>,
    new: Option<&str>,
) {
    let old = old.filter(|s| !s.trim().is_empty());
    let new = new.filter(|s| !s.trim().is_empty());
    let same = match (old, new) {
        (None, None) => true,
        (Some(a), Some(b)) => same_word_text(a, b),
        _ => false,
    };
    if !same {
        out.push(PropertyDiff::property(
            object.clone(),
            prop,
            old.map(text),
            new.map(text),
        ));
    }
}

fn expression(
    out: &mut Vec<PropertyDiff>,
    rules: &dyn CompareRules,
    object: &ObjectRef,
    prop: PropId,
    old: Option<&str>,
    new: Option<&str>,
) {
    let old = old.filter(|s| !s.trim().is_empty());
    let new = new.filter(|s| !s.trim().is_empty());
    let same = match (old, new) {
        (None, None) => true,
        (Some(a), Some(b)) => rules.same_expression(a, b),
        _ => false,
    };
    if !same {
        out.push(PropertyDiff::property(
            object.clone(),
            prop,
            old.map(text),
            new.map(text),
        ));
    }
}

/// `body` when both sides report one, else `definition` (module docs).
fn body_or_definition(
    out: &mut Vec<PropertyDiff>,
    rules: &dyn CompareRules,
    object: &ObjectRef,
    old: (Option<&str>, Option<&str>),
    new: (Option<&str>, Option<&str>),
) {
    match (old.0, new.0) {
        (None, None) => expression(out, rules, object, props::DEFINITION, old.1, new.1),
        (old_body, new_body) => expression(out, rules, object, props::BODY, old_body, new_body),
    }
}

fn number(
    out: &mut Vec<PropertyDiff>,
    object: &ObjectRef,
    prop: PropId,
    old: Option<i128>,
    new: Option<i128>,
) {
    if old != new {
        out.push(PropertyDiff::property(
            object.clone(),
            prop,
            old.map(|n| Value::Decimal(n.to_string())),
            new.map(|n| Value::Decimal(n.to_string())),
        ));
    }
}

fn timing_text(timing: TriggerTiming) -> &'static str {
    match timing {
        TriggerTiming::Before => "before",
        TriggerTiming::After => "after",
        TriggerTiming::InsteadOf => "instead_of",
    }
}

fn level_text(level: TriggerLevel) -> &'static str {
    match level {
        TriggerLevel::Row => "row",
        TriggerLevel::Statement => "statement",
    }
}

/// `schema.name` of a trigger function (signature left out: the function is matched by name).
fn function_text(function: &ObjectRef) -> String {
    match &function.schema {
        Some(schema) => format!("{schema}.{}", function.name),
        None => function.name.clone(),
    }
}

/// `table.column` of a sequence owner.
fn owner_text(owner: &ObjectRef) -> String {
    match &owner.parent {
        Some(table) => format!("{table}.{}", owner.name),
        None => owner.name.clone(),
    }
}

fn json<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).map_or(Value::Null, Value::Json)
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn changed(out: &mut Vec<PropertyDiff>, object: &ObjectRef, prop: PropId, old: Value, new: Value) {
    out.push(PropertyDiff::property(
        object.clone(),
        prop,
        Some(old),
        Some(new),
    ));
}

fn comment(out: &mut Vec<PropertyDiff>, object: &ObjectRef, old: Option<&str>, new: Option<&str>) {
    let old = old.filter(|c| !c.is_empty());
    let new = new.filter(|c| !c.is_empty());
    if old != new {
        out.push(PropertyDiff::property(
            object.clone(),
            props::COMMENT,
            old.map(text),
            new.map(text),
        ));
    }
}

/// Extras present on both sides that differ, where the rules compare them.
fn extras(
    out: &mut Vec<PropertyDiff>,
    object: &ObjectRef,
    old: &Extras,
    new: &Extras,
    rules: &dyn CompareRules,
) {
    let kind: ObjectKind = object.kind;
    for prop in old.changed_keys(new) {
        if !rules.compare_extra(kind, prop) {
            continue;
        }
        if let (Some(before), Some(after)) = (old.get(prop), new.get(prop)) {
            changed(out, object, prop.clone(), before.clone(), after.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::DefaultRules;
    use oxbase_types::{ArgMode, DiffKind, TriggerEvent};
    use pretty_assertions::assert_eq;

    fn props_of(diffs: &[PropertyDiff]) -> Vec<&str> {
        diffs
            .iter()
            .map(|d| d.prop.as_ref().map_or("", PropId::as_str))
            .collect()
    }

    fn view(definition: &str) -> ViewDef {
        ViewDef {
            obj: ObjectRef::schema(None, "s").child(ObjectKind::View, "v"),
            definition: definition.into(),
            columns: Vec::new(),
            comment: None,
            extras: Extras::new(),
        }
    }

    #[test]
    fn views_compare_text_columns_comments_and_extras() {
        let a = view("SELECT a\n  FROM t");
        let mut b = view("SELECT a FROM t");
        assert_eq!(
            diff_definitions(
                &DefaultRules,
                &ObjectDef::View(a.clone()),
                &ObjectDef::View(b.clone())
            ),
            Vec::new()
        );
        b.definition = "SELECT b FROM t".into();
        b.columns = vec!["x".into()];
        b.comment = Some("note".into());
        let mut a2 = a.clone();
        a2.comment = Some(String::new());
        a2.extras.insert(props::CHECK_OPTION, "local");
        b.extras.insert(props::CHECK_OPTION, "cascaded");
        // Only on one side: not compared.
        b.extras.insert(props::SECURITY, "invoker");
        let d = diff_definitions(
            &DefaultRules,
            &ObjectDef::View(a2),
            &ObjectDef::View(b.clone()),
        );
        assert_eq!(
            props_of(&d),
            ["definition", "columns", "comment", "check_option"]
        );
        assert_eq!(d[2].kind, DiffKind::Added);
        assert_eq!(d[0].object, b.obj);
    }

    #[test]
    fn routines_compare_in_the_fixed_order() {
        let r = ObjectRef::schema(None, "s")
            .child(ObjectKind::Function, "f")
            .with_signature("integer");
        let mut a = RoutineDef::new(r.clone());
        a.args.push(RoutineArg {
            name: Some("n".into()),
            mode: ArgMode::In,
            native_type: "integer".into(),
            default: None,
        });
        a.returns = Some("integer".into());
        a.language = Some("plpgsql".into());
        a.body = Some("BEGIN RETURN n; END".into());
        a.definition = Some("CREATE FUNCTION ...".into());
        let mut b = a.clone();
        b.args[0].native_type = "INTEGER".into();
        b.language = Some("PLPGSQL".into());
        b.body = Some("BEGIN  RETURN n;\nEND".into());
        b.definition = Some("different text".into());
        assert_eq!(
            diff_definitions(
                &DefaultRules,
                &ObjectDef::Routine(a.clone()),
                &ObjectDef::Routine(b.clone())
            ),
            Vec::new(),
            "types and languages fold case, bodies whitespace; definition ignored with bodies"
        );
        b.args[0].name = Some("m".into());
        b.returns = Some("bigint".into());
        b.language = Some("sql".into());
        b.body = Some("SELECT 1".into());
        b.comment = Some("c".into());
        a.extras.insert(props::VOLATILITY, "volatile");
        b.extras.insert(props::VOLATILITY, "stable");
        let d = diff_definitions(
            &DefaultRules,
            &ObjectDef::Routine(a.clone()),
            &ObjectDef::Routine(b.clone()),
        );
        assert_eq!(
            props_of(&d),
            [
                "args",
                "returns",
                "language",
                "body",
                "comment",
                "volatility"
            ]
        );
        // No bodies: the definition is compared.
        a.body = None;
        b = a.clone();
        b.definition = Some("CREATE FUNCTION other".into());
        let d = diff_definitions(
            &DefaultRules,
            &ObjectDef::Routine(a),
            &ObjectDef::Routine(b),
        );
        assert_eq!(props_of(&d), ["definition"]);
    }

    fn trigger() -> TriggerDef {
        TriggerDef {
            obj: ObjectRef::table(None, Some("s"), "t").child(ObjectKind::Trigger, "trg"),
            timing: TriggerTiming::After,
            events: vec![TriggerEvent::Insert],
            update_columns: Vec::new(),
            level: TriggerLevel::Row,
            condition: None,
            function: Some(ObjectRef::schema(None, "s").child(ObjectKind::Function, "f")),
            body: None,
            definition: Some("CREATE TRIGGER trg AFTER INSERT ON s.t".into()),
            enabled: true,
            comment: None,
            extras: Extras::new(),
        }
    }

    #[test]
    fn triggers_compare_every_property() {
        let a = trigger();
        let mut b = trigger();
        b.definition = Some("CREATE  TRIGGER trg AFTER INSERT\nON s.t".into());
        assert_eq!(
            diff_definitions(
                &DefaultRules,
                &ObjectDef::Trigger(a.clone()),
                &ObjectDef::Trigger(b.clone())
            ),
            Vec::new()
        );
        b.timing = TriggerTiming::Before;
        b.events = vec![TriggerEvent::Update, TriggerEvent::Insert];
        b.update_columns = vec!["c".into()];
        b.level = TriggerLevel::Statement;
        b.condition = Some("NEW.c > 0".into());
        b.function = Some(ObjectRef::schema(None, "s").child(ObjectKind::Function, "g"));
        b.definition = Some("CREATE TRIGGER trg BEFORE ...".into());
        b.enabled = false;
        b.comment = Some("c".into());
        let d = diff_definitions(
            &DefaultRules,
            &ObjectDef::Trigger(a),
            &ObjectDef::Trigger(b),
        );
        assert_eq!(
            props_of(&d),
            [
                "timing",
                "events",
                "update_columns",
                "level",
                "condition",
                "function",
                "definition",
                "enabled",
                "comment"
            ]
        );
        assert_eq!(d[0].new, Some(Value::Text("before".into())));
        assert_eq!(d[5].new, Some(Value::Text("s.g".into())));
    }

    #[test]
    fn sequences_compare_numbers_as_decimal_text() {
        let r = ObjectRef::schema(None, "s").child(ObjectKind::Sequence, "q");
        let a = SequenceDef::new(r.clone());
        let mut b = SequenceDef::new(r);
        assert_eq!(
            diff_definitions(
                &DefaultRules,
                &ObjectDef::Sequence(a.clone()),
                &ObjectDef::Sequence(b.clone())
            ),
            Vec::new()
        );
        b.data_type = Some("bigint".into());
        b.start = 100;
        b.increment = 5;
        b.min_value = Some(1);
        b.max_value = Some(i128::from(i64::MAX));
        b.cache = Some(10);
        b.cycle = true;
        b.owned_by = Some(ObjectRef::table(None, Some("s"), "t").child(ObjectKind::Column, "id"));
        b.comment = Some("c".into());
        let d = diff_definitions(
            &DefaultRules,
            &ObjectDef::Sequence(a),
            &ObjectDef::Sequence(b),
        );
        assert_eq!(
            props_of(&d),
            [
                "data_type",
                "start",
                "increment",
                "min_value",
                "max_value",
                "cache",
                "cycle",
                "owned_by",
                "comment"
            ]
        );
        assert_eq!(d[1].new, Some(Value::Decimal("100".into())));
        assert_eq!(d[4].new, Some(Value::Decimal("9223372036854775807".into())));
        assert_eq!(d[7].new, Some(Value::Text("t.id".into())));
        assert_eq!(d[3].kind, DiffKind::Added);
    }

    #[test]
    fn other_pairs_are_one_definition_difference() {
        let a = ObjectDef::View(view("SELECT 1"));
        let r = ObjectRef::schema(None, "s").child(ObjectKind::Sequence, "q");
        let b = ObjectDef::Sequence(SequenceDef::new(r));
        let d = diff_definitions(&DefaultRules, &a, &b);
        assert_eq!(props_of(&d), ["definition"]);
        assert_eq!(diff_definitions(&DefaultRules, &b, &b.clone()), Vec::new());
    }
}
