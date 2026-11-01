//! A script: its statements (from `oxbase_sql::split`) and the gaps between them.
//!
//! A gap holds the terminator (`;`, a MySQL client delimiter), comments, blank lines and the
//! client directive lines (T-SQL `GO`, MySQL `DELIMITER`). The formatter keeps every
//! non-blank line of a gap as typed (only its surrounding whitespace changes): the first one
//! stays at the end of the statement's last line when it was there (or when it starts with
//! `;`), the others go on lines of their own. Blank lines collapse to at most one; with
//! `blank_line_between_statements` there is at least one between two statements (before the
//! first comment line of the gap, which usually belongs to the next statement, else right
//! before the next statement), without it there are none.

use oxbase_sql::{SqlTokenKind, Statement, tokenize};

use super::FormatOptions;
use super::doc::{Layout, render};
use super::{layout, prep, tree};
use crate::{Dialect, check_format};

/// One non-blank line of a gap.
#[derive(Debug, Clone, Copy)]
struct GapLine<'a> {
    text: &'a str,
    /// On the line the gap started on (right after the statement before it).
    same_line: bool,
    /// A blank line came before it.
    blank_before: bool,
    /// Holds only comments.
    comment_only: bool,
}

#[derive(Debug, Clone, Default)]
struct Gap<'a> {
    lines: Vec<GapLine<'a>>,
    /// Whitespace came before the first line.
    space_first: bool,
    /// A blank line came after the last line.
    blank_end: bool,
}

/// The lines of `text` (a gap): runs of tokens between line breaks that are not inside a token.
fn gap<'a>(dialect: &Dialect, text: &'a str) -> Gap<'a> {
    let mut g = Gap::default();
    let mut newlines = 0usize;
    let mut seen = false;
    let mut cur: Option<GapLine<'a>> = None;
    let mut start = 0;
    for t in tokenize(dialect.sql, text) {
        if t.kind == SqlTokenKind::Space {
            if !seen {
                g.space_first = true;
            }
            let n = t.text(text).matches('\n').count();
            if n > 0
                && let Some(line) = cur.take()
            {
                g.lines.push(line);
            }
            newlines += n;
            continue;
        }
        let comment = matches!(
            t.kind,
            SqlTokenKind::LineComment | SqlTokenKind::BlockComment
        );
        if cur.is_none() {
            start = t.start;
            cur = Some(GapLine {
                text: "",
                same_line: !seen && newlines == 0,
                blank_before: newlines >= 2,
                comment_only: true,
            });
            newlines = 0;
        }
        if let Some(line) = &mut cur {
            line.text = &text[start..t.end];
            line.comment_only &= comment;
        }
        seen = true;
    }
    if let Some(line) = cur {
        g.lines.push(line);
    }
    g.blank_end = newlines >= 2;
    g
}

/// `true` when characters `a` then `b`, touching in the input, must keep touching (a `.`, or
/// two operator characters: the rule of `oxbase_sql::tokens_equivalent`).
fn glued(a: Option<char>, b: Option<char>) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return false;
    };
    let op = |c: char| c.is_ascii_punctuation() && !"(),;'\"`[]".contains(c);
    a == '.' || b == '.' || (op(a) && op(b))
}

/// `true` when the gap `text` ends without whitespace right before `next`, and touching matters
/// there.
fn touches_next(text: &str, next: &str) -> bool {
    !text.ends_with(|c: char| c.is_ascii_whitespace())
        && glued(text.chars().last(), next.chars().next())
}

/// `true` when `text` starts with the word `DELIMITER`: at the start of a line it would become
/// a MySQL client directive, so such a statement is never moved to the start of a line.
fn starts_with_delimiter(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes
        .get(..9)
        .is_some_and(|b| b.eq_ignore_ascii_case(b"DELIMITER"))
        && bytes.get(9).is_none_or(u8::is_ascii_whitespace)
}

/// Columns of `text` with tabs counted as `tab` columns.
pub(super) fn columns(text: &str, tab: usize) -> usize {
    text.chars().map(|c| if c == '\t' { tab } else { 1 }).sum()
}

