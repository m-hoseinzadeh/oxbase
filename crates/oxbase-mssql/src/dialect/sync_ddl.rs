//! M8 structure sync DDL cells (`docs/m8-contracts.md` §5.2) for SQL Server views, sequences,
//! functions, procedures and triggers. Tables keep the M4 cells (`ddl.rs`).
//!
//! # Statements
//!
//! - Views: `CREATE VIEW [s].[v] [(columns)] [WITH SCHEMABINDING] AS <query>` (the query as
//!   `sys.sql_modules` keeps it after the header).
//! - Functions, procedures, triggers: the module text the server keeps (`sys.sql_modules`, a
//!   complete `CREATE` statement, names already moved by the caller); a disabled trigger is
//!   disabled again (`DISABLE TRIGGER`). An encrypted module has no text and is refused.
//! - Sequences: `CREATE SEQUENCE [s].[q] AS type START WITH ... INCREMENT BY ... MINVALUE ...
//!   MAXVALUE ... [NO] CYCLE [CACHE n | NO CACHE]`; an alter is `ALTER SEQUENCE` when the value
//!   type, the start and the comment stay, else a drop and a create.
//! - Comments: the `MS_Description` extended property (`sys.sp_addextendedproperty`).
//!
//! Views, routines and triggers are altered by a drop and a create rather than `CREATE OR
//! ALTER`, so the module text the server keeps is exactly the source's (a structure compare
//! compares that text). Every statement is transactional; drops are destructive.

use std::fmt::Write as _;

use oxbase_dialect::{BracketQuoting, DdlContext, DdlGenerator, Quoting};
use oxbase_types::{
    Error, ObjectDef, ObjectKind, ObjectRef, Result, RoutineDef, Script, ScriptStep, SequenceDef,
    TriggerDef, Value, ViewDef,
};

use super::fetch::DEFAULT_CACHE;
use crate::extras as x;

/// The kinds these cells cover.
pub(crate) const KINDS: [ObjectKind; 5] = [
    ObjectKind::View,
    ObjectKind::Sequence,
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Trigger,
];

/// The structure sync DDL generator of SQL Server (module docs).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MsSqlSyncDdl;

fn q(r: &ObjectRef) -> String {
    BracketQuoting.qualify(r)
}

fn ident(s: &str) -> String {
    BracketQuoting.quote_ident(s)
}

fn lit(s: &str) -> String {
    BracketQuoting.quote_str(s)
}

fn step(sql: String, description: String) -> ScriptStep {
    ScriptStep::new(sql).described(description)
}

fn not_covered(kind: ObjectKind) -> Error {
    Error::unsupported(format!(
        "SQL Server structure sync has no DDL for a {}",
        kind.display_name().to_lowercase()
    ))
}

fn comment_text(comment: Option<&str>) -> Option<&str> {
    comment.filter(|c| !c.is_empty())
}

fn schema_name(r: &ObjectRef) -> &str {
    r.schema.as_deref().unwrap_or("dbo")
}

/// `sp_addextendedproperty` of the `MS_Description` of an object of the schema (`level1`) or
/// of a child of one (`level2`: a trigger of a table).
fn comment_step(
    level1: (&str, &str),
    level2: Option<(&str, &str)>,
    schema: &str,
    text: &str,
) -> ScriptStep {
    let mut sql = format!(
        "EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = {}, \
         @level0type = N'SCHEMA', @level0name = {}, @level1type = N'{}', @level1name = {}",
        lit(text),
        lit(schema),
        level1.0,
        lit(level1.1)
    );
    if let Some((kind, name)) = level2 {
        let _ = write!(
            sql,
            ", @level2type = N'{kind}', @level2name = {}",
            lit(name)
        );
    }
    let what = level2.map_or(level1.1, |(_, name)| name);
    step(sql, format!("Comment on {what}"))
}

/// The module text of a routine or trigger, ready to run.
fn module_text(definition: Option<&str>, r: &ObjectRef) -> Result<String> {
    definition
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(|d| d.trim_end_matches(';').trim_end().to_owned())
        .ok_or_else(|| {
            Error::unsupported(format!(
                "{} {} has no readable definition (encrypted?), so it cannot be created",
                r.kind.display_name(),
                r.name
            ))
        })
}

