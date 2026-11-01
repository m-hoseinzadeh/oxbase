//! What may be typed at the cursor (`docs/m6-contracts.md` §4.2). Owner of [`context_at`]: the
//! **context** stream.

use std::ops::Range;

use oxbase_sql::{SqlDialect, SqlToken, SqlTokenKind, ident_value, split, tokenize};
use oxbase_types::IdentCase;

use crate::outline::parse::{self, BlockKind, Parsed, Slot, SlotKind, same_name, words};
use crate::outline::{Clause, Cte, Outline, RelationSource, ScopeRelation, StatementClass};
use crate::{Dialect, Ident};

/// An item kind the cursor position accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Expected {
    /// A keyword ([`CursorContext::keywords`] lists the likely ones; empty = any).
    Keyword,
    Database,
    Schema,
    /// A table or view.
    Relation,
    Column,
    /// A scalar, aggregate or window function (built-in or user-defined).
    Function,
    /// A routine for `CALL` / `EXEC`.
    Procedure,
    Type,
    Sequence,
    /// Snippets (statement starts).
    Snippet,
}

/// The cursor's context in one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorContext {
    /// The cursor (byte offset in the full text).
    pub offset: usize,
    /// The statement around the cursor ([`oxbase_sql::pick_statement`] rules; an empty range
    /// at the cursor when the text has no statement).
    pub statement: Range<usize>,
    pub class: StatementClass,
    pub clause: Clause,
    /// The range a completion replaces: the identifier or keyword being typed, including an
    /// opening quote, up to the end of the word (it may extend past the cursor). Empty at the
    /// cursor when nothing is being typed.
    pub word: Range<usize>,
    /// The typed part of the word before the cursor, without its opening quote.
    pub prefix: String,
    /// The opening quote byte when the cursor is inside a quoted identifier (`"`, `` ` ``,
    /// `[`).
    pub quote: Option<u8>,
    /// The parts before the last `.` (`s.t.c|` → `[s, t]`); empty when unqualified.
    pub qualifier: Vec<Ident>,
    /// What may be typed here, most likely first. Empty inside a string literal or a comment,
    /// and where a new name is being defined (no completion).
    pub expected: Vec<Expected>,
    /// Keywords that fit here (upper case, most likely first), when [`Expected::Keyword`] is
    /// expected; empty = any keyword of the dialect.
    pub keywords: Vec<&'static str>,
    /// Relations visible at the cursor: the innermost block's first, then enclosing blocks'
    /// (for correlated subqueries). For a DML statement, its target first.
    pub scope: Vec<ScopeRelation>,
    /// CTEs visible at the cursor, innermost first.
    pub ctes: Vec<Cte>,
    /// The statement's outline.
    pub outline: Outline,
}

impl CursorContext {
    /// `true` when `kind` is expected.
    pub fn expects(&self, kind: Expected) -> bool {
        self.expected.contains(&kind)
    }

    /// The scope relations a column completion draws from: with a qualifier (`x.|`), those
    /// whose alias, or whose own name (a table's or function's last part, a CTE's name) when
    /// it has no alias, names what the qualifier's last part names under `case`; else every
    /// relation in scope.
    pub fn column_sources(&self, case: IdentCase) -> Vec<&ScopeRelation> {
        let Some(q) = self.qualifier.last() else {
            return self.scope.iter().collect();
        };
        self.scope
            .iter()
            .filter(|r| answers_to(r, &self.outline, q, case))
            .collect()
    }
}

/// `true` when `qualifier` (as written) refers to `relation`: its alias, else its own name.
fn answers_to(
    relation: &ScopeRelation,
    outline: &Outline,
    qualifier: &Ident,
    case: IdentCase,
) -> bool {
    match (&relation.alias, &relation.source) {
        (Some(alias), _) => same_name(case, qualifier, alias),
        (None, RelationSource::Named(parts) | RelationSource::Function(parts)) => parts
            .last()
            .is_some_and(|last| same_name(case, qualifier, last)),
        (None, RelationSource::Cte { query, index }) => outline
            .queries
            .get(*query)
            .and_then(|block| block.ctes.get(*index))
            .is_some_and(|cte| same_name(case, qualifier, &cte.name)),
        (None, _) => false,
    }
}

