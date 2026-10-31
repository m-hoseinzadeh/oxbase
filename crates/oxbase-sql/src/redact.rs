//! [`crate::redact_secrets`]: masks password literals before SQL text is stored in history.
//!
//! One pass over the tokens of [`crate::lex`] (comments are skipped, so keywords inside them
//! are ignored; MySQL executable comments are opened so the keywords inside them count). A
//! small state machine arms on a secret keyword and replaces the next string literal; every
//! literal that is not replaced is scanned for `password=` / `pwd=` connection-string values.
//!
//! Beyond the crate docs, also masked (best effort): MySQL `SET PASSWORD FOR user = '...'`,
//! the old password of `ALTER USER ... IDENTIFIED BY '...' REPLACE '...'`, SQLCipher
//! `PRAGMA rekey = '...'`, and SQLite double-quoted keys (`PRAGMA key = "x'...'"`).

use std::borrow::Cow;

use crate::lex::{self, Rules, TokenKind};
use crate::{REDACTED_LITERAL, SqlDialect};

/// What replaces a connection-string password value.
const REDACTED_VALUE: &str = "***";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Off,
    /// The next string literal is a secret. `paren`: a `(` was skipped since arming;
    /// `password`: armed by `PASSWORD` itself (so `FOR user` may follow).
    Armed {
        paren: bool,
        password: bool,
    },
    /// After `WITH`: the next word or literal is an authentication plugin name.
    Plugin {
        paren: bool,
    },
    /// MySQL `SET PASSWORD FOR user`: skip the user name up to `=`.
    ForUser,
    /// A secret was just masked (`REPLACE '<old>'` may follow).
    AfterSecret,
}

/// Words the state machine reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Word {
    /// Arms; `password` is true for `PASSWORD` itself.
    Secret {
        password: bool,
    },
    /// `BY`, `AS`: may sit between the keyword and the literal.
    Filler,
    With,
    For,
    Replace,
    Other,
}

impl Word {
    fn of(word: &[u8]) -> Self {
        let is = |k: &str| word.eq_ignore_ascii_case(k.as_bytes());
        if is("PASSWORD") {
            Self::Secret { password: true }
        } else if is("IDENTIFIED")
            || is("MASTER_PASSWORD")
            || is("SOURCE_PASSWORD")
            || is("SECRET")
            || is("KEY")
            || is("REKEY")
        {
            Self::Secret { password: false }
        } else if is("BY") || is("AS") {
            Self::Filler
        } else if is("WITH") {
            Self::With
        } else if is("FOR") {
            Self::For
        } else if is("REPLACE") {
            Self::Replace
        } else {
            Self::Other
        }
    }
}

