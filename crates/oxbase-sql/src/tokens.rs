//! M6: the public token API over the splitter's lexer (`docs/m6-contracts.md` §2), and the
//! formatter's safety check [`tokens_equivalent`].
//!
//! [`tokenize`] yields the same tokens the splitter sees, so code insight, the formatter and the
//! splitter never disagree about where a string, a quoted identifier or a comment ends. It works
//! on whole texts and never fails: unterminated strings, identifiers and comments run to the end
//! of the text (see [`SqlToken::is_terminated`]).
//!
//! The tokenizer knows no client directives: a T-SQL `GO` line or a MySQL `DELIMITER` line is
//! ordinary words and punctuation here. Callers that work per statement cut the text with
//! [`crate::split`] first.

use std::fmt;
use std::ops::Range;

use crate::SqlDialect;
use crate::lex::{self, Rules, TokenKind};

/// What a [`SqlToken`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqlTokenKind {
    /// ASCII whitespace.
    Space,
    /// `-- ...` (MySQL also `# ...`) up to the end of the line, without trailing whitespace.
    LineComment,
    /// `/* ... */` (nested where the dialect nests them).
    BlockComment,
    /// MySQL `/*! ... */`, `/*M! ... */` and `/*+ ... */`: content for the server.
    ExecComment,
    /// A string literal in any quote form of the dialect, including its prefix (`E'...'`) and
    /// dollar tags. `inner` is its content without quotes, prefix or tags (escapes kept as
    /// written).
    String {
        inner_start: usize,
        inner_end: usize,
    },
    /// A quoted identifier opened by `quote` (`"`, `` ` `` or `[`).
    QuotedIdent { quote: u8 },
    /// A keyword, identifier, number or parameter (`$1`, T-SQL `@var`).
    Word,
    /// One ASCII byte of punctuation or an operator character, or `:=`.
    Punct,
}

/// One token: its kind and byte range in the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SqlToken {
    pub kind: SqlTokenKind,
    pub start: usize,
    pub end: usize,
}

impl SqlToken {
    /// The token's byte range.
    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }

    /// The token's text in `source` (the text it was read from).
    pub fn text<'a>(&self, source: &'a str) -> &'a str {
        &source[self.start..self.end]
    }

    /// Whitespace or a comment (not an executable MySQL comment).
    pub fn is_trivia(&self) -> bool {
        matches!(
            self.kind,
            SqlTokenKind::Space | SqlTokenKind::LineComment | SqlTokenKind::BlockComment
        )
    }

    /// `false` for a string, quoted identifier or block comment that runs to the end of the
    /// text without its closing quote or `*/`.
    pub fn is_terminated(&self, source: &str) -> bool {
        let text = self.text(source);
        match self.kind {
            // The lexer ends an unterminated string's content at the token's end; a closed
            // one has its closing quote or tag after the content.
            SqlTokenKind::String { inner_end, .. } => inner_end < self.end,
            SqlTokenKind::QuotedIdent { quote } => {
                let close = if quote == b'[' {
                    ']'
                } else {
                    char::from(quote)
                };
                let inner = &text[1..];
                inner.ends_with(close) && !ends_with_escaped(inner, close)
            }
            // Best effort for nesting dialects: `/* a /* b */` counts as closed.
            SqlTokenKind::BlockComment | SqlTokenKind::ExecComment => {
                text.len() >= 4 && text.ends_with("*/")
            }
            _ => true,
        }
    }
}

/// The tokens of `text` in order. Their ranges cover the text without gaps.
pub fn tokenize(dialect: SqlDialect, text: &str) -> Tokens<'_> {
    Tokens {
        text,
        at: 0,
        rules: Rules::new(dialect),
    }
}

/// Iterator returned by [`tokenize`].
#[derive(Debug, Clone)]
pub struct Tokens<'a> {
    text: &'a str,
    at: usize,
    rules: Rules,
}

