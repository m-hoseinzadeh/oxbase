//! M8 structure sync DDL cells (`docs/m8-contracts.md` §5.2) for SQL Server views, sequences,
//! functions, procedures and triggers. Tables keep the M4 cells (`ddl.rs`).
//!
//! # Statements
//!
//! - Views: `CREATE VIEW [s].[v] [(columns)] [WITH SCHEMABINDING] AS <query>` (the query as
//!   `sys.sql_modules` keeps it after the header).
//! - Functions, procedures, triggers: the module text the server keeps (`sys.sql_modules`, a
//!   complete `CREATE` statement, names already moved by the caller), with the header's object
//!   name (and a trigger's `ON` table) written qualified with the definition's own schema: a
//!   module created unqualified would otherwise land in the applying login's default schema. A
//!   disabled trigger is disabled again (`DISABLE TRIGGER`). An encrypted module has no text
//!   and is refused.
//! - Sequences: `CREATE SEQUENCE [s].[q] AS type START WITH ... INCREMENT BY ... MINVALUE ...
//!   MAXVALUE ... [NO] CYCLE [CACHE n | NO CACHE]`. An alter keeps the sequence (and its current
//!   value): `ALTER SEQUENCE` for the options and the extended-property procedures for the
//!   comment. Only a new value type or start value needs a drop and a create; that drop stays
//!   destructive (the sequence starts again at `START WITH`) and the script warns.
//! - Comments: the `MS_Description` extended property (`sys.sp_addextendedproperty`,
//!   `sp_updateextendedproperty`, `sp_dropextendedproperty`).
//!
//! Views, routines and triggers are altered by a drop and a create rather than `CREATE OR
//! ALTER`, so the module text the server keeps is the source's (a structure compare compares
//! that text, with [`MsSqlSyncRules`] ignoring how the header spells the object's name); the
//! script warns that permissions granted on them are lost. Every statement is transactional;
//! drops are destructive.

use std::fmt::Write as _;

use oxbase_dialect::compare::{CompareRules, normalize_ws};
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
    property_step("sp_addextendedproperty", level1, level2, schema, Some(text))
}