pub(crate) fn redact(dialect: SqlDialect, sql: &str) -> Cow<'_, str> {
    let mut rules = Rules::new(dialect);
    rules.open_exec_comments = true;
    let bytes = sql.as_bytes();
    // Replacements as (start, end, text), in order and not overlapping.
    let mut edits: Vec<(usize, usize, &'static str)> = Vec::new();
    let mut state = State::Off;
    let mut i = 0;
    while i < bytes.len() {
        let tok = lex::token_at(sql, i, rules, &[]);
        i = tok.end;
        state = match tok.kind {
            TokenKind::Space | TokenKind::LineComment | TokenKind::BlockComment => continue,
            TokenKind::Str {
                inner_start,
                inner_end,
            } => {
                if let State::Armed { .. } = state {
                    edits.push((tok.start, tok.end, REDACTED_LITERAL));
                    State::AfterSecret
                } else {
                    connection_string_values(bytes, inner_start, inner_end, &mut edits);
                    match state {
                        State::Plugin { paren } => State::Armed {
                            paren,
                            password: false,
                        },
                        State::ForUser => State::ForUser,
                        _ => State::Off,
                    }
                }
            }
            TokenKind::Ident { quote } => match state {
                // SQLCipher keys are often written `PRAGMA key = "x'...'"`.
                State::Armed { paren: false, .. }
                    if dialect == SqlDialect::Sqlite && quote == b'"' =>
                {
                    edits.push((tok.start, tok.end, REDACTED_LITERAL));
                    State::AfterSecret
                }
                State::Plugin { paren } => State::Armed {
                    paren,
                    password: false,
                },
                State::ForUser => State::ForUser,
                _ => State::Off,
            },
            TokenKind::Word => {
                let word = &bytes[tok.start..tok.end];
                match (state, Word::of(word)) {
                    (_, Word::Secret { password }) => State::Armed {
                        paren: false,
                        password,
                    },
                    (State::Armed { paren, .. }, Word::Filler) | (State::Plugin { paren }, _) => {
                        State::Armed {
                            paren,
                            password: false,
                        }
                    }
                    (State::Armed { paren, .. }, Word::With) => State::Plugin { paren },
                    (State::Armed { password: true, .. }, Word::For) | (State::ForUser, _) => {
                        State::ForUser
                    }
                    (State::Armed { .. }, _) if is_introducer(word, bytes.get(tok.end)) => state,
                    (State::AfterSecret, Word::Replace) => State::Armed {
                        paren: false,
                        password: false,
                    },
                    _ => State::Off,
                }
            }
            TokenKind::Punct => match (state, bytes[tok.start]) {
                (State::Armed { password, .. }, b'(') => State::Armed {
                    paren: true,
                    password,
                },
                (State::Armed { .. }, b'=') => state,
                (State::ForUser, b'=') => State::Armed {
                    paren: false,
                    password: false,
                },
                (State::ForUser, c) if c != b';' => State::ForUser,
                _ => State::Off,
            },
            TokenKind::ExecComment => State::Off,
        };
    }
    if edits.is_empty() {
        return Cow::Borrowed(sql);
    }
    let mut out = String::with_capacity(sql.len());
    let mut last = 0;
    for (start, end, replacement) in edits {
        // Every edit boundary is next to an ASCII byte or at a token boundary.
        out.push_str(sql.get(last..start).unwrap_or_default());
        out.push_str(replacement);
        last = end;
    }
    out.push_str(sql.get(last..).unwrap_or_default());
    Cow::Owned(out)
}

/// Rule 2: inside a literal's content `start..end`, masks the value after `password=` or
/// `pwd=` (case-insensitive, not inside a longer word, spaces allowed around `=`) up to the
/// next whitespace, `;`, `&` or the end of the content. A value starting with quotes runs to
/// the closing quote run.
fn connection_string_values(
    bytes: &[u8],
    start: usize,
    end: usize,
    edits: &mut Vec<(usize, usize, &'static str)>,
) {
    let end = end.min(bytes.len());
    let mut k = start;
    while k < end {
        if bytes[k] | 0x20 != b'p' {
            k += 1;
            continue;
        }
        let rest = &bytes[k..end];
        let key_len = if starts_with_ignore_case(rest, b"password") {
            8
        } else if starts_with_ignore_case(rest, b"pwd") {
            3
        } else {
            0
        };
        let in_word = k > start && is_key_byte(bytes[k - 1]);
        if key_len == 0 || in_word {
            k += 1;
            continue;
        }
        let mut v = skip_blanks(bytes, k + key_len, end);
        if v >= end || bytes[v] != b'=' {
            k += key_len;
            continue;
        }
        v = skip_blanks(bytes, v + 1, end);
        let stop = value_end(bytes, v, end);
        if stop > v {
            edits.push((v, stop, REDACTED_VALUE));
        }
        k = stop.max(v);
    }
}

fn starts_with_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .get(..needle.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(needle))
}

/// A literal prefix written as a separate word: a MySQL charset introducer (`_utf8mb4'...'`)
/// or a one-letter prefix directly before the quote (`N'...'`, `X'...'`, `B'...'`).
fn is_introducer(word: &[u8], next: Option<&u8>) -> bool {
    word.first() == Some(&b'_')
        || (matches!(word, [b'N' | b'n' | b'X' | b'x' | b'B' | b'b']) && next == Some(&b'\''))
}

const fn is_key_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn skip_blanks(bytes: &[u8], mut i: usize, end: usize) -> usize {
    while i < end && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    i
}

fn value_end(bytes: &[u8], v: usize, end: usize) -> usize {
    let Some(&first) = bytes.get(v).filter(|_| v < end) else {
        return v;
    };
    let mut j = v;
    if first == b'\'' || first == b'"' {
        while j < end && bytes[j] == first {
            j += 1;
        }
        while j < end && bytes[j] != first {
            j += 1;
        }
        while j < end && bytes[j] == first {
            j += 1;
        }
        return j;
    }
    while j < end && !matches!(bytes[j], b';' | b'&') && !lex::is_space(bytes[j]) {
        j += 1;
    }
    j
}
