//! PostgreSQL [`CompareRules`] (M3): what PostgreSQL spells its own way.
//!
//! - **Types**: aliases fold to the name `format_type` prints (`int4` / `int` → `integer`,
//!   `varchar(20)` → `character varying(20)`, `timestamptz` → `timestamp with time zone`,
//!   `serial` → `integer`, `decimal(10, 2)` → `numeric(10,2)`, `int[]` → `integer[]`), see
//!   [`canonical_type`].
//! - **Expressions** (defaults, checks, generated columns, index predicates and expression
//!   keys): the server stores a parsed form and prints it back with casts and parentheses of its
//!   own (`'x'::text`, `char_length(code::text) > 0`, `'-1'::integer`, `(amount * 2::numeric)`),
//!   so both sides are normalized before comparing, see [`normalize_expression`].
//!
//! Everything else (the walk, property ids, order) is `oxbase_dialect::compare`.

#![allow(
    clippy::many_single_char_names,
    reason = "a hand-written lexer reads best with short index names"
)]

use oxbase_dialect::Comparator;
use oxbase_dialect::compare::{CompareRules, diff_objects, normalize_ws};
use oxbase_types::{
    CheckDef, ColumnDef, Extras, ForeignKeyDef, IndexDef, ObjectDef, PropId, PropertyDiff,
    TableDef, Value, props,
};

use super::ddl::neutral_type;
use super::fetch::{
    INDEX_OPTIONS, KEY_COLLATIONS, MATCH_TYPE, NO_INHERIT, NOT_ENFORCED, NOT_VALID, NULLS_FIRST,
    NULLS_NOT_DISTINCT, OPCLASS_OPTIONS, OPCLASSES,
};

/// The PostgreSQL [`Comparator`] for the designer kinds: [`normalize_def`] on both sides, then
/// the shared walk (`oxbase_dialect::compare::diff_objects`) with [`PgRules`].
///
/// The shared walk compares an extra only when both sides have it, so an extra the designer
/// sets on an object fetched without it (or a fetched extra the designer's definition leaves
/// out) would never show as a difference. Filling in PostgreSQL's default first makes "absent"
/// mean "the default" on both sides.
#[derive(Debug, Clone, Copy, Default)]
pub struct PgComparator;

impl Comparator for PgComparator {
    fn diff(&self, a: &ObjectDef, b: &ObjectDef) -> Vec<PropertyDiff> {
        diff_objects(&normalize_def(a), &normalize_def(b), &PgRules)
    }
}

/// `def` with the PostgreSQL default filled in for every extra the designer edits:
///
/// - table: `unlogged` and `temporary` `false`; `tablespace`, `partition_by` and `table_type`
///   `null`;
/// - index: `index_method` `btree`; `nulls_not_distinct`, `not_valid`, `opclass_options`,
///   `deferrable` and `initially_deferred` `false`; `include_columns` and `index_options` `[]`;
///   `tablespace` `null`; `nulls_first`, `opclasses` and `key_collations` one entry per key
///   (`nulls_first` defaults to the key's `descending`, the others to `null`);
/// - foreign key: `deferrable`, `initially_deferred`, `not_valid`, `not_enforced` `false`,
///   `match_type` `simple`;
/// - check: `no_inherit`, `not_valid`, `not_enforced` `false`.
///
/// `initially_deferred` implies `deferrable`, and `not_enforced` makes `not_valid` `false` (as
/// the server reports them). Other kinds are returned unchanged.
#[must_use]
pub fn normalize_def(def: &ObjectDef) -> ObjectDef {
    let mut out = def.clone();
    match &mut out {
        ObjectDef::Table(t) => normalize_table(t),
        ObjectDef::Index { index, .. } => normalize_index(index),
        ObjectDef::ForeignKey { foreign_key, .. } => normalize_fk(foreign_key),
        ObjectDef::Check { check, .. } => normalize_check(check),
        _ => {}
    }
    out
}

/// Sets `prop` to `value` when it is absent or `null`.
fn fill(extras: &mut Extras, prop: &PropId, value: impl Into<Value>) {
    if matches!(extras.get(prop), None | Some(Value::Null)) {
        extras.insert(prop.clone(), value);
    }
}

fn is_true(extras: &Extras, prop: &PropId) -> bool {
    extras.get(prop).and_then(Value::as_bool).unwrap_or(false)
}

/// `deferrable` / `initially_deferred` defaults; deferred implies deferrable.
fn fill_deferrable(extras: &mut Extras) {
    fill(extras, &props::INITIALLY_DEFERRED, false);
    if is_true(extras, &props::INITIALLY_DEFERRED) {
        extras.insert(props::DEFERRABLE, true);
    }
    fill(extras, &props::DEFERRABLE, false);
}

