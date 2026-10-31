//! Canonical forms of SQL expressions as MySQL and MariaDB spell them back (M3 comparator).
//!
//! The server rewrites what it stores: MySQL prints a check `qty >= 0` as `` (`qty` >= 0) ``,
//! lowercases function names, adds parentheses around every comparison of an `AND`, writes string
//! literals with a character-set introducer (`_utf8mb4'x'`) and, in some catalog columns, with
//! backslash-escaped quotes (`_utf8mb4\'x\'`); MariaDB reports `current_timestamp()` where MySQL
//! says `CURRENT_TIMESTAMP`. [`canonical`] reduces both spellings to one text so the comparator
//! can tell "the same expression" from "a changed one":
//!
//! - tokens are separated by one space; words are lowercased, backtick-quoted identifiers
//!   unquoted (MySQL column names are case-insensitive);
//! - string literals (`'...'` or `"..."`, doubled quotes or backslash escapes) are decoded and
//!   re-rendered as `'...'`; character-set introducers are dropped;
//! - `!=` is `<>`, `&&` is `and`, `||` is `or`;
//! - redundant grouping parentheses are removed: around the whole expression, doubled ones, and
//!   around an operand of `AND` / `OR` / `,` unless that changes precedence (an `OR` group next to
//!   an `AND`). Parentheses of function calls, `IN` lists and arithmetic groups stay.
//!
//! It is a normalizer for comparison, not a parser: two texts with the same canonical form mean
//! the same to the server in every case the tests cover; precedence inside arithmetic is never
//! touched.

/// One token of a canonical expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Tok {
    /// A decoded string literal.
    Str(String),
    /// A lowercased word: keyword, function or column name, number.
    Word(String),
    /// An operator.
    Sym(String),
    Open,
    Close,
    Comma,
}

/// Words after which `(` opens a group, not a function call or list.
const GROUP_KEYWORDS: [&str; 14] = [
    "and", "or", "xor", "not", "when", "then", "else", "case", "between", "like", "regexp",
    "rlike", "is", "return",
];

/// The canonical text of `expr` (see the module docs).
pub(crate) fn canonical(expr: &str) -> String {
    render(&canonical_tokens(expr))
}

/// The canonical tokens of `expr`.
pub(crate) fn canonical_tokens(expr: &str) -> Vec<Tok> {
    let tokens = tokenize(expr).unwrap_or_else(|| {
        // A backslash outside any string: the whole text is backslash-escaped once
        // (`_utf8mb4\'x\'`), as some catalog columns print it.
        tokenize(&unescape_once(expr)).unwrap_or_default()
    });
    strip_groups(tokens)
}

/// Renders tokens with one space between them.
pub(crate) fn render(tokens: &[Tok]) -> String {
    let mut out = String::new();
    for (pos, tok) in tokens.iter().enumerate() {
        if pos > 0 {
            out.push(' ');
        }
        match tok {
            Tok::Str(s) => {
                out.push('\'');
                out.push_str(&s.replace('\'', "''"));
                out.push('\'');
            }
            Tok::Word(w) | Tok::Sym(w) => out.push_str(w),
            Tok::Open => out.push('('),
            Tok::Close => out.push(')'),
            Tok::Comma => out.push(','),
        }
    }
    out
}

