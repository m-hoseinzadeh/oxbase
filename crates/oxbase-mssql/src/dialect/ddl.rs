//! The [`DdlGenerator`] for tables, columns, indexes, foreign keys and checks (M4, **ddl**
//! stream). Contract: `docs/m4-contracts.md` §4 and the M3 `DdlGenerator` rules.
//!
//! Summary of the SQL Server rules:
//!
//! - Identifiers through [`oxbase_dialect::BracketQuoting`] (`[a]]b]`), names `[schema].[name]`
//!   (no database part: the designer runs on a session of the table's database). Literals
//!   (comments, `sp_rename` / extended-property arguments) through `quote_str` (`N'...'`).
//!   Expressions (defaults, checks, computed columns, filters) are checked with
//!   [`check_expression`] and always written inside parentheses, so they cannot reach the
//!   statement around them. One step per statement; every step transactional (SQL Server DDL
//!   rolls back), so scripts are atomic.
//! - Renames: `EXEC sp_rename N'[s].[t]', N'new'` (table), `N'[s].[t].[c]', N'new', N'COLUMN'`,
//!   `N'[s].[t].[ix]', N'new', N'INDEX'`, `N'[s].[fk]', N'new', N'OBJECT'` (constraints:
//!   primary key, unique, foreign key, check, default). The table moves to another schema with
//!   `ALTER SCHEMA [s2] TRANSFER [s].[t]`.
//! - Defaults are named constraints: `CONSTRAINT [df] DEFAULT (<expr>)` inline, or
//!   `ALTER TABLE ... ADD CONSTRAINT [df] DEFAULT (<expr>) FOR [c]`; changing or dropping a
//!   default drops the constraint by its name ([`crate::extras::DEFAULT_CONSTRAINT`] of the old
//!   column; when the name is unknown, a step looks it up in `sys.default_constraints`) and adds
//!   a new one. New defaults are named by the new column's `default_constraint`, else the old
//!   one's, else `DF_<table>_<column>` (at most 128 characters). A primary key without a name is
//!   named `PK_<table>`.
//! - `ALTER TABLE ... ALTER COLUMN [c] <type> [COLLATE x] NULL|NOT NULL [SPARSE]` always
//!   restates the nullability (and the collation, which would otherwise fall back to the
//!   database default). Before it, the script drops what blocks it and recreates it after: the
//!   default constraint (kept when the type stays and only its length, precision or scale
//!   grows), indexes, unique constraints and checks that use the column (kept when only the
//!   length of a `varchar` / `nvarchar` / `varbinary` grows, not to `max`), the primary key and
//!   foreign keys on the column or referencing it from the same table (always), and computed
//!   columns that reference it (dropped and added again: destructive, they move to the end). A
//!   `rowversion` column is never altered; `text` / `ntext` / `image` change only to
//!   `varchar(max)` / `nvarchar(max)` / `varbinary(max)`.
//! - `IDENTITY` cannot be added to or removed from an existing column, nor its seed /
//!   increment changed: `Unsupported`. A new column may have it.
//! - Computed columns: `[c] AS (<expr>) [PERSISTED [NOT NULL]]`; a change is drop + add
//!   (destructive).
//! - Comments: `sys.sp_addextendedproperty` / `sp_updateextendedproperty` /
//!   `sp_dropextendedproperty` with `@name = N'MS_Description'` and level 0/1/2 names as
//!   `N'...'` literals (schema `dbo` when the table has none).
//! - Indexes: `CREATE [UNIQUE] CLUSTERED|NONCLUSTERED INDEX [ix] ON [s].[t] ([c] [DESC], ...)
//!   [INCLUDE (...)] [WHERE (<predicate>)] [WITH (...)]`; unique constraints and primary keys
//!   through `ALTER TABLE ... ADD CONSTRAINT`; `DROP INDEX [ix] ON [s].[t]`. Columnstore, XML,
//!   spatial and hash indexes are kept as they are or dropped; creating or changing one is
//!   `Unsupported`, as are expression keys and prefix lengths.
//! - Foreign keys: `NO ACTION`, `CASCADE`, `SET NULL`, `SET DEFAULT` (`RESTRICT` is
//!   `Unsupported`); `WITH NOCHECK` for an untrusted key, `NOCHECK CONSTRAINT` for a disabled one.
//! - Reordering columns, temporal (system-versioned), memory-optimized, graph and ledger tables,
//!   data compression and sparse changes: `Unsupported` in M4.
//!
//! # Table `alter`
//!
//! In order: schema move and renames; drops of foreign keys, checks, indexes (nonclustered
//! first) and the primary key that go, change or block a column change; default constraints
//! that change or block; column drops (also computed columns that are added again); `ALTER
//! COLUMN`s; column adds; defaults; the primary key; indexes (clustered first), checks and
//! foreign keys; comments.

#![allow(
    clippy::many_single_char_names,
    reason = "short names for versions, scripts and columns read best in SQL builders"
)]

use std::collections::HashSet;
use std::fmt::Write as _;

use oxbase_dialect::compare::{CompareRules, diff_checks, diff_foreign_keys, diff_indexes};
use oxbase_dialect::sql::check_expression;
use oxbase_dialect::{BracketQuoting, DdlContext, DdlGenerator, Quoting};
use oxbase_types::{
    CheckDef, ColumnDef, DataType, Error, Extras, FkAction, ForeignKeyDef, IdentityDef, IndexDef,
    ObjectDef, ObjectKind, ObjectRef, PrimaryKeyDef, PropId, Result, Script, ScriptStep,
    ServerVersion, TableDef, Value, props,
};

use super::compare::{
    MsSqlRules, canonical_type, index_clustered, mentions_column, normalize_index, pk_clustered,
    split_type,
};
use crate::extras;

#[cfg(test)]
mod tests;

/// Longest identifier, in characters.
const MAX_NAME: usize = 128;

/// The DDL generator of every designer kind.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MsSqlDdl;

impl DdlGenerator for MsSqlDdl {
    fn create(&self, d: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        let v = ctx.version;
        match d {
            ObjectDef::Table(t) => create_table(v, t),
            ObjectDef::Column { table, column } => {
                check_name("column", &column.name)?;
                let mut s = Script::new();
                s.push_step(
                    ScriptStep::new(format!(
                        "ALTER TABLE {} ADD {}",
                        table_sql(table),
                        column_sql(v, table, column)?
                    ))
                    .described(format!("Add column {}", column.name)),
                );
                if let Some(text) = comment_text(column.comment.as_deref()) {
                    s.push_step(comment_step(
                        Proc::Add,
                        table,
                        Some(&column.name),
                        Some(text),
                    ));
                }
                Ok(s)
            }
            ObjectDef::Index { table, index } => {
                let mut s = Script::new();
                s.extend(index_steps(table, index)?);
                Ok(s)
            }
            ObjectDef::ForeignKey { table, foreign_key } => {
                let mut s = Script::new();
                s.extend(fk_steps(table, foreign_key)?);
                Ok(s)
            }
            ObjectDef::Check { table, check } => {
                let mut s = Script::new();
                s.extend(check_steps(table, check)?);
                Ok(s)
            }
            other => Err(not_designer_kind(other.kind())),
        }
    }

    fn alter(&self, old: &ObjectDef, new: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        let v = ctx.version;
        match (old, new) {
            (ObjectDef::Table(before), ObjectDef::Table(after)) => {
                alter_table(v, before, after, ctx)
            }
            _ if old.kind() != new.kind() => Err(Error::invalid_input(format!(
                "cannot alter a {} into a {}",
                old.kind().display_name(),
                new.kind().display_name()
            ))),
            (ObjectDef::Column { .. }, ObjectDef::Column { table, .. }) => {
                let Some(current) = ctx.table else {
                    return Err(Error::invalid_input(format!(
                        "changing a column of {} needs the table as it is on the server",
                        table.display_name()
                    )));
                };
                let target = replace_child(current, Some(old), Some(new));
                alter_table(v, current, &target, ctx)
            }
            (
                ObjectDef::Index { table, .. }
                | ObjectDef::ForeignKey { table, .. }
                | ObjectDef::Check { table, .. },
                _,
            ) => {
                // Without the table, a table holding only this child stands in for it.
                let from = match ctx.table {
                    Some(current) => current.clone(),
                    None => replace_child(&TableDef::new(table.clone()), None, Some(old)),
                };
                let target = replace_child(&from, Some(old), Some(new));
                alter_table(v, &from, &target, ctx)
            }
            _ => Err(not_designer_kind(new.kind())),
        }
    }

    fn drop_object(&self, r: &ObjectRef, ctx: &DdlContext<'_>) -> Result<Script> {
        let mut s = Script::new();
        match r.kind {
            ObjectKind::Table => {
                s.push_step(
                    ScriptStep::new(format!("DROP TABLE {}", table_sql(r)))
                        .described(format!("Drop table {}", r.name))
                        .destructive(),
                );
            }
            ObjectKind::Column => {
                let table = owner(r)?;
                if let Some(current) = ctx.table.filter(|t| t.column(&r.name).is_some()) {
                    let target = without_column(current, &r.name);
                    return alter_table(ctx.version, current, &target, ctx);
                }
                s.push_step(drop_default_step(&table, &r.name, None));
                s.push_step(drop_column_step(&table, &r.name));
            }
            ObjectKind::Index => {
                let table = owner(r)?;
                let known = ctx
                    .table
                    .and_then(|t| t.indexes.iter().find(|i| i.name == r.name));
                let index = known.cloned().unwrap_or_else(|| IndexDef {
                    name: r.name.clone(),
                    columns: Vec::new(),
                    unique: false,
                    is_constraint: false,
                    predicate: None,
                    extras: Extras::new(),
                });
                s.push_step(drop_index_step(&table, &index, true)?);
            }
            ObjectKind::ForeignKey | ObjectKind::Check => {
                let table = owner(r)?;
                if r.name.is_empty() {
                    return Err(Error::invalid_input(
                        "an unnamed constraint cannot be dropped by name",
                    ));
                }
                s.push_step(drop_constraint_step(&table, r.kind, &r.name, true));
            }
            other => return Err(not_designer_kind(other)),
        }
        Ok(s)
    }
}