/// The context of the cursor at byte `offset` of `text` (clamped to the text and to a char
/// boundary). Never fails and never panics; pure function of its inputs (no cache: names are
/// resolved by [`crate::complete`]). Must take well under 5 ms for a statement of 10 000
/// tokens in a release build (`docs/m6-contracts.md` §5).
pub fn context_at(dialect: &Dialect, text: &str, offset: usize) -> CursorContext {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    context_in(dialect, text, 0..text.len(), offset, 0)
}

/// How deep SQL-language routine bodies (dollar-quoted) are followed.
const MAX_BODY_DEPTH: usize = 4;

/// The context of `offset` in the script `text[region]` (the whole text, or the body of a
/// SQL-language routine).
fn context_in(
    dialect: &Dialect,
    text: &str,
    region: Range<usize>,
    offset: usize,
    depth: usize,
) -> CursorContext {
    let base = region.start;
    let script = text.get(region).unwrap_or("");
    let statements: Vec<(Range<usize>, usize)> = split(dialect.sql, script)
        .into_iter()
        .map(|s| {
            (
                s.range.start + base..s.range.end + base,
                s.segment_end + base,
            )
        })
        .collect();
    let (statement, lex_from, fresh) = pick(&statements, base, offset);
    let lex = lexical(dialect.sql, text, lex_from, offset);
    if let Some(body) = &lex.body
        && depth < MAX_BODY_DEPTH
        && is_sql_body(dialect.sql, text, &statement)
    {
        return context_in(dialect, text, body.clone(), offset, depth + 1);
    }
    let mut ctx = if fresh {
        CursorContext {
            offset,
            statement: statement.clone(),
            class: StatementClass::Empty,
            clause: Clause::Start,
            word: offset..offset,
            prefix: String::new(),
            quote: None,
            qualifier: Vec::new(),
            expected: vec![Expected::Keyword, Expected::Snippet],
            keywords: keywords_for(dialect, words::STATEMENT_STARTERS),
            scope: Vec::new(),
            ctes: Vec::new(),
            outline: Outline::empty(statement),
        }
    } else {
        let parsed = parse::parse(dialect, text, statement.clone(), Some(offset));
        from_parse(dialect, parsed, statement, offset, &lex)
    };
    if let Some(word) = &lex.word {
        ctx.word = word.clone();
        ctx.prefix.clone_from(&lex.prefix);
        ctx.quote = lex.quote;
    }
    if lex.opaque || lex.literal {
        ctx.expected.clear();
    }
    if ctx.quote.is_some() {
        ctx.expected
            .retain(|e| !matches!(e, Expected::Keyword | Expected::Snippet));
    }
    if !ctx.expects(Expected::Keyword) {
        ctx.keywords.clear();
    }
    ctx
}

/// The statement for `offset` among `statements` (ranges and segment ends), the offset the
/// lexical scan starts at, and whether the cursor is in no statement (a new one starts there:
/// before the first token of the picked statement, or after its terminator).
fn pick(
    statements: &[(Range<usize>, usize)],
    base: usize,
    offset: usize,
) -> (Range<usize>, usize, bool) {
    let index = statements
        .iter()
        .position(|(_, segment_end)| offset <= *segment_end)
        .or_else(|| statements.len().checked_sub(1));
    let Some(i) = index else {
        return (offset..offset, base, true);
    };
    let (range, segment_end) = statements[i].clone();
    let previous_end = if i > 0 { statements[i - 1].1 } else { base };
    if offset < range.start {
        (offset..offset, previous_end, true)
    } else if offset >= segment_end && segment_end > range.end {
        (offset..offset, segment_end, true)
    } else {
        (range, previous_end, false)
    }
}

/// What the text right at the cursor says, before any parsing.
#[derive(Debug, Default)]
struct Lexical {
    /// Inside a string literal or a comment.
    opaque: bool,
    /// Inside a dollar-quoted string: its content.
    body: Option<Range<usize>>,
    /// The word being typed (a quoted identifier including its quotes).
    word: Option<Range<usize>>,
    prefix: String,
    quote: Option<u8>,
    qualifier: Vec<Ident>,
    /// Typing a number, a parameter or a variable.
    literal: bool,
}

