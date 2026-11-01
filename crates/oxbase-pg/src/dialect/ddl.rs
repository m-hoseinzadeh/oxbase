//! The PostgreSQL [`DdlGenerator`] cells (M3): tables, columns, indexes, foreign keys and
//! checks, with comments.
//!
//! Everything PostgreSQL does is native and transactional, so every step is a transactional
//! main step and a script is all-or-nothing. Identifiers go through [`PgQuoting`]; comments and
//! literal defaults arrive already quoted by the designer or are quoted here with
//! [`Quoting::quote_str`]; expressions (defaults, checks, generated columns, index keys and
//! predicates, `PARTITION BY`) are inserted as they are after
//! [`check_expression`](oxbase_dialect::sql::check_expression) and a check that they are one
//! expression (no `,` outside parentheses), and native type names after a stricter check (no
//! clauses hidden in a type). String literals with a backslash (in comments, defaults and other
//! expressions) are written as escape strings (`E'...'`), so a script means the same whatever
//! the session's `standard_conforming_strings` is.
//!
//! Indexes keep what the fetcher reports beyond keys and predicate when they are dropped and
//! created again: per-key operator classes and collations, storage parameters (`WITH`), the
//! tablespace and the deferrability of a `UNIQUE` constraint. An index whose operator classes
//! have parameters cannot be written and is `Unsupported`. Changed flags are found by comparing
//! with PostgreSQL's defaults filled in (`compare::normalize_def`).
//!
//! # Version notes
//!
//! Identity columns need 10, `INCLUDE` 11, stored generated columns 12, `DROP EXPRESSION` 13,
//! `NULLS NOT DISTINCT` 15, `SET EXPRESSION` 17, virtual generated columns and `NOT ENFORCED`
//! 18. A server version of `0.0.0` (unknown) counts as current.
//!
//! # Table `alter`
//!
//! In order: schema move and renames (table, columns, indexes, constraints, primary key name);
//! drops of foreign keys, checks and indexes that go or change, and of the primary key when it
//! changes (a foreign key of the table to itself that references a dropped key is dropped and
//! added again; foreign keys of other tables get a warning); column drops, changes and adds; the primary key; new or changed indexes, checks and
//! foreign keys (`VALIDATE CONSTRAINT` when only `NOT VALID` went away, `ALTER CONSTRAINT` when
//! only deferrability changed); table storage options; comments. PostgreSQL cannot reorder
//! columns: kept columns must stay in order and new columns go at the end, else the alter is
//! `Unsupported`.
//!
//! A column change runs: `DROP EXPRESSION`, `DROP IDENTITY`, `DROP DEFAULT` (also before a type
//! change, so the old default does not block it), `TYPE ... [COLLATE ...] USING col::type`,
//! `SET / DROP NOT NULL`, `SET DEFAULT`, identity (`ADD GENERATED` / `SET GENERATED`,
//! `SET START WITH`, `SET INCREMENT BY`), `SET EXPRESSION`. A type change that may fail or lose
//! data (anything but a widening such as `integer` → `bigint` or `varchar(10)` → `text`) is
//! destructive.

#![allow(
    clippy::many_single_char_names,
    reason = "short names for versions, scripts and columns read best in SQL builders"
)]

use std::collections::HashSet;
use std::fmt::Write as _;

use oxbase_dialect::compare::{CompareRules, diff_checks, diff_foreign_keys, diff_indexes};
use oxbase_dialect::sql::check_expression;
use oxbase_dialect::{DdlContext, DdlGenerator, PgQuoting, Quoting};
use oxbase_types::{
    CheckDef, ColumnDef, DataType, Error, Extras, FkAction, ForeignKeyDef, IdentityDef, IndexDef,
    ObjectDef, ObjectKind, ObjectRef, PrimaryKeyDef, PropId, PropertyDiff, Result, Script,
    ScriptStep, ServerVersion, TableDef, Value, props,
};

use super::compare::{
    PgRules, canonical_type, column_type_key, is_serial_type, normalize_check, normalize_fk,
    normalize_index,
};
use super::fetch::{
    EXCLUSION, INDEX_OPTIONS, KEY_COLLATIONS, MATCH_TYPE, NO_INHERIT, NOT_ENFORCED, NOT_VALID,
    NULLS_FIRST, NULLS_NOT_DISTINCT, OPCLASS_OPTIONS, OPCLASSES,
};
use super::sql::at_least;

/// The PostgreSQL DDL generator. One stateless value serves every designer kind.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PgDdl;

impl DdlGenerator for PgDdl {
    fn create(&self, d: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        let v = ctx.version;
        match d {
            ObjectDef::Table(t) => create_table(v, t),
            ObjectDef::Column { table, column } => {
                let mut s = Script::new();
                s.push_step(
                    ScriptStep::new(format!(
                        "ALTER TABLE {} ADD COLUMN {}",
                        table_sql(table),
                        column_sql(v, column)?
                    ))
                    .described(format!("Add column {}", column.name)),
                );
                if let Some(text) = comment_text(column.comment.as_deref()) {
                    s.push_step(column_comment_step(table, &column.name, Some(text)));
                }
                Ok(s)
            }
            ObjectDef::Index { table, index } => Ok(single(index_step(v, table, index)?)),
            ObjectDef::ForeignKey { table, foreign_key } => {
                Ok(single(add_fk_step(v, table, foreign_key)?))
            }
            ObjectDef::Check { table, check } => Ok(single(add_check_step(v, table, check)?)),
            other => Err(not_designer_kind(other.kind())),
        }
    }

    fn alter(&self, old: &ObjectDef, new: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        let v = ctx.version;
        let mut s = Script::new();
        match (old, new) {
            (ObjectDef::Table(before), ObjectDef::Table(after)) => {
                return alter_table(v, before, after, ctx);
            }
            (
                ObjectDef::Column { column: before, .. },
                ObjectDef::Column {
                    table,
                    column: after,
                },
            ) => {
                let mut cur = before.clone();
                if before.name != after.name {
                    s.push_step(rename_column_step(table, &before.name, &after.name));
                    cur.name.clone_from(&after.name);
                }
                column_changes(v, table, &cur, after, &mut s)?;
                if comment_text(cur.comment.as_deref()) != comment_text(after.comment.as_deref()) {
                    s.push_step(column_comment_step(
                        table,
                        &after.name,
                        comment_text(after.comment.as_deref()),
                    ));
                }
            }
            (
                ObjectDef::Index { index: before, .. },
                ObjectDef::Index {
                    table,
                    index: after,
                },
            ) => {
                let mut cur = before.clone();
                if before.name != after.name && !before.name.is_empty() && !after.name.is_empty() {
                    s.push_step(rename_index_step(table, &before.name, &after.name));
                    cur.name.clone_from(&after.name);
                }
                if index_changed(table, &cur, after) {
                    s.push_step(drop_index_step(table, &cur)?);
                    s.push_step(index_step(v, table, after)?);
                }
            }
            (
                ObjectDef::ForeignKey {
                    foreign_key: before,
                    ..
                },
                ObjectDef::ForeignKey {
                    table,
                    foreign_key: after,
                },
            ) => {
                let mut cur = before.clone();
                if let (Some(from), Some(to)) = (&before.name, &after.name)
                    && from != to
                {
                    s.push_step(rename_constraint_step(table, from, to));
                    cur.name = Some(to.clone());
                }
                let mut adds = Vec::new();
                change_fk(v, table, &cur, after, false, &mut s, &mut adds)?;
                s.extend(adds);
            }
            (
                ObjectDef::Check { check: before, .. },
                ObjectDef::Check {
                    table,
                    check: after,
                },
            ) => {
                let mut cur = before.clone();
                if let (Some(from), Some(to)) = (&before.name, &after.name)
                    && from != to
                {
                    s.push_step(rename_constraint_step(table, from, to));
                    cur.name = Some(to.clone());
                }
                let mut adds = Vec::new();
                change_check(v, table, &cur, after, &mut s, &mut adds)?;
                s.extend(adds);
            }
            _ if old.kind() != new.kind() => {
                return Err(Error::invalid_input(format!(
                    "cannot alter a {} into a {}",
                    old.kind().display_name(),
                    new.kind().display_name()
                )));
            }
            _ => return Err(not_designer_kind(new.kind())),
        }
        Ok(s)
    }

    fn drop_object(&self, r: &ObjectRef, ctx: &DdlContext<'_>) -> Result<Script> {
        let step = match r.kind {
            ObjectKind::Table => ScriptStep::new(format!("DROP TABLE {}", table_sql(r)))
                .described(format!("Drop table {}", r.name))
                .destructive(),
            ObjectKind::Column => {
                let table = owner(r)?;
                ScriptStep::new(format!(
                    "ALTER TABLE {} DROP COLUMN {}",
                    table_sql(&table),
                    ident(&r.name)
                ))
                .described(format!("Drop column {}", r.name))
                .destructive()
            }
            // A UNIQUE / EXCLUDE constraint's index is dropped as a constraint; that needs the
            // table in `ctx.table` (without it the index is dropped with `DROP INDEX`, which the
            // server refuses for a constraint's index).
            ObjectKind::Index => {
                let table = owner(r)?;
                let known = ctx
                    .table
                    .and_then(|t| t.indexes.iter().find(|i| i.name == r.name));
                match known {
                    Some(index) => drop_index_step(&table, index)?,
                    None => drop_index_step(
                        &table,
                        &IndexDef {
                            name: r.name.clone(),
                            columns: Vec::new(),
                            unique: false,
                            is_constraint: false,
                            predicate: None,
                            extras: Extras::new(),
                        },
                    )?,
                }
            }
            ObjectKind::ForeignKey | ObjectKind::Check => {
                let table = owner(r)?;
                if r.name.is_empty() {
                    return Err(Error::invalid_input(
                        "an unnamed constraint cannot be dropped by name",
                    ));
                }
                drop_constraint_step(&table, r.kind, &r.name)
            }
            other => return Err(not_designer_kind(other)),
        };
        Ok(single(step))
    }
}

/// A script of one step.
fn single(step: ScriptStep) -> Script {
    let mut s = Script::new();
    s.push_step(step);
    s
}

fn not_designer_kind(kind: ObjectKind) -> Error {
    Error::unsupported(format!(
        "PostgreSQL DDL generation covers tables, columns, indexes, foreign keys and checks, \
         not {}",
        kind.plural_display_name()
    ))
}

fn owner(r: &ObjectRef) -> Result<ObjectRef> {
    r.owner()
        .filter(|o| !o.name.is_empty())
        .ok_or_else(|| Error::invalid_input(format!("{r} has no owning table")))
}

// --- names and literals ---------------------------------------------------------------------

fn ident(name: &str) -> String {
    PgQuoting.quote_ident(name)
}

/// A string literal. Text with a backslash is written as an escape string (`E'a\\b'`), which
/// reads the same whatever `standard_conforming_strings` is; other text through
/// [`Quoting::quote_str`].
fn literal(text: &str) -> String {
    if text.contains('\\') {
        format!("E'{}'", text.replace('\\', "\\\\").replace('\'', "''"))
    } else {
        PgQuoting.quote_str(text)
    }
}

/// `"schema"."table"` (a PostgreSQL session never names its database).
fn table_sql(table: &ObjectRef) -> String {
    PgQuoting.qualify(&ObjectRef::table(
        None,
        table.schema.as_deref(),
        table.name.as_str(),
    ))
}

/// `"schema"."index"`: indexes live in their table's schema.
fn index_sql(table: &ObjectRef, name: &str) -> String {
    PgQuoting.qualify(&ObjectRef::new(ObjectKind::Index, name).in_schema(table.schema.as_deref()))
}

fn column_list(columns: &[String]) -> String {
    columns
        .iter()
        .map(|c| ident(c))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `text` after [`check_expression`] and [`one_expression`], or `InvalidInput` naming `what`.
fn checked(what: &str, text: &str) -> Result<String> {
    let named = |e: Error| match e {
        Error::InvalidInput { message } => Error::invalid_input(format!("{what}: {message}")),
        other => other,
    };
    check_expression(&PgQuoting, text).map_err(named)?;
    one_expression(text.trim()).map_err(named)
}

/// `true` for characters that continue an identifier or keyword.
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// End (exclusive) of the quoted text starting at `start` (a `'` or `"`), with doubled quotes
/// inside and, for escape strings, backslash escapes.
fn quoted_end(chars: &[char], start: usize, backslashes: bool) -> Result<usize> {
    let q = chars[start];
    let mut i = start + 1;
    while i < chars.len() {
        let c = chars[i];
        if backslashes && c == '\\' {
            i += 2;
        } else if c == q {
            if chars.get(i + 1) == Some(&q) {
                i += 2;
            } else {
                return Ok(i + 1);
            }
        } else {
            i += 1;
        }
    }
    Err(Error::invalid_input("unterminated quoted text"))
}

/// End (exclusive) of the dollar-quoted string starting at `start`, if one starts there.
fn dollar_end(chars: &[char], start: usize) -> Option<usize> {
    let mut j = start + 1;
    while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
        j += 1;
    }
    if chars.get(j) != Some(&'$') || chars.get(start + 1).is_some_and(char::is_ascii_digit) {
        return None;
    }
    let tag = &chars[start..=j];
    (j + 1..chars.len())
        .find(|&k| chars[k..].starts_with(tag))
        .map(|k| k + tag.len())
}

/// Checks that an expression (already through [`check_expression`]) is one expression, and
/// makes its string literals independent of `standard_conforming_strings`:
///
/// - a `,` outside parentheses and brackets is refused (a default such as `0, "x" text` would
///   add a column to `CREATE TABLE`);
/// - a plain string literal with a backslash is rewritten as an escape string with the
///   backslash doubled: with `standard_conforming_strings = off` the server would read the
///   backslash as an escape, so a literal the lexical check sees as one string (a backslash
///   followed by a doubled quote and `; DROP ...`) would end early there.
///   Escape strings, dollar quotes, bit strings and identifiers read the same either way;
///   `N'...'` with a backslash is refused.
fn one_expression(text: &str) -> Result<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    let mut depth = 0usize;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let prev = i.checked_sub(1).map(|k| chars[k]);
        let prefix_start = |len: usize| i <= len || !is_ident_char(chars[i - len - 1]);
        match c {
            '\'' => {
                // `E'...'`, `B'...'`, `X'...'`, `N'...'` and `U&'...'` prefixes.
                let prefix = match prev {
                    Some('e' | 'E') if prefix_start(1) => Some('e'),
                    Some('b' | 'B' | 'x' | 'X') if prefix_start(1) => Some('b'),
                    Some('n' | 'N') if prefix_start(1) => Some('n'),
                    Some('&') if i >= 2 && matches!(chars[i - 2], 'u' | 'U') && prefix_start(2) => {
                        Some('u')
                    }
                    _ => None,
                };
                let end = quoted_end(&chars, i, prefix == Some('e'))?;
                let body: String = chars[i + 1..end - 1].iter().collect();
                match prefix {
                    None if body.contains('\\') => {
                        if prev.is_some_and(is_ident_char) {
                            out.push(' ');
                        }
                        out.push_str("E'");
                        out.push_str(&body.replace('\\', "\\\\"));
                        out.push('\'');
                    }
                    Some('n') if body.contains('\\') => {
                        return Err(Error::invalid_input(
                            "write a national string literal with a backslash as E'...'",
                        ));
                    }
                    _ => out.extend(&chars[i..end]),
                }
                i = end;
            }
            '"' => {
                let end = quoted_end(&chars, i, false)?;
                out.extend(&chars[i..end]);
                i = end;
            }
            '$' if prev.is_none_or(|p| !is_ident_char(p)) => {
                let end = dollar_end(&chars, i).unwrap_or(i + 1);
                out.extend(&chars[i..end]);
                i = end;
            }
            '(' | '[' => {
                depth += 1;
                out.push(c);
                i += 1;
            }
            ')' | ']' => {
                depth = depth.saturating_sub(1);
                out.push(c);
                i += 1;
            }
            ',' if depth == 0 => {
                return Err(Error::invalid_input(
                    "a ',' outside parentheses (only one expression is allowed)",
                ));
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Words that start a column clause and so must not appear in a type name.
const CLAUSE_WORDS: [&str; 13] = [
    "not",
    "null",
    "default",
    "primary",
    "references",
    "collate",
    "generated",
    "check",
    "constraint",
    "unique",
    "using",
    "as",
    "identity",
];

/// A native type name as written by the user or the catalog, checked so that it cannot carry
/// another clause: one expression ([`check_expression`]), no `,` or operator outside
/// parentheses, no string literal, and no clause keyword (`NOT NULL`, `DEFAULT`, ...).
pub(crate) fn checked_type(column: &str, native: &str) -> Result<String> {
    let what = format!("type of column {column}");
    let text = checked(&what, native)?;
    let bad = |why: &str| Err(Error::invalid_input(format!("{what}: {why} in '{text}'")));
    let mut depth = 0usize;
    let mut quoted = false;
    let mut word = String::new();
    for c in text.chars().chain([' ']) {
        if c == '"' {
            quoted = !quoted;
            continue;
        }
        if quoted {
            continue;
        }
        if c.is_alphanumeric() || c == '_' || c == '$' {
            word.push(c);
            continue;
        }
        if depth == 0 && CLAUSE_WORDS.contains(&word.to_lowercase().as_str()) {
            return bad("a clause keyword");
        }
        word.clear();
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '\'' => return bad("a string literal"),
            _ if depth > 0 || matches!(c, ' ' | '.' | '[' | ']') => {}
            ',' => return bad("a ','"),
            _ => return bad("an unexpected character"),
        }
    }
    Ok(text)
}

/// `true` when `v` is at least `major` (unknown versions count as current).
fn need(v: &ServerVersion, major: u32, what: &str) -> Result<()> {
    if at_least(v, major) {
        Ok(())
    } else {
        Err(Error::unsupported(format!(
            "{what} needs PostgreSQL {major} or newer (the server is {v})"
        )))
    }
}

fn flag(extras: &Extras, prop: &PropId) -> bool {
    extras.get(prop).and_then(Value::as_bool).unwrap_or(false)
}

fn text_extra<'e>(extras: &'e Extras, prop: &PropId) -> Option<&'e str> {
    extras.get(prop).and_then(Value::as_str)
}

