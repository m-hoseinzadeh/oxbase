//! The SQLite [`DdlGenerator`] (M3), one generator for `Table`, `Column`, `Index`, `ForeignKey`
//! and `Check`.
//!
//! # In place or rebuilt
//!
//! SQLite changes a table in place only with `ALTER TABLE ... RENAME TO`, `RENAME COLUMN`
//! (3.25+), `ADD COLUMN` and `DROP COLUMN` (3.35+), and with `CREATE` / `DROP INDEX`. An `alter`
//! uses those when they can express the whole change:
//!
//! - `ADD COLUMN` only for a column SQLite accepts there: not in the primary key, a `UNIQUE`
//!   constraint or a foreign key, not a `STORED` generated column, a constant default (a number,
//!   a string or blob literal, `NULL`, `TRUE` / `FALSE`), a default when it is `NOT NULL`, and
//!   placed after every existing column;
//! - `DROP COLUMN` only for a plain column: not in the primary key, a `UNIQUE` constraint or a
//!   foreign key, not generated or used by a generated column, a check or a trigger (indexes on
//!   it are dropped first).
//!
//! Anything else (a column's type, nullability, default, collation or `AUTOINCREMENT`, the
//! column order, the primary key, `UNIQUE` constraints, foreign keys, checks, `WITHOUT ROWID` /
//! `STRICT`, or an add / drop SQLite refuses) **rebuilds** the table with SQLite's documented
//! procedure, as one script:
//!
//! ```text
//! prepare  PRAGMA foreign_keys                        capture "fk"
//! prepare  PRAGMA foreign_keys = OFF                  (no-op inside a transaction)
//! prepare  PRAGMA legacy_alter_table                  capture "legacy_alter"
//! main     renames (RENAME TO / RENAME COLUMN), when the design renamed things
//! main     CREATE TABLE "<db>"."<name>__oxbase_new" (the new definition)
//! main     INSERT INTO "<db>"."<name>__oxbase_new" (kept columns) SELECT ... FROM "<db>"."<name>"
//! main     (AUTOINCREMENT tables: carry the sqlite_sequence value over)
//! main     DROP TABLE "<db>"."<name>"
//! main     PRAGMA legacy_alter_table = ON             (views on the table are not re-checked)
//! main     ALTER TABLE "<db>"."<name>__oxbase_new" RENAME TO "<name>"
//! main     CREATE INDEX ... (every index of the new definition)
//! main     CREATE TRIGGER ... (every trigger of DdlContext::triggers, from its definition)
//! main     PRAGMA "<db>".foreign_key_check("<name>")  expect no rows, only if fk = "1"
//! finally  PRAGMA foreign_keys = ON                   only if fk = "1"
//! finally  PRAGMA legacy_alter_table = OFF            only if legacy_alter = "0"
//! ```
//!
//! Every main step of a rebuild is destructive. The main steps run in one transaction, so a
//! failure (a type that does not convert, a row breaking a new check, a foreign-key violation)
//! leaves the original table untouched. Foreign keys are off during the copy, so dropping the
//! old table neither cascades nor fails; enforcement is restored only if it was on. Views that
//! reference the table are not dropped (SQLite keeps them when the table is dropped) and work
//! again once the new table has the old name: `legacy_alter_table` is on for that rename only so
//! SQLite does not re-check those views while the table is briefly missing. Kept columns are
//! copied (with `CAST(col AS <new type>)` when the type changed), new columns get their default,
//! dropped columns are not copied, the rowid is kept for rowid tables, and generated columns
//! are computed again.
//!
//! **Renames** run first and natively (they also rewrite views, triggers and foreign keys that
//! use the old names); a rebuild then re-creates the table's triggers from the definitions it
//! was given, with the renamed table and column names replaced (the script carries a warning
//! asking to review them). Before 3.25 a column rename is done by the rebuild's copy instead.
//!
//! # Limits
//!
//! - **`UNIQUE` constraints** are re-declared by a rebuild (in their original order, so their
//!   `sqlite_autoindex_<table>_<n>` names stay), but cannot be added under a name: SQLite names
//!   them itself. Use a unique index ([`Error::Unsupported`] otherwise).
//! - **Index collations** are not in the model: a rebuild re-creates `col COLLATE x` keys
//!   without their collation.
//! - Comments, identity columns, index prefix lengths and virtual tables are
//!   [`Error::Unsupported`]. Foreign keys cannot leave the table's database.
//! - Child cells (`Column`, `ForeignKey`, `Check`, and `Index` on a `UNIQUE` constraint) need
//!   the table as it is now ([`DdlContext::table`]): they change it with a table `alter`.

use oxbase_dialect::compare::{CompareRules, diff_indexes, diff_tables};
use oxbase_dialect::{AnsiQuoting, DdlContext, DdlGenerator, Quoting};
use oxbase_types::{
    CheckDef, ColumnDef, DataType, DiffKind, Error, Extras, FkAction, ForeignKeyDef, IndexDef,
    ObjectDef, ObjectKind, ObjectRef, PrimaryKeyDef, PropId, PropertyDiff, Rename, Result, Script,
    ScriptStep, ServerVersion, StepPhase, TableDef, Value, props,
};

use super::compare::{SqliteRules, strip_outer_parens};
use super::parse;
use super::{AUTOINCREMENT, MODULE};

/// Suffix of the table a rebuild creates next to the old one (`<name>__oxbase_new`).
pub const REBUILD_SUFFIX: &str = "__oxbase_new";

/// Prefix of the indexes SQLite creates for `PRIMARY KEY` and `UNIQUE` constraints.
const AUTOINDEX_PREFIX: &str = "sqlite_autoindex_";

/// Words that would turn a type name into a column constraint.
const CONSTRAINT_WORDS: &[&str] = &[
    "AS",
    "CHECK",
    "COLLATE",
    "CONSTRAINT",
    "DEFAULT",
    "GENERATED",
    "NOT",
    "NULL",
    "ON",
    "PRIMARY",
    "REFERENCES",
    "UNIQUE",
];

/// The SQLite DDL generator. See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteDdl;

// --- small helpers -------------------------------------------------------------------------

fn ident(name: &str) -> String {
    AnsiQuoting.quote_ident(name)
}

fn literal(text: &str) -> String {
    AnsiQuoting.quote_str(text)
}

/// The SQLite database (schema) `obj` lives in: its catalog, or `main`.
fn schema_of(obj: &ObjectRef) -> &str {
    obj.catalog
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("main")
}

fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", ident(schema), ident(name))
}

fn flag(extras: &Extras, prop: &PropId) -> bool {
    matches!(extras.get(prop), Some(Value::Bool(true)))
}

fn is_virtual(t: &TableDef) -> bool {
    t.extras.contains(&MODULE)
        || matches!(t.extras.get(&props::TABLE_TYPE), Some(Value::Text(kind)) if kind == "virtual")
}

fn without_rowid(t: &TableDef) -> bool {
    flag(&t.extras, &props::WITHOUT_ROWID)
}

fn pk_columns(t: &TableDef) -> &[String] {
    t.primary_key
        .as_ref()
        .map(|pk| pk.columns.as_slice())
        .unwrap_or_default()
}

fn same_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn step(sql: String, description: String) -> ScriptStep {
    ScriptStep::new(sql).described(description)
}

fn unsupported_kind(kind: ObjectKind) -> Error {
    Error::unsupported(format!(
        "the SQLite DDL generator does not handle {}",
        kind.plural_display_name()
    ))
}

/// The number at the end of a `sqlite_autoindex_<table>_<n>` name.
fn autoindex_number(name: &str) -> Option<u32> {
    name.strip_prefix(AUTOINDEX_PREFIX)?
        .rsplit_once('_')?
        .1
        .parse()
        .ok()
}

// --- types, defaults and names -------------------------------------------------------------

fn sized(base: &str, length: Option<u32>) -> String {
    match length {
        Some(n) => format!("{base}({n})"),
        None => base.to_owned(),
    }
}

/// A declared type for a column built without a native type, chosen so the type SQLite reports
/// maps back to the same neutral type. `plain_int`: the column is the only primary-key column of
/// a rowid table and must not become a rowid alias (`INT`, not `INTEGER`).
pub(crate) fn derived_type(data_type: &DataType, plain_int: bool) -> String {
    match data_type {
        DataType::Bool => "BOOLEAN".to_owned(),
        DataType::Int {
            bits: 64,
            unsigned: true,
        } => "UNSIGNED BIG INT".to_owned(),
        DataType::Int { bits, .. } => match *bits {
            8 => "TINYINT",
            16 => "SMALLINT",
            32 => "INT4",
            _ if plain_int => "INT",
            _ => "INTEGER",
        }
        .to_owned(),
        DataType::Float { .. } => "REAL".to_owned(),
        DataType::Decimal {
            precision: Some(p),
            scale: Some(s),
        } => format!("DECIMAL({p},{s})"),
        DataType::Decimal {
            precision: Some(p),
            scale: None,
        } => format!("DECIMAL({p})"),
        DataType::Decimal { .. } => "NUMERIC".to_owned(),
        DataType::Char { length } => sized("CHAR", *length),
        DataType::VarChar { length } => sized("VARCHAR", *length),
        DataType::Text | DataType::Interval | DataType::Array { .. } | DataType::Enum { .. } => {
            "TEXT".to_owned()
        }
        DataType::Binary { .. } | DataType::VarBinary { .. } => "BLOB".to_owned(),
        DataType::Date => "DATE".to_owned(),
        DataType::Time { .. } => "TIME".to_owned(),
        DataType::Timestamp { .. } => "DATETIME".to_owned(),
        DataType::TimestampTz { .. } => "TIMESTAMPTZ".to_owned(),
        DataType::Uuid => "UUID".to_owned(),
        DataType::Json { binary: false } => "JSON".to_owned(),
        DataType::Json { binary: true } => "JSONB".to_owned(),
        DataType::Other { name } => name.trim().to_owned(),
    }
}

/// Checks that `type_name` is a plain SQLite type name (`VARCHAR(20)`, `UNSIGNED BIG INT`,
/// `DECIMAL(10, 2)`), so it cannot add constraints or end the column definition.
fn check_type_name(type_name: &str) -> Result<()> {
    let bad = || Error::invalid_input(format!("'{type_name}' is not a SQLite type name"));
    let (base, args) = match type_name.split_once('(') {
        Some((base, rest)) => (base, Some(rest.strip_suffix(')').ok_or_else(bad)?)),
        None => (type_name, None),
    };
    let base_ok = base
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && base
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ' ')
        && !base
            .split_whitespace()
            .any(|word| CONSTRAINT_WORDS.iter().any(|kw| same_name(kw, word)));
    let args_ok = args.is_none_or(|a| {
        !a.trim().is_empty()
            && a.chars()
                .all(|c| c.is_ascii_digit() || matches!(c, ' ' | ',' | '+' | '-' | '.'))
    });
    if base_ok && args_ok {
        Ok(())
    } else {
        Err(bad())
    }
}