/// Lexes `text` from `from` (a statement boundary) to the cursor.
fn lexical(sql: SqlDialect, text: &str, from: usize, offset: usize) -> Lexical {
    let mut lex = Lexical::default();
    let Some(slice) = text.get(from..) else {
        return lex;
    };
    let rel = offset.saturating_sub(from);
    let toks: Vec<SqlToken> = tokenize(sql, slice).take_while(|t| t.start < rel).collect();
    let Some(last) = toks.last().copied() else {
        return lex;
    };
    let unterminated = rel < last.end || !last.is_terminated(slice);
    let mut word_index = toks.len();
    match last.kind {
        SqlTokenKind::String {
            inner_start,
            inner_end,
        } if unterminated => {
            lex.opaque = true;
            if slice[last.start..].starts_with('$') && rel >= inner_start {
                lex.body = Some(from + inner_start..from + inner_end);
            }
            return lex;
        }
        SqlTokenKind::LineComment => {
            lex.opaque = true;
            return lex;
        }
        // Spaces after a line comment's text, on its line.
        SqlTokenKind::Space
            if toks.len() >= 2
                && toks[toks.len() - 2].kind == SqlTokenKind::LineComment
                && !slice
                    .get(last.start..rel)
                    .unwrap_or("\n")
                    .contains(['\n', '\r']) =>
        {
            lex.opaque = true;
            return lex;
        }
        SqlTokenKind::BlockComment | SqlTokenKind::ExecComment if unterminated => {
            lex.opaque = true;
            return lex;
        }
        SqlTokenKind::QuotedIdent { quote } => {
            word_index -= 1;
            lex.quote = Some(quote);
            lex.prefix = ident_value(slice.get(last.start..rel).unwrap_or(""));
        }
        SqlTokenKind::Word => {
            word_index -= 1;
            lex.prefix = slice.get(last.start..rel).unwrap_or("").to_owned();
            lex.literal = last
                .text(slice)
                .starts_with(|c: char| c.is_ascii_digit() || c == '@' || c == '$')
                || is_parameter_sigil(&toks, word_index, slice);
        }
        _ => {}
    }
    let mut start = rel;
    if word_index < toks.len() {
        lex.word = Some(from + last.start..from + last.end);
        start = last.start;
    }
    // The qualifier: `part.` pairs that touch, right before the word (or the cursor).
    let mut at = word_index;
    while at >= 2 {
        let (dot, part) = (toks[at - 1], toks[at - 2]);
        if dot.kind != SqlTokenKind::Punct
            || dot.text(slice) != "."
            || dot.end != start
            || part.end != dot.start
        {
            break;
        }
        let ident = match part.kind {
            SqlTokenKind::Word => {
                if part.text(slice).starts_with(|c: char| c.is_ascii_digit()) {
                    lex.literal = true;
                }
                Ident {
                    text: part.text(slice).to_owned(),
                    quoted: false,
                }
            }
            SqlTokenKind::QuotedIdent { .. } => Ident {
                text: ident_value(part.text(slice)),
                quoted: true,
            },
            _ => break,
        };
        lex.qualifier.push(ident);
        start = part.start;
        at -= 2;
    }
    lex.qualifier.reverse();
    lex
}

/// The word at `index` follows a `@` or a single `:` it touches (a MySQL variable, a named
/// parameter).
fn is_parameter_sigil(toks: &[SqlToken], index: usize, slice: &str) -> bool {
    let Some(sigil) = index.checked_sub(1).and_then(|i| toks.get(i)) else {
        return false;
    };
    let Some(word) = toks.get(index) else {
        return false;
    };
    if sigil.kind != SqlTokenKind::Punct || sigil.end != word.start {
        return false;
    }
    match sigil.text(slice) {
        "@" => true,
        ":" => !index
            .checked_sub(2)
            .and_then(|i| toks.get(i))
            .is_some_and(|before| before.end == sigil.start && before.text(slice) == ":"),
        _ => false,
    }
}