fn not_designer_kind(kind: ObjectKind) -> Error {
    Error::unsupported(format!(
        "SQL Server DDL generation covers tables, columns, indexes, foreign keys and checks, \
         not {}",
        kind.plural_display_name()
    ))
}

fn owner(r: &ObjectRef) -> Result<ObjectRef> {
    r.owner()
        .filter(|o| !o.name.is_empty())
        .ok_or_else(|| Error::invalid_input(format!("{r} has no owning table")))
}

// --- names, literals, expressions -------------------------------------------------------------

fn ident(name: &str) -> String {
    BracketQuoting.quote_ident(name)
}

/// A Unicode string literal (`N'...'`).
fn lit(text: &str) -> String {
    BracketQuoting.quote_str(text)
}

/// `[schema].[table]` (never the database).
fn table_sql(table: &ObjectRef) -> String {
    BracketQuoting.qualify(&ObjectRef::table(
        None,
        table.schema.as_deref(),
        table.name.as_str(),
    ))
}

/// `[schema].[name]` of a schema-scoped object of `table`'s schema (a constraint).
fn object_sql(table: &ObjectRef, name: &str) -> String {
    BracketQuoting.qualify(&ObjectRef::table(None, table.schema.as_deref(), name))
}

/// The schema named in extended-property calls: the table's, else `dbo`.
fn schema_name(table: &ObjectRef) -> &str {
    table.schema.as_deref().unwrap_or("dbo")
}

fn column_list(columns: &[String]) -> String {
    columns
        .iter()
        .map(|c| ident(c))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Refuses an empty name or one longer than SQL Server's 128 characters.
fn check_name(what: &str, name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::invalid_input(format!("a {what} needs a name")));
    }
    if name.chars().count() > MAX_NAME {
        return Err(Error::invalid_input(format!(
            "the {what} name '{name}' is longer than SQL Server's {MAX_NAME} characters"
        )));
    }
    Ok(())
}

/// `prefix_<parts joined by _>`, cut to 128 characters.
fn generated_name(prefix: &str, parts: &[&str]) -> String {
    let mut name = prefix.to_owned();
    for part in parts {
        name.push('_');
        name.push_str(part);
    }
    name.chars().take(MAX_NAME).collect()
}

/// `text` after [`check_expression`] and a check that it is one expression (no `,` outside
/// parentheses, strings and quoted identifiers), or `InvalidInput` naming `what`. The caller
/// writes it inside parentheses.
fn checked(what: &str, text: &str) -> Result<String> {
    check_expression(&BracketQuoting, text)
        .map_err(|e| Error::invalid_input(format!("{what}: {e}")))?;
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for c in text.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '[' => quote = Some(']'),
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    return Err(Error::invalid_input(format!(
                        "{what}: a ',' outside parentheses (only one expression is allowed)"
                    )));
                }
                _ => {}
            },
        }
    }
    Ok(text.trim().to_owned())
}

/// Words that start a column clause and so must not appear in a type name.
const CLAUSE_WORDS: [&str; 22] = [
    "not",
    "null",
    "default",
    "primary",
    "references",
    "collate",
    "constraint",
    "unique",
    "check",
    "identity",
    "as",
    "sparse",
    "rowguidcol",
    "persisted",
    "foreign",
    "with",
    "masked",
    "filestream",
    "generated",
    "encrypted",
    "for",
    "on",
];

/// A native type as written by the user or the catalog, checked so that it cannot carry
/// another clause, and written back safely: `name [name ...]` or dotted / bracketed parts (an
/// alias or CLR type: each part bracketed), then optionally `(n)`, `(p, s)` or `(max)`.
pub(crate) fn checked_type(column: &str, native: &str) -> Result<String> {
    let text = native.trim();
    let bad = |why: &str| {
        Err(Error::invalid_input(format!(
            "type of column {column}: {why} in '{text}'"
        )))
    };
    let (name, args) = match text.find('(') {
        Some(open) => {
            let Some(inner) = text[open + 1..].strip_suffix(')') else {
                return bad("an unclosed or misplaced parenthesis");
            };
            (&text[..open], Some(inner))
        }
        None => (text, None),
    };
    // Name parts.
    let chars: Vec<char> = name.trim().chars().collect();
    let mut parts: Vec<(String, bool)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            let mut j = i + 1;
            let mut part = String::new();
            loop {
                match chars.get(j) {
                    None => return bad("an unclosed '['"),
                    Some(']') if chars.get(j + 1) == Some(&']') => {
                        part.push(']');
                        j += 2;
                    }
                    Some(']') => break,
                    Some(c) => {
                        part.push(*c);
                        j += 1;
                    }
                }
            }
            parts.push((part, true));
            i = j + 1;
        } else {
            let start = i;
            while i < chars.len() && chars[i] != '.' {
                i += 1;
            }
            let part: String = chars[start..i].iter().collect();
            let words: Vec<&str> = part.split_whitespace().collect();
            if words.is_empty() {
                return bad("an empty name");
            }
            for w in &words {
                if !w.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    return bad("an unexpected character");
                }
                if CLAUSE_WORDS.contains(&w.to_lowercase().as_str()) {
                    return bad("a clause keyword");
                }
            }
            parts.push((words.join(" "), false));
        }
        if i < chars.len() {
            if chars[i] != '.' || i + 1 == chars.len() {
                return bad("an unexpected character");
            }
            i += 1;
        }
    }
    if parts.is_empty() || parts.len() > 2 {
        return bad("not a type name");
    }
    let mut out = if parts.len() == 1 && !parts[0].1 {
        parts[0].0.clone()
    } else {
        parts
            .iter()
            .map(|(p, _)| ident(p))
            .collect::<Vec<_>>()
            .join(".")
    };
    if let Some(inner) = args {
        let items: Vec<String> = inner.split(',').map(|a| a.trim().to_lowercase()).collect();
        let ok = !items.is_empty()
            && items.len() <= 2
            && items
                .iter()
                .all(|a| a == "max" || (!a.is_empty() && a.chars().all(|c| c.is_ascii_digit())));
        if !ok {
            return bad("type arguments other than numbers or max");
        }
        let _ = write!(out, "({})", items.join(", "));
    }
    Ok(out)
}

/// A collation name: letters, digits and `_` only.
fn checked_collation(column: &str, collation: &str) -> Result<String> {
    let c = collation.trim();
    if c.is_empty() || !c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return Err(Error::invalid_input(format!(
            "column {column}: '{collation}' is not a collation name"
        )));
    }
    Ok(c.to_owned())
}

fn flag(e: &Extras, prop: &PropId) -> bool {
    e.get(prop).and_then(Value::as_bool).unwrap_or(false)
}

fn text_extra<'e>(e: &'e Extras, prop: &PropId) -> Option<&'e str> {
    e.get(prop).and_then(Value::as_str)
}

fn comment_text(comment: Option<&str>) -> Option<&str> {
    comment.filter(|c| !c.is_empty())
}

/// `include_columns` as names.
fn include_columns(idx: &IndexDef) -> Result<Vec<String>> {
    match idx.extras.get(&props::INCLUDE_COLUMNS) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Json(serde_json::Value::Array(items))) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_owned).ok_or_else(|| {
                    Error::invalid_input(format!(
                        "index {}: include_columns must be a JSON array of column names",
                        idx.name
                    ))
                })
            })
            .collect(),
        Some(_) => Err(Error::invalid_input(format!(
            "index {}: include_columns must be a JSON array of column names",
            idx.name
        ))),
    }
}

// --- types --------------------------------------------------------------------------------------

/// `true` when `v` has the native `json` type (SQL Server 2025, or an unknown version).
fn has_json(v: &ServerVersion) -> bool {
    v.major == 0 || v.major >= 17
}

/// The SQL Server spelling of a neutral type, for columns built without a native type.
/// Character types are Unicode (`nchar`, `nvarchar`).
pub(crate) fn neutral_type(t: &DataType, v: &ServerVersion) -> Result<String> {
    let sized = |name: &str, n: Option<u32>, limit: u32| match n {
        Some(n) if n <= limit => format!("{name}({n})"),
        _ => format!("{name}(max)"),
    };
    let precise = |name: &str, p: Option<u8>| match p {
        Some(p) => format!("{name}({})", p.min(7)),
        None => name.to_owned(),
    };
    Ok(match t {
        DataType::Bool => "bit".to_owned(),
        DataType::Int { bits, unsigned } => match (*bits, *unsigned) {
            (8, true) => "tinyint",
            (8 | 16, false) => "smallint",
            (32, false) | (16, true) => "int",
            (64, false) | (32, true) => "bigint",
            (64, true) => "decimal(20, 0)",
            _ => {
                return Err(Error::invalid_input(format!(
                    "no SQL Server integer type for {t}"
                )));
            }
        }
        .to_owned(),
        DataType::Float { bits } => if *bits <= 32 { "real" } else { "float" }.to_owned(),
        DataType::Decimal { precision, scale } => match (precision, scale) {
            (Some(p), Some(s)) => format!("decimal({p}, {s})"),
            (Some(p), None) => format!("decimal({p})"),
            (None, _) => "decimal(38, 10)".to_owned(),
        },
        DataType::Char { length } => format!("nchar({})", length.unwrap_or(1).min(4000)),
        DataType::VarChar { length } => sized("nvarchar", *length, 4000),
        DataType::Text => "nvarchar(max)".to_owned(),
        DataType::Binary { length } => format!("binary({})", length.unwrap_or(1).min(8000)),
        DataType::VarBinary { length } => sized("varbinary", *length, 8000),
        DataType::Date => "date".to_owned(),
        DataType::Time { precision } => precise("time", *precision),
        DataType::Timestamp { precision } => precise("datetime2", *precision),
        DataType::TimestampTz { precision } => precise("datetimeoffset", *precision),
        DataType::Uuid => "uniqueidentifier".to_owned(),
        DataType::Json { .. } => {
            if has_json(v) {
                "json".to_owned()
            } else {
                "nvarchar(max)".to_owned()
            }
        }
        DataType::Interval | DataType::Array { .. } | DataType::Enum { .. } => {
            return Err(Error::unsupported(format!(
                "SQL Server has no {t} type; give the column a native type"
            )));
        }
        DataType::Other { name } => checked_type("?", name)?,
    })
}

