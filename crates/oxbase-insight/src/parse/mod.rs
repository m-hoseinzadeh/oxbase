//! The tolerant parser behind [`crate::outline`] and [`crate::context_at`]
//! (`docs/m6-contracts.md` §2.1, §4.1, §4.2). Private to the crate; owner: the **context**
//! stream.
//!
//! It reads the significant tokens of `oxbase_sql::tokenize` (whitespace and comments dropped,
//! literal prefixes and `::`, `@var`, `:name`, numbers with a fraction or exponent merged) with a
//! recursive-descent parser that never fails: each construct takes what it recognises and
//! leaves the rest to the enclosing loop, which skips what nobody claims. Recursion only happens
//! at `(` and stops at [`MAX_DEPTH`] (deeper groups are skipped flat), so the stack stays small
//! whatever the input; every loop consumes a token per turn or stops, so the parse is linear in
//! the number of tokens.
//!
//! Grammar knowledge (which words start clauses, which are operators) is the same for every
//! dialect and lives in [`words`]. What differs per engine comes from data: the lexer rules of
//! [`SqlDialect`], reserved words from [`SqlLanguage`], identifier case from
//! [`Dialect::case_rules`] and the two flags of [`Grammar`].
//!
//! With a cursor, the parser also records the first [`Slot`] it reaches at the cursor's token:
//! what it was about to read there (a relation, an operand, a type, a new name, one of some
//! keywords). [`crate::context_at`] turns that slot into [`crate::Expected`] items.

use std::ops::Range;

use oxbase_dialect::SqlLanguage;
use oxbase_sql::{SqlDialect, SqlTokenKind, ident_value, tokenize};
use oxbase_types::IdentCase;

use crate::outline::{
    Clause, NameRef, Outline, QueryBlock, RefRole, ScopeRelation, StatementClass,
};
use crate::{Dialect, Ident};

mod dml;
mod expr;
mod query;
mod statement;
pub(crate) mod words;

/// The deepest nesting of parenthesised groups the parser descends into; deeper groups are
/// skipped without looking inside.
const MAX_DEPTH: usize = 48;

/// A significant token's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tk {
    /// A keyword or bare identifier.
    Word,
    /// A quoted identifier.
    Quoted,
    /// A string literal (with its prefix: `N'x'`, `X'ff'`, `_utf8'a'`, `E'x'`, dollar quotes).
    Str,
    /// A number (`1`, `1.5`, `.5`, `1e-3`, `0xff`).
    Num,
    /// A parameter or variable (`$1`, `?`, `:name`, `@var`, `@@var`).
    Param,
    /// One byte of punctuation or an operator character.
    Punct(u8),
    /// PostgreSQL `::`.
    Cast,
    /// `:=`.
    Assign,
}

/// A significant token: kind and byte range in the full text.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tok {
    pub(crate) kind: Tk,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// The significant tokens of `text[range]`, with offsets into `text`. Empty when `range` is not
/// a valid range of `text`.
pub(crate) fn significant_tokens(sql: SqlDialect, text: &str, range: Range<usize>) -> Vec<Tok> {
    let base = range.start;
    let Some(slice) = text.get(range) else {
        return Vec::new();
    };
    let bytes = text.as_bytes();
    let mut out: Vec<Tok> = Vec::new();
    for raw in tokenize(sql, slice) {
        let (start, end) = (base + raw.start, base + raw.end);
        let kind = match raw.kind {
            SqlTokenKind::Space
            | SqlTokenKind::LineComment
            | SqlTokenKind::BlockComment
            | SqlTokenKind::ExecComment => continue,
            SqlTokenKind::String { .. } => Tk::Str,
            SqlTokenKind::QuotedIdent { .. } => Tk::Quoted,
            SqlTokenKind::Word => match bytes[start] {
                b'0'..=b'9' => Tk::Num,
                b'@' | b'$' => Tk::Param,
                _ => Tk::Word,
            },
            SqlTokenKind::Punct if end - start > 1 => Tk::Assign,
            SqlTokenKind::Punct if bytes[start] == b'?' => Tk::Param,
            SqlTokenKind::Punct => Tk::Punct(bytes[start]),
        };
        push_merged(&mut out, Tok { kind, start, end }, bytes);
    }
    out
}

