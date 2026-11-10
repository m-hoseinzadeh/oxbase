//! M8 structure sync DDL cells (`docs/m8-contracts.md` §5.2) for views, materialized views,
//! sequences, functions, procedures and triggers. Tables keep the M3 cells (`ddl.rs`).
//!
//! # Statements
//!
//! - Views: `CREATE [MATERIALIZED] VIEW q [(columns)] AS <query>` (`WITH ... CHECK OPTION`,
//!   materialized `WITH NO DATA` when not populated), `COMMENT ON`. An alter drops and creates.
//! - Routines: built from the model (`CREATE FUNCTION q(args) RETURNS ... LANGUAGE ...
//!   <volatility> [STRICT] [SECURITY DEFINER] [PARALLEL ...] AS $body$...$body$`; a SQL-standard
//!   body, `RETURN ...` / `BEGIN ATOMIC ... END`, inline, but only when the catalog reports one,
//!   `SQL_STANDARD_BODY`), so names inside the body that structure sync moved between schemas
//!   are what runs. An alter is `CREATE OR REPLACE` when the
//!   arguments and the return type stay (PostgreSQL refuses to change them in place), else a
//!   drop and a create. `DROP FUNCTION q(<identity arguments>)`.
//! - Triggers: the server's `pg_get_triggerdef` text (names already moved by the caller), or
//!   built from the model when there is none; `ALTER TABLE ... DISABLE TRIGGER` for a disabled
//!   one; `COMMENT ON TRIGGER`. `DROP TRIGGER t ON q`. An alter drops and creates.
//! - Sequences: `CREATE SEQUENCE q AS type INCREMENT BY ... MINVALUE ... MAXVALUE ... START WITH
//!   ... CACHE ... [NO] CYCLE`; an alter is one `ALTER SEQUENCE` with every option (the current
//!   value is kept).
//!
//! Every statement is transactional. Drops are destructive (the structure sync script clears
//! the flag for objects it drops only to create them again).

use std::fmt::Write as _;

use oxbase_dialect::compare::normalize_ws;
use oxbase_dialect::{DdlContext, DdlGenerator, PgQuoting, Quoting};
use oxbase_types::{
    ArgMode, Error, ObjectDef, ObjectKind, ObjectRef, Result, RoutineArg, RoutineDef, Script,
    ScriptStep, SequenceDef, TriggerDef, TriggerEvent, TriggerLevel, TriggerTiming, Value, ViewDef,
    props,
};

use super::fetch::{PARALLEL, SQL_STANDARD_BODY};

/// The kinds these cells cover.
pub(crate) const KINDS: [ObjectKind; 6] = [
    ObjectKind::View,
    ObjectKind::MatView,
    ObjectKind::Sequence,
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Trigger,
];

/// The structure sync DDL generator of PostgreSQL (module docs).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PgSyncDdl;

fn q(r: &ObjectRef) -> String {
    PgQuoting.qualify(r)
}

fn ident(s: &str) -> String {
    PgQuoting.quote_ident(s)
}

fn lit(s: &str) -> String {
    PgQuoting.quote_str(s)
}

fn step(sql: String, description: String) -> ScriptStep {
    ScriptStep::new(sql).described(description)
}

fn not_covered(kind: ObjectKind) -> Error {
    Error::unsupported(format!(
        "PostgreSQL structure sync has no DDL for a {}",
        kind.display_name().to_lowercase()
    ))
}

fn comment_text(comment: Option<&str>) -> Option<&str> {
    comment.filter(|c| !c.is_empty())
}

impl DdlGenerator for PgSyncDdl {
    fn create(&self, d: &ObjectDef, _ctx: &DdlContext<'_>) -> Result<Script> {
        match d {
            ObjectDef::View(v) => Ok(create_view(v)),
            ObjectDef::Routine(r) => create_routine(r, false),
            ObjectDef::Trigger(t) => create_trigger(t),
            ObjectDef::Sequence(s) => Ok(create_sequence(s)),
            other => Err(not_covered(other.kind())),
        }
    }

    fn alter(&self, old: &ObjectDef, new: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        match (old, new) {
            (ObjectDef::Sequence(a), ObjectDef::Sequence(b)) => Ok(alter_sequence(a, b)),
            (ObjectDef::Routine(a), ObjectDef::Routine(b)) if replaceable(a, b) => {
                let mut script = create_routine(b, true)?;
                let before = comment_text(a.comment.as_deref());
                if before.is_some() && comment_text(b.comment.as_deref()).is_none() {
                    script.push_step(step(
                        format!("COMMENT ON {} IS NULL", routine_target(b)),
                        format!("Remove the comment of {}", b.obj.name),
                    ));
                }
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
                Ok(script)
            }
        }
    }