impl Iterator for Tokens<'_> {
    type Item = SqlToken;

    fn next(&mut self) -> Option<SqlToken> {
        if self.at >= self.text.len() {
            return None;
        }
        let tok = lex::token_at(self.text, self.at, self.rules, &[]);
        self.at = tok.end;
        let kind = match tok.kind {
            TokenKind::Space => SqlTokenKind::Space,
            TokenKind::LineComment => SqlTokenKind::LineComment,
            TokenKind::BlockComment => SqlTokenKind::BlockComment,
            TokenKind::ExecComment => SqlTokenKind::ExecComment,
            TokenKind::Str {
                inner_start,
                inner_end,
            } => SqlTokenKind::String {
                inner_start,
                inner_end,
            },
            TokenKind::Ident { quote } => SqlTokenKind::QuotedIdent { quote },
            TokenKind::Word => SqlTokenKind::Word,
            TokenKind::Punct => SqlTokenKind::Punct,
        };
        Some(SqlToken {
            kind,
            start: tok.start,
            end: tok.end,
        })
    }
}

/// The value of a quoted identifier token's text (`"a""b"` → `a"b`, `[x]]y]` → `x]y`,
/// `` `c` `` → `c`); an unterminated one runs to the end. Any other text is returned unchanged.
pub fn ident_value(token_text: &str) -> String {
    let bytes = token_text.as_bytes();
    let Some(&open) = bytes.first() else {
        return String::new();
    };
    let close = match open {
        b'"' => '"',
        b'`' => '`',
        b'[' => ']',
        _ => return token_text.to_owned(),
    };
    let mut inner = &token_text[1..];
    if inner.ends_with(close) && !ends_with_escaped(inner, close) {
        inner = &inner[..inner.len() - 1];
    }
    let doubled: String = [close, close].iter().collect();
    inner.replace(&doubled, &close.to_string())
}

/// `true` when `inner` (identifier text after the opening quote) ends with a doubled
/// closing quote that is content, not the terminator (an unterminated `"a""`).
fn ends_with_escaped(inner: &str, close: char) -> bool {
    let run = inner.chars().rev().take_while(|&c| c == close).count();
    run % 2 == 0
}

/// Where [`tokens_equivalent`] found the first difference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenMismatch {
    /// Byte offset in the first text (its length when the first text ran out).
    pub before: usize,
    /// Byte offset in the second text (its length when it ran out).
    pub after: usize,
    pub reason: &'static str,
}

impl fmt::Display for TokenMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (byte {} before, byte {} after)",
            self.reason, self.before, self.after
        )
    }
}

impl std::error::Error for TokenMismatch {}