/// The type of `c` as written in DDL (native type, else the neutral type's spelling).
fn type_sql(v: &ServerVersion, c: &ColumnDef) -> Result<String> {
    if c.native_type.trim().is_empty() {
        neutral_type(&c.data_type, v)
    } else {
        checked_type(&c.name, &c.native_type)
    }
}

/// The canonical type of `c` (for comparisons), or its native text when it cannot be written.
fn type_key(v: &ServerVersion, c: &ColumnDef) -> String {
    type_sql(v, c).map_or_else(|_| canonical_type(&c.native_type), |t| canonical_type(&t))
}

fn is_rowversion(v: &ServerVersion, c: &ColumnDef) -> bool {
    flag(&c.extras, &extras::ROW_VERSION) || type_key(v, c) == "rowversion"
}

fn is_period(c: &ColumnDef) -> bool {
    matches!(c.extras.get(&extras::PERIOD), Some(Value::Text(_)))
}

/// The identity of `c`: its `IdentityDef`, or the default one for a plain auto-increment column.
fn identity_of(c: &ColumnDef) -> Option<IdentityDef> {
    c.identity.clone().or_else(|| {
        (c.auto_increment && c.generated.is_none()).then_some(IdentityDef {
            always: true,
            start: None,
            increment: None,
        })
    })
}

/// `true` when every value of type `old` fits type `new` (both canonical): the same type,
/// longer strings, more digits, more fractional-second digits, integer widening, `real` →
/// `float`, non-Unicode → Unicode, `text` / `ntext` / `image` → their `(max)` successors.
pub(crate) fn is_widening(old: &str, new: &str) -> bool {
    if old == new {
        return true;
    }
    let (ob, oa) = split_type(old);
    let (nb, na) = split_type(new);
    let num = |a: &[&str], i: usize| a.get(i).and_then(|x| x.parse::<u64>().ok());
    let max = |a: &[&str]| a.first() == Some(&"max");
    let int_rank = |b: &str| match b {
        "tinyint" => Some((1, 3)),
        "smallint" => Some((2, 5)),
        "int" => Some((3, 10)),
        "bigint" => Some((4, 19)),
        _ => None,
    };
    let decimal = |b: &str| matches!(b, "decimal" | "numeric");
    if let (Some((o, _)), Some((n, _))) = (int_rank(ob), int_rank(nb)) {
        return o <= n;
    }
    if let (Some((_, digits)), true) = (int_rank(ob), decimal(nb)) {
        return match (num(&na, 0), num(&na, 1)) {
            (Some(p), Some(s)) => p >= s + digits,
            _ => false,
        };
    }
    if decimal(ob) && decimal(nb) {
        return match (num(&oa, 0), num(&oa, 1), num(&na, 0), num(&na, 1)) {
            (Some(p), Some(s), Some(p2), Some(s2)) => s2 >= s && p2 + s >= p + s2,
            _ => false,
        };
    }
    if ob == "real" && nb == "float" {
        return true;
    }
    let chars = |b: &str| match b {
        "char" | "varchar" => Some(false),
        "nchar" | "nvarchar" => Some(true),
        _ => None,
    };
    if let (Some(o_unicode), Some(n_unicode)) = (chars(ob), chars(nb)) {
        if o_unicode && !n_unicode {
            return false;
        }
        if max(&na) {
            return true;
        }
        return !max(&oa) && matches!((num(&oa, 0), num(&na, 0)), (Some(o), Some(n)) if n >= o);
    }
    if matches!(ob, "binary" | "varbinary") && matches!(nb, "binary" | "varbinary") {
        if max(&na) {
            return true;
        }
        return !max(&oa) && matches!((num(&oa, 0), num(&na, 0)), (Some(o), Some(n)) if n >= o);
    }
    if ob == nb && matches!(ob, "datetime2" | "time" | "datetimeoffset") {
        return matches!((num(&oa, 0), num(&na, 0)), (Some(o), Some(n)) if n >= o);
    }
    match (ob, new) {
        ("text", "varchar(max)" | "nvarchar(max)")
        | ("ntext", "nvarchar(max)")
        | ("image", "varbinary(max)") => true,
        ("date", _) => nb == "datetime2" || nb == "datetime" || nb == "datetimeoffset",
        ("smalldatetime", _) => nb == "datetime" || nb == "datetime2",
        ("datetime", _) => nb == "datetime2" && num(&na, 0).is_some_and(|n| n >= 3),
        _ => false,
    }
}

// --- column clauses -----------------------------------------------------------------------------

/// The name of the default constraint of `c` on `table`: its `default_constraint` extra, else
/// `DF_<table>_<column>`.
fn default_name(table: &ObjectRef, c: &ColumnDef) -> String {
    text_extra(&c.extras, &extras::DEFAULT_CONSTRAINT)
        .filter(|n| !n.is_empty())
        .map_or_else(
            || generated_name("DF", &[&table.name, &c.name]),
            str::to_owned,
        )
}

fn identity_clause(id: &IdentityDef, c: &ColumnDef) -> String {
    let mut s = format!(
        "IDENTITY({}, {})",
        id.start.unwrap_or(1),
        id.increment.unwrap_or(1)
    );
    if flag(&c.extras, &extras::NOT_FOR_REPLICATION) {
        s.push_str(" NOT FOR REPLICATION");
    }
    s
}

/// Rules every column definition must follow.
fn validate_column(v: &ServerVersion, c: &ColumnDef) -> Result<()> {
    check_name("column", &c.name)?;
    let name = &c.name;
    if c.generated.is_some() && (c.identity.is_some() || c.auto_increment || c.default.is_some()) {
        return Err(Error::invalid_input(format!(
            "column {name}: a computed column cannot also have a default or IDENTITY"
        )));
    }
    if let Some(id) = identity_of(c) {
        if c.default.is_some() {
            return Err(Error::invalid_input(format!(
                "column {name}: an IDENTITY column cannot have a default"
            )));
        }
        if c.nullable {
            return Err(Error::invalid_input(format!(
                "column {name}: IDENTITY columns are always NOT NULL"
            )));
        }
        if id.increment == Some(0) {
            return Err(Error::invalid_input(format!(
                "column {name}: the IDENTITY increment cannot be 0"
            )));
        }
    }
    if is_period(c) {
        return Err(Error::unsupported(format!(
            "column {name} is a period column of a system-versioned table; the designer cannot \
             create or change it"
        )));
    }
    if is_rowversion(v, c) && c.default.is_some() {
        return Err(Error::invalid_input(format!(
            "column {name}: a rowversion column cannot have a default"
        )));
    }
    Ok(())
}

/// One column as written in `CREATE TABLE` / `ALTER TABLE ... ADD`.
fn column_sql(v: &ServerVersion, table: &ObjectRef, c: &ColumnDef) -> Result<String> {
    validate_column(v, c)?;
    let name = &c.name;
    if let Some(generated) = &c.generated {
        let expr = checked(&format!("computed column {name}"), &generated.expression)?;
        let mut s = format!("{} AS ({expr})", ident(name));
        if generated.stored {
            s.push_str(" PERSISTED");
            if !c.nullable {
                s.push_str(" NOT NULL");
            }
        }
        return Ok(s);
    }
    let mut s = format!("{} {}", ident(name), type_sql(v, c)?);
    if flag(&c.extras, &extras::SPARSE) {
        s.push_str(" SPARSE");
    }
    if let Some(collation) = &c.collation {
        s.push_str(" COLLATE ");
        s.push_str(&checked_collation(name, collation)?);
    }
    s.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
    if let Some(default) = &c.default {
        let expr = checked(&format!("default of column {name}"), default)?;
        let _ = write!(
            s,
            " CONSTRAINT {} DEFAULT ({expr})",
            ident(&default_name(table, c))
        );
    }
    if let Some(id) = identity_of(c) {
        s.push(' ');
        s.push_str(&identity_clause(&id, c));
    }
    Ok(s)
}

// --- constraints and indexes --------------------------------------------------------------------

fn clustered_word(clustered: bool) -> &'static str {
    if clustered {
        "CLUSTERED"
    } else {
        "NONCLUSTERED"
    }
}

/// The primary key's constraint name: its own, else `fallback`, else `PK_<table>`.
fn pk_name(table: &ObjectRef, pk: &PrimaryKeyDef, fallback: Option<&str>) -> String {
    pk.name
        .as_deref()
        .filter(|n| !n.is_empty())
        .or(fallback)
        .map_or_else(|| generated_name("PK", &[&table.name]), str::to_owned)
}

fn pk_sql(pk: &PrimaryKeyDef, clustered: bool, name: &str) -> Result<String> {
    if pk.columns.is_empty() {
        return Err(Error::invalid_input(
            "a primary key needs at least one column",
        ));
    }
    check_name("primary key", name)?;
    Ok(format!(
        "CONSTRAINT {} PRIMARY KEY {} ({})",
        ident(name),
        clustered_word(clustered),
        column_list(&pk.columns)
    ))
}

