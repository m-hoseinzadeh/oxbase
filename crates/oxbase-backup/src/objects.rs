//! The objects of a container for a backup (read through the engine's fetchers), their
//! creation order, and the SQL-text relocation a restore into another container needs.
//!
//! # Reading ([`load_container`])
//!
//! Top-level kinds ([`TOP_KINDS`]) are listed under the container and loaded one by one;
//! triggers are listed under each loaded table, view and materialized view. Kinds without a
//! fetcher, or that the server version does not have (`Capabilities::supports_kind`), are
//! skipped silently; system objects are never backed up. An object that fails to load is left
//! out with a warning (tables excepted: a table that cannot be read fails the backup, because
//! its rows would be missing). A sequence owned by an auto-increment column of a backed-up table
//! (PostgreSQL `serial` / identity) is not backed up on its own: creating the table creates it.
//!
//! # Order ([`creation_order`])
//!
//! Sequences, then the functions and procedures a table's defaults, generated columns or checks
//! call, then tables by foreign keys (parents first, `oxbase_dialect::table_dependencies`;
//! tables on a cycle in insertion order), then the other functions and procedures, then views
//! and materialized views (a view whose query names another view after it), then triggers: the
//! order of the structure sync script (`docs/m8-contracts.md` §5.4). Within a group, the
//! fetchers' order (by name).
//!
//! # After the rows ([`after_data`], [`late_order`])
//!
//! A restore creates materialized views (they read the tables) and triggers (they must not fire
//! on restored rows) after the rows load, and with them every view, function or procedure whose
//! SQL text names one of those (a view over a materialized view), in an order where what a text
//! names comes first.

use oxbase_dialect::{DependencyGraph, DialectRegistry, table_dependencies};
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    Engine, Error, Location, ObjectDef, ObjectKind, ObjectRef, Result, ServerVersion, TableDef,
};

/// The kinds a backup lists directly under its container.
pub(crate) const TOP_KINDS: [ObjectKind; 6] = [
    ObjectKind::Sequence,
    ObjectKind::Table,
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::View,
    ObjectKind::MatView,
];

/// What [`load_container`] read.
#[derive(Debug, Default)]
pub(crate) struct Loaded {
    pub objects: Vec<ObjectDef>,
    pub warnings: Vec<String>,
}

/// `true` when `r` is selected by `selection` (empty: everything). A selected ref matches by
/// kind and name, and by signature and parent when it names them.
pub(crate) fn is_selected(selection: &[ObjectRef], r: &ObjectRef) -> bool {
    selection.is_empty()
        || selection.iter().any(|s| {
            s.kind == r.kind
                && s.name == r.name
                && (s.signature.is_none() || s.signature == r.signature)
                && (s.parent.is_none() || s.parent == r.parent)
        })
}