/// Appends `tok`, merging it into the previous token where the two touch and form one
/// (`N` + `'x'`, `:` + `:`, `@` + `var`, `1` + `.` + `5`).
fn push_merged(out: &mut Vec<Tok>, tok: Tok, bytes: &[u8]) {
    let n = out.len();
    if n > 0 && out[n - 1].end == tok.start {
        let prev = out[n - 1];
        let before_touches = n > 1
            && out[n - 2].end == prev.start
            && matches!(out[n - 2].kind, Tk::Word | Tk::Quoted | Tk::Num | Tk::Param);
        if let Some(kind) = merged_kind(prev, tok, before_touches, bytes) {
            out[n - 1] = Tok {
                kind,
                start: prev.start,
                end: tok.end,
            };
            return;
        }
    }
    out.push(tok);
}

/// The kind of `prev` + `next` read as one token, if they form one.
fn merged_kind(prev: Tok, next: Tok, before_touches: bool, bytes: &[u8]) -> Option<Tk> {
    let (a, b) = (prev.kind, next.kind);
    if a == Tk::Word && b == Tk::Str {
        return Some(Tk::Str);
    }
    if a == Tk::Punct(b':') && b == Tk::Punct(b':') {
        return Some(Tk::Cast);
    }
    let param = (a == Tk::Punct(b'@') && matches!(b, Tk::Punct(b'@') | Tk::Word))
        || (a == Tk::Param && b == Tk::Word)
        || (a == Tk::Param && b == Tk::Punct(b'.') && bytes[prev.start] == b'@')
        || (a == Tk::Punct(b':') && b == Tk::Word && !before_touches);
    if param {
        return Some(Tk::Param);
    }
    let last = bytes[prev.end - 1];
    let hex = bytes[prev.start..prev.end].starts_with(b"0x");
    let number = match a {
        Tk::Num => {
            b == Tk::Punct(b'.')
                || (b == Tk::Num && matches!(last, b'.' | b'+' | b'-'))
                || (b == Tk::Word && last == b'.')
                || (matches!(b, Tk::Punct(b'+' | b'-')) && matches!(last, b'e' | b'E') && !hex)
        }
        Tk::Punct(b'.') => b == Tk::Num && !before_touches,
        _ => false,
    };
    number.then_some(Tk::Num)
}

/// Grammar differences between dialects that are not word lists.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Grammar {
    /// A select-list item may be `alias = expr` (T-SQL).
    pub(crate) assign_alias: bool,
    /// Statements of one text need no terminator: a statement keyword ends the previous
    /// statement (T-SQL batches).
    pub(crate) batches: bool,
}

impl Grammar {
    pub(crate) const fn new(sql: SqlDialect) -> Self {
        let tsql = matches!(sql, SqlDialect::TSql);
        Self {
            assign_alias: tsql,
            batches: tsql,
        }
    }
}

/// How a query block sees the relations of the blocks around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockKind {
    /// A statement's block, a scalar / `EXISTS` / `IN` subquery or a `LATERAL` derived table:
    /// the relations of its parent are visible (correlation).
    Nested,
    /// A CTE body, a derived table or the query of `INSERT ... SELECT`: its parent's relations
    /// are not visible, those further out are.
    Isolated,
}

/// Parser facts about a query block that the public [`QueryBlock`] does not carry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BlockInfo {
    pub(crate) kind: BlockKind,
    /// The block whose CTEs are visible next (the parent, or for a later branch of a set
    /// operation the first branch, which owns the `WITH`).
    pub(crate) cte_parent: Option<usize>,
}

/// One statement found in the parsed range (a T-SQL batch or a routine body holds several).
#[derive(Debug, Clone)]
pub(crate) struct StmtInfo {
    pub(crate) range: Range<usize>,
    pub(crate) class: StatementClass,
}