/// The index kind the designer can write: `clustered` / `nonclustered` (or none); anything
/// else is `Unsupported`.
fn plain_kind(idx: &IndexDef) -> Result<()> {
    match text_extra(&idx.extras, &extras::INDEX_KIND) {
        None => Ok(()),
        Some(kind) if kind.eq_ignore_ascii_case("clustered") => Ok(()),
        Some(kind) if kind.eq_ignore_ascii_case("nonclustered") => Ok(()),
        Some(kind) => Err(Error::unsupported(format!(
            "index {} is a {kind} index; the designer keeps or drops it but cannot create or \
             change it",
            idx.name
        ))),
    }
}

/// The key list `[a], [b] DESC`.
fn index_keys(idx: &IndexDef) -> Result<String> {
    if idx.columns.is_empty() {
        return Err(Error::invalid_input(format!(
            "index {} has no columns",
            idx.name
        )));
    }
    let mut keys = Vec::with_capacity(idx.columns.len());
    for part in &idx.columns {
        if part.is_expression {
            return Err(Error::unsupported(format!(
                "index {}: SQL Server has no expression keys; index a computed column instead",
                idx.name
            )));
        }
        if part.length.is_some() {
            return Err(Error::unsupported(format!(
                "index {}: SQL Server has no prefix lengths on index keys",
                idx.name
            )));
        }
        let mut key = ident(&part.name);
        if part.descending {
            key.push_str(" DESC");
        }
        keys.push(key);
    }
    Ok(keys.join(", "))
}

/// `WITH (...)` of the index options the fetcher reports, if any.
fn index_options(idx: &IndexDef) -> Result<Option<String>> {
    let mut options = Vec::new();
    match idx.extras.get(&props::FILL_FACTOR) {
        None | Some(Value::Null) => {}
        Some(value) => match value.as_i64() {
            Some(0) => {}
            Some(n @ 1..=100) => options.push(format!("FILLFACTOR = {n}")),
            _ => {
                return Err(Error::invalid_input(format!(
                    "index {}: fill_factor must be a number from 1 to 100",
                    idx.name
                )));
            }
        },
    }
    if flag(&idx.extras, &extras::IGNORE_DUP_KEY) {
        options.push("IGNORE_DUP_KEY = ON".to_owned());
    }
    if let Some(compression) = text_extra(&idx.extras, &extras::DATA_COMPRESSION) {
        match compression.to_ascii_uppercase().as_str() {
            "NONE" => {}
            c @ ("ROW" | "PAGE") => options.push(format!("DATA_COMPRESSION = {c}")),
            other => {
                return Err(Error::unsupported(format!(
                    "index {}: data compression {other} cannot be written by the designer",
                    idx.name
                )));
            }
        }
    }
    Ok((!options.is_empty()).then(|| format!(" WITH ({})", options.join(", "))))
}

/// `CONSTRAINT [uq] UNIQUE CLUSTERED|NONCLUSTERED (...)` of a unique constraint.
fn unique_sql(idx: &IndexDef) -> Result<String> {
    check_name("unique constraint", &idx.name)?;
    plain_kind(idx)?;
    if !idx.unique {
        return Err(Error::invalid_input(format!(
            "index {}: a constraint index must be unique",
            idx.name
        )));
    }
    if idx.predicate.is_some() || !include_columns(idx)?.is_empty() {
        return Err(Error::invalid_input(format!(
            "index {}: a UNIQUE constraint has no filter or included columns; make it a unique \
             index instead",
            idx.name
        )));
    }
    let mut s = format!(
        "CONSTRAINT {} UNIQUE {} ({})",
        ident(&idx.name),
        clustered_word(index_clustered(idx)),
        index_keys(idx)?
    );
    if let Some(with) = index_options(idx)? {
        s.push_str(&with);
    }
    Ok(s)
}

fn create_index_sql(table: &ObjectRef, idx: &IndexDef) -> Result<String> {
    check_name("index", &idx.name)?;
    plain_kind(idx)?;
    let clustered = index_clustered(idx);
    let include = include_columns(idx)?;
    if clustered && !include.is_empty() {
        return Err(Error::invalid_input(format!(
            "index {}: only nonclustered indexes have included columns",
            idx.name
        )));
    }
    if clustered && idx.predicate.is_some() {
        return Err(Error::invalid_input(format!(
            "index {}: a clustered index cannot have a filter",
            idx.name
        )));
    }
    let mut s = String::from("CREATE ");
    if idx.unique {
        s.push_str("UNIQUE ");
    }
    let _ = write!(
        s,
        "{} INDEX {} ON {} ({})",
        clustered_word(clustered),
        ident(&idx.name),
        table_sql(table),
        index_keys(idx)?
    );
    if !include.is_empty() {
        let _ = write!(s, " INCLUDE ({})", column_list(&include));
    }
    if let Some(predicate) = &idx.predicate {
        let expr = checked(&format!("filter of index {}", idx.name), predicate)?;
        let _ = write!(s, " WHERE ({expr})");
    }
    if let Some(with) = index_options(idx)? {
        s.push_str(&with);
    }
    Ok(s)
}

/// The steps that create `idx` (and disable it when it is disabled).
fn index_steps(table: &ObjectRef, idx: &IndexDef) -> Result<Vec<ScriptStep>> {
    let mut steps = vec![if idx.is_constraint {
        ScriptStep::new(format!(
            "ALTER TABLE {} ADD {}",
            table_sql(table),
            unique_sql(idx)?
        ))
        .described(format!("Add unique constraint {}", idx.name))
    } else {
        ScriptStep::new(create_index_sql(table, idx)?)
            .described(format!("Create index {}", idx.name))
    }];
    if flag(&idx.extras, &extras::DISABLED) {
        steps.push(
            ScriptStep::new(format!(
                "ALTER INDEX {} ON {} DISABLE",
                ident(&idx.name),
                table_sql(table)
            ))
            .described(format!("Disable index {}", idx.name)),
        );
    }
    Ok(steps)
}

/// Drops `idx`; `gone` marks the step destructive (the index is not created again).
fn drop_index_step(table: &ObjectRef, idx: &IndexDef, gone: bool) -> Result<ScriptStep> {
    if idx.name.is_empty() {
        return Err(Error::invalid_input("an unnamed index cannot be dropped"));
    }
    if idx.is_constraint {
        return Ok(drop_constraint_step(
            table,
            ObjectKind::Index,
            &idx.name,
            gone,
        ));
    }
    let step = ScriptStep::new(format!(
        "DROP INDEX {} ON {}",
        ident(&idx.name),
        table_sql(table)
    ))
    .described(format!("Drop index {}", idx.name));
    Ok(if gone { step.destructive() } else { step })
}

fn drop_constraint_step(table: &ObjectRef, kind: ObjectKind, name: &str, gone: bool) -> ScriptStep {
    let what = match kind {
        ObjectKind::ForeignKey => "foreign key",
        ObjectKind::Check => "check",
        ObjectKind::Table => "primary key",
        ObjectKind::Index => "unique constraint",
        _ => "constraint",
    };
    let step = ScriptStep::new(format!(
        "ALTER TABLE {} DROP CONSTRAINT {}",
        table_sql(table),
        ident(name)
    ))
    .described(format!("Drop {what} {name}"));
    if gone { step.destructive() } else { step }
}

/// Drops the primary key: by name, else looked up in `sys.key_constraints`.
fn drop_pk_step(table: &ObjectRef, name: Option<&str>, gone: bool) -> ScriptStep {
    if let Some(name) = name.filter(|n| !n.is_empty()) {
        return drop_constraint_step(table, ObjectKind::Table, name, gone);
    }
    let t = table_sql(table);
    let step = ScriptStep::new(format!(
        "DECLARE @pk sysname = (SELECT name FROM sys.key_constraints WHERE parent_object_id = \
         OBJECT_ID({}) AND type = 'PK'); IF @pk IS NOT NULL EXEC (N'ALTER TABLE ' + {} + N' \
         DROP CONSTRAINT ' + QUOTENAME(@pk))",
        lit(&t),
        lit(&t)
    ))
    .described("Drop the primary key");
    if gone { step.destructive() } else { step }
}

/// Drops the default constraint of `column`: by name, else looked up in
/// `sys.default_constraints` (nothing happens when there is none).
fn drop_default_step(table: &ObjectRef, column: &str, name: Option<&str>) -> ScriptStep {
    if let Some(name) = name.filter(|n| !n.is_empty()) {
        return ScriptStep::new(format!(
            "ALTER TABLE {} DROP CONSTRAINT {}",
            table_sql(table),
            ident(name)
        ))
        .described(format!("Drop the default of column {column}"));
    }
    let t = table_sql(table);
    ScriptStep::new(format!(
        "DECLARE @df sysname = (SELECT d.name FROM sys.default_constraints AS d JOIN sys.columns \
         AS c ON c.object_id = d.parent_object_id AND c.column_id = d.parent_column_id WHERE \
         d.parent_object_id = OBJECT_ID({}) AND c.name = {}); IF @df IS NOT NULL EXEC (N'ALTER \
         TABLE ' + {} + N' DROP CONSTRAINT ' + QUOTENAME(@df))",
        lit(&t),
        lit(column),
        lit(&t)
    ))
    .described(format!("Drop the default of column {column}"))
}

fn add_default_step(table: &ObjectRef, c: &ColumnDef, name: &str) -> Result<ScriptStep> {
    check_name("default constraint", name)?;
    let default = c.default.as_deref().unwrap_or("NULL");
    let expr = checked(&format!("default of column {}", c.name), default)?;
    Ok(ScriptStep::new(format!(
        "ALTER TABLE {} ADD CONSTRAINT {} DEFAULT ({expr}) FOR {}",
        table_sql(table),
        ident(name),
        ident(&c.name)
    ))
    .described(format!("Set the default of column {}", c.name)))
}