/// A JSON-array extra (`include_columns`, `nulls_first`) as its elements.
fn json_array<'e>(extras: &'e Extras, prop: &PropId) -> Result<Option<&'e Vec<serde_json::Value>>> {
    match extras.get(prop) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Json(serde_json::Value::Array(items))) => Ok(Some(items)),
        Some(_) => Err(Error::invalid_input(format!(
            "extra '{prop}' must be a JSON array"
        ))),
    }
}

fn include_columns(extras: &Extras) -> Result<Vec<String>> {
    let Some(items) = json_array(extras, &props::INCLUDE_COLUMNS)? else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|item| {
            item.as_str().map(str::to_owned).ok_or_else(|| {
                Error::invalid_input("include_columns must be a JSON array of column names")
            })
        })
        .collect()
}

fn comment_text(comment: Option<&str>) -> Option<&str> {
    comment.filter(|c| !c.is_empty())
}

// --- types ----------------------------------------------------------------------------------

/// The PostgreSQL spelling of a neutral type, for columns built without a native type.
pub(crate) fn neutral_type(t: &DataType) -> Result<String> {
    let with_len = |name: &str, n: Option<u32>| match n {
        Some(n) => format!("{name}({n})"),
        None => name.to_owned(),
    };
    let with_precision = |name: &str, p: Option<u8>, tail: &str| match p {
        Some(p) => format!("{name}({p}){tail}"),
        None => format!("{name}{tail}"),
    };
    Ok(match t {
        DataType::Bool => "boolean".to_owned(),
        DataType::Int { bits, unsigned } => match (*bits, *unsigned) {
            (8 | 16, false) | (8, true) => "smallint",
            (32, false) | (16, true) => "integer",
            (64, false) | (32, true) => "bigint",
            (64, true) => "numeric(20,0)",
            _ => {
                return Err(Error::invalid_input(format!(
                    "no PostgreSQL integer type for {t}"
                )));
            }
        }
        .to_owned(),
        DataType::Float { bits } => {
            if *bits <= 32 {
                "real".to_owned()
            } else {
                "double precision".to_owned()
            }
        }
        DataType::Decimal { precision, scale } => match (precision, scale) {
            (Some(p), Some(s)) => format!("numeric({p},{s})"),
            (Some(p), None) => format!("numeric({p})"),
            (None, _) => "numeric".to_owned(),
        },
        DataType::Char { length } => with_len("character", *length),
        DataType::VarChar { length } => with_len("character varying", *length),
        DataType::Text => "text".to_owned(),
        DataType::Binary { .. } | DataType::VarBinary { .. } => "bytea".to_owned(),
        DataType::Date => "date".to_owned(),
        DataType::Time { precision } => with_precision("time", *precision, " without time zone"),
        DataType::Timestamp { precision } => {
            with_precision("timestamp", *precision, " without time zone")
        }
        DataType::TimestampTz { precision } => {
            with_precision("timestamp", *precision, " with time zone")
        }
        DataType::Interval => "interval".to_owned(),
        DataType::Uuid => "uuid".to_owned(),
        DataType::Json { binary } => if *binary { "jsonb" } else { "json" }.to_owned(),
        DataType::Array { element } => format!("{}[]", neutral_type(element)?),
        DataType::Enum { .. } => {
            return Err(Error::unsupported(
                "an enum column needs the name of a PostgreSQL enum type as its native type",
            ));
        }
        DataType::Other { name } => checked_type("?", name)?,
    })
}

/// The type of `c` as written in DDL (native type, else the neutral type's spelling).
fn type_sql(c: &ColumnDef) -> Result<String> {
    if c.native_type.trim().is_empty() {
        neutral_type(&c.data_type)
    } else {
        checked_type(&c.name, &c.native_type)
    }
}

/// `serial`-style auto-increment: auto-increment without identity or generation.
fn is_serial(c: &ColumnDef) -> bool {
    c.auto_increment && c.identity.is_none() && c.generated.is_none()
}

/// The type of `c` in `ALTER COLUMN ... TYPE`: like [`type_sql`], with `serial` pseudo-types
/// spelled as their integer type.
fn alter_type_sql(c: &ColumnDef) -> Result<String> {
    if is_serial_type(&c.native_type) {
        Ok(canonical_type(&c.native_type))
    } else {
        type_sql(c)
    }
}

/// The `serial` pseudo-type for an auto-increment column.
fn serial_type(c: &ColumnDef) -> Result<String> {
    if is_serial_type(&c.native_type) {
        return Ok(c.native_type.trim().to_lowercase());
    }
    match column_type_key(c).as_str() {
        "integer" => Ok("serial".to_owned()),
        "bigint" => Ok("bigserial".to_owned()),
        "smallint" => Ok("smallserial".to_owned()),
        other => Err(Error::invalid_input(format!(
            "column {}: auto-increment needs an integer type, not {other}",
            c.name
        ))),
    }
}

fn is_nextval(default: Option<&str>) -> bool {
    default.is_some_and(|d| d.trim_start().to_lowercase().starts_with("nextval("))
}

/// `true` when changing `old` to `new` cannot fail or lose data: integer widening, `real` →
/// `double precision`, longer `varchar` / `char`, wider `numeric`, more fractional-second
/// digits, anything → `text`.
fn is_widening(old: &str, new: &str) -> bool {
    if old == new || new == "text" {
        return true;
    }
    let int_rank = |t: &str| match t {
        "smallint" => Some(1),
        "integer" => Some(2),
        "bigint" => Some(3),
        "numeric" => Some(4),
        _ => None,
    };
    if let (Some(from), Some(to)) = (int_rank(old), int_rank(new)) {
        return from <= to;
    }
    // Decimal digits an integer type needs.
    let int_digits = |t: &str| -> Option<u32> {
        match t {
            "smallint" => Some(5),
            "integer" => Some(10),
            "bigint" => Some(19),
            _ => None,
        }
    };
    if old == "real" && new == "double precision" {
        return true;
    }
    let args = |t: &str, base: &str| -> Option<Vec<u32>> {
        let rest = t.strip_prefix(base)?;
        if rest.is_empty() {
            return Some(Vec::new());
        }
        let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
        inner.split(',').map(|a| a.trim().parse().ok()).collect()
    };
    // varchar(n) / char(n) -> varchar(m >= n) or unbounded varchar.
    for from_base in ["character varying", "character"] {
        if let (Some(from), Some(to)) = (args(old, from_base), args(new, "character varying")) {
            return match (from.first(), to.first()) {
                (_, None) => true,
                (Some(n), Some(m)) => m >= n,
                (None, Some(_)) => false,
            };
        }
    }
    // numeric(p, s) -> numeric(p2, s2) keeps every value when it has as many digits on both
    // sides of the point (p2 - s2 >= p - s, written without subtraction; in u64, so typed
    // modifiers near u32::MAX cannot overflow).
    let from_numeric = args(old, "numeric").or_else(|| int_digits(old).map(|p| vec![p]));
    if let (Some(from), Some(to)) = (from_numeric, args(new, "numeric")) {
        return match (from.as_slice(), to.as_slice()) {
            (_, []) => true,
            ([p, s], [p2, s2]) => {
                s2 >= s && u64::from(*p2) + u64::from(*s) >= u64::from(*p) + u64::from(*s2)
            }
            ([p], [p2, s2]) => u64::from(*p2) >= u64::from(*p) + u64::from(*s2),
            ([p], [p2]) => p2 >= p,
            _ => false,
        };
    }
    for (base, tail) in [
        ("time", " without time zone"),
        ("timestamp", " without time zone"),
        ("timestamp", " with time zone"),
    ] {
        let precision = |t: &str| -> Option<Option<u32>> {
            let rest = t.strip_prefix(base)?.strip_suffix(tail)?;
            if rest.is_empty() {
                return Some(None);
            }
            rest.strip_prefix('(')?
                .strip_suffix(')')?
                .parse()
                .ok()
                .map(Some)
        };
        if let (Some(from), Some(to)) = (precision(old), precision(new)) {
            return match (from, to) {
                (_, None) => true,
                (Some(p), Some(q)) => q >= p,
                (None, Some(_)) => false,
            };
        }
    }
    false
}

// --- column clauses -------------------------------------------------------------------------

fn identity_clause(id: &IdentityDef) -> String {
    let mut s = format!(
        "GENERATED {} AS IDENTITY",
        if id.always { "ALWAYS" } else { "BY DEFAULT" }
    );
    let mut options = Vec::new();
    if let Some(start) = id.start {
        options.push(format!("START WITH {start}"));
    }
    if let Some(increment) = id.increment {
        options.push(format!("INCREMENT BY {increment}"));
    }
    if !options.is_empty() {
        s.push_str(" (");
        s.push_str(&options.join(" "));
        s.push(')');
    }
    s
}

/// One column as written in `CREATE TABLE` / `ADD COLUMN`.
fn column_sql(v: &ServerVersion, c: &ColumnDef) -> Result<String> {
    if c.name.is_empty() {
        return Err(Error::invalid_input("a column needs a name"));
    }
    let serial = is_serial(c);
    let ty = if serial {
        serial_type(c)?
    } else {
        type_sql(c)?
    };
    let mut s = format!("{} {ty}", ident(&c.name));
    if let Some(collation) = &c.collation {
        s.push_str(" COLLATE ");
        s.push_str(&qualified_name(collation));
    }
    if let Some(generated) = &c.generated {
        if c.identity.is_some() || c.auto_increment || c.default.is_some() {
            return Err(Error::invalid_input(format!(
                "column {}: a generated column cannot also have a default, identity or \
                 auto-increment",
                c.name
            )));
        }
        let kind = if generated.stored {
            need(v, 12, "a stored generated column")?;
            "STORED"
        } else {
            need(v, 18, "a virtual generated column")?;
            "VIRTUAL"
        };
        let expr = checked(
            &format!("generation expression of column {}", c.name),
            &generated.expression,
        )?;
        let _ = write!(s, " GENERATED ALWAYS AS ({expr}) {kind}");
    } else if let Some(identity) = &c.identity {
        need(v, 10, "an identity column")?;
        if c.default.is_some() {
            return Err(Error::invalid_input(format!(
                "column {}: an identity column cannot have a default",
                c.name
            )));
        }
        s.push(' ');
        s.push_str(&identity_clause(identity));
    } else if serial && c.default.is_some() && !is_nextval(c.default.as_deref()) {
        return Err(Error::invalid_input(format!(
            "column {}: an auto-increment column cannot have another default",
            c.name
        )));
    }
    if (c.identity.is_some() || serial) && c.nullable {
        return Err(Error::invalid_input(format!(
            "column {}: identity and auto-increment columns are always NOT NULL",
            c.name
        )));
    }
    if !c.nullable {
        s.push_str(" NOT NULL");
    }
    if !serial
        && c.generated.is_none()
        && c.identity.is_none()
        && let Some(default) = &c.default
    {
        let expr = checked(&format!("default of column {}", c.name), default)?;
        s.push_str(" DEFAULT ");
        s.push_str(&expr);
    }
    Ok(s)
}

// --- constraints and indexes ----------------------------------------------------------------

fn constraint_prefix(name: Option<&str>) -> String {
    match name.filter(|n| !n.is_empty()) {
        Some(n) => format!("CONSTRAINT {} ", ident(n)),
        None => String::new(),
    }
}

fn pk_sql(pk: &PrimaryKeyDef) -> Result<String> {
    if pk.columns.is_empty() {
        return Err(Error::invalid_input(
            "a primary key needs at least one column",
        ));
    }
    Ok(format!(
        "{}PRIMARY KEY ({})",
        constraint_prefix(pk.name.as_deref()),
        column_list(&pk.columns)
    ))
}

/// `true` when `text` is `"schema"."name"`: two double-quoted parts and nothing else.
fn is_quoted_pair(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let mut parts = 0;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '"' {
            return false;
        }
        match quoted_end(&chars, i, false) {
            Ok(end) if end > i + 2 => i = end,
            _ => return false,
        }
        parts += 1;
        if i < chars.len() {
            if chars[i] != '.' || i + 1 == chars.len() {
                return false;
            }
            i += 1;
        }
    }
    parts == 2
}

/// A collation or operator class name as the fetcher reports it: `"schema"."name"` as it is,
/// anything else as one quoted identifier.
fn qualified_name(text: &str) -> String {
    if is_quoted_pair(text) {
        text.to_owned()
    } else {
        ident(text)
    }
}

/// A per-key JSON array extra of names (`opclasses`, `key_collations`): one entry per key,
/// `None` for the default.
fn per_key_names(idx: &IndexDef, prop: &PropId) -> Result<Vec<Option<String>>> {
    let Some(items) = json_array(&idx.extras, prop)? else {
        return Ok(vec![None; idx.columns.len()]);
    };
    if items.len() > idx.columns.len() {
        return Err(Error::invalid_input(format!(
            "index {}: '{prop}' has more entries than the index has keys",
            idx.name
        )));
    }
    let mut out = Vec::with_capacity(idx.columns.len());
    for k in 0..idx.columns.len() {
        out.push(match items.get(k) {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(name)) if name.trim().is_empty() => None,
            Some(serde_json::Value::String(name)) => Some(name.clone()),
            Some(_) => {
                return Err(Error::invalid_input(format!(
                    "index {}: '{prop}' must be a JSON array of names and nulls",
                    idx.name
                )));
            }
        });
    }
    Ok(out)
}

/// `WITH (name = 'value', ...)` of an index's `index_options` (`name=value` texts), if any.
fn index_options_sql(idx: &IndexDef) -> Result<Option<String>> {
    let Some(items) = json_array(&idx.extras, &INDEX_OPTIONS)? else {
        return Ok(None);
    };
    let mut options = Vec::new();
    for item in items {
        let parsed = item.as_str().and_then(|o| o.split_once('='));
        let Some((name, value)) = parsed.filter(|(name, _)| {
            let name = name.trim();
            name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        }) else {
            return Err(Error::invalid_input(format!(
                "index {}: index_options must be 'name=value' texts with a lowercase name",
                idx.name
            )));
        };
        options.push(format!("{} = {}", name.trim(), literal(value.trim())));
    }
    Ok((!options.is_empty()).then(|| format!("WITH ({})", options.join(", "))))
}

/// Refuses an index whose operator classes have parameters (the designer cannot write them).
fn refuse_opclass_options(idx: &IndexDef) -> Result<()> {
    if flag(&idx.extras, &OPCLASS_OPTIONS) {
        return Err(Error::unsupported(format!(
            "index {}: operator class parameters cannot be recreated by the designer; change \
             this index with SQL",
            idx.name
        )));
    }
    Ok(())
}

/// `DEFERRABLE [INITIALLY DEFERRED]` / `NOT DEFERRABLE` of a constraint's extras.
fn deferrable_clause(extras: &Extras, explicit: bool) -> Option<&'static str> {
    let deferred = flag(extras, &props::INITIALLY_DEFERRED);
    let deferrable = deferred || flag(extras, &props::DEFERRABLE);
    match (deferrable, deferred, explicit) {
        (true, true, _) => Some("DEFERRABLE INITIALLY DEFERRED"),
        (true, false, false) => Some("DEFERRABLE"),
        (true, false, true) => Some("DEFERRABLE INITIALLY IMMEDIATE"),
        (false, _, true) => Some("NOT DEFERRABLE"),
        (false, _, false) => None,
    }
}