/// The type a column is declared with (empty: no type).
fn type_sql(c: &ColumnDef, plain_int: bool) -> Result<String> {
    let native = c.native_type.trim();
    let text = if native.is_empty() {
        derived_type(&c.data_type, plain_int)
    } else {
        native.to_owned()
    };
    if !text.is_empty() {
        check_type_name(&text)?;
    }
    Ok(text)
}

fn collation_sql(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain { name.to_owned() } else { ident(name) }
}

/// `true` when `text` is one string or blob literal (`'it''s'`, `X'00ff'`).
fn is_string_literal(text: &str) -> bool {
    let body = text
        .strip_prefix(['x', 'X'])
        .filter(|rest| rest.starts_with('\''))
        .unwrap_or(text);
    let Some(inner) = body.strip_prefix('\'') else {
        return false;
    };
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
            } else {
                return chars.next().is_none();
            }
        }
    }
    false
}

/// `true` for a default `ALTER TABLE ... ADD COLUMN` accepts on a table with rows: a number, a
/// string or blob literal, `NULL`, `TRUE` or `FALSE` (optionally in parentheses).
fn is_constant_default(text: &str) -> bool {
    let bare = strip_outer_parens(text);
    let bare = bare.trim();
    let number = bare.strip_prefix(['-', '+']).unwrap_or(bare).trim_start();
    let is_number = number
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit() || c == '.')
        && number
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
    is_number
        || is_string_literal(bare)
        || ["NULL", "TRUE", "FALSE"]
            .iter()
            .any(|kw| same_name(kw, bare))
}

fn is_null_default(c: &ColumnDef) -> bool {
    c.default
        .as_deref()
        .is_none_or(|d| d.trim().is_empty() || same_name(d.trim(), "NULL"))
}

// --- validation ----------------------------------------------------------------------------

/// The column of `t` that is an auto-increment rowid alias, declared inline as
/// `"id" INTEGER PRIMARY KEY [AUTOINCREMENT]`.
fn auto_column(t: &TableDef) -> Option<&ColumnDef> {
    match pk_columns(t) {
        [only] => t
            .column(only)
            .filter(|c| c.auto_increment || flag(&c.extras, &AUTOINCREMENT)),
        _ => None,
    }
}

/// `true` when `c` is the only primary-key column of a rowid table but not an auto-increment
/// column: a type derived for it must not make it a rowid alias.
fn is_plain_int_pk(t: &TableDef, c: &ColumnDef) -> bool {
    !without_rowid(t) && !c.auto_increment && matches!(pk_columns(t), [only] if *only == c.name)
}

fn check_index(t: &TableDef, idx: &IndexDef, v: &ServerVersion) -> Result<()> {
    if idx.name.trim().is_empty() {
        return Err(Error::invalid_input("an index has no name"));
    }
    if idx.columns.is_empty() {
        return Err(Error::invalid_input(format!(
            "index {} has no key columns",
            idx.name
        )));
    }
    if idx.is_constraint && !idx.name.starts_with(AUTOINDEX_PREFIX) {
        return Err(Error::unsupported(format!(
            "SQLite names UNIQUE constraints itself ({AUTOINDEX_PREFIX}...), so constraint {} \
             cannot be created under that name; use a unique index",
            idx.name
        )));
    }
    for part in &idx.columns {
        if part.length.is_some() {
            return Err(Error::unsupported(format!(
                "index {}: SQLite has no index prefix lengths",
                idx.name
            )));
        }
        if part.is_expression {
            if idx.is_constraint {
                return Err(Error::invalid_input(format!(
                    "UNIQUE constraint {}: SQLite allows only columns in a constraint",
                    idx.name
                )));
            }
            if !v.at_least(3, 9) {
                return Err(Error::unsupported("indexes on expressions need SQLite 3.9"));
            }
        } else if !t.columns.iter().any(|c| same_name(&c.name, &part.name)) {
            return Err(Error::invalid_input(format!(
                "index {} uses column {}, which the table does not have",
                idx.name, part.name
            )));
        }
    }
    if idx.predicate.is_some() && !v.at_least(3, 8) {
        return Err(Error::unsupported("partial indexes need SQLite 3.8"));
    }
    Ok(())
}

fn check_column(t: &TableDef, pos: usize, c: &ColumnDef, v: &ServerVersion) -> Result<()> {
    if c.name.is_empty() {
        return Err(Error::invalid_input(format!(
            "column {} has no name",
            pos + 1
        )));
    }
    if t.columns[..pos].iter().any(|o| same_name(&o.name, &c.name)) {
        return Err(Error::invalid_input(format!(
            "column {} is defined twice",
            c.name
        )));
    }
    if c.comment.as_deref().is_some_and(|text| !text.is_empty()) {
        return Err(Error::unsupported("SQLite has no column comments"));
    }
    if c.identity.is_some() {
        return Err(Error::unsupported(format!(
            "column {}: SQLite has no identity columns (an INTEGER PRIMARY KEY generates values)",
            c.name
        )));
    }
    if c.generated.is_some() {
        if !v.at_least(3, 31) {
            return Err(Error::unsupported("generated columns need SQLite 3.31"));
        }
        if !is_null_default(c) {
            return Err(Error::invalid_input(format!(
                "generated column {} cannot have a default",
                c.name
            )));
        }
        if pk_columns(t).iter().any(|k| same_name(k, &c.name)) {
            return Err(Error::invalid_input(format!(
                "generated column {} cannot be in the primary key",
                c.name
            )));
        }
    }
    let ty = type_sql(c, is_plain_int_pk(t, c))?;
    if c.auto_increment || flag(&c.extras, &AUTOINCREMENT) {
        let alone = matches!(pk_columns(t), [only] if *only == c.name);
        if !alone || !same_name(&ty, "INTEGER") || without_rowid(t) {
            return Err(Error::invalid_input(format!(
                "column {}: SQLite generates values only for the INTEGER PRIMARY KEY of a rowid \
                 table (one INTEGER column as the whole primary key)",
                c.name
            )));
        }
    }
    Ok(())
}

/// Everything the generator needs to hold for `t` before writing SQL for it.
fn check_table(t: &TableDef, v: &ServerVersion) -> Result<()> {
    let name = &t.obj.name;
    if is_virtual(t) {
        return Err(Error::unsupported(format!(
            "{name} is a virtual table; SQLite cannot re-create or alter it from a definition"
        )));
    }
    if name.trim().is_empty() {
        return Err(Error::invalid_input("the table has no name"));
    }
    if t.columns.is_empty() {
        return Err(Error::invalid_input(format!("table {name} has no columns")));
    }
    if t.comment.as_deref().is_some_and(|text| !text.is_empty()) {
        return Err(Error::unsupported("SQLite has no table comments"));
    }
    if flag(&t.extras, &props::STRICT) && !v.at_least(3, 37) {
        return Err(Error::unsupported("STRICT tables need SQLite 3.37"));
    }
    if without_rowid(t) {
        if !v.at_least_patch(3, 8, 2) {
            return Err(Error::unsupported("WITHOUT ROWID tables need SQLite 3.8.2"));
        }
        if t.primary_key.is_none() {
            return Err(Error::invalid_input(format!(
                "WITHOUT ROWID table {name} needs a primary key"
            )));
        }
    }
    for (pos, c) in t.columns.iter().enumerate() {
        check_column(t, pos, c, v)?;
    }
    for k in pk_columns(t) {
        if !t.columns.iter().any(|c| same_name(&c.name, k)) {
            return Err(Error::invalid_input(format!(
                "the primary key uses column {k}, which the table does not have"
            )));
        }
    }
    if t.primary_key
        .as_ref()
        .is_some_and(|pk| pk.columns.is_empty())
    {
        return Err(Error::invalid_input("the primary key has no columns"));
    }
    for idx in &t.indexes {
        check_index(t, idx, v)?;
    }
    let schema = schema_of(&t.obj);
    for fk in &t.foreign_keys {
        let label = fk.name.as_deref().unwrap_or("(unnamed)");
        if fk.columns.is_empty() || fk.columns.len() != fk.ref_columns.len() {
            return Err(Error::invalid_input(format!(
                "foreign key {label}: {} columns reference {} columns",
                fk.columns.len(),
                fk.ref_columns.len()
            )));
        }
        if let Some(absent) = fk
            .columns
            .iter()
            .find(|k| !t.columns.iter().any(|c| same_name(&c.name, k)))
        {
            return Err(Error::invalid_input(format!(
                "foreign key {label} uses column {absent}, which the table does not have"
            )));
        }
        if fk
            .ref_table
            .catalog
            .as_deref()
            .is_some_and(|db| !db.is_empty() && !same_name(db, schema))
        {
            return Err(Error::unsupported(format!(
                "foreign key {label}: SQLite foreign keys cannot reference another database"
            )));
        }
    }
    for check in &t.checks {
        if check.expression.trim().is_empty() {
            return Err(Error::invalid_input("a check has no expression"));
        }
    }
    Ok(())
}

// --- SQL text ------------------------------------------------------------------------------

fn column_sql(t: &TableDef, c: &ColumnDef, inline_pk: bool) -> Result<String> {
    let mut sql = ident(&c.name);
    let ty = type_sql(c, is_plain_int_pk(t, c))?;
    if !ty.is_empty() {
        sql.push(' ');
        sql.push_str(&ty);
    }
    if inline_pk {
        if let Some(name) = t.primary_key.as_ref().and_then(|pk| pk.name.as_deref()) {
            sql.push_str(" CONSTRAINT ");
            sql.push_str(&ident(name));
        }
        sql.push_str(" PRIMARY KEY");
        if flag(&c.extras, &AUTOINCREMENT) {
            sql.push_str(" AUTOINCREMENT");
        }
    }
    if !c.nullable {
        sql.push_str(" NOT NULL");
    }
    if c.generated.is_none()
        && let Some(default) = c
            .default
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
    {
        sql.push_str(" DEFAULT (");
        sql.push_str(default);
        sql.push(')');
    }
    if let Some(collation) = c.collation.as_deref().filter(|x| !x.is_empty()) {
        sql.push_str(" COLLATE ");
        sql.push_str(&collation_sql(collation));
    }
    if let Some(generated) = &c.generated {
        sql.push_str(" GENERATED ALWAYS AS (");
        sql.push_str(generated.expression.trim());
        sql.push_str(if generated.stored {
            ") STORED"
        } else {
            ") VIRTUAL"
        });
    }
    Ok(sql)
}