fn drop_column_step(table: &ObjectRef, column: &str) -> ScriptStep {
    ScriptStep::new(format!(
        "ALTER TABLE {} DROP COLUMN {}",
        table_sql(table),
        ident(column)
    ))
    .described(format!("Drop column {column}"))
    .destructive()
}

fn fk_action_sql(fk: &ForeignKeyDef, action: FkAction) -> Result<Option<&'static str>> {
    Ok(match action {
        FkAction::NoAction => None,
        FkAction::Cascade => Some("CASCADE"),
        FkAction::SetNull => Some("SET NULL"),
        FkAction::SetDefault => Some("SET DEFAULT"),
        FkAction::Restrict => {
            return Err(Error::unsupported(format!(
                "foreign key {}: SQL Server has no RESTRICT action; use NO ACTION",
                fk.name.as_deref().unwrap_or("(unnamed)")
            )));
        }
    })
}

fn fk_sql(table: &ObjectRef, fk: &ForeignKeyDef) -> Result<String> {
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
    if let (Some(db), Some(other)) = (&table.catalog, &fk.ref_table.catalog)
        && db != other
    {
        return Err(Error::unsupported(format!(
            "foreign key {label}: SQL Server cannot reference a table of another database"
        )));
    }
    let target = ObjectRef::table(
        None,
        fk.ref_table.schema.as_deref().or(table.schema.as_deref()),
        fk.ref_table.name.as_str(),
    );
    let mut s = String::new();
    if let Some(name) = &fk.name {
        check_name("foreign key", name)?;
        let _ = write!(s, "CONSTRAINT {} ", ident(name));
    }
    let _ = write!(
        s,
        "FOREIGN KEY ({}) REFERENCES {}",
        column_list(&fk.columns),
        table_sql(&target)
    );
    if !fk.ref_columns.is_empty() {
        let _ = write!(s, " ({})", column_list(&fk.ref_columns));
    }
    if let Some(action) = fk_action_sql(fk, fk.on_delete)? {
        let _ = write!(s, " ON DELETE {action}");
    }
    if let Some(action) = fk_action_sql(fk, fk.on_update)? {
        let _ = write!(s, " ON UPDATE {action}");
    }
    if flag(&fk.extras, &extras::NOT_FOR_REPLICATION) {
        s.push_str(" NOT FOR REPLICATION");
    }
    Ok(s)
}

/// `ALTER TABLE [s].[t] [WITH NOCHECK] ADD ...` (untrusted constraints are added without
/// checking the rows), then `NOCHECK CONSTRAINT` when the constraint is disabled.
fn constraint_steps(
    table: &ObjectRef,
    body: &str,
    extras_of: &Extras,
    name: Option<&str>,
    description: String,
) -> Result<Vec<ScriptStep>> {
    let nocheck = flag(extras_of, &extras::NOT_TRUSTED) || flag(extras_of, &extras::DISABLED);
    let mut steps = vec![
        ScriptStep::new(format!(
            "ALTER TABLE {}{} ADD {body}",
            table_sql(table),
            if nocheck { " WITH NOCHECK" } else { "" }
        ))
        .described(description),
    ];
    if flag(extras_of, &extras::DISABLED) {
        let name = name.ok_or_else(|| {
            Error::invalid_input("a disabled constraint needs a name to be disabled")
        })?;
        steps.push(
            ScriptStep::new(format!(
                "ALTER TABLE {} NOCHECK CONSTRAINT {}",
                table_sql(table),
                ident(name)
            ))
            .described(format!("Disable constraint {name}")),
        );
    }
    Ok(steps)
}

fn fk_steps(table: &ObjectRef, fk: &ForeignKeyDef) -> Result<Vec<ScriptStep>> {
    let description = match &fk.name {
        Some(name) => format!("Add foreign key {name}"),
        None => format!("Add foreign key on {}", fk.columns.join(", ")),
    };
    constraint_steps(
        table,
        &fk_sql(table, fk)?,
        &fk.extras,
        fk.name.as_deref(),
        description,
    )
}

fn check_sql(check: &CheckDef) -> Result<String> {
    let label = check.name.as_deref().unwrap_or("(unnamed)");
    let expr = checked(&format!("check {label}"), &check.expression)?;
    let mut s = String::new();
    if let Some(name) = &check.name {
        check_name("check", name)?;
        let _ = write!(s, "CONSTRAINT {} ", ident(name));
    }
    s.push_str("CHECK ");
    if flag(&check.extras, &extras::NOT_FOR_REPLICATION) {
        s.push_str("NOT FOR REPLICATION ");
    }
    let _ = write!(s, "({expr})");
    Ok(s)
}

fn check_steps(table: &ObjectRef, check: &CheckDef) -> Result<Vec<ScriptStep>> {
    let description = match &check.name {
        Some(name) => format!("Add check {name}"),
        None => "Add check".to_owned(),
    };
    constraint_steps(
        table,
        &check_sql(check)?,
        &check.extras,
        check.name.as_deref(),
        description,
    )
}

// --- comments and renames -----------------------------------------------------------------------

/// The extended-property procedure of a comment change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proc {
    Add,
    Update,
    Drop,
}

/// `EXEC sys.sp_{add,update,drop}extendedproperty @name = N'MS_Description', ...` for the
/// table, or one of its columns.
fn comment_step(
    proc: Proc,
    table: &ObjectRef,
    column: Option<&str>,
    text: Option<&str>,
) -> ScriptStep {
    let name = match proc {
        Proc::Add => "sp_addextendedproperty",
        Proc::Update => "sp_updateextendedproperty",
        Proc::Drop => "sp_dropextendedproperty",
    };
    let mut sql = format!("EXEC sys.{name} @name = N'MS_Description'");
    if proc != Proc::Drop {
        let _ = write!(sql, ", @value = {}", lit(text.unwrap_or("")));
    }
    let _ = write!(
        sql,
        ", @level0type = N'SCHEMA', @level0name = {}, @level1type = N'TABLE', @level1name = {}",
        lit(schema_name(table)),
        lit(&table.name)
    );
    if let Some(column) = column {
        let _ = write!(
            sql,
            ", @level2type = N'COLUMN', @level2name = {}",
            lit(column)
        );
    }
    let what = column.map_or_else(
        || format!("table {}", table.name),
        |c| format!("column {c}"),
    );
    ScriptStep::new(sql).described(if proc == Proc::Drop {
        format!("Remove the comment of {what}")
    } else {
        format!("Set the comment of {what}")
    })
}

/// The step that turns comment `old` (as on the server) into `new`, if they differ.
fn comment_change(
    table: &ObjectRef,
    column: Option<&str>,
    old: Option<&str>,
    new: Option<&str>,
) -> Option<ScriptStep> {
    let (before, after) = (comment_text(old), comment_text(new));
    if before == after {
        return None;
    }
    Some(match (old.is_some(), after) {
        (false, Some(text)) => comment_step(Proc::Add, table, column, Some(text)),
        (true, Some(text)) => comment_step(Proc::Update, table, column, Some(text)),
        (_, None) => comment_step(Proc::Drop, table, column, None),
    })
}

fn rename_step(first: &str, to: &str, kind: Option<&str>, what: String) -> ScriptStep {
    let mut sql = format!("EXEC sp_rename {}, {}", lit(first), lit(to));
    if let Some(kind) = kind {
        let _ = write!(sql, ", N'{kind}'");
    }
    ScriptStep::new(sql).described(what)
}

fn rename_column_step(table: &ObjectRef, from: &str, to: &str) -> ScriptStep {
    rename_step(
        &format!("{}.{}", table_sql(table), ident(from)),
        to,
        Some("COLUMN"),
        format!("Rename column {from} to {to}"),
    )
}

fn rename_index_step(table: &ObjectRef, from: &str, to: &str, constraint: bool) -> ScriptStep {
    if constraint {
        rename_object_step(table, from, to)
    } else {
        rename_step(
            &format!("{}.{}", table_sql(table), ident(from)),
            to,
            Some("INDEX"),
            format!("Rename index {from} to {to}"),
        )
    }
}

fn rename_object_step(table: &ObjectRef, from: &str, to: &str) -> ScriptStep {
    rename_step(
        &object_sql(table, from),
        to,
        Some("OBJECT"),
        format!("Rename constraint {from} to {to}"),
    )
}

// --- create table -------------------------------------------------------------------------------

/// Table kinds the designer does not change in M4.
fn refuse_special(t: &TableDef) -> Result<()> {
    let name = &t.obj.name;
    if text_extra(&t.extras, &extras::TEMPORAL_TYPE).is_some_and(|kind| {
        matches!(
            kind.to_ascii_lowercase().as_str(),
            "system_versioned" | "history"
        )
    }) {
        return Err(Error::unsupported(format!(
            "table {name} is a system-versioned (temporal) or history table; the designer cannot \
             change it in M4"
        )));
    }
    if flag(&t.extras, &extras::MEMORY_OPTIMIZED) {
        return Err(Error::unsupported(format!(
            "table {name} is memory-optimized; the designer cannot change it in M4"
        )));
    }
    if let Some(kind) = text_extra(&t.extras, &props::TABLE_TYPE)
        && matches!(
            kind.to_ascii_lowercase().as_str(),
            "graph_node" | "graph_edge" | "ledger"
        )
    {
        return Err(Error::unsupported(format!(
            "table {name} is a {kind} table; the designer cannot change it in M4"
        )));
    }
    Ok(())
}

