//! Counts the statements in a script: just enough PostgreSQL lexing to find the semicolons that
//! end statements.
//!
//! The count only chooses how a script is sent (see `conn.rs`); the server still parses it, so
//! a wrong count never changes what runs. Known gaps: `standard_conforming_strings = off` (a
//! backslash then escapes a quote in plain `'...'` strings) and SQL-standard function bodies
//! (`BEGIN ATOMIC ... END`), whose inner semicolons are counted as statement ends. Both only
//! make a single statement look like several, which loses column types but runs correctly.

/// The number of non-empty statements in `sql` (comments and whitespace alone do not count).
pub(crate) fn count_statements(sql: &str) -> usize {
    let b = sql.as_bytes();
    let mut i = 0;
    let mut count = 0;
    let mut has_content = false;
    let mut depth = 0usize;
    while i < b.len() {
        let c = b[i];
        match c {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = skip_line_comment(b, i);
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = skip_block_comment(b, i);
                continue;
            }
            b'\'' => {
                has_content = true;
                let escapes =
                    i > 0 && matches!(b[i - 1], b'E' | b'e') && (i < 2 || !is_ident_byte(b[i - 2]));
                i = skip_quoted(b, i, b'\'', escapes);
                continue;
            }
            b'"' => {
                has_content = true;
                i = skip_quoted(b, i, b'"', false);
                continue;
            }
            b'$' => {
                has_content = true;
                if (i == 0 || !is_ident_byte(b[i - 1]))
                    && let Some(end) = skip_dollar_quoted(b, i)
                {
                    i = end;
                    continue;
                }
            }
            b'(' => {
                has_content = true;
                depth += 1;
            }
            b')' => {
                has_content = true;
                depth = depth.saturating_sub(1);
            }
            b';' if depth == 0 => {
                if has_content {
                    count += 1;
                }
                has_content = false;
            }
            c if c.is_ascii_whitespace() => {}
            _ => has_content = true,
        }
        i += 1;
    }
    if has_content {
        count += 1;
    }
    count
}

/// Letters, digits, `_`, `$` and non-ASCII bytes can continue an identifier.
fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

fn skip_line_comment(b: &[u8], start: usize) -> usize {
    b[start..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |p| start + p + 1)
}

/// Skips a `/* ... */` comment; PostgreSQL block comments nest.
fn skip_block_comment(b: &[u8], start: usize) -> usize {
    let mut i = start + 2;
    let mut depth = 1usize;
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

/// Skips a quoted string or identifier starting at `start` (the opening quote). A doubled quote
/// is an escaped quote; with `backslash_escapes` (`E'...'`) a backslash escapes the next byte.
fn skip_quoted(b: &[u8], start: usize, quote: u8, backslash_escapes: bool) -> usize {
    let mut i = start + 1;
    while i < b.len() {
        let c = b[i];
        if backslash_escapes && c == b'\\' {
            i += 2;
        } else if c == quote {
            if b.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

/// If a dollar quote (`$$` or `$tag$`) starts at `start`, returns the index after its closing
/// delimiter (or the end of input when unterminated). `None` when `$` starts something else,
/// such as a parameter `$1`.
fn skip_dollar_quoted(b: &[u8], start: usize) -> Option<usize> {
    let mut j = start + 1;
    if let Some(&first) = b.get(j) {
        if first != b'$' {
            if !(first.is_ascii_alphabetic() || first == b'_' || first >= 0x80) {
                return None;
            }
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
                j += 1;
            }
            if b.get(j) != Some(&b'$') {
                return None;
            }
        }
    } else {
        return None;
    }
    let tag = &b[start..=j];
    let body = j + 1;
    Some(
        b[body..]
            .windows(tag.len())
            .position(|w| w == tag)
            .map_or(b.len(), |p| body + p + tag.len()),
    )
}

#[cfg(test)]
mod tests {
    use super::count_statements;

    #[test]
    fn counts_plain_statements() {
        assert_eq!(count_statements(""), 0);
        assert_eq!(count_statements("  \n\t "), 0);
        assert_eq!(count_statements(";;  ;"), 0);
        assert_eq!(count_statements("-- just a comment\n/* and another */"), 0);
        assert_eq!(count_statements("SELECT 1"), 1);
        assert_eq!(count_statements("SELECT 1;"), 1);
        assert_eq!(count_statements("SELECT 1; -- trailing comment"), 1);
        assert_eq!(count_statements("SELECT 1; SELECT 2"), 2);
        assert_eq!(
            count_statements("CREATE TABLE t(a int); INSERT INTO t VALUES (1);"),
            2
        );
    }

    #[test]
    fn ignores_semicolons_in_quotes_and_comments() {
        assert_eq!(count_statements("SELECT ';' AS x"), 1);
        assert_eq!(count_statements("SELECT 'it''s; fine'"), 1);
        assert_eq!(count_statements(r"SELECT E'a\'; b'"), 1);
        assert_eq!(count_statements(r#"SELECT 1 AS "a;b""#), 1);
        assert_eq!(count_statements("SELECT 1 -- ; not here\n"), 1);
        assert_eq!(count_statements("SELECT /* ; /* nested ; */ ; */ 1"), 1);
        assert_eq!(
            count_statements("DO $$ BEGIN RAISE NOTICE 'x'; END $$; SELECT 2"),
            2
        );
        assert_eq!(
            count_statements("CREATE FUNCTION f() RETURNS int AS $fn$ SELECT 1; $fn$ LANGUAGE sql"),
            1
        );
        assert_eq!(count_statements("SELECT $1; SELECT $2::int"), 2);
        assert_eq!(count_statements("SELECT a$b FROM t"), 1);
    }

    #[test]
    fn unterminated_input_is_one_statement() {
        assert_eq!(count_statements("SELECT 'abc; def"), 1);
        assert_eq!(count_statements("SELECT $$ abc; def"), 1);
        assert_eq!(count_statements("SELECT 1 /* ; "), 1);
    }

    #[test]
    fn parentheses_keep_rule_actions_together() {
        assert_eq!(
            count_statements("CREATE RULE r AS ON INSERT TO t DO ALSO (SELECT 1; SELECT 2)"),
            1
        );
    }
}