/// `\X` → `X` everywhere (one level of backslash escaping).
fn unescape_once(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Splits `text` into tokens. `None` when a backslash appears outside a string (the text is
/// escaped as a whole; see [`canonical_tokens`]).
fn tokenize(text: &str) -> Option<Vec<Tok>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '\\' => return None,
            '\'' | '"' => {
                let (s, next) = read_string(&chars, i);
                out.push(Tok::Str(s));
                i = next;
            }
            '`' => {
                let mut name = String::new();
                i += 1;
                while i < chars.len() {
                    if chars[i] == '`' {
                        if chars.get(i + 1) == Some(&'`') {
                            name.push('`');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    name.push(chars[i]);
                    i += 1;
                }
                out.push(Tok::Word(name.to_lowercase()));
            }
            '(' => {
                out.push(Tok::Open);
                i += 1;
            }
            ')' => {
                out.push(Tok::Close);
                i += 1;
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            c if is_word_char(c) || c == '.' => {
                let start = i;
                while i < chars.len() && (is_word_char(chars[i]) || chars[i] == '.') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect::<String>().to_lowercase();
                // A character-set introducer (`_utf8mb4'x'`) belongs to the literal.
                let introducer = word.starts_with('_')
                    && word.len() > 1
                    && matches!(chars.get(i), Some('\'' | '"' | '\\'));
                if !introducer {
                    out.push(Tok::Word(word));
                }
            }
            _ => {
                let rest: String = chars[i..chars.len().min(i + 3)].iter().collect();
                let op = [
                    "<=>", ">=", "<=", "<>", "!=", "||", "&&", ":=", "<<", ">>", "->",
                ]
                .into_iter()
                .find(|op| rest.starts_with(op))
                .unwrap_or("");
                let (text, len) = if op.is_empty() {
                    (c.to_string(), 1)
                } else {
                    (op.to_owned(), op.chars().count())
                };
                out.push(match text.as_str() {
                    "!=" => Tok::Sym("<>".into()),
                    "&&" => Tok::Word("and".into()),
                    "||" => Tok::Word("or".into()),
                    _ => Tok::Sym(text),
                });
                i += len;
            }
        }
    }
    Some(out)
}

/// Reads the string literal starting at `chars[start]` (a quote); returns its decoded content and
/// the index after it.
fn read_string(chars: &[char], start: usize) -> (String, usize) {
    let quote = chars[start];
    let mut s = String::new();
    let mut i = start + 1;
    while i < chars.len() {
        let c = chars[i];
        if c == quote {
            if chars.get(i + 1) == Some(&quote) {
                s.push(quote);
                i += 2;
                continue;
            }
            return (s, i + 1);
        }
        if c == '\\' {
            match chars.get(i + 1) {
                Some(&next) => {
                    match next {
                        'n' => s.push('\n'),
                        'r' => s.push('\r'),
                        't' => s.push('\t'),
                        '0' => s.push('\0'),
                        'b' => s.push('\u{8}'),
                        'Z' => s.push('\u{1a}'),
                        // `\%` and `\_` keep their backslash (LIKE patterns).
                        '%' | '_' => {
                            s.push('\\');
                            s.push(next);
                        }
                        other => s.push(other),
                    }
                    i += 2;
                }
                None => {
                    s.push('\\');
                    i += 1;
                }
            }
            continue;
        }
        s.push(c);
        i += 1;
    }
    (s, i)
}

/// Index of the `)` matching the `(` at `open`, if any.
fn matching(tokens: &[Tok], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (pos, tok) in tokens.iter().enumerate().skip(open) {
        match tok {
            Tok::Open => depth += 1,
            Tok::Close => {
                depth -= 1;
                if depth == 0 {
                    return Some(pos);
                }
            }
            _ => {}
        }
    }
    None
}

/// `true` when the `(` at `open` starts a group (not a call or a list).
fn is_group(tokens: &[Tok], open: usize) -> bool {
    match open.checked_sub(1).map(|prev| &tokens[prev]) {
        Some(Tok::Word(w)) => GROUP_KEYWORDS.contains(&w.as_str()),
        Some(Tok::Str(_) | Tok::Close) => false,
        _ => true,
    }
}

/// Precedence of a binary operator (MySQL's table, higher binds tighter); `NOT` is 4.
fn binary_prec(tok: &Tok) -> Option<u8> {
    match tok {
        Tok::Word(w) => match w.as_str() {
            "or" => Some(1),
            "xor" => Some(2),
            "and" => Some(3),
            "not" => Some(4),
            "is" | "like" | "in" | "between" | "regexp" | "rlike" => Some(5),
            "div" | "mod" => Some(10),
            _ => None,
        },
        Tok::Sym(s) => match s.as_str() {
            "=" | "<=>" | ">=" | ">" | "<=" | "<" | "<>" => Some(5),
            "|" => Some(6),
            "&" => Some(7),
            "<<" | ">>" => Some(8),
            "+" | "-" => Some(9),
            "*" | "/" | "%" => Some(10),
            "^" => Some(11),
            _ => Some(UNARY),
        },
        _ => None,
    }
}

/// Precedence of a unary sign or of anything that binds tighter than every binary operator.
const UNARY: u8 = 12;
/// "No operator": an atom.
const ATOM: u8 = u8::MAX;

/// `true` when the token at `pos` is an operator with no left operand (`-x`, `NOT x`).
fn is_prefix(tokens: &[Tok], pos: usize) -> bool {
    match pos.checked_sub(1).map(|p| &tokens[p]) {
        None | Some(Tok::Open | Tok::Comma) => true,
        Some(prev @ (Tok::Sym(_) | Tok::Word(_))) => {
            binary_prec(prev).is_some() || is_word(Some(prev), &SEPARATORS)
        }
        Some(_) => false,
    }
}

/// Words that separate operands like a comma (`CASE ... WHEN ... THEN ... ELSE ... END`).
const SEPARATORS: [&str; 6] = ["case", "when", "then", "else", "end", "return"];

fn is_word(tok: Option<&Tok>, words: &[&str]) -> bool {
    matches!(tok, Some(Tok::Word(w)) if words.contains(&w.as_str()))
}

/// Precedence of the operator on the left of a group starting at `open`; `None` when the group
/// cannot be judged (keep it).
fn left_prec(tokens: &[Tok], open: usize) -> Option<u8> {
    let Some(prev_pos) = open.checked_sub(1) else {
        return Some(0);
    };
    let prev = &tokens[prev_pos];
    match prev {
        Tok::Open | Tok::Comma => Some(0),
        _ if is_word(Some(prev), &SEPARATORS) => Some(0),
        Tok::Sym(s) if (s == "-" || s == "+") && is_prefix(tokens, prev_pos) => Some(UNARY),
        _ => binary_prec(prev),
    }
}

/// Precedence of the operator on the right of a group ending at `close`; `None` to keep it.
fn right_prec(tokens: &[Tok], close: usize) -> Option<u8> {
    match tokens.get(close + 1) {
        None | Some(Tok::Close | Tok::Comma) => Some(0),
        Some(next) if is_word(Some(next), &SEPARATORS) => Some(0),
        Some(Tok::Word(w)) if w == "not" => Some(5), // NOT LIKE / NOT IN / NOT BETWEEN
        Some(next) => binary_prec(next),
    }
}

/// The lowest precedence among the operators directly inside `inner` (not nested in parentheses
/// or a `CASE ... END`); [`ATOM`] when there is none, 0 for a top-level comma.
fn inner_prec(inner: &[Tok]) -> u8 {
    let mut depth = 0usize;
    let mut lowest = ATOM;
    for (pos, tok) in inner.iter().enumerate() {
        match tok {
            Tok::Open => depth += 1,
            Tok::Close => depth = depth.saturating_sub(1),
            Tok::Word(w) if w == "case" => depth += 1,
            Tok::Word(w) if w == "end" => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            Tok::Comma => lowest = 0,
            Tok::Sym(s) if (s == "-" || s == "+") && is_prefix(inner, pos) => {
                lowest = lowest.min(UNARY);
            }
            other => {
                if let Some(p) = binary_prec(other) {
                    lowest = lowest.min(p);
                }
            }
        }
    }
    lowest
}

/// Removes redundant grouping parentheses until none is left (see the module docs): a group goes
/// when every operator directly inside binds tighter than the operators on both sides (equal on
/// the right, which is left-associative; equal on the left only for `AND`, `OR`, `XOR`).
fn strip_groups(mut tokens: Vec<Tok>) -> Vec<Tok> {
    while let Some((open, close)) = redundant_group(&tokens) {
        tokens.remove(close);
        tokens.remove(open);
    }
    tokens
}

/// The first redundant group of `tokens` (see [`strip_groups`]), as `(open, close)` indexes.
fn redundant_group(tokens: &[Tok]) -> Option<(usize, usize)> {
    for (open, tok) in tokens.iter().enumerate() {
        if *tok != Tok::Open || !is_group(tokens, open) {
            continue;
        }
        let close = matching(tokens, open)?;
        if close == open + 1 {
            continue;
        }
        let (Some(left), Some(right)) = (left_prec(tokens, open), right_prec(tokens, close)) else {
            continue;
        };
        let inner = inner_prec(&tokens[open + 1..close]);
        let left_ok = inner > left || (inner == left && (1..=3).contains(&left));
        if left_ok && inner >= right && inner > 0 {
            return Some((open, close));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn same(a: &str, b: &str) {
        assert_eq!(canonical(a), canonical(b), "{a:?} vs {b:?}");
    }

    #[track_caller]
    fn differ(a: &str, b: &str) {
        assert_ne!(canonical(a), canonical(b), "{a:?} vs {b:?}");
    }

    #[test]
    fn server_rewrites_of_checks() {
        // MySQL 8 CHECK_CLAUSE spellings.
        same("qty >= 0", "(`qty` >= 0)");
        same("qty >= 0 AND qty < 10", "((`qty` >= 0) and (`qty` < 10))");
        same("char_length(code) > 0", "(char_length(`code`) > 0)");
        same("CHAR_LENGTH(Code) > 0", "(char_length(`code`) > 0)");
        same("status <> 'x'", "(`status` <> _utf8mb4'x')");
        same("status != 'x'", "(`status` <> _utf8mb4\\'x\\')");
        same(
            "status IN ('a', 'b')",
            "(`status` in (_utf8mb4'a',_utf8mb4'b'))",
        );
        same("NOT (a > 0)", "(not((`a` > 0)))");
        same("a > 0 && b > 0", "((`a` > 0) and (`b` > 0))");
        // MariaDB spellings.
        same("qty >= 0", "`qty` >= 0");
        same("note <> 'it''s'", "`note` <> 'it\\'s'");
        // Precedence is kept.
        same("(a OR b) AND c", "((`a` or `b`) and `c`)");
        differ("(a OR b) AND c", "a OR b AND c");
        same("a OR (b AND c)", "a or b and c");
        differ("(a + b) * 2 > 0", "a + b * 2 > 0");
        same("(a + b) * 2 > 0", "(((`a` + `b`) * 2) > 0)");
        differ("qty >= 0", "qty > 0");
        differ("note = 'A'", "note = 'a'");
    }

    #[test]
    fn strings_and_identifiers() {
        assert_eq!(canonical("'it''s'"), "'it''s'");
        assert_eq!(canonical("'it\\'s'"), "'it''s'");
        assert_eq!(canonical("'a\\\\b'"), "'a\\b'");
        assert_eq!(canonical("\"x\""), "'x'");
        assert_eq!(canonical("`a``b` = 1"), "a`b = 1");
        assert_eq!(canonical("`ünï` = 'ç'"), "ünï = 'ç'");
        assert_eq!(canonical("f(a, (b))"), "f ( a , b )");
        assert_eq!(canonical(""), "");
        // Unbalanced input does not loop or panic.
        assert_eq!(canonical("((a"), "( ( a");
        assert_eq!(canonical("a)"), "a )");
    }
}