    fn drop_object(&self, r: &ObjectRef, _ctx: &DdlContext<'_>) -> Result<Script> {
        let (sql, what) = match r.kind {
            ObjectKind::View => (format!("DROP VIEW {}", q(r)), "view"),
            ObjectKind::MatView => (
                format!("DROP MATERIALIZED VIEW {}", q(r)),
                "materialized view",
            ),
            ObjectKind::Sequence => (format!("DROP SEQUENCE {}", q(r)), "sequence"),
            ObjectKind::Function | ObjectKind::Procedure => {
                let keyword = if r.kind == ObjectKind::Procedure {
                    "PROCEDURE"
                } else {
                    "FUNCTION"
                };
                let sql = match &r.signature {
                    Some(signature) => format!("DROP {keyword} {}({signature})", q(r)),
                    None => format!("DROP {keyword} {}", q(r)),
                };
                (
                    sql,
                    if r.kind == ObjectKind::Procedure {
                        "procedure"
                    } else {
                        "function"
                    },
                )
            }
            ObjectKind::Trigger => {
                let table = r
                    .owner()
                    .ok_or_else(|| Error::invalid_input(format!("{r} names no table")))?;
                (
                    format!("DROP TRIGGER {} ON {}", ident(&r.name), q(&table)),
                    "trigger",
                )
            }
            other => return Err(not_covered(other)),
        };
        let mut script = Script::new();
        script.push_step(step(sql, format!("Drop {what} {}", r.name)).destructive());
        Ok(script)
    }
}

// --- views ---------------------------------------------------------------------------------------

fn create_view(v: &ViewDef) -> Script {
    let materialized = v.obj.kind == ObjectKind::MatView;
    let keyword = if materialized {
        "MATERIALIZED VIEW"
    } else {
        "VIEW"
    };
    let mut sql = format!("CREATE {keyword} {}", q(&v.obj));
    if !v.columns.is_empty() {
        let columns: Vec<String> = v.columns.iter().map(|c| ident(c)).collect();
        let _ = write!(sql, " ({})", columns.join(", "));
    }
    sql.push_str(" AS\n");
    sql.push_str(v.definition.trim().trim_end_matches(';').trim_end());
    if !materialized
        && let Some(Value::Text(option)) = v.extras.get(&props::CHECK_OPTION)
        && !option.is_empty()
    {
        let _ = write!(sql, "\n  WITH {} CHECK OPTION", option.to_uppercase());
    }
    if materialized && v.extras.get(&props::POPULATED) == Some(&Value::Bool(false)) {
        sql.push_str("\n  WITH NO DATA");
    }
    let what = if materialized {
        "materialized view"
    } else {
        "view"
    };
    let mut script = Script::new();
    script.push_step(step(sql, format!("Create {what} {}", v.obj.name)));
    if let Some(comment) = comment_text(v.comment.as_deref()) {
        script.push_step(step(
            format!("COMMENT ON {keyword} {} IS {}", q(&v.obj), lit(comment)),
            format!("Comment on {what} {}", v.obj.name),
        ));
    }
    script
}

// --- routines ------------------------------------------------------------------------------------

fn arg_text(a: &RoutineArg) -> String {
    let mode = match a.mode {
        ArgMode::In | ArgMode::Table => "",
        ArgMode::Out => "OUT ",
        ArgMode::InOut => "INOUT ",
        ArgMode::Variadic => "VARIADIC ",
    };
    let mut out = mode.to_owned();
    if let Some(name) = a.name.as_deref().filter(|n| !n.is_empty()) {
        out.push_str(&ident(name));
        out.push(' ');
    }
    out.push_str(a.native_type.trim());
    if let Some(default) = &a.default {
        let _ = write!(out, " DEFAULT {default}");
    }
    out
}

/// `FUNCTION q(identity arguments)` for `COMMENT ON`.
fn routine_target(r: &RoutineDef) -> String {
    let keyword = if r.obj.kind == ObjectKind::Procedure {
        "PROCEDURE"
    } else {
        "FUNCTION"
    };
    format!(
        "{keyword} {}({})",
        q(&r.obj),
        r.obj.signature.as_deref().unwrap_or("")
    )
}

/// A dollar quote tag that does not occur in `body`.
fn dollar_tag(body: &str) -> String {
    let mut tag = "$body$".to_owned();
    let mut n = 0_u32;
    while body.contains(&tag) {
        n += 1;
        tag = format!("$body{n}$");
    }
    tag
}

/// `true` when `body` (of a `LANGUAGE sql` routine) is a SQL-standard body, written inline;
/// any other body is dollar-quoted. Only the catalog can tell: a quoted body is string content
/// that may itself start with `RETURN` or `BEGIN ATOMIC` (written inline it would be read as
/// SQL, statements after a `;` included), so the text is never the test.
///
/// The fetcher reports [`SQL_STANDARD_BODY`]. A definition without it (a backup archive written
/// before it existed) is a standard body only when its `pg_get_functiondef` text ends with the
/// body: the server writes a quoted body between dollar tags it picks so that they do not occur
/// in the body, so such a text always ends with a tag, never with the body.
fn standard_body(r: &RoutineDef, body: &str) -> bool {
    match r.extras.get(&SQL_STANDARD_BODY) {
        Some(Value::Bool(standard)) => *standard,
        Some(_) => false,
        None => {
            let body = body.trim();
            let upper = body.to_ascii_uppercase();
            (upper.starts_with("BEGIN ATOMIC") || upper.starts_with("RETURN "))
                && r.definition
                    .as_deref()
                    .is_some_and(|d| d.trim_end().ends_with(body))
        }
    }
}