fn name_list(names: &[String]) -> String {
    names
        .iter()
        .map(|n| ident(n.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn constraint_prefix(name: Option<&str>) -> String {
    match name {
        Some(n) if !n.is_empty() => format!("CONSTRAINT {} ", ident(n)),
        _ => String::new(),
    }
}

fn pk_sql(pk: &PrimaryKeyDef) -> String {
    format!(
        "{}PRIMARY KEY ({})",
        constraint_prefix(pk.name.as_deref()),
        name_list(&pk.columns)
    )
}

fn index_parts(idx: &IndexDef) -> String {
    idx.columns
        .iter()
        .map(|part| {
            let mut text = if part.is_expression {
                part.name.trim().to_owned()
            } else {
                ident(&part.name)
            };
            if part.descending {
                text.push_str(" DESC");
            }
            text
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn check_sql(check: &CheckDef) -> String {
    format!(
        "{}CHECK ({})",
        constraint_prefix(check.name.as_deref()),
        check.expression.trim()
    )
}

fn action_sql(action: FkAction) -> Option<&'static str> {
    match action {
        FkAction::NoAction => None,
        FkAction::Restrict => Some("RESTRICT"),
        FkAction::Cascade => Some("CASCADE"),
        FkAction::SetNull => Some("SET NULL"),
        FkAction::SetDefault => Some("SET DEFAULT"),
    }
}

fn fk_sql(fk: &ForeignKeyDef) -> String {
    let mut sql = format!(
        "{}FOREIGN KEY ({}) REFERENCES {} ({})",
        constraint_prefix(fk.name.as_deref()),
        name_list(&fk.columns),
        ident(&fk.ref_table.name),
        name_list(&fk.ref_columns)
    );
    if let Some(action) = action_sql(fk.on_delete) {
        sql.push_str(" ON DELETE ");
        sql.push_str(action);
    }
    if let Some(action) = action_sql(fk.on_update) {
        sql.push_str(" ON UPDATE ");
        sql.push_str(action);
    }
    if flag(&fk.extras, &props::DEFERRABLE) {
        sql.push_str(" DEFERRABLE");
        if flag(&fk.extras, &props::INITIALLY_DEFERRED) {
            sql.push_str(" INITIALLY DEFERRED");
        }
    }
    sql
}

/// `CREATE TABLE "<schema>"."<name>" (...)` for `t`.
fn create_table_sql(t: &TableDef, schema: &str, name: &str) -> Result<String> {
    let inline = auto_column(t).map(|c| c.name.as_str());
    let mut items = Vec::with_capacity(t.columns.len() + t.checks.len() + t.foreign_keys.len());
    for c in &t.columns {
        items.push(column_sql(t, c, inline == Some(c.name.as_str()))?);
    }
    // The primary key and the UNIQUE constraints in the order of their autoindex numbers, so a
    // rebuilt table keeps the names `sqlite_autoindex_<table>_<n>` its constraints had.
    let uniques: Vec<&IndexDef> = t.indexes.iter().filter(|i| i.is_constraint).collect();
    let numbers: Vec<Option<u32>> = uniques.iter().map(|i| autoindex_number(&i.name)).collect();
    let mut keyed: Vec<(u32, String)> = Vec::new();
    if inline.is_none()
        && let Some(pk) = &t.primary_key
    {
        let mut n = 1;
        while numbers.contains(&Some(n)) {
            n += 1;
        }
        keyed.push((n, pk_sql(pk)));
    }
    for (idx, number) in uniques.iter().zip(&numbers) {
        keyed.push((
            number.unwrap_or(u32::MAX),
            format!("UNIQUE ({})", index_parts(idx)),
        ));
    }
    keyed.sort_by_key(|(n, _)| *n);
    items.extend(keyed.into_iter().map(|(_, sql)| sql));
    items.extend(t.checks.iter().map(check_sql));
    items.extend(t.foreign_keys.iter().map(fk_sql));
    let mut sql = format!(
        "CREATE TABLE {} (\n    {}\n)",
        qualified(schema, name),
        items.join(",\n    ")
    );
    let mut options = Vec::new();
    if without_rowid(t) {
        options.push("WITHOUT ROWID");
    }
    if flag(&t.extras, &props::STRICT) {
        options.push("STRICT");
    }
    if !options.is_empty() {
        sql.push(' ');
        sql.push_str(&options.join(", "));
    }
    Ok(sql)
}

/// `CREATE [UNIQUE] INDEX "<schema>"."<index>" ON "<table>" (...) [WHERE ...]`.
fn create_index_sql(schema: &str, table: &str, idx: &IndexDef) -> String {
    let mut sql = format!(
        "CREATE {}INDEX {} ON {} ({})",
        if idx.unique { "UNIQUE " } else { "" },
        qualified(schema, &idx.name),
        ident(table),
        index_parts(idx)
    );
    if let Some(predicate) = idx
        .predicate
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        sql.push_str(" WHERE ");
        sql.push_str(predicate);
    }
    sql
}

fn create_index_step(schema: &str, table: &str, idx: &IndexDef) -> ScriptStep {
    step(
        create_index_sql(schema, table, idx),
        format!("Create index {} on {table}", idx.name),
    )
}

fn drop_index_step(schema: &str, name: &str) -> ScriptStep {
    step(
        format!("DROP INDEX {}", qualified(schema, name)),
        format!("Drop index {name}"),
    )
    .destructive()
}

// --- create --------------------------------------------------------------------------------

fn create_table(t: &TableDef, ctx: &DdlContext<'_>) -> Result<Script> {
    check_table(t, ctx.version)?;
    let schema = schema_of(&t.obj);
    let name = &t.obj.name;
    let mut script = Script::new();
    script.push_step(step(
        create_table_sql(t, schema, name)?,
        format!("Create table {name}"),
    ));
    for idx in t.indexes.iter().filter(|i| !i.is_constraint) {
        script.push_step(create_index_step(schema, name, idx));
    }
    Ok(script)
}

// --- renames -------------------------------------------------------------------------------

/// The renames of one `alter`, checked against the definitions.
#[derive(Debug, Default)]
struct Renames {
    table: Option<(String, String)>,
    columns: Vec<(String, String)>,
    indexes: Vec<(String, String)>,
    foreign_keys: Vec<(String, String)>,
    checks: Vec<(String, String)>,
}

fn renamed<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(old, _)| old == name)
        .map(|(_, new)| new.as_str())
}

fn rename_error(r: &Rename, what: &str) -> Error {
    Error::invalid_input(format!(
        "cannot rename {} {} to {}: {what}",
        r.kind.display_name(),
        r.old,
        r.new
    ))
}

/// The renames of `ctx` that apply to `old` → `new`. The table rename comes from the names.
fn resolve_renames(old: &TableDef, new: &TableDef, renames: &[Rename]) -> Result<Renames> {
    let mut out = Renames::default();
    if old.obj.name != new.obj.name {
        out.table = Some((old.obj.name.clone(), new.obj.name.clone()));
    }
    for r in renames.iter().filter(|r| r.old != r.new) {
        let pair = (r.old.clone(), r.new.clone());
        match r.kind {
            ObjectKind::Table => {}
            ObjectKind::Column => {
                if old.column(&r.old).is_none() || new.column(&r.new).is_none() {
                    return Err(rename_error(r, "no such column on both sides"));
                }
                out.columns.push(pair);
            }
            ObjectKind::Index => {
                if !old.indexes.iter().any(|i| i.name == r.old)
                    || !new.indexes.iter().any(|i| i.name == r.new)
                {
                    return Err(rename_error(r, "no such index on both sides"));
                }
                out.indexes.push(pair);
            }
            ObjectKind::ForeignKey => {
                if !old
                    .foreign_keys
                    .iter()
                    .any(|f| f.name.as_deref() == Some(r.old.as_str()))
                {
                    return Err(rename_error(r, "no such foreign key"));
                }
                out.foreign_keys.push(pair);
            }
            ObjectKind::Check => {
                if !old
                    .checks
                    .iter()
                    .any(|c| c.name.as_deref() == Some(r.old.as_str()))
                {
                    return Err(rename_error(r, "no such check"));
                }
                out.checks.push(pair);
            }
            other => {
                return Err(Error::invalid_input(format!(
                    "cannot rename a {} in a table design",
                    other.display_name()
                )));
            }
        }
    }
    Ok(out)
}

/// `old` with `r` applied: the table, column names wherever they appear (columns, primary key,
/// index columns, foreign-key columns), index and constraint names. Expressions are kept.
fn apply_renames(old: &TableDef, r: &Renames) -> TableDef {
    let mut out = old.clone();
    let column = |name: &mut String| {
        if let Some(new) = renamed(&r.columns, name) {
            *name = new.to_owned();
        }
    };
    if let Some((_, new)) = &r.table {
        out.obj.name.clone_from(new);
    }
    for c in &mut out.columns {
        column(&mut c.name);
    }
    if let Some(pk) = &mut out.primary_key {
        pk.columns.iter_mut().for_each(column);
    }
    for idx in &mut out.indexes {
        if let Some(new) = renamed(&r.indexes, &idx.name) {
            idx.name = new.to_owned();
        }
        for part in idx.columns.iter_mut().filter(|p| !p.is_expression) {
            column(&mut part.name);
        }
    }
    for fk in &mut out.foreign_keys {
        if let Some(new) = fk.name.as_deref().and_then(|n| renamed(&r.foreign_keys, n)) {
            fk.name = Some(new.to_owned());
        }
        fk.columns.iter_mut().for_each(column);
    }
    for check in &mut out.checks {
        if let Some(new) = check.name.as_deref().and_then(|n| renamed(&r.checks, n)) {
            check.name = Some(new.to_owned());
        }
    }
    out
}

// --- alter ---------------------------------------------------------------------------------

/// What an `alter` does besides the renames.
#[derive(Debug, Default)]
struct Plan {
    /// Why the table must be rebuilt (empty: in place).
    rebuild: Vec<String>,
    drop_columns: Vec<String>,
    add_columns: Vec<String>,
    /// Plain indexes to drop, by their name on the server.
    drop_indexes: Vec<String>,
    /// Plain indexes of the new definition to create.
    create_indexes: Vec<String>,
}

fn push_once(list: &mut Vec<String>, name: &str) {
    if !list.iter().any(|n| n == name) {
        list.push(name.to_owned());
    }
}

fn prop_label(prop: Option<&PropId>) -> &str {
    prop.map_or("definition", PropId::as_str)
}

/// Sorts the differences between `current` (the old table with the renames applied) and `new`
/// into in-place changes and reasons to rebuild.
fn classify(
    plan: &mut Plan,
    current: &TableDef,
    new: &TableDef,
    r: &Renames,
    diffs: &[PropertyDiff],
) {
    for d in diffs {
        let name = d.object.name.as_str();
        let whole_object = d.prop.is_none();
        match d.object.kind {
            ObjectKind::Column if whole_object && d.kind == DiffKind::Removed => {
                push_once(&mut plan.drop_columns, name);
            }
            ObjectKind::Column if whole_object && d.kind == DiffKind::Added => {
                push_once(&mut plan.add_columns, name);
            }
            ObjectKind::Column => plan
                .rebuild
                .push(format!("column {name}: {}", prop_label(d.prop.as_ref()))),
            ObjectKind::Index => {
                let before = current.indexes.iter().find(|i| i.name == name);
                let after = new.indexes.iter().find(|i| i.name == name);
                if before.is_some_and(|i| i.is_constraint) || after.is_some_and(|i| i.is_constraint)
                {
                    plan.rebuild.push(format!("UNIQUE constraint {name}"));
                } else {
                    if before.is_some() {
                        let on_server = r
                            .indexes
                            .iter()
                            .find(|(_, to)| to == name)
                            .map_or(name, |(from, _)| from.as_str());
                        push_once(&mut plan.drop_indexes, on_server);
                    }
                    if after.is_some() {
                        push_once(&mut plan.create_indexes, name);
                    }
                }
            }
            ObjectKind::ForeignKey => plan.rebuild.push("foreign keys".to_owned()),
            ObjectKind::Check => plan.rebuild.push("checks".to_owned()),
            _ if d.prop == Some(props::NAME) || d.prop == Some(props::COMMENT) => {}
            _ => plan
                .rebuild
                .push(format!("table {}", prop_label(d.prop.as_ref()))),
        }
    }
    // Index renames without other changes: SQLite cannot rename an index.
    for (from, to) in &r.indexes {
        if new
            .indexes
            .iter()
            .any(|i| i.name == *to && !i.is_constraint)
        {
            push_once(&mut plan.drop_indexes, from);
            push_once(&mut plan.create_indexes, to);
        }
    }
    // What the shared comparison leaves out but SQLite only changes by a rebuild.
    for c in &new.columns {
        let Some(before) = current.column(&c.name) else {
            continue;
        };
        if flag(&before.extras, &AUTOINCREMENT) != flag(&c.extras, &AUTOINCREMENT) {
            plan.rebuild
                .push(format!("column {}: AUTOINCREMENT", c.name));
        }
        let same_collation = match (before.collation.as_deref(), c.collation.as_deref()) {
            (Some(a), Some(b)) => same_name(a, b),
            (a, b) => a.is_none_or(str::is_empty) && b.is_none_or(str::is_empty),
        };
        if !same_collation {
            plan.rebuild.push(format!("column {}: collation", c.name));
        }
    }
    if without_rowid(current) != without_rowid(new) {
        plan.rebuild.push("WITHOUT ROWID".to_owned());
    }
    if flag(&current.extras, &props::STRICT) != flag(&new.extras, &props::STRICT) {
        plan.rebuild.push("STRICT".to_owned());
    }
    if !r.foreign_keys.is_empty() || !r.checks.is_empty() {
        plan.rebuild.push("constraint names".to_owned());
    }
}

/// Why `name` (a column of `current`) cannot be dropped with `DROP COLUMN`, if it cannot.
fn drop_blocker(current: &TableDef, name: &str, ctx: &DdlContext<'_>) -> Option<String> {
    if !ctx.version.at_least(3, 35) {
        return Some("DROP COLUMN needs SQLite 3.35".to_owned());
    }
    let column = current.column(name)?;
    let uses = |text: &str| parse::mentions_identifier(text, name);
    let reason = if pk_columns(current).iter().any(|k| same_name(k, name)) {
        "it is in the primary key"
    } else if current.indexes.iter().any(|i| {
        i.is_constraint
            && i.columns
                .iter()
                .any(|p| !p.is_expression && same_name(&p.name, name))
    }) {
        "it has a UNIQUE constraint"
    } else if current
        .foreign_keys
        .iter()
        .any(|fk| fk.columns.iter().any(|k| same_name(k, name)))
    {
        "it is in a foreign key"
    } else if column.generated.is_some() {
        "it is a generated column"
    } else if current
        .columns
        .iter()
        .filter_map(|c| c.generated.as_ref())
        .any(|g| uses(g.expression.as_str()))
    {
        "a generated column uses it"
    } else if current.checks.iter().any(|c| uses(c.expression.as_str())) {
        "a check uses it"
    } else if ctx
        .triggers
        .iter()
        .filter_map(|t| t.definition.as_deref())
        .any(uses)
    {
        "a trigger uses it"
    } else {
        return None;
    };
    Some(format!("column {name}: {reason}"))
}

/// Why `c` (a column of `new` that `current` does not have) cannot be added with `ADD COLUMN`.
fn add_blocker(new: &TableDef, c: &ColumnDef) -> Option<String> {
    let name = &c.name;
    let reason = if pk_columns(new).iter().any(|k| same_name(k, name)) || c.auto_increment {
        "it is in the primary key"
    } else if new.indexes.iter().any(|i| {
        i.is_constraint
            && i.columns
                .iter()
                .any(|p| !p.is_expression && same_name(&p.name, name))
    }) {
        "it has a UNIQUE constraint"
    } else if new
        .foreign_keys
        .iter()
        .any(|fk| fk.columns.iter().any(|k| same_name(k, name)))
    {
        "it has a foreign key"
    } else if c.generated.as_ref().is_some_and(|g| g.stored) {
        "it is a STORED generated column"
    } else if c.generated.is_none() && !c.nullable && is_null_default(c) {
        "it is NOT NULL without a default"
    } else if c
        .default
        .as_deref()
        .is_some_and(|d| !d.trim().is_empty() && !is_constant_default(d))
    {
        "its default is not a constant"
    } else {
        return None;
    };
    Some(format!("new column {name}: {reason}"))
}

/// `true` when every column `current` does not have comes after every column it has.
fn added_at_end(current: &TableDef, new: &TableDef) -> bool {
    let kept: Vec<bool> = new
        .columns
        .iter()
        .map(|c| current.column(&c.name).is_some())
        .collect();
    let first_added = kept.iter().position(|k| !k).unwrap_or(kept.len());
    kept[first_added..].iter().all(|k| !k)
}

fn alter_table(old: &TableDef, new: &TableDef, ctx: &DdlContext<'_>) -> Result<Script> {
    if is_virtual(old) {
        return Err(Error::unsupported(format!(
            "{} is a virtual table; SQLite cannot alter it",
            old.obj.name
        )));
    }
    check_table(new, ctx.version)?;
    let schema = schema_of(&old.obj);
    if !same_name(schema_of(&new.obj), schema) {
        return Err(Error::unsupported(
            "SQLite cannot move a table to another database",
        ));
    }
    let renames = resolve_renames(old, new, ctx.renames)?;
    let current = apply_renames(old, &renames);
    let diffs = diff_tables(&current, new, &SqliteRules);
    let mut plan = Plan::default();
    classify(&mut plan, &current, new, &renames, &diffs);
    let native_column_renames = ctx.version.at_least(3, 25);
    if !renames.columns.is_empty() && !native_column_renames {
        plan.rebuild
            .push("RENAME COLUMN needs SQLite 3.25".to_owned());
    }
    if plan.rebuild.is_empty() {
        for name in &plan.drop_columns {
            if let Some(why) = drop_blocker(&current, name, ctx) {
                plan.rebuild.push(why);
            }
        }
        for name in &plan.add_columns {
            if let Some(why) = new.column(name).and_then(|c| add_blocker(new, c)) {
                plan.rebuild.push(why);
            }
        }
        if !plan.add_columns.is_empty() && !added_at_end(&current, new) {
            plan.rebuild
                .push("new columns before existing ones".to_owned());
        }
    }

    let mut script = Script::new();
    let rebuild = !plan.rebuild.is_empty();
    if rebuild {
        push_rebuild_prepare(&mut script);
    }
    // Renames first, natively.
    let mut name_now = old.obj.name.clone();
    if let Some((from, to)) = &renames.table {
        script.push_step(step(
            format!(
                "ALTER TABLE {} RENAME TO {}",
                qualified(schema, from),
                ident(to)
            ),
            format!("Rename table {from} to {to}"),
        ));
        name_now.clone_from(to);
    }
    if native_column_renames {
        for (from, to) in &renames.columns {
            script.push_step(step(
                format!(
                    "ALTER TABLE {} RENAME COLUMN {} TO {}",
                    qualified(schema, &name_now),
                    ident(from),
                    ident(to)
                ),
                format!("Rename column {from} to {to}"),
            ));
        }
    }
    if rebuild {
        let source = |column: &str| -> String {
            if native_column_renames {
                column.to_owned()
            } else {
                renames
                    .columns
                    .iter()
                    .find(|(_, to)| to == column)
                    .map_or_else(|| column.to_owned(), |(from, _)| from.clone())
            }
        };
        let rebuild_ctx = Rebuild {
            schema,
            old,
            current: &current,
            new,
            renames: &renames,
            source: &source,
            ctx,
        };
        push_rebuild(&mut script, &rebuild_ctx, &plan.rebuild)?;
        return Ok(script);
    }
    for name in &plan.drop_indexes {
        script.push_step(drop_index_step(schema, name));
    }
    for name in &plan.drop_columns {
        script.push_step(
            step(
                format!(
                    "ALTER TABLE {} DROP COLUMN {}",
                    qualified(schema, &name_now),
                    ident(name)
                ),
                format!("Drop column {name} from {name_now}"),
            )
            .destructive(),
        );
    }
    for name in &plan.add_columns {
        if let Some(c) = new.column(name) {
            script.push_step(step(
                format!(
                    "ALTER TABLE {} ADD COLUMN {}",
                    qualified(schema, &name_now),
                    column_sql(new, c, false)?
                ),
                format!("Add column {name} to {name_now}"),
            ));
        }
    }
    for name in &plan.create_indexes {
        if let Some(idx) = new.indexes.iter().find(|i| i.name == *name) {
            script.push_step(create_index_step(schema, &name_now, idx));
        }
    }
    Ok(script)
}

// --- the rebuild ---------------------------------------------------------------------------

const FK_CAPTURE: &str = "fk";
const LEGACY_CAPTURE: &str = "legacy_alter";

fn push_rebuild_prepare(script: &mut Script) {
    script
        .push_step(
            ScriptStep::new("PRAGMA foreign_keys")
                .described("Remember whether foreign keys are enforced")
                .in_phase(StepPhase::Prepare)
                .capture(FK_CAPTURE),
        )
        .push_step(
            ScriptStep::new("PRAGMA foreign_keys = OFF")
                .described(
                    "Stop enforcing foreign keys while the table is rebuilt (outside the \
                     transaction: SQLite ignores this pragma inside one)",
                )
                .in_phase(StepPhase::Prepare),
        )
        .push_step(
            ScriptStep::new("PRAGMA legacy_alter_table")
                .described("Remember the legacy_alter_table setting")
                .in_phase(StepPhase::Prepare)
                .capture(LEGACY_CAPTURE),
        );
}

/// Inputs of one rebuild.
struct Rebuild<'a> {
    schema: &'a str,
    /// The table as loaded (names before the renames).
    old: &'a TableDef,
    /// `old` with the renames applied.
    current: &'a TableDef,
    new: &'a TableDef,
    renames: &'a Renames,
    /// The name a column of `new` that `current` has is read from in the copy.
    source: &'a dyn Fn(&str) -> String,
    ctx: &'a DdlContext<'a>,
}

/// The first rowid name no column of `a` or `b` takes.
fn free_rowid_name(a: &TableDef, b: &TableDef) -> Option<&'static str> {
    ["rowid", "_rowid_", "oid"].into_iter().find(|name| {
        !a.columns
            .iter()
            .chain(&b.columns)
            .any(|c| same_name(&c.name, name))
    })
}

