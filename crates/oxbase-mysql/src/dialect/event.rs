//! Scheduled events (`information_schema.EVENTS`). Not part of the M2 tree layout; listed and
//! loaded (as a [`GenericDef`]) for callers that ask for them.

use async_trait::async_trait;
use oxbase_dialect::Fetcher;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    Extras, GenericDef, ObjectDef, ObjectKind, ObjectRef, ObjectSummary, Result, props,
};

use super::ddl_text::{create_prefix, ident, literal};
use super::rows::{Row, query, text};
use super::{STATUS, by_name, database_of, expect_kind, is_system_database, not_found, scoped};

/// Lists the events of a database and loads one.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct EventFetcher;

#[async_trait]
impl Fetcher for EventFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let db = database_of(c, parent, &cancel).await?;
        let parent = scoped(parent, &db);
        let rows = query(
            c,
            "SELECT EVENT_NAME, STATUS, EVENT_COMMENT FROM information_schema.EVENTS \
             WHERE EVENT_SCHEMA = ? ORDER BY EVENT_NAME",
            &[text(&db)],
            &cancel,
        )
        .await?;
        let system = is_system_database(&db);
        Ok(by_name(
            rows.iter()
                .map(|row| {
                    let mut s = ObjectSummary::new(parent.child(ObjectKind::Event, row.string(0)));
                    s.system = system;
                    s.comment = row.non_empty(2);
                    if let Some(status) = row.non_empty(1) {
                        s.extras.insert(STATUS, status);
                    }
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
        expect_kind(obj, ObjectKind::Event)?;
        let db = database_of(c, obj, &cancel).await?;
        let rows = query(
            c,
            "SELECT DEFINER, EVENT_DEFINITION, EVENT_TYPE, CAST(EXECUTE_AT AS CHAR), \
             INTERVAL_VALUE, INTERVAL_FIELD, CAST(STARTS AS CHAR), CAST(ENDS AS CHAR), STATUS, \
             ON_COMPLETION, EVENT_COMMENT FROM information_schema.EVENTS \
             WHERE EVENT_SCHEMA = ? AND EVENT_NAME = ?",
            &[text(&db), text(&obj.name)],
            &cancel,
        )
        .await?;
        let row = rows.first().ok_or_else(|| not_found(obj))?;
        let mut def = GenericDef {
            obj: scoped(obj, &db),
            definition: row.text(1).map(|body| event_text(&obj.name, row, &body)),
            comment: row.non_empty(10),
            extras: Extras::new(),
        };
        if let Some(d) = row.non_empty(0) {
            def.extras.insert(props::DEFINER, d);
        }
        if let Some(status) = row.non_empty(8) {
            def.extras.insert(STATUS, status);
        }
        Ok(ObjectDef::Generic(def))
    }
}

/// `CREATE EVENT` assembled from an `EVENTS` row (column order of the `load` query).
fn event_text(name: &str, row: &Row, body: &str) -> String {
    let mut out = format!(
        "{}EVENT {} ON SCHEDULE ",
        create_prefix(row.non_empty(0).as_deref()),
        ident(name)
    );
    if row.string(2).eq_ignore_ascii_case("ONE TIME") {
        out.push_str(&format!("AT {}", literal(&row.string(3))));
    } else {
        let value = row.string(4);
        let value = if !value.is_empty() && value.chars().all(|c| c.is_ascii_digit()) {
            value
        } else {
            literal(&value)
        };
        out.push_str(&format!("EVERY {value} {}", row.string(5)));
        if let Some(starts) = row.non_empty(6) {
            out.push_str(&format!(" STARTS {}", literal(&starts)));
        }
        if let Some(ends) = row.non_empty(7) {
            out.push_str(&format!(" ENDS {}", literal(&ends)));
        }
    }
    out.push_str(" ON COMPLETION ");
    out.push_str(&row.string(9).to_ascii_uppercase());
    let status = row.string(8).to_ascii_uppercase();
    out.push_str(match status.as_str() {
        "ENABLED" => " ENABLE",
        "DISABLED" => " DISABLE",
        _ => " DISABLE ON SLAVE",
    });
    if let Some(comment) = row.non_empty(10) {
        out.push_str(&format!(" COMMENT {}", literal(&comment)));
    }
    out.push_str(" DO ");
    out.push_str(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::rows::Cell;

    fn t(s: &str) -> Cell {
        Cell::Text(s.into())
    }

    #[test]
    fn assembles_create_event() {
        let row = Row(vec![
            t("oxbase@%"),
            t("DELETE FROM t WHERE 1 = 0"),
            t("RECURRING"),
            Cell::Null,
            t("1"),
            t("DAY"),
            t("2026-10-31 00:00:00"),
            Cell::Null,
            t("DISABLED"),
            t("PRESERVE"),
            t("M2 event"),
        ]);
        assert_eq!(
            event_text("m2_evt", &row, "DELETE FROM t WHERE 1 = 0"),
            "CREATE DEFINER=`oxbase`@`%` EVENT `m2_evt` ON SCHEDULE EVERY 1 DAY \
             STARTS '2026-10-31 00:00:00' ON COMPLETION PRESERVE DISABLE COMMENT 'M2 event' \
             DO DELETE FROM t WHERE 1 = 0"
        );
    }
}
