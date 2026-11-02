//! The significant tokens of one statement, as the diagnostics parser and hover read them: the
//! `oxbase-sql` tokens without whitespace and comments, numbers and parameters told apart from
//! names, and every `(` paired with its `)`.

use std::ops::Range;

use oxbase_sql::{SqlTokenKind, tokenize};

use super::features::Features;
use crate::Dialect;

/// What a [`Tok`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A keyword or a bare identifier.
    Word,
    /// `12`, `1.5`, `1e3`, `.5`.
    Number,
    /// `$1`, `?`, `?1`, `:name`, `@name`, `@@name`, `$name`.
    Param,
    /// A quoted identifier.
    Quoted,
    /// A string literal.
    Str,
    /// One byte of punctuation or an operator character.
    Punct(u8),
    /// Anything else (`:=`).
    Other,
}

/// One significant token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tok {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
    /// For `(`: the index of its `)`, `None` when it is never closed.
    pub close: Option<usize>,
    /// A line break separates it from the previous significant token (or it is the first).
    pub line_start: bool,
}

/// The significant tokens of `text[range]`.
#[derive(Debug, Clone)]
pub(crate) struct Toks<'a> {
    pub text: &'a str,
    pub all: Vec<Tok>,
    /// The statement holds a MySQL executable comment (`/*! ... */`) whose content the parser
    /// does not read: findings that depend on the statement's structure are only hints.
    pub opaque: bool,
}

impl<'a> Toks<'a> {
    /// Reads the tokens of `text[range]` (a statement as `oxbase_sql::split` cuts it).
    pub fn new(dialect: &Dialect, feat: &Features, text: &'a str, range: Range<usize>) -> Self {
        let range = clamp(text, range);
        let base = range.start;
        let slice = &text[range];
        let mut raw: Vec<Tok> = Vec::new();
        let mut opaque = false;
        let mut line_break = true;
        for tok in tokenize(dialect.sql, slice) {
            let (start, end) = (base + tok.start, base + tok.end);
            let kind = match tok.kind {
                SqlTokenKind::Space | SqlTokenKind::LineComment | SqlTokenKind::BlockComment => {
                    if text[start..end].contains('\n') {
                        line_break = true;
                    }
                    continue;
                }
                SqlTokenKind::ExecComment => {
                    // Optimizer hints (`/*+ ... */`) change no names; other executable
                    // comments may hold any SQL.
                    if !text[start..end].starts_with("/*+") {
                        opaque = true;
                    }
                    continue;
                }
                SqlTokenKind::String { .. } => Kind::Str,
                SqlTokenKind::QuotedIdent { .. } => Kind::Quoted,
                SqlTokenKind::Word => word_kind(&text[start..end]),
                SqlTokenKind::Punct => {
                    if end - start == 1 {
                        Kind::Punct(text.as_bytes()[start])
                    } else {
                        Kind::Other
                    }
                }
            };
            raw.push(Tok {
                kind,
                start,
                end,
                close: None,
                line_start: line_break,
            });
            line_break = false;
        }
        let mut toks = merge(&raw, feat);
        pair_parens(&mut toks);
        Self {
            text,
            all: toks,
            opaque,
        }
    }

    pub fn len(&self) -> usize {
        self.all.len()
    }

    pub fn kind(&self, i: usize) -> Option<Kind> {
        self.all.get(i).map(|t| t.kind)
    }

    /// The token's text (empty past the end).
    pub fn text(&self, i: usize) -> &'a str {
        self.all.get(i).map_or("", |t| &self.text[t.start..t.end])
    }

    pub fn range(&self, i: usize) -> Range<usize> {
        self.all.get(i).map_or(0..0, |t| t.start..t.end)
    }

    /// `true` when token `i` is the word `kw` (ASCII case-insensitive).
    pub fn is_word(&self, i: usize, kw: &str) -> bool {
        self.kind(i) == Some(Kind::Word) && self.text(i).eq_ignore_ascii_case(kw)
    }

    /// `true` when token `i` is one of the words of `list`.
    pub fn word_in(&self, i: usize, list: &[&str]) -> bool {
        self.kind(i) == Some(Kind::Word) && {
            let w = self.text(i);
            list.iter().any(|k| w.eq_ignore_ascii_case(k))
        }
    }

    pub fn is_punct(&self, i: usize, c: u8) -> bool {
        self.kind(i) == Some(Kind::Punct(c))
    }

    /// `true` when token `i` is the punctuation `c` and touches the token before it.
    pub fn is_punct_touching(&self, i: usize, c: u8) -> bool {
        self.is_punct(i, c)
            && i > 0
            && self
                .all
                .get(i - 1)
                .zip(self.all.get(i))
                .is_some_and(|(a, b)| a.end == b.start)
    }

    /// A word or quoted identifier.
    pub fn is_name(&self, i: usize) -> bool {
        matches!(self.kind(i), Some(Kind::Word | Kind::Quoted))
    }

    /// The index of the `)` of the `(` at `i`, or `hi` when it is not closed before `hi`.
    pub fn close_of(&self, i: usize, hi: usize) -> usize {
        self.all
            .get(i)
            .and_then(|t| t.close)
            .filter(|&c| c < hi)
            .unwrap_or(hi)
    }

    /// The upper-case text of word `i` (empty for other tokens).
    pub fn upper(&self, i: usize) -> String {
        if self.kind(i) == Some(Kind::Word) {
            self.text(i).to_ascii_uppercase()
        } else {
            String::new()
        }
    }
}