/// The column list and the `SELECT` list that copy the rows of the old table.
fn copy_lists(r: &Rebuild<'_>) -> Result<(Vec<String>, Vec<String>)> {
    let mut targets = Vec::new();
    let mut sources = Vec::new();
    let mut alias_copied = false;
    for c in &r.new.columns {
        if c.generated.is_some() {
            continue;
        }
        let Some(before) = r.current.column(&c.name) else {
            continue;
        };
        targets.push(ident(&c.name));
        let from = ident(&(r.source)(&c.name));
        let ty = type_sql(c, is_plain_int_pk(r.new, c))?;
        if ty.is_empty() || SqliteRules.same_type(before, c) {
            sources.push(from);
        } else {
            sources.push(format!("CAST({from} AS {ty})"));
        }
        if auto_column(r.new).is_some_and(|a| a.name == c.name)
            || (is_rowid_alias_type(&ty) && matches!(pk_columns(r.new), [only] if *only == c.name))
        {
            alias_copied = true;
        }
    }
    if !alias_copied
        && !without_rowid(r.current)
        && !without_rowid(r.new)
        && let Some(rowid) = free_rowid_name(r.current, r.new)
    {
        targets.insert(0, rowid.to_owned());
        sources.insert(0, rowid.to_owned());
    }
    Ok((targets, sources))
}

