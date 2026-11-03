//! A small JavaScript tokenizer for the editor-side work on shell scripts: splitting
//! ([`super::split`]), guards and history redaction ([`super::lang`]). It never fails and
//! never panics: unterminated strings end at the end of their line, unterminated comments,
//! templates and regular expressions at the end of the text.
//!
//! It knows just enough JavaScript for that: strings, template literals with nested `${ }`
//! expressions, comments, regular-expression literals (a `/` where an operand is expected,
//! including after `return`, `typeof`, ...), brackets and punctuation. Line breaks are tokens
//! of their own, comments are skipped.

use std::ops::Range;

/// Words after which a `/` starts a regular expression rather than a division.
const REGEX_KEYWORDS: [&str; 14] = [
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "yield",
    "await",
];

/// What a token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tok {
    /// An identifier, keyword or number (`[A-Za-z0-9_$]` and every non-ASCII character).
    Word,
    /// A `'...'` or `"..."` string, quotes included.
    Str,
    /// A piece of a template literal: from its backtick (or the `}` that ends an embedded
    /// expression) to the closing backtick or the next `${`.
    Template,
    /// A regular-expression literal with its flags.
    Regex,
    /// Punctuation: the first byte (`++`, `--` and `=>` are one token each).
    Punct(u8),
    /// A line break (`\n`).
    Newline,
}

/// An open bracket or template expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frame {
    Paren,
    Bracket,
    Brace,
    /// Inside the `${ }` of a template literal.
    Template,
}

/// What the previous significant token was, for the regular-expression rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prev {
    /// Start of text, an operator or an opening bracket: a `/` starts a regular expression.
    Operator,
    /// An operand (word, literal, closing bracket): a `/` divides.
    Operand,
    /// A keyword of [`REGEX_KEYWORDS`]: a `/` starts a regular expression.
    Keyword,
}

/// The tokenizer. See the module docs.
#[derive(Debug, Clone)]
pub(super) struct Lexer<'a> {
    text: &'a str,
    i: usize,
    frames: Vec<Frame>,
    prev: Prev,
}

pub(super) fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | 0x0b | 0x0c)
}

impl<'a> Lexer<'a> {
    pub(super) fn new(text: &'a str) -> Self {
        Self {
            text,
            i: 0,
            frames: Vec::new(),
            prev: Prev::Operator,
        }
    }

    /// Open brackets and template expressions at the current position.
    pub(super) fn depth(&self) -> usize {
        self.frames.len()
    }

    /// Continues at byte `at` (a character boundary at bracket depth 0), as at a statement
    /// start.
    pub(super) fn restart_at(&mut self, at: usize) {
        self.i = at.min(self.text.len());
        self.frames.clear();
        self.prev = Prev::Operator;
    }