/// `UNIQUE (...)` or `EXCLUDE ...` of an index that is a declared constraint.
fn unique_constraint_sql(v: &ServerVersion, idx: &IndexDef) -> Result<String> {
    let prefix = constraint_prefix(Some(idx.name.as_str()));
    if let Some(exclusion) = text_extra(&idx.extras, &EXCLUSION) {
        // `pg_get_constraintdef` already carries the index parameters and deferrability.
        let text = checked(&format!("exclusion constraint {}", idx.name), exclusion)?;
        return Ok(format!("{prefix}{text}"));
    }
    refuse_opclass_options(idx)?;
    let plain = idx
        .columns
        .iter()
        .all(|c| !c.is_expression && !c.descending)
        && idx.predicate.is_none()
        && json_array(&idx.extras, &NULLS_FIRST)?
            .is_none_or(|flags| flags.iter().all(|f| f.as_bool() != Some(true)))
        && per_key_names(idx, &OPCLASSES)?.iter().all(Option::is_none)
        && per_key_names(idx, &KEY_COLLATIONS)?
            .iter()
            .all(Option::is_none)
        && text_extra(&idx.extras, &props::INDEX_METHOD).is_none_or(|m| m == "btree");
    if !idx.unique || !plain {
        return Err(Error::invalid_input(format!(
            "index {}: a UNIQUE constraint covers plain ascending columns only (no expressions, \
             predicate, operator class, collation or other method); make it a unique index \
             instead",
            idx.name
        )));
    }
    if idx.columns.is_empty() {
        return Err(Error::invalid_input(format!(
            "index {} has no columns",
            idx.name
        )));
    }
    let names: Vec<String> = idx.columns.iter().map(|c| c.name.clone()).collect();
    let mut s = format!("{prefix}UNIQUE");
    if flag(&idx.extras, &NULLS_NOT_DISTINCT) {
        need(v, 15, "NULLS NOT DISTINCT")?;
        s.push_str(" NULLS NOT DISTINCT");
    }
    let _ = write!(s, " ({})", column_list(&names));
    let include = include_columns(&idx.extras)?;
    if !include.is_empty() {
        need(v, 11, "INCLUDE columns")?;
        let _ = write!(s, " INCLUDE ({})", column_list(&include));
    }
    if let Some(with) = index_options_sql(idx)? {
        s.push(' ');
        s.push_str(&with);
    }
    if let Some(space) = text_extra(&idx.extras, &props::TABLESPACE) {
        s.push_str(" USING INDEX TABLESPACE ");
        s.push_str(&ident(space));
    }
    if let Some(clause) = deferrable_clause(&idx.extras, false) {
        s.push(' ');
        s.push_str(clause);
    }
    Ok(s)
}

/// `CREATE [UNIQUE] INDEX ...` of a plain index.
fn create_index_sql(v: &ServerVersion, table: &ObjectRef, idx: &IndexDef) -> Result<String> {
    if idx.columns.is_empty() {
        return Err(Error::invalid_input(format!(
            "index {} has no columns",
            idx.name
        )));
    }
    refuse_opclass_options(idx)?;
    if flag(&idx.extras, &props::DEFERRABLE) || flag(&idx.extras, &props::INITIALLY_DEFERRED) {
        return Err(Error::invalid_input(format!(
            "index {}: only a UNIQUE constraint can be deferrable",
            idx.name
        )));
    }
    let nulls_first = json_array(&idx.extras, &NULLS_FIRST)?;
    let opclasses = per_key_names(idx, &OPCLASSES)?;
    let collations = per_key_names(idx, &KEY_COLLATIONS)?;
    let mut keys = Vec::new();
    for (pos, part) in idx.columns.iter().enumerate() {
        if part.length.is_some() {
            return Err(Error::unsupported(format!(
                "index {}: PostgreSQL has no prefix lengths on index keys",
                idx.name
            )));
        }
        let mut key = if part.is_expression {
            format!(
                "({})",
                checked(&format!("expression key of index {}", idx.name), &part.name)?
            )
        } else {
            ident(&part.name)
        };
        if let Some(collation) = &collations[pos] {
            key.push_str(" COLLATE ");
            key.push_str(&qualified_name(collation));
        }
        if let Some(opclass) = &opclasses[pos] {
            key.push(' ');
            key.push_str(&qualified_name(opclass));
        }
        if part.descending {
            key.push_str(" DESC");
        }
        if let Some(first) = nulls_first
            .and_then(|flags| flags.get(pos))
            .and_then(serde_json::Value::as_bool)
            && first != part.descending
        {
            key.push_str(if first { " NULLS FIRST" } else { " NULLS LAST" });
        }
        keys.push(key);
    }
    let mut s = String::from("CREATE ");
    if idx.unique {
        s.push_str("UNIQUE ");
    }
    s.push_str("INDEX ");
    if !idx.name.is_empty() {
        s.push_str(&ident(&idx.name));
        s.push(' ');
    }
    s.push_str("ON ");
    s.push_str(&table_sql(table));
    if let Some(method) = text_extra(&idx.extras, &props::INDEX_METHOD) {
        s.push_str(" USING ");
        s.push_str(&ident(method));
    }
    let _ = write!(s, " ({})", keys.join(", "));
    let include = include_columns(&idx.extras)?;
    if !include.is_empty() {
        need(v, 11, "INCLUDE columns")?;
        let _ = write!(s, " INCLUDE ({})", column_list(&include));
    }
    if flag(&idx.extras, &NULLS_NOT_DISTINCT) {
        need(v, 15, "NULLS NOT DISTINCT")?;
        s.push_str(" NULLS NOT DISTINCT");
    }
    if let Some(with) = index_options_sql(idx)? {
        s.push(' ');
        s.push_str(&with);
    }
    if let Some(space) = text_extra(&idx.extras, &props::TABLESPACE) {
        s.push_str(" TABLESPACE ");
        s.push_str(&ident(space));
    }
    if let Some(predicate) = &idx.predicate {
        let expr = checked(&format!("predicate of index {}", idx.name), predicate)?;
        s.push_str(" WHERE ");
        s.push_str(&expr);
    }
    Ok(s)
}

/// The step that creates `idx`: `ALTER TABLE ... ADD CONSTRAINT` for a declared constraint,
/// else `CREATE INDEX`.
fn index_step(v: &ServerVersion, table: &ObjectRef, idx: &IndexDef) -> Result<ScriptStep> {
    if idx.is_constraint {
        if idx.name.is_empty() {
            return Err(Error::invalid_input("a UNIQUE constraint needs a name"));
        }
        Ok(ScriptStep::new(format!(
            "ALTER TABLE {} ADD {}",
            table_sql(table),
            unique_constraint_sql(v, idx)?
        ))
        .described(format!("Add constraint {}", idx.name)))
    } else {
        Ok(ScriptStep::new(create_index_sql(v, table, idx)?)
            .described(format!("Create index {}", idx.name)))
    }
}

fn drop_index_step(table: &ObjectRef, idx: &IndexDef) -> Result<ScriptStep> {
    if idx.name.is_empty() {
        return Err(Error::invalid_input("an unnamed index cannot be dropped"));
    }
    Ok(if idx.is_constraint {
        drop_constraint_step(table, ObjectKind::Index, &idx.name)
    } else {
        ScriptStep::new(format!("DROP INDEX {}", index_sql(table, &idx.name)))
            .described(format!("Drop index {}", idx.name))
            .destructive()
    })
}

fn drop_constraint_step(table: &ObjectRef, kind: ObjectKind, name: &str) -> ScriptStep {
    let what = match kind {
        ObjectKind::ForeignKey => "foreign key",
        ObjectKind::Check => "check",
        ObjectKind::Table => "primary key",
        _ => "constraint",
    };
    ScriptStep::new(format!(
        "ALTER TABLE {} DROP CONSTRAINT {}",
        table_sql(table),
        ident(name)
    ))
    .described(format!("Drop {what} {name}"))
    .destructive()
}

fn fk_action_sql(action: FkAction) -> Option<&'static str> {
    match action {
        FkAction::NoAction => None,
        FkAction::Restrict => Some("RESTRICT"),
        FkAction::Cascade => Some("CASCADE"),
        FkAction::SetNull => Some("SET NULL"),
        FkAction::SetDefault => Some("SET DEFAULT"),
    }
}

fn fk_sql(v: &ServerVersion, table: &ObjectRef, fk: &ForeignKeyDef) -> Result<String> {
    let label = fk.name.as_deref().unwrap_or("(unnamed)");
    if fk.columns.is_empty() {
        return Err(Error::invalid_input(format!(
            "foreign key {label} has no columns"
        )));
    }
    if !fk.ref_columns.is_empty() && fk.ref_columns.len() != fk.columns.len() {
        return Err(Error::invalid_input(format!(
            "foreign key {label}: {} columns reference {} columns",
            fk.columns.len(),
            fk.ref_columns.len()
        )));
    }
    let target = ObjectRef::table(
        None,
        fk.ref_table.schema.as_deref().or(table.schema.as_deref()),
        fk.ref_table.name.as_str(),
    );
    let mut s = format!(
        "{}FOREIGN KEY ({}) REFERENCES {}",
        constraint_prefix(fk.name.as_deref()),
        column_list(&fk.columns),
        table_sql(&target)
    );
    if !fk.ref_columns.is_empty() {
        let _ = write!(s, " ({})", column_list(&fk.ref_columns));
    }
    match text_extra(&fk.extras, &MATCH_TYPE) {
        None | Some("simple") => {}
        Some("full") => s.push_str(" MATCH FULL"),
        Some(other) => {
            return Err(Error::unsupported(format!(
                "foreign key {label}: MATCH {other} is not supported by PostgreSQL"
            )));
        }
    }
    if let Some(action) = fk_action_sql(fk.on_update) {
        s.push_str(" ON UPDATE ");
        s.push_str(action);
    }
    if let Some(action) = fk_action_sql(fk.on_delete) {
        s.push_str(" ON DELETE ");
        s.push_str(action);
    }
    if let Some(deferrable) = deferrable_clause(&fk.extras, false) {
        s.push(' ');
        s.push_str(deferrable);
    }
    if flag(&fk.extras, &NOT_ENFORCED) {
        need(v, 18, "NOT ENFORCED foreign keys")?;
        s.push_str(" NOT ENFORCED");
    } else if flag(&fk.extras, &NOT_VALID) {
        s.push_str(" NOT VALID");
    }
    Ok(s)
}

fn add_fk_step(v: &ServerVersion, table: &ObjectRef, fk: &ForeignKeyDef) -> Result<ScriptStep> {
    Ok(ScriptStep::new(format!(
        "ALTER TABLE {} ADD {}",
        table_sql(table),
        fk_sql(v, table, fk)?
    ))
    .described(match &fk.name {
        Some(name) => format!("Add foreign key {name}"),
        None => format!("Add foreign key on {}", fk.columns.join(", ")),
    }))
}

/// `CHECK (...)`; `NOT VALID` only when `alter` (it is ignored in `CREATE TABLE`).
fn check_sql(v: &ServerVersion, check: &CheckDef, alter: bool) -> Result<String> {
    let label = check.name.as_deref().unwrap_or("(unnamed)");
    let expr = checked(&format!("check {label}"), &check.expression)?;
    let mut s = format!("{}CHECK ({expr})", constraint_prefix(check.name.as_deref()));
    if flag(&check.extras, &NO_INHERIT) {
        s.push_str(" NO INHERIT");
    }
    if flag(&check.extras, &NOT_ENFORCED) {
        need(v, 18, "NOT ENFORCED checks")?;
        s.push_str(" NOT ENFORCED");
    } else if alter && flag(&check.extras, &NOT_VALID) {
        s.push_str(" NOT VALID");
    }
    Ok(s)
}

fn add_check_step(v: &ServerVersion, table: &ObjectRef, check: &CheckDef) -> Result<ScriptStep> {
    Ok(ScriptStep::new(format!(
        "ALTER TABLE {} ADD {}",
        table_sql(table),
        check_sql(v, check, true)?
    ))
    .described(match &check.name {
        Some(name) => format!("Add check {name}"),
        None => "Add check".to_owned(),
    }))
}

fn validate_step(table: &ObjectRef, name: &str) -> ScriptStep {
    ScriptStep::new(format!(
        "ALTER TABLE {} VALIDATE CONSTRAINT {}",
        table_sql(table),
        ident(name)
    ))
    .described(format!("Validate constraint {name}"))
}

fn rename_column_step(table: &ObjectRef, from: &str, to: &str) -> ScriptStep {
    ScriptStep::new(format!(
        "ALTER TABLE {} RENAME COLUMN {} TO {}",
        table_sql(table),
        ident(from),
        ident(to)
    ))
    .described(format!("Rename column {from} to {to}"))
}

fn rename_index_step(table: &ObjectRef, from: &str, to: &str) -> ScriptStep {
    ScriptStep::new(format!(
        "ALTER INDEX {} RENAME TO {}",
        index_sql(table, from),
        ident(to)
    ))
    .described(format!("Rename index {from} to {to}"))
}

fn rename_constraint_step(table: &ObjectRef, from: &str, to: &str) -> ScriptStep {
    ScriptStep::new(format!(
        "ALTER TABLE {} RENAME CONSTRAINT {} TO {}",
        table_sql(table),
        ident(from),
        ident(to)
    ))
    .described(format!("Rename constraint {from} to {to}"))
}

fn table_comment_step(table: &ObjectRef, comment: Option<&str>) -> ScriptStep {
    ScriptStep::new(format!(
        "COMMENT ON TABLE {} IS {}",
        table_sql(table),
        comment.map_or_else(|| "NULL".to_owned(), literal)
    ))
    .described(match comment {
        Some(_) => format!("Set the comment of table {}", table.name),
        None => format!("Remove the comment of table {}", table.name),
    })
}

fn column_comment_step(table: &ObjectRef, column: &str, comment: Option<&str>) -> ScriptStep {
    ScriptStep::new(format!(
        "COMMENT ON COLUMN {}.{} IS {}",
        table_sql(table),
        ident(column),
        comment.map_or_else(|| "NULL".to_owned(), literal)
    ))
    .described(match comment {
        Some(_) => format!("Set the comment of column {column}"),
        None => format!("Remove the comment of column {column}"),
    })
}

// --- create table ---------------------------------------------------------------------------

fn create_table(v: &ServerVersion, t: &TableDef) -> Result<Script> {
    if t.obj.name.is_empty() {
        return Err(Error::invalid_input("a table needs a name"));
    }
    let mut script = Script::new();
    match text_extra(&t.extras, &props::TABLE_TYPE) {
        None | Some("partitioned") => {}
        Some(other) => {
            return Err(Error::unsupported(format!(
                "the designer cannot create a {other} table"
            )));
        }
    }
    if flag(&t.extras, &props::TEMPORARY) {
        return Err(Error::unsupported(
            "the designer cannot create a temporary table",
        ));
    }
    if text_extra(&t.extras, &props::OWNER).is_some() {
        script.warn("The table is owned by the user who creates it; the owner was not set.");
    }

    let mut lines = Vec::new();
    for c in &t.columns {
        lines.push(column_sql(v, c)?);
    }
    if let Some(pk) = &t.primary_key {
        lines.push(pk_sql(pk)?);
    }
    for idx in t.indexes.iter().filter(|i| i.is_constraint) {
        if idx.name.is_empty() {
            return Err(Error::invalid_input("a UNIQUE constraint needs a name"));
        }
        lines.push(unique_constraint_sql(v, idx)?);
    }
    for check in t.checks.iter().filter(|c| !flag(&c.extras, &NOT_VALID)) {
        lines.push(check_sql(v, check, false)?);
    }
    let mut sql = format!(
        "CREATE {}TABLE {} (",
        if flag(&t.extras, &props::UNLOGGED) {
            "UNLOGGED "
        } else {
            ""
        },
        table_sql(&t.obj)
    );
    if !lines.is_empty() {
        sql.push_str("\n    ");
        sql.push_str(&lines.join(",\n    "));
        sql.push('\n');
    }
    sql.push(')');
    if let Some(partition) = text_extra(&t.extras, &props::PARTITION_BY) {
        let text = checked("PARTITION BY", partition)?;
        sql.push_str(" PARTITION BY ");
        sql.push_str(&text);
    }
    if let Some(space) = text_extra(&t.extras, &props::TABLESPACE) {
        sql.push_str(" TABLESPACE ");
        sql.push_str(&ident(space));
    }
    script.push_step(ScriptStep::new(sql).described(format!("Create table {}", t.obj.name)));

    for idx in t.indexes.iter().filter(|i| !i.is_constraint) {
        script.push_step(index_step(v, &t.obj, idx)?);
    }
    for check in t.checks.iter().filter(|c| flag(&c.extras, &NOT_VALID)) {
        script.push_step(add_check_step(v, &t.obj, check)?);
    }
    for fk in &t.foreign_keys {
        script.push_step(add_fk_step(v, &t.obj, fk)?);
    }
    if let Some(text) = comment_text(t.comment.as_deref()) {
        script.push_step(table_comment_step(&t.obj, Some(text)));
    }
    for c in &t.columns {
        if let Some(text) = comment_text(c.comment.as_deref()) {
            script.push_step(column_comment_step(&t.obj, &c.name, Some(text)));
        }
    }
    Ok(script)
}

// --- alter table ----------------------------------------------------------------------------

/// Renames to run for one kind, through temporary names when they overlap (`a → b`, `b → a`).
/// Temporary names avoid every name in `taken`.
fn rename_plan(pairs: &[(String, String)], taken: &HashSet<String>) -> Vec<(String, String)> {
    let olds: HashSet<&str> = pairs.iter().map(|(old, _)| old.as_str()).collect();
    if !pairs.iter().any(|(_, new)| olds.contains(new.as_str())) {
        return pairs.to_vec();
    }
    let mut temp: Vec<String> = Vec::with_capacity(pairs.len());
    let mut n = 0usize;
    while temp.len() < pairs.len() {
        let name = format!("__oxbase_rename_{n}");
        n += 1;
        if !taken.contains(&name) {
            temp.push(name);
        }
    }
    let mut out: Vec<(String, String)> = pairs
        .iter()
        .zip(&temp)
        .map(|((old, _), tmp)| (old.clone(), tmp.clone()))
        .collect();
    out.extend(
        pairs
            .iter()
            .zip(&temp)
            .map(|((_, new), tmp)| (tmp.clone(), new.clone())),
    );
    out
}