/// What the parser was about to read at the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotKind {
    /// Nothing to complete: a new name, a literal position.
    Nothing,
    /// A statement start.
    Start,
    /// An operand: a column, a function or one of these keywords.
    Expr(&'static [&'static str]),
    /// A table or view (`FROM`, `JOIN`, DML targets).
    Relation,
    /// A column of [`Slot::only`] (or of the scope when that is `None` or a qualifier is typed).
    Columns,
    Type,
    Procedure,
    Function,
    Schema,
    Database,
    Sequence,
    /// One of these keywords (an alias may be typed here too: no names are offered).
    Keywords(&'static [&'static str]),
}

/// The slot at the cursor.
#[derive(Debug, Clone)]
pub(crate) struct Slot {
    pub(crate) kind: SlotKind,
    pub(crate) clause: Clause,
    /// The query block being parsed there.
    pub(crate) query: Option<usize>,
    /// The statement being parsed there (index into [`Parsed::statements`]).
    pub(crate) stmt: Option<usize>,
    /// For [`SlotKind::Columns`]: the one relation the columns come from (`INSERT INTO t (`,
    /// `UPDATE t SET`).
    pub(crate) only: Option<ScopeRelation>,
}

/// The result of [`parse`].
#[derive(Debug, Clone)]
pub(crate) struct Parsed {
    pub(crate) outline: Outline,
    /// Per query block of the outline.
    pub(crate) blocks: Vec<BlockInfo>,
    pub(crate) statements: Vec<StmtInfo>,
    pub(crate) slot: Option<Slot>,
}

/// Parses `text[range]` (one statement as split by `oxbase_sql::split`, or several in a T-SQL
/// batch). With `cursor` (a byte offset into `text`), also records the slot at the cursor.
pub(crate) fn parse(
    dialect: &Dialect,
    text: &str,
    range: Range<usize>,
    cursor: Option<usize>,
) -> Parsed {
    let Range { start, end } = range;
    let start = start.min(text.len());
    let end = end.clamp(start, text.len());
    let toks = significant_tokens(dialect.sql, text, start..end);
    let cursor = cursor.map(|c| cursor_index(&toks, c));
    let mut parser = Parser {
        text,
        lang: &dialect.language,
        case: dialect.case_rules.relations,
        grammar: Grammar::new(dialect.sql),
        toks,
        pos: 0,
        depth: 0,
        queries: Vec::new(),
        blocks: Vec::new(),
        refs: Vec::new(),
        statements: Vec::new(),
        cte_scope: Vec::new(),
        cursor,
        slot: None,
        block: None,
        stmt: None,
    };
    parser.script();
    parser.finish(start..end)
}

/// The token index the cursor is at: the name or number being typed (the cursor inside it or
/// right after it), else the first token at or after the cursor.
fn cursor_index(toks: &[Tok], cursor: usize) -> usize {
    let at = toks.partition_point(|t| t.end < cursor);
    match toks.get(at) {
        Some(t) if t.start < cursor => {
            if matches!(t.kind, Tk::Word | Tk::Quoted | Tk::Num | Tk::Param) {
                at
            } else {
                at + 1
            }
        }
        _ => at,
    }
}

/// A name path as read by [`Parser::path`].
#[derive(Debug, Clone, Default)]
pub(crate) struct Path {
    pub(crate) parts: Vec<Ident>,
    pub(crate) ranges: Vec<Range<usize>>,
    /// Ends with a name (not with `.` or `.*`).
    pub(crate) complete: bool,
    /// Ends with `.*`.
    pub(crate) star: bool,
}

impl Path {
    /// The byte range from the first part to the last.
    pub(crate) fn range(&self) -> Range<usize> {
        match (self.ranges.first(), self.ranges.last()) {
            (Some(a), Some(b)) => a.start..b.end,
            _ => 0..0,
        }
    }
}