    fn bytes(&self) -> &'a [u8] {
        self.text.as_bytes()
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.bytes().get(self.i + offset).copied()
    }

    /// The index of the `\n` ending the line comment that starts at `i`, or the end.
    fn line_comment_end(&self, i: usize) -> usize {
        let b = self.bytes();
        b[i..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(b.len(), |p| i + p)
    }

    /// The index after the `*/` closing the block comment that starts at `i`, or the end.
    fn block_comment_end(&self, i: usize) -> usize {
        let b = self.bytes();
        let mut j = i + 2;
        while j + 1 < b.len() {
            if b[j] == b'*' && b[j + 1] == b'/' {
                return j + 2;
            }
            j += 1;
        }
        b.len()
    }

    /// The index after the string that starts with the quote at `i` (a line break ends an
    /// unterminated string, without being part of it).
    fn string_end(&self, i: usize) -> usize {
        let b = self.bytes();
        let quote = b[i];
        let mut j = i + 1;
        while j < b.len() {
            match b[j] {
                b'\\' => j += 2,
                b'\n' => return j,
                c if c == quote => return j + 1,
                _ => j += 1,
            }
        }
        b.len()
    }

    /// Scans template text from `j` (after a backtick or an expression's `}`): the index after
    /// the closing backtick (`false`) or after a `${` (`true`), or the end.
    fn template_end(&self, mut j: usize) -> (usize, bool) {
        let b = self.bytes();
        while j < b.len() {
            match b[j] {
                b'\\' => j += 2,
                b'`' => return (j + 1, false),
                b'$' if b.get(j + 1) == Some(&b'{') => return (j + 2, true),
                _ => j += 1,
            }
        }
        (b.len(), false)
    }

    /// The index after the regular expression that starts with the `/` at `i` (its flags
    /// included); a line break ends an unterminated one.
    fn regex_end(&self, i: usize) -> usize {
        let b = self.bytes();
        let mut j = i + 1;
        let mut in_class = false;
        while j < b.len() {
            match b[j] {
                b'\\' => j += 2,
                b'\n' => return j,
                b'[' => {
                    in_class = true;
                    j += 1;
                }
                b']' => {
                    in_class = false;
                    j += 1;
                }
                b'/' if !in_class => {
                    j += 1;
                    while j < b.len() && b[j].is_ascii_alphabetic() {
                        j += 1;
                    }
                    return j;
                }
                _ => j += 1,
            }
        }
        b.len()
    }

    /// A template piece ending at `end` (`expr`: it opened a `${`).
    fn template_piece(&mut self, end: usize, expr: bool) -> Tok {
        self.i = end;
        if expr {
            self.frames.push(Frame::Template);
            self.prev = Prev::Operator;
        } else {
            self.prev = Prev::Operand;
        }
        Tok::Template
    }

    fn close(&mut self, frame: Frame) {
        if self.frames.last() == Some(&frame) {
            self.frames.pop();
        }
    }
}

impl Iterator for Lexer<'_> {
    type Item = (Tok, Range<usize>);

    fn next(&mut self) -> Option<Self::Item> {
        let len = self.text.len();
        loop {
            // Escapes may step past the end; clamp once per token.
            self.i = self.i.min(len);
            let c = self.peek(0)?;
            let start = self.i;
            if c == b'\n' {
                self.i += 1;
                return Some((Tok::Newline, start..self.i));
            }
            if is_space(c) {
                self.i += 1;
                continue;
            }
            if c == b'/' && self.peek(1) == Some(b'/') {
                self.i = self.line_comment_end(start);
                continue;
            }
            if c == b'/' && self.peek(1) == Some(b'*') {
                self.i = self.block_comment_end(start);
                continue;
            }
            let tok = match c {
                b'"' | b'\'' => {
                    self.i = self.string_end(start);
                    self.prev = Prev::Operand;
                    Tok::Str
                }
                b'`' => {
                    let (end, expr) = self.template_end(start + 1);
                    self.template_piece(end, expr)
                }
                b'/' if self.prev != Prev::Operand => {
                    self.i = self.regex_end(start);
                    self.prev = Prev::Operand;
                    Tok::Regex
                }
                b'}' if self.frames.last() == Some(&Frame::Template) => {
                    self.frames.pop();
                    let (end, expr) = self.template_end(start + 1);
                    self.template_piece(end, expr)
                }
                c if is_word_byte(c) => {
                    let b = self.bytes();
                    let mut j = start + 1;
                    while j < len && is_word_byte(b[j]) {
                        j += 1;
                    }
                    self.i = j;
                    self.prev = if REGEX_KEYWORDS.contains(&&self.text[start..j]) {
                        Prev::Keyword
                    } else {
                        Prev::Operand
                    };
                    Tok::Word
                }
                _ => {
                    self.i += 1;
                    match c {
                        b'(' => self.frames.push(Frame::Paren),
                        b'[' => self.frames.push(Frame::Bracket),
                        b'{' => self.frames.push(Frame::Brace),
                        b')' => self.close(Frame::Paren),
                        b']' => self.close(Frame::Bracket),
                        b'}' => self.close(Frame::Brace),
                        _ => {}
                    }
                    let pair = self.peek(0);
                    self.prev = match (c, pair) {
                        (b')' | b']' | b'}', _) => Prev::Operand,
                        (b'+', Some(b'+')) | (b'-', Some(b'-')) => {
                            self.i += 1;
                            Prev::Operand
                        }
                        (b'=', Some(b'>')) => {
                            self.i += 1;
                            Prev::Operator
                        }
                        _ => Prev::Operator,
                    };
                    Tok::Punct(c)
                }
            };
            self.i = self.i.min(len);
            return Some((tok, start..self.i));
        }
    }
}