/// `not_valid` / `not_enforced` defaults; a constraint that is not enforced is not "not valid".
fn fill_validity(extras: &mut Extras) {
    fill(extras, &NOT_ENFORCED, false);
    if is_true(extras, &NOT_ENFORCED) {
        extras.insert(NOT_VALID, false);
    }
    fill(extras, &NOT_VALID, false);
}

/// [`normalize_def`] of a table and everything in it.
pub(crate) fn normalize_table(t: &mut TableDef) {
    fill(&mut t.extras, &props::UNLOGGED, false);
    fill(&mut t.extras, &props::TEMPORARY, false);
    for prop in [props::TABLESPACE, props::PARTITION_BY, props::TABLE_TYPE] {
        if t.extras.get(&prop).is_none() {
            t.extras.insert(prop, Value::Null);
        }
    }
    for index in &mut t.indexes {
        normalize_index(index);
    }
    for fk in &mut t.foreign_keys {
        normalize_fk(fk);
    }
    for check in &mut t.checks {
        normalize_check(check);
    }
}

/// One entry per key column of a per-key JSON array extra: the given entry, else `default(k)`.
/// Left alone when it is not an array (the generator reports that).
fn per_key(
    extras: &mut Extras,
    prop: &PropId,
    keys: usize,
    default: impl Fn(usize) -> serde_json::Value,
) {
    let given: Vec<serde_json::Value> = match extras.get(prop) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Json(serde_json::Value::Array(items))) => items.clone(),
        Some(_) => return,
    };
    let full: Vec<serde_json::Value> = (0..keys.max(given.len()))
        .map(|k| match given.get(k) {
            Some(v) if !v.is_null() && v.as_str() != Some("") => v.clone(),
            _ => default(k),
        })
        .collect();
    extras.insert(prop.clone(), Value::Json(serde_json::Value::Array(full)));
}

/// [`normalize_def`] of an index.
pub(crate) fn normalize_index(i: &mut IndexDef) {
    let e = &mut i.extras;
    fill(e, &props::INDEX_METHOD, "btree");
    fill(e, &NULLS_NOT_DISTINCT, false);
    fill(e, &NOT_VALID, false);
    fill(e, &OPCLASS_OPTIONS, false);
    fill_deferrable(e);
    for prop in [props::INCLUDE_COLUMNS, INDEX_OPTIONS] {
        fill(e, &prop, Value::Json(serde_json::Value::Array(Vec::new())));
    }
    if e.get(&props::TABLESPACE).is_none() {
        e.insert(props::TABLESPACE, Value::Null);
    }
    let descending: Vec<bool> = i.columns.iter().map(|c| c.descending).collect();
    per_key(e, &NULLS_FIRST, descending.len(), |k| {
        serde_json::Value::Bool(descending.get(k).copied().unwrap_or(false))
    });
    for prop in [OPCLASSES, KEY_COLLATIONS] {
        per_key(e, &prop, descending.len(), |_| serde_json::Value::Null);
    }
}

/// [`normalize_def`] of a foreign key.
pub(crate) fn normalize_fk(fk: &mut ForeignKeyDef) {
    fill_deferrable(&mut fk.extras);
    fill_validity(&mut fk.extras);
    fill(&mut fk.extras, &MATCH_TYPE, "simple");
}

/// [`normalize_def`] of a check.
pub(crate) fn normalize_check(check: &mut CheckDef) {
    fill(&mut check.extras, &NO_INHERIT, false);
    fill_validity(&mut check.extras);
}

/// The PostgreSQL normalization rules, registered as `RulesComparator(PgRules)` for the
/// designer kinds.
#[derive(Debug, Clone, Copy, Default)]
pub struct PgRules;

impl CompareRules for PgRules {
    fn type_text(&self, c: &ColumnDef) -> String {
        column_type_key(c)
    }

    fn same_type(&self, old: &ColumnDef, new: &ColumnDef) -> bool {
        column_type_key(old) == column_type_key(new)
    }

    fn same_expression(&self, old: &str, new: &str) -> bool {
        normalize_expression(old) == normalize_expression(new)
    }
}

/// The canonical type of a column: its native type, else the PostgreSQL spelling of its neutral
/// type.
pub(crate) fn column_type_key(c: &ColumnDef) -> String {
    if c.native_type.trim().is_empty() {
        neutral_type(&c.data_type).map_or_else(|_| c.data_type.to_string(), |t| canonical_type(&t))
    } else {
        canonical_type(&c.native_type)
    }
}

/// `true` for the `serial` pseudo-types (`serial`, `bigserial`, `smallserial`, `serial4`, ...).
pub(crate) fn is_serial_type(native: &str) -> bool {
    matches!(
        lower_outside_quotes(&normalize_ws(native)).as_str(),
        "serial" | "serial4" | "bigserial" | "serial8" | "smallserial" | "serial2"
    )
}