/// The formatter invariant (`docs/m6-contracts.md` §7): `after` must be `before` with only
/// whitespace and keyword case changed.
///
/// Checked, in order:
///
/// 1. Both texts split into the same number of statements ([`crate::split`]): no `GO` or
///    `DELIMITER` line was joined to another line, no terminator moved.
/// 2. The non-whitespace tokens are the same, one for one: same kind and same text, except
///    that a word for which `case_insensitive_word(word)` is `true` may differ in ASCII case.
///    Comments must be byte-identical (they may move to another position only together with
///    their neighbours: a line comment is still followed by a line break, or the next token
///    would have become part of it).
/// 3. Tokens that touch in one text touch in the other, for every pair whose meaning depends
///    on touching: two operator characters (`<` `=`, `:` `:`, `|` `|`, `-` `>`), anything next
///    to `.`, a word followed by a string or quoted identifier (`N'x'`, `X'ff'`, `_utf8'a'`),
///    a parameter or variable sigil (`$ @ : ? # &`) followed by a word, and a number with an
///    exponent marker followed by its sign (`1e` `-` `5`). Parentheses, commas and `;` may
///    gain or lose spaces freely.
#[allow(clippy::many_single_char_names)] // short loop and closure names
pub fn tokens_equivalent(
    dialect: SqlDialect,
    before: &str,
    after: &str,
    case_insensitive_word: &dyn Fn(&str) -> bool,
) -> Result<(), TokenMismatch> {
    let a: Vec<SqlToken> = tokenize(dialect, before)
        .filter(|t| t.kind != SqlTokenKind::Space)
        .collect();
    let b: Vec<SqlToken> = tokenize(dialect, after)
        .filter(|t| t.kind != SqlTokenKind::Space)
        .collect();
    for i in 0..a.len().max(b.len()) {
        let (Some(x), Some(y)) = (a.get(i), b.get(i)) else {
            return Err(TokenMismatch {
                before: a.get(i).map_or(before.len(), |t| t.start),
                after: b.get(i).map_or(after.len(), |t| t.start),
                reason: "the texts have a different number of tokens",
            });
        };
        let mismatch = |reason| TokenMismatch {
            before: x.start,
            after: y.start,
            reason,
        };
        if std::mem::discriminant(&x.kind) != std::mem::discriminant(&y.kind) {
            return Err(mismatch("a token changed its kind"));
        }
        let (tx, ty) = (x.text(before), y.text(after));
        let same = tx == ty
            || (x.kind == SqlTokenKind::Word
                && tx.eq_ignore_ascii_case(ty)
                && case_insensitive_word(tx));
        if !same {
            return Err(mismatch("a token changed its text"));
        }
        if i > 0 {
            let (px, py) = (a[i - 1], b[i - 1]);
            if glue_matters(before, px, *x) || glue_matters(after, py, *y) {
                let touch_a = px.end == x.start;
                let touch_b = py.end == y.start;
                if touch_a != touch_b {
                    return Err(mismatch("tokens that must touch were separated or joined"));
                }
            }
        }
    }
    if crate::split(dialect, before).len() != crate::split(dialect, after).len() {
        return Err(TokenMismatch {
            before: 0,
            after: 0,
            reason: "the texts split into a different number of statements",
        });
    }
    Ok(())
}