fn is_rowid_alias_type(ty: &str) -> bool {
    same_name(ty.trim(), "INTEGER")
}

fn push_rebuild(script: &mut Script, r: &Rebuild<'_>, reasons: &[String]) -> Result<()> {
    let schema = r.schema;
    let name = r.new.obj.name.as_str();
    let tmp = format!("{name}{REBUILD_SUFFIX}");
    let table = qualified(schema, name);
    let temp_table = qualified(schema, &tmp);
    let main = |sql: String, description: String| step(sql, description).destructive();

    script.push_step(main(
        create_table_sql(r.new, schema, &tmp)?,
        format!("Rebuild {name}: create the new table"),
    ));
    let (targets, sources) = copy_lists(r)?;
    if targets.is_empty() {
        script.warn(format!(
            "{name} keeps none of its columns: the rebuild does not copy its rows"
        ));
    } else {
        script.push_step(main(
            format!(
                "INSERT INTO {temp_table} ({}) SELECT {} FROM {table}",
                targets.join(", "),
                sources.join(", ")
            ),
            format!("Rebuild {name}: copy the rows"),
        ));
    }
    let keeps_sequence = auto_column(r.current).is_some_and(|c| flag(&c.extras, &AUTOINCREMENT))
        && auto_column(r.new).is_some_and(|c| flag(&c.extras, &AUTOINCREMENT));
    if keeps_sequence {
        let seq = qualified(schema, "sqlite_sequence");
        let (old_lit, tmp_lit) = (literal(name), literal(&tmp));
        let old_seq = format!("(SELECT seq FROM {seq} WHERE name = {old_lit})");
        script.push_step(main(
            format!("UPDATE {seq} SET seq = {old_seq} WHERE name = {tmp_lit} AND seq < {old_seq}"),
            format!("Rebuild {name}: keep the AUTOINCREMENT counter"),
        ));
        script.push_step(main(
            format!(
                "INSERT INTO {seq} (name, seq) SELECT {tmp_lit}, seq FROM {seq} WHERE name = \
                 {old_lit} AND NOT EXISTS (SELECT 1 FROM {seq} WHERE name = {tmp_lit})"
            ),
            format!("Rebuild {name}: keep the AUTOINCREMENT counter"),
        ));
    }
    script.push_step(main(
        format!("DROP TABLE {table}"),
        format!("Rebuild {name}: drop the old table"),
    ));
    script.push_step(main(
        "PRAGMA legacy_alter_table = ON".to_owned(),
        "Do not re-check views on the table while it is renamed".to_owned(),
    ));
    script.push_step(main(
        format!("ALTER TABLE {temp_table} RENAME TO {}", ident(name)),
        format!("Rebuild {name}: give the new table its name"),
    ));
    for idx in r.new.indexes.iter().filter(|i| !i.is_constraint) {
        script.push_step(create_index_step(schema, name, idx).destructive());
    }
    push_triggers(script, r)?;
    script.push_step(
        main(
            format!(
                "PRAGMA {}.foreign_key_check({})",
                ident(schema),
                ident(name)
            ),
            format!("Check the foreign keys of {name}"),
        )
        .expect_no_rows()
        .only_if(FK_CAPTURE, "1"),
    );
    script
        .push_step(
            ScriptStep::new("PRAGMA foreign_keys = ON")
                .described("Enforce foreign keys again")
                .in_phase(StepPhase::Finally)
                .only_if(FK_CAPTURE, "1"),
        )
        .push_step(
            ScriptStep::new("PRAGMA legacy_alter_table = OFF")
                .described("Restore the legacy_alter_table setting")
                .in_phase(StepPhase::Finally)
                .only_if(LEGACY_CAPTURE, "0"),
        );
    script.warn(format!(
        "{name} is rebuilt ({}): its rows are copied into a new table, then its indexes and \
         triggers are re-created. Views on it are kept.",
        reasons.join(", ")
    ));
    Ok(())
}

fn push_triggers(script: &mut Script, r: &Rebuild<'_>) -> Result<()> {
    let table_rename = r
        .renames
        .table
        .as_ref()
        .map(|(from, to)| (from.as_str(), to.as_str()));
    let attached = !same_name(r.schema, "main") && !same_name(r.schema, "temp");
    for trigger in r.ctx.triggers {
        let name = &trigger.obj.name;
        let definition = trigger.definition.as_deref().ok_or_else(|| {
            Error::invalid_input(format!(
                "trigger {name} has no definition, so the rebuild of {} cannot re-create it",
                r.old.obj.name
            ))
        })?;
        let mut sql = definition
            .trim()
            .trim_end_matches(';')
            .trim_end()
            .to_owned();
        if table_rename.is_some() || !r.renames.columns.is_empty() {
            if let Some(rewritten) =
                parse::rename_in_trigger(&sql, table_rename, &r.renames.columns, &ident)
            {
                sql = rewritten;
                script.warn(format!(
                    "trigger {name} is re-created with the renamed names replaced; review it"
                ));
            }
        }
        if attached && !flag(&trigger.extras, &props::TEMPORARY) {
            sql = parse::qualify_trigger_name(&sql, &ident(r.schema));
        }
        script.push_step(step(sql, format!("Re-create trigger {name}")).destructive());
    }
    Ok(())
}

// --- child kinds ---------------------------------------------------------------------------

/// The table as it is now, for a change to one of its children.
fn current_table<'a>(
    ctx: &DdlContext<'a>,
    table: &ObjectRef,
    kind: ObjectKind,
) -> Result<&'a TableDef> {
    let cur = ctx.table.ok_or_else(|| {
        Error::invalid_input(format!(
            "SQLite changes a {} with the table's definition: DdlContext::table is needed for {}",
            kind.display_name(),
            table.name
        ))
    })?;
    if same_name(&cur.obj.name, &table.name) {
        Ok(cur)
    } else {
        Err(Error::invalid_input(format!(
            "the {} belongs to {}, but the context table is {}",
            kind.display_name(),
            table.name,
            cur.obj.name
        )))
    }
}

fn owner_of(r: &ObjectRef) -> Result<ObjectRef> {
    r.owner().ok_or_else(|| {
        Error::invalid_input(format!(
            "{} {} names no owning table",
            r.kind.display_name(),
            r.name
        ))
    })
}

/// Position of the foreign key or check `name` in a table's list: a constraint name, or the
/// `(foreign key n)` / `(check n)` label listings give unnamed ones.
fn constraint_position<'a>(
    names: impl Iterator<Item = Option<&'a str>>,
    name: &str,
    label: &str,
) -> Option<usize> {
    let names: Vec<Option<&str>> = names.collect();
    names
        .iter()
        .position(|n| n.is_some_and(|n| same_name(n, name)))
        .or_else(|| {
            names
                .iter()
                .enumerate()
                .position(|(i, n)| n.is_none() && name == format!("({label} {})", i + 1))
        })
}

fn missing(kind: ObjectKind, name: &str, table: &str) -> Error {
    Error::not_found(format!(
        "{} {name} does not exist on {table}",
        kind.display_name()
    ))
}