/// One `MS_Description` extended-property procedure call (`value` for add and update).
fn property_step(
    procedure: &str,
    level1: (&str, &str),
    level2: Option<(&str, &str)>,
    schema: &str,
    value: Option<&str>,
) -> ScriptStep {
    let value = value
        .map(|v| format!(", @value = {}", lit(v)))
        .unwrap_or_default();
    let mut sql = format!(
        "EXEC sys.{procedure} @name = N'MS_Description'{value}, \
         @level0type = N'SCHEMA', @level0name = {}, @level1type = N'{}', @level1name = {}",
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

/// Rules of the definition comparator: a module header names its object the way it was
/// created (`CREATE PROCEDURE p`, `CREATE PROCEDURE [s].[p]`); the compared object is already
/// known, so only the rest of the text counts.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MsSqlSyncRules;

impl CompareRules for MsSqlSyncRules {
    fn same_expression(&self, old: &str, new: &str) -> bool {
        normalize_ws(&header_names_bare(old)) == normalize_ws(&header_names_bare(new))
    }
}

/// A token of a module header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Word,
    Quoted,
    Dot,
    Other,
}

/// The first `limit` tokens of T-SQL `text` (byte ranges), comments and whitespace skipped.
fn header_tokens(text: &str, limit: usize) -> Vec<(Tok, usize, usize)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() && out.len() < limit {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if bytes[i..].starts_with(b"--") {
            i = bytes[i..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(bytes.len(), |n| i + n);
        } else if bytes[i..].starts_with(b"/*") {
            let mut depth = 0_usize;
            while i < bytes.len() {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if c == b'[' || c == b'"' {
            let close = if c == b'[' { b']' } else { b'"' };
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == close {
                    if bytes.get(i + 1) == Some(&close) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push((Tok::Quoted, start, i));
        } else if c == b'.' {
            out.push((Tok::Dot, i, i + 1));
            i += 1;
        } else if c.is_ascii_alphanumeric() || matches!(c, b'_' | b'@' | b'#' | b'$') || c >= 0x80 {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || matches!(bytes[i], b'_' | b'@' | b'#' | b'$')
                    || bytes[i] >= 0x80)
            {
                i += 1;
            }
            out.push((Tok::Word, start, i));
        } else {
            // One ASCII character (every other byte above is part of a word).
            out.push((Tok::Other, i, i + 1));
            i += 1;
        }
    }
    out
}

/// The byte ranges of the object name in a `CREATE [OR ALTER] {PROCEDURE | PROC | FUNCTION |
/// TRIGGER | VIEW} name` header and, for a trigger, of the table after `ON`, with the value of
/// each name's last part. `None` when the text does not start like that.
fn header_names(text: &str) -> Option<Vec<(usize, usize, String)>> {
    let tokens = header_tokens(text, 64);
    let word = |i: usize, w: &str| {
        tokens
            .get(i)
            .is_some_and(|&(k, a, b)| k == Tok::Word && text[a..b].eq_ignore_ascii_case(w))
    };
    let mut i = 0;
    if !word(i, "CREATE") {
        return None;
    }
    i += 1;
    if word(i, "OR") && word(i + 1, "ALTER") {
        i += 2;
    }
    let trigger = word(i, "TRIGGER");
    if !(trigger
        || ["PROCEDURE", "PROC", "FUNCTION", "VIEW"]
            .iter()
            .any(|w| word(i, w)))
    {
        return None;
    }
    i += 1;
    // A multi-part name: name (. name)*; returns the next token, the range, the last part.
    let name_at = |first: usize| -> Option<(usize, usize, usize, String)> {
        let start = tokens.get(first)?.1;
        let mut i = first;
        loop {
            let &(kind, a, b) = tokens.get(i)?;
            if !matches!(kind, Tok::Word | Tok::Quoted) {
                return None;
            }
            if tokens.get(i + 1).is_some_and(|t| t.0 == Tok::Dot) {
                i += 2;
            } else {
                return Some((i + 1, start, b, part_value(&text[a..b])));
            }
        }
    };
    let (next, start, end, last) = name_at(i)?;
    let mut out = vec![(start, end, last)];
    if trigger {
        if !word(next, "ON") {
            return None;
        }
        let (_, start, end, last) = name_at(next + 1)?;
        out.push((start, end, last));
    }
    Some(out)
}

/// The value of one name part (`[a]]b]` is `a]b`, `"x"` is `x`).
fn part_value(part: &str) -> String {
    let close = match part.as_bytes().first() {
        Some(b'[') => "]",
        Some(b'"') => "\"",
        _ => return part.to_owned(),
    };
    let body = part
        .get(1..part.len().saturating_sub(1))
        .unwrap_or_default();
    body.replace(&close.repeat(2), close)
}

/// `text` with the header names replaced by their last part, lower-cased (for comparing).
fn header_names_bare(text: &str) -> String {
    let Some(names) = header_names(text) else {
        return text.to_owned();
    };
    let mut out = text.to_owned();
    for (start, end, last) in names.into_iter().rev() {
        out.replace_range(start..end, &last.to_lowercase());
    }
    out
}

/// `text` with the header's object name written as `name` and a trigger's table as `table`.
fn qualify_header(text: &str, name: &str, table: Option<&str>) -> String {
    let Some(names) = header_names(text) else {
        return text.to_owned();
    };
    let mut out = text.to_owned();
    for (i, (start, end, _)) in names.into_iter().enumerate().rev() {
        let with = if i == 0 { Some(name) } else { table };
        if let Some(with) = with {
            out.replace_range(start..end, with);
        }
    }
    out
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
                if same_type(a.data_type.as_deref(), b.data_type.as_deref())
                    && a.start == b.start =>
            {
                Ok(alter_sequence(a, b))
            }
            (ObjectDef::Sequence(_), ObjectDef::Sequence(b)) => {
                // The drop stays destructive: the sequence starts again.
                let mut script = self.drop_object(&old.object_ref(), ctx)?;
                script.append(self.create(new, ctx)?);
                script.warn(format!(
                    "sequence {} is dropped and created again (its value type or start value \
                     changes): it starts again at {}, and SQL Server refuses the drop while a \
                     default uses it",
                    b.obj.name, b.start
                ));
                Ok(script)
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
                script.warn(PERMISSIONS_LOST);
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

/// The warning of an alter that drops and creates a module.
const PERMISSIONS_LOST: &str = "views, routines and triggers that are dropped and created again \
                                lose the permissions granted on them; grant them again after \
                                the apply";

fn same_type(a: Option<&str>, b: Option<&str>) -> bool {
    let norm = |t: Option<&str>| t.map(|t| t.trim().to_lowercase()).unwrap_or_default();
    norm(a) == norm(b)
}

fn create_routine(r: &RoutineDef) -> Result<Script> {
    let sql = qualify_header(
        &module_text(r.definition.as_deref(), &r.obj)?,
        &q(&r.obj),
        None,
    );
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
        qualify_header(
            &module_text(t.definition.as_deref(), &t.obj)?,
            &q(&t.obj),
            Some(q(&table).as_str()),
        ),
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

/// The options and the comment changed in place (the current value stays).
fn alter_sequence(old: &SequenceDef, new: &SequenceDef) -> Script {
    let mut script = Script::new();
    if sequence_options(old) != sequence_options(new) {
        script.push_step(step(
            format!("ALTER SEQUENCE {}{}", q(&new.obj), sequence_options(new)),
            format!("Change sequence {}", new.obj.name),
        ));
    }
    let level1 = ("SEQUENCE", new.obj.name.as_str());
    let schema = schema_name(&new.obj);
    let comment = match (
        comment_text(old.comment.as_deref()),
        comment_text(new.comment.as_deref()),
    ) {
        (None, Some(text)) => Some(comment_step(level1, None, schema, text)),
        (Some(before), Some(text)) if before != text => Some(
            property_step(
                "sp_updateextendedproperty",
                level1,
                None,
                schema,
                Some(text),
            )
            .described(format!("Change the comment of {}", new.obj.name)),
        ),
        (Some(_), None) => Some(
            property_step("sp_dropextendedproperty", level1, None, schema, None)
                .described(format!("Remove the comment of {}", new.obj.name)),
        ),
        _ => None,
    };
    if let Some(comment) = comment {
        script.push_step(comment);
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
    fn unqualified_module_headers_are_written_with_the_schema() {
        let version = ServerVersion::new(17, 0, 0);
        let ctx = DdlContext::new(&version);
        let mut p = RoutineDef::new(schema().child(ObjectKind::Procedure, "touch_order"));
        p.definition = Some(
            "-- keeps totals\n/* note */ create   proc touch_order @p_id int AS UPDATE orders SET total = total"
                .into(),
        );
        let script = MsSqlSyncDdl
            .create(&ObjectDef::Routine(p.clone()), &ctx)
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "-- keeps totals\n/* note */ create   proc [m8_tgt].[touch_order] @p_id int AS UPDATE orders SET total = total"
            ]
        );
        let table = ObjectRef::table(Some("oxbase_test"), Some("m8_tgt"), "orders");
        let mut trigger = TriggerDef {
            obj: table.child(ObjectKind::Trigger, "orders_audit"),
            timing: TriggerTiming::After,
            events: vec![TriggerEvent::Insert],
            update_columns: Vec::new(),
            level: TriggerLevel::Statement,
            condition: None,
            function: None,
            body: None,
            definition: Some(
                "CREATE TRIGGER orders_audit ON dbo.[orders] AFTER INSERT AS SET NOCOUNT ON".into(),
            ),
            enabled: true,
            comment: None,
            extras: Extras::new(),
        };
        let script = MsSqlSyncDdl
            .create(&ObjectDef::Trigger(trigger.clone()), &ctx)
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "CREATE TRIGGER [m8_tgt].[orders_audit] ON [m8_tgt].[orders] AFTER INSERT AS SET NOCOUNT ON"
            ]
        );
        // Text that does not start with a module header is left alone.
        assert_eq!(qualify_header("SELECT 1", "[s].[x]", None), "SELECT 1");

        // The compare ignores how the header spells the name, not the rest.
        let rules = MsSqlSyncRules;
        assert!(rules.same_expression(
            "CREATE PROCEDURE touch_order AS SELECT 1",
            "CREATE  PROCEDURE [m8_tgt].[Touch_Order]\nAS SELECT 1"
        ));
        assert!(!rules.same_expression(
            "CREATE PROCEDURE touch_order AS SELECT 1",
            "CREATE PROCEDURE [m8_tgt].[touch_order] AS SELECT 2"
        ));
        assert!(rules.same_expression(
            "CREATE TRIGGER t ON orders AFTER INSERT AS SELECT 1",
            "CREATE TRIGGER [s].[t] ON [s].[orders] AFTER INSERT AS SELECT 1"
        ));
        assert!(!rules.same_expression(
            "CREATE TRIGGER t ON orders AFTER INSERT AS SELECT 1",
            "CREATE TRIGGER [s].[t] ON [s].[items] AFTER INSERT AS SELECT 1"
        ));
        assert_eq!(part_value("[a]]b]"), "a]b");
        assert_eq!(part_value("\"x\"\"y\""), "x\"y");
        trigger.definition = None;
        assert!(
            MsSqlSyncDdl
                .create(&ObjectDef::Trigger(trigger), &ctx)
                .is_err()
        );
    }

    #[test]
    fn sequence_comments_change_in_place() {
        let version = ServerVersion::new(17, 0, 0);
        let ctx = DdlContext::new(&version);
        let mut seq = SequenceDef::new(schema().child(ObjectKind::Sequence, "ticket_seq"));
        seq.data_type = Some("int".into());
        seq.start = 1;
        let mut commented = seq.clone();
        commented.comment = Some("Tickets".into());
        let alter = |a: &SequenceDef, b: &SequenceDef| {
            MsSqlSyncDdl
                .alter(
                    &ObjectDef::Sequence(a.clone()),
                    &ObjectDef::Sequence(b.clone()),
                    &ctx,
                )
                .unwrap()
        };
        let script = alter(&seq, &commented);
        assert_eq!(
            sql(&script),
            [
                "EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Tickets', @level0type = N'SCHEMA', @level0name = N'm8_tgt', @level1type = N'SEQUENCE', @level1name = N'ticket_seq'"
            ]
        );
        assert!(!script.is_destructive());
        let mut changed = commented.clone();
        changed.comment = Some("Ticket numbers".into());
        assert_eq!(
            sql(&alter(&commented, &changed)),
            [
                "EXEC sys.sp_updateextendedproperty @name = N'MS_Description', @value = N'Ticket numbers', @level0type = N'SCHEMA', @level0name = N'm8_tgt', @level1type = N'SEQUENCE', @level1name = N'ticket_seq'"
            ]
        );
        assert_eq!(
            sql(&alter(&commented, &seq)),
            [
                "EXEC sys.sp_dropextendedproperty @name = N'MS_Description', @level0type = N'SCHEMA', @level0name = N'm8_tgt', @level1type = N'SEQUENCE', @level1name = N'ticket_seq'"
            ]
        );
        // The same type spelled another way changes nothing.
        let mut spelled = seq.clone();
        spelled.data_type = Some(" INT".into());
        assert!(alter(&seq, &spelled).is_empty());
        // A new value type: drop and create, destructive, with a warning.
        let mut wider = seq.clone();
        wider.data_type = Some("bigint".into());
        let script = alter(&seq, &wider);
        assert_eq!(script.steps[0].sql, "DROP SEQUENCE [m8_tgt].[ticket_seq]");
        assert!(script.is_destructive());
        assert_eq!(script.warnings.len(), 1);
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
