//! Reads the header of a `CREATE VIEW` module definition (`sys.sql_modules.definition`): the
//! optional column list and the query after `AS` (docs/m4-contracts.md §3.3, View).
//!
//! The scan is a small T-SQL tokenizer: `--` and nested `/* */` comments, `'...'` / `N'...'`
//! strings with `''`, `[...]` identifiers with `]]` and `"..."` identifiers with `""` are
//! skipped as units, so an `AS` inside any of them is never taken for the header's end.

/// A view definition split at its header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewText {
    /// The explicit column list (`CREATE VIEW v (a, b) AS`), unquoted; empty without one.
    pub columns: Vec<String>,
    /// The text after `AS`, comments inside it kept, without surrounding whitespace and the
    /// trailing `;`.
    pub query: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok<'a> {
    Word(&'a str),
    /// A bracketed or double-quoted identifier, with its quotes.
    Quoted(&'a str),
    Str,
    Punct(char),
}

/// Splits `text` into tokens with their end offsets, comments and whitespace dropped.
fn tokens(text: &str) -> Vec<(Tok<'_>, usize)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
        } else if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let mut depth = 0usize;
            while i < bytes.len() {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if b == b'\'' || ((b == b'N' || b == b'n') && bytes.get(i + 1) == Some(&b'\'')) {
            i += if b == b'\'' { 1 } else { 2 };
            i = close(bytes, i, b'\'');
            out.push((Tok::Str, i));
        } else if b == b'[' || b == b'"' {
            let start = i;
            i = close(bytes, i + 1, if b == b'[' { b']' } else { b'"' });
            out.push((Tok::Quoted(&text[start..i]), i));
        } else if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'@' | b'#' | b'$') || b >= 0x80 {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || matches!(bytes[i], b'_' | b'@' | b'#' | b'$')
                    || bytes[i] >= 0x80)
            {
                i += 1;
            }
            out.push((Tok::Word(&text[start..i]), i));
        } else {
            i += 1;
            out.push((Tok::Punct(char::from(b)), i));
        }
    }
    out
}

/// The offset just past the closing `quote` of a quoted run starting at `i` (doubled quotes
/// escape it); the end of the text when it is not closed.
fn close(bytes: &[u8], mut i: usize, quote: u8) -> usize {
    while i < bytes.len() {
        if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// An identifier token without its quotes.
fn unquote(tok: Tok<'_>) -> Option<String> {
    match tok {
        Tok::Word(w) => Some(w.to_owned()),
        Tok::Quoted(q) => {
            let (open, inner) = q.split_at(1);
            let (close, doubled) = if open == "[" {
                ("]", "]]")
            } else {
                ("\"", "\"\"")
            };
            let inner = inner.strip_suffix(close).unwrap_or(inner);
            Some(inner.replace(doubled, close))
        }
        Tok::Str | Tok::Punct(_) => None,
    }
}

/// Splits a `CREATE [OR ALTER] VIEW` definition. `None` when the text has no recognizable
/// header (the caller then keeps the whole text).
pub(crate) fn split_view(definition: &str) -> Option<ViewText> {
    let toks = tokens(definition);
    let view_at = toks
        .iter()
        .position(|(t, _)| matches!(t, Tok::Word(w) if w.eq_ignore_ascii_case("VIEW")))?;
    let mut i = view_at + 1;
    // The name: identifiers separated by dots.
    while matches!(toks.get(i), Some((Tok::Word(_) | Tok::Quoted(_), _)))
        && !matches!(toks.get(i), Some((Tok::Word(w), _)) if w.eq_ignore_ascii_case("AS") || w.eq_ignore_ascii_case("WITH"))
    {
        i += 1;
        if matches!(toks.get(i), Some((Tok::Punct('.'), _))) {
            i += 1;
        } else {
            break;
        }
    }
    let mut columns = Vec::new();
    if matches!(toks.get(i), Some((Tok::Punct('('), _))) {
        i += 1;
        while let Some((tok, _)) = toks.get(i) {
            i += 1;
            match tok {
                Tok::Punct(')') => break,
                Tok::Punct(',') => {}
                other => columns.push(unquote(*other)?),
            }
        }
    }
    // Options (`WITH SCHEMABINDING, VIEW_METADATA`) up to the header's `AS`.
    let mut depth = 0i32;
    while let Some((tok, end)) = toks.get(i) {
        match tok {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => depth -= 1,
            Tok::Word(w) if depth == 0 && w.eq_ignore_ascii_case("AS") => {
                let query = definition[*end..].trim();
                let query = query.strip_suffix(';').unwrap_or(query).trim_end();
                return Some(ViewText {
                    columns,
                    query: query.to_owned(),
                });
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn split(text: &str) -> (Vec<String>, String) {
        let v = split_view(text).unwrap_or_else(|| panic!("no header in {text}"));
        (v.columns, v.query)
    }

    #[test]
    fn plain_view() {
        let (cols, q) =
            split("CREATE VIEW m2.m2_view AS\nSELECT c.id, c.qty, p.code\nFROM m2.m2_child c;\n");
        assert_eq!(cols, Vec::<String>::new());
        assert_eq!(q, "SELECT c.id, c.qty, p.code\nFROM m2.m2_child c");
    }

    #[test]
    fn comments_before_and_inside() {
        let text = "-- the view AS documented\n/* outer /* nested AS */ AS */\ncreate or alter view [m4].[a]]s] (x, [y z]) \
                    with schemabinding, view_metadata\nas -- body comment\nSELECT 1 AS x, 'AS' AS [y z]";
        let (cols, q) = split(text);
        assert_eq!(cols, vec!["x".to_owned(), "y z".to_owned()]);
        assert_eq!(q, "-- body comment\nSELECT 1 AS x, 'AS' AS [y z]");
    }

    #[test]
    fn quoted_names_and_unicode() {
        let (cols, q) = split("CREATE VIEW \"s\".\"vüe\" AS SELECT N'é' AS a");
        assert_eq!(cols, Vec::<String>::new());
        assert_eq!(q, "SELECT N'é' AS a");
    }

    #[test]
    fn no_header() {
        assert_eq!(split_view("SELECT 1"), None);
        assert_eq!(split_view("CREATE VIEW v"), None);
    }
}