/// Reads the objects of `container` (module docs). `on_object` gets (loaded, listed) after each
/// object.
pub(crate) async fn load_container(
    dialects: &DialectRegistry,
    connection: &dyn Connection,
    container: &ObjectRef,
    selection: &[ObjectRef],
    on_object: &(dyn Fn(u32, u32) + Send + Sync),
    cancel: &CancellationToken,
) -> Result<Loaded> {
    let engine = connection.engine();
    let version = connection.server_version().clone();
    let supports = |kind: ObjectKind| {
        dialects
            .capabilities(engine)
            .map_or(true, |c| c.supports_kind(kind, &version))
    };
    let mut out = Loaded::default();
    let mut listed = 0_u32;
    let mut loaded = 0_u32;
    for kind in TOP_KINDS {
        if !supports(kind) {
            continue;
        }
        let Ok(fetcher) = dialects.fetcher(engine, kind) else {
            continue;
        };
        check(cancel)?;
        let summaries = match fetcher.list(connection, container, cancel.clone()).await {
            Ok(s) => s,
            Err(e) if e.is_cancelled() || kind == ObjectKind::Table => return Err(e),
            Err(e) => {
                out.warnings
                    .push(format!("the {kind} objects could not be listed: {e}"));
                continue;
            }
        };
        let wanted: Vec<ObjectRef> = summaries
            .into_iter()
            .filter(|s| !s.system && is_selected(selection, &s.obj))
            .map(|s| s.obj)
            .collect();
        listed += u32::try_from(wanted.len()).unwrap_or(u32::MAX);
        for r in wanted {
            check(cancel)?;
            match fetcher.load(connection, &r, cancel.clone()).await {
                Ok(def) => out.objects.push(def),
                Err(e) if e.is_cancelled() || kind == ObjectKind::Table => return Err(e),
                Err(e) => out.warnings.push(format!(
                    "{kind} {} was left out: it could not be read: {e}",
                    r.display_name()
                )),
            }
            loaded += 1;
            on_object(loaded, listed);
        }
    }
    load_triggers(
        dialects,
        connection,
        selection,
        &version,
        &mut out,
        (&mut loaded, &mut listed),
        on_object,
        cancel,
    )
    .await?;
    drop_owned_sequences(&mut out.objects);
    Ok(out)
}

/// Triggers of the loaded tables and views: all of them when the owner was selected (or
/// nothing was), else only the explicitly selected ones.
#[allow(
    clippy::too_many_arguments,
    reason = "one private step of load_container"
)]
async fn load_triggers(
    dialects: &DialectRegistry,
    connection: &dyn Connection,
    selection: &[ObjectRef],
    version: &ServerVersion,
    out: &mut Loaded,
    (loaded, listed): (&mut u32, &mut u32),
    on_object: &(dyn Fn(u32, u32) + Send + Sync),
    cancel: &CancellationToken,
) -> Result<()> {
    let engine = connection.engine();
    let kind = ObjectKind::Trigger;
    if !dialects
        .capabilities(engine)
        .map_or(true, |c| c.supports_kind(kind, version))
    {
        return Ok(());
    }
    let Ok(fetcher) = dialects.fetcher(engine, kind) else {
        return Ok(());
    };
    let owners: Vec<ObjectRef> = out
        .objects
        .iter()
        .filter(|d| {
            matches!(
                d.kind(),
                ObjectKind::Table | ObjectKind::View | ObjectKind::MatView
            )
        })
        .map(ObjectDef::object_ref)
        .collect();
    for owner in owners {
        check(cancel)?;
        let summaries = match fetcher.list(connection, &owner, cancel.clone()).await {
            Ok(s) => s,
            Err(e) if e.is_cancelled() => return Err(e),
            Err(e) => {
                out.warnings.push(format!(
                    "the triggers of {} could not be listed: {e}",
                    owner.display_name()
                ));
                continue;
            }
        };
        let owner_selected = is_selected(selection, &owner);
        let wanted: Vec<ObjectRef> = summaries
            .into_iter()
            .filter(|s| !s.system && (owner_selected || is_selected(selection, &s.obj)))
            .map(|s| s.obj)
            .collect();
        *listed += u32::try_from(wanted.len()).unwrap_or(u32::MAX);
        for r in wanted {
            check(cancel)?;
            match fetcher.load(connection, &r, cancel.clone()).await {
                Ok(def) => out.objects.push(def),
                Err(e) if e.is_cancelled() => return Err(e),
                Err(e) => out.warnings.push(format!(
                    "trigger {} was left out: it could not be read: {e}",
                    r.display_name()
                )),
            }
            *loaded += 1;
            on_object(*loaded, *listed);
        }
    }
    Ok(())
}