/// The renames of `kind` from `ctx`, keeping only real changes; `InvalidInput` for an old name
/// that `exists` does not know.
fn renames_of(
    ctx: &DdlContext<'_>,
    kind: ObjectKind,
    exists: impl Fn(&str) -> bool,
) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for r in ctx
        .renames
        .iter()
        .filter(|r| r.kind == kind && r.old != r.new)
    {
        if !exists(&r.old) {
            return Err(Error::invalid_input(format!(
                "cannot rename {} {}: the table has no such {}",
                kind.display_name().to_lowercase(),
                r.old,
                kind.display_name().to_lowercase()
            )));
        }
        out.push((r.old.clone(), r.new.clone()));
    }
    Ok(out)
}

fn renamed<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(old, _)| old == name)
        .map(|(_, new)| new.as_str())
}

/// Index of the matching item in `old` for each item of `new`: equal names when both are
/// named, else the first unmatched item with the same content where one side is unnamed.
fn pair_items<T>(
    old: &[T],
    new: &[T],
    name_of: impl Fn(&T) -> Option<&str>,
    same: impl Fn(&T, &T) -> bool,
) -> Vec<Option<usize>> {
    let mut used = vec![false; old.len()];
    let mut matched = vec![None; new.len()];
    for (pos, item) in new.iter().enumerate() {
        if let Some(wanted) = name_of(item)
            && let Some(found) =
                (0..old.len()).find(|&k| !used[k] && name_of(&old[k]) == Some(wanted))
        {
            used[found] = true;
            matched[pos] = Some(found);
        }
    }
    for (pos, item) in new.iter().enumerate() {
        if matched[pos].is_some() {
            continue;
        }
        if let Some(found) = (0..old.len()).find(|&k| {
            !used[k]
                && (name_of(&old[k]).is_none() || name_of(item).is_none())
                && same(&old[k], item)
        }) {
            used[found] = true;
            matched[pos] = Some(found);
        }
    }
    matched
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

/// How a matched foreign key or check changes.
enum ConstraintChange {
    None,
    Validate,
    Deferrable,
    Recreate,
}

fn classify(diffs: &[PropertyDiff], after: &Extras) -> ConstraintChange {
    if diffs.is_empty() {
        return ConstraintChange::None;
    }
    let only = |allowed: &[&PropId]| {
        diffs
            .iter()
            .all(|d| d.prop.as_ref().is_some_and(|p| allowed.contains(&p)))
    };
    if only(&[&NOT_VALID]) && !flag(after, &NOT_VALID) {
        ConstraintChange::Validate
    } else if only(&[&props::DEFERRABLE, &props::INITIALLY_DEFERRED]) {
        ConstraintChange::Deferrable
    } else {
        ConstraintChange::Recreate
    }
}

/// Whether index `a` and `b` differ, with PostgreSQL defaults filled in on both sides.
fn index_changed(table: &ObjectRef, a: &IndexDef, b: &IndexDef) -> bool {
    let (mut a, mut b) = (a.clone(), b.clone());
    normalize_index(&mut a);
    normalize_index(&mut b);
    !diff_indexes(table, &a, &b, &PgRules).is_empty()
}

/// Steps for a matched foreign key `cur` → `new`. A recreate pushes the drop to `script` and
/// the add to `adds` (run later, after column changes). `force` recreates it even when it did
/// not change (the key it references is dropped and added again).
fn change_fk(
    v: &ServerVersion,
    table: &ObjectRef,
    cur: &ForeignKeyDef,
    new: &ForeignKeyDef,
    force: bool,
    script: &mut Script,
    adds: &mut Vec<ScriptStep>,
) -> Result<()> {
    let (mut a, mut b) = (cur.clone(), new.clone());
    normalize_fk(&mut a);
    normalize_fk(&mut b);
    let diffs = diff_foreign_keys(table, &a, &b, &PgRules);
    let name = || {
        cur.name
            .clone()
            .ok_or_else(|| Error::invalid_input("an unnamed foreign key cannot be changed"))
    };
    let change = if force {
        ConstraintChange::Recreate
    } else {
        classify(&diffs, &b.extras)
    };
    match change {
        ConstraintChange::None => {}
        ConstraintChange::Validate => adds.push(validate_step(table, &name()?)),
        ConstraintChange::Deferrable => {
            let name = name()?;
            adds.push(
                ScriptStep::new(format!(
                    "ALTER TABLE {} ALTER CONSTRAINT {} {}",
                    table_sql(table),
                    ident(&name),
                    deferrable_clause(&new.extras, true).unwrap_or("NOT DEFERRABLE")
                ))
                .described(format!("Change deferrability of foreign key {name}")),
            );
        }
        ConstraintChange::Recreate => {
            script.push_step(drop_constraint_step(
                table,
                ObjectKind::ForeignKey,
                &name()?,
            ));
            adds.push(add_fk_step(v, table, new)?);
        }
    }
    Ok(())
}

/// Steps for a matched check `cur` → `new`, like [`change_fk`].
fn change_check(
    v: &ServerVersion,
    table: &ObjectRef,
    cur: &CheckDef,
    new: &CheckDef,
    script: &mut Script,
    adds: &mut Vec<ScriptStep>,
) -> Result<()> {
    let (mut a, mut b) = (cur.clone(), new.clone());
    normalize_check(&mut a);
    normalize_check(&mut b);
    let diffs = diff_checks(table, &a, &b, &PgRules);
    let name = || {
        cur.name
            .clone()
            .ok_or_else(|| Error::invalid_input("an unnamed check cannot be changed"))
    };
    match classify(&diffs, &b.extras) {
        ConstraintChange::None => {}
        ConstraintChange::Validate => adds.push(validate_step(table, &name()?)),
        ConstraintChange::Deferrable | ConstraintChange::Recreate => {
            script.push_step(drop_constraint_step(table, ObjectKind::Check, &name()?));
            adds.push(add_check_step(v, table, new)?);
        }
    }
    Ok(())
}

/// `true` when `a` and `b` name the same columns, in any order.
fn same_columns(a: &[String], b: &[String]) -> bool {
    let mut a: Vec<&str> = a.iter().map(String::as_str).collect();
    let mut b: Vec<&str> = b.iter().map(String::as_str).collect();
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

fn same_fk_content(old: &ForeignKeyDef, new: &ForeignKeyDef) -> bool {
    old.columns == new.columns
        && old.ref_columns == new.ref_columns
        && PgRules.same_table_ref(&old.ref_table, &new.ref_table)
}

fn rename_column_refs(names: &mut [String], columns: &[(String, String)]) {
    for name in names {
        if let Some(new) = renamed(columns, name) {
            new.clone_into(name);
        }
    }
}

/// Emits the schema move and every rename, returning `old` as it is after them (so it can be
/// compared with `new` item by item).
/// Every column, index and constraint name of `old` and `new` (temporary rename names avoid
/// them).
fn taken_names(old: &TableDef, new: &TableDef) -> HashSet<String> {
    let mut taken = HashSet::new();
    for t in [old, new] {
        taken.extend(t.columns.iter().map(|c| c.name.clone()));
        taken.extend(t.indexes.iter().map(|i| i.name.clone()));
        taken.extend(t.foreign_keys.iter().filter_map(|f| f.name.clone()));
        taken.extend(t.checks.iter().filter_map(|c| c.name.clone()));
        taken.extend(t.primary_key.as_ref().and_then(|pk| pk.name.clone()));
    }
    taken
}

fn apply_renames(
    old: &TableDef,
    new: &TableDef,
    ctx: &DdlContext<'_>,
    s: &mut Script,
) -> Result<TableDef> {
    let taken = taken_names(old, new);
    let mut cur = old.clone();
    let mut at = old.obj.clone();
    if old.obj.schema != new.obj.schema {
        let Some(schema) = &new.obj.schema else {
            return Err(Error::invalid_input("the new table has no schema"));
        };
        s.push_step(
            ScriptStep::new(format!(
                "ALTER TABLE {} SET SCHEMA {}",
                table_sql(&at),
                ident(schema)
            ))
            .described(format!("Move table {} to schema {schema}", at.name)),
        );
        at.schema = Some(schema.clone());
    }
    if at.name != new.obj.name {
        if new.obj.name.is_empty() {
            return Err(Error::invalid_input("a table needs a name"));
        }
        s.push_step(
            ScriptStep::new(format!(
                "ALTER TABLE {} RENAME TO {}",
                table_sql(&at),
                ident(&new.obj.name)
            ))
            .described(format!("Rename table {} to {}", at.name, new.obj.name)),
        );
    }
    let table = &new.obj;
    // Foreign keys of the table to itself follow the rename.
    for fk in &mut cur.foreign_keys {
        if fk.ref_table.name == old.obj.name
            && fk
                .ref_table
                .schema
                .as_deref()
                .is_none_or(|schema| Some(schema) == old.obj.schema.as_deref())
        {
            fk.ref_table.name.clone_from(&table.name);
            fk.ref_table.schema.clone_from(&table.schema);
        }
    }
    cur.obj.clone_from(table);

    let columns = renames_of(ctx, ObjectKind::Column, |n| old.column(n).is_some())?;
    for (from, to) in rename_plan(&columns, &taken) {
        s.push_step(rename_column_step(table, &from, &to));
    }
    for col in &mut cur.columns {
        if let Some(to) = renamed(&columns, &col.name) {
            to.clone_into(&mut col.name);
        }
    }
    if let Some(pk) = &mut cur.primary_key {
        rename_column_refs(&mut pk.columns, &columns);
    }
    for idx in &mut cur.indexes {
        for part in idx.columns.iter_mut().filter(|p| !p.is_expression) {
            if let Some(to) = renamed(&columns, &part.name) {
                to.clone_into(&mut part.name);
            }
        }
    }
    for fk in &mut cur.foreign_keys {
        rename_column_refs(&mut fk.columns, &columns);
        if fk.ref_table.name == table.name && fk.ref_table.schema == table.schema {
            rename_column_refs(&mut fk.ref_columns, &columns);
        }
    }

    let indexes = renames_of(ctx, ObjectKind::Index, |n| {
        old.indexes.iter().any(|i| i.name == n)
    })?;
    for (from, to) in rename_plan(&indexes, &taken) {
        s.push_step(rename_index_step(table, &from, &to));
    }
    for idx in &mut cur.indexes {
        if let Some(to) = renamed(&indexes, &idx.name) {
            to.clone_into(&mut idx.name);
        }
    }

    let fks = renames_of(ctx, ObjectKind::ForeignKey, |n| {
        old.foreign_keys
            .iter()
            .any(|f| f.name.as_deref() == Some(n))
    })?;
    let checks = renames_of(ctx, ObjectKind::Check, |n| {
        old.checks.iter().any(|c| c.name.as_deref() == Some(n))
    })?;
    let mut constraints = fks.clone();
    constraints.extend(checks.iter().cloned());
    for (from, to) in rename_plan(&constraints, &taken) {
        s.push_step(rename_constraint_step(table, &from, &to));
    }
    for fk in &mut cur.foreign_keys {
        if let Some(to) = fk.name.as_deref().and_then(|n| renamed(&fks, n)) {
            fk.name = Some(to.to_owned());
        }
    }
    for check in &mut cur.checks {
        if let Some(to) = check.name.as_deref().and_then(|n| renamed(&checks, n)) {
            check.name = Some(to.to_owned());
        }
    }

    // A primary key that only changed its name.
    if let (Some(before), Some(after)) = (&mut cur.primary_key, &new.primary_key)
        && before.columns == after.columns
        && let (Some(from), Some(to)) = (&before.name, &after.name)
        && from != to
    {
        s.push_step(rename_constraint_step(table, from, to));
        before.name = Some(to.clone());
    }
    Ok(cur)
}

fn alter_table(
    v: &ServerVersion,
    old: &TableDef,
    new: &TableDef,
    ctx: &DdlContext<'_>,
) -> Result<Script> {
    let mut s = Script::new();
    let cur = apply_renames(old, new, ctx, &mut s)?;
    let table = &new.obj;
    let mut index_adds = Vec::new();
    let mut check_adds = Vec::new();
    let mut fk_adds = Vec::new();

    // Indexes that go or change and a changed primary key are decided first: a foreign key of
    // the table to itself that references one of them must be dropped before it and added
    // again after it.
    let mut index_drops: Vec<&IndexDef> = cur
        .indexes
        .iter()
        .filter(|idx| !new.indexes.iter().any(|i| i.name == idx.name))
        .collect();
    for idx in &new.indexes {
        match cur
            .indexes
            .iter()
            .find(|i| i.name == idx.name && !i.name.is_empty())
        {
            None => index_adds.push(index_step(v, table, idx)?),
            Some(before) => {
                if index_changed(table, before, idx) {
                    index_drops.push(before);
                    index_adds.push(index_step(v, table, idx)?);
                }
            }
        }
    }
    let pk_changed = !same_pk(cur.primary_key.as_ref(), new.primary_key.as_ref());
    let pk_drop = match &cur.primary_key {
        Some(pk) if pk_changed => Some(pk.name.clone().ok_or_else(|| {
            Error::invalid_input(format!(
                "the primary key of table {} has no name; reload the table before changing it",
                old.obj.name
            ))
        })?),
        _ => None,
    };
    // Column lists of the keys that are dropped: a changed primary key, unique indexes.
    let old_pk: Vec<String> = cur
        .primary_key
        .as_ref()
        .map(|pk| pk.columns.clone())
        .unwrap_or_default();
    let mut dropped_keys: Vec<Vec<String>> = Vec::new();
    let mut referenced: Vec<&str> = Vec::new();
    if let Some(name) = &pk_drop {
        dropped_keys.push(old_pk.clone());
        referenced.push(name);
    }
    for idx in index_drops.iter().copied() {
        if idx.unique && idx.predicate.is_none() && idx.columns.iter().all(|c| !c.is_expression) {
            dropped_keys.push(idx.columns.iter().map(|c| c.name.clone()).collect());
            if idx.is_constraint {
                referenced.push(&idx.name);
            }
        }
    }
    if !referenced.is_empty() {
        s.warn(format!(
            "{} {} dropped and added again: PostgreSQL refuses this while foreign keys of other \
             tables reference {}; drop those foreign keys first.",
            referenced.join(", "),
            if referenced.len() == 1 { "is" } else { "are" },
            if referenced.len() == 1 { "it" } else { "them" },
        ));
    }
    let depends_on_dropped = |fk: &ForeignKeyDef| {
        let self_ref = fk.ref_table.name == table.name
            && fk
                .ref_table
                .schema
                .as_deref()
                .is_none_or(|schema| Some(schema) == table.schema.as_deref());
        let refs = if fk.ref_columns.is_empty() {
            &old_pk
        } else {
            &fk.ref_columns
        };
        self_ref && dropped_keys.iter().any(|key| same_columns(key, refs))
    };

    // Drops of foreign keys, checks and indexes that go or change, then of the primary key.
    let fk_match = pair_items(
        &cur.foreign_keys,
        &new.foreign_keys,
        |fk| fk.name.as_deref(),
        same_fk_content,
    );
    for (k, fk) in cur.foreign_keys.iter().enumerate() {
        if !fk_match.contains(&Some(k)) {
            let name = fk.name.as_deref().ok_or_else(|| {
                Error::invalid_input("an unnamed foreign key cannot be dropped by name")
            })?;
            s.push_step(drop_constraint_step(table, ObjectKind::ForeignKey, name));
        }
    }
    for (fk, found) in new.foreign_keys.iter().zip(&fk_match) {
        match found {
            None => fk_adds.push(add_fk_step(v, table, fk)?),
            Some(k) => {
                let before = &cur.foreign_keys[*k];
                let force = depends_on_dropped(before);
                change_fk(v, table, before, fk, force, &mut s, &mut fk_adds)?;
            }
        }
    }
    let check_match = pair_items(
        &cur.checks,
        &new.checks,
        |c| c.name.as_deref(),
        |a, b| PgRules.same_expression(&a.expression, &b.expression),
    );
    for (k, check) in cur.checks.iter().enumerate() {
        if !check_match.contains(&Some(k)) {
            let name = check.name.as_deref().ok_or_else(|| {
                Error::invalid_input("an unnamed check cannot be dropped by name")
            })?;
            s.push_step(drop_constraint_step(table, ObjectKind::Check, name));
        }
    }
    for (check, found) in new.checks.iter().zip(&check_match) {
        match found {
            None => check_adds.push(add_check_step(v, table, check)?),
            Some(k) => change_check(v, table, &cur.checks[*k], check, &mut s, &mut check_adds)?,
        }
    }
    for idx in &index_drops {
        s.push_step(drop_index_step(table, idx)?);
    }
    if let Some(name) = &pk_drop {
        s.push_step(drop_constraint_step(table, ObjectKind::Table, name));
    }

    // Columns: drops, changes, adds.
    for col in cur.columns.iter().filter(|c| new.column(&c.name).is_none()) {
        s.push_step(
            ScriptStep::new(format!(
                "ALTER TABLE {} DROP COLUMN {}",
                table_sql(table),
                ident(&col.name)
            ))
            .described(format!("Drop column {}", col.name))
            .destructive(),
        );
    }
    let mut expected: Vec<&str> = cur
        .columns
        .iter()
        .filter(|c| new.column(&c.name).is_some())
        .map(|c| c.name.as_str())
        .collect();
    expected.extend(
        new.columns
            .iter()
            .filter(|c| cur.column(&c.name).is_none())
            .map(|c| c.name.as_str()),
    );
    let wanted: Vec<&str> = new.columns.iter().map(|c| c.name.as_str()).collect();
    if wanted != expected {
        return Err(Error::unsupported(
            "PostgreSQL cannot reorder columns: keep the existing columns in their order and \
             add new columns at the end",
        ));
    }
    for col in &new.columns {
        if let Some(before) = cur.column(&col.name) {
            column_changes(v, table, before, col, &mut s)?;
        }
    }
    for col in new.columns.iter().filter(|c| cur.column(&c.name).is_none()) {
        s.push_step(
            ScriptStep::new(format!(
                "ALTER TABLE {} ADD COLUMN {}",
                table_sql(table),
                column_sql(v, col)?
            ))
            .described(format!("Add column {}", col.name)),
        );
    }

    // Primary key, then new or changed indexes, checks and foreign keys.
    if pk_changed && let Some(pk) = &new.primary_key {
        s.push_step(
            ScriptStep::new(format!(
                "ALTER TABLE {} ADD {}",
                table_sql(table),
                pk_sql(pk)?
            ))
            .described(format!("Add primary key ({})", pk.columns.join(", "))),
        );
    }
    s.extend(index_adds);
    s.extend(check_adds);
    s.extend(fk_adds);

    table_options(&cur, new, table, &mut s)?;

    // Comments last.
    if comment_text(cur.comment.as_deref()) != comment_text(new.comment.as_deref()) {
        s.push_step(table_comment_step(
            table,
            comment_text(new.comment.as_deref()),
        ));
    }
    for col in &new.columns {
        let before = cur
            .column(&col.name)
            .and_then(|c| comment_text(c.comment.as_deref()));
        let after = comment_text(col.comment.as_deref());
        if before != after {
            s.push_step(column_comment_step(table, &col.name, after));
        }
    }
    Ok(s)
}

/// A table extra that says something: not absent, `null` or `false`.
fn set_extra<'e>(extras: &'e Extras, prop: &PropId) -> Option<&'e Value> {
    extras
        .get(prop)
        .filter(|v| !matches!(v, Value::Null | Value::Bool(false)))
}