/// Whether touching or not changes the meaning of `prev` followed by `next` (rule 3 of
/// [`tokens_equivalent`]).
fn glue_matters(text: &str, prev: SqlToken, next: SqlToken) -> bool {
    use SqlTokenKind::{Punct, QuotedIdent, String as Str, Word};
    let pt = prev.text(text);
    let nt = next.text(text);
    let free = |t: &str| matches!(t, "(" | ")" | "," | ";");
    if pt == "." || nt == "." {
        return true;
    }
    match (prev.kind, next.kind) {
        (Punct, Punct) => !free(pt) && !free(nt),
        (Word, Str { .. } | QuotedIdent { .. }) => true,
        (Punct, Word) => matches!(pt, "$" | "@" | ":" | "?" | "#" | "&"),
        (Word, Punct) => {
            matches!(nt, "+" | "-")
                && pt.as_bytes()[0].is_ascii_digit()
                && matches!(pt.as_bytes()[pt.len() - 1], b'e' | b'E')
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(dialect: SqlDialect, text: &str) -> Vec<(SqlTokenKind, &str)> {
        tokenize(dialect, text)
            .filter(|t| t.kind != SqlTokenKind::Space)
            .map(|t| (t.kind, t.text(text)))
            .collect()
    }

    #[test]
    fn tokens_cover_the_text() {
        let text = "SELECT \"a\"\"b\", 'x''y' -- c\nFROM [t] /* open";
        let mut at = 0;
        for t in tokenize(SqlDialect::TSql, text) {
            assert_eq!(t.start, at);
            assert!(t.end > t.start);
            at = t.end;
        }
        assert_eq!(at, text.len());
        let toks = kinds(SqlDialect::TSql, text);
        assert_eq!(toks[0], (SqlTokenKind::Word, "SELECT"));
        assert_eq!(
            toks[1],
            (SqlTokenKind::QuotedIdent { quote: b'"' }, "\"a\"\"b\"")
        );
        assert_eq!(toks[4].0, SqlTokenKind::LineComment);
        assert_eq!(toks[6], (SqlTokenKind::QuotedIdent { quote: b'[' }, "[t]"));
        assert_eq!(toks[7].0, SqlTokenKind::BlockComment);
    }

    #[test]
    fn termination() {
        let check = |dialect, text: &str| {
            let t = tokenize(dialect, text)
                .find(|t| t.kind != SqlTokenKind::Space)
                .unwrap();
            t.is_terminated(text)
        };
        assert!(check(SqlDialect::Postgres, "'abc'"));
        assert!(!check(SqlDialect::Postgres, "'abc"));
        assert!(!check(SqlDialect::Postgres, "'"));
        assert!(check(SqlDialect::Postgres, "''"));
        assert!(check(SqlDialect::Postgres, "$$x$$"));
        assert!(!check(SqlDialect::Postgres, "$$x"));
        assert!(!check(SqlDialect::Postgres, "\"ab"));
        assert!(check(SqlDialect::Postgres, "\"ab\""));
        assert!(!check(SqlDialect::TSql, "[ab"));
        assert!(!check(SqlDialect::Postgres, "/* x"));
        assert!(!check(SqlDialect::Postgres, "/*/"));
        assert!(check(SqlDialect::Postgres, "/**/"));
        assert!(check(SqlDialect::Postgres, "word"));
    }

    #[test]
    fn identifier_values() {
        assert_eq!(ident_value("\"a\"\"b\""), "a\"b");
        assert_eq!(ident_value("[x]]y]"), "x]y");
        assert_eq!(ident_value("`c``d`"), "c`d");
        assert_eq!(ident_value("\"open"), "open");
        assert_eq!(ident_value("\"a\"\""), "a\"");
        assert_eq!(ident_value("plain"), "plain");
        assert_eq!(ident_value(""), "");
    }

    fn equivalent(dialect: SqlDialect, a: &str, b: &str) -> bool {
        let keyword = |w: &str| {
            ["select", "from", "where"]
                .iter()
                .any(|k| k.eq_ignore_ascii_case(w))
        };
        tokens_equivalent(dialect, a, b, &keyword).is_ok()
    }

    #[test]
    fn equivalence_allows_whitespace_and_keyword_case() {
        let pg = SqlDialect::Postgres;
        assert!(equivalent(
            pg,
            "select a,b from t where x=1",
            "SELECT a, b\nFROM t\nWHERE x = 1"
        ));
        assert!(equivalent(pg, "select (a)", "SELECT ( a )"));
        assert!(equivalent(
            pg,
            "select 1 -- c\nfrom t",
            "SELECT 1 -- c\n  FROM t"
        ));
    }

    #[test]
    fn equivalence_rejects_changes() {
        let pg = SqlDialect::Postgres;
        // Identifier case.
        assert!(!equivalent(pg, "select Abc", "select abc"));
        // A line comment swallowing the next line.
        assert!(!equivalent(
            pg,
            "select 1 -- c\nfrom t",
            "select 1 -- c from t"
        ));
        // Operators split or joined.
        assert!(!equivalent(pg, "select a<=b", "select a < = b"));
        assert!(!equivalent(pg, "select a::int", "select a: :int"));
        assert!(!equivalent(pg, "select t.a", "select t . a"));
        assert!(!equivalent(pg, "select 1.5", "select 1. 5"));
        assert!(!equivalent(pg, "select 1e-5", "select 1e - 5"));
        assert!(!equivalent(pg, "select $1", "select $ 1"));
        assert!(!equivalent(SqlDialect::TSql, "select N'x'", "select N 'x'"));
        // Minus minus becomes a comment.
        assert!(!equivalent(pg, "select a - -b", "select a--b"));
        // Comment text.
        assert!(!equivalent(pg, "select 1 /* a  b */", "select 1 /* a b */"));
        // A missing token.
        assert!(!equivalent(pg, "select 1;", "select 1"));
        // A GO line joined to the batch.
        assert!(!equivalent(
            SqlDialect::TSql,
            "select 1\nGO\nselect 2",
            "select 1 GO select 2"
        ));
    }
}