/// The parser state. Methods live in the sibling modules by topic.
pub(crate) struct Parser<'a> {
    text: &'a str,
    lang: &'a SqlLanguage,
    /// Case rule for CTE names.
    case: IdentCase,
    grammar: Grammar,
    toks: Vec<Tok>,
    pos: usize,
    depth: usize,
    queries: Vec<QueryBlock>,
    blocks: Vec<BlockInfo>,
    refs: Vec<NameRef>,
    statements: Vec<StmtInfo>,
    /// CTEs in scope while parsing, as (owning block, index), innermost last.
    cte_scope: Vec<(usize, usize)>,
    /// The cursor's token index.
    cursor: Option<usize>,
    slot: Option<Slot>,
    /// The query block being parsed.
    block: Option<usize>,
    /// The statement being parsed.
    stmt: Option<usize>,
}

impl<'a> Parser<'a> {
    fn finish(mut self, range: Range<usize>) -> Parsed {
        self.refs
            .sort_by_key(|r| r.ranges.first().map_or(0, |x| x.start));
        for q in &mut self.queries {
            q.clauses.sort_by_key(|(_, r)| r.start);
        }
        let class = self
            .statements
            .iter()
            .find(|s| s.class != StatementClass::Set)
            .or_else(|| self.statements.first())
            .map_or(StatementClass::Empty, |s| s.class);
        Parsed {
            outline: Outline {
                range,
                class,
                queries: self.queries,
                refs: self.refs,
            },
            blocks: self.blocks,
            statements: self.statements,
            slot: self.slot,
        }
    }

    // --- tokens ------------------------------------------------------------------------------

    fn tok(&self, i: usize) -> Option<Tok> {
        self.toks.get(i).copied()
    }

    fn peek(&self) -> Option<Tok> {
        self.tok(self.pos)
    }

    fn at_end(&self) -> bool {
        self.pos >= self.toks.len()
    }

