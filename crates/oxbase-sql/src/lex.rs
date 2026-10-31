//! The hand-written lexer behind [`crate::split`] and [`crate::redact_secrets`].
//!
//! It works on bytes. Every byte that matters to SQL syntax is ASCII, and the bytes of a
//! multi-byte UTF-8 character are never ASCII, so token boundaries always fall on character
//! boundaries: a token ends after an ASCII byte or at the end of the text. Bytes `>= 0x80` are
//! word bytes, so a non-ASCII character is always inside a word, a string, an identifier or a
//! comment.
//!
//! [`token_at`] always returns a token of at least one byte and never looks backwards, so a loop
//! over it is linear in the text length (a dollar-quoted body is searched once with a linear
//! substring search).

use crate::SqlDialect;

/// What a token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenKind {
    /// ASCII whitespace (space, tab, line feed, carriage return, vertical tab, form feed).
    Space,
    /// `-- ...` (or MySQL `# ...`) up to the end of the line, without trailing whitespace.
    LineComment,
    /// `/* ... */`, nested where the dialect nests them. Unterminated: to the end of the text.
    BlockComment,
    /// MySQL `/*! ... */`, `/*M! ... */` and `/*+ ... */`: statement content, not a comment.
    ExecComment,
    /// A string literal (any quote form of the dialect). `inner_start..inner_end` is its
    /// content without the quotes, `E` prefix or dollar tags.
    Str {
        inner_start: usize,
        inner_end: usize,
    },
    /// A quoted identifier; `quote` is its opening byte (`"`, `` ` `` or `[`).
    Ident { quote: u8 },
    /// A keyword, identifier, number or parameter (`$1`).
    Word,
    /// Anything else: one ASCII byte, or `:=`.
    Punct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

/// The lexing rules of a dialect (see the crate docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rules {
    pub dialect: SqlDialect,
    /// A backslash escapes the next byte in `'...'` (and in `"..."` strings): MySQL.
    pub backslash_escapes: bool,
    /// `"..."` is a string, not an identifier: MySQL.
    double_quote_string: bool,
    /// `` `...` `` identifiers: MySQL, SQLite.
    backtick_ident: bool,
    /// `[...]` identifiers: SQLite, T-SQL.
    bracket_ident: bool,
    /// `]]` inside `[...]` is an escaped `]`: T-SQL.
    bracket_doubling: bool,
    /// `$$ ... $$`, `$tag$ ... $tag$`, `E'...'`: PostgreSQL.
    postgres_strings: bool,
    /// Block comments nest: PostgreSQL, T-SQL.
    nested_comments: bool,
    /// MySQL comments: `#`, `--` only before whitespace, `/*! */` and `/*+ */` are content.
    mysql_comments: bool,
    /// `@` and `#` inside words: T-SQL (`@var`, `#temp`).
    tsql_word_bytes: bool,
    /// Redaction only: lex the opener of an executable comment (`/*!50001`) as a comment and
    /// its body as ordinary tokens, so keywords inside it are seen.
    pub open_exec_comments: bool,
}

impl Rules {
    pub(crate) const fn new(dialect: SqlDialect) -> Self {
        let pg = matches!(dialect, SqlDialect::Postgres);
        let mysql = matches!(dialect, SqlDialect::MySql { .. });
        let sqlite = matches!(dialect, SqlDialect::Sqlite);
        let tsql = matches!(dialect, SqlDialect::TSql);
        Self {
            dialect,
            backslash_escapes: mysql,
            double_quote_string: mysql,
            backtick_ident: mysql || sqlite,
            bracket_ident: sqlite || tsql,
            bracket_doubling: tsql,
            postgres_strings: pg,
            nested_comments: pg || tsql,
            mysql_comments: mysql,
            tsql_word_bytes: tsql,
            open_exec_comments: false,
        }
    }

    /// Whether `b` continues a word, or starts one (a PostgreSQL `$` is lexed before words, so
    /// there a word never starts with `$`).
    pub(crate) const fn is_word_byte(self, b: u8) -> bool {
        b.is_ascii_alphanumeric()
            || b == b'_'
            || b == b'$'
            || b >= 0x80
            || (self.tsql_word_bytes && (b == b'@' || b == b'#'))
    }
}