fn create_routine(r: &RoutineDef, replace: bool) -> Result<Script> {
    let procedure = r.obj.kind == ObjectKind::Procedure;
    let body = r.body.as_deref().ok_or_else(|| {
        Error::unsupported(format!(
            "the body of {} cannot be read, so it cannot be created",
            r.obj.display_name()
        ))
    })?;
    let keyword = if procedure { "PROCEDURE" } else { "FUNCTION" };
    let args: Vec<String> = r
        .args
        .iter()
        .filter(|a| a.mode != ArgMode::Table)
        .map(arg_text)
        .collect();
    let mut sql = format!(
        "CREATE {}{keyword} {}({})",
        if replace { "OR REPLACE " } else { "" },
        q(&r.obj),
        args.join(", ")
    );
    if !procedure {
        let returns = r.returns.as_deref().ok_or_else(|| {
            Error::invalid_input(format!("function {} has no return type", r.obj.name))
        })?;
        let _ = write!(sql, "\n RETURNS {returns}");
    }
    let language = r.language.as_deref().unwrap_or("sql");
    let _ = write!(sql, "\n LANGUAGE {language}");
    if !procedure {
        if let Some(Value::Text(volatility)) = r.extras.get(&props::VOLATILITY) {
            let _ = write!(sql, " {}", volatility.to_uppercase());
        }
        if r.extras.get(&props::STRICT_INPUT) == Some(&Value::Bool(true)) {
            sql.push_str(" STRICT");
        }
    }
    if let Some(Value::Text(security)) = r.extras.get(&props::SECURITY)
        && security.eq_ignore_ascii_case("definer")
    {
        sql.push_str(" SECURITY DEFINER");
    }
    if !procedure && let Some(Value::Text(parallel)) = r.extras.get(&PARALLEL) {
        let _ = write!(sql, " PARALLEL {}", parallel.to_uppercase());
    }
    if language.eq_ignore_ascii_case("sql") && standard_body(r, body) {
        let _ = write!(sql, "\n{}", body.trim());
    } else {
        let tag = dollar_tag(body);
        let _ = write!(sql, "\nAS {tag}{body}{tag}");
    }
    let what = if procedure { "procedure" } else { "function" };
    let mut script = Script::new();
    script.push_step(step(
        sql,
        format!(
            "{} {what} {}",
            if replace { "Replace" } else { "Create" },
            r.obj.name
        ),
    ));
    if let Some(comment) = comment_text(r.comment.as_deref()) {
        script.push_step(step(
            format!("COMMENT ON {} IS {}", routine_target(r), lit(comment)),
            format!("Comment on {what} {}", r.obj.name),
        ));
    }
    Ok(script)
}

/// `CREATE OR REPLACE` can turn `old` into `new`: same arguments (names, modes, types,
/// defaults) and the same return type.
fn replaceable(old: &RoutineDef, new: &RoutineDef) -> bool {
    let same_text = |a: Option<&str>, b: Option<&str>| match (a, b) {
        (None, None) => true,
        (Some(first), Some(second)) => {
            normalize_ws(first).eq_ignore_ascii_case(&normalize_ws(second))
        }
        _ => false,
    };
    old.obj.kind == new.obj.kind
        && same_text(old.returns.as_deref(), new.returns.as_deref())
        && old.args.len() == new.args.len()
        && old.args.iter().zip(&new.args).all(|(a, b)| {
            a.name == b.name
                && a.mode == b.mode
                && same_text(Some(a.native_type.as_str()), Some(b.native_type.as_str()))
                && same_text(a.default.as_deref(), b.default.as_deref())
        })
}

// --- triggers ------------------------------------------------------------------------------------

fn trigger_sql(t: &TriggerDef, table: &ObjectRef) -> Result<String> {
    if let Some(definition) = t.definition.as_deref().filter(|d| !d.trim().is_empty()) {
        let definition = definition.trim().trim_end_matches(';').trim_end();
        return trigger_on_table(definition, table).map_err(|why| {
            Error::invalid_input(format!(
                "the definition of trigger {} is not usable: {why}",
                t.obj.name
            ))
        });
    }
    let function = t
        .function
        .as_ref()
        .ok_or_else(|| Error::invalid_input(format!("trigger {} names no function", t.obj.name)))?;
    let timing = match t.timing {
        TriggerTiming::Before => "BEFORE",
        TriggerTiming::After => "AFTER",
        TriggerTiming::InsteadOf => "INSTEAD OF",
    };
    let events: Vec<String> = t
        .events
        .iter()
        .map(|e| match e {
            TriggerEvent::Insert => "INSERT".to_owned(),
            TriggerEvent::Update if !t.update_columns.is_empty() => {
                let columns: Vec<String> = t.update_columns.iter().map(|c| ident(c)).collect();
                format!("UPDATE OF {}", columns.join(", "))
            }
            TriggerEvent::Update => "UPDATE".to_owned(),
            TriggerEvent::Delete => "DELETE".to_owned(),
            TriggerEvent::Truncate => "TRUNCATE".to_owned(),
        })
        .collect();
    let level = match t.level {
        TriggerLevel::Row => "ROW",
        TriggerLevel::Statement => "STATEMENT",
    };
    let mut sql = format!(
        "CREATE TRIGGER {} {timing} {} ON {} FOR EACH {level}",
        ident(&t.obj.name),
        events.join(" OR "),
        q(table)
    );
    if let Some(condition) = t.condition.as_deref().filter(|c| !c.trim().is_empty()) {
        let _ = write!(sql, " WHEN ({condition})");
    }
    let _ = write!(sql, " EXECUTE FUNCTION {}()", q(function));
    Ok(sql)
}