/// Writes formatted statements and gaps.
pub(super) struct Writer<'d> {
    pub dialect: &'d Dialect,
    pub options: &'d FormatOptions,
    pub eol: &'static str,
    /// Written at the start of every new line (`format_range`).
    pub prefix: String,
    pub out: String,
}

impl Writer<'_> {
    fn blank(&self) -> bool {
        self.options.blank_line_between_statements
    }

    /// Starts a new line (with the prefix when `text` follows).
    fn line(&mut self, text: &str) {
        self.out.push_str(self.eol);
        self.out.push_str(&self.prefix);
        self.out.push_str(text);
    }

    /// One statement (`text`, starting at column `start_col`): formatted, or as typed when
    /// the formatter cannot lay it out safely.
    pub fn statement(&mut self, text: &str, start_col: usize) {
        let formatted = self.format_statement(text, start_col);
        self.out.push_str(formatted.as_deref().unwrap_or(text));
    }

    fn format_statement(&self, text: &str, start_col: usize) -> Option<String> {
        let o = self.options;
        let items = prep::prepare(self.dialect.sql, text, &self.dialect.language)?;
        if prep::is_procedural(&items) {
            return None;
        }
        let nodes = tree::tree(&items)?;
        let docs = layout::build(&items, &nodes, o.keyword_case, o.leading_commas);
        let unit = if o.use_tabs {
            "\t".to_owned()
        } else {
            " ".repeat(usize::from(o.indent_width))
        };
        let tab = usize::from(o.indent_width);
        let out = render(
            &docs,
            &Layout {
                max: usize::from(o.max_line_length),
                unit: &unit,
                unit_width: tab,
                prefix: &self.prefix,
                prefix_width: columns(&self.prefix, tab),
                start_col,
                eol: self.eol,
            },
        );
        check_format(self.dialect, text, &out).ok()?;
        Some(out)
    }

    /// `true` when statement `text` ends with a line comment.
    fn ends_with_line_comment(&self, text: &str) -> bool {
        tokenize(self.dialect.sql, text)
            .filter(|t| t.kind != SqlTokenKind::Space)
            .last()
            .is_some_and(|t| t.kind == SqlTokenKind::LineComment)
    }

    /// Puts the gap's first line after the statement just written when it belongs there;
    /// returns the remaining lines.
    fn attach<'g>(&mut self, g: &'g Gap<'g>, statement: &str) -> &'g [GapLine<'g>] {
        let Some(first) = g.lines.first() else {
            return &g.lines;
        };
        let terminator = first.text.starts_with(';');
        let movable = terminator
            && !self.ends_with_line_comment(statement)
            && !glued(statement.chars().last(), Some(';'));
        if first.same_line || movable {
            if first.same_line && g.space_first && !terminator {
                self.out.push(' ');
            }
            self.out.push_str(first.text);
            return &g.lines[1..];
        }
        &g.lines
    }

    /// The gap `text` between `statement` (just written) and the next statement; ends on a new
    /// line, ready for the next statement.
    pub fn between(&mut self, text: &str, statement: &str, next: &str) {
        if starts_with_delimiter(next) && !text.contains('\n') {
            self.out.push_str(text);
            return;
        }
        let g = gap(self.dialect, text);
        let lines = self.attach(&g, statement);
        let blank = self.blank();
        let has_blank = lines.iter().any(|l| l.blank_before) || g.blank_end;
        let insert_at = if blank && !has_blank {
            lines
                .iter()
                .position(|l| l.comment_only)
                .unwrap_or(lines.len())
        } else {
            usize::MAX
        };
        for (k, l) in lines.iter().enumerate() {
            if blank && (l.blank_before || k == insert_at) {
                self.out.push_str(self.eol);
            }
            self.line(l.text);
        }
        if touches_next(text, next) {
            return;
        }
        if blank && (g.blank_end || insert_at == lines.len()) {
            self.out.push_str(self.eol);
        }
        self.out.push_str(self.eol);
        self.out.push_str(&self.prefix);
    }

    /// The gap before the first statement (`statement`) or a text without statements.
    pub fn leading(&mut self, text: &str, statement: Option<&str>) {
        let last_line = &text[text.rfind('\n').map_or(0, |nl| nl + 1)..];
        if statement.is_some_and(starts_with_delimiter) && !last_line.trim().is_empty() {
            self.out.push_str(text);
            return;
        }
        let g = gap(self.dialect, text);
        let blank = self.blank();
        for (k, l) in g.lines.iter().enumerate() {
            if k == 0 {
                self.out.push_str(l.text);
                continue;
            }
            if blank && l.blank_before {
                self.out.push_str(self.eol);
            }
            self.line(l.text);
        }
        if let Some(statement) = statement
            && !g.lines.is_empty()
            && !touches_next(text, statement)
        {
            if blank && g.blank_end {
                self.out.push_str(self.eol);
            }
            self.out.push_str(self.eol);
        }
    }

    /// The gap after the last statement.
    pub fn trailing(&mut self, text: &str, statement: &str) {
        let g = gap(self.dialect, text);
        let lines = self.attach(&g, statement);
        let blank = self.blank();
        for l in lines {
            if blank && l.blank_before {
                self.out.push_str(self.eol);
            }
            self.line(l.text);
        }
    }
}