    fn tok_text(&self, t: Tok) -> &'a str {
        self.text.get(t.start..t.end).unwrap_or("")
    }

    /// The start of the current token (the end of the previous one at the end).
    fn cur_start(&self) -> usize {
        self.peek().map_or_else(|| self.prev_end(), |t| t.start)
    }

    /// The end of the last consumed token (the start of the first one when none is).
    fn prev_end(&self) -> usize {
        match self.pos.min(self.toks.len()).checked_sub(1) {
            Some(i) => self.toks[i].end,
            None => self.toks.first().map_or(0, |t| t.start),
        }
    }

    fn is_word(&self, i: usize, word: &str) -> bool {
        self.tok(i)
            .is_some_and(|t| t.kind == Tk::Word && self.tok_text(t).eq_ignore_ascii_case(word))
    }

    fn at_word(&self, word: &str) -> bool {
        self.is_word(self.pos, word)
    }

    fn eat_word(&mut self, word: &str) -> bool {
        let found = self.at_word(word);
        if found {
            self.pos += 1;
        }
        found
    }

    fn word_in(&self, i: usize, list: &[&str]) -> bool {
        self.tok(i)
            .is_some_and(|t| t.kind == Tk::Word && words::contains(list, self.tok_text(t)))
    }

    fn is_punct(&self, i: usize, c: u8) -> bool {
        self.tok(i).is_some_and(|t| t.kind == Tk::Punct(c))
    }

    fn at_punct(&self, c: u8) -> bool {
        self.is_punct(self.pos, c)
    }

    fn eat_punct(&mut self, c: u8) -> bool {
        let found = self.at_punct(c);
        if found {
            self.pos += 1;
        }
        found
    }

    /// The upper-case text of the word at `i` (empty for anything else).
    fn upper(&self, i: usize) -> String {
        match self.tok(i) {
            Some(t) if t.kind == Tk::Word => self.tok_text(t).to_ascii_uppercase(),
            _ => String::new(),
        }
    }

    /// The identifier at `i`: a word as written, a quoted identifier unquoted.
    fn ident_at(&self, i: usize) -> Option<Ident> {
        let t = self.tok(i)?;
        let text = self.tok_text(t);
        match t.kind {
            Tk::Word => Some(Ident {
                text: text.to_owned(),
                quoted: false,
            }),
            Tk::Quoted => Some(Ident {
                text: ident_value(text),
                quoted: true,
            }),
            _ => None,
        }
    }

    /// A token that can be a bare name here: a quoted identifier, or a word that is not
    /// reserved, starts no clause and is no operator.
    fn is_name(&self, i: usize) -> bool {
        match self.tok(i) {
            Some(t) if t.kind == Tk::Quoted => true,
            Some(t) if t.kind == Tk::Word => {
                let w = self.tok_text(t);
                !self.lang.is_reserved(w)
                    && !words::contains(words::STOP, w)
                    && !words::contains(words::OPERATORS, w)
            }
            _ => false,
        }
    }

    /// A quoted identifier or any word that starts no clause (after `AS`, where reserved words
    /// are names too).
    fn is_any_name(&self, i: usize) -> bool {
        self.tok(i).is_some_and(|t| {
            t.kind == Tk::Quoted
                || (t.kind == Tk::Word && !words::contains(words::STOP, self.tok_text(t)))
        })
    }

    /// At a statement keyword that ends the current statement in a batch without terminators.
    fn strong_stop(&self) -> bool {
        self.grammar.batches
            && self.word_in(self.pos, words::STRONG)
            && !(self.at_word("WITH") && self.is_punct(self.pos + 1, b'('))
    }

    /// The next tokens start a query (`SELECT`, `VALUES`, `WITH x`, also behind `(`).
    fn query_starts(&self, i: usize) -> bool {
        let mut j = i;
        while self.is_punct(j, b'(') {
            j += 1;
        }
        self.is_word(j, "SELECT")
            || self.is_word(j, "VALUES")
            || (self.is_word(j, "WITH") && !self.is_punct(j + 1, b'('))
    }

    // --- cursor ------------------------------------------------------------------------------

    /// Records `kind` as the cursor's slot when token `i` is the cursor's and no slot was
    /// recorded yet.
    fn mark(&mut self, i: usize, kind: SlotKind, clause: Clause) {
        self.mark_only(i, kind, clause, None);
    }

    /// [`Parser::mark`] with the one relation columns come from.
    fn mark_only(
        &mut self,
        i: usize,
        kind: SlotKind,
        clause: Clause,
        only: Option<&ScopeRelation>,
    ) {
        if self.slot.is_none() && self.cursor == Some(i) {
            self.slot = Some(Slot {
                kind,
                clause,
                query: self.block,
                stmt: self.stmt,
                only: only.cloned(),
            });
        }
    }

    // --- groups ------------------------------------------------------------------------------

    /// At `(`: `true` when the parser may descend into the group (call [`Parser::leave`]
    /// after); else skips the whole group and returns `false`.
    fn enter(&mut self) -> bool {
        if self.depth < MAX_DEPTH {
            self.depth += 1;
            true
        } else {
            self.skip_group();
            false
        }
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// At `(`: skips to just after its matching `)` (or to the end).
    fn skip_group(&mut self) {
        let mut depth = 0_usize;
        while let Some(t) = self.peek() {
            self.pos += 1;
            match t.kind {
                Tk::Punct(b'(') => depth += 1,
                Tk::Punct(b')') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
    }

    /// Inside a group: skips to just after the `)` that closes it (nested groups skipped);
    /// stops before a `;`.
    fn close_paren(&mut self) {
        let mut depth = 0_usize;
        while let Some(t) = self.peek() {
            match t.kind {
                Tk::Punct(b')') if depth == 0 => {
                    self.pos += 1;
                    return;
                }
                Tk::Punct(b')') => depth -= 1,
                Tk::Punct(b'(') => depth += 1,
                Tk::Punct(b';') if depth == 0 => return,
                _ => {}
            }
            self.pos += 1;
        }
    }

    // --- output ------------------------------------------------------------------------------

    fn push_ref(
        &mut self,
        parts: Vec<Ident>,
        ranges: Vec<Range<usize>>,
        role: RefRole,
        clause: Clause,
    ) {
        if parts.is_empty() || parts.len() != ranges.len() {
            return;
        }
        self.refs.push(NameRef {
            parts,
            ranges,
            role,
            query: self.block,
            clause,
        });
    }

    /// Records a complete `path` as a reference.
    fn push_path(&mut self, path: &Path, role: RefRole, clause: Clause) {
        if path.complete {
            self.push_ref(path.parts.clone(), path.ranges.clone(), role, clause);
        }
    }

    /// A new query block starting at the current token.
    fn new_block(&mut self, parent: Option<usize>, kind: BlockKind) -> usize {
        self.new_block_with(parent, kind, parent)
    }

    fn new_block_with(
        &mut self,
        parent: Option<usize>,
        kind: BlockKind,
        cte_parent: Option<usize>,
    ) -> usize {
        let start = self.cur_start();
        self.queries.push(QueryBlock {
            range: start..start,
            parent,
            ctes: Vec::new(),
            relations: Vec::new(),
            output: Vec::new(),
            star: false,
            clauses: Vec::new(),
        });
        self.blocks.push(BlockInfo { kind, cte_parent });
        self.queries.len() - 1
    }

    /// Sets the end of block `b` to the last consumed token.
    fn end_block(&mut self, b: usize) {
        let end = self.prev_end();
        if let Some(q) = self.queries.get_mut(b) {
            q.range.end = end.max(q.range.start);
        }
    }

    /// Runs `f` and records the tokens it consumed as `clause` of the current block.
    fn in_clause<R>(&mut self, clause: Clause, f: impl FnOnce(&mut Self) -> R) -> R {
        let start = self.cur_start();
        let result = f(self);
        let end = self.prev_end().max(start);
        if let Some(b) = self.block
            && let Some(q) = self.queries.get_mut(b)
        {
            q.clauses.push((clause, start..end));
        }
        result
    }

    // --- names -------------------------------------------------------------------------------

    /// Reads a name path `a.b.c` (also `a.*`, T-SQL `db..t`) from the current token, which the
    /// caller checked is a word or a quoted identifier. Marks `slot` after each `.`.
    fn path(&mut self, slot: SlotKind, clause: Clause) -> Path {
        let mut path = Path::default();
        while let Some(t) = self.peek() {
            let Some(part) = self.ident_at(self.pos) else {
                break;
            };
            path.parts.push(part);
            path.ranges.push(t.start..t.end);
            self.pos += 1;
            let Some(dot) = self.peek().filter(|d| d.kind == Tk::Punct(b'.')) else {
                path.complete = true;
                break;
            };
            self.pos += 1;
            let mut gap_end = dot.end;
            while let Some(d) = self.peek().filter(|d| d.kind == Tk::Punct(b'.')) {
                path.parts.push(Ident {
                    text: String::new(),
                    quoted: false,
                });
                path.ranges.push(gap_end..gap_end);
                gap_end = d.end;
                self.pos += 1;
            }
            self.mark(self.pos, slot, clause);
            let Some(next) = self.peek() else {
                break;
            };
            if next.kind == Tk::Punct(b'*') {
                self.pos += 1;
                path.star = true;
                break;
            }
            let name_follows = next.kind == Tk::Quoted
                || (next.kind == Tk::Word && (next.start == gap_end || self.is_name(self.pos)));
            if !name_follows {
                break;
            }
        }
        path
    }

    /// A one-part path for the name at the current token (consumed).
    fn single(&mut self) -> Option<Path> {
        let t = self.peek()?;
        let part = self.ident_at(self.pos)?;
        self.pos += 1;
        Some(Path {
            parts: vec![part],
            ranges: vec![t.start..t.end],
            complete: true,
            star: false,
        })
    }

    /// The CTE in scope named `name`, as (owning block, index).
    fn find_cte(&self, name: &Ident) -> Option<(usize, usize)> {
        self.cte_scope.iter().rev().copied().find(|&(q, i)| {
            self.queries
                .get(q)
                .and_then(|block| block.ctes.get(i))
                .is_some_and(|cte| same_name(self.case, name, &cte.name))
        })
    }
}

/// `true` when `written` names what `defined` (another identifier as written) names, under
/// `case`.
pub(crate) fn same_name(case: IdentCase, written: &Ident, defined: &Ident) -> bool {
    let catalog = if defined.quoted {
        defined.text.clone()
    } else {
        case.fold_unquoted(&defined.text)
    };
    case.matches(&written.text, written.quoted, &catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(sql: SqlDialect, text: &str) -> Vec<(Tk, String)> {
        significant_tokens(sql, text, 0..text.len())
            .into_iter()
            .map(|t| (t.kind, text[t.start..t.end].to_owned()))
            .collect()
    }

    #[test]
    fn tokens_merge_literals_and_operators() {
        let toks = kinds(
            SqlDialect::Postgres,
            "x::int + 1.5 - .5 * 1e-3, $1, :name, a:b /* c */ E'x' -- d",
        );
        let expect = [
            (Tk::Word, "x"),
            (Tk::Cast, "::"),
            (Tk::Word, "int"),
            (Tk::Punct(b'+'), "+"),
            (Tk::Num, "1.5"),
            (Tk::Punct(b'-'), "-"),
            (Tk::Num, ".5"),
            (Tk::Punct(b'*'), "*"),
            (Tk::Num, "1e-3"),
            (Tk::Punct(b','), ","),
            (Tk::Param, "$1"),
            (Tk::Punct(b','), ","),
            (Tk::Param, ":name"),
            (Tk::Punct(b','), ","),
            (Tk::Word, "a"),
            (Tk::Punct(b':'), ":"),
            (Tk::Word, "b"),
            (Tk::Str, "E'x'"),
        ];
        let got: Vec<(Tk, &str)> = toks.iter().map(|(k, s)| (*k, s.as_str())).collect();
        assert_eq!(got, expect);
        let my = kinds(
            SqlDialect::MySql { mariadb: false },
            "@a, @@b, ?, _utf8'x', a.b",
        );
        let got: Vec<Tk> = my.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            got,
            [
                Tk::Param,
                Tk::Punct(b','),
                Tk::Param,
                Tk::Punct(b','),
                Tk::Param,
                Tk::Punct(b','),
                Tk::Str,
                Tk::Punct(b','),
                Tk::Word,
                Tk::Punct(b'.'),
                Tk::Word,
            ]
        );
        let ms = kinds(SqlDialect::TSql, "N'x' @v #t [a b]");
        let got: Vec<Tk> = ms.iter().map(|(k, _)| *k).collect();
        assert_eq!(got, [Tk::Str, Tk::Param, Tk::Word, Tk::Quoted]);
        assert!(
            significant_tokens(SqlDialect::Sqlite, "abc", 5..9).is_empty(),
            "an invalid range gives no tokens"
        );
    }

    #[test]
    fn cursor_index_prefers_the_word_being_typed() {
        let text = "SELECT ab FROM t.";
        let toks = significant_tokens(SqlDialect::Postgres, text, 0..text.len());
        assert_eq!(cursor_index(&toks, 0), 0);
        assert_eq!(cursor_index(&toks, 9), 1, "end of `ab`");
        assert_eq!(cursor_index(&toks, 8), 1, "inside `ab`");
        assert_eq!(cursor_index(&toks, 10), 2, "before FROM");
        assert_eq!(cursor_index(&toks, text.len()), toks.len(), "after the dot");
    }

    #[test]
    fn names_compare_under_case_rules() {
        let id = |text: &str, quoted| Ident {
            text: text.to_owned(),
            quoted,
        };
        let lower = IdentCase::FoldLower;
        assert!(same_name(lower, &id("CTE", false), &id("cte", false)));
        assert!(same_name(lower, &id("cte", false), &id("Cte", false)));
        assert!(!same_name(lower, &id("Cte", true), &id("Cte", false)));
        assert!(same_name(lower, &id("Cte", true), &id("Cte", true)));
        assert!(same_name(
            IdentCase::Insensitive,
            &id("A", true),
            &id("a", false)
        ));
        assert!(!same_name(
            IdentCase::Sensitive,
            &id("A", false),
            &id("a", false)
        ));
    }
}