/// Lowercases `text` except inside double-quoted identifiers.
fn lower_outside_quotes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut quoted = false;
    for c in text.chars() {
        if c == '"' {
            quoted = !quoted;
        }
        if quoted {
            out.push(c);
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// The type name as `format_type` prints it, for comparing types written in different ways.
///
/// Aliases fold to one spelling, whitespace and case are normalized (outside quoted names),
/// modifiers lose their spaces (`numeric(10, 2)` → `numeric(10,2)`) and go where PostgreSQL
/// puts them (`timestamp(3) with time zone`). Unknown names (domains, enums, extension types)
/// are only lowercased and trimmed.
pub fn canonical_type(native: &str) -> String {
    let text = lower_outside_quotes(&normalize_ws(native));
    // Array suffixes: `integer[]`, `int[3][]`, `integer array`.
    let mut base = text.as_str();
    let mut dims = 0;
    loop {
        let trimmed = base.trim_end();
        if let Some(rest) = trimmed.strip_suffix(']')
            && let Some(open) = rest.rfind('[')
            && rest[open + 1..].chars().all(|c| c.is_ascii_digit())
        {
            base = &rest[..open];
            dims += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" array") {
            base = rest;
            dims += 1;
            continue;
        }
        break;
    }
    let mut out = canonical_scalar(base.trim());
    for _ in 0..dims {
        out.push_str("[]");
    }
    out
}

/// [`canonical_type`] of a type without array suffixes.
fn canonical_scalar(text: &str) -> String {
    if text.starts_with('"') {
        return text.to_owned();
    }
    // Split `name(args) rest` into its parts.
    let (head, args, tail) = match (text.find('('), text.rfind(')')) {
        (Some(open), Some(close)) if open < close => (
            text[..open].trim(),
            Some(
                text[open + 1..close]
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>(),
            ),
            text[close + 1..].trim(),
        ),
        _ => (text, None, ""),
    };
    let words = if tail.is_empty() {
        head.to_owned()
    } else {
        format!("{head} {tail}")
    };
    let base: &str = match words.as_str() {
        "int" | "int4" | "integer" | "serial" | "serial4" => "integer",
        "int2" | "smallint" | "smallserial" | "serial2" => "smallint",
        "int8" | "bigint" | "bigserial" | "serial8" => "bigint",
        "float4" | "real" => "real",
        "float8" | "double precision" => "double precision",
        "float" => {
            return match args.as_deref().map(str::parse::<u32>) {
                Some(Ok(p)) if p <= 24 => "real".to_owned(),
                _ => "double precision".to_owned(),
            };
        }
        "bool" | "boolean" => "boolean",
        "varchar" | "character varying" | "char varying" => "character varying",
        "char" | "character" => {
            // `char` without a length is `character(1)`.
            return format!("character({})", args.as_deref().unwrap_or("1"));
        }
        "decimal" | "numeric" => "numeric",
        "timestamp" | "timestamp without time zone" => "timestamp without time zone",
        "timestamptz" | "timestamp with time zone" => "timestamp with time zone",
        "time" | "time without time zone" => "time without time zone",
        "timetz" | "time with time zone" => "time with time zone",
        "varbit" | "bit varying" => "bit varying",
        "bit" => return format!("bit({})", args.as_deref().unwrap_or("1")),
        other => other,
    };
    match args {
        None => base.to_owned(),
        // PostgreSQL prints `timestamp(3) with time zone`: the modifier after the first word.
        Some(args) => match base.split_once(' ') {
            Some((first, rest)) if rest.starts_with("with") => {
                format!("{first}({args}) {rest}")
            }
            _ => format!("{base}({args})"),
        },
    }
}

/// One lexical token of an expression.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// A string literal's content (quotes removed, doubled quotes undone).
    Str(String),
    /// A quoted identifier's content.
    QIdent(String),
    /// A keyword or unquoted identifier, lowercased.
    Word(String),
    Number(String),
    /// `::`
    Cast,
    /// A run of operator characters.
    Op(String),
    Open,
    Close,
    /// Any other single character (`,`, `.`, `[`, `]`).
    Punct(char),
}

fn tokenize(text: &str) -> Vec<Tok> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '\'' || c == '"' {
            let mut content = String::new();
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j] == c {
                    if chars.get(j + 1) == Some(&c) {
                        content.push(c);
                        j += 2;
                        continue;
                    }
                    break;
                }
                content.push(chars[j]);
                j += 1;
            }
            // An `E` prefix glued to a string is part of the literal.
            if c == '\''
                && let Some(Tok::Word(w)) = out.last()
                && w == "e"
                && i > 0
                && matches!(chars[i - 1], 'e' | 'E')
            {
                out.pop();
            }
            out.push(if c == '\'' {
                Tok::Str(content)
            } else {
                Tok::QIdent(content)
            });
            i = j + 1;
        } else if c == '$'
            && let Some(end) = dollar_string_end(&chars, i)
        {
            let (tag_len, body_end) = end;
            out.push(Tok::Str(chars[i + tag_len..body_end].iter().collect()));
            i = body_end + tag_len;
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            out.push(Tok::Word(word.to_lowercase()));
        } else if c.is_ascii_digit()
            || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit))
        {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            if i < chars.len() && matches!(chars[i], 'e' | 'E') {
                let mut j = i + 1;
                if j < chars.len() && matches!(chars[j], '+' | '-') {
                    j += 1;
                }
                if j < chars.len() && chars[j].is_ascii_digit() {
                    i = j;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            out.push(Tok::Number(chars[start..i].iter().collect()));
        } else if c == ':' && chars.get(i + 1) == Some(&':') {
            out.push(Tok::Cast);
            i += 2;
        } else if c == '(' {
            out.push(Tok::Open);
            i += 1;
        } else if c == ')' {
            out.push(Tok::Close);
            i += 1;
        } else if "+-*/<>=~!@#%^&|`?".contains(c) {
            let start = i;
            while i < chars.len() && "+-*/<>=~!@#%^&|`?".contains(chars[i]) {
                i += 1;
            }
            let op: String = chars[start..i].iter().collect();
            out.push(Tok::Op(if op == "!=" { "<>".to_owned() } else { op }));
        } else {
            out.push(Tok::Punct(c));
            i += 1;
        }
    }
    out
}