fn table_compression(t: &TableDef) -> String {
    text_extra(&t.extras, &extras::DATA_COMPRESSION)
        .unwrap_or("NONE")
        .to_ascii_uppercase()
}

fn create_table(v: &ServerVersion, t: &TableDef) -> Result<Script> {
    check_name("table", &t.obj.name)?;
    refuse_special(t)?;
    let table = &t.obj;
    let mut lines = Vec::new();
    for c in &t.columns {
        lines.push(column_sql(v, table, c)?);
    }
    if let Some(pk) = &t.primary_key {
        lines.push(pk_sql(pk, pk_clustered(t), &pk_name(table, pk, None))?);
    }
    for idx in t.indexes.iter().filter(|i| i.is_constraint) {
        lines.push(unique_sql(idx)?);
    }
    // Untrusted or disabled checks are added after the table, so their flags can be written.
    let later =
        |c: &CheckDef| flag(&c.extras, &extras::NOT_TRUSTED) || flag(&c.extras, &extras::DISABLED);
    for check in t.checks.iter().filter(|c| !later(c)) {
        lines.push(check_sql(check)?);
    }
    if lines.is_empty() {
        return Err(Error::invalid_input("a table needs at least one column"));
    }
    let mut sql = format!(
        "CREATE TABLE {} (\n    {}\n)",
        table_sql(table),
        lines.join(",\n    ")
    );
    match table_compression(t).as_str() {
        "NONE" => {}
        c @ ("ROW" | "PAGE") => {
            let _ = write!(sql, " WITH (DATA_COMPRESSION = {c})");
        }
        other => {
            return Err(Error::unsupported(format!(
                "data compression {other} cannot be written by the designer"
            )));
        }
    }
    let mut s = Script::new();
    s.push_step(ScriptStep::new(sql).described(format!("Create table {}", table.name)));
    for check in t.checks.iter().filter(|c| later(c)) {
        s.extend(check_steps(table, check)?);
    }
    let mut indexes: Vec<&IndexDef> = t.indexes.iter().filter(|i| !i.is_constraint).collect();
    indexes.sort_by_key(|i| !index_clustered(i));
    for idx in indexes {
        s.extend(index_steps(table, idx)?);
    }
    for idx in t.indexes.iter().filter(|i| i.is_constraint) {
        if flag(&idx.extras, &extras::DISABLED) {
            s.push_step(
                ScriptStep::new(format!(
                    "ALTER INDEX {} ON {} DISABLE",
                    ident(&idx.name),
                    table_sql(table)
                ))
                .described(format!("Disable index {}", idx.name)),
            );
        }
    }
    for fk in &t.foreign_keys {
        s.extend(fk_steps(table, fk)?);
    }
    if let Some(text) = comment_text(t.comment.as_deref()) {
        s.push_step(comment_step(Proc::Add, table, None, Some(text)));
    }
    for c in &t.columns {
        if let Some(text) = comment_text(c.comment.as_deref()) {
            s.push_step(comment_step(Proc::Add, table, Some(&c.name), Some(text)));
        }
    }
    Ok(s)
}

// --- child kinds through the table ------------------------------------------------------------

/// `table` with child `old` replaced by `new` (added when `old` is `None`, removed when `new`
/// is `None`), matched by kind and name.
fn replace_child(table: &TableDef, old: Option<&ObjectDef>, new: Option<&ObjectDef>) -> TableDef {
    fn put<T>(items: &mut Vec<T>, pos: Option<usize>, new: Option<T>) {
        match (pos, new) {
            (Some(i), Some(n)) => items[i] = n,
            (Some(i), None) => {
                items.remove(i);
            }
            (None, Some(n)) => items.push(n),
            (None, None) => {}
        }
    }
    let mut t = table.clone();
    let kind = new.or(old).map(ObjectDef::kind);
    let old_name = old.map(ObjectDef::name);
    match kind {
        Some(ObjectKind::Column) => {
            let pos = old_name.and_then(|o| t.columns.iter().position(|c| c.name == o));
            let new = new.and_then(|d| match d {
                ObjectDef::Column { column, .. } => Some(column.clone()),
                _ => None,
            });
            put(&mut t.columns, pos, new);
        }
        Some(ObjectKind::Index) => {
            let pos = old_name.and_then(|o| t.indexes.iter().position(|i| i.name == o));
            let new = new.and_then(|d| match d {
                ObjectDef::Index { index, .. } => Some(index.clone()),
                _ => None,
            });
            put(&mut t.indexes, pos, new);
        }
        Some(ObjectKind::ForeignKey) => {
            let pos = old_name.and_then(|o| {
                t.foreign_keys
                    .iter()
                    .position(|f| f.name.as_deref() == Some(o))
            });
            let new = new.and_then(|d| match d {
                ObjectDef::ForeignKey { foreign_key, .. } => Some(foreign_key.clone()),
                _ => None,
            });
            put(&mut t.foreign_keys, pos, new);
        }
        Some(ObjectKind::Check) => {
            let pos =
                old_name.and_then(|o| t.checks.iter().position(|c| c.name.as_deref() == Some(o)));
            let new = new.and_then(|d| match d {
                ObjectDef::Check { check, .. } => Some(check.clone()),
                _ => None,
            });
            put(&mut t.checks, pos, new);
        }
        _ => {}
    }
    t
}

/// `table` without column `name` and without the keys, indexes and checks that use it.
fn without_column(table: &TableDef, name: &str) -> TableDef {
    let gone: HashSet<String> = std::iter::once(name.to_owned()).collect();
    let mut t = table.clone();
    t.columns.retain(|c| c.name != name);
    if t.primary_key
        .as_ref()
        .is_some_and(|pk| pk.columns.iter().any(|c| c == name))
    {
        t.primary_key = None;
    }
    t.indexes.retain(|i| !index_uses(i, &gone));
    t.checks.retain(|c| !check_uses(c, &gone));
    let obj = t.obj.clone();
    t.foreign_keys.retain(|f| !fk_uses(f, &obj, &gone));
    t
}

// --- alter table --------------------------------------------------------------------------------

/// Renames to run for one kind, through temporary names when they overlap (`a → b`, `b → a`).
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
            let what = kind.display_name().to_lowercase();
            return Err(Error::invalid_input(format!(
                "cannot rename {what} {}: the table has no such {what}",
                r.old
            )));
        }
        check_name(&kind.display_name().to_lowercase(), &r.new)?;
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

fn rename_refs(names: &mut [String], pairs: &[(String, String)]) {
    for name in names {
        if let Some(new) = renamed(pairs, name) {
            new.clone_into(name);
        }
    }
}

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

fn same_table(a: &ObjectRef, b: &ObjectRef) -> bool {
    a.name == b.name
        && match (a.schema.as_deref(), b.schema.as_deref()) {
            (Some(x), Some(y)) => x == y,
            _ => true,
        }
}