/// ASCII whitespace as the splitter counts it (includes vertical tab, unlike
/// [`u8::is_ascii_whitespace`]).
pub(crate) const fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// The token starting at byte `start` (`start < text.len()`, on a token boundary).
///
/// `word_stop` (may be empty) ends a word early where it begins: the MySQL client delimiter
/// when it starts with a word byte (`END$$`).
pub(crate) fn token_at(text: &str, start: usize, rules: Rules, word_stop: &[u8]) -> Token {
    let bytes = text.as_bytes();
    let b = bytes[start];
    let next = bytes.get(start + 1).copied();
    let (kind, end) = match b {
        _ if is_space(b) => (TokenKind::Space, scan_while(bytes, start + 1, is_space)),
        b'-' if next == Some(b'-') && dash_comment(bytes, start, rules) => {
            (TokenKind::LineComment, line_comment_end(bytes, start))
        }
        b'#' if rules.mysql_comments => (TokenKind::LineComment, line_comment_end(bytes, start)),
        b'/' if next == Some(b'*') => block_comment(bytes, start, rules),
        b'\'' => string(bytes, start, b'\'', rules.backslash_escapes),
        b'"' if rules.double_quote_string => string(bytes, start, b'"', rules.backslash_escapes),
        b'"' => quoted_ident(bytes, start, b'"', true),
        b'`' if rules.backtick_ident => quoted_ident(bytes, start, b'`', true),
        b'[' if rules.bracket_ident => quoted_ident(bytes, start, b']', rules.bracket_doubling),
        b'$' if rules.postgres_strings => dollar(text, start),
        _ if rules.is_word_byte(b) => word(bytes, start, rules, word_stop),
        b':' if next == Some(b'=') => (TokenKind::Punct, start + 2),
        _ => (TokenKind::Punct, start + 1),
    };
    Token { kind, start, end }
}

fn scan_while(bytes: &[u8], mut i: usize, keep: impl Fn(u8) -> bool) -> usize {
    while i < bytes.len() && keep(bytes[i]) {
        i += 1;
    }
    i
}

/// `--` at `start`: always a comment, except in MySQL where it must be followed by whitespace,
/// a control character or the end of the text (`--1` is minus minus one).
fn dash_comment(bytes: &[u8], start: usize, rules: Rules) -> bool {
    !rules.mysql_comments || bytes.get(start + 2).is_none_or(|&c| c <= b' ' || c == 0x7f)
}

/// End of a line comment starting at `start`: before the next `\n` (or the end of the text),
/// without trailing whitespace (`\r` of a CRLF, trailing spaces), keeping at least one byte.
fn line_comment_end(bytes: &[u8], start: usize) -> usize {
    let mut end = bytes[start..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(bytes.len(), |p| start + p);
    while end > start + 1 && is_space(bytes[end - 1]) {
        end -= 1;
    }
    end
}

fn block_comment(bytes: &[u8], start: usize, rules: Rules) -> (TokenKind, usize) {
    let marker = bytes.get(start + 2).copied();
    let mariadb_exec = marker == Some(b'M') && bytes.get(start + 3) == Some(&b'!');
    let exec = rules.mysql_comments && (matches!(marker, Some(b'!' | b'+')) || mariadb_exec);
    if exec && rules.open_exec_comments {
        // Only the opener and its version number; the body is lexed as ordinary tokens.
        let after_marker = start + if mariadb_exec { 4 } else { 3 };
        return (
            TokenKind::BlockComment,
            scan_while(bytes, after_marker, |c| c.is_ascii_digit()),
        );
    }
    let nested = rules.nested_comments;
    let mut depth = 1_usize;
    let mut i = start + 2;
    let end = loop {
        if i >= bytes.len() {
            break bytes.len();
        }
        if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            i += 2;
            depth -= 1;
            if depth == 0 {
                break i;
            }
        } else if nested && bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            depth += 1;
        } else {
            i += 1;
        }
    };
    let kind = if exec {
        TokenKind::ExecComment
    } else {
        TokenKind::BlockComment
    };
    (kind, end)
}