/// Removes sequences owned by an auto-increment column of a backed-up table (module docs).
fn drop_owned_sequences(objects: &mut Vec<ObjectDef>) {
    let owned: Vec<bool> = objects
        .iter()
        .map(|d| {
            let ObjectDef::Sequence(s) = d else {
                return false;
            };
            let Some(owner) = &s.owned_by else {
                return false;
            };
            objects.iter().any(|t| match t {
                ObjectDef::Table(t) => {
                    owner.parent.as_deref() == Some(t.obj.name.as_str())
                        && t.column(&owner.name).is_some_and(|c| c.auto_increment)
                }
                _ => false,
            })
        })
        .collect();
    let mut flags = owned.into_iter();
    objects.retain(|_| !flags.next().unwrap_or(false));
}

fn check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn rank(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Sequence => 0,
        ObjectKind::Table => 1,
        ObjectKind::Function | ObjectKind::Procedure => 2,
        ObjectKind::View | ObjectKind::MatView => 3,
        ObjectKind::Trigger => 4,
        _ => 5,
    }
}

/// `true` when `name` occurs in `text` as a whole identifier (ASCII case-insensitive).
pub(crate) fn mentions(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let hay = text.to_lowercase();
    let needle = name.to_lowercase();
    let mut from = 0;
    while let Some(pos) = hay[from..].find(&needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before = hay[..start].chars().next_back();
        let after = hay[end..].chars().next();
        if !before.is_some_and(ident) && !after.is_some_and(ident) {
            return true;
        }
        from = start + needle.len().max(1);
        while from < hay.len() && !hay.is_char_boundary(from) {
            from += 1;
        }
    }
    false
}