impl DdlGenerator for MsSqlSyncDdl {
    fn create(&self, d: &ObjectDef, _ctx: &DdlContext<'_>) -> Result<Script> {
        match d {
            ObjectDef::View(v) => Ok(create_view(v)),
            ObjectDef::Routine(r) => create_routine(r),
            ObjectDef::Trigger(t) => create_trigger(t),
            ObjectDef::Sequence(s) => Ok(create_sequence(s)),
            other => Err(not_covered(other.kind())),
        }
    }

    fn alter(&self, old: &ObjectDef, new: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        match (old, new) {
            (ObjectDef::Sequence(a), ObjectDef::Sequence(b))
                if a.data_type == b.data_type && a.start == b.start && a.comment == b.comment =>
            {
                Ok(alter_sequence(a, b))
            }
            _ if old.kind() != new.kind() => Err(Error::invalid_input(format!(
                "cannot alter a {} into a {}",
                old.kind().display_name(),
                new.kind().display_name()
            ))),
            _ => {
                let mut script = self.drop_object(&old.object_ref(), ctx)?;
                for s in &mut script.steps {
                    // Created again right after.
                    s.destructive = false;
                }
                script.append(self.create(new, ctx)?);
                Ok(script)
            }
        }
    }

    fn drop_object(&self, r: &ObjectRef, _ctx: &DdlContext<'_>) -> Result<Script> {
        let keyword = match r.kind {
            ObjectKind::View => "VIEW",
            ObjectKind::Function => "FUNCTION",
            ObjectKind::Procedure => "PROCEDURE",
            ObjectKind::Trigger => "TRIGGER",
            ObjectKind::Sequence => "SEQUENCE",
            other => return Err(not_covered(other)),
        };
        let mut script = Script::new();
        script.push_step(
            step(
                format!("DROP {keyword} {}", q(r)),
                format!("Drop {} {}", r.kind.display_name().to_lowercase(), r.name),
            )
            .destructive(),
        );
        Ok(script)
    }
}

fn create_view(v: &ViewDef) -> Script {
    let mut sql = format!("CREATE VIEW {}", q(&v.obj));
    if !v.columns.is_empty() {
        let columns: Vec<String> = v.columns.iter().map(|c| ident(c)).collect();
        let _ = write!(sql, " ({})", columns.join(", "));
    }
    if v.extras.get(&x::SCHEMA_BINDING) == Some(&Value::Bool(true)) {
        sql.push_str(" WITH SCHEMABINDING");
    }
    let _ = write!(
        sql,
        " AS\n{}",
        v.definition.trim().trim_end_matches(';').trim_end()
    );
    let mut script = Script::new();
    script.push_step(step(sql, format!("Create view {}", v.obj.name)));
    if let Some(comment) = comment_text(v.comment.as_deref()) {
        script.push_step(comment_step(
            ("VIEW", &v.obj.name),
            None,
            schema_name(&v.obj),
            comment,
        ));
    }
    script
}

fn create_routine(r: &RoutineDef) -> Result<Script> {
    let sql = module_text(r.definition.as_deref(), &r.obj)?;
    let (kind, what) = if r.obj.kind == ObjectKind::Procedure {
        ("PROCEDURE", "procedure")
    } else {
        ("FUNCTION", "function")
    };
    let mut script = Script::new();
    script.push_step(step(sql, format!("Create {what} {}", r.obj.name)));
    if let Some(comment) = comment_text(r.comment.as_deref()) {
        script.push_step(comment_step(
            (kind, &r.obj.name),
            None,
            schema_name(&r.obj),
            comment,
        ));
    }
    Ok(script)
}

fn create_trigger(t: &TriggerDef) -> Result<Script> {
    let table = t
        .obj
        .owner()
        .ok_or_else(|| Error::invalid_input(format!("trigger {} names no table", t.obj.name)))?;
    let mut script = Script::new();
    script.push_step(step(
        module_text(t.definition.as_deref(), &t.obj)?,
        format!("Create trigger {} on {}", t.obj.name, table.name),
    ));
    if !t.enabled {
        script.push_step(step(
            format!("DISABLE TRIGGER {} ON {}", q(&t.obj), q(&table)),
            format!("Disable trigger {}", t.obj.name),
        ));
    }
    if let Some(comment) = comment_text(t.comment.as_deref()) {
        let level1 = if table.kind == ObjectKind::View {
            "VIEW"
        } else {
            "TABLE"
        };
        script.push_step(comment_step(
            (level1, &table.name),
            Some(("TRIGGER", &t.obj.name)),
            schema_name(&t.obj),
            comment,
        ));
    }
    Ok(script)
}