/// A string whose opening `quote` is at `open`; the token starts at `open` too. A doubled
/// quote is an escaped quote; with `backslash`, a backslash escapes the next byte.
fn string(bytes: &[u8], open: usize, quote: u8, backslash: bool) -> (TokenKind, usize) {
    let (inner_end, end) = quoted_end(bytes, open, quote, backslash);
    (
        TokenKind::Str {
            inner_start: open + 1,
            inner_end,
        },
        end,
    )
}

/// `(inner_end, end)` of a quoted run opened at `open`; unterminated: both are the text end.
fn quoted_end(bytes: &[u8], open: usize, quote: u8, backslash: bool) -> (usize, usize) {
    let mut i = open + 1;
    while i < bytes.len() {
        let c = bytes[i];
        if backslash && c == b'\\' {
            i += 2;
        } else if c == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return (i, i + 1);
            }
        } else {
            i += 1;
        }
    }
    (bytes.len(), bytes.len())
}

/// A quoted identifier opened at `start`, closed by `close` (`]]` / `""` / ``` `` ``` escapes
/// when `doubling`).
fn quoted_ident(bytes: &[u8], start: usize, close: u8, doubling: bool) -> (TokenKind, usize) {
    let kind = TokenKind::Ident {
        quote: bytes[start],
    };
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == close {
            if doubling && bytes.get(i + 1) == Some(&close) {
                i += 2;
                continue;
            }
            return (kind, i + 1);
        }
        i += 1;
    }
    (kind, bytes.len())
}

/// PostgreSQL `$`: a dollar quote `$$ ... $$` / `$tag$ ... $tag$`, a parameter `$1`, or a
/// lone `$`.
fn dollar(text: &str, start: usize) -> (TokenKind, usize) {
    let bytes = text.as_bytes();
    let tag_start = |c: u8| c.is_ascii_alphabetic() || c == b'_' || c >= 0x80;
    let tag_byte = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    let mut i = start + 1;
    if i < bytes.len() && tag_start(bytes[i]) {
        i = scan_while(bytes, i + 1, tag_byte);
    }
    if bytes.get(i) == Some(&b'$') {
        let open_end = i + 1;
        // Both ends are next to an ASCII `$`, so they are char boundaries.
        let tag = &text[start..open_end];
        let (inner_end, end) = text[open_end..]
            .find(tag)
            .map_or((bytes.len(), bytes.len()), |at| {
                (open_end + at, open_end + at + tag.len())
            });
        return (
            TokenKind::Str {
                inner_start: open_end,
                inner_end,
            },
            end,
        );
    }
    if bytes.get(start + 1).is_some_and(u8::is_ascii_digit) {
        return (
            TokenKind::Word,
            scan_while(bytes, start + 1, |c| c.is_ascii_digit()),
        );
    }
    (TokenKind::Punct, start + 1)
}