/// Emits the schema move and every rename; returns `old` as it is after them.
fn apply_renames(
    old: &TableDef,
    new: &TableDef,
    ctx: &DdlContext<'_>,
    s: &mut Script,
) -> Result<(TableDef, Vec<(String, String)>)> {
    let taken = taken_names(old, new);
    let mut cur = old.clone();
    let mut at = old.obj.clone();
    if old.obj.schema != new.obj.schema {
        let Some(schema) = &new.obj.schema else {
            return Err(Error::invalid_input("the new table has no schema"));
        };
        s.push_step(
            ScriptStep::new(format!(
                "ALTER SCHEMA {} TRANSFER {}",
                ident(schema),
                table_sql(&at)
            ))
            .described(format!("Move table {} to schema {schema}", at.name)),
        );
        at.schema = Some(schema.clone());
    }
    if at.name != new.obj.name {
        check_name("table", &new.obj.name)?;
        s.push_step(rename_step(
            &table_sql(&at),
            &new.obj.name,
            None,
            format!("Rename table {} to {}", at.name, new.obj.name),
        ));
    }
    let table = &new.obj;
    for fk in &mut cur.foreign_keys {
        if same_table(&fk.ref_table, &old.obj) {
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
        rename_refs(&mut pk.columns, &columns);
    }
    for idx in &mut cur.indexes {
        for part in idx.columns.iter_mut().filter(|p| !p.is_expression) {
            if let Some(to) = renamed(&columns, &part.name) {
                to.clone_into(&mut part.name);
            }
        }
        if let Ok(mut include) = include_columns(idx)
            && !include.is_empty()
        {
            rename_refs(&mut include, &columns);
            idx.extras.insert(
                props::INCLUDE_COLUMNS,
                Value::Json(serde_json::Value::from(include)),
            );
        }
    }
    for fk in &mut cur.foreign_keys {
        rename_refs(&mut fk.columns, &columns);
        if same_table(&fk.ref_table, table) {
            rename_refs(&mut fk.ref_columns, &columns);
        }
    }
    let indexes = renames_of(ctx, ObjectKind::Index, |n| {
        old.indexes.iter().any(|i| i.name == n)
    })?;
    // Step k renames index k (modulo the count when temporary names are used).
    for (k, (from, to)) in rename_plan(&indexes, &taken).iter().enumerate() {
        let original = &indexes[k % indexes.len()].0;
        let constraint = old
            .indexes
            .iter()
            .any(|i| i.name == *original && i.is_constraint);
        s.push_step(rename_index_step(table, from, to, constraint));
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
        s.push_step(rename_object_step(table, &from, &to));
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
        check_name("primary key", to)?;
        s.push_step(rename_object_step(table, from, to));
        before.name = Some(to.clone());
    }
    // Default constraints that only changed their name.
    for col in &mut cur.columns {
        let Some(after) = new.column(&col.name) else {
            continue;
        };
        if let (Some(from), Some(to)) = (
            text_extra(&col.extras, &extras::DEFAULT_CONSTRAINT),
            text_extra(&after.extras, &extras::DEFAULT_CONSTRAINT),
        ) && from != to
            && col.default.is_some()
            && after.default.is_some()
        {
            check_name("default constraint", to)?;
            s.push_step(rename_object_step(table, from, to));
            let to = to.to_owned();
            col.extras.insert(extras::DEFAULT_CONSTRAINT, to);
        }
    }
    Ok((cur, columns))
}

/// Key, included and filter columns of `idx` that are in `names`.
fn index_uses(idx: &IndexDef, names: &HashSet<String>) -> bool {
    idx.columns
        .iter()
        .any(|p| !p.is_expression && names.contains(&p.name))
        || include_columns(idx).is_ok_and(|inc| inc.iter().any(|c| names.contains(c)))
        || idx
            .predicate
            .as_deref()
            .is_some_and(|p| names.iter().any(|n| mentions_column(p, n)))
}

fn check_uses(check: &CheckDef, names: &HashSet<String>) -> bool {
    names.iter().any(|n| mentions_column(&check.expression, n))
}

fn fk_uses(fk: &ForeignKeyDef, table: &ObjectRef, names: &HashSet<String>) -> bool {
    fk.columns.iter().any(|c| names.contains(c))
        || (same_table(&fk.ref_table, table) && fk.ref_columns.iter().any(|c| names.contains(c)))
}

/// What happens to one kept column.
#[derive(Debug, Default)]
struct ColumnChange {
    /// `ALTER COLUMN` runs.
    alter: bool,
    /// Indexes, unique constraints and checks on it may stay (only a variable length grows).
    keeps_indexes: bool,
    /// Its default constraint may stay (the type stays or only its size grows).
    keeps_default: bool,
    /// The default changed.
    default_changed: bool,
    /// A computed column dropped and added again.
    readd: bool,
    /// The type change can lose data.
    narrowing: bool,
}

fn classify(v: &ServerVersion, before: &ColumnDef, after: &ColumnDef) -> Result<ColumnChange> {
    let rules = MsSqlRules;
    let name = &after.name;
    validate_column(v, after).or_else(|e| match e {
        // A period column that does not change is fine; the caller refuses changes.
        Error::Unsupported { .. } if is_period(before) => Ok(()),
        other => Err(other),
    })?;
    let unsupported = |why: &str| Err(Error::unsupported(format!("column {name}: {why}")));
    match (identity_of(before), identity_of(after)) {
        (None, Some(_)) | (Some(_), None) => {
            return unsupported(
                "SQL Server cannot add or remove IDENTITY on an existing column; add a new \
                 column or recreate the table",
            );
        }
        (Some(a), Some(b)) => {
            let differs =
                |x: Option<i128>, y: Option<i128>| matches!((x, y), (Some(x), Some(y)) if x != y);
            if differs(a.start, b.start) || differs(a.increment, b.increment) {
                return unsupported(
                    "SQL Server cannot change the seed or increment of an IDENTITY column; add \
                     a new column or recreate the table",
                );
            }
        }
        (None, None) => {}
    }
    if flag(&before.extras, &extras::SPARSE) != flag(&after.extras, &extras::SPARSE) {
        return unsupported("SPARSE cannot be changed by the designer in M4");
    }
    let mut change = ColumnChange {
        keeps_indexes: true,
        keeps_default: true,
        ..ColumnChange::default()
    };
    if before.generated.is_some() || after.generated.is_some() {
        let same = match (&before.generated, &after.generated) {
            (Some(a), Some(b)) => {
                a.stored == b.stored && rules.same_expression(&a.expression, &b.expression)
            }
            _ => false,
        };
        change.readd = !same
            || (after.generated.as_ref().is_some_and(|g| g.stored)
                && before.nullable != after.nullable);
        return Ok(change);
    }
    let type_changed = !rules.same_type(before, after);
    let null_changed = before.nullable != after.nullable;
    let collation_changed = before.collation != after.collation;
    let changed = type_changed || null_changed || collation_changed;
    if is_period(before) || is_period(after) {
        if changed || !rules.same_default(before, after) {
            return unsupported("period columns cannot be changed by the designer in M4");
        }
        return Ok(change);
    }
    if (is_rowversion(v, before) || is_rowversion(v, after)) && changed {
        return unsupported("a rowversion column cannot be altered; drop it and add a new column");
    }
    let (old_type, new_type) = (type_key(v, before), type_key(v, after));
    if type_changed {
        let (ob, _) = split_type(&old_type);
        let successor = match ob {
            "text" => Some(["varchar(max)", "nvarchar(max)"].as_slice()),
            "ntext" => Some(["nvarchar(max)"].as_slice()),
            "image" => Some(["varbinary(max)"].as_slice()),
            _ => None,
        };
        if let Some(allowed) = successor
            && !allowed.contains(&new_type.as_str())
        {
            return unsupported(&format!("{ob} can only change to {}", allowed.join(" or ")));
        }
    }
    let (ob, _) = split_type(&old_type);
    let (nb, na) = split_type(&new_type);
    let grows = ob == nb && is_widening(&old_type, &new_type);
    change.alter = changed;
    change.narrowing = type_changed && !is_widening(&old_type, &new_type);
    change.keeps_default = (!type_changed || grows) && !collation_changed;
    change.keeps_indexes = !changed
        || (!null_changed
            && !collation_changed
            && grows
            && matches!(nb, "varchar" | "nvarchar" | "varbinary")
            && na.first() != Some(&"max"));
    change.default_changed = !rules.same_default(before, after);
    Ok(change)
}

/// `ALTER TABLE ... ALTER COLUMN [c] <type> [COLLATE x] NULL|NOT NULL [SPARSE]`.
fn alter_column_step(
    v: &ServerVersion,
    table: &ObjectRef,
    c: &ColumnDef,
    narrowing: bool,
) -> Result<ScriptStep> {
    let ty = type_sql(v, c)?;
    let mut sql = format!(
        "ALTER TABLE {} ALTER COLUMN {} {ty}",
        table_sql(table),
        ident(&c.name)
    );
    if let Some(collation) = &c.collation {
        sql.push_str(" COLLATE ");
        sql.push_str(&checked_collation(&c.name, collation)?);
    }
    sql.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
    if flag(&c.extras, &extras::SPARSE) {
        sql.push_str(" SPARSE");
    }
    let step = ScriptStep::new(sql).described(format!("Change column {} to {ty}", c.name));
    Ok(if narrowing { step.destructive() } else { step })
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

fn index_changed(table: &ObjectRef, a: &IndexDef, b: &IndexDef) -> bool {
    let (mut a, mut b) = (a.clone(), b.clone());
    normalize_index(&mut a);
    normalize_index(&mut b);
    !diff_indexes(table, &a, &b, &MsSqlRules).is_empty()
}

fn alter_table(
    v: &ServerVersion,
    old: &TableDef,
    new: &TableDef,
    ctx: &DdlContext<'_>,
) -> Result<Script> {
    refuse_special(old)?;
    refuse_special(new)?;
    if table_compression(old) != table_compression(new) {
        return Err(Error::unsupported(
            "data compression changes are not supported by the designer in M4",
        ));
    }
    let mut s = Script::new();
    let (cur, column_renames) = apply_renames(old, new, ctx, &mut s)?;
    let table = &new.obj;

    // SQL Server cannot reorder columns.
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
            "SQL Server cannot reorder columns: keep the existing columns in their order and add \
             new columns at the end",
        ));
    }

    // Columns.
    let dropped: Vec<&ColumnDef> = cur
        .columns
        .iter()
        .filter(|c| new.column(&c.name).is_none())
        .collect();
    let mut changes: Vec<(&ColumnDef, &ColumnDef, ColumnChange)> = Vec::new();
    for col in &new.columns {
        if let Some(before) = cur.column(&col.name) {
            changes.push((before, col, classify(v, before, col)?));
        }
    }
    let mut altered: HashSet<String> = changes
        .iter()
        .filter(|(_, _, c)| c.alter)
        .map(|(_, a, _)| a.name.clone())
        .collect();
    let renamed_altered: Vec<String> = column_renames
        .iter()
        .filter(|(_, to)| altered.contains(to))
        .map(|(from, _)| from.clone())
        .collect();
    altered.extend(renamed_altered);
    // Computed columns that reference an altered column go and come back.
    for (before, _, change) in &mut changes {
        if let Some(g) = &before.generated
            && altered.iter().any(|n| mentions_column(&g.expression, n))
        {
            change.readd = true;
        }
    }
    let readd: HashSet<String> = changes
        .iter()
        .filter(|(_, _, c)| c.readd)
        .map(|(_, a, _)| a.name.clone())
        .collect();
    let gone: HashSet<String> = dropped.iter().map(|c| c.name.clone()).collect();
    // Columns whose indexes, unique constraints and checks must go first.
    let mut touched: HashSet<String> = changes
        .iter()
        .filter(|(_, _, c)| c.alter && !c.keeps_indexes)
        .map(|(_, a, _)| a.name.clone())
        .collect();
    touched.extend(readd.iter().cloned());
    touched.extend(gone.iter().cloned());
    // Columns whose primary key and foreign keys must go first.
    let mut keyed = altered.clone();
    keyed.extend(readd.iter().cloned());
    keyed.extend(gone.iter().cloned());
    // Expressions (checks, filters, computed columns) of `cur` still name renamed columns by
    // their old names.
    for set in [&mut touched, &mut keyed] {
        let olds: Vec<String> = column_renames
            .iter()
            .filter(|(_, to)| set.contains(to))
            .map(|(from, _)| from.clone())
            .collect();
        set.extend(olds);
    }
    for name in &readd {
        s.warn(format!(
            "Computed column {name} is dropped and added again: it moves to the end of the table."
        ));
    }

    // Indexes.
    let mut index_drops: Vec<(&IndexDef, bool)> = Vec::new();
    let mut index_adds: Vec<&IndexDef> = Vec::new();
    for idx in &cur.indexes {
        match new.indexes.iter().find(|i| i.name == idx.name) {
            None => index_drops.push((idx, true)),
            Some(after) => {
                // A filter that names an altered column blocks it in every case.
                let blocked = index_uses(idx, &touched)
                    || idx
                        .predicate
                        .as_deref()
                        .is_some_and(|p| altered.iter().any(|n| mentions_column(p, n)));
                if index_changed(table, idx, after) || blocked {
                    index_drops.push((idx, false));
                    index_adds.push(after);
                }
            }
        }
    }
    for idx in &new.indexes {
        if !cur.indexes.iter().any(|i| i.name == idx.name) {
            index_adds.push(idx);
        }
    }
    for idx in &index_adds {
        plain_kind(idx)?;
    }

    // Primary key.
    let pk_changed = !same_pk(cur.primary_key.as_ref(), new.primary_key.as_ref())
        || (cur.primary_key.is_some()
            && new.primary_key.is_some()
            && pk_clustered(&cur) != pk_clustered(new));
    let pk_blocked = cur
        .primary_key
        .as_ref()
        .is_some_and(|pk| pk.columns.iter().any(|c| keyed.contains(c)));
    let pk_drop = cur.primary_key.is_some() && (pk_changed || pk_blocked);
    let pk_add = new.primary_key.is_some() && (pk_changed || pk_blocked);
    if pk_drop {
        s.warn(
            "The primary key is dropped and added again: SQL Server refuses this while foreign \
             keys of other tables reference it; drop those foreign keys first.",
        );
    }
    // Keys dropped and added again, which foreign keys of the table to itself reference.
    let mut dropped_keys: Vec<Vec<String>> = Vec::new();
    if pk_drop && let Some(pk) = &cur.primary_key {
        dropped_keys.push(pk.columns.clone());
    }
    for (idx, _) in &index_drops {
        if idx.unique {
            dropped_keys.push(idx.columns.iter().map(|c| c.name.clone()).collect());
        }
    }
    let old_pk: Vec<String> = cur
        .primary_key
        .as_ref()
        .map(|pk| pk.columns.clone())
        .unwrap_or_default();
    let depends_on_dropped = |fk: &ForeignKeyDef| {
        let refs = if fk.ref_columns.is_empty() {
            &old_pk
        } else {
            &fk.ref_columns
        };
        same_table(&fk.ref_table, table)
            && dropped_keys.iter().any(|key| {
                let mut a: Vec<&String> = key.iter().collect();
                let mut b: Vec<&String> = refs.iter().collect();
                a.sort_unstable();
                b.sort_unstable();
                a == b
            })
    };

    // Foreign keys.
    let fk_match = pair_items(
        &cur.foreign_keys,
        &new.foreign_keys,
        |fk| fk.name.as_deref(),
        |a, b| {
            a.columns == b.columns
                && a.ref_columns == b.ref_columns
                && MsSqlRules.same_table_ref(&a.ref_table, &b.ref_table)
        },
    );
    let mut fk_drops: Vec<ScriptStep> = Vec::new();
    let mut fk_adds: Vec<&ForeignKeyDef> = Vec::new();
    for (k, fk) in cur.foreign_keys.iter().enumerate() {
        if !fk_match.contains(&Some(k)) {
            let name = fk.name.as_deref().ok_or_else(|| {
                Error::invalid_input("an unnamed foreign key cannot be dropped by name")
            })?;
            fk_drops.push(drop_constraint_step(
                table,
                ObjectKind::ForeignKey,
                name,
                true,
            ));
        }
    }
    for (fk, found) in new.foreign_keys.iter().zip(&fk_match) {
        match found {
            None => fk_adds.push(fk),
            Some(k) => {
                let before = &cur.foreign_keys[*k];
                let changed = !diff_foreign_keys(table, before, fk, &MsSqlRules).is_empty();
                if changed || fk_uses(before, table, &keyed) || depends_on_dropped(before) {
                    let name = before.name.as_deref().ok_or_else(|| {
                        Error::invalid_input("an unnamed foreign key cannot be changed")
                    })?;
                    fk_drops.push(drop_constraint_step(
                        table,
                        ObjectKind::ForeignKey,
                        name,
                        false,
                    ));
                    fk_adds.push(fk);
                }
            }
        }
    }

    // Checks.
    let check_match = pair_items(
        &cur.checks,
        &new.checks,
        |c| c.name.as_deref(),
        |a, b| MsSqlRules.same_expression(&a.expression, &b.expression),
    );
    let mut check_drops: Vec<ScriptStep> = Vec::new();
    let mut check_adds: Vec<&CheckDef> = Vec::new();
    for (k, check) in cur.checks.iter().enumerate() {
        if !check_match.contains(&Some(k)) {
            let name = check.name.as_deref().ok_or_else(|| {
                Error::invalid_input("an unnamed check cannot be dropped by name")
            })?;
            check_drops.push(drop_constraint_step(table, ObjectKind::Check, name, true));
        }
    }
    for (check, found) in new.checks.iter().zip(&check_match) {
        match found {
            None => check_adds.push(check),
            Some(k) => {
                let before = &cur.checks[*k];
                let changed = !diff_checks(table, before, check, &MsSqlRules).is_empty();
                if changed || check_uses(before, &touched) {
                    let name = before.name.as_deref().ok_or_else(|| {
                        Error::invalid_input("an unnamed check cannot be changed")
                    })?;
                    check_drops.push(drop_constraint_step(table, ObjectKind::Check, name, false));
                    check_adds.push(check);
                }
            }
        }
    }

    // Drops.
    s.extend(fk_drops);
    s.extend(check_drops);
    index_drops.sort_by_key(|(idx, _)| index_clustered(idx));
    for (idx, gone_for_good) in &index_drops {
        s.push_step(drop_index_step(table, idx, *gone_for_good)?);
    }
    if pk_drop && let Some(pk) = &cur.primary_key {
        s.push_step(drop_pk_step(table, pk.name.as_deref(), !pk_add));
    }
    let mut default_adds: Vec<(&ColumnDef, String)> = Vec::new();
    for (before, after, change) in &changes {
        if change.readd {
            continue;
        }
        let had = before.default.is_some() && identity_of(before).is_none();
        let drop = had && (change.default_changed || (change.alter && !change.keeps_default));
        if drop {
            s.push_step(drop_default_step(
                table,
                &before.name,
                text_extra(&before.extras, &extras::DEFAULT_CONSTRAINT),
            ));
        }
        if after.default.is_some() && (drop || (change.default_changed && !had)) {
            let name = text_extra(&after.extras, &extras::DEFAULT_CONSTRAINT)
                .or_else(|| text_extra(&before.extras, &extras::DEFAULT_CONSTRAINT))
                .filter(|n| !n.is_empty())
                .map_or_else(
                    || generated_name("DF", &[&table.name, &after.name]),
                    str::to_owned,
                );
            default_adds.push((after, name));
        }
    }
    for col in &dropped {
        if col.default.is_some() {
            s.push_step(drop_default_step(
                table,
                &col.name,
                text_extra(&col.extras, &extras::DEFAULT_CONSTRAINT),
            ));
        }
    }
    for col in &dropped {
        s.push_step(drop_column_step(table, &col.name));
    }
    for (before, _, change) in &changes {
        if change.readd {
            s.push_step(drop_column_step(table, &before.name));
        }
    }

    // Column changes and adds.
    for (_, after, change) in &changes {
        if change.alter && !change.readd {
            s.push_step(alter_column_step(v, table, after, change.narrowing)?);
        }
    }
    for col in new.columns.iter().filter(|c| cur.column(&c.name).is_none()) {
        s.push_step(
            ScriptStep::new(format!(
                "ALTER TABLE {} ADD {}",
                table_sql(table),
                column_sql(v, table, col)?
            ))
            .described(format!("Add column {}", col.name)),
        );
    }
    for (_, after, change) in &changes {
        if change.readd {
            s.push_step(
                ScriptStep::new(format!(
                    "ALTER TABLE {} ADD {}",
                    table_sql(table),
                    column_sql(v, table, after)?
                ))
                .described(format!("Add computed column {} again", after.name))
                .destructive(),
            );
        }
    }
    for (col, name) in &default_adds {
        s.push_step(add_default_step(table, col, name)?);
    }

    // Primary key, indexes, checks, foreign keys.
    if pk_add && let Some(pk) = &new.primary_key {
        let fallback = cur.primary_key.as_ref().and_then(|p| p.name.as_deref());
        let name = pk_name(table, pk, fallback);
        s.push_step(
            ScriptStep::new(format!(
                "ALTER TABLE {} ADD {}",
                table_sql(table),
                pk_sql(pk, pk_clustered(new), &name)?
            ))
            .described(format!("Add primary key ({})", pk.columns.join(", "))),
        );
    }
    index_adds.sort_by_key(|idx| !index_clustered(idx));
    for idx in index_adds {
        s.extend(index_steps(table, idx)?);
    }
    for check in check_adds {
        s.extend(check_steps(table, check)?);
    }
    for fk in fk_adds {
        s.extend(fk_steps(table, fk)?);
    }

    // Comments last.
    if let Some(step) = comment_change(table, None, cur.comment.as_deref(), new.comment.as_deref())
    {
        s.push_step(step);
    }
    for col in &new.columns {
        let before = cur
            .column(&col.name)
            .filter(|_| !readd.contains(&col.name))
            .and_then(|c| c.comment.as_deref());
        if let Some(step) = comment_change(table, Some(&col.name), before, col.comment.as_deref()) {
            s.push_step(step);
        }
    }
    Ok(s)
}