impl DdlGenerator for SqliteDdl {
    fn create(&self, d: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        match d {
            ObjectDef::Table(t) => create_table(t, ctx),
            ObjectDef::Column { table, column } => {
                let cur = current_table(ctx, table, ObjectKind::Column)?;
                if cur.columns.iter().any(|c| same_name(&c.name, &column.name)) {
                    return Err(Error::invalid_input(format!(
                        "{} already has a column {}",
                        cur.obj.name, column.name
                    )));
                }
                let mut next = cur.clone();
                next.columns.push(column.clone());
                alter_table(cur, &next, ctx)
            }
            ObjectDef::Index { table, index } => {
                if ctx.table.is_none() && !index.is_constraint {
                    let mut owner = TableDef::new(table.clone());
                    // The columns are unknown here; SQLite checks them when the index is made.
                    owner.columns = index
                        .columns
                        .iter()
                        .filter(|p| !p.is_expression)
                        .map(|p| ColumnDef::new(p.name.clone(), DataType::Text))
                        .collect();
                    check_index(&owner, index, ctx.version)?;
                    let mut script = Script::new();
                    script.push_step(create_index_step(schema_of(table), &table.name, index));
                    return Ok(script);
                }
                let cur = current_table(ctx, table, ObjectKind::Index)?;
                let mut next = cur.clone();
                next.indexes.push(index.clone());
                alter_table(cur, &next, ctx)
            }
            ObjectDef::ForeignKey { table, foreign_key } => {
                let cur = current_table(ctx, table, ObjectKind::ForeignKey)?;
                let mut next = cur.clone();
                next.foreign_keys.push(foreign_key.clone());
                alter_table(cur, &next, ctx)
            }
            ObjectDef::Check { table, check } => {
                let cur = current_table(ctx, table, ObjectKind::Check)?;
                let mut next = cur.clone();
                next.checks.push(check.clone());
                alter_table(cur, &next, ctx)
            }
            other => Err(unsupported_kind(other.kind())),
        }
    }

    fn alter(&self, old: &ObjectDef, new: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        match (old, new) {
            (ObjectDef::Table(a), ObjectDef::Table(b)) => alter_table(a, b, ctx),
            (ObjectDef::Column { table, column: a }, ObjectDef::Column { column: b, .. }) => {
                let cur = current_table(ctx, table, ObjectKind::Column)?;
                let pos = cur
                    .columns
                    .iter()
                    .position(|c| c.name == a.name)
                    .ok_or_else(|| missing(ObjectKind::Column, &a.name, &cur.obj.name))?;
                let mut next = cur.clone();
                next.columns[pos] = b.clone();
                alter_table(cur, &next, ctx)
            }
            (ObjectDef::Index { table, index: a }, ObjectDef::Index { index: b, .. }) => {
                if ctx.table.is_some() || a.is_constraint || b.is_constraint {
                    let cur = current_table(ctx, table, ObjectKind::Index)?;
                    let pos = cur
                        .indexes
                        .iter()
                        .position(|i| i.name == a.name)
                        .ok_or_else(|| missing(ObjectKind::Index, &a.name, &cur.obj.name))?;
                    let mut next = cur.clone();
                    next.indexes[pos] = b.clone();
                    return alter_table(cur, &next, ctx);
                }
                let mut script = Script::new();
                if a.name == b.name && diff_indexes(table, a, b, &SqliteRules).is_empty() {
                    return Ok(script);
                }
                let mut owner = TableDef::new(table.clone());
                owner.columns = b
                    .columns
                    .iter()
                    .filter(|p| !p.is_expression)
                    .map(|p| ColumnDef::new(p.name.clone(), DataType::Text))
                    .collect();
                check_index(&owner, b, ctx.version)?;
                let schema = schema_of(table);
                script.push_step(drop_index_step(schema, &a.name));
                script.push_step(create_index_step(schema, &table.name, b));
                Ok(script)
            }
            (
                ObjectDef::ForeignKey {
                    table,
                    foreign_key: a,
                },
                ObjectDef::ForeignKey { foreign_key: b, .. },
            ) => {
                let cur = current_table(ctx, table, ObjectKind::ForeignKey)?;
                let pos = cur
                    .foreign_keys
                    .iter()
                    .position(|f| match &a.name {
                        Some(n) => f.name.as_deref().is_some_and(|m| same_name(m, n)),
                        None => f == a,
                    })
                    .ok_or_else(|| {
                        missing(
                            ObjectKind::ForeignKey,
                            a.name.as_deref().unwrap_or("(unnamed)"),
                            &cur.obj.name,
                        )
                    })?;
                let mut next = cur.clone();
                next.foreign_keys[pos] = b.clone();
                alter_table(cur, &next, ctx)
            }
            (ObjectDef::Check { table, check: a }, ObjectDef::Check { check: b, .. }) => {
                let cur = current_table(ctx, table, ObjectKind::Check)?;
                let pos = cur
                    .checks
                    .iter()
                    .position(|c| match &a.name {
                        Some(n) => c.name.as_deref().is_some_and(|m| same_name(m, n)),
                        None => c == a,
                    })
                    .ok_or_else(|| {
                        missing(
                            ObjectKind::Check,
                            a.name.as_deref().unwrap_or("(unnamed)"),
                            &cur.obj.name,
                        )
                    })?;
                let mut next = cur.clone();
                next.checks[pos] = b.clone();
                alter_table(cur, &next, ctx)
            }
            _ => Err(Error::invalid_input(format!(
                "cannot alter a {} into a {}",
                old.kind().display_name(),
                new.kind().display_name()
            ))),
        }
    }