/// The significant tokens of `text` (no line breaks).
pub(super) fn tokens(text: &str) -> Vec<(Tok, Range<usize>)> {
    Lexer::new(text)
        .filter(|(t, _)| *t != Tok::Newline)
        .collect()
}

/// The byte range of a string token's content (without its quotes; an unterminated string
/// has no closing quote), or of a template literal without `${ }`. `None` for other tokens.
pub(super) fn string_content(text: &str, tok: Tok, range: &Range<usize>) -> Option<Range<usize>> {
    let s = &text[range.clone()];
    let quote = match tok {
        Tok::Str => s.as_bytes().first().copied()?,
        Tok::Template if s.starts_with('`') && s.len() >= 2 && s.ends_with('`') => b'`',
        _ => return None,
    };
    let end = if closes(s.as_bytes(), quote) {
        range.end - 1
    } else {
        range.end
    };
    Some(range.start + 1..end.max(range.start + 1))
}

/// Whether the quoted token `s` (opening quote at 0) ends with its closing quote.
fn closes(s: &[u8], quote: u8) -> bool {
    let mut j = 1;
    while j < s.len() {
        match s[j] {
            b'\\' => j += 2,
            c if c == quote => return j == s.len() - 1,
            _ => j += 1,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<(Tok, &str)> {
        Lexer::new(text).map(|(t, r)| (t, &text[r])).collect()
    }

    #[test]
    fn tokens_of_a_chain() {
        assert_eq!(
            kinds("db.c.find({a: 'x;y'}) // c\n"),
            vec![
                (Tok::Word, "db"),
                (Tok::Punct(b'.'), "."),
                (Tok::Word, "c"),
                (Tok::Punct(b'.'), "."),
                (Tok::Word, "find"),
                (Tok::Punct(b'('), "("),
                (Tok::Punct(b'{'), "{"),
                (Tok::Word, "a"),
                (Tok::Punct(b':'), ":"),
                (Tok::Str, "'x;y'"),
                (Tok::Punct(b'}'), "}"),
                (Tok::Punct(b')'), ")"),
                (Tok::Newline, "\n"),
            ]
        );
    }

    #[test]
    fn regex_or_division() {
        assert_eq!(
            kinds("a / b / c"),
            vec![
                (Tok::Word, "a"),
                (Tok::Punct(b'/'), "/"),
                (Tok::Word, "b"),
                (Tok::Punct(b'/'), "/"),
                (Tok::Word, "c"),
            ]
        );
        assert_eq!(
            kinds("x = /a;b[/]/gi"),
            vec![
                (Tok::Word, "x"),
                (Tok::Punct(b'='), "="),
                (Tok::Regex, "/a;b[/]/gi"),
            ]
        );
        assert_eq!(
            kinds("return /x/"),
            vec![(Tok::Word, "return"), (Tok::Regex, "/x/")]
        );
        assert_eq!(kinds("i++ / 2")[1], (Tok::Punct(b'+'), "++"));
        assert_eq!(kinds("i++ / 2")[2], (Tok::Punct(b'/'), "/"));
    }

    #[test]
    fn templates_nest() {
        let text = "`a ${ {b: `c${d}`}.b } e` + 1";
        let toks = kinds(text);
        assert_eq!(toks[0], (Tok::Template, "`a ${"));
        assert_eq!(toks.last(), Some(&(Tok::Word, "1")));
        assert!(toks.contains(&(Tok::Template, "} e`")));
        let mut lx = Lexer::new(text);
        while lx.next().is_some() {}
        assert_eq!(lx.depth(), 0);
    }

    #[test]
    fn string_contents() {
        let text = r#"'ab' "c\"d" `t` "e\\" 'open"#;
        let toks = tokens(text);
        let contents: Vec<&str> = toks
            .iter()
            .filter_map(|(t, r)| string_content(text, *t, r))
            .map(|r| &text[r])
            .collect();
        assert_eq!(contents, vec!["ab", "c\\\"d", "t", "e\\\\", "open"]);
    }
}