/// For a `$tag$ ... $tag$` string starting at `start`: (tag length, index of the closing tag).
fn dollar_string_end(chars: &[char], start: usize) -> Option<(usize, usize)> {
    let mut j = start + 1;
    while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
        j += 1;
    }
    if chars.get(j) != Some(&'$') {
        return None;
    }
    let tag = &chars[start..=j];
    let mut k = j + 1;
    while k + tag.len() <= chars.len() {
        if &chars[k..k + tag.len()] == tag {
            return Some((tag.len(), k));
        }
        k += 1;
    }
    None
}

/// Words that continue a multi-word type name after a cast (`::character varying`,
/// `::timestamp(3) with time zone`).
const TYPE_WORDS: [&str; 6] = ["varying", "precision", "without", "with", "time", "zone"];

/// Index just past the type name of a cast that starts at `i` (after `::`).
fn skip_type(toks: &[Tok], mut i: usize) -> usize {
    match toks.get(i) {
        Some(Tok::Word(_) | Tok::QIdent(_)) => i += 1,
        _ => return i,
    }
    // Schema-qualified names.
    while matches!(toks.get(i), Some(Tok::Punct('.')))
        && matches!(toks.get(i + 1), Some(Tok::Word(_) | Tok::QIdent(_)))
    {
        i += 2;
    }
    loop {
        match toks.get(i) {
            Some(Tok::Word(w)) if TYPE_WORDS.contains(&w.as_str()) => i += 1,
            Some(Tok::Open) => {
                let mut depth = 0usize;
                while let Some(t) = toks.get(i) {
                    match t {
                        Tok::Open => depth += 1,
                        Tok::Close => {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
            Some(Tok::Punct('[')) => {
                let mut j = i + 1;
                if matches!(toks.get(j), Some(Tok::Number(_))) {
                    j += 1;
                }
                if matches!(toks.get(j), Some(Tok::Punct(']'))) {
                    i = j + 1;
                } else {
                    return i;
                }
            }
            _ => return i,
        }
    }
}

fn is_plain_number(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    !body.is_empty()
        && body.chars().all(|c| c.is_ascii_digit() || c == '.')
        && body.chars().filter(|c| *c == '.').count() <= 1
        && body.chars().any(|c| c.is_ascii_digit())
}

fn is_simple_lower_ident(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Index of the token closing the group opened at `open`.
fn matching_close(toks: &[Tok], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (k, t) in toks.iter().enumerate().skip(open) {
        match t {
            Tok::Open => depth += 1,
            Tok::Close => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

fn is_word(t: Option<&Tok>, words: &[&str]) -> bool {
    matches!(t, Some(Tok::Word(w)) if words.contains(&w.as_str()))
}

/// `true` when `toks` (a group's content) has `and` / `or` outside nested parentheses.
fn has_top_level_bool(toks: &[Tok]) -> bool {
    let mut depth = 0usize;
    for t in toks {
        match t {
            Tok::Open => depth += 1,
            Tok::Close => depth = depth.saturating_sub(1),
            Tok::Word(w) if depth == 0 && (w == "and" || w == "or") => return true,
            _ => {}
        }
    }
    false
}

/// Removes parentheses PostgreSQL adds or users write that do not change the meaning: around
/// the whole expression, around a single token, and around a comparison that is an operand of
/// `AND` / `OR` / `NOT`.
fn strip_parens(mut toks: Vec<Tok>) -> Vec<Tok> {
    loop {
        let mut changed = false;
        let mut i = 0;
        while i < toks.len() {
            if toks[i] != Tok::Open {
                i += 1;
                continue;
            }
            let Some(close) = matching_close(&toks, i) else {
                return toks;
            };
            let before = if i == 0 { None } else { toks.get(i - 1) };
            let after = toks.get(close + 1);
            let call = matches!(before, Some(Tok::Word(_) | Tok::QIdent(_)))
                && !is_word(before, &["and", "or", "not", "when", "then", "else", "is"]);
            let inner = &toks[i + 1..close];
            let atom = inner.len() == 1;
            let whole = i == 0 && close == toks.len() - 1;
            // `(now())::date`, `(code)::text`: `::` binds tighter than anything but a call or a
            // single token.
            let cast_target = matches!(after, Some(Tok::Cast)) && (atom || is_call(inner));
            let bool_operand = !has_top_level_bool(inner)
                && (before.is_none()
                    || matches!(before, Some(Tok::Open))
                    || is_word(before, &["and", "or", "not"]))
                && (after.is_none()
                    || matches!(after, Some(Tok::Close))
                    || is_word(after, &["and", "or"]));
            if !inner.is_empty() && !call && (atom || whole || bool_operand || cast_target) {
                toks.remove(close);
                toks.remove(i);
                changed = true;
            } else {
                i += 1;
            }
        }
        if !changed {
            return toks;
        }
    }
}

/// `true` when `toks` is one call, `f(...)`.
fn is_call(toks: &[Tok]) -> bool {
    toks.len() >= 3
        && matches!(toks[0], Tok::Word(_) | Tok::QIdent(_))
        && toks[1] == Tok::Open
        && matching_close(toks, 1) == Some(toks.len() - 1)
}

/// Index of the token opening the group closed at `close`.
fn matching_open(toks: &[Tok], close: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (k, t) in toks.get(..=close)?.iter().enumerate().rev() {
        match t {
            Tok::Close => depth += 1,
            Tok::Open => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

/// What a `::type` cast applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CastOperand {
    /// A literal (`'x'`, `2`, `(-1)`, `NULL`): the server types every literal with a cast.
    Literal,
    /// A bare column (`code`, `(code)`): the server writes out implicit casts of columns.
    Column,
    /// Anything else (a call, an operator expression): only the user writes such casts.
    Other,
}

/// The operand of a cast that follows the normalized tokens `out`.
fn cast_operand(out: &[Tok]) -> CastOperand {
    match out.last() {
        Some(Tok::Str(_) | Tok::Number(_)) => CastOperand::Literal,
        Some(Tok::Word(w)) if matches!(w.as_str(), "null" | "true" | "false") => {
            CastOperand::Literal
        }
        Some(Tok::Word(_) | Tok::QIdent(_)) => CastOperand::Column,
        Some(Tok::Close) => {
            let close = out.len() - 1;
            let Some(open) = matching_open(out, close) else {
                return CastOperand::Other;
            };
            let before = open.checked_sub(1).map(|k| &out[k]);
            let call = matches!(before, Some(Tok::Word(_) | Tok::QIdent(_)))
                && !is_word(before, &["and", "or", "not", "when", "then", "else", "is"]);
            match &out[open + 1..close] {
                [single] if !call && *single != Tok::Close => {
                    cast_operand(std::slice::from_ref(single))
                }
                _ => CastOperand::Other,
            }
        }
        _ => CastOperand::Other,
    }
}

/// Types the server casts a column to on its own (an implicit cast written out by
/// `pg_get_expr`: `code::text` for a `varchar` column in a text function, `(qty)::numeric`
/// next to a decimal literal). Narrowing casts (`::integer`, `::date`) are the user's.
fn implicit_target(ty: &str) -> bool {
    let mut base = ty;
    while let Some(rest) = base.strip_suffix("[]") {
        base = rest;
    }
    matches!(
        base,
        "text"
            | "character varying"
            | "numeric"
            | "bigint"
            | "double precision"
            | "real"
            | "timestamp with time zone"
            | "timestamp without time zone"
    )
}

/// The text of a cast's type tokens, for [`canonical_type`].
fn type_text(toks: &[Tok]) -> String {
    let mut out = String::new();
    for t in toks {
        match t {
            Tok::Word(w) | Tok::Number(w) => {
                if matches!(t, Tok::Word(_))
                    && out
                        .chars()
                        .last()
                        .is_some_and(|c| c.is_alphanumeric() || c == ')' || c == '"' || c == '_')
                {
                    out.push(' ');
                }
                out.push_str(w);
            }
            Tok::QIdent(q) => {
                out.push('"');
                out.push_str(&q.replace('"', "\"\""));
                out.push('"');
            }
            Tok::Open => out.push('('),
            Tok::Close => out.push(')'),
            Tok::Punct(c) => out.push(*c),
            Tok::Str(_) | Tok::Cast | Tok::Op(_) => {}
        }
    }
    out
}

/// A canonical form of a PostgreSQL expression, for comparing an expression as written with
/// the text `pg_get_expr` prints back. Two expressions with the same canonical form mean the
/// same; the converse does not hold (the server may rewrite `IN (...)` as `= ANY (ARRAY[...])`,
/// for example).
///
/// It drops the `::type` casts the server adds on its own: casts of literals (`'x'::text`,
/// `(2)::numeric`, `NULL::integer`) and widening casts of a bare column (`code::text`,
/// `(qty)::numeric`, see `implicit_target`). Every other cast (`now()::date`, `amount::integer`)
/// is kept, with its type in [`canonical_type`] spelling. It also unquotes numeric string
/// literals (`'-1'::integer` is `-1`) and quoted identifiers that need no quotes, lowercases
/// keywords and names outside quotes, writes `!=` as `<>`, ignores whitespace, and removes
/// parentheses that do not change the meaning (see `strip_parens`).
pub fn normalize_expression(text: &str) -> String {
    let toks = tokenize(text);
    let mut out: Vec<Tok> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if toks[i] == Tok::Cast {
            let end = skip_type(&toks, i + 1);
            if end == i + 1 {
                out.push(Tok::Cast);
                i += 1;
                continue;
            }
            let ty = canonical_type(&type_text(&toks[i + 1..end]));
            let server_added = match cast_operand(&out) {
                CastOperand::Literal => true,
                CastOperand::Column => implicit_target(&ty),
                CastOperand::Other => false,
            };
            if !server_added {
                out.push(Tok::Cast);
                out.push(Tok::Word(ty));
            }
            i = end;
            continue;
        }
        let tok = match &toks[i] {
            Tok::Str(s) if is_plain_number(s) => Tok::Number(s.clone()),
            Tok::QIdent(s) if is_simple_lower_ident(s) => Tok::Word(s.clone()),
            other => other.clone(),
        };
        i += 1;
        // A unary minus glued to a number (`-1`, as `'-1'::integer` prints).
        if let Tok::Number(n) = &tok
            && !n.starts_with('-')
            && matches!(out.last(), Some(Tok::Op(minus)) if minus == "-")
        {
            let before = out.len().checked_sub(2).map(|k| &out[k]);
            let unary = matches!(before, None | Some(Tok::Open | Tok::Op(_) | Tok::Punct(_)));
            if unary {
                out.pop();
                out.push(Tok::Number(format!("-{n}")));
                continue;
            }
        }
        out.push(tok);
    }
    let out = strip_parens(out);
    let mut text = String::new();
    for (k, t) in out.iter().enumerate() {
        if k > 0 {
            text.push(' ');
        }
        match t {
            Tok::Str(s) => {
                text.push('\'');
                text.push_str(&s.replace('\'', "''"));
                text.push('\'');
            }
            Tok::QIdent(s) => {
                text.push('"');
                text.push_str(&s.replace('"', "\"\""));
                text.push('"');
            }
            Tok::Word(s) | Tok::Number(s) | Tok::Op(s) => text.push_str(s),
            Tok::Cast => text.push_str("::"),
            Tok::Open => text.push('('),
            Tok::Close => text.push(')'),
            Tok::Punct(c) => text.push(*c),
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_dialect::compare::{RulesComparator, diff_tables};
    use oxbase_types::{DataType, FkAction, IndexColumn, ObjectRef};

    #[test]
    fn type_aliases_fold_to_the_catalog_spelling() {
        for (written, printed) in [
            ("int", "integer"),
            ("INT4", "integer"),
            ("serial", "integer"),
            ("bigserial", "bigint"),
            ("int8", "bigint"),
            ("smallserial", "smallint"),
            ("float4", "real"),
            ("float", "double precision"),
            ("float(10)", "real"),
            ("float(53)", "double precision"),
            ("bool", "boolean"),
            ("varchar(20)", "character varying(20)"),
            ("VARCHAR", "character varying"),
            ("char", "character(1)"),
            ("char(3)", "character(3)"),
            ("decimal(10, 2)", "numeric(10,2)"),
            ("numeric", "numeric"),
            ("timestamp", "timestamp without time zone"),
            ("timestamptz(3)", "timestamp(3) with time zone"),
            ("timestamp(3) with time zone", "timestamp(3) with time zone"),
            ("time(0)", "time(0) without time zone"),
            ("timetz", "time with time zone"),
            ("int[]", "integer[]"),
            ("varchar(5)[]", "character varying(5)[]"),
            ("integer[3][]", "integer[][]"),
            ("int array", "integer[]"),
            ("varbit(4)", "bit varying(4)"),
            ("bit", "bit(1)"),
            ("m2.Mood", "m2.mood"),
            ("\"Weird\"", "\"Weird\""),
            ("  double   precision ", "double precision"),
        ] {
            assert_eq!(canonical_type(written), printed, "{written}");
        }
        assert!(is_serial_type("SERIAL"));
        assert!(is_serial_type("bigserial"));
        assert!(!is_serial_type("integer"));
    }

    #[test]
    fn columns_compare_by_canonical_type() {
        let rules = PgRules;
        let mut a = ColumnDef::new("c", DataType::VarChar { length: Some(20) });
        let mut b = a.clone();
        assert!(rules.same_type(&a, &b));
        b.native_type = "character varying(20)".into();
        assert!(
            rules.same_type(&a, &b),
            "neutral type spelled by PostgreSQL"
        );
        a.native_type = "varchar(20)".into();
        assert!(rules.same_type(&a, &b));
        b.native_type = "character varying(30)".into();
        assert!(!rules.same_type(&a, &b));
        assert_eq!(rules.type_text(&b), "character varying(30)");
    }

    #[test]
    fn expressions_compare_as_the_server_prints_them() {
        let same = |a: &str, b: &str| {
            assert_eq!(
                normalize_expression(a),
                normalize_expression(b),
                "{a} | {b}"
            );
        };
        let differ = |a: &str, b: &str| {
            assert_ne!(
                normalize_expression(a),
                normalize_expression(b),
                "{a} | {b}"
            );
        };
        same("'x'", "'x'::text");
        same("'it''s'", "'it''s'::character varying");
        same("-1", "'-1'::integer");
        same("0", "0");
        same("now()", "NOW( )");
        same("CURRENT_TIMESTAMP", "current_timestamp");
        same("qty >= 0", "(qty >= 0)");
        same("qty>=0", "((qty >= 0))");
        same("char_length(code) > 0", "char_length(code::text) > 0");
        same("amount * 2", "(amount * (2)::numeric)");
        same("\"qty\" <> 1", "qty != 1");
        same("a > 0 AND b > 0", "(a > 0) AND (b > 0)");
        same("nextval('m3.t_id_seq')", "nextval('m3.t_id_seq'::regclass)");
        same("'2020-01-01'::date", "'2020-01-01'");
        same(
            "'2020-01-01 00:00'::timestamp without time zone",
            "'2020-01-01 00:00'",
        );
        same("lower(name)", "lower((name)::text)");
        same("'{a}'::character varying(10)[]", "'{a}'");
        same("NULL::integer", "NULL");
        same("(-1)::integer", "-1");
        same("tags @> ARRAY['a']", "(tags)::text[] @> ARRAY['a'::text]");
        same("now()::date", "(now())::date");
        same("price::int > 0", "((price)::integer > 0)");
        same("x::timestamptz(3)", "(x)::timestamp(3) with time zone");
        // Casts the user wrote on calls, expressions and narrowing column casts stay.
        differ("amount::integer * 2", "amount * 2");
        differ("now()::date", "now()");
        differ("(price::int > 0)", "(price > 0)");
        differ("(a + b)::bigint", "a + b");
        differ("now()::date", "now()::timestamp");
        same("E'a\\b'", "E'a\\b'");
        differ("qty > 0", "qty >= 0");
        differ("(a OR b) AND c", "a OR b AND c");
        differ("'X'", "'x'");
        differ("\"Qty\" > 0", "qty > 0");
        differ("'a  b'", "'a b'");
        differ("f(a) + 1", "f(a + 1)");
    }

    #[test]
    fn comparator_fills_in_defaults_before_comparing() {
        let table = ObjectRef::table(None, Some("m3"), "t");
        let fetched = IndexDef {
            name: "t_i".into(),
            columns: vec![IndexColumn::column("a")],
            unique: false,
            is_constraint: false,
            predicate: None,
            extras: Extras::new().with(props::INDEX_METHOD, "btree"),
        };
        let index = |i: &IndexDef| ObjectDef::Index {
            table: table.clone(),
            index: i.clone(),
        };
        let cmp = PgComparator;
        assert_eq!(cmp.diff(&index(&fetched), &index(&fetched)), []);
        for (prop, value) in [
            (NULLS_NOT_DISTINCT, Value::Bool(true)),
            (
                props::INCLUDE_COLUMNS,
                Value::Json(serde_json::json!(["b"])),
            ),
            (NULLS_FIRST, Value::Json(serde_json::json!([true]))),
            (
                OPCLASSES,
                Value::Json(serde_json::json!(["text_pattern_ops"])),
            ),
            (KEY_COLLATIONS, Value::Json(serde_json::json!(["C"]))),
            (
                INDEX_OPTIONS,
                Value::Json(serde_json::json!(["fillfactor=70"])),
            ),
            (props::TABLESPACE, Value::Text("fast".into())),
        ] {
            let mut changed = fetched.clone();
            changed.extras.insert(prop.clone(), value);
            let diffs = cmp.diff(&index(&fetched), &index(&changed));
            assert_eq!(diffs.len(), 1, "{prop}: {diffs:?}");
            assert_eq!(diffs[0].prop.as_ref(), Some(&prop));
            // The shared walk alone misses a property one side does not have.
            assert_eq!(
                RulesComparator(PgRules).diff(&index(&fetched), &index(&changed)),
                [],
                "{prop}"
            );
        }
        // Restating the default is no difference.
        let mut restated = fetched.clone();
        restated
            .extras
            .insert(NULLS_FIRST, Value::Json(serde_json::json!([false])));
        restated.extras.insert(NULLS_NOT_DISTINCT, false);
        restated
            .extras
            .insert(OPCLASSES, Value::Json(serde_json::json!([null])));
        assert_eq!(cmp.diff(&index(&fetched), &index(&restated)), []);

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
        for (prop, value, expected) in [
            (MATCH_TYPE, Value::Text("full".into()), 1),
            (MATCH_TYPE, Value::Text("simple".into()), 0),
            (NOT_VALID, Value::Bool(true), 1),
            (props::DEFERRABLE, Value::Bool(true), 1),
            // Initially deferred implies deferrable: two properties change.
            (props::INITIALLY_DEFERRED, Value::Bool(true), 2),
        ] {
            let mut changed = fk.clone();
            changed.extras.insert(prop.clone(), value);
            assert_eq!(
                cmp.diff(&fk_def(&fk), &fk_def(&changed)).len(),
                expected,
                "{prop}"
            );
        }
        let check = CheckDef {
            name: Some("t_chk".into()),
            expression: "a > 0".into(),
            extras: Extras::new(),
        };
        let mut no_inherit = check.clone();
        no_inherit.extras.insert(NO_INHERIT, true);
        let check_def = |c: &CheckDef| ObjectDef::Check {
            table: table.clone(),
            check: c.clone(),
        };
        assert_eq!(
            cmp.diff(&check_def(&check), &check_def(&no_inherit)).len(),
            1
        );
        // Not enforced: never "not valid" as well.
        let mut not_enforced = check.clone();
        not_enforced.extras.insert(NOT_ENFORCED, true);
        let mut both = not_enforced.clone();
        both.extras.insert(NOT_VALID, true);
        assert_eq!(cmp.diff(&check_def(&not_enforced), &check_def(&both)), []);

        // Table: unlogged and tablespace, absent meaning the default.
        let t = TableDef::new(table.clone());
        let mut unlogged = t.clone();
        unlogged.extras.insert(props::UNLOGGED, true);
        assert_eq!(
            cmp.diff(&ObjectDef::Table(t.clone()), &ObjectDef::Table(unlogged))
                .len(),
            1
        );
        let mut spaced = t.clone();
        spaced.extras.insert(props::TABLESPACE, "fast");
        assert_eq!(
            cmp.diff(&ObjectDef::Table(spaced), &ObjectDef::Table(t))
                .len(),
            1
        );
    }

    #[test]
    fn tables_with_server_spellings_compare_equal() {
        let mut written = TableDef::new(ObjectRef::table(None, Some("m3"), "t"));
        let mut id = ColumnDef::new("id", DataType::INT32).not_null();
        id.native_type = "serial".into();
        id.auto_increment = true;
        written.columns.push(id);
        let mut code = ColumnDef::new("code", DataType::VarChar { length: Some(20) });
        code.native_type = "varchar(20)".into();
        code.default = Some("'it''s'".into());
        written.columns.push(code);
        written.checks.push(CheckDef {
            name: None,
            expression: "char_length(code) > 0".into(),
            extras: Extras::new(),
        });
        written.indexes.push(IndexDef {
            name: "t_lower_idx".into(),
            columns: vec![IndexColumn {
                name: "lower(code)".into(),
                is_expression: true,
                descending: false,
                length: None,
            }],
            unique: false,
            is_constraint: false,
            predicate: Some("code IS NOT NULL".into()),
            extras: Extras::new(),
        });

        let mut fetched = written.clone();
        fetched.obj.catalog = Some("oxbase_test".into());
        fetched.columns[0].native_type = "integer".into();
        fetched.columns[0].default = Some("nextval('m3.t_id_seq'::regclass)".into());
        fetched.columns[1].native_type = "character varying(20)".into();
        fetched.columns[1].default = Some("'it''s'::character varying".into());
        fetched.checks[0].name = Some("t_code_check".into());
        fetched.checks[0].expression = "char_length(code::text) > 0".into();
        fetched.indexes[0].columns[0].name = "lower(code::text)".into();
        fetched.indexes[0].predicate = Some("(code IS NOT NULL)".into());
        fetched.indexes[0]
            .extras
            .insert(props::INDEX_METHOD, "btree");
        assert_eq!(diff_tables(&written, &fetched, &PgRules), []);
        let cmp = RulesComparator(PgRules);
        assert_eq!(
            cmp.diff(
                &ObjectDef::Table(written.clone()),
                &ObjectDef::Table(fetched.clone())
            ),
            []
        );

        fetched.checks[0].expression = "char_length(code::text) > 1".into();
        let diffs = diff_tables(&written, &fetched, &PgRules);
        assert_eq!(diffs.len(), 2, "{diffs:?}"); // unnamed check: removed + added
    }
}