fn word(bytes: &[u8], start: usize, rules: Rules, word_stop: &[u8]) -> (TokenKind, usize) {
    let mut end = start + 1;
    while end < bytes.len() && rules.is_word_byte(bytes[end]) {
        if !word_stop.is_empty() && bytes[end..].starts_with(word_stop) {
            break;
        }
        end += 1;
    }
    // PostgreSQL `E'...'`: the prefix belongs to the string, and backslashes escape.
    if rules.postgres_strings
        && end == start + 1
        && matches!(bytes[start], b'E' | b'e')
        && bytes.get(end) == Some(&b'\'')
    {
        let (inner_end, string_end) = quoted_end(bytes, end, b'\'', true);
        return (
            TokenKind::Str {
                inner_start: end + 1,
                inner_end,
            },
            string_end,
        );
    }
    (TokenKind::Word, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(dialect: SqlDialect, text: &str) -> Vec<(TokenKind, &str)> {
        let rules = Rules::new(dialect);
        let mut out = Vec::new();
        let mut i = 0;
        while i < text.len() {
            let tok = token_at(text, i, rules, &[]);
            assert!(tok.end > tok.start, "empty token at {i} in {text:?}");
            out.push((tok.kind, &text[tok.start..tok.end]));
            i = tok.end;
        }
        out
    }

    #[test]
    fn postgres_dollar_quotes_and_parameters() {
        let toks = kinds(SqlDialect::Postgres, "$$a;b$$ $1 $t$ $$ $t$ a$b $ $x");
        let texts: Vec<_> = toks
            .iter()
            .filter(|(k, _)| *k != TokenKind::Space)
            .map(|(_, t)| *t)
            .collect();
        assert_eq!(
            texts,
            ["$$a;b$$", "$1", "$t$ $$ $t$", "a$b", "$", "$", "x"],
            "{toks:?}"
        );
        assert_eq!(
            toks[0].0,
            TokenKind::Str {
                inner_start: 2,
                inner_end: 5
            }
        );
    }

    #[test]
    fn postgres_e_strings_and_nested_comments() {
        let toks = kinds(SqlDialect::Postgres, r"E'a\'b' e'' /* x /* y */ z */x");
        assert_eq!(toks[0].1, r"E'a\'b'");
        assert_eq!(
            toks[0].0,
            TokenKind::Str {
                inner_start: 2,
                inner_end: 6
            }
        );
        assert_eq!(toks[2].1, "e''");
        assert_eq!(toks[4], (TokenKind::BlockComment, "/* x /* y */ z */"));
        assert_eq!(toks[5], (TokenKind::Word, "x"));
    }

    #[test]
    fn mysql_comments_and_strings() {
        let toks = kinds(SqlDialect::MySql { mariadb: false }, "--1 -- c  \n# d\r\n");
        assert_eq!(toks[0], (TokenKind::Punct, "-"));
        assert_eq!(toks[1], (TokenKind::Punct, "-"));
        assert_eq!(toks[2], (TokenKind::Word, "1"));
        assert_eq!(toks[4], (TokenKind::LineComment, "-- c"));
        assert_eq!(toks[6], (TokenKind::LineComment, "# d"));
        let toks = kinds(
            SqlDialect::MySql { mariadb: true },
            r#"'a\'b' "c\"d""" `e``f` /*!40101 x */ /*M!100100 y */ /*+ z */ /* /* */"#,
        );
        let non_space: Vec<_> = toks
            .iter()
            .filter(|(k, _)| *k != TokenKind::Space)
            .collect();
        assert_eq!(non_space[0].1, r"'a\'b'");
        assert_eq!(non_space[1].1, r#""c\"d""""#);
        assert_eq!(non_space[2].1, "`e``f`");
        assert_eq!(non_space[3].0, TokenKind::ExecComment);
        assert_eq!(non_space[4].0, TokenKind::ExecComment);
        assert_eq!(non_space[5].0, TokenKind::ExecComment);
        assert_eq!(*non_space[6], (TokenKind::BlockComment, "/* /* */"));
    }

    #[test]
    fn redaction_opens_exec_comments() {
        let mut rules = Rules::new(SqlDialect::MySql { mariadb: false });
        rules.open_exec_comments = true;
        let tok = token_at("/*!80001 x */", 0, rules, &[]);
        assert_eq!((tok.kind, tok.end), (TokenKind::BlockComment, 8));
    }

    #[test]
    fn brackets_and_unterminated_runs() {
        let toks = kinds(SqlDialect::TSql, "[a]]b] [c");
        assert_eq!(toks[0], (TokenKind::Ident { quote: b'[' }, "[a]]b]"));
        assert_eq!(toks[2], (TokenKind::Ident { quote: b'[' }, "[c"));
        let toks = kinds(SqlDialect::Sqlite, "[a]]b]");
        assert_eq!(toks[0], (TokenKind::Ident { quote: b'[' }, "[a]"));
        let toks = kinds(SqlDialect::Postgres, "'abc");
        assert_eq!(
            toks[0].0,
            TokenKind::Str {
                inner_start: 1,
                inner_end: 4
            }
        );
        let toks = kinds(SqlDialect::Postgres, "$q$ body");
        assert_eq!(toks.len(), 1);
    }

    #[test]
    fn word_stop_ends_words() {
        let rules = Rules::new(SqlDialect::MySql { mariadb: false });
        let tok = token_at("END$$", 0, rules, b"$$");
        assert_eq!(tok.end, 3);
        let tok = token_at("END$$", 0, rules, &[]);
        assert_eq!(tok.end, 5);
    }
}