/// `range` cut to the text and to char boundaries (statement ranges always are; this keeps a
/// bad range from panicking).
fn clamp(text: &str, range: Range<usize>) -> Range<usize> {
    let Range { start, end } = range;
    let end = floor_boundary(text, end.min(text.len()));
    let start = floor_boundary(text, start.min(end));
    start..end
}

pub(crate) fn floor_boundary(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn word_kind(w: &str) -> Kind {
    match w.as_bytes().first() {
        Some(b) if b.is_ascii_digit() => Kind::Number,
        // T-SQL `@var` / `@@ROWCOUNT` (the lexer keeps `@` in words there), PostgreSQL `$1`,
        // SQLite `$name`.
        Some(b'@' | b'$') => Kind::Param,
        _ => Kind::Word,
    }
}

/// Joins tokens the lexer keeps apart: `1` `.` `5` into one number, `@` `name` and `:` `name`
/// and `?` `1` into one parameter.
fn merge(raw: &[Tok], feat: &Features) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let t = raw[i];
        let touch = |a: &Tok, b: Option<&Tok>| b.is_some_and(|b| a.end == b.start);
        let next = raw.get(i + 1);
        let next2 = raw.get(i + 2);
        let prev_touching = out.last().is_some_and(|p| p.end == t.start);
        match t.kind {
            // `1.5` and `1.`
            Kind::Number
                if next.is_some_and(|n| n.kind == Kind::Punct(b'.')) && touch(&t, next) =>
            {
                let dot = raw[i + 1];
                let mut end = dot.end;
                let mut used = 2;
                if let Some(frac) = next2
                    && frac.kind == Kind::Number
                    && dot.end == frac.start
                {
                    end = frac.end;
                    used = 3;
                }
                out.push(Tok { end, ..t });
                i += used;
            }
            // `.5` where no name comes before the dot.
            Kind::Punct(b'.')
                if !prev_touching
                    && next.is_some_and(|n| n.kind == Kind::Number)
                    && touch(&t, next) =>
            {
                let n = raw[i + 1];
                out.push(Tok {
                    kind: Kind::Number,
                    end: n.end,
                    ..t
                });
                i += 2;
            }
            // `?` and `?1`
            Kind::Punct(b'?') => {
                if next.is_some_and(|n| n.kind == Kind::Number) && touch(&t, next) {
                    out.push(Tok {
                        kind: Kind::Param,
                        end: raw[i + 1].end,
                        ..t
                    });
                    i += 2;
                } else {
                    out.push(Tok {
                        kind: Kind::Param,
                        ..t
                    });
                    i += 1;
                }
            }
            // `:name` (not the `::` of a cast)
            Kind::Punct(b':')
                if next.is_some_and(|n| n.kind == Kind::Word)
                    && touch(&t, next)
                    && !(prev_touching
                        && out.last().is_some_and(|p| p.kind == Kind::Punct(b':'))) =>
            {
                out.push(Tok {
                    kind: Kind::Param,
                    end: raw[i + 1].end,
                    ..t
                });
                i += 2;
            }
            // MySQL / SQLite `@name`, `@@name`
            Kind::Punct(b'@') if feat.at_params => {
                let mut j = i + 1;
                let mut end = t.end;
                while let Some(n) = raw.get(j)
                    && n.start == end
                    && matches!(
                        n.kind,
                        Kind::Punct(b'@') | Kind::Word | Kind::Quoted | Kind::Str
                    )
                {
                    end = n.end;
                    j += 1;
                    if !matches!(n.kind, Kind::Punct(b'@')) {
                        break;
                    }
                }
                if j > i + 1 {
                    out.push(Tok {
                        kind: Kind::Param,
                        end,
                        ..t
                    });
                    i = j;
                } else {
                    out.push(t);
                    i += 1;
                }
            }
            _ => {
                out.push(t);
                i += 1;
            }
        }
    }
    out
}

fn pair_parens(toks: &mut [Tok]) {
    let mut stack: Vec<usize> = Vec::new();
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            Kind::Punct(b'(') => stack.push(i),
            Kind::Punct(b')') => {
                if let Some(open) = stack.pop() {
                    pairs.push((open, i));
                }
            }
            _ => {}
        }
    }
    for (open, close) in pairs {
        toks[open].close = Some(close);
    }
}
