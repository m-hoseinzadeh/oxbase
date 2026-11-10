//! M8 structure sync DDL cells (`docs/m8-contracts.md` §5.2) for SQLite views and triggers.
//! Tables keep the M3 cells (`ddl.rs`, rebuilds included).
//!
//! - Views: `CREATE VIEW "db"."v" [(columns)] AS <query>`.
//! - Triggers: the stored `CREATE TRIGGER` text (`sqlite_schema.sql`), which is a complete
//!   statement (SQLite has no schemas to move).
//! - Drops: `DROP VIEW` / `DROP TRIGGER`; an alter drops and creates. Every statement is
//!   transactional; drops are destructive.

use std::fmt::Write as _;

use oxbase_dialect::sql::leading_keywords;
use oxbase_dialect::{AnsiQuoting, DdlContext, DdlGenerator, Quoting};
use oxbase_types::{
    Error, ObjectDef, ObjectKind, ObjectRef, Result, Script, ScriptStep, TriggerDef, ViewDef,
};

/// The kinds these cells cover.
pub(crate) const KINDS: [ObjectKind; 2] = [ObjectKind::View, ObjectKind::Trigger];

/// The structure sync DDL generator of SQLite (module docs).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SqliteSyncDdl;

fn not_covered(kind: ObjectKind) -> Error {
    Error::unsupported(format!(
        "SQLite structure sync has no DDL for a {}",
        kind.display_name().to_lowercase()
    ))
}

fn create_view(v: &ViewDef) -> Script {
    let mut sql = format!("CREATE VIEW {}", AnsiQuoting.qualify(&v.obj));
    if !v.columns.is_empty() {
        let columns: Vec<String> = v
            .columns
            .iter()
            .map(|c| AnsiQuoting.quote_ident(c))
            .collect();
        let _ = write!(sql, " ({})", columns.join(", "));
    }
    let _ = write!(
        sql,
        " AS {}",
        v.definition.trim().trim_end_matches(';').trim_end()
    );
    let mut script = Script::new();
    script.push_step(ScriptStep::new(sql).described(format!("Create view {}", v.obj.name)));
    script
}

fn create_trigger(t: &TriggerDef) -> Result<Script> {
    let sql = t
        .definition
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .ok_or_else(|| Error::invalid_input(format!("trigger {} has no definition", t.obj.name)))?;
    // The definition is used whole (from the schema or a backup archive): it must create a
    // trigger. SQLite block comments do not nest. That it is one statement is enforced when
    // it runs (`execute_one`: SQLite's own parser refuses a second statement).
    let words = leading_keywords(sql, false, 3);
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let creates_trigger = matches!(
        words.as_slice(),
        ["CREATE", "TRIGGER", ..] | ["CREATE", "TEMP" | "TEMPORARY", "TRIGGER"]
    );
    if !creates_trigger {
        return Err(Error::invalid_input(format!(
            "the definition of trigger {} does not start with CREATE TRIGGER",
            t.obj.name
        )));
    }
    let mut script = Script::new();
    script.push_step(
        ScriptStep::new(sql.trim_end_matches(';').trim_end())
            .described(format!("Create trigger {}", t.obj.name)),
    );
    Ok(script)
}

impl DdlGenerator for SqliteSyncDdl {
    fn create(&self, d: &ObjectDef, _ctx: &DdlContext<'_>) -> Result<Script> {
        match d {
            ObjectDef::View(v) if v.obj.kind == ObjectKind::View => Ok(create_view(v)),
            ObjectDef::Trigger(t) => create_trigger(t),
            other => Err(not_covered(other.kind())),
        }
    }

    fn alter(&self, old: &ObjectDef, new: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        if old.kind() != new.kind() {
            return Err(Error::invalid_input(format!(
                "cannot alter a {} into a {}",
                old.kind().display_name(),
                new.kind().display_name()
            )));
        }
        let mut script = self.drop_object(&old.object_ref(), ctx)?;
        for s in &mut script.steps {
            // Created again right after.
            s.destructive = false;
        }
        script.append(self.create(new, ctx)?);
        Ok(script)
    }