/// `UNLOGGED` (absent counts as logged), tablespace (absent counts as the default one) and owner
/// changes; partitioning, the table type and `TEMPORARY` cannot change.
fn table_options(cur: &TableDef, new: &TableDef, table: &ObjectRef, s: &mut Script) -> Result<()> {
    let name = table_sql(table);
    for prop in [props::PARTITION_BY, props::TABLE_TYPE, props::TEMPORARY] {
        if set_extra(&cur.extras, &prop) != set_extra(&new.extras, &prop) {
            return Err(Error::unsupported(format!(
                "PostgreSQL cannot change '{prop}' of an existing table"
            )));
        }
    }
    let unlogged = flag(&new.extras, &props::UNLOGGED);
    if unlogged != flag(&cur.extras, &props::UNLOGGED) {
        s.push_step(
            ScriptStep::new(format!(
                "ALTER TABLE {name} SET {}",
                if unlogged { "UNLOGGED" } else { "LOGGED" }
            ))
            .described(if unlogged {
                "Make the table unlogged"
            } else {
                "Make the table logged"
            }),
        );
    }
    let space = text_extra(&new.extras, &props::TABLESPACE);
    if space != text_extra(&cur.extras, &props::TABLESPACE) {
        s.push_step(
            ScriptStep::new(format!(
                "ALTER TABLE {name} SET TABLESPACE {}",
                space.map_or_else(|| "pg_default".to_owned(), ident)
            ))
            .described(format!(
                "Move the table to tablespace {}",
                space.unwrap_or("pg_default")
            )),
        );
    }
    if let (Some(before), Some(after)) = (
        text_extra(&cur.extras, &props::OWNER),
        text_extra(&new.extras, &props::OWNER),
    ) && before != after
    {
        s.push_step(
            ScriptStep::new(format!("ALTER TABLE {name} OWNER TO {}", ident(after)))
                .described(format!("Change the owner to {after}")),
        );
    }
    Ok(())
}

/// The sequence a `serial` column's default draws from (`nextval('m3.t_id_seq'::regclass)`),
/// quoted for SQL; `None` when the default does not name one in a form read here.
fn serial_sequence(default: Option<&str>) -> Option<String> {
    let text = default?.trim();
    let rest = text
        .get(..8)
        .filter(|p| p.eq_ignore_ascii_case("nextval("))
        .map(|_| text[8..].trim_start())?;
    let chars: Vec<char> = rest.chars().collect();
    if chars.first() != Some(&'\'') {
        return None;
    }
    let end = quoted_end(&chars, 0, false).ok()?;
    let name: String = chars[1..end - 1]
        .iter()
        .collect::<String>()
        .replace("''", "'");
    // `schema.name`, each part bare (already folded by the server) or double-quoted.
    let chars: Vec<char> = name.chars().collect();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '"' {
            let end = quoted_end(&chars, i, false).ok()?;
            parts.push(
                chars[i + 1..end - 1]
                    .iter()
                    .collect::<String>()
                    .replace("\"\"", "\""),
            );
            i = end;
        } else {
            let start = i;
            while i < chars.len() && chars[i] != '.' {
                i += 1;
            }
            let part: String = chars[start..i].iter().collect();
            if part.is_empty() || !part.chars().all(is_ident_char) {
                return None;
            }
            parts.push(part);
        }
        if i < chars.len() {
            if chars[i] != '.' {
                return None;
            }
            i += 1;
        }
    }
    if parts.is_empty() || parts.len() > 2 {
        return None;
    }
    Some(parts.iter().map(|p| ident(p)).collect::<Vec<_>>().join("."))
}