/// `definition` (the whole `CREATE TRIGGER` statement, from `pg_get_triggerdef` or a backup
/// archive) when it creates a trigger on `table`: it must start with `CREATE [OR REPLACE]
/// [CONSTRAINT] TRIGGER`, and the relation after `ON` must be `table`. An unqualified relation
/// (the server leaves the schema out when it is on the `search_path`) is qualified with
/// `table`'s schema, so the trigger lands on that table whatever the session's `search_path`.
///
/// The text is used whole, so this is what keeps a definition from doing something else
/// (`DROP SCHEMA ...`, a trigger on another schema's table). That the step is one statement is
/// enforced when it runs (`execute_one`: one prepared statement, parsed by the server).
fn trigger_on_table(definition: &str, table: &ObjectRef) -> std::result::Result<String, String> {
    let mut lx = TriggerLexer::new(definition);
    let word = |lx: &mut TriggerLexer<'_>| match lx.token() {
        Some(TriggerToken::Word(w)) => Some(w),
        _ => None,
    };
    let start = "it does not start with CREATE [OR REPLACE] [CONSTRAINT] TRIGGER";
    if word(&mut lx).as_deref() != Some("create") {
        return Err(start.into());
    }
    let mut next = word(&mut lx);
    if next.as_deref() == Some("or") {
        if word(&mut lx).as_deref() != Some("replace") {
            return Err(start.into());
        }
        next = word(&mut lx);
    }
    if next.as_deref() == Some("constraint") {
        next = word(&mut lx);
    }
    if next.as_deref() != Some("trigger") {
        return Err(start.into());
    }
    // The name, timing and events: words, quoted names and the commas of `UPDATE OF a, b`.
    loop {
        match lx.token() {
            Some(TriggerToken::Word(w)) if w == "on" => break,
            Some(TriggerToken::Word(_) | TriggerToken::Quoted(_) | TriggerToken::Comma) => {}
            _ => return Err("it has no ON <table> after the trigger events".into()),
        }
    }
    let target_start = lx.offset();
    let first = lx.name().ok_or("it names no table after ON")?;
    let (schema, name) = if lx.dot() {
        let second = lx.name().ok_or("it names no table after ON")?;
        (Some(first), second)
    } else {
        (None, first)
    };
    let target_end = lx.offset();
    if name != table.name {
        return Err(format!("it is on table {name}, not {}", table.name));
    }
    match (schema, table.schema.as_deref()) {
        (Some(s), Some(expected)) if s != expected => {
            Err(format!("it is on a table of schema {s}, not {expected}"))
        }
        (None, Some(_)) => Ok(format!(
            "{} {}{}",
            definition[..target_start].trim_end(),
            q(table),
            &definition[target_end..]
        )),
        _ => Ok(definition.to_owned()),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TriggerToken {
    /// An unquoted word, folded to lower case like the server folds names.
    Word(String),
    /// A `"..."` name, `""` undoubled.
    Quoted(String),
    Comma,
    Dot,
    /// Anything else (strings, operators, parentheses): never part of the checked prefix.
    Other,
}

/// Just enough PostgreSQL lexing for [`trigger_on_table`]: whitespace, `--` and nested `/* */`
/// comments, words (letters, digits, `_`, `$` and non-ASCII characters, as the server reads
/// them) and quoted names.
struct TriggerLexer<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> TriggerLexer<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, pos: 0 }
    }

    /// The byte offset after the last token (and before any space that follows it).
    fn offset(&self) -> usize {
        self.pos
    }

    fn skip_space(&mut self) {
        let b = self.text.as_bytes();
        while self.pos < b.len() {
            if b[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            } else if b[self.pos..].starts_with(b"--") {
                self.pos = b[self.pos..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |p| self.pos + p + 1);
            } else if b[self.pos..].starts_with(b"/*") {
                let mut depth = 0usize;
                while self.pos < b.len() {
                    if b[self.pos..].starts_with(b"/*") {
                        depth += 1;
                        self.pos += 2;
                    } else if b[self.pos..].starts_with(b"*/") {
                        depth -= 1;
                        self.pos += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        self.pos += 1;
                    }
                }
            } else {
                break;
            }
        }
    }

    fn token(&mut self) -> Option<TriggerToken> {
        self.skip_space();
        let rest = &self.text[self.pos..];
        let c = rest.chars().next()?;
        let ident_start = |c: char| c.is_ascii_alphabetic() || c == '_' || !c.is_ascii();
        let ident_char =
            |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii();
        if ident_start(c) {
            let len = rest.find(|c: char| !ident_char(c)).unwrap_or(rest.len());
            self.pos += len;
            return Some(TriggerToken::Word(rest[..len].to_ascii_lowercase()));
        }
        if c == '"' {
            let mut name = String::new();
            let mut chars = rest.char_indices().skip(1).peekable();
            while let Some((i, ch)) = chars.next() {
                if ch == '"' {
                    if chars.peek().is_some_and(|&(_, n)| n == '"') {
                        chars.next();
                        name.push('"');
                        continue;
                    }
                    self.pos += i + 1;
                    return Some(TriggerToken::Quoted(name));
                }
                name.push(ch);
            }
            self.pos = self.text.len();
            return Some(TriggerToken::Other);
        }
        self.pos += c.len_utf8();
        Some(match c {
            ',' => TriggerToken::Comma,
            '.' => TriggerToken::Dot,
            _ => TriggerToken::Other,
        })
    }

    /// A word or quoted name, as the server stores it.
    fn name(&mut self) -> Option<String> {
        match self.token() {
            Some(TriggerToken::Word(w) | TriggerToken::Quoted(w)) => Some(w),
            _ => None,
        }
    }

    /// Consumes a `.` when one comes next.
    fn dot(&mut self) -> bool {
        let saved = self.pos;
        if self.token() == Some(TriggerToken::Dot) {
            true
        } else {
            self.pos = saved;
            false
        }
    }
}