/// The options after `START WITH` shared by create and alter.
fn sequence_options(s: &SequenceDef) -> String {
    let mut out = format!(" INCREMENT BY {}", s.increment);
    if let Some(min) = s.min_value {
        let _ = write!(out, " MINVALUE {min}");
    }
    if let Some(max) = s.max_value {
        let _ = write!(out, " MAXVALUE {max}");
    }
    out.push_str(if s.cycle { " CYCLE" } else { " NO CYCLE" });
    // `CACHE n`, `NO CACHE`, or nothing for the server's default cache.
    match s.cache {
        Some(n) => {
            let _ = write!(out, " CACHE {n}");
        }
        None if s.extras.get(&DEFAULT_CACHE) == Some(&Value::Bool(true)) => {}
        None => out.push_str(" NO CACHE"),
    }
    out
}

fn create_sequence(s: &SequenceDef) -> Script {
    let mut sql = format!("CREATE SEQUENCE {}", q(&s.obj));
    if let Some(data_type) = s.data_type.as_deref().filter(|t| !t.trim().is_empty()) {
        let _ = write!(sql, " AS {data_type}");
    }
    let _ = write!(sql, " START WITH {}{}", s.start, sequence_options(s));
    let mut script = Script::new();
    script.push_step(step(sql, format!("Create sequence {}", s.obj.name)));
    if let Some(comment) = comment_text(s.comment.as_deref()) {
        script.push_step(comment_step(
            ("SEQUENCE", &s.obj.name),
            None,
            schema_name(&s.obj),
            comment,
        ));
    }
    script
}