/// The steps that turn column `cur` into `new` (same name), in the order of the module docs.
fn column_changes(
    v: &ServerVersion,
    table: &ObjectRef,
    cur: &ColumnDef,
    new: &ColumnDef,
    s: &mut Script,
) -> Result<()> {
    let rules = PgRules;
    let name = &new.name;
    let at = format!(
        "ALTER TABLE {} ALTER COLUMN {}",
        table_sql(table),
        ident(name)
    );
    let type_changed = !rules.same_type(cur, new);
    let collation_changed = new.collation != cur.collation;
    let default_changed = !rules.same_default(cur, new);
    let (cur_serial, new_serial) = (is_serial(cur), is_serial(new));
    if new_serial && !cur_serial {
        return Err(Error::unsupported(format!(
            "column {name}: PostgreSQL cannot make an existing column auto-increment (serial); \
             make it an identity column instead"
        )));
    }
    if (new.identity.is_some() || new_serial) && new.nullable {
        return Err(Error::invalid_input(format!(
            "column {name}: identity and auto-increment columns are always NOT NULL"
        )));
    }
    if (new.generated.is_some() || new.identity.is_some()) && new.default.is_some() {
        return Err(Error::invalid_input(format!(
            "column {name}: a generated or identity column cannot have a default"
        )));
    }
    if new.generated.is_some() && new.identity.is_some() {
        return Err(Error::invalid_input(format!(
            "column {name}: a column cannot be both generated and identity"
        )));
    }

    match (&cur.generated, &new.generated) {
        (None, Some(_)) => {
            return Err(Error::unsupported(format!(
                "column {name}: PostgreSQL cannot turn an existing column into a generated \
                 column; drop it and add a new one"
            )));
        }
        (Some(generated), None) => {
            if !generated.stored {
                return Err(Error::unsupported(format!(
                    "column {name}: a virtual generated column cannot become a plain column"
                )));
            }
            need(v, 13, "DROP EXPRESSION")?;
            s.push_step(
                ScriptStep::new(format!("{at} DROP EXPRESSION"))
                    .described(format!("Make column {name} a plain column")),
            );
        }
        _ => {}
    }
    if cur.identity.is_some() && new.identity.is_none() {
        // The identity's sequence goes with it.
        s.push_step(
            ScriptStep::new(format!("{at} DROP IDENTITY"))
                .described(format!("Drop the identity of column {name}"))
                .destructive(),
        );
    }
    let had_default = cur.default.is_some() && cur.generated.is_none() && cur.identity.is_none();
    let drop_default = had_default
        && (type_changed || default_changed || new.identity.is_some() || new.generated.is_some());
    if drop_default {
        s.push_step(
            ScriptStep::new(format!("{at} DROP DEFAULT"))
                .described(format!("Drop the default of column {name}")),
        );
    }
    if type_changed || collation_changed {
        let ty = alter_type_sql(new)?;
        let mut sql = format!("{at} TYPE {ty}");
        match &new.collation {
            Some(collation) => {
                sql.push_str(" COLLATE ");
                sql.push_str(&qualified_name(collation));
            }
            // Back to the type's default collation (the column had one, so the type is
            // collatable). With a type change the new type's default applies anyway.
            None if !type_changed => sql.push_str(" COLLATE pg_catalog.\"default\""),
            None => {}
        }
        if type_changed && new.generated.is_none() {
            let _ = write!(sql, " USING {}::{ty}", ident(name));
        }
        let mut step = ScriptStep::new(sql).described(if type_changed {
            format!("Change the type of column {name} to {ty}")
        } else {
            format!("Change the collation of column {name}")
        });
        let narrowing = type_changed && !is_widening(&column_type_key(cur), &column_type_key(new));
        if narrowing {
            step = step.destructive();
        }
        s.push_step(step);
        // A serial column's sequence has a type of its own (10+): it must follow, or nextval
        // fails past the old type's maximum.
        let int_type = matches!(
            column_type_key(new).as_str(),
            "smallint" | "integer" | "bigint"
        );
        if type_changed && cur_serial && new_serial && int_type {
            match serial_sequence(cur.default.as_deref()) {
                Some(seq) if at_least(v, 10) => {
                    let mut step = ScriptStep::new(format!("ALTER SEQUENCE {seq} AS {ty}"))
                        .described(format!("Change the type of the sequence of column {name}"));
                    if narrowing {
                        step = step.destructive();
                    }
                    s.push_step(step);
                }
                _ => {
                    s.warn(format!(
                        "The sequence of column {name} keeps its type; change it with ALTER \
                         SEQUENCE ... AS {ty}."
                    ));
                }
            }
        }
    }
    if cur.nullable != new.nullable {
        s.push_step(if new.nullable {
            ScriptStep::new(format!("{at} DROP NOT NULL"))
                .described(format!("Allow NULL in column {name}"))
        } else {
            ScriptStep::new(format!("{at} SET NOT NULL"))
                .described(format!("Make column {name} NOT NULL"))
        });
    }
    if new.generated.is_none() && new.identity.is_none() {
        let wanted = new.default.as_deref().or(if new_serial && cur_serial {
            cur.default.as_deref()
        } else {
            None
        });
        if let Some(default) = wanted
            && (drop_default || default_changed)
        {
            let expr = checked(&format!("default of column {name}"), default)?;
            s.push_step(
                ScriptStep::new(format!("{at} SET DEFAULT {expr}"))
                    .described(format!("Set the default of column {name}")),
            );
        }
    }
    match (&cur.identity, &new.identity) {
        (None, Some(identity)) => {
            need(v, 10, "an identity column")?;
            s.push_step(
                ScriptStep::new(format!("{at} ADD {}", identity_clause(identity)))
                    .described(format!("Make column {name} an identity column")),
            );
        }
        (Some(before), Some(after)) => {
            let mut clauses = Vec::new();
            if before.always != after.always {
                clauses.push(if after.always {
                    "SET GENERATED ALWAYS".to_owned()
                } else {
                    "SET GENERATED BY DEFAULT".to_owned()
                });
            }
            if let (Some(from), Some(to)) = (before.start, after.start)
                && from != to
            {
                clauses.push(format!("SET START WITH {to}"));
            }
            if let (Some(from), Some(to)) = (before.increment, after.increment)
                && from != to
            {
                clauses.push(format!("SET INCREMENT BY {to}"));
            }
            if !clauses.is_empty() {
                s.push_step(
                    ScriptStep::new(format!("{at} {}", clauses.join(" ")))
                        .described(format!("Change the identity of column {name}")),
                );
            }
        }
        _ => {}
    }
    if let (Some(before), Some(after)) = (&cur.generated, &new.generated)
        && !(before.stored == after.stored
            && rules.same_expression(&before.expression, &after.expression))
    {
        if before.stored != after.stored {
            return Err(Error::unsupported(format!(
                "column {name}: PostgreSQL cannot switch a generated column between stored and \
                 virtual"
            )));
        }
        need(v, 17, "changing a generation expression")?;
        let expr = checked(
            &format!("generation expression of column {name}"),
            &after.expression,
        )?;
        s.push_step(
            ScriptStep::new(format!("{at} SET EXPRESSION AS ({expr})"))
                .described(format!("Change the generation expression of column {name}")),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_dialect::Comparator;
    use oxbase_types::{GeneratedColumn, IndexColumn, Rename};

    fn v18() -> ServerVersion {
        ServerVersion::new(18, 6, 0)
    }

    fn col(name: &str, native: &str) -> ColumnDef {
        let mut c = ColumnDef::new(name, super::super::types::data_type(native));
        c.native_type = native.to_owned();
        c
    }

    fn table_ref(name: &str) -> ObjectRef {
        ObjectRef::table(Some("oxbase_test"), Some("m3"), name)
    }

    /// A table with a reserved word, quotes, a backslash and unicode in names, defaults and
    /// comments.
    fn sample() -> TableDef {
        let mut t = TableDef::new(table_ref("order"));
        let mut id = col("id", "integer").not_null();
        id.auto_increment = true;
        id.identity = Some(IdentityDef {
            always: false,
            start: None,
            increment: None,
        });
        t.columns.push(id);
        let mut code = col("na\"me", "varchar(20)").not_null();
        code.default = Some(PgQuoting.quote_str("it's a \\ path ü"));
        code.comment = Some("Kunde's \"code\" – ü".into());
        t.columns.push(code);
        let mut qty = col("select", "integer").not_null();
        qty.default = Some("0".into());
        t.columns.push(qty);
        let mut naive = col("naïve", "text");
        naive.collation = Some("C".into());
        t.columns.push(naive);
        let mut total = col("total", "numeric(12,2)");
        total.generated = Some(GeneratedColumn {
            expression: "\"select\" * 2".into(),
            stored: true,
        });
        t.columns.push(total);
        t.primary_key = Some(PrimaryKeyDef {
            name: Some("order_pkey".into()),
            columns: vec!["id".into()],
        });
        t.indexes.push(IndexDef {
            name: "order_code_key".into(),
            columns: vec![IndexColumn::column("na\"me")],
            unique: true,
            is_constraint: true,
            predicate: None,
            extras: Extras::new(),
        });
        t.indexes.push(IndexDef {
            name: "order_lower_idx".into(),
            columns: vec![
                IndexColumn {
                    name: "lower(\"naïve\")".into(),
                    is_expression: true,
                    descending: true,
                    length: None,
                },
                IndexColumn::column("select"),
            ],
            unique: false,
            is_constraint: false,
            predicate: Some("\"select\" > 0".into()),
            extras: Extras::new()
                .with(props::INDEX_METHOD, "btree")
                .with(
                    props::INCLUDE_COLUMNS,
                    Value::Json(serde_json::json!(["total"])),
                )
                .with(NULLS_FIRST, Value::Json(serde_json::json!([false, true]))),
        });
        t.checks.push(CheckDef {
            name: Some("order_qty_check".into()),
            expression: "\"select\" >= 0".into(),
            extras: Extras::new(),
        });
        t.foreign_keys.push(ForeignKeyDef {
            name: Some("order_parent_fk".into()),
            columns: vec!["select".into()],
            ref_table: ObjectRef::table(None, None, "parent"),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::Cascade,
            on_update: FkAction::NoAction,
            extras: Extras::new()
                .with(props::DEFERRABLE, true)
                .with(props::INITIALLY_DEFERRED, false),
        });
        t.comment = Some("O'Reilly \\ orders 🚀".into());
        t
    }

    fn sqls(s: &Script) -> Vec<&str> {
        s.steps.iter().map(|st| st.sql.as_str()).collect()
    }

    fn create(d: &ObjectDef) -> Result<Script> {
        PgDdl.create(d, &DdlContext::new(&v18()))
    }

    fn alter_with(old: &TableDef, new: &TableDef, renames: &[Rename]) -> Result<Script> {
        let v = v18();
        let ctx = DdlContext::new(&v).with_table(old).with_renames(renames);
        PgDdl.alter(
            &ObjectDef::Table(old.clone()),
            &ObjectDef::Table(new.clone()),
            &ctx,
        )
    }

    #[test]
    fn create_table_golden() {
        let s = create(&ObjectDef::Table(sample())).unwrap();
        assert_eq!(
            sqls(&s),
            [
                "CREATE TABLE \"m3\".\"order\" (\n    \
                 \"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL,\n    \
                 \"na\"\"me\" varchar(20) NOT NULL DEFAULT E'it''s a \\\\ path ü',\n    \
                 \"select\" integer NOT NULL DEFAULT 0,\n    \
                 \"naïve\" text COLLATE \"C\",\n    \
                 \"total\" numeric(12,2) GENERATED ALWAYS AS (\"select\" * 2) STORED,\n    \
                 CONSTRAINT \"order_pkey\" PRIMARY KEY (\"id\"),\n    \
                 CONSTRAINT \"order_code_key\" UNIQUE (\"na\"\"me\"),\n    \
                 CONSTRAINT \"order_qty_check\" CHECK (\"select\" >= 0)\n)",
                "CREATE INDEX \"order_lower_idx\" ON \"m3\".\"order\" USING \"btree\" \
                 ((lower(\"naïve\")) DESC NULLS LAST, \"select\" NULLS FIRST) \
                 INCLUDE (\"total\") WHERE \"select\" > 0",
                "ALTER TABLE \"m3\".\"order\" ADD CONSTRAINT \"order_parent_fk\" FOREIGN KEY \
                 (\"select\") REFERENCES \"m3\".\"parent\" (\"id\") ON DELETE CASCADE DEFERRABLE",
                "COMMENT ON TABLE \"m3\".\"order\" IS E'O''Reilly \\\\ orders 🚀'",
                "COMMENT ON COLUMN \"m3\".\"order\".\"na\"\"me\" IS 'Kunde''s \"code\" – ü'",
            ]
        );
        assert!(s.is_atomic());
        assert!(!s.is_destructive());
        assert_eq!(s.steps[0].description, "Create table order");
        assert_eq!(s.warnings, Vec::<String>::new());
    }

    #[test]
    fn serial_unlogged_and_neutral_types() {
        let mut t = TableDef::new(table_ref("t"));
        let mut id = ColumnDef::new("id", DataType::INT64).not_null();
        id.auto_increment = true;
        t.columns.push(id);
        t.columns
            .push(ColumnDef::new("v", DataType::VarChar { length: Some(5) }));
        t.columns.push(ColumnDef::new(
            "at",
            DataType::TimestampTz { precision: Some(3) },
        ));
        t.columns.push(ColumnDef::new(
            "tags",
            DataType::Array {
                element: Box::new(DataType::Text),
            },
        ));
        t.extras.insert(props::UNLOGGED, true);
        t.extras.insert(props::OWNER, "oxbase");
        let s = create(&ObjectDef::Table(t.clone())).unwrap();
        assert_eq!(
            sqls(&s),
            [
                "CREATE UNLOGGED TABLE \"m3\".\"t\" (\n    \"id\" bigserial NOT NULL,\n    \
              \"v\" character varying(5),\n    \"at\" timestamp(3) with time zone,\n    \
              \"tags\" text[]\n)"
            ]
        );
        assert_eq!(s.warnings.len(), 1, "owner is not set");

        // Auto-increment needs NOT NULL and an integer type.
        t.columns[0].nullable = true;
        assert!(create(&ObjectDef::Table(t.clone())).is_err());
        t.columns[0].nullable = false;
        t.columns[0].data_type = DataType::Text;
        let e = create(&ObjectDef::Table(t)).unwrap_err();
        assert!(e.to_string().contains("integer"), "{e}");
    }

    #[test]
    fn version_gates_and_invalid_definitions() {
        let mut t = TableDef::new(table_ref("t"));
        let mut g = col("g", "integer");
        g.generated = Some(GeneratedColumn {
            expression: "1".into(),
            stored: false,
        });
        t.columns.push(g);
        let v12 = ServerVersion::new(12, 0, 0);
        let e = PgDdl
            .create(&ObjectDef::Table(t.clone()), &DdlContext::new(&v12))
            .unwrap_err();
        assert!(matches!(e, Error::Unsupported { .. }), "{e}");
        assert!(e.to_string().contains("18"), "{e}");
        assert!(create(&ObjectDef::Table(t.clone())).is_ok());

        let v10 = ServerVersion::new(10, 0, 0);
        let idx = IndexDef {
            name: "i".into(),
            columns: vec![IndexColumn::column("g")],
            unique: false,
            is_constraint: false,
            predicate: None,
            extras: Extras::new().with(
                props::INCLUDE_COLUMNS,
                Value::Json(serde_json::json!(["x"])),
            ),
        };
        let index = ObjectDef::Index {
            table: table_ref("t"),
            index: idx.clone(),
        };
        assert!(matches!(
            PgDdl.create(&index, &DdlContext::new(&v10)),
            Err(Error::Unsupported { .. })
        ));
        let mut prefix = idx;
        prefix.extras = Extras::new();
        prefix.columns[0].length = Some(3);
        assert!(matches!(
            create(&ObjectDef::Index {
                table: table_ref("t"),
                index: prefix
            }),
            Err(Error::Unsupported { .. })
        ));

        // Type names and expressions cannot smuggle clauses or statements.
        for bad in [
            "integer, \"x\" integer",
            "integer NOT NULL",
            "integer DEFAULT 1",
            "text); DROP TABLE x; --",
            "text /* c */",
            "varchar(5) COLLATE \"C\"",
            "'text'",
        ] {
            let mut t = TableDef::new(table_ref("t"));
            t.columns.push(col("c", bad));
            let e = create(&ObjectDef::Table(t)).unwrap_err();
            assert!(matches!(e, Error::InvalidInput { .. }), "{bad}: {e}");
        }
        for good in [
            "character varying(20)",
            "numeric(10, 2)",
            "m2.\"Mood\"",
            "timestamp(3) with time zone",
            "integer[]",
            "geometry(Point,4326)",
            "double precision",
        ] {
            let mut t = TableDef::new(table_ref("t"));
            t.columns.push(col("c", good));
            assert!(create(&ObjectDef::Table(t)).is_ok(), "{good}");
        }
        // Defaults, generation expressions and checks are one expression each: no `;`, no
        // top-level `,` (which would add a column), no unbalanced parentheses.
        for bad in [
            "1; DROP TABLE x",
            "0, \"x\" text",
            "1) , \"x\" text CHECK (true",
        ] {
            let mut t = TableDef::new(table_ref("t"));
            let mut c = col("c", "integer");
            c.default = Some(bad.into());
            t.columns.push(c);
            let e = create(&ObjectDef::Table(t)).unwrap_err();
            assert!(matches!(e, Error::InvalidInput { .. }), "{bad}: {e}");
            let mut t = TableDef::new(table_ref("t"));
            let mut g = col("g", "integer");
            g.generated = Some(GeneratedColumn {
                expression: bad.into(),
                stored: true,
            });
            t.columns.push(g);
            assert!(create(&ObjectDef::Table(t)).is_err(), "{bad}");
        }
        for good in [
            "ARRAY[1, 2]",
            "coalesce(1, 2)",
            "'a,b'",
            "$$a, b$$",
            "\"a,b\" + 1",
        ] {
            let mut t = TableDef::new(table_ref("t"));
            let mut c = col("c", "text");
            c.default = Some(good.into());
            t.columns.push(c);
            assert!(create(&ObjectDef::Table(t)).is_ok(), "{good}");
        }
        assert!(matches!(
            PgDdl.create(
                &ObjectDef::View(oxbase_types::ViewDef {
                    obj: ObjectRef::new(ObjectKind::View, "v"),
                    definition: "SELECT 1".into(),
                    columns: vec![],
                    comment: None,
                    extras: Extras::new(),
                }),
                &DdlContext::new(&v18())
            ),
            Err(Error::Unsupported { .. })
        ));
    }

    #[test]
    fn alter_runs_in_the_documented_order() {
        let old = sample();
        let mut new = sample();
        let renames = [
            Rename::new(ObjectKind::Table, "order", "orders"),
            Rename::new(ObjectKind::Column, "naïve", "note"),
            Rename::new(ObjectKind::Index, "order_lower_idx", "orders_lower_idx"),
            Rename::new(ObjectKind::Check, "order_qty_check", "orders_qty_check"),
        ];
        new.obj.name = "orders".into();
        new.columns[3].name = "note".into();
        new.indexes[1].name = "orders_lower_idx".into();
        new.checks[0].name = Some("orders_qty_check".into());
        // Changes: drop the FK, a type change (widening) with a default, nullability, a new
        // column, a new check, a changed index, comments.
        new.foreign_keys.clear();
        new.columns[2].native_type = "bigint".into();
        new.columns[2].default = Some("1".into());
        new.columns[1].nullable = true;
        let mut added = col("extra", "text");
        added.default = Some(PgQuoting.quote_str(""));
        added.comment = Some("new".into());
        new.columns.push(added);
        new.checks.push(CheckDef {
            name: Some("orders_extra_check".into()),
            expression: "extra <> 'x'".into(),
            extras: Extras::new().with(NOT_VALID, true),
        });
        new.indexes[1].predicate = Some("\"select\" > 1".into());
        new.indexes[1].columns[0].name = "lower(note)".into();
        new.comment = None;
        let s = alter_with(&old, &new, &renames).unwrap();
        let t = "\"m3\".\"orders\"";
        let expected = [
            "ALTER TABLE \"m3\".\"order\" RENAME TO \"orders\"".to_owned(),
            format!("ALTER TABLE {t} RENAME COLUMN \"naïve\" TO \"note\""),
            "ALTER INDEX \"m3\".\"order_lower_idx\" RENAME TO \"orders_lower_idx\"".to_owned(),
            format!(
                "ALTER TABLE {t} RENAME CONSTRAINT \"order_qty_check\" TO \"orders_qty_check\""
            ),
            format!("ALTER TABLE {t} DROP CONSTRAINT \"order_parent_fk\""),
            "DROP INDEX \"m3\".\"orders_lower_idx\"".to_owned(),
            format!("ALTER TABLE {t} ALTER COLUMN \"na\"\"me\" DROP NOT NULL"),
            format!("ALTER TABLE {t} ALTER COLUMN \"select\" DROP DEFAULT"),
            format!("ALTER TABLE {t} ALTER COLUMN \"select\" TYPE bigint USING \"select\"::bigint"),
            format!("ALTER TABLE {t} ALTER COLUMN \"select\" SET DEFAULT 1"),
            format!("ALTER TABLE {t} ADD COLUMN \"extra\" text DEFAULT ''"),
            "CREATE INDEX \"orders_lower_idx\" ON \"m3\".\"orders\" USING \"btree\" \
             ((lower(note)) DESC NULLS LAST, \"select\" NULLS FIRST) INCLUDE (\"total\") \
             WHERE \"select\" > 1"
                .to_owned(),
            format!(
                "ALTER TABLE {t} ADD CONSTRAINT \"orders_extra_check\" CHECK (extra <> 'x') \
                 NOT VALID"
            ),
            format!("COMMENT ON TABLE {t} IS NULL"),
            format!("COMMENT ON COLUMN {t}.\"extra\" IS 'new'"),
        ];
        assert_eq!(sqls(&s), expected);
        assert!(s.is_atomic());
        // The FK and index drops are destructive; the widening type change is not.
        let destructive: Vec<bool> = s.steps.iter().map(|st| st.destructive).collect();
        assert_eq!(destructive.iter().filter(|d| **d).count(), 2);
        assert!(s.steps[4].destructive);
        assert!(s.steps[5].destructive);

        // Nothing changed: empty script; the new definition as fetched compares equal.
        assert!(alter_with(&old, &old, &[]).unwrap().is_empty());
    }

    #[test]
    fn column_type_changes_and_identity() {
        let table = table_ref("t");
        let alter = |a: &ColumnDef, b: &ColumnDef| {
            PgDdl.alter(
                &ObjectDef::Column {
                    table: table.clone(),
                    column: a.clone(),
                },
                &ObjectDef::Column {
                    table: table.clone(),
                    column: b.clone(),
                },
                &DdlContext::new(&v18()),
            )
        };
        let at = "ALTER TABLE \"m3\".\"t\" ALTER COLUMN \"c\"";
        // Narrowing: destructive.
        let s = alter(&col("c", "bigint"), &col("c", "integer")).unwrap();
        assert_eq!(
            sqls(&s),
            [format!("{at} TYPE integer USING \"c\"::integer")]
        );
        assert!(s.is_destructive());
        for (from, to, destructive) in [
            ("varchar(10)", "varchar(20)", false),
            ("varchar(20)", "varchar(10)", true),
            ("varchar(20)", "text", false),
            ("text", "varchar(20)", true),
            ("integer", "numeric(12,2)", false),
            ("numeric(12,2)", "numeric(14,2)", false),
            ("numeric(12,2)", "numeric(12,3)", true),
            ("timestamp(3)", "timestamp(6)", false),
            ("text", "integer", true),
            ("real", "double precision", false),
        ] {
            let s = alter(&col("c", from), &col("c", to)).unwrap();
            assert_eq!(s.is_destructive(), destructive, "{from} -> {to}");
        }
        // Alias spellings are no change.
        assert!(
            alter(&col("c", "int4"), &col("c", "integer"))
                .unwrap()
                .is_empty()
        );

        // Identity: add (after NOT NULL), change, drop.
        let plain = col("c", "integer");
        let mut ident_col = col("c", "integer").not_null();
        ident_col.auto_increment = true;
        ident_col.identity = Some(IdentityDef {
            always: true,
            start: Some(10),
            increment: None,
        });
        let s = alter(&plain, &ident_col).unwrap();
        assert_eq!(
            sqls(&s),
            [
                format!("{at} SET NOT NULL"),
                format!("{at} ADD GENERATED ALWAYS AS IDENTITY (START WITH 10)"),
            ]
        );
        let mut changed = ident_col.clone();
        changed.identity = Some(IdentityDef {
            always: false,
            start: Some(20),
            increment: Some(2),
        });
        let s = alter(&ident_col, &changed).unwrap();
        assert_eq!(
            sqls(&s),
            [format!("{at} SET GENERATED BY DEFAULT SET START WITH 20")]
        );
        let s = alter(&ident_col, &plain).unwrap();
        assert_eq!(
            sqls(&s),
            [format!("{at} DROP IDENTITY"), format!("{at} DROP NOT NULL")]
        );
        // A serial column keeps its sequence default across a type change.
        let mut serial = col("c", "integer").not_null();
        serial.auto_increment = true;
        serial.default = Some("nextval('m3.t_c_seq'::regclass)".into());
        let mut wider = serial.clone();
        wider.native_type = "bigint".into();
        let s = alter(&serial, &wider).unwrap();
        assert_eq!(
            sqls(&s),
            [
                format!("{at} DROP DEFAULT"),
                format!("{at} TYPE bigint USING \"c\"::bigint"),
                "ALTER SEQUENCE \"m3\".\"t_c_seq\" AS bigint".to_owned(),
                format!("{at} SET DEFAULT nextval('m3.t_c_seq'::regclass)"),
            ]
        );
        assert!(!s.is_destructive());
        // Back to integer: the sequence follows, and both steps are destructive.
        let s = alter(&wider, &serial).unwrap();
        assert!(s.steps[1].destructive && s.steps[2].destructive, "{s}");
        // Quoted sequence names are read and quoted again; unreadable ones leave a warning.
        let mut quoted = serial.clone();
        quoted.default = Some("nextval('\"M3\".\"T c\"\"seq\"'::regclass)".into());
        let mut quoted_wider = quoted.clone();
        quoted_wider.native_type = "bigint".into();
        let s = alter(&quoted, &quoted_wider).unwrap();
        assert_eq!(
            s.steps[2].sql,
            "ALTER SEQUENCE \"M3\".\"T c\"\"seq\" AS bigint"
        );
        let mut odd = serial.clone();
        odd.default = Some("nextval(some_function())".into());
        let mut odd_wider = odd.clone();
        odd_wider.native_type = "bigint".into();
        let s = alter(&odd, &odd_wider).unwrap();
        assert!(!s.steps.iter().any(|st| st.sql.contains("SEQUENCE")), "{s}");
        assert_eq!(s.warnings.len(), 1, "{s}");
        // Dropping an identity drops its sequence: destructive.
        let s = alter(&ident_col, &plain).unwrap();
        assert!(s.steps[0].destructive);
        // Making a plain column serial or generated is not supported.
        let mut to_serial = plain.clone().not_null();
        to_serial.auto_increment = true;
        assert!(matches!(
            alter(&plain.clone().not_null(), &to_serial),
            Err(Error::Unsupported { .. })
        ));
        let mut generated = plain.clone();
        generated.generated = Some(GeneratedColumn {
            expression: "1".into(),
            stored: true,
        });
        assert!(matches!(
            alter(&plain, &generated),
            Err(Error::Unsupported { .. })
        ));
        // Generated: drop expression (13+), change expression (17+).
        assert_eq!(
            sqls(&alter(&generated, &plain).unwrap()),
            [format!("{at} DROP EXPRESSION")]
        );
        let mut regenerated = generated.clone();
        regenerated.generated = Some(GeneratedColumn {
            expression: "2".into(),
            stored: true,
        });
        assert_eq!(
            sqls(&alter(&generated, &regenerated).unwrap()),
            [format!("{at} SET EXPRESSION AS (2)")]
        );
        let v16 = ServerVersion::new(16, 0, 0);
        assert!(matches!(
            PgDdl.alter(
                &ObjectDef::Column {
                    table: table.clone(),
                    column: generated
                },
                &ObjectDef::Column {
                    table: table.clone(),
                    column: regenerated
                },
                &DdlContext::new(&v16)
            ),
            Err(Error::Unsupported { .. })
        ));
        // Rename, collation and comment of a column on its own.
        let mut renamed = plain.clone();
        renamed.name = "d".into();
        renamed.collation = Some("C".into());
        renamed.comment = Some("x".into());
        let s = alter(&plain, &renamed).unwrap();
        assert_eq!(
            sqls(&s),
            [
                "ALTER TABLE \"m3\".\"t\" RENAME COLUMN \"c\" TO \"d\"".to_owned(),
                "ALTER TABLE \"m3\".\"t\" ALTER COLUMN \"d\" TYPE integer COLLATE \"C\"".to_owned(),
                "COMMENT ON COLUMN \"m3\".\"t\".\"d\" IS 'x'".to_owned(),
            ]
        );
    }

    #[test]
    fn reorder_is_unsupported_and_swaps_use_temporary_names() {
        let mut old = TableDef::new(table_ref("t"));
        old.columns.push(col("a", "integer"));
        old.columns.push(col("b", "integer"));
        let mut new = old.clone();
        new.columns.swap(0, 1);
        assert!(matches!(
            alter_with(&old, &new, &[]),
            Err(Error::Unsupported { .. })
        ));
        // New columns only at the end.
        let mut new = old.clone();
        new.columns.insert(0, col("z", "integer"));
        assert!(matches!(
            alter_with(&old, &new, &[]),
            Err(Error::Unsupported { .. })
        ));
        // a <-> b by renames.
        let mut new = old.clone();
        new.columns[0].name = "b".into();
        new.columns[1].name = "a".into();
        let renames = [
            Rename::new(ObjectKind::Column, "a", "b"),
            Rename::new(ObjectKind::Column, "b", "a"),
        ];
        let s = alter_with(&old, &new, &renames).unwrap();
        assert_eq!(
            sqls(&s),
            [
                "ALTER TABLE \"m3\".\"t\" RENAME COLUMN \"a\" TO \"__oxbase_rename_0\"",
                "ALTER TABLE \"m3\".\"t\" RENAME COLUMN \"b\" TO \"__oxbase_rename_1\"",
                "ALTER TABLE \"m3\".\"t\" RENAME COLUMN \"__oxbase_rename_0\" TO \"b\"",
                "ALTER TABLE \"m3\".\"t\" RENAME COLUMN \"__oxbase_rename_1\" TO \"a\"",
            ]
        );
        // A rename of an unknown column is refused.
        assert!(matches!(
            alter_with(&old, &old, &[Rename::new(ObjectKind::Column, "x", "y")]),
            Err(Error::InvalidInput { .. })
        ));
        // Primary key change: drop before the columns, add after.
        let mut keyed = old.clone();
        keyed.columns[0].nullable = false;
        keyed.primary_key = Some(PrimaryKeyDef {
            name: None,
            columns: vec!["a".into()],
        });
        let s = alter_with(&old, &keyed, &[]).unwrap();
        assert_eq!(
            sqls(&s),
            [
                "ALTER TABLE \"m3\".\"t\" ALTER COLUMN \"a\" SET NOT NULL",
                "ALTER TABLE \"m3\".\"t\" ADD PRIMARY KEY (\"a\")",
            ]
        );
        let mut fetched = keyed.clone();
        fetched.primary_key = Some(PrimaryKeyDef {
            name: Some("t_pkey".into()),
            columns: vec!["a".into()],
        });
        let mut rekeyed = fetched.clone();
        rekeyed.primary_key = Some(PrimaryKeyDef {
            name: Some("t_pkey".into()),
            columns: vec!["a".into(), "b".into()],
        });
        rekeyed.columns[1].nullable = false;
        let s = alter_with(&fetched, &rekeyed, &[]).unwrap();
        assert_eq!(
            sqls(&s),
            [
                "ALTER TABLE \"m3\".\"t\" DROP CONSTRAINT \"t_pkey\"",
                "ALTER TABLE \"m3\".\"t\" ALTER COLUMN \"b\" SET NOT NULL",
                "ALTER TABLE \"m3\".\"t\" ADD CONSTRAINT \"t_pkey\" PRIMARY KEY (\"a\", \"b\")",
            ]
        );
        assert!(s.steps[0].destructive);
        // A primary key without a name cannot be dropped: no guessing `<table>_pkey`.
        let e = alter_with(&keyed, &rekeyed, &[]).unwrap_err();
        assert!(matches!(e, Error::InvalidInput { .. }), "{e}");

        // Temporary names avoid names the table already has.
        let mut old = TableDef::new(table_ref("t"));
        for name in ["a", "b", "__oxbase_rename_0", "__oxbase_rename_2"] {
            old.columns.push(col(name, "integer"));
        }
        let mut new = old.clone();
        new.columns[0].name = "b".into();
        new.columns[1].name = "a".into();
        let s = alter_with(&old, &new, &renames).unwrap();
        assert_eq!(
            sqls(&s),
            [
                "ALTER TABLE \"m3\".\"t\" RENAME COLUMN \"a\" TO \"__oxbase_rename_1\"",
                "ALTER TABLE \"m3\".\"t\" RENAME COLUMN \"b\" TO \"__oxbase_rename_3\"",
                "ALTER TABLE \"m3\".\"t\" RENAME COLUMN \"__oxbase_rename_1\" TO \"b\"",
                "ALTER TABLE \"m3\".\"t\" RENAME COLUMN \"__oxbase_rename_3\" TO \"a\"",
            ]
        );
    }

    #[test]
    fn primary_key_change_recreates_self_references() {
        let mut old = TableDef::new(table_ref("t"));
        for name in ["a", "b", "c"] {
            old.columns.push(col(name, "integer").not_null());
        }
        old.primary_key = Some(PrimaryKeyDef {
            name: Some("t_pkey".into()),
            columns: vec!["a".into()],
        });
        old.foreign_keys.push(ForeignKeyDef {
            name: Some("t_c_fk".into()),
            columns: vec!["c".into()],
            ref_table: ObjectRef::table(Some("oxbase_test"), Some("m3"), "t"),
            ref_columns: vec!["a".into()],
            on_delete: FkAction::NoAction,
            on_update: FkAction::NoAction,
            extras: Extras::new(),
        });
        // The key moves to b; a stays unique so the (unchanged) foreign key still fits.
        let mut new = old.clone();
        new.primary_key = Some(PrimaryKeyDef {
            name: Some("t_pkey".into()),
            columns: vec!["b".into()],
        });
        new.indexes.push(IndexDef {
            name: "t_a_key".into(),
            columns: vec![IndexColumn::column("a")],
            unique: true,
            is_constraint: true,
            predicate: None,
            extras: Extras::new(),
        });
        let s = alter_with(&old, &new, &[]).unwrap();
        let t = "\"m3\".\"t\"";
        assert_eq!(
            sqls(&s),
            [
                format!("ALTER TABLE {t} DROP CONSTRAINT \"t_c_fk\""),
                format!("ALTER TABLE {t} DROP CONSTRAINT \"t_pkey\""),
                format!("ALTER TABLE {t} ADD CONSTRAINT \"t_pkey\" PRIMARY KEY (\"b\")"),
                format!("ALTER TABLE {t} ADD CONSTRAINT \"t_a_key\" UNIQUE (\"a\")"),
                format!(
                    "ALTER TABLE {t} ADD CONSTRAINT \"t_c_fk\" FOREIGN KEY (\"c\") REFERENCES \
                     {t} (\"a\")"
                ),
            ]
        );
        assert_eq!(s.warnings.len(), 1, "other tables' foreign keys: {s}");
        // A foreign key to another table is left alone.
        let mut other = old.clone();
        other.foreign_keys[0].ref_table = ObjectRef::table(None, Some("m3"), "p");
        let mut other_new = new.clone();
        other_new.foreign_keys[0].ref_table = ObjectRef::table(None, Some("m3"), "p");
        let s = alter_with(&other, &other_new, &[]).unwrap();
        assert!(!s.steps.iter().any(|st| st.sql.contains("t_c_fk")), "{s}");
        // A recreated unique constraint that the table's own foreign key references.
        let mut keyed = new.clone();
        keyed.foreign_keys[0].name = Some("t_c_fk".into());
        let mut changed = keyed.clone();
        changed.indexes[0].extras.insert(
            INDEX_OPTIONS,
            Value::Json(serde_json::json!(["fillfactor=80"])),
        );
        let s = alter_with(&keyed, &changed, &[]).unwrap();
        assert_eq!(
            sqls(&s),
            [
                format!("ALTER TABLE {t} DROP CONSTRAINT \"t_c_fk\""),
                format!("ALTER TABLE {t} DROP CONSTRAINT \"t_a_key\""),
                format!(
                    "ALTER TABLE {t} ADD CONSTRAINT \"t_a_key\" UNIQUE (\"a\") WITH (fillfactor \
                     = '80')"
                ),
                format!(
                    "ALTER TABLE {t} ADD CONSTRAINT \"t_c_fk\" FOREIGN KEY (\"c\") REFERENCES \
                     {t} (\"a\")"
                ),
            ]
        );
    }

    #[test]
    fn index_parameters_are_kept_when_an_index_is_recreated() {
        let table = table_ref("t");
        let idx = IndexDef {
            name: "t_name_idx".into(),
            columns: vec![IndexColumn::column("name"), IndexColumn::column("n")],
            unique: false,
            is_constraint: false,
            predicate: None,
            extras: Extras::new()
                .with(props::INDEX_METHOD, "btree")
                .with(
                    OPCLASSES,
                    Value::Json(serde_json::json!(["text_pattern_ops", null])),
                )
                .with(
                    KEY_COLLATIONS,
                    Value::Json(serde_json::json!(["\"public\".\"my coll\""])),
                )
                .with(
                    INDEX_OPTIONS,
                    Value::Json(serde_json::json!(["fillfactor=70"])),
                )
                .with(props::TABLESPACE, "fast"),
        };
        let def = |i: &IndexDef| ObjectDef::Index {
            table: table.clone(),
            index: i.clone(),
        };
        let expected = "CREATE INDEX \"t_name_idx\" ON \"m3\".\"t\" USING \"btree\" (\"name\" \
                        COLLATE \"public\".\"my coll\" \"text_pattern_ops\", \"n\") WITH (fillfactor = \
                        '70') TABLESPACE \"fast\"";
        assert_eq!(sqls(&create(&def(&idx)).unwrap()), [expected]);
        // Only the predicate changes: the recreated index keeps everything else.
        let mut partial = idx.clone();
        partial.predicate = Some("n > 0".into());
        let s = PgDdl
            .alter(&def(&idx), &def(&partial), &DdlContext::new(&v18()))
            .unwrap();
        assert_eq!(
            sqls(&s),
            [
                "DROP INDEX \"m3\".\"t_name_idx\"".to_owned(),
                format!("{expected} WHERE n > 0"),
            ]
        );
        assert!(s.steps[0].destructive);
        // Operator class parameters cannot be written: the index cannot be recreated.
        let mut with_params = idx.clone();
        with_params.extras.insert(OPCLASS_OPTIONS, true);
        assert!(matches!(
            create(&def(&with_params)),
            Err(Error::Unsupported { .. })
        ));
        assert!(matches!(
            PgDdl.alter(
                &def(&with_params),
                &def(&{
                    let mut p = with_params.clone();
                    p.predicate = Some("n > 0".into());
                    p
                }),
                &DdlContext::new(&v18())
            ),
            Err(Error::Unsupported { .. })
        ));
        // Bad entries are refused.
        for (prop, value) in [
            (OPCLASSES, serde_json::json!(["a", "b", "c"])),
            (OPCLASSES, serde_json::json!([1])),
            (INDEX_OPTIONS, serde_json::json!(["fill factor=1"])),
            (INDEX_OPTIONS, serde_json::json!(["novalue"])),
        ] {
            let mut bad = idx.clone();
            bad.extras.insert(prop.clone(), Value::Json(value));
            let e = create(&def(&bad)).unwrap_err();
            assert!(matches!(e, Error::InvalidInput { .. }), "{prop}: {e}");
        }
        let mut deferrable = idx.clone();
        deferrable.extras.insert(props::DEFERRABLE, true);
        assert!(create(&def(&deferrable)).is_err());

        // A UNIQUE constraint keeps its storage parameters, tablespace and deferrability.
        let uq = IndexDef {
            name: "t_code_key".into(),
            columns: vec![IndexColumn::column("code")],
            unique: true,
            is_constraint: true,
            predicate: None,
            extras: Extras::new()
                .with(props::INDEX_METHOD, "btree")
                .with(
                    INDEX_OPTIONS,
                    Value::Json(serde_json::json!(["fillfactor=80"])),
                )
                .with(props::TABLESPACE, "fast")
                .with(props::DEFERRABLE, true)
                .with(props::INITIALLY_DEFERRED, true),
        };
        assert_eq!(
            sqls(&create(&def(&uq)).unwrap()),
            [
                "ALTER TABLE \"m3\".\"t\" ADD CONSTRAINT \"t_code_key\" UNIQUE (\"code\") WITH \
                 (fillfactor = '80') USING INDEX TABLESPACE \"fast\" DEFERRABLE INITIALLY DEFERRED"
            ]
        );
        let mut immediate = uq.clone();
        immediate.extras.insert(props::INITIALLY_DEFERRED, false);
        let s = PgDdl
            .alter(&def(&uq), &def(&immediate), &DdlContext::new(&v18()))
            .unwrap();
        assert_eq!(s.len(), 2, "{s}");
        assert!(s.steps[1].sql.ends_with("\"fast\" DEFERRABLE"), "{s}");
        let mut opclass = uq.clone();
        opclass.extras.insert(
            OPCLASSES,
            Value::Json(serde_json::json!(["text_pattern_ops"])),
        );
        assert!(matches!(
            create(&def(&opclass)),
            Err(Error::InvalidInput { .. })
        ));
    }

    /// Fetched objects whose flags are absent (as the M2 fetcher reported them) or at their
    /// default: turning one flag on alone must produce a step.
    #[test]
    fn each_designer_flag_alone_changes_the_script() {
        let table = table_ref("t");
        let ctx_v = v18();
        let ctx = DdlContext::new(&ctx_v);
        let idx = IndexDef {
            name: "t_i".into(),
            columns: vec![IndexColumn::column("a")],
            unique: true,
            is_constraint: false,
            predicate: None,
            extras: Extras::new().with(props::INDEX_METHOD, "btree"),
        };
        let mut defaults = idx.clone();
        defaults.extras.insert(NULLS_NOT_DISTINCT, false);
        defaults
            .extras
            .insert(props::INCLUDE_COLUMNS, Value::Json(serde_json::json!([])));
        let index = |i: &IndexDef| ObjectDef::Index {
            table: table.clone(),
            index: i.clone(),
        };
        for (prop, value, sql) in [
            (NULLS_NOT_DISTINCT, Value::Bool(true), "NULLS NOT DISTINCT"),
            (
                props::INCLUDE_COLUMNS,
                Value::Json(serde_json::json!(["b"])),
                "INCLUDE (\"b\")",
            ),
            (
                NULLS_FIRST,
                Value::Json(serde_json::json!([true])),
                "NULLS FIRST",
            ),
            (
                OPCLASSES,
                Value::Json(serde_json::json!(["text_pattern_ops"])),
                "\"text_pattern_ops\"",
            ),
            (
                KEY_COLLATIONS,
                Value::Json(serde_json::json!(["C"])),
                "COLLATE \"C\"",
            ),
            (
                INDEX_OPTIONS,
                Value::Json(serde_json::json!(["fillfactor=50"])),
                "WITH (fillfactor = '50')",
            ),
            (
                props::TABLESPACE,
                Value::Text("fast".into()),
                "TABLESPACE \"fast\"",
            ),
        ] {
            for before in [&idx, &defaults] {
                let mut after = before.clone();
                after.extras.insert(prop.clone(), value.clone());
                let s = PgDdl.alter(&index(before), &index(&after), &ctx).unwrap();
                assert_eq!(s.len(), 2, "{prop}: {s}");
                assert!(s.steps[1].sql.contains(sql), "{prop}: {s}");
                // And the comparator sees it.
                let diffs = super::super::PgComparator.diff(&index(before), &index(&after));
                assert_eq!(diffs.len(), 1, "{prop}: {diffs:?}");
            }
        }
        // Restating a default is no change.
        let mut restated = idx.clone();
        restated
            .extras
            .insert(NULLS_FIRST, Value::Json(serde_json::json!([false])));
        assert!(
            PgDdl
                .alter(&index(&idx), &index(&restated), &ctx)
                .unwrap()
                .is_empty()
        );
        assert!(
            PgDdl
                .alter(&index(&idx), &index(&defaults), &ctx)
                .unwrap()
                .is_empty()
        );

        let check = CheckDef {
            name: Some("t_chk".into()),
            expression: "a > 0".into(),
            extras: Extras::new(),
        };
        let check_def = |c: &CheckDef| ObjectDef::Check {
            table: table.clone(),
            check: c.clone(),
        };
        let mut no_inherit = check.clone();
        no_inherit.extras.insert(NO_INHERIT, true);
        let s = PgDdl
            .alter(&check_def(&check), &check_def(&no_inherit), &ctx)
            .unwrap();
        assert_eq!(s.len(), 2, "{s}");
        assert!(s.steps[1].sql.ends_with("NO INHERIT"), "{s}");

        let fk = ForeignKeyDef {
            name: Some("t_fk".into()),
            columns: vec!["a".into()],
            ref_table: ObjectRef::table(None, Some("m3"), "p"),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::NoAction,
            on_update: FkAction::NoAction,
            extras: Extras::new(),
        };
        let fk_def = |f: &ForeignKeyDef| ObjectDef::ForeignKey {
            table: table.clone(),
            foreign_key: f.clone(),
        };
        let mut full = fk.clone();
        full.extras.insert(MATCH_TYPE, "full");
        let s = PgDdl.alter(&fk_def(&fk), &fk_def(&full), &ctx).unwrap();
        assert_eq!(s.len(), 2, "{s}");
        assert!(s.steps[1].sql.contains("MATCH FULL"), "{s}");
        let mut deferrable = fk.clone();
        deferrable.extras.insert(props::DEFERRABLE, true);
        let s = PgDdl
            .alter(&fk_def(&fk), &fk_def(&deferrable), &ctx)
            .unwrap();
        assert_eq!(
            sqls(&s),
            ["ALTER TABLE \"m3\".\"t\" ALTER CONSTRAINT \"t_fk\" DEFERRABLE INITIALLY IMMEDIATE"]
        );
        let mut simple = fk.clone();
        simple.extras.insert(MATCH_TYPE, "simple");
        assert!(
            PgDdl
                .alter(&fk_def(&fk), &fk_def(&simple), &ctx)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn literals_read_the_same_without_standard_conforming_strings() {
        assert_eq!(literal("it's"), "'it''s'");
        assert_eq!(literal("a\\b"), "E'a\\\\b'");
        // A comment that would end the literal early when backslashes escape.
        assert_eq!(
            literal("x\\'; DROP TABLE t; --"),
            "E'x\\\\''; DROP TABLE t; --'"
        );
        let expr = |text: &str| checked("default", text);
        assert_eq!(expr("'a\\b' || 'c'").unwrap(), "E'a\\\\b' || 'c'");
        // One literal for the lexical check, two statements without standard strings.
        assert_eq!(
            expr("'\\'' ; DROP TABLE t; --'").unwrap(),
            "E'\\\\'' ; DROP TABLE t; --'"
        );
        assert_eq!(expr("date'2020\\01'").unwrap(), "date E'2020\\\\01'");
        for same in [
            "E'a\\\\b'",
            "B'0101'",
            "X'1F'",
            "$$a\\b$$",
            "$t$a\\'b$t$",
            "\"a\\b\" + 1",
            "U&'d\\0061t'",
            "'plain'",
        ] {
            assert_eq!(expr(same).unwrap(), same);
        }
        assert!(matches!(expr("N'a\\b'"), Err(Error::InvalidInput { .. })));
        // Literal defaults written through the script come out as escape strings.
        let mut c = col("c", "text");
        c.default = Some(PgQuoting.quote_str("C:\\path"));
        let s = create(&ObjectDef::Column {
            table: table_ref("t"),
            column: c,
        })
        .unwrap();
        assert_eq!(
            sqls(&s),
            ["ALTER TABLE \"m3\".\"t\" ADD COLUMN \"c\" text DEFAULT E'C:\\\\path'"]
        );
    }

    #[test]
    fn widening_survives_huge_modifiers() {
        let max = u32::MAX;
        assert!(!is_widening(
            "numeric(10,2)",
            &format!("numeric({max},{max})")
        ));
        assert!(is_widening(
            &format!("numeric({max},0)"),
            &format!("numeric({max},0)")
        ));
        assert!(!is_widening(
            &format!("numeric({max},{max})"),
            "numeric(10,2)"
        ));
        // Too large for a modifier: not known to widen, so destructive.
        assert!(!is_widening("numeric(10,2)", "numeric(99999999999,2)"));
    }

    #[test]
    fn collation_and_table_option_removals() {
        let table = table_ref("t");
        let alter = |a: &ColumnDef, b: &ColumnDef| {
            PgDdl.alter(
                &ObjectDef::Column {
                    table: table.clone(),
                    column: a.clone(),
                },
                &ObjectDef::Column {
                    table: table.clone(),
                    column: b.clone(),
                },
                &DdlContext::new(&v18()),
            )
        };
        let mut c = col("c", "text");
        c.collation = Some("C".into());
        let plain = col("c", "text");
        assert_eq!(
            sqls(&alter(&c, &plain).unwrap()),
            [
                "ALTER TABLE \"m3\".\"t\" ALTER COLUMN \"c\" TYPE text COLLATE pg_catalog.\"default\""
            ]
        );
        // With a type change the new type's default collation applies.
        let s = alter(&c, &col("c", "integer")).unwrap();
        assert!(!s.steps.iter().any(|st| st.sql.contains("COLLATE")), "{s}");
        // A collation outside the search path, as the fetcher reports it.
        let mut qualified = plain.clone();
        qualified.collation = Some("\"other\".\"my \"\"coll\"".into());
        assert_eq!(
            sqls(&alter(&plain, &qualified).unwrap()),
            [
                "ALTER TABLE \"m3\".\"t\" ALTER COLUMN \"c\" TYPE text COLLATE \"other\".\"my \"\"coll\""
            ]
        );
        // Not a "schema"."name" pair: one quoted name.
        for (text, sql) in [
            ("en_US.utf8", "\"en_US.utf8\""),
            ("\"a\".\"b\".\"c\"", "\"\"\"a\"\".\"\"b\"\".\"\"c\"\"\""),
            ("\"a\"", "\"\"\"a\"\"\""),
        ] {
            assert_eq!(qualified_name(text), sql, "{text}");
        }

        let mut old = TableDef::new(table.clone());
        old.columns.push(col("a", "integer"));
        old.extras.insert(props::TABLESPACE, "fast");
        old.extras.insert(props::UNLOGGED, true);
        let mut new = old.clone();
        new.extras = Extras::new();
        assert_eq!(
            sqls(&alter_with(&old, &new, &[]).unwrap()),
            [
                "ALTER TABLE \"m3\".\"t\" SET LOGGED",
                "ALTER TABLE \"m3\".\"t\" SET TABLESPACE pg_default",
            ]
        );
        for (prop, value) in [
            (props::PARTITION_BY, Value::Text("RANGE (a)".into())),
            (props::TABLE_TYPE, Value::Text("partitioned".into())),
            (props::TEMPORARY, Value::Bool(true)),
        ] {
            let mut with = TableDef::new(table.clone());
            with.columns.push(col("a", "integer"));
            with.extras.insert(prop.clone(), value);
            let mut without = with.clone();
            without.extras = Extras::new();
            for (a, b) in [(&with, &without), (&without, &with)] {
                assert!(
                    matches!(alter_with(a, b, &[]), Err(Error::Unsupported { .. })),
                    "{prop}"
                );
            }
        }
    }

    #[test]
    fn child_kinds_create_alter_and_drop() {
        let table = table_ref("t");
        let v = v18();
        let ctx = DdlContext::new(&v);
        // Column.
        let mut c = col("note", "text");
        c.default = Some(PgQuoting.quote_str("a'b"));
        c.comment = Some("ü".into());
        let s = create(&ObjectDef::Column {
            table: table.clone(),
            column: c,
        })
        .unwrap();
        assert_eq!(
            sqls(&s),
            [
                "ALTER TABLE \"m3\".\"t\" ADD COLUMN \"note\" text DEFAULT 'a''b'",
                "COMMENT ON COLUMN \"m3\".\"t\".\"note\" IS 'ü'",
            ]
        );
        let s = PgDdl
            .drop_object(&table.child(ObjectKind::Column, "note"), &ctx)
            .unwrap();
        assert_eq!(sqls(&s), ["ALTER TABLE \"m3\".\"t\" DROP COLUMN \"note\""]);
        assert!(s.is_destructive());

        // Index: a unique constraint and a plain index, dropped the matching way.
        let uq = IndexDef {
            name: "t_code_key".into(),
            columns: vec![IndexColumn::column("code")],
            unique: true,
            is_constraint: true,
            predicate: None,
            extras: Extras::new().with(NULLS_NOT_DISTINCT, true),
        };
        let s = create(&ObjectDef::Index {
            table: table.clone(),
            index: uq.clone(),
        })
        .unwrap();
        assert_eq!(
            sqls(&s),
            [
                "ALTER TABLE \"m3\".\"t\" ADD CONSTRAINT \"t_code_key\" UNIQUE NULLS NOT DISTINCT \
              (\"code\")"
            ]
        );
        let mut owner = TableDef::new(table.clone());
        owner.indexes.push(uq.clone());
        let s = PgDdl
            .drop_object(
                &table.child(ObjectKind::Index, "t_code_key"),
                &DdlContext::new(&v).with_table(&owner),
            )
            .unwrap();
        assert_eq!(
            sqls(&s),
            ["ALTER TABLE \"m3\".\"t\" DROP CONSTRAINT \"t_code_key\""]
        );
        let s = PgDdl
            .drop_object(&table.child(ObjectKind::Index, "t_x_idx"), &ctx)
            .unwrap();
        assert_eq!(sqls(&s), ["DROP INDEX \"m3\".\"t_x_idx\""]);
        // Renaming only: ALTER INDEX; a changed definition: drop and create.
        let mut renamed = uq.clone();
        renamed.name = "t_code_uq".into();
        let s = PgDdl
            .alter(
                &ObjectDef::Index {
                    table: table.clone(),
                    index: uq.clone(),
                },
                &ObjectDef::Index {
                    table: table.clone(),
                    index: renamed.clone(),
                },
                &ctx,
            )
            .unwrap();
        assert_eq!(
            sqls(&s),
            ["ALTER INDEX \"m3\".\"t_code_key\" RENAME TO \"t_code_uq\""]
        );
        let mut plain = renamed.clone();
        plain.is_constraint = false;
        plain.extras = Extras::new();
        plain.predicate = Some("code <> ''".into());
        let s = PgDdl
            .alter(
                &ObjectDef::Index {
                    table: table.clone(),
                    index: renamed,
                },
                &ObjectDef::Index {
                    table: table.clone(),
                    index: plain,
                },
                &ctx,
            )
            .unwrap();
        assert_eq!(
            sqls(&s),
            [
                "ALTER TABLE \"m3\".\"t\" DROP CONSTRAINT \"t_code_uq\"",
                "CREATE UNIQUE INDEX \"t_code_uq\" ON \"m3\".\"t\" (\"code\") WHERE code <> ''",
            ]
        );
        // A UNIQUE constraint cannot have an expression.
        let mut bad = uq;
        bad.columns[0].is_expression = true;
        assert!(
            create(&ObjectDef::Index {
                table: table.clone(),
                index: bad
            })
            .is_err()
        );

        // Foreign key: deferrability alone, validation alone, anything else recreates.
        let fk = ForeignKeyDef {
            name: Some("t_fk".into()),
            columns: vec!["a".into(), "b".into()],
            ref_table: ObjectRef::table(Some("oxbase_test"), Some("other"), "p"),
            ref_columns: vec!["x".into(), "y".into()],
            on_delete: FkAction::SetNull,
            on_update: FkAction::Restrict,
            extras: Extras::new()
                .with(props::DEFERRABLE, false)
                .with(props::INITIALLY_DEFERRED, false)
                .with(MATCH_TYPE, "full")
                .with(NOT_VALID, true),
        };
        let fk_def = |fk: &ForeignKeyDef| ObjectDef::ForeignKey {
            table: table.clone(),
            foreign_key: fk.clone(),
        };
        assert_eq!(
            sqls(&create(&fk_def(&fk)).unwrap()),
            [
                "ALTER TABLE \"m3\".\"t\" ADD CONSTRAINT \"t_fk\" FOREIGN KEY (\"a\", \"b\") \
              REFERENCES \"other\".\"p\" (\"x\", \"y\") MATCH FULL ON UPDATE RESTRICT \
              ON DELETE SET NULL NOT VALID"
            ]
        );
        let mut deferred = fk.clone();
        deferred.extras.insert(props::INITIALLY_DEFERRED, true);
        let s = PgDdl.alter(&fk_def(&fk), &fk_def(&deferred), &ctx).unwrap();
        assert_eq!(
            sqls(&s),
            ["ALTER TABLE \"m3\".\"t\" ALTER CONSTRAINT \"t_fk\" DEFERRABLE INITIALLY DEFERRED"]
        );
        let mut validated = fk.clone();
        validated.extras.insert(NOT_VALID, false);
        let s = PgDdl
            .alter(&fk_def(&fk), &fk_def(&validated), &ctx)
            .unwrap();
        assert_eq!(
            sqls(&s),
            ["ALTER TABLE \"m3\".\"t\" VALIDATE CONSTRAINT \"t_fk\""]
        );
        let mut cascade = validated.clone();
        cascade.on_delete = FkAction::Cascade;
        let s = PgDdl
            .alter(&fk_def(&validated), &fk_def(&cascade), &ctx)
            .unwrap();
        assert_eq!(s.len(), 2);
        assert!(s.steps[0].destructive);
        assert!(s.steps[1].sql.contains("ON DELETE CASCADE"));
        let mut partial = fk.clone();
        partial.extras.insert(MATCH_TYPE, "partial");
        assert!(matches!(
            create(&fk_def(&partial)),
            Err(Error::Unsupported { .. })
        ));
        assert_eq!(
            sqls(
                &PgDdl
                    .drop_object(&table.child(ObjectKind::ForeignKey, "t_fk"), &ctx)
                    .unwrap()
            ),
            ["ALTER TABLE \"m3\".\"t\" DROP CONSTRAINT \"t_fk\""]
        );

        // Check: rename and expression change.
        let check = CheckDef {
            name: Some("t_chk".into()),
            expression: "a > 0".into(),
            extras: Extras::new().with(NO_INHERIT, true),
        };
        let check_def = |c: &CheckDef| ObjectDef::Check {
            table: table.clone(),
            check: c.clone(),
        };
        assert_eq!(
            sqls(&create(&check_def(&check)).unwrap()),
            ["ALTER TABLE \"m3\".\"t\" ADD CONSTRAINT \"t_chk\" CHECK (a > 0) NO INHERIT"]
        );
        let mut changed = check.clone();
        changed.name = Some("t_chk2".into());
        changed.expression = "a >= 0".into();
        let s = PgDdl
            .alter(&check_def(&check), &check_def(&changed), &ctx)
            .unwrap();
        assert_eq!(
            sqls(&s),
            [
                "ALTER TABLE \"m3\".\"t\" RENAME CONSTRAINT \"t_chk\" TO \"t_chk2\"",
                "ALTER TABLE \"m3\".\"t\" DROP CONSTRAINT \"t_chk2\"",
                "ALTER TABLE \"m3\".\"t\" ADD CONSTRAINT \"t_chk2\" CHECK (a >= 0) NO INHERIT",
            ]
        );
        // The server's spelling of the same check is no change.
        let mut spelled = check.clone();
        spelled.expression = "(a > 0)".into();
        assert!(
            PgDdl
                .alter(&check_def(&check), &check_def(&spelled), &ctx)
                .unwrap()
                .is_empty()
        );

        // Table drop.
        let s = PgDdl.drop_object(&table, &ctx).unwrap();
        assert_eq!(sqls(&s), ["DROP TABLE \"m3\".\"t\""]);
        assert!(s.is_destructive());
        assert!(
            PgDdl
                .drop_object(&table.child(ObjectKind::Check, ""), &ctx)
                .is_err()
        );
    }

    #[test]
    fn table_options_and_moves() {
        let mut old = TableDef::new(table_ref("t"));
        old.columns.push(col("a", "integer"));
        old.extras.insert(props::OWNER, "oxbase");
        let mut new = old.clone();
        new.obj.schema = Some("archive".into());
        new.extras.insert(props::UNLOGGED, true);
        new.extras.insert(props::TABLESPACE, "fast");
        new.extras.insert(props::OWNER, "other");
        let s = alter_with(&old, &new, &[]).unwrap();
        assert_eq!(
            sqls(&s),
            [
                "ALTER TABLE \"m3\".\"t\" SET SCHEMA \"archive\"",
                "ALTER TABLE \"archive\".\"t\" SET UNLOGGED",
                "ALTER TABLE \"archive\".\"t\" SET TABLESPACE \"fast\"",
                "ALTER TABLE \"archive\".\"t\" OWNER TO \"other\"",
            ]
        );
        let mut partitioned = old.clone();
        partitioned.extras.insert(props::PARTITION_BY, "RANGE (a)");
        let mut repartitioned = partitioned.clone();
        repartitioned.extras.insert(props::PARTITION_BY, "LIST (a)");
        assert!(matches!(
            alter_with(&partitioned, &repartitioned, &[]),
            Err(Error::Unsupported { .. })
        ));
        assert!(
            alter_with(&partitioned, &partitioned, &[])
                .unwrap()
                .is_empty()
        );
    }
}
