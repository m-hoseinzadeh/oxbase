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

use oxbase_dialect::compare::{CompareRules, normalize_ws};
use oxbase_types::ColumnDef;

use super::ddl::neutral_type;

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
            let bool_operand = !has_top_level_bool(inner)
                && (before.is_none()
                    || matches!(before, Some(Tok::Open))
                    || is_word(before, &["and", "or", "not"]))
                && (after.is_none()
                    || matches!(after, Some(Tok::Close))
                    || is_word(after, &["and", "or"]));
            if !inner.is_empty() && !call && (atom || whole || bool_operand) {
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

/// A canonical form of a PostgreSQL expression, for comparing an expression as written with
/// the text `pg_get_expr` prints back. Two expressions with the same canonical form mean the
/// same; the converse does not hold (the server may rewrite `IN (...)` as `= ANY (ARRAY[...])`,
/// for example).
///
/// It drops `::type` casts (the server adds them to literals and to implicitly cast columns),
/// unquotes numeric string literals (`'-1'::integer` is `-1`) and quoted identifiers that need
/// no quotes, lowercases keywords and names outside quotes, writes `!=` as `<>`, ignores
/// whitespace, and removes parentheses that do not change the meaning (see `strip_parens`).
pub fn normalize_expression(text: &str) -> String {
    let toks = tokenize(text);
    let mut out: Vec<Tok> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if toks[i] == Tok::Cast {
            i = skip_type(&toks, i + 1);
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
    use oxbase_dialect::Comparator;
    use oxbase_dialect::compare::{RulesComparator, diff_tables};
    use oxbase_types::{
        CheckDef, DataType, Extras, IndexColumn, IndexDef, ObjectDef, ObjectRef, TableDef,
    };

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
        same("x::character varying(10)[]", "x");
        same("E'a\\b'", "E'a\\b'");
        differ("qty > 0", "qty >= 0");
        differ("(a OR b) AND c", "a OR b AND c");
        differ("'X'", "'x'");
        differ("\"Qty\" > 0", "qty > 0");
        differ("'a  b'", "'a b'");
        differ("f(a) + 1", "f(a + 1)");
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
            .insert(oxbase_types::props::INDEX_METHOD, "btree");
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
