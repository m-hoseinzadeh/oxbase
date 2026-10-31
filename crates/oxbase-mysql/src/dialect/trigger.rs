//! Triggers (`information_schema.TRIGGERS`). MySQL and MariaDB triggers are row-level, on
//! tables only, with an inline body.

use async_trait::async_trait;
use oxbase_dialect::Fetcher;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    Extras, ObjectDef, ObjectKind, ObjectRef, ObjectSummary, Result, TriggerDef, TriggerEvent,
    TriggerLevel, TriggerTiming, props,
};

use super::ddl_text::{create_prefix, ident};
use super::rows::{query, text};
use super::{
    ACTION_ORDER, by_name, database_of, expect_kind, is_system_database, not_found, scoped,
};

/// Lists the triggers of a table and loads one as a [`TriggerDef`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TriggerFetcher;

/// `BEFORE` / `AFTER`.
fn timing(text: &str) -> TriggerTiming {
    if text.trim().eq_ignore_ascii_case("AFTER") {
        TriggerTiming::After
    } else {
        TriggerTiming::Before
    }
}

/// `INSERT`, or a list such as `INSERT,UPDATE` / `INSERT OR UPDATE`, sorted.
fn events(text: &str) -> Vec<TriggerEvent> {
    let upper = text.to_ascii_uppercase();
    let mut out: Vec<TriggerEvent> = upper
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter_map(|word| match word {
            "INSERT" => Some(TriggerEvent::Insert),
            "UPDATE" => Some(TriggerEvent::Update),
            "DELETE" => Some(TriggerEvent::Delete),
            _ => None,
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

#[async_trait]
impl Fetcher for TriggerFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        if parent.kind != ObjectKind::Table {
            // No triggers on views in MySQL / MariaDB.
            return Ok(Vec::new());
        }
        let db = database_of(c, parent, &cancel).await?;
        let parent = scoped(parent, &db);
        let rows = query(
            c,
            "SELECT TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION \
             FROM information_schema.TRIGGERS \
             WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? ORDER BY TRIGGER_NAME",
            &[text(&db), text(&parent.name)],
            &cancel,
        )
        .await?;
        let system = is_system_database(&db);
        Ok(by_name(
            rows.iter()
                .map(|row| {
                    let mut s =
                        ObjectSummary::new(parent.child(ObjectKind::Trigger, row.string(0)));
                    s.system = system;
                    s
                })
                .collect(),
        ))
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        expect_kind(obj, ObjectKind::Trigger)?;
        let db = database_of(c, obj, &cancel).await?;
        // Trigger names are unique per database; the table comes back with the row.
        let rows = query(
            c,
            "SELECT EVENT_OBJECT_TABLE, EVENT_MANIPULATION, ACTION_TIMING, ACTION_ORIENTATION, \
             ACTION_STATEMENT, ACTION_ORDER, DEFINER FROM information_schema.TRIGGERS \
             WHERE TRIGGER_SCHEMA = ? AND TRIGGER_NAME = ?",
            &[text(&db), text(&obj.name)],
            &cancel,
        )
        .await?;
        let row = rows
            .first()
            .filter(|row| {
                obj.parent
                    .as_deref()
                    .is_none_or(|table| table.is_empty() || row.string(0) == table)
            })
            .ok_or_else(|| not_found(obj))?;
        let table = row.string(0);
        let owner = ObjectRef::table(Some(&db), None, table.as_str());
        let event_text = row.string(1);
        let timing_text = row.string(2);
        let level = if row.string(3).eq_ignore_ascii_case("STATEMENT") {
            TriggerLevel::Statement
        } else {
            TriggerLevel::Row
        };
        let body = row.text(4);
        let definer = row.non_empty(6);
        let definition = body.as_deref().map(|body| {
            format!(
                "{}TRIGGER {} {} {} ON {} FOR EACH ROW {body}",
                create_prefix(definer.as_deref()),
                ident(&obj.name),
                timing_text.to_ascii_uppercase(),
                event_text.to_ascii_uppercase(),
                ident(&table),
            )
        });
        let mut def = TriggerDef {
            obj: owner.child(ObjectKind::Trigger, obj.name.as_str()),
            timing: timing(&timing_text),
            events: events(&event_text),
            update_columns: Vec::new(),
            level,
            condition: None,
            function: None,
            body,
            definition,
            enabled: true,
            comment: None,
            extras: Extras::new(),
        };
        if let Some(order) = row.int(5).and_then(|o| i64::try_from(o).ok()) {
            def.extras.insert(ACTION_ORDER, order);
        }
        if let Some(d) = definer {
            def.extras.insert(props::DEFINER, d);
        }
        Ok(ObjectDef::Trigger(def))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timing_and_events() {
        assert_eq!(timing("BEFORE"), TriggerTiming::Before);
        assert_eq!(timing("after"), TriggerTiming::After);
        assert_eq!(events("INSERT"), [TriggerEvent::Insert]);
        assert_eq!(
            events("UPDATE OR INSERT"),
            [TriggerEvent::Insert, TriggerEvent::Update]
        );
        assert_eq!(
            events("delete,insert"),
            [TriggerEvent::Insert, TriggerEvent::Delete]
        );
    }
}