fn create_trigger(t: &TriggerDef) -> Result<Script> {
    let table = t
        .obj
        .owner()
        .ok_or_else(|| Error::invalid_input(format!("trigger {} names no table", t.obj.name)))?;
    let mut script = Script::new();
    script.push_step(step(
        trigger_sql(t, &table)?,
        format!("Create trigger {} on {}", t.obj.name, table.name),
    ));
    if !t.enabled {
        script.push_step(step(
            format!(
                "ALTER TABLE {} DISABLE TRIGGER {}",
                q(&table),
                ident(&t.obj.name)
            ),
            format!("Disable trigger {}", t.obj.name),
        ));
    }
    if let Some(comment) = comment_text(t.comment.as_deref()) {
        script.push_step(step(
            format!(
                "COMMENT ON TRIGGER {} ON {} IS {}",
                ident(&t.obj.name),
                q(&table),
                lit(comment)
            ),
            format!("Comment on trigger {}", t.obj.name),
        ));
    }
    Ok(script)
}

// --- sequences -----------------------------------------------------------------------------------

/// The options shared by `CREATE SEQUENCE` and `ALTER SEQUENCE`.
fn sequence_options(s: &SequenceDef) -> String {
    let mut out = String::new();
    if let Some(data_type) = s.data_type.as_deref().filter(|t| !t.trim().is_empty()) {
        let _ = write!(out, " AS {data_type}");
    }
    let _ = write!(out, " INCREMENT BY {}", s.increment);
    match s.min_value {
        Some(min) => {
            let _ = write!(out, " MINVALUE {min}");
        }
        None => out.push_str(" NO MINVALUE"),
    }
    match s.max_value {
        Some(max) => {
            let _ = write!(out, " MAXVALUE {max}");
        }
        None => out.push_str(" NO MAXVALUE"),
    }
    let _ = write!(out, " START WITH {}", s.start);
    if let Some(cache) = s.cache {
        let _ = write!(out, " CACHE {cache}");
    }
    out.push_str(if s.cycle { " CYCLE" } else { " NO CYCLE" });
    out
}

fn create_sequence(s: &SequenceDef) -> Script {
    let mut script = Script::new();
    script.push_step(step(
        format!("CREATE SEQUENCE {}{}", q(&s.obj), sequence_options(s)),
        format!("Create sequence {}", s.obj.name),
    ));
    if let Some(comment) = comment_text(s.comment.as_deref()) {
        script.push_step(step(
            format!("COMMENT ON SEQUENCE {} IS {}", q(&s.obj), lit(comment)),
            format!("Comment on sequence {}", s.obj.name),
        ));
    }
    script
}

