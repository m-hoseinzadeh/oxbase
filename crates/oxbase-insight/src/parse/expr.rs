//! Expressions: operands (column paths, function calls, literals, subqueries, `CASE`, casts)
//! and the operators between them. No precedence: insight needs the names in an expression
//! and where it ends, not its tree.

use super::words;
use super::{BlockKind, Parser, SlotKind, Tk};
use crate::Ident;
use crate::outline::{Clause, RefRole};

/// Where an expression is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExprMode {
    /// Anywhere but a select list.
    Plain,
    /// A select-list item: an alias (`AS x`, a bare `x`, a string) may follow.
    Select,
}

/// What [`Parser::expr`] found.
#[derive(Debug, Clone, Default)]
pub(crate) struct ExprResult {
    /// The item's alias (select lists only).
    pub(crate) alias: Option<Ident>,
    /// The column name when the expression is one plain column path.
    pub(crate) name: Option<Ident>,
    /// The expression is `t.*`.
    pub(crate) star: bool,
}

/// What an operand that starts with a name turned out to be.
enum NameOperand {
    Column(Ident),
    Star,
    Other,
}

impl Parser<'_> {
    /// One expression from the current token. Stops before `,`, `)`, `;`, a clause keyword or
    /// anything that cannot continue it; records the names in it as references of `clause`.
    pub(crate) fn expr(&mut self, clause: Clause, mode: ExprMode) -> ExprResult {
        let mut out = ExprResult::default();
        let select = mode == ExprMode::Select;
        let after = words::after_operand(clause, select);
        // Expecting an operand (else an operator, an alias or the end).
        let mut operand = true;
        let mut case_depth = 0_usize;
        let mut operands = 0_usize;
        let mut simple: Option<Ident> = None;
        let mut interval = false;
        loop {
            let i = self.pos;
            if operand {
                self.mark(i, SlotKind::Expr(words::EXPR_START), clause);
            } else {
                self.mark(i, SlotKind::Keywords(after), clause);
            }
            let Some(t) = self.peek() else {
                break;
            };
            match t.kind {
                Tk::Punct(b',' | b')' | b';') => break,
                Tk::Punct(b'(') => {
                    self.paren_operand(clause);
                    operand = false;
                    operands += 1;
                    simple = None;
                }
                Tk::Punct(b'*') if operand => {
                    self.pos += 1;
                    operand = false;
                    operands += 1;
                    simple = None;
                }
                Tk::Punct(b']') => {
                    self.pos += 1;
                    operand = false;
                }
                Tk::Punct(_) | Tk::Assign => {
                    self.pos += 1;
                    operand = true;
                    simple = None;
                }
                Tk::Cast => {
                    self.pos += 1;
                    self.type_name(clause);
                    operand = false;
                    simple = None;
                }
                Tk::Str if !operand && select => {
                    // `SELECT a 'label'`.
                    self.pos += 1;
                    out.alias = Some(Ident {
                        text: string_value(self.tok_text(t)),
                        quoted: true,
                    });
                    break;
                }
                Tk::Str | Tk::Num | Tk::Param => {
                    self.pos += 1;
                    operand = false;
                    operands += 1;
                    simple = None;
                }
                Tk::Quoted if operand => {
                    match self.name_operand(clause) {
                        NameOperand::Column(name) if operands == 0 => simple = Some(name),
                        NameOperand::Star => out.star = true,
                        NameOperand::Column(_) | NameOperand::Other => simple = None,
                    }
                    operand = false;
                    operands += 1;
                }
                Tk::Quoted => {
                    if select && let Some(path) = self.single() {
                        self.push_path(&path, RefRole::Definition, clause);
                        out.alias = path.parts.into_iter().next();
                    }
                    break;
                }
                Tk::Word if operand => {
                    let w = self.tok_text(t);
                    let call = self.is_punct(self.pos + 1, b'(');
                    if words::contains(words::STOP, w)
                        && !(call && words::contains(words::STOP_FUNCTIONS, w))
                        && !(case_depth > 0 && is_case_word(w))
                    {
                        break;
                    }
                    if w.eq_ignore_ascii_case("CASE") {
                        self.pos += 1;
                        case_depth += 1;
                        simple = None;
                    } else if case_depth > 0 && is_case_word(w) {
                        self.pos += 1;
                        if w.eq_ignore_ascii_case("END") {
                            case_depth -= 1;
                            operand = false;
                            operands += 1;
                        }
                    } else if words::contains(words::PREFIX, w) {
                        self.pos += 1;
                        simple = None;
                    } else if w.eq_ignore_ascii_case("INTERVAL") {
                        self.pos += 1;
                        interval = true;
                        simple = None;
                    } else if words::contains(words::VALUE_WORDS, w) && !call {
                        self.pos += 1;
                        operand = false;
                        operands += 1;
                        simple = None;
                    } else if self.is_typed_literal(w) {
                        self.pos += 2;
                        operand = false;
                        operands += 1;
                        simple = None;
                    } else if self.lang.is_reserved(w) && !call {
                        break;
                    } else {
                        match self.name_operand(clause) {
                            NameOperand::Column(name) if operands == 0 => simple = Some(name),
                            NameOperand::Star => out.star = true,
                            NameOperand::Column(_) | NameOperand::Other => simple = None,
                        }
                        operand = false;
                        operands += 1;
                    }
                }
                Tk::Word => {
                    let w = self.tok_text(t);
                    if case_depth > 0 && is_case_word(w) {
                        self.pos += 1;
                        if w.eq_ignore_ascii_case("END") {
                            case_depth -= 1;
                        } else {
                            operand = true;
                        }
                    } else if interval && words::contains(words::INTERVAL_UNITS, w) {
                        self.pos += 1;
                    } else if w.eq_ignore_ascii_case("ISNULL") || w.eq_ignore_ascii_case("NOTNULL")
                    {
                        self.pos += 1;
                        simple = None;
                    } else if w.eq_ignore_ascii_case("AT")
                        && (self.is_word(self.pos + 1, "TIME")
                            || self.is_word(self.pos + 1, "LOCAL"))
                    {
                        self.pos += 2;
                        self.eat_word("ZONE");
                        operand = true;
                        simple = None;
                    } else if words::contains(words::OPERATORS, w) {
                        self.pos += 1;
                        self.operator_continuation();
                        operand = true;
                        interval = false;
                        simple = None;
                    } else if w.eq_ignore_ascii_case("COLLATE") {
                        self.pos += 1;
                        if self
                            .peek()
                            .is_some_and(|n| matches!(n.kind, Tk::Word | Tk::Quoted))
                        {
                            self.path(SlotKind::Nothing, clause);
                        }
                    } else if w.eq_ignore_ascii_case("OVER") {
                        self.pos += 1;
                        if self.at_punct(b'(') {
                            self.window_spec();
                        } else if self.is_name(self.pos) {
                            self.pos += 1;
                        }
                        simple = None;
                    } else if w.eq_ignore_ascii_case("FILTER") && self.is_punct(self.pos + 1, b'(')
                    {
                        self.pos += 1;
                        self.filter_clause(clause);
                        simple = None;
                    } else if w.eq_ignore_ascii_case("WITHIN")
                        && self.is_word(self.pos + 1, "GROUP")
                    {
                        self.pos += 2;
                        self.within_group();
                        simple = None;
                    } else if select && w.eq_ignore_ascii_case("AS") {
                        self.pos += 1;
                        self.mark(self.pos, SlotKind::Nothing, clause);
                        out.alias = self.alias_after_as(clause);
                        break;
                    } else if select && self.is_name(self.pos) {
                        if let Some(path) = self.single() {
                            self.push_path(&path, RefRole::Definition, clause);
                            out.alias = path.parts.into_iter().next();
                        }
                        break;
                    } else {
                        break;
                    }
                }
            }
        }
        if operands == 1 {
            out.name = simple;
        }
        out
    }

    /// The name after `AS` in a select list (any word but a clause keyword, a quoted
    /// identifier or a string).
    fn alias_after_as(&mut self, clause: Clause) -> Option<Ident> {
        let t = self.peek()?;
        if t.kind == Tk::Str {
            self.pos += 1;
            return Some(Ident {
                text: string_value(self.tok_text(t)),
                quoted: true,
            });
        }
        if !self.is_any_name(self.pos) {
            return None;
        }
        let path = self.single()?;
        self.push_path(&path, RefRole::Definition, clause);
        path.parts.into_iter().next()
    }

    /// The words that may follow an operator word (`NOT IN`, `IS NOT DISTINCT FROM`,
    /// `SIMILAR TO`).
    fn operator_continuation(&mut self) {
        let mut distinct = false;
        while self.word_in(self.pos, words::OPERATOR_CONT) {
            distinct = self.at_word("DISTINCT");
            self.pos += 1;
        }
        if distinct {
            self.eat_word("FROM");
        }
    }

    /// `type 'literal'` at the current word: a typed literal (`DATE '2020-01-01'`,
    /// `TIMESTAMP '...'`).
    fn is_typed_literal(&self, word: &str) -> bool {
        self.tok(self.pos + 1).is_some_and(|t| t.kind == Tk::Str)
            && (words::contains(words::TYPED_LITERALS, word) || self.lang.type_info(word).is_some())
    }

    /// An operand that starts with a name: a column path or `t.*`, or a function call.
    fn name_operand(&mut self, clause: Clause) -> NameOperand {
        let path = self.path(SlotKind::Expr(&[]), clause);
        if path.star {
            return NameOperand::Star;
        }
        if !path.complete {
            return NameOperand::Other;
        }
        if self.at_punct(b'(') {
            self.push_path(&path, RefRole::Function, clause);
            let name = path
                .parts
                .last()
                .map_or_else(String::new, |p| p.text.clone());
            self.call_args(clause, &name);
            return NameOperand::Other;
        }
        self.push_path(&path, RefRole::Column, clause);
        let mut parts = path.parts;
        parts.pop().map_or(NameOperand::Other, NameOperand::Column)
    }

    /// `(` as an operand: a subquery or a parenthesised expression list.
    pub(super) fn paren_operand(&mut self, clause: Clause) {
        if !self.enter() {
            return;
        }
        self.pos += 1;
        if self.query_starts(self.pos) {
            let parent = self.block;
            self.query(parent, BlockKind::Nested);
            self.close_paren();
        } else {
            self.list_until_close(clause);
        }
        self.leave();
    }

    /// `(a, b, ...)` of expressions at the current `(`.
    pub(super) fn paren_list(&mut self, clause: Clause) {
        if !self.enter() {
            return;
        }
        self.pos += 1;
        self.list_until_close(clause);
        self.leave();
    }

    /// Expressions separated by commas up to and including the closing `)`.
    fn list_until_close(&mut self, clause: Clause) {
        loop {
            let before = self.pos;
            self.expr(clause, ExprMode::Plain);
            if self.eat_punct(b',') {
                continue;
            }
            if self.eat_punct(b')') || self.at_end() || self.at_punct(b';') {
                break;
            }
            if self.pos == before {
                // A word no expression takes (`FROM` in `SUBSTRING(x FROM 2)`): skip it.
                self.pos += 1;
            }
        }
    }

    /// Expressions separated by commas.
    pub(super) fn expr_list(&mut self, clause: Clause) {
        loop {
            self.expr(clause, ExprMode::Plain);
            if !self.eat_punct(b',') {
                break;
            }
        }
    }

    /// The arguments of a call to `name` at the current `(`: expressions, a subquery, and the
    /// keyword forms (`CAST(x AS t)`, `EXTRACT(YEAR FROM d)`, `TRIM(BOTH ' ' FROM s)`,
    /// `string_agg(x, ',' ORDER BY y)`, `CONVERT(varchar(10), x)`).
    pub(super) fn call_args(&mut self, clause: Clause, name: &str) {
        if !self.enter() {
            return;
        }
        self.pos += 1;
        if self.query_starts(self.pos) {
            let parent = self.block;
            self.query(parent, BlockKind::Nested);
            self.close_paren();
            self.leave();
            return;
        }
        let mut field_first = words::contains(words::FIELD_FIRST, name);
        let convert =
            name.eq_ignore_ascii_case("CONVERT") || name.eq_ignore_ascii_case("TRY_CONVERT");
        loop {
            self.mark(self.pos, SlotKind::Expr(words::EXPR_START), clause);
            let Some(t) = self.peek() else {
                break;
            };
            match t.kind {
                Tk::Punct(b')') => {
                    self.pos += 1;
                    break;
                }
                Tk::Punct(b';') => break,
                Tk::Punct(b',') => {
                    self.pos += 1;
                    field_first = false;
                }
                Tk::Word if field_first && !self.is_punct(self.pos + 1, b'(') => {
                    self.pos += 1;
                    field_first = false;
                }
                Tk::Word if convert && self.lang.type_info(self.tok_text(t)).is_some() => {
                    let before = self.pos;
                    self.type_name(clause);
                    if self.pos == before {
                        self.pos += 1;
                    }
                }
                _ => {
                    field_first = false;
                    let before = self.pos;
                    self.expr(clause, ExprMode::Plain);
                    if self.pos == before {
                        self.arg_keyword(clause);
                    }
                }
            }
        }
        self.leave();
    }

    /// A keyword inside call arguments where no expression starts: `AS type`,
    /// `RETURNING type`, `ORDER BY ...`, `FROM`, `SEPARATOR`, ... (skipped).
    fn arg_keyword(&mut self, clause: Clause) {
        let word = self.upper(self.pos);
        self.pos += 1;
        match word.as_str() {
            "AS" | "RETURNING" => self.type_name(clause),
            "ORDER" => {
                self.expect_by(Clause::OrderBy);
                self.order_list(Clause::OrderBy);
            }
            _ => {}
        }
    }

    /// A data type at the current token (`int`, `varchar(10)`, `double precision`,
    /// `timestamp with time zone`, `public.my_type`, `int[]`).
    pub(super) fn type_name(&mut self, clause: Clause) {
        self.mark(self.pos, SlotKind::Type, clause);
        if !self.is_any_name(self.pos) || self.word_in(self.pos, words::OPERATORS) {
            return;
        }
        let path = self.path(SlotKind::Type, clause);
        self.push_path(&path, RefRole::Type, clause);
        if !path.complete {
            return;
        }
        while self.word_in(self.pos, words::TYPE_CONT) {
            if self.at_word("WITH")
                && !(self.is_word(self.pos + 1, "TIME") || self.is_word(self.pos + 1, "LOCAL"))
            {
                break;
            }
            self.pos += 1;
        }
        if self.at_punct(b'(') {
            self.skip_group();
        }
        while self.at_punct(b'[') {
            self.pos += 1;
            if self.peek().is_some_and(|t| t.kind == Tk::Num) {
                self.pos += 1;
            }
            self.eat_punct(b']');
        }
        self.eat_word("ARRAY");
    }

    /// `OVER (` at the current `(`: `[window] [PARTITION BY ...] [ORDER BY ...] [frame]`.
    pub(super) fn window_spec(&mut self) {
        if !self.enter() {
            return;
        }
        self.pos += 1;
        if self.is_name(self.pos) && !self.word_in(self.pos, words::FRAME) {
            self.pos += 1;
        }
        if self.at_word("PARTITION") {
            self.pos += 1;
            self.expect_by(Clause::Window);
            self.expr_list(Clause::Window);
        }
        if self.at_word("ORDER") {
            self.pos += 1;
            self.expect_by(Clause::OrderBy);
            self.order_list(Clause::OrderBy);
        }
        self.close_paren();
        self.leave();
    }

    /// `FILTER (WHERE ...)` at the current `(`.
    fn filter_clause(&mut self, clause: Clause) {
        if !self.enter() {
            return;
        }
        self.pos += 1;
        self.eat_word("WHERE");
        self.expr(clause, ExprMode::Plain);
        self.close_paren();
        self.leave();
    }

    /// `WITHIN GROUP (ORDER BY ...)` after `GROUP`.
    fn within_group(&mut self) {
        if !self.at_punct(b'(') || !self.enter() {
            return;
        }
        self.pos += 1;
        if self.eat_word("ORDER") {
            self.expect_by(Clause::OrderBy);
            self.order_list(Clause::OrderBy);
        }
        self.close_paren();
        self.leave();
    }
}

/// `WHEN`, `THEN`, `ELSE`, `END` (inside a `CASE`).
fn is_case_word(w: &str) -> bool {
    ["WHEN", "THEN", "ELSE", "END"]
        .iter()
        .any(|k| w.eq_ignore_ascii_case(k))
}

/// The content of a simple string literal token (`'a''b'` → `a'b`, `N'x'` → `x`).
fn string_value(token: &str) -> String {
    let inner = token.find('\'').map_or(token, |q| &token[q + 1..]);
    let inner = inner.strip_suffix('\'').unwrap_or(inner);
    inner.replace("''", "'")
}