    fn drop_object(&self, r: &ObjectRef, _ctx: &DdlContext<'_>) -> Result<Script> {
        let keyword = match r.kind {
            ObjectKind::View => "VIEW",
            ObjectKind::Trigger => "TRIGGER",
            other => return Err(not_covered(other)),
        };
        let mut script = Script::new();
        script.push_step(
            ScriptStep::new(format!("DROP {keyword} {}", AnsiQuoting.qualify(r)))
                .described(format!(
                    "Drop {} {}",
                    r.kind.display_name().to_lowercase(),
                    r.name
                ))
                .destructive(),
        );
        Ok(script)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{Extras, ServerVersion, TriggerEvent, TriggerLevel, TriggerTiming};

    #[test]
    fn views_and_triggers() {
        let version = ServerVersion::new(3, 50, 0);
        let ctx = DdlContext::new(&version);
        let db = ObjectRef::database("main");
        let view = ViewDef {
            obj: db.child(ObjectKind::View, "active_customers"),
            definition: "SELECT id FROM customers WHERE active".into(),
            columns: vec!["id".into()],
            comment: None,
            extras: Extras::new(),
        };
        let script = SqliteSyncDdl
            .create(&ObjectDef::View(view.clone()), &ctx)
            .unwrap();
        assert_eq!(
            script.steps[0].sql,
            "CREATE VIEW \"main\".\"active_customers\" (\"id\") AS SELECT id FROM customers WHERE active"
        );
        let script = SqliteSyncDdl
            .alter(
                &ObjectDef::View(view.clone()),
                &ObjectDef::View(view.clone()),
                &ctx,
            )
            .unwrap();
        assert_eq!(
            script.steps[0].sql,
            "DROP VIEW \"main\".\"active_customers\""
        );
        assert!(!script.is_destructive());

        let table = ObjectRef::table(Some("main"), None, "orders");
        let trigger = TriggerDef {
            obj: table.child(ObjectKind::Trigger, "orders_audit"),
            timing: TriggerTiming::After,
            events: vec![TriggerEvent::Insert],
            update_columns: Vec::new(),
            level: TriggerLevel::Row,
            condition: None,
            function: None,
            body: Some("INSERT INTO order_audit (order_id) VALUES (NEW.id);".into()),
            definition: Some(
                "CREATE TRIGGER orders_audit AFTER INSERT ON orders BEGIN INSERT INTO order_audit (order_id) VALUES (NEW.id); END"
                    .into(),
            ),
            enabled: true,
            comment: None,
            extras: Extras::new(),
        };
        let script = SqliteSyncDdl
            .create(&ObjectDef::Trigger(trigger.clone()), &ctx)
            .unwrap();
        assert_eq!(script.steps[0].sql, trigger.definition.as_deref().unwrap());
        let drop = SqliteSyncDdl.drop_object(&trigger.obj, &ctx).unwrap();
        assert_eq!(drop.steps[0].sql, "DROP TRIGGER \"main\".\"orders_audit\"");
        assert!(drop.is_destructive());
        assert!(SqliteSyncDdl.drop_object(&table, &ctx).is_err());

        // The definition is used whole, so it must be a CREATE TRIGGER (a backup archive can
        // carry any text). SQLite block comments do not nest.
        for bad in [
            "DROP TABLE orders",
            "/* /* */ DROP TABLE orders; */ CREATE TRIGGER t AFTER INSERT ON orders BEGIN SELECT 1; END",
            "CREATE TABLE x (a)",
        ] {
            let mut t = trigger.clone();
            t.definition = Some(bad.into());
            let e = SqliteSyncDdl
                .create(&ObjectDef::Trigger(t), &ctx)
                .unwrap_err();
            assert!(matches!(e, Error::InvalidInput { .. }), "{bad}: {e}");
        }
        let mut t = trigger.clone();
        t.definition = Some(
            "-- audit\nCREATE TEMP TRIGGER t AFTER INSERT ON orders BEGIN SELECT 1; END".into(),
        );
        assert!(SqliteSyncDdl.create(&ObjectDef::Trigger(t), &ctx).is_ok());
    }

    #[test]
    fn registers_every_structure_sync_role() {
        let mut reg = oxbase_dialect::DialectRegistry::new();
        super::super::register(&mut reg);
        let missing: Vec<String> = reg
            .missing_m8_roles(oxbase_types::Engine::Sqlite)
            .into_iter()
            .filter(|m| !m.contains("TypeMapper"))
            .collect();
        assert_eq!(missing, Vec::<String>::new());
    }
}