    fn drop_object(&self, r: &ObjectRef, ctx: &DdlContext<'_>) -> Result<Script> {
        let schema = schema_of(r);
        match r.kind {
            ObjectKind::Table => {
                let mut script = Script::new();
                script.push_step(
                    step(
                        format!("DROP TABLE {}", qualified(schema, &r.name)),
                        format!("Drop table {}", r.name),
                    )
                    .destructive(),
                );
                Ok(script)
            }
            ObjectKind::Index => {
                let constraint = ctx.table.and_then(|cur| {
                    cur.indexes
                        .iter()
                        .position(|i| i.is_constraint && same_name(&i.name, &r.name))
                        .map(|pos| (cur, pos))
                });
                if let Some((cur, pos)) = constraint {
                    let mut next = cur.clone();
                    next.indexes.remove(pos);
                    return alter_table(cur, &next, ctx);
                }
                let mut script = Script::new();
                script.push_step(drop_index_step(schema, &r.name));
                Ok(script)
            }
            ObjectKind::Column => {
                let owner = owner_of(r)?;
                let cur = current_table(ctx, &owner, ObjectKind::Column)?;
                let pos = cur
                    .columns
                    .iter()
                    .position(|c| c.name == r.name)
                    .or_else(|| cur.columns.iter().position(|c| same_name(&c.name, &r.name)))
                    .ok_or_else(|| missing(ObjectKind::Column, &r.name, &cur.obj.name))?;
                let mut next = cur.clone();
                next.columns.remove(pos);
                alter_table(cur, &next, ctx)
            }
            ObjectKind::ForeignKey => {
                let owner = owner_of(r)?;
                let cur = current_table(ctx, &owner, ObjectKind::ForeignKey)?;
                let pos = constraint_position(
                    cur.foreign_keys.iter().map(|f| f.name.as_deref()),
                    &r.name,
                    "foreign key",
                )
                .ok_or_else(|| missing(ObjectKind::ForeignKey, &r.name, &cur.obj.name))?;
                let mut next = cur.clone();
                next.foreign_keys.remove(pos);
                alter_table(cur, &next, ctx)
            }
            ObjectKind::Check => {
                let owner = owner_of(r)?;
                let cur = current_table(ctx, &owner, ObjectKind::Check)?;
                let pos = constraint_position(
                    cur.checks.iter().map(|c| c.name.as_deref()),
                    &r.name,
                    "check",
                )
                .ok_or_else(|| missing(ObjectKind::Check, &r.name, &cur.obj.name))?;
                let mut next = cur.clone();
                next.checks.remove(pos);
                alter_table(cur, &next, ctx)
            }
            other => Err(unsupported_kind(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{GeneratedColumn, IdentityDef, IndexColumn, TriggerDef};

    fn v(text: &str) -> ServerVersion {
        text.parse().unwrap()
    }

    fn col(name: &str, native: &str) -> ColumnDef {
        let mut c = ColumnDef::new(name, super::super::types::data_type(native));
        c.native_type = native.to_owned();
        c
    }

    fn index(name: &str, columns: &[&str]) -> IndexDef {
        IndexDef {
            name: name.to_owned(),
            columns: columns.iter().map(|c| IndexColumn::column(*c)).collect(),
            unique: false,
            is_constraint: false,
            predicate: None,
            extras: Extras::new(),
        }
    }

    /// `t (id INTEGER PRIMARY KEY, qty INTEGER DEFAULT 1, note TEXT)` with index `t_qty`.
    fn base() -> TableDef {
        let mut t = TableDef::new(ObjectRef::table(Some("main"), None, "t"));
        let mut id = col("id", "INTEGER").not_null();
        id.auto_increment = true;
        t.columns.push(id);
        let mut qty = col("qty", "INTEGER");
        qty.default = Some("1".into());
        t.columns.push(qty);
        t.columns.push(col("note", "TEXT"));
        t.primary_key = Some(PrimaryKeyDef {
            name: None,
            columns: vec!["id".into()],
        });
        t.indexes.push(index("t_qty", &["qty"]));
        t
    }

    fn trigger(definition: &str) -> TriggerDef {
        let table = ObjectRef::table(Some("main"), None, "t");
        let p = parse::parse_create_trigger(definition).unwrap();
        TriggerDef {
            obj: table.child(ObjectKind::Trigger, "t_trg"),
            timing: oxbase_types::TriggerTiming::After,
            events: vec![oxbase_types::TriggerEvent::Insert],
            update_columns: vec![],
            level: oxbase_types::TriggerLevel::Row,
            condition: p.condition,
            function: None,
            body: Some(p.body),
            definition: Some(definition.to_owned()),
            enabled: true,
            comment: None,
            extras: Extras::new(),
        }
    }

    fn sqls(script: &Script) -> Vec<&str> {
        script.ordered().map(|s| s.sql.as_str()).collect()
    }

    #[test]
    fn create_quotes_names_and_literals() {
        let mut t = TableDef::new(ObjectRef::table(Some("main"), None, "order"));
        let mut id = col("id", "INTEGER").not_null();
        id.auto_increment = true;
        id.extras.insert(AUTOINCREMENT, true);
        t.columns.push(id);
        let mut quoted = col("a\"b", "TEXT").not_null();
        quoted.default = Some(AnsiQuoting.quote_str("it's"));
        quoted.collation = Some("NOCASE".into());
        t.columns.push(quoted);
        let mut unicode = col("名前", "VARCHAR(20)");
        unicode.default = Some(AnsiQuoting.quote_str("C:\\path ü"));
        t.columns.push(unicode);
        let mut reserved = col("select", "DECIMAL(10,2)");
        reserved.generated = Some(GeneratedColumn {
            expression: "id * 2".into(),
            stored: true,
        });
        t.columns.push(reserved);
        t.columns
            .push(ColumnDef::new("doc", DataType::Json { binary: false }));
        t.primary_key = Some(PrimaryKeyDef {
            name: Some("order_pk".into()),
            columns: vec!["id".into()],
        });
        let mut by_name = index("order_idx", &["名前"]);
        by_name.columns[0].descending = true;
        by_name.predicate = Some("\"select\" > 0".into());
        t.indexes.push(by_name);
        let mut unique = index("sqlite_autoindex_order_1", &["a\"b"]);
        unique.unique = true;
        unique.is_constraint = true;
        t.indexes.push(unique);
        t.checks.push(CheckDef {
            name: Some("order_chk".into()),
            expression: "\"select\" >= 0".into(),
            extras: Extras::new(),
        });
        t.foreign_keys.push(ForeignKeyDef {
            name: Some("order_fk".into()),
            columns: vec!["名前".into()],
            ref_table: ObjectRef::table(Some("main"), None, "parent"),
            ref_columns: vec!["code".into()],
            on_delete: FkAction::Cascade,
            on_update: FkAction::NoAction,
            extras: Extras::new()
                .with(props::DEFERRABLE, true)
                .with(props::INITIALLY_DEFERRED, true),
        });
        let version = v("3.51.0");
        let script = SqliteDdl
            .create(&ObjectDef::Table(t), &DdlContext::new(&version))
            .unwrap();
        assert_eq!(
            sqls(&script),
            [
                "CREATE TABLE \"main\".\"order\" (\n    \
                 \"id\" INTEGER CONSTRAINT \"order_pk\" PRIMARY KEY AUTOINCREMENT NOT NULL,\n    \
                 \"a\"\"b\" TEXT NOT NULL DEFAULT ('it''s') COLLATE NOCASE,\n    \
                 \"名前\" VARCHAR(20) DEFAULT ('C:\\path ü'),\n    \
                 \"select\" DECIMAL(10,2) GENERATED ALWAYS AS (id * 2) STORED,\n    \
                 \"doc\" JSON,\n    \
                 UNIQUE (\"a\"\"b\"),\n    \
                 CONSTRAINT \"order_chk\" CHECK (\"select\" >= 0),\n    \
                 CONSTRAINT \"order_fk\" FOREIGN KEY (\"名前\") REFERENCES \"parent\" (\"code\") \
                 ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED\n)",
                "CREATE INDEX \"main\".\"order_idx\" ON \"order\" (\"名前\" DESC) WHERE \"select\" > 0",
            ]
        );
        assert!(!script.is_destructive() && script.is_atomic());
        assert_eq!(script.steps[0].description, "Create table order");
    }

    #[test]
    fn derived_types_map_back_to_their_neutral_type() {
        for dt in [
            DataType::Bool,
            DataType::INT16,
            DataType::INT32,
            DataType::INT64,
            DataType::FLOAT64,
            DataType::Decimal {
                precision: Some(12),
                scale: Some(2),
            },
            DataType::Decimal {
                precision: None,
                scale: None,
            },
            DataType::VarChar { length: Some(20) },
            DataType::Char { length: Some(3) },
            DataType::Text,
            DataType::VarBinary { length: None },
            DataType::Date,
            DataType::Timestamp { precision: None },
            DataType::Uuid,
            DataType::Json { binary: false },
        ] {
            let text = derived_type(&dt, false);
            assert_eq!(super::super::types::data_type(&text), dt, "{text}");
        }
        assert_eq!(derived_type(&DataType::INT64, true), "INT");
        assert!(check_type_name("UNSIGNED BIG INT").is_ok());
        assert!(check_type_name("DECIMAL(10, 2)").is_ok());
        assert!(check_type_name("INT PRIMARY KEY").is_err());
        assert!(check_type_name("INT); DROP TABLE x; --").is_err());
        assert!(check_type_name("TEXT NOT NULL").is_err());
        assert!(is_constant_default("-1.5"));
        assert!(is_constant_default("('it''s')"));
        assert!(is_constant_default("X'00ff'"));
        assert!(is_constant_default("null"));
        assert!(!is_constant_default("CURRENT_TIMESTAMP"));
        assert!(!is_constant_default("'a' || 'b'"));
        assert!(!is_constant_default("1 + 2"));
    }

    #[test]
    fn native_changes_stay_in_place() {
        let old = base();
        let mut new = base();
        new.obj.name = "t2".into();
        new.columns[2].name = "n o\"te".into();
        new.indexes.clear();
        new.columns.remove(1);
        let mut added = col("c", "INTEGER").not_null();
        added.default = Some("0".into());
        new.columns.push(added);
        new.indexes.push(index("t_c", &["c"]));
        let renames = [
            Rename::new(ObjectKind::Table, "t", "t2"),
            Rename::new(ObjectKind::Column, "note", "n o\"te"),
        ];
        let version = v("3.51.0");
        let ctx = DdlContext::new(&version).with_renames(&renames);
        let script = SqliteDdl
            .alter(&ObjectDef::Table(old), &ObjectDef::Table(new), &ctx)
            .unwrap();
        assert_eq!(
            sqls(&script),
            [
                "ALTER TABLE \"main\".\"t\" RENAME TO \"t2\"",
                "ALTER TABLE \"main\".\"t2\" RENAME COLUMN \"note\" TO \"n o\"\"te\"",
                "DROP INDEX \"main\".\"t_qty\"",
                "ALTER TABLE \"main\".\"t2\" DROP COLUMN \"qty\"",
                "ALTER TABLE \"main\".\"t2\" ADD COLUMN \"c\" INTEGER NOT NULL DEFAULT (0)",
                "CREATE INDEX \"main\".\"t_c\" ON \"t2\" (\"c\")",
            ]
        );
        let destructive: Vec<bool> = script.steps.iter().map(|s| s.destructive).collect();
        assert_eq!(destructive, [false, false, true, true, false, false]);
        assert!(script.is_atomic());
        assert!(script.steps.iter().all(|s| s.phase == StepPhase::Main));

        // Nothing changed: nothing to do.
        let same = SqliteDdl
            .alter(
                &ObjectDef::Table(base()),
                &ObjectDef::Table(base()),
                &DdlContext::new(&version),
            )
            .unwrap();
        assert_eq!(same.steps, []);
    }

    #[test]
    fn other_changes_rebuild_the_table() {
        let old = base();
        let mut new = base();
        new.columns[1].native_type = "REAL".into();
        new.columns[1].data_type = DataType::FLOAT64;
        new.columns.remove(2);
        new.checks.push(CheckDef {
            name: Some("t_qty_chk".into()),
            expression: "qty >= 0".into(),
            extras: Extras::new(),
        });
        let triggers = [trigger(
            "CREATE TRIGGER t_trg AFTER INSERT ON t BEGIN SELECT 1; END",
        )];
        let version = v("3.51.0");
        let ctx = DdlContext::new(&version)
            .with_table(&old)
            .with_triggers(&triggers);
        let script = SqliteDdl
            .alter(&ObjectDef::Table(old.clone()), &ObjectDef::Table(new), &ctx)
            .unwrap();
        assert_eq!(
            sqls(&script),
            [
                "PRAGMA foreign_keys",
                "PRAGMA foreign_keys = OFF",
                "PRAGMA legacy_alter_table",
                "CREATE TABLE \"main\".\"t__oxbase_new\" (\n    \
                 \"id\" INTEGER PRIMARY KEY NOT NULL,\n    \
                 \"qty\" REAL DEFAULT (1),\n    \
                 CONSTRAINT \"t_qty_chk\" CHECK (qty >= 0)\n)",
                "INSERT INTO \"main\".\"t__oxbase_new\" (\"id\", \"qty\") SELECT \"id\", \
                 CAST(\"qty\" AS REAL) FROM \"main\".\"t\"",
                "DROP TABLE \"main\".\"t\"",
                "PRAGMA legacy_alter_table = ON",
                "ALTER TABLE \"main\".\"t__oxbase_new\" RENAME TO \"t\"",
                "CREATE INDEX \"main\".\"t_qty\" ON \"t\" (\"qty\")",
                "CREATE TRIGGER t_trg AFTER INSERT ON t BEGIN SELECT 1; END",
                "PRAGMA \"main\".foreign_key_check(\"t\")",
                "PRAGMA foreign_keys = ON",
                "PRAGMA legacy_alter_table = OFF",
            ]
        );
        let ordered: Vec<&ScriptStep> = script.ordered().collect();
        assert_eq!(ordered[0].capture.as_deref(), Some("fk"));
        assert_eq!(ordered[2].capture.as_deref(), Some("legacy_alter"));
        assert!(ordered[..3].iter().all(|s| s.phase == StepPhase::Prepare));
        assert!(
            ordered[3..11]
                .iter()
                .all(|s| s.phase == StepPhase::Main && s.destructive)
        );
        let check = ordered[10];
        assert_eq!(check.check, oxbase_types::StepCheck::ExpectNoRows);
        assert_eq!(check.only_if.as_ref().map(|c| c.equals.as_str()), Some("1"));
        assert!(ordered[11..].iter().all(|s| s.phase == StepPhase::Finally));
        assert_eq!(
            ordered[12].only_if.as_ref().map(|c| c.equals.as_str()),
            Some("0")
        );
        assert!(script.is_atomic() && script.is_destructive());
        assert!(script.warnings.iter().any(|w| w.contains("rebuilt")));

        // A trigger without its definition cannot be re-created.
        let mut bare = triggers[0].clone();
        bare.definition = None;
        let bare = [bare];
        let ctx = DdlContext::new(&version).with_triggers(&bare);
        let mut changed = base();
        changed.columns[1].nullable = false;
        assert!(
            SqliteDdl
                .alter(&ObjectDef::Table(base()), &ObjectDef::Table(changed), &ctx)
                .is_err()
        );
    }

    #[test]
    fn rebuild_after_renames_rewrites_triggers_and_keeps_rowids() {
        let mut old = base();
        old.primary_key = None;
        old.columns[0].auto_increment = false;
        old.obj.catalog = Some("aux".into());
        let mut new = old.clone();
        new.obj.name = "u".into();
        new.columns[1].name = "amount".into();
        new.indexes[0].columns[0].name = "amount".into();
        new.columns[2].nullable = false;
        new.columns[2].default = Some("''".into());
        let renames = [
            Rename::new(ObjectKind::Table, "t", "u"),
            Rename::new(ObjectKind::Column, "qty", "amount"),
        ];
        let triggers = [trigger(
            "CREATE TRIGGER t_trg AFTER UPDATE OF qty ON t BEGIN UPDATE t SET note = 'x'; END",
        )];
        let version = v("3.51.0");
        let ctx = DdlContext::new(&version)
            .with_renames(&renames)
            .with_triggers(&triggers);
        let script = SqliteDdl
            .alter(
                &ObjectDef::Table(old.clone()),
                &ObjectDef::Table(new.clone()),
                &ctx,
            )
            .unwrap();
        let all = sqls(&script);
        assert_eq!(all[3], "ALTER TABLE \"aux\".\"t\" RENAME TO \"u\"");
        assert_eq!(
            all[4],
            "ALTER TABLE \"aux\".\"u\" RENAME COLUMN \"qty\" TO \"amount\""
        );
        assert!(all.contains(
            &"INSERT INTO \"aux\".\"u__oxbase_new\" (rowid, \"id\", \"amount\", \"note\") \
              SELECT rowid, \"id\", \"amount\", \"note\" FROM \"aux\".\"u\""
        ));
        assert!(all.contains(
            &"CREATE TRIGGER \"aux\".t_trg AFTER UPDATE OF \"amount\" ON \"u\" BEGIN UPDATE \
              \"u\" SET note = 'x'; END"
        ));
        assert!(all.contains(&"PRAGMA \"aux\".foreign_key_check(\"u\")"));
        assert!(script.warnings.iter().any(|w| w.contains("review")));
        assert!(!script.steps[3].destructive && !script.steps[4].destructive);

        // Before 3.25 the column rename is done by the copy.
        let old_lib = v("3.24.0");
        let ctx = DdlContext::new(&old_lib)
            .with_renames(&renames)
            .with_triggers(&triggers);
        let script = SqliteDdl
            .alter(&ObjectDef::Table(old), &ObjectDef::Table(new), &ctx)
            .unwrap();
        let all = sqls(&script);
        assert!(!all.iter().any(|s| s.contains("RENAME COLUMN")));
        assert!(all.contains(
            &"INSERT INTO \"aux\".\"u__oxbase_new\" (rowid, \"id\", \"amount\", \"note\") \
              SELECT rowid, \"id\", \"qty\", \"note\" FROM \"aux\".\"u\""
        ));
    }

    #[test]
    fn drop_and_add_column_limits() {
        let version = v("3.51.0");
        let ctx = DdlContext::new(&version);
        // Dropping an indexed column drops the index first; a column in a check rebuilds.
        let mut new = base();
        new.columns.remove(1);
        new.indexes.clear();
        let script = SqliteDdl
            .alter(
                &ObjectDef::Table(base()),
                &ObjectDef::Table(new.clone()),
                &ctx,
            )
            .unwrap();
        assert_eq!(
            sqls(&script),
            [
                "DROP INDEX \"main\".\"t_qty\"",
                "ALTER TABLE \"main\".\"t\" DROP COLUMN \"qty\""
            ]
        );
        let mut checked = base();
        checked.checks.push(CheckDef {
            name: None,
            expression: "qty > 0".into(),
            extras: Extras::new(),
        });
        let mut new_checked = new.clone();
        new_checked.checks = checked.checks.clone();
        new_checked.checks[0].expression = "1".into();
        let script = SqliteDdl
            .alter(
                &ObjectDef::Table(checked),
                &ObjectDef::Table(new_checked),
                &ctx,
            )
            .unwrap();
        assert!(sqls(&script).contains(&"PRAGMA foreign_keys = OFF"));
        // Before 3.35 every drop rebuilds.
        let old_lib = v("3.34.0");
        let script = SqliteDdl
            .alter(
                &ObjectDef::Table(base()),
                &ObjectDef::Table(new),
                &DdlContext::new(&old_lib),
            )
            .unwrap();
        assert!(!sqls(&script).iter().any(|s| s.contains("DROP COLUMN")));

        // A NOT NULL column without a default, or with a non-constant one, rebuilds.
        for default in [None, Some("CURRENT_TIMESTAMP")] {
            let mut add = base();
            let mut c = col("at", "TEXT").not_null();
            c.default = default.map(str::to_owned);
            add.columns.push(c);
            let script = SqliteDdl
                .alter(&ObjectDef::Table(base()), &ObjectDef::Table(add), &ctx)
                .unwrap();
            assert!(sqls(&script).contains(&"PRAGMA foreign_keys = OFF"));
        }
        // A column added before existing ones rebuilds.
        let mut first = base();
        first.columns.insert(0, col("z", "TEXT"));
        let script = SqliteDdl
            .alter(&ObjectDef::Table(base()), &ObjectDef::Table(first), &ctx)
            .unwrap();
        assert!(sqls(&script).contains(&"PRAGMA foreign_keys = OFF"));
    }

    #[test]
    fn unsupported_and_invalid_designs() {
        let version = v("3.51.0");
        let ctx = DdlContext::new(&version);
        let create = |t: TableDef| SqliteDdl.create(&ObjectDef::Table(t), &ctx);
        let category = |r: Result<Script>| r.unwrap_err().category();

        let mut commented = base();
        commented.comment = Some("x".into());
        assert_eq!(
            category(create(commented)),
            oxbase_types::ErrorCategory::Unsupported
        );
        let mut identity = base();
        identity.columns[1].identity = Some(IdentityDef {
            always: true,
            start: None,
            increment: None,
        });
        assert_eq!(
            category(create(identity)),
            oxbase_types::ErrorCategory::Unsupported
        );
        let mut named_unique = base();
        let mut u = index("t_code_key", &["note"]);
        u.is_constraint = true;
        u.unique = true;
        named_unique.indexes.push(u);
        assert_eq!(
            category(create(named_unique)),
            oxbase_types::ErrorCategory::Unsupported
        );
        let mut prefix = base();
        prefix.indexes[0].columns[0].length = Some(3);
        assert_eq!(
            category(create(prefix)),
            oxbase_types::ErrorCategory::Unsupported
        );
        let mut virtual_table = base();
        virtual_table.extras.insert(MODULE, "fts5");
        assert_eq!(
            category(create(virtual_table)),
            oxbase_types::ErrorCategory::Unsupported
        );
        let mut strict = base();
        strict.extras.insert(props::STRICT, true);
        assert!(create(strict.clone()).is_ok());
        let old_lib = v("3.36.0");
        assert!(
            SqliteDdl
                .create(&ObjectDef::Table(strict), &DdlContext::new(&old_lib))
                .is_err()
        );
        let mut bad_type = base();
        bad_type.columns[2].native_type = "TEXT NOT NULL".into();
        assert_eq!(
            category(create(bad_type)),
            oxbase_types::ErrorCategory::InvalidInput
        );
        let mut bad_auto = base();
        bad_auto.columns[0].native_type = "BIGINT".into();
        assert_eq!(
            category(create(bad_auto)),
            oxbase_types::ErrorCategory::InvalidInput
        );
        let mut other_db = base();
        other_db.foreign_keys.push(ForeignKeyDef {
            name: None,
            columns: vec!["qty".into()],
            ref_table: ObjectRef::table(Some("aux"), None, "p"),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::NoAction,
            on_update: FkAction::NoAction,
            extras: Extras::new(),
        });
        assert_eq!(
            category(create(other_db)),
            oxbase_types::ErrorCategory::Unsupported
        );
    }

    #[test]
    fn child_kinds_change_their_table() {
        let version = v("3.51.0");
        let table = base();
        let tref = table.obj.clone();
        let ctx = DdlContext::new(&version).with_table(&table);

        let mut c = col("extra", "TEXT");
        c.default = Some("'x'".into());
        let script = SqliteDdl
            .create(
                &ObjectDef::Column {
                    table: tref.clone(),
                    column: c,
                },
                &ctx,
            )
            .unwrap();
        assert_eq!(
            sqls(&script),
            ["ALTER TABLE \"main\".\"t\" ADD COLUMN \"extra\" TEXT DEFAULT ('x')"]
        );

        // An index needs no table definition.
        let idx = ObjectDef::Index {
            table: tref.clone(),
            index: index("t_note", &["note"]),
        };
        let script = SqliteDdl.create(&idx, &DdlContext::new(&version)).unwrap();
        assert_eq!(
            sqls(&script),
            ["CREATE INDEX \"main\".\"t_note\" ON \"t\" (\"note\")"]
        );
        let mut unique = index("t_note", &["note"]);
        unique.unique = true;
        let changed = ObjectDef::Index {
            table: tref.clone(),
            index: unique,
        };
        let script = SqliteDdl
            .alter(&idx, &changed, &DdlContext::new(&version))
            .unwrap();
        assert_eq!(
            sqls(&script),
            [
                "DROP INDEX \"main\".\"t_note\"",
                "CREATE UNIQUE INDEX \"main\".\"t_note\" ON \"t\" (\"note\")"
            ]
        );
        let script = SqliteDdl
            .drop_object(
                &tref.child(ObjectKind::Index, "t_qty"),
                &DdlContext::new(&version),
            )
            .unwrap();
        assert_eq!(sqls(&script), ["DROP INDEX \"main\".\"t_qty\""]);
        assert!(script.is_destructive());

        // Checks and foreign keys rebuild; they need the table.
        let check = ObjectDef::Check {
            table: tref.clone(),
            check: CheckDef {
                name: Some("t_chk".into()),
                expression: "qty > 0".into(),
                extras: Extras::new(),
            },
        };
        assert!(
            SqliteDdl
                .create(&check, &DdlContext::new(&version))
                .is_err()
        );
        let script = SqliteDdl.create(&check, &ctx).unwrap();
        assert!(
            sqls(&script)
                .iter()
                .any(|s| s.contains("CONSTRAINT \"t_chk\" CHECK (qty > 0)"))
        );
        let mut with_check = base();
        if let ObjectDef::Check { check: def, .. } = &check {
            with_check.checks.push(def.clone());
        }
        let ctx_checked = DdlContext::new(&version).with_table(&with_check);
        let script = SqliteDdl
            .drop_object(&tref.child(ObjectKind::Check, "t_chk"), &ctx_checked)
            .unwrap();
        assert!(!sqls(&script).iter().any(|s| s.contains("CHECK")));

        let script = SqliteDdl
            .drop_object(&tref.child(ObjectKind::Column, "note"), &ctx)
            .unwrap();
        assert_eq!(
            sqls(&script),
            ["ALTER TABLE \"main\".\"t\" DROP COLUMN \"note\""]
        );
        let script = SqliteDdl
            .drop_object(&tref, &DdlContext::new(&version))
            .unwrap();
        assert_eq!(sqls(&script), ["DROP TABLE \"main\".\"t\""]);
        assert!(
            SqliteDdl
                .drop_object(&ObjectRef::new(ObjectKind::View, "v"), &ctx)
                .is_err()
        );
    }
}