/// The whole of `text` formatted (statements from `statements`, the split of `text`).
///
/// From the first token of `text` that runs unterminated to its end (an open string,
/// identifier or comment) the text is kept as typed: every character after it belongs to that
/// token. The statement holding it, or the gap holding it and everything after, is copied.
pub(super) fn whole(w: &mut Writer<'_>, text: &str, statements: &[Statement]) {
    let open = tokenize(w.dialect.sql, text)
        .find(|t| !t.is_terminated(text))
        .map(|t| t.start);
    let Some(open) = open else {
        whole_closed(w, text, statements);
        return;
    };
    // Statements laid out, where the verbatim rest starts, and whether the gap before it is
    // laid out (the rest starts at a statement).
    let (count, cut, gap_before_cut) = match statements.iter().position(|s| s.range.end > open) {
        Some(k) if statements[k].range.start <= open => (k, statements[k].range.start, true),
        Some(k) => (
            k,
            k.checked_sub(1).map_or(0, |p| statements[p].range.end),
            false,
        ),
        None => (
            statements.len(),
            statements.last().map_or(0, |s| s.range.end),
            false,
        ),
    };
    if let Some(first) = statements.first()
        && (count > 0 || gap_before_cut)
    {
        w.leading(&text[..first.range.start], Some(&first.text));
        for (k, s) in statements[..count].iter().enumerate() {
            w.statement(&s.text, 0);
            if let Some(next) = statements.get(k + 1)
                && (k + 1 < count || gap_before_cut)
            {
                w.between(&text[s.range.end..next.range.start], &s.text, &next.text);
            }
        }
    }
    w.out.push_str(&text[cut..]);
}

/// [`whole`] for a text whose tokens are all terminated.
fn whole_closed(w: &mut Writer<'_>, text: &str, statements: &[Statement]) {
    let Some(first) = statements.first() else {
        w.leading(text, None);
        if !w.out.is_empty() && text.ends_with('\n') {
            w.out.push_str(w.eol);
        }
        return;
    };
    w.leading(&text[..first.range.start], Some(&first.text));
    for (k, s) in statements.iter().enumerate() {
        w.statement(&s.text, 0);
        match statements.get(k + 1) {
            Some(next) => w.between(&text[s.range.end..next.range.start], &s.text, &next.text),
            None => w.trailing(&text[s.range.end..], &s.text),
        }
    }
    if text.ends_with('\n') {
        w.out.push_str(w.eol);
    }
}

/// Statements `statements` (consecutive, at least one) of `text` and the gaps between them;
/// the first starts at column `start_col`.
pub(super) fn span(w: &mut Writer<'_>, text: &str, statements: &[Statement], start_col: usize) {
    let tab = usize::from(w.options.indent_width);
    for (k, s) in statements.iter().enumerate() {
        let col = if k == 0 {
            start_col
        } else {
            columns(&w.prefix, tab)
        };
        w.statement(&s.text, col);
        if let Some(next) = statements.get(k + 1) {
            w.between(&text[s.range.end..next.range.start], &s.text, &next.text);
        }
    }
}