/// `true` when the statement is a `FUNCTION` / `PROCEDURE` with `LANGUAGE sql`, whose
/// dollar-quoted body is SQL (other languages' bodies stay opaque).
fn is_sql_body(sql: SqlDialect, text: &str, statement: &Range<usize>) -> bool {
    let Some(stmt) = text.get(statement.clone()) else {
        return false;
    };
    let words: Vec<&str> = tokenize(sql, stmt)
        .filter(|t| !t.is_trivia())
        .map(|t| match t.kind {
            SqlTokenKind::String {
                inner_start,
                inner_end,
            } => stmt.get(inner_start..inner_end).unwrap_or(""),
            _ => t.text(stmt),
        })
        .collect();
    let routine = words
        .iter()
        .any(|w| w.eq_ignore_ascii_case("FUNCTION") || w.eq_ignore_ascii_case("PROCEDURE"));
    routine
        && words.windows(2).any(|pair| {
            matches!(pair, [language, name]
                if language.eq_ignore_ascii_case("LANGUAGE") && name.eq_ignore_ascii_case("sql"))
        })
}

/// The context from the parse of the statement at `statement`.
fn from_parse(
    dialect: &Dialect,
    parsed: Parsed,
    statement: Range<usize>,
    offset: usize,
    lex: &Lexical,
) -> CursorContext {
    let slot = parsed.slot.clone().unwrap_or_else(|| Slot {
        kind: SlotKind::Keywords(&[]),
        clause: Clause::Other,
        query: parsed.outline.query_at(offset),
        stmt: parsed
            .statements
            .iter()
            .rposition(|s| s.range.start <= offset),
        only: None,
    });
    let case = dialect.case_rules.relations;
    let class = match slot.stmt.and_then(|s| parsed.statements.get(s)) {
        Some(s) => s.class,
        None if slot.kind == SlotKind::Start => StatementClass::Empty,
        None => parsed.outline.class,
    };
    let full_scope = scope_of(&parsed, slot.query);
    let ctes = ctes_of(&parsed, slot.query);
    let qualified = lex.qualifier.len();
    let scope = match &slot.only {
        Some(only) if slot.kind == SlotKind::Columns && qualified == 0 => {
            vec![resolve_target(only, &full_scope, case)]
        }
        _ => full_scope,
    };
    let matched = lex.qualifier.last().is_some_and(|q| {
        scope
            .iter()
            .any(|r| answers_to(r, &parsed.outline, q, case))
    });
    let (expected, keywords) = expected_at(slot.kind, qualified, matched, scope.is_empty());
    CursorContext {
        offset,
        statement,
        class,
        clause: slot.clause,
        word: offset..offset,
        prefix: String::new(),
        quote: None,
        qualifier: lex.qualifier.clone(),
        expected,
        keywords: keywords_for(dialect, keywords),
        scope,
        ctes,
        outline: parsed.outline,
    }
}

/// The expected kinds and keywords of a slot, given how many qualifier parts were typed,
/// whether the last one names a relation in scope, and whether the scope is empty.
fn expected_at(
    kind: SlotKind,
    qualified: usize,
    matched: bool,
    no_scope: bool,
) -> (Vec<Expected>, &'static [&'static str]) {
    use Expected::{
        Column, Database, Function, Keyword, Procedure, Relation, Schema, Sequence, Snippet, Type,
    };
    let none: &'static [&'static str] = &[];
    match kind {
        SlotKind::Start if qualified == 0 => (vec![Keyword, Snippet], words::STATEMENT_STARTERS),
        SlotKind::Expr(keywords) if qualified == 0 => (vec![Column, Function, Keyword], keywords),
        SlotKind::Expr(_) if matched || qualified > 1 => (vec![Column], none),
        SlotKind::Expr(_) if no_scope => (vec![Relation, Column], none),
        SlotKind::Expr(_) => (vec![Column, Function], none),
        SlotKind::Relation if qualified == 0 => (vec![Relation, Schema], none),
        SlotKind::Relation => (vec![Relation], none),
        SlotKind::Columns => (vec![Column], none),
        SlotKind::Type => (vec![Type], none),
        SlotKind::Procedure => (vec![Procedure], none),
        SlotKind::Function => (vec![Function], none),
        SlotKind::Schema => (vec![Schema], none),
        SlotKind::Database => (vec![Database, Schema], none),
        SlotKind::Sequence => (vec![Sequence], none),
        SlotKind::Keywords(keywords) if qualified == 0 => (vec![Keyword], keywords),
        SlotKind::Nothing | SlotKind::Start | SlotKind::Keywords(_) => (Vec::new(), none),
    }
}