fn alter_sequence(old: &SequenceDef, new: &SequenceDef) -> Script {
    let mut script = Script::new();
    let mut before = old.clone();
    before.comment.clone_from(&new.comment);
    before.obj.clone_from(&new.obj);
    if before != *new {
        script.push_step(step(
            format!("ALTER SEQUENCE {}{}", q(&new.obj), sequence_options(new)),
            format!("Change sequence {}", new.obj.name),
        ));
    }
    let (a, b) = (
        comment_text(old.comment.as_deref()),
        comment_text(new.comment.as_deref()),
    );
    if a != b {
        let text = b.map_or_else(|| "NULL".to_owned(), lit);
        script.push_step(step(
            format!("COMMENT ON SEQUENCE {} IS {text}", q(&new.obj)),
            format!("Comment on sequence {}", new.obj.name),
        ));
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{Extras, ServerVersion};

    fn schema() -> ObjectRef {
        ObjectRef::schema(Some("oxbase_test"), "m8_tgt")
    }

    fn sql(script: &Script) -> Vec<&str> {
        script.steps.iter().map(|s| s.sql.as_str()).collect()
    }

    fn ctx_version() -> ServerVersion {
        ServerVersion::new(18, 6, 0)
    }

    #[test]
    fn views_and_materialized_views() {
        let v = ctx_version();
        let ctx = DdlContext::new(&v);
        let mut view = ViewDef {
            obj: schema().child(ObjectKind::View, "active_customers"),
            definition: " SELECT id\n   FROM m8_tgt.customers;".into(),
            columns: Vec::new(),
            comment: Some("it's".into()),
            extras: Extras::new().with(props::CHECK_OPTION, "local"),
        };
        let script = PgSyncDdl
            .create(&ObjectDef::View(view.clone()), &ctx)
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "CREATE VIEW \"m8_tgt\".\"active_customers\" AS\nSELECT id\n   FROM m8_tgt.customers\n  WITH LOCAL CHECK OPTION",
                "COMMENT ON VIEW \"m8_tgt\".\"active_customers\" IS 'it''s'",
            ]
        );
        view.obj.kind = ObjectKind::MatView;
        view.comment = None;
        view.extras = Extras::new().with(props::POPULATED, false);
        let script = PgSyncDdl
            .create(&ObjectDef::View(view.clone()), &ctx)
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "CREATE MATERIALIZED VIEW \"m8_tgt\".\"active_customers\" AS\nSELECT id\n   FROM m8_tgt.customers\n  WITH NO DATA"
            ]
        );
        let script = PgSyncDdl
            .alter(
                &ObjectDef::View(view.clone()),
                &ObjectDef::View(view.clone()),
                &ctx,
            )
            .unwrap();
        assert_eq!(script.steps.len(), 2);
        assert!(!script.is_destructive());
        let drop = PgSyncDdl.drop_object(&view.obj, &ctx).unwrap();
        assert_eq!(
            sql(&drop),
            ["DROP MATERIALIZED VIEW \"m8_tgt\".\"active_customers\""]
        );
        assert!(drop.is_destructive());
    }

    fn function() -> RoutineDef {
        let mut f = RoutineDef::new(
            schema()
                .child(ObjectKind::Function, "order_total")
                .with_signature("p_id integer"),
        );
        f.args.push(RoutineArg {
            name: Some("p_id".into()),
            mode: ArgMode::In,
            native_type: "integer".into(),
            default: None,
        });
        f.returns = Some("numeric".into());
        f.language = Some("plpgsql".into());
        f.body =
            Some("\nBEGIN\n  RETURN (SELECT 1 FROM m8_tgt.orders WHERE id = p_id);\nEND\n".into());
        f.extras.insert(props::VOLATILITY, "stable");
        f.extras.insert(props::STRICT_INPUT, false);
        f.extras.insert(props::SECURITY, "invoker");
        f.extras.insert(PARALLEL, "unsafe");
        f
    }

    #[test]
    fn routines_are_built_from_the_model() {
        let version = ctx_version();
        let ctx = DdlContext::new(&version);
        let mut func = function();
        func.comment = Some("total".into());
        let script = PgSyncDdl
            .create(&ObjectDef::Routine(func.clone()), &ctx)
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "CREATE FUNCTION \"m8_tgt\".\"order_total\"(\"p_id\" integer)\n RETURNS numeric\n LANGUAGE plpgsql STABLE PARALLEL UNSAFE\nAS $body$\nBEGIN\n  RETURN (SELECT 1 FROM m8_tgt.orders WHERE id = p_id);\nEND\n$body$",
                "COMMENT ON FUNCTION \"m8_tgt\".\"order_total\"(p_id integer) IS 'total'",
            ]
        );
        // Same arguments and result: replaced in place.
        let mut changed = func.clone();
        changed.body = Some("BEGIN RETURN 2; END".into());
        changed.comment = None;
        let script = PgSyncDdl
            .alter(
                &ObjectDef::Routine(func.clone()),
                &ObjectDef::Routine(changed.clone()),
                &ctx,
            )
            .unwrap();
        assert!(
            script.steps[0]
                .sql
                .starts_with("CREATE OR REPLACE FUNCTION")
        );
        assert_eq!(
            script.steps[1].sql,
            "COMMENT ON FUNCTION \"m8_tgt\".\"order_total\"(p_id integer) IS NULL"
        );
        // Another result type: dropped and created.
        changed.returns = Some("bigint".into());
        let script = PgSyncDdl
            .alter(
                &ObjectDef::Routine(func.clone()),
                &ObjectDef::Routine(changed),
                &ctx,
            )
            .unwrap();
        assert_eq!(
            script.steps[0].sql,
            "DROP FUNCTION \"m8_tgt\".\"order_total\"(p_id integer)"
        );
        assert!(script.steps[1].sql.starts_with("CREATE FUNCTION"));
        // SQL-standard bodies stay inline.
        let mut standard = function();
        standard.language = Some("sql".into());
        standard.body = Some("RETURN (p_id + 1)".into());
        standard.extras.insert(SQL_STANDARD_BODY, true);
        let script = PgSyncDdl
            .create(&ObjectDef::Routine(standard), &ctx)
            .unwrap();
        assert!(
            script.steps[0]
                .sql
                .ends_with("PARALLEL UNSAFE\nRETURN (p_id + 1)")
        );
        assert_eq!(dollar_tag("x $body$ y"), "$body1$");
        let mut procedure = RoutineDef::new(
            schema()
                .child(ObjectKind::Procedure, "p")
                .with_signature(""),
        );
        procedure.language = Some("plpgsql".into());
        procedure.body = Some("BEGIN NULL; END".into());
        procedure.extras.insert(props::SECURITY, "definer");
        let script = PgSyncDdl
            .create(&ObjectDef::Routine(procedure.clone()), &ctx)
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "CREATE PROCEDURE \"m8_tgt\".\"p\"()\n LANGUAGE plpgsql SECURITY DEFINER\nAS $body$BEGIN NULL; END$body$"
            ]
        );
        assert_eq!(
            sql(&PgSyncDdl.drop_object(&procedure.obj, &ctx).unwrap()),
            ["DROP PROCEDURE \"m8_tgt\".\"p\"()"]
        );
    }

    /// A quoted (`prosrc`) body that starts with `RETURN` stays quoted: written inline, the
    /// statement after its `;` would run on the target (audit lead 12).
    #[test]
    fn only_catalog_standard_bodies_are_written_inline() {
        let version = ctx_version();
        let ctx = DdlContext::new(&version);
        let body = "RETURN 1; CREATE TABLE public.oxbase_probe(x int)";
        let header = "CREATE FUNCTION \"m8_tgt\".\"f\"()\n RETURNS integer\n LANGUAGE sql";
        let routine = |standard: Option<bool>, definition: Option<&str>| {
            let mut f =
                RoutineDef::new(schema().child(ObjectKind::Function, "f").with_signature(""));
            f.returns = Some("integer".into());
            f.language = Some("sql".into());
            f.body = Some(body.into());
            f.definition = definition.map(str::to_owned);
            if let Some(standard) = standard {
                f.extras.insert(SQL_STANDARD_BODY, standard);
            }
            ObjectDef::Routine(f)
        };
        let quoted = format!("{header}\nAS $body${body}$body$");
        let first = |def: &ObjectDef| PgSyncDdl.create(def, &ctx).unwrap().steps[0].sql.clone();
        // The catalog says it is a quoted body.
        assert_eq!(first(&routine(Some(false), None)), quoted);
        // An archive written before the flag: quoted unless the server's definition ends with
        // the body (it ends with the closing dollar tag for a quoted one).
        assert_eq!(first(&routine(None, None)), quoted);
        let server_quoted = format!(
            "CREATE OR REPLACE FUNCTION m8_tgt.f()\n RETURNS integer\n LANGUAGE sql\nAS $function${body}$function$\n"
        );
        assert_eq!(first(&routine(None, Some(&server_quoted))), quoted);
        // A real standard body: inline, from the flag or from the server's definition.
        let standard = "RETURN 1";
        let inline = |def: ObjectDef| {
            let ObjectDef::Routine(mut f) = def else {
                unreachable!()
            };
            f.body = Some(standard.into());
            ObjectDef::Routine(f)
        };
        let expected = format!("{header}\n{standard}");
        assert_eq!(first(&inline(routine(Some(true), None))), expected);
        let server_standard =
            "CREATE OR REPLACE FUNCTION m8_tgt.f()\n RETURNS integer\n LANGUAGE sql\nRETURN 1\n";
        assert_eq!(
            first(&inline(routine(None, Some(server_standard)))),
            expected
        );
        // Only `LANGUAGE sql` has standard bodies.
        let ObjectDef::Routine(mut plpgsql) = inline(routine(Some(true), None)) else {
            unreachable!()
        };
        plpgsql.language = Some("plpgsql".into());
        assert!(
            first(&ObjectDef::Routine(plpgsql))
                .ends_with("LANGUAGE plpgsql\nAS $body$RETURN 1$body$")
        );
    }

    #[test]
    fn triggers_use_the_server_text_or_the_model() {
        let v = ctx_version();
        let ctx = DdlContext::new(&v);
        let table = ObjectRef::table(Some("oxbase_test"), Some("m8_tgt"), "orders");
        let mut t = TriggerDef {
            obj: table.child(ObjectKind::Trigger, "orders_audit"),
            timing: TriggerTiming::After,
            events: vec![TriggerEvent::Insert, TriggerEvent::Update],
            update_columns: vec!["status".into()],
            level: TriggerLevel::Row,
            condition: Some("new.status <> 'x'".into()),
            function: Some(schema().child(ObjectKind::Function, "audit")),
            body: None,
            definition: None,
            enabled: false,
            comment: Some("c".into()),
            extras: Extras::new(),
        };
        let script = PgSyncDdl
            .create(&ObjectDef::Trigger(t.clone()), &ctx)
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "CREATE TRIGGER \"orders_audit\" AFTER INSERT OR UPDATE OF \"status\" ON \"m8_tgt\".\"orders\" FOR EACH ROW WHEN (new.status <> 'x') EXECUTE FUNCTION \"m8_tgt\".\"audit\"()",
                "ALTER TABLE \"m8_tgt\".\"orders\" DISABLE TRIGGER \"orders_audit\"",
                "COMMENT ON TRIGGER \"orders_audit\" ON \"m8_tgt\".\"orders\" IS 'c'",
            ]
        );
        t.definition = Some("CREATE TRIGGER orders_audit AFTER INSERT ON m8_tgt.orders FOR EACH ROW EXECUTE FUNCTION m8_tgt.audit()".into());
        t.enabled = true;
        t.comment = None;
        let script = PgSyncDdl
            .create(&ObjectDef::Trigger(t.clone()), &ctx)
            .unwrap();
        assert_eq!(sql(&script), [t.definition.as_deref().unwrap()]);
        assert_eq!(
            sql(&PgSyncDdl.drop_object(&t.obj, &ctx).unwrap()),
            ["DROP TRIGGER \"orders_audit\" ON \"m8_tgt\".\"orders\""]
        );
    }

    /// The definition is used whole (it comes from the catalog or a backup archive), so it must
    /// be a `CREATE TRIGGER` on the trigger's own table (audit: restore container check).
    #[test]
    fn trigger_definitions_must_create_this_trigger() {
        let v = ctx_version();
        let ctx = DdlContext::new(&v);
        let table = ObjectRef::table(Some("oxbase_test"), Some("m8_tgt"), "orders");
        let def = |definition: &str| {
            ObjectDef::Trigger(TriggerDef {
                obj: table.child(ObjectKind::Trigger, "orders_audit"),
                timing: TriggerTiming::After,
                events: vec![TriggerEvent::Insert],
                update_columns: vec![],
                level: TriggerLevel::Row,
                condition: None,
                function: None,
                body: None,
                definition: Some(definition.into()),
                enabled: true,
                comment: None,
                extras: Extras::new(),
            })
        };
        for bad in [
            "DROP SCHEMA other CASCADE",
            "SELECT 1",
            "CREATE TRIGGER t AFTER INSERT ON other.accounts FOR EACH ROW EXECUTE FUNCTION m8_tgt.f()",
            "CREATE TRIGGER t AFTER INSERT ON m8_tgt.accounts FOR EACH ROW EXECUTE FUNCTION m8_tgt.f()",
            "CREATE OR DROP TRIGGER t AFTER INSERT ON m8_tgt.orders EXECUTE FUNCTION f()",
            "CREATE TRIGGER t AFTER INSERT ('x') ON m8_tgt.orders EXECUTE FUNCTION f()",
            "CREATE TRIGGER t AFTER INSERT ON",
        ] {
            let e = PgSyncDdl.create(&def(bad), &ctx).unwrap_err();
            assert!(matches!(e, Error::InvalidInput { .. }), "{bad}: {e}");
        }
        // Accepted as they are: the server's own forms, comments and quoted names included.
        for good in [
            "CREATE TRIGGER orders_audit AFTER INSERT ON m8_tgt.orders FOR EACH ROW EXECUTE FUNCTION m8_tgt.audit()",
            "CREATE CONSTRAINT TRIGGER \"Orders Audit\" AFTER UPDATE OF status, \"on\" ON \"m8_tgt\".\"orders\" DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION m8_tgt.audit('a')",
            "/* c */ create or replace trigger t before delete on m8_tgt.orders for each statement execute function m8_tgt.audit()",
        ] {
            let script = PgSyncDdl.create(&def(good), &ctx).unwrap();
            assert_eq!(sql(&script), [good]);
        }
        // An unqualified table (left out by the server when on the search_path) is pinned to
        // the trigger's schema.
        let script = PgSyncDdl
            .create(
                &def("CREATE TRIGGER t AFTER INSERT ON orders FOR EACH ROW EXECUTE FUNCTION f()"),
                &ctx,
            )
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "CREATE TRIGGER t AFTER INSERT ON \"m8_tgt\".\"orders\" FOR EACH ROW EXECUTE FUNCTION f()"
            ]
        );
    }

    #[test]
    fn sequences() {
        let v = ctx_version();
        let ctx = DdlContext::new(&v);
        let mut s = SequenceDef::new(schema().child(ObjectKind::Sequence, "invoice_seq"));
        s.data_type = Some("bigint".into());
        s.start = 1000;
        s.min_value = Some(1);
        s.max_value = Some(i128::from(i64::MAX));
        s.cache = Some(1);
        let script = PgSyncDdl
            .create(&ObjectDef::Sequence(s.clone()), &ctx)
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "CREATE SEQUENCE \"m8_tgt\".\"invoice_seq\" AS bigint INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 START WITH 1000 CACHE 1 NO CYCLE"
            ]
        );
        let mut t = s.clone();
        t.increment = 10;
        t.comment = Some("n".into());
        let script = PgSyncDdl
            .alter(
                &ObjectDef::Sequence(s.clone()),
                &ObjectDef::Sequence(t),
                &ctx,
            )
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "ALTER SEQUENCE \"m8_tgt\".\"invoice_seq\" AS bigint INCREMENT BY 10 MINVALUE 1 MAXVALUE 9223372036854775807 START WITH 1000 CACHE 1 NO CYCLE",
                "COMMENT ON SEQUENCE \"m8_tgt\".\"invoice_seq\" IS 'n'",
            ]
        );
        assert!(
            PgSyncDdl
                .alter(
                    &ObjectDef::Sequence(s.clone()),
                    &ObjectDef::Sequence(s.clone()),
                    &ctx
                )
                .unwrap()
                .is_empty()
        );
        assert!(
            PgSyncDdl
                .create(
                    &ObjectDef::Table(oxbase_types::TableDef::new(ObjectRef::table(
                        None,
                        Some("s"),
                        "t"
                    ))),
                    &ctx
                )
                .is_err()
        );
    }

    #[test]
    fn registers_every_structure_sync_role() {
        let mut reg = oxbase_dialect::DialectRegistry::new();
        super::super::register(&mut reg);
        let missing: Vec<String> = reg
            .missing_m8_roles(oxbase_types::Engine::Postgres)
            .into_iter()
            .filter(|m| !m.contains("TypeMapper"))
            .collect();
        assert_eq!(missing, Vec::<String>::new());
    }
}