fn alter_sequence(old: &SequenceDef, new: &SequenceDef) -> Script {
    let mut before = old.clone();
    before.obj.clone_from(&new.obj);
    let mut script = Script::new();
    if before != *new {
        script.push_step(step(
            format!("ALTER SEQUENCE {}{}", q(&new.obj), sequence_options(new)),
            format!("Change sequence {}", new.obj.name),
        ));
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{Extras, ServerVersion, TriggerEvent, TriggerLevel, TriggerTiming};
    use pretty_assertions::assert_eq;

    fn schema() -> ObjectRef {
        ObjectRef::schema(Some("oxbase_test"), "m8_tgt")
    }

    fn sql(script: &Script) -> Vec<&str> {
        script.steps.iter().map(|s| s.sql.as_str()).collect()
    }

    #[test]
    fn views_routines_and_triggers() {
        let version = ServerVersion::new(17, 0, 0);
        let ctx = DdlContext::new(&version);
        let view = ViewDef {
            obj: schema().child(ObjectKind::View, "active_customers"),
            definition: "SELECT [id] FROM [m8_tgt].[customers];".into(),
            columns: vec!["id".into()],
            comment: Some("it's".into()),
            extras: Extras::new().with(x::SCHEMA_BINDING, true),
        };
        let script = MsSqlSyncDdl
            .create(&ObjectDef::View(view.clone()), &ctx)
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "CREATE VIEW [m8_tgt].[active_customers] ([id]) WITH SCHEMABINDING AS\nSELECT [id] FROM [m8_tgt].[customers]",
                "EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'it''s', @level0type = N'SCHEMA', @level0name = N'm8_tgt', @level1type = N'VIEW', @level1name = N'active_customers'",
            ]
        );
        let script = MsSqlSyncDdl
            .alter(
                &ObjectDef::View(view.clone()),
                &ObjectDef::View(view.clone()),
                &ctx,
            )
            .unwrap();
        assert_eq!(script.steps[0].sql, "DROP VIEW [m8_tgt].[active_customers]");
        assert!(!script.is_destructive() && script.is_atomic());

        let mut f = RoutineDef::new(schema().child(ObjectKind::Function, "order_total"));
        f.definition = Some("\nCREATE FUNCTION [m8_tgt].[order_total](@id int) RETURNS int AS BEGIN RETURN @id END\n".into());
        let script = MsSqlSyncDdl
            .create(&ObjectDef::Routine(f.clone()), &ctx)
            .unwrap();
        assert_eq!(
            sql(&script),
            ["CREATE FUNCTION [m8_tgt].[order_total](@id int) RETURNS int AS BEGIN RETURN @id END"]
        );
        f.definition = None;
        assert!(MsSqlSyncDdl.create(&ObjectDef::Routine(f), &ctx).is_err());

        let table = ObjectRef::table(Some("oxbase_test"), Some("m8_tgt"), "orders");
        let trigger = TriggerDef {
            obj: table.child(ObjectKind::Trigger, "orders_audit"),
            timing: TriggerTiming::After,
            events: vec![TriggerEvent::Insert],
            update_columns: Vec::new(),
            level: TriggerLevel::Statement,
            condition: None,
            function: None,
            body: None,
            definition: Some("CREATE TRIGGER [m8_tgt].[orders_audit] ON [m8_tgt].[orders] AFTER INSERT AS SET NOCOUNT ON".into()),
            enabled: false,
            comment: Some("c".into()),
            extras: Extras::new(),
        };
        let script = MsSqlSyncDdl
            .create(&ObjectDef::Trigger(trigger.clone()), &ctx)
            .unwrap();
        assert_eq!(
            sql(&script)[1..],
            [
                "DISABLE TRIGGER [m8_tgt].[orders_audit] ON [m8_tgt].[orders]",
                "EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'c', @level0type = N'SCHEMA', @level0name = N'm8_tgt', @level1type = N'TABLE', @level1name = N'orders', @level2type = N'TRIGGER', @level2name = N'orders_audit'",
            ]
        );
        assert_eq!(
            sql(&MsSqlSyncDdl.drop_object(&trigger.obj, &ctx).unwrap()),
            ["DROP TRIGGER [m8_tgt].[orders_audit]"]
        );
    }

    #[test]
    fn sequences() {
        let version = ServerVersion::new(17, 0, 0);
        let ctx = DdlContext::new(&version);
        let mut seq = SequenceDef::new(schema().child(ObjectKind::Sequence, "invoice_seq"));
        seq.data_type = Some("bigint".into());
        seq.start = 1000;
        seq.min_value = Some(1);
        seq.max_value = Some(9_999_999);
        seq.extras.insert(DEFAULT_CACHE, true);
        let script = MsSqlSyncDdl
            .create(&ObjectDef::Sequence(seq.clone()), &ctx)
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "CREATE SEQUENCE [m8_tgt].[invoice_seq] AS bigint START WITH 1000 INCREMENT BY 1 MINVALUE 1 MAXVALUE 9999999 NO CYCLE"
            ]
        );
        let mut changed = seq.clone();
        changed.increment = 10;
        changed.cache = Some(50);
        let script = MsSqlSyncDdl
            .alter(
                &ObjectDef::Sequence(seq.clone()),
                &ObjectDef::Sequence(changed.clone()),
                &ctx,
            )
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "ALTER SEQUENCE [m8_tgt].[invoice_seq] INCREMENT BY 10 MINVALUE 1 MAXVALUE 9999999 NO CYCLE CACHE 50"
            ]
        );
        changed.start = 5;
        let script = MsSqlSyncDdl
            .alter(
                &ObjectDef::Sequence(seq.clone()),
                &ObjectDef::Sequence(changed),
                &ctx,
            )
            .unwrap();
        assert_eq!(script.steps[0].sql, "DROP SEQUENCE [m8_tgt].[invoice_seq]");
        seq.extras = Extras::new();
        assert!(
            MsSqlSyncDdl
                .create(&ObjectDef::Sequence(seq), &ctx)
                .unwrap()
                .steps[0]
                .sql
                .ends_with(" NO CYCLE NO CACHE")
        );
    }

    #[test]
    fn registers_every_structure_sync_role() {
        let mut reg = oxbase_dialect::DialectRegistry::new();
        super::super::register(&mut reg);
        let missing: Vec<String> = reg
            .missing_m8_roles(oxbase_types::Engine::MsSql)
            .into_iter()
            .filter(|m| !m.contains("TypeMapper"))
            .collect();
        assert_eq!(missing, Vec::<String>::new());
    }
}