/// `list` without the words the dialect's language data does not know (engine data only;
/// the standard core keeps them all).
fn keywords_for(dialect: &Dialect, list: &'static [&'static str]) -> Vec<&'static str> {
    list.iter()
        .copied()
        .filter(|k| dialect.language.engine.is_none() || dialect.language.is_keyword(k))
        .collect()
}

/// The relations visible in block `query`: its own, then its ancestors' (a CTE body, a derived
/// table or an `INSERT ... SELECT` does not see its parent's own relations).
fn scope_of(parsed: &Parsed, query: Option<usize>) -> Vec<ScopeRelation> {
    let mut out = Vec::new();
    let mut at = query;
    let mut hidden = false;
    while let Some(i) = at {
        let Some(block) = parsed.outline.queries.get(i) else {
            break;
        };
        if !hidden {
            out.extend(block.relations.iter().cloned());
        }
        hidden = parsed
            .blocks
            .get(i)
            .is_some_and(|b| b.kind == BlockKind::Isolated);
        at = block.parent.filter(|&p| p < i);
    }
    out
}

/// The CTEs visible in block `query`, innermost first.
fn ctes_of(parsed: &Parsed, query: Option<usize>) -> Vec<Cte> {
    let mut out = Vec::new();
    let mut at = query;
    while let Some(i) = at {
        let Some(block) = parsed.outline.queries.get(i) else {
            break;
        };
        out.extend(block.ctes.iter().cloned());
        at = parsed
            .blocks
            .get(i)
            .and_then(|b| b.cte_parent)
            .filter(|&p| p < i);
    }
    out
}

/// The DML target a column slot refers to: when it is a bare name that another relation in
/// scope declares as its alias (`UPDATE o SET ‸ FROM orders o`), that relation.
fn resolve_target(only: &ScopeRelation, scope: &[ScopeRelation], case: IdentCase) -> ScopeRelation {
    if let (RelationSource::Named(parts), None) = (&only.source, &only.alias)
        && let [name] = parts.as_slice()
        && let Some(found) = scope.iter().find(|r| {
            r.range != only.range && r.alias.as_ref().is_some_and(|a| same_name(case, name, a))
        })
    {
        return found.clone();
    }
    only.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(text: &str) -> Ident {
        Ident {
            text: text.to_owned(),
            quoted: false,
        }
    }

    fn rel(name: &str, alias: Option<&str>) -> ScopeRelation {
        ScopeRelation {
            source: RelationSource::Named(vec![ident("public"), ident(name)]),
            alias: alias.map(ident),
            column_aliases: vec![],
            range: 0..0,
        }
    }

    fn ctx(qualifier: &[&str]) -> CursorContext {
        CursorContext {
            offset: 0,
            statement: 0..0,
            class: StatementClass::Select,
            clause: Clause::SelectList,
            word: 0..0,
            prefix: String::new(),
            quote: None,
            qualifier: qualifier.iter().copied().map(ident).collect(),
            expected: vec![Expected::Column],
            keywords: vec![],
            scope: vec![rel("orders", Some("o")), rel("customers", None)],
            ctes: vec![],
            outline: Outline::empty(0..0),
        }
    }

    #[test]
    fn column_sources_follow_the_qualifier() {
        let names = |c: &CursorContext, case| -> Vec<String> {
            c.column_sources(case)
                .iter()
                .map(|r| match &r.source {
                    RelationSource::Named(p) => p[1].text.clone(),
                    _ => String::new(),
                })
                .collect()
        };
        let lower = IdentCase::FoldLower;
        assert_eq!(names(&ctx(&[]), lower), ["orders", "customers"]);
        assert_eq!(names(&ctx(&["O"]), lower), ["orders"]);
        assert_eq!(names(&ctx(&["customers"]), lower), ["customers"]);
        // An aliased relation is reached through its alias only.
        assert_eq!(names(&ctx(&["orders"]), lower), Vec::<String>::new());
        assert_eq!(
            names(&ctx(&["CUSTOMERS"]), IdentCase::Sensitive),
            Vec::<String>::new()
        );
        assert!(ctx(&[]).expects(Expected::Column));
        assert!(!ctx(&[]).expects(Expected::Keyword));
    }
}