/// `objects` in creation order (module docs).
pub(crate) fn creation_order(objects: Vec<ObjectDef>) -> Vec<ObjectDef> {
    let mut groups: [Vec<ObjectDef>; 6] = Default::default();
    for d in objects {
        groups[usize::from(rank(d.kind()))].push(d);
    }
    let [sequences, tables, routines, views, triggers, others] = groups;

    // Routines a table's expressions call are created before the tables.
    let (early, routines): (Vec<ObjectDef>, Vec<ObjectDef>) = routines.into_iter().partition(|r| {
        tables.iter().any(|t| match t {
            ObjectDef::Table(t) => table_mentions(t, r.name()),
            _ => false,
        })
    });

    // Tables: foreign keys.
    let defs: Vec<TableDef> = tables
        .iter()
        .filter_map(|d| match d {
            ObjectDef::Table(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    let order = table_dependencies(&defs).order().order;
    let tables = reorder(tables, &order);

    // Views: names of other views inside their queries.
    let mut graph = DependencyGraph::new();
    for v in &views {
        graph.add(v.object_ref());
    }
    for v in &views {
        let ObjectDef::View(def) = v else { continue };
        for w in &views {
            if w.object_ref() != v.object_ref() && mentions(&def.definition, w.name()) {
                graph.depends_on(&v.object_ref(), &w.object_ref());
            }
        }
    }
    let views = reorder(views, &graph.order().order);

    let mut out = sequences;
    out.extend(early);
    out.extend(tables);
    out.extend(routines);
    out.extend(views);
    out.extend(triggers);
    out.extend(others);
    out
}

/// `true` when a default, generated column, check or index predicate of `t` names `name`.
fn table_mentions(t: &TableDef, name: &str) -> bool {
    t.columns.iter().any(|c| {
        c.default.as_deref().is_some_and(|d| mentions(d, name))
            || c.generated
                .as_ref()
                .is_some_and(|g| mentions(&g.expression, name))
    }) || t.checks.iter().any(|c| mentions(&c.expression, name))
        || t.indexes
            .iter()
            .any(|i| i.predicate.as_deref().is_some_and(|p| mentions(p, name)))
}

/// `true` when the SQL text of a view, materialized view or routine names `name`.
fn text_mentions(def: &ObjectDef, name: &str) -> bool {
    match def {
        ObjectDef::View(v) => mentions(&v.definition, name),
        ObjectDef::Routine(r) => [r.body.as_deref(), r.definition.as_deref()]
            .into_iter()
            .flatten()
            .any(|t| mentions(t, name)),
        _ => false,
    }
}

/// For `defs` (what a restore creates, in creation order): `true` for each object created after
/// the rows (module docs): materialized views, triggers, and views and routines whose text
/// names another such object (transitively).
pub(crate) fn after_data(defs: &[&ObjectDef]) -> Vec<bool> {
    let mut late: Vec<bool> = defs
        .iter()
        .map(|d| matches!(d.kind(), ObjectKind::MatView | ObjectKind::Trigger))
        .collect();
    loop {
        let mut changed = false;
        for i in 0..defs.len() {
            if late[i]
                || !matches!(
                    defs[i].kind(),
                    ObjectKind::View | ObjectKind::Function | ObjectKind::Procedure
                )
            {
                continue;
            }
            let names_late = (0..defs.len()).any(|j| {
                j != i
                    && late[j]
                    && defs[j].kind() != ObjectKind::Trigger
                    && text_mentions(defs[i], defs[j].name())
            });
            if names_late {
                late[i] = true;
                changed = true;
            }
        }
        if !changed {
            return late;
        }
    }
}

/// The creation order of `defs` (the objects created after the rows, in creation order) as
/// indexes into `defs`: what a view's or routine's text names first, a trigger after the view
/// it belongs to; otherwise the given order.
pub(crate) fn late_order(defs: &[&ObjectDef]) -> Vec<usize> {
    let mut graph = DependencyGraph::new();
    for d in defs {
        graph.add(d.object_ref());
    }
    for a in defs {
        for b in defs {
            let (ra, rb) = (a.object_ref(), b.object_ref());
            if ra == rb {
                continue;
            }
            let needs = match a {
                ObjectDef::Trigger(t) => t
                    .obj
                    .owner()
                    .is_some_and(|o| o.kind == rb.kind && o.name == rb.name),
                _ => b.kind() != ObjectKind::Trigger && text_mentions(a, b.name()),
            };
            if needs {
                graph.depends_on(&ra, &rb);
            }
        }
    }
    let order = graph.order().order;
    let mut out: Vec<usize> = order
        .iter()
        .filter_map(|r| defs.iter().position(|d| d.object_ref() == *r))
        .collect();
    for i in 0..defs.len() {
        if !out.contains(&i) {
            out.push(i);
        }
    }
    out
}

/// `defs` in the order of their refs in `order` (refs not in `order` keep their place at the
/// end).
fn reorder(defs: Vec<ObjectDef>, order: &[ObjectRef]) -> Vec<ObjectDef> {
    let mut slots: Vec<Option<ObjectDef>> = defs.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(slots.len());
    for r in order {
        if let Some(slot) = slots
            .iter_mut()
            .find(|s| s.as_ref().is_some_and(|d| d.object_ref() == *r))
        {
            out.extend(slot.take());
        }
    }
    out.extend(slots.into_iter().flatten());
    out
}

/// Moves `def` from the backed-up container `from` to `to` the way a restore does: its refs
/// (`oxbase_types::relocate_def`) and the names inside its SQL text (`oxbase_diff::retarget_sql`).
/// Tests compare a restored object with its source moved this way.
pub fn move_definition(engine: Engine, def: &mut ObjectDef, from: &Location, to: &Location) {
    oxbase_types::relocate_def(def, from, to);
    if from != to {
        retarget_text(engine, def, from, to);
    }
}

/// Moves names inside the engine-native SQL text of `def` from `from` to `to`
/// (`oxbase_diff::retarget_sql`): defaults, generated columns, checks and index predicates of
/// a table; a view's query; a routine's body and definition; a trigger's condition, body and
/// definition. Only called when the containers differ.
pub(crate) fn retarget_text(engine: Engine, def: &mut ObjectDef, from: &Location, to: &Location) {
    let mv = |text: &mut String| *text = oxbase_diff::retarget_sql(engine, text, from, to);
    let mv_opt = |text: &mut Option<String>| {
        if let Some(t) = text {
            *t = oxbase_diff::retarget_sql(engine, t, from, to);
        }
    };
    match def {
        ObjectDef::Table(t) => {
            for c in &mut t.columns {
                mv_opt(&mut c.default);
                if let Some(g) = &mut c.generated {
                    mv(&mut g.expression);
                }
            }
            for c in &mut t.checks {
                mv(&mut c.expression);
            }
            for i in &mut t.indexes {
                mv_opt(&mut i.predicate);
                for c in &mut i.columns {
                    if c.is_expression {
                        mv(&mut c.name);
                    }
                }
            }
        }
        ObjectDef::View(v) => mv(&mut v.definition),
        ObjectDef::Routine(r) => {
            mv_opt(&mut r.body);
            mv_opt(&mut r.definition);
        }
        ObjectDef::Trigger(t) => {
            mv_opt(&mut t.condition);
            mv_opt(&mut t.body);
            mv_opt(&mut t.definition);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{
        ColumnDef, DataType, Extras, FkAction, ForeignKeyDef, RoutineDef, SequenceDef, TriggerDef,
        TriggerEvent, TriggerLevel, TriggerTiming, ViewDef,
    };
    use pretty_assertions::assert_eq;

    fn table(name: &str, parents: &[&str]) -> ObjectDef {
        let mut t = TableDef::new(ObjectRef::table(None, Some("s"), name));
        let mut id = ColumnDef::new("id", DataType::INT32).not_null();
        id.auto_increment = name == "orders";
        t.columns.push(id);
        for p in parents {
            t.foreign_keys.push(ForeignKeyDef {
                name: Some(format!("{name}_{p}")),
                columns: vec!["id".into()],
                ref_table: ObjectRef::table(None, Some("s"), *p),
                ref_columns: vec!["id".into()],
                on_delete: FkAction::NoAction,
                on_update: FkAction::NoAction,
                extras: Extras::default(),
            });
        }
        ObjectDef::Table(t)
    }

    fn view(name: &str, query: &str) -> ObjectDef {
        ObjectDef::View(ViewDef {
            obj: ObjectRef::schema(None, "s").child(ObjectKind::View, name),
            definition: query.into(),
            columns: Vec::new(),
            comment: None,
            extras: Extras::default(),
        })
    }

    fn sequence(name: &str, owner: Option<(&str, &str)>) -> ObjectDef {
        let r = ObjectRef::schema(None, "s").child(ObjectKind::Sequence, name);
        ObjectDef::Sequence(SequenceDef {
            obj: r,
            data_type: None,
            start: 1,
            increment: 1,
            min_value: None,
            max_value: None,
            cache: None,
            cycle: false,
            owned_by: owner
                .map(|(t, c)| ObjectRef::table(None, Some("s"), t).child(ObjectKind::Column, c)),
            comment: None,
            extras: Extras::default(),
        })
    }

    fn names(defs: &[ObjectDef]) -> Vec<String> {
        defs.iter().map(|d| d.name().to_owned()).collect()
    }

    #[test]
    fn creation_order_puts_dependencies_first() {
        let defs = vec![
            view("v_top", "SELECT * FROM v_base JOIN orders o ON true"),
            table("order_items", &["orders", "products"]),
            view("v_base", "SELECT * FROM customers"),
            table("orders", &["customers"]),
            sequence("invoice_seq", None),
            table("customers", &[]),
            table("products", &[]),
        ];
        assert_eq!(
            names(&creation_order(defs)),
            [
                "invoice_seq",
                "customers",
                "orders",
                "products",
                "order_items",
                "v_base",
                "v_top"
            ]
        );
    }

    #[test]
    fn serial_sequences_come_with_their_table() {
        let mut defs = vec![
            table("orders", &[]),
            sequence("orders_id_seq", Some(("orders", "id"))),
            sequence("invoice_seq", None),
            sequence("other_seq", Some(("customers", "id"))),
        ];
        drop_owned_sequences(&mut defs);
        assert_eq!(names(&defs), ["orders", "invoice_seq", "other_seq"]);
    }

    #[test]
    fn selections_match_kind_name_and_optional_parts() {
        let t = ObjectRef::table(Some("db"), Some("s"), "orders");
        assert!(is_selected(&[], &t));
        assert!(is_selected(
            &[ObjectRef::new(ObjectKind::Table, "orders")],
            &t
        ));
        assert!(!is_selected(
            &[ObjectRef::new(ObjectKind::View, "orders")],
            &t
        ));
        let trigger = t.child(ObjectKind::Trigger, "audit");
        let mut other_parent = ObjectRef::new(ObjectKind::Trigger, "audit");
        other_parent.parent = Some("customers".into());
        assert!(!is_selected(&[other_parent], &trigger));
    }

    fn routine(name: &str, body: &str) -> ObjectDef {
        let mut r = RoutineDef::new(ObjectRef::schema(None, "s").child(ObjectKind::Function, name));
        r.body = Some(body.into());
        ObjectDef::Routine(r)
    }

    fn matview(name: &str, query: &str) -> ObjectDef {
        let mut v = view(name, query);
        if let ObjectDef::View(def) = &mut v {
            def.obj.kind = ObjectKind::MatView;
        }
        v
    }

    fn trigger(name: &str, owner: &ObjectRef) -> ObjectDef {
        ObjectDef::Trigger(TriggerDef {
            obj: owner.child(ObjectKind::Trigger, name),
            timing: TriggerTiming::InsteadOf,
            events: vec![TriggerEvent::Insert],
            update_columns: Vec::new(),
            level: TriggerLevel::Row,
            condition: None,
            function: None,
            body: Some("BEGIN END".into()),
            definition: None,
            enabled: true,
            comment: None,
            extras: Extras::default(),
        })
    }

    #[test]
    fn routines_that_tables_call_come_before_the_tables() {
        let mut t = table("orders", &[]);
        if let ObjectDef::Table(def) = &mut t {
            def.columns[0].default = Some("s.next_order_no()".into());
        }
        let defs = vec![
            routine("next_order_no", "SELECT 1"),
            routine("order_total", "SELECT sum(qty) FROM orders"),
            t,
            sequence("invoice_seq", None),
        ];
        assert_eq!(
            names(&creation_order(defs)),
            ["invoice_seq", "next_order_no", "orders", "order_total"]
        );
    }

    #[test]
    fn views_over_materialized_views_wait_for_them() {
        let mv = matview("mv_totals", "SELECT 1 FROM orders");
        let defs = [
            table("orders", &[]),
            routine("read_totals", "SELECT * FROM v_totals"),
            view("v_plain", "SELECT * FROM orders"),
            mv,
            view("v_totals", "SELECT * FROM mv_totals"),
            trigger(
                "v_totals_ins",
                &ObjectRef::schema(None, "s").child(ObjectKind::View, "v_totals"),
            ),
        ];
        let refs: Vec<&ObjectDef> = defs.iter().collect();
        let late = after_data(&refs);
        assert_eq!(late, [false, true, false, true, true, true]);
        let deferred: Vec<&ObjectDef> = refs
            .iter()
            .zip(&late)
            .filter(|(_, l)| **l)
            .map(|(d, _)| *d)
            .collect();
        let order: Vec<&str> = late_order(&deferred)
            .into_iter()
            .map(|i| deferred[i].name())
            .collect();
        assert_eq!(
            order,
            ["mv_totals", "v_totals", "read_totals", "v_totals_ins"]
        );
    }

    #[test]
    fn mentions_whole_identifiers_only() {
        assert!(mentions("select * from S.Orders o", "orders"));
        assert!(!mentions("select * from orders_old", "orders"));
        assert!(!mentions("select * from my_orders", "orders"));
        assert!(mentions("orders", "orders"));
        assert!(!mentions("ordérs", "orders"));
    }
}
