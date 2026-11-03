//! Keyset ranges for chunked, key-ordered reads (M8 data sync and transfer,
//! `docs/m8-contracts.md` §6.2), the optional per-engine [`ChunkHasher`] role, and
//! [`inline_params`], which renders a bound statement as previewable SQL text.
//!
//! # Key ranges
//!
//! Data compare reads both tables in chunks ordered by a key (`ORDER BY k1, k2, ...` ascending,
//! never NULL: a primary key or a NOT NULL unique key). A chunk is the half-open key range
//! `(after, upto]`: [`write_key_after`] writes `key > after` and [`write_key_at_most`] writes
//! `key <= upto`, both lexicographic over the key columns and spelled with plain comparisons
//! (`k1 > ? OR (k1 = ? AND k2 > ?)`), because SQL Server has no row-value comparison. Every value
//! is a bound parameter. The same range sent to both sides selects the same keys when both
//! columns sort the same way (same type and collation; the data-sync plan checks it).
//!
//! # Chunk hashes (optional role)
//!
//! An engine may register a [`ChunkHasher`]: one statement that returns the row count and a
//! digest of a key range, so equal ranges are skipped without reading their rows. Both sides of a
//! data compare are the same engine, so a digest only has to be stable within the engine. An
//! engine without one is compared row by row (always correct, more traffic).

use oxbase_types::{BoundStatement, ColumnDef, Error, ObjectRef, Result, Value};

use crate::Quoting;
use crate::sql::SqlWriter;

/// Appends `(key) > (after)` lexicographically: `(k1 > ? OR (k1 = ? AND k2 > ?) ...)`.
/// `InvalidInput` when the counts differ, the key is empty or a value is NULL.
pub fn write_key_after(w: &mut SqlWriter<'_>, columns: &[String], after: &[Value]) -> Result<()> {
    write_bound(w, columns, after, " > ", false)
}

/// Appends `(key) <= (upto)` lexicographically: `(k1 < ? OR (k1 = ? AND k2 < ?) OR (k1 = ? AND
/// k2 = ?))`. Same errors as [`write_key_after`].
pub fn write_key_at_most(w: &mut SqlWriter<'_>, columns: &[String], upto: &[Value]) -> Result<()> {
    write_bound(w, columns, upto, " < ", true)
}

fn write_bound(
    w: &mut SqlWriter<'_>,
    columns: &[String],
    values: &[Value],
    op: &str,
    or_equal: bool,
) -> Result<()> {
    if columns.is_empty() || columns.len() != values.len() {
        return Err(Error::invalid_input(format!(
            "{} key values for {} key columns",
            values.len(),
            columns.len()
        )));
    }
    if let Some(pos) = values.iter().position(Value::is_null) {
        return Err(Error::invalid_input(format!(
            "key column '{}' is NULL",
            columns[pos]
        )));
    }
    w.push("(");
    let terms = columns.len() + usize::from(or_equal);
    for term in 0..terms {
        if term > 0 {
            w.push(" OR ");
        }
        // Term `term`: the first `term` columns equal, then column `term` compared with `op`
        // (the extra last term of `or_equal` is "all columns equal").
        let equal = term.min(columns.len());
        let wrap = equal > 0 || (term < columns.len() && columns.len() > 1);
        if wrap {
            w.push("(");
        }
        for (k, (column, value)) in columns.iter().zip(values).take(equal).enumerate() {
            if k > 0 {
                w.push(" AND ");
            }
            w.ident(column).push(" = ").param(value.clone());
        }
        if term < columns.len() {
            if equal > 0 {
                w.push(" AND ");
            }
            w.ident(&columns[term]).push(op).param(values[term].clone());
        }
        if wrap {
            w.push(")");
        }
    }
    w.push(")");
    Ok(())
}

/// A key range `(after, upto]`; `None` bounds are open.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyRange<'a> {
    pub after: Option<&'a [Value]>,
    pub upto: Option<&'a [Value]>,
}

impl KeyRange<'_> {
    /// Appends the range's predicate (`TRUE` spelled `1 = 1` when both bounds are open).
    pub fn write(&self, w: &mut SqlWriter<'_>, columns: &[String]) -> Result<()> {
        match (self.after, self.upto) {
            (None, None) => {
                w.push("1 = 1");
            }
            (Some(after), None) => write_key_after(w, columns, after)?,
            (None, Some(upto)) => write_key_at_most(w, columns, upto)?,
            (Some(after), Some(upto)) => {
                write_key_after(w, columns, after)?;
                w.push(" AND ");
                write_key_at_most(w, columns, upto)?;
            }
        }
        Ok(())
    }
}

/// One statement that returns exactly one row `(row_count, digest)` for the rows of a key range
/// (module docs, *Chunk hashes*). Optional per engine, registered with
/// [`DialectRegistry::register_chunk_hasher`](crate::DialectRegistry::register_chunk_hasher).
///
/// Rules: `row_count` is an integer, `digest` text (NULL for an empty range); the digest covers
/// every value of `columns` of every row in key order, NULL distinguishable from every value
/// and from the empty string; values are bound, names quoted.
pub trait ChunkHasher: Send + Sync {
    /// The hash statement for the rows of `table` whose `key` is in `range`.
    fn chunk_hash(
        &self,
        table: &ObjectRef,
        key: &[String],
        columns: &[ColumnDef],
        range: KeyRange<'_>,
    ) -> Result<BoundStatement>;
}

/// `statement` as SQL text for a preview or a saved script: every placeholder replaced by the
/// engine's dump literal ([`Quoting::dump_literal`]), in the placeholder style of `q`
/// ([`Quoting::placeholder`]: `?`, `$n` or `@Pn`). Placeholders inside quoted strings, quoted
/// identifiers and comments are left alone. `InvalidInput` when the statement uses a parameter
/// it does not bind, or binds more values than it uses (positional `?` style).
///
/// The text is for people (and for running the script by hand); the apply path always executes
/// the bound statement itself.
pub fn inline_params(quoting: &dyn Quoting, statement: &BoundStatement) -> Result<String> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Style {
        Question,
        Dollar,
        AtP,
    }
    let style = {
        let first = quoting.placeholder(1);
        if first.starts_with('$') {
            Style::Dollar
        } else if first.starts_with("@P") {
            Style::AtP
        } else {
            Style::Question
        }
    };
    let rules = quoting.lex_rules();
    let chars: Vec<char> = statement.sql.chars().collect();
    let params = &statement.params;
    let literal = |index: usize| -> Result<String> {
        params
            .get(index)
            .map(|value| quoting.dump_literal(value).sql)
            .ok_or_else(|| {
                Error::invalid_input(format!(
                    "the statement uses parameter {} but binds {}",
                    index + 1,
                    params.len()
                ))
            })
    };
    let digits_at = |start: usize| -> (usize, Option<usize>) {
        let mut end = start;
        while end < chars.len() && chars[end].is_ascii_digit() {
            end += 1;
        }
        let number: String = chars[start..end].iter().collect();
        (end, number.parse::<usize>().ok())
    };
    let ident_before =
        |at: usize| at > 0 && (chars[at - 1].is_alphanumeric() || chars[at - 1] == '_');

    let mut out = String::with_capacity(statement.sql.len() + params.len() * 8);
    let mut next_positional = 0_usize;
    let mut pos = 0;
    while pos < chars.len() {
        let ch = chars[pos];
        let next = chars.get(pos + 1).copied();
        let e_string = rules.e_strings
            && pos > 0
            && matches!(chars[pos - 1], 'e' | 'E')
            && (pos < 2 || !(chars[pos - 2].is_alphanumeric() || chars[pos - 2] == '_'));
        let skip_to = match ch {
            '\'' => Some(skip_quoted(
                &chars,
                pos,
                '\'',
                rules.backslash_escapes || e_string,
            )),
            '"' => Some(skip_quoted(&chars, pos, '"', rules.backslash_escapes)),
            '`' => Some(skip_quoted(&chars, pos, '`', false)),
            '[' if rules.bracket_identifiers => Some(skip_quoted(&chars, pos, ']', false)),
            '-' if next == Some('-') => Some(skip_until(&chars, pos, '\n')),
            '/' if next == Some('*') => Some(skip_block_comment(&chars, pos)),
            '#' if rules.hash_comments => Some(skip_until(&chars, pos, '\n')),
            '$' if rules.dollar_quotes
                && !ident_before(pos)
                && next.is_some_and(|after| !after.is_ascii_digit()) =>
            {
                skip_dollar_quoted(&chars, pos)
            }
            _ => None,
        };
        if let Some(end) = skip_to {
            out.extend(&chars[pos..end]);
            pos = end;
            continue;
        }
        match (style, ch) {
            (Style::Question, '?') => {
                out.push_str(&literal(next_positional)?);
                next_positional += 1;
                pos += 1;
            }
            (Style::Dollar, '$')
                if next.is_some_and(|after| after.is_ascii_digit()) && !ident_before(pos) =>
            {
                let (end, number) = digits_at(pos + 1);
                let number = number.filter(|&number| number > 0).ok_or_else(|| {
                    Error::invalid_input("the statement has an invalid parameter number")
                })?;
                out.push_str(&literal(number - 1)?);
                pos = end;
            }
            (Style::AtP, '@')
                if next == Some('P')
                    && chars.get(pos + 2).is_some_and(char::is_ascii_digit)
                    && !ident_before(pos) =>
            {
                let (end, number) = digits_at(pos + 2);
                let number = number.filter(|&number| number > 0).ok_or_else(|| {
                    Error::invalid_input("the statement has an invalid parameter number")
                })?;
                out.push_str(&literal(number - 1)?);
                pos = end;
            }
            _ => {
                out.push(ch);
                pos += 1;
            }
        }
    }
    if style == Style::Question && next_positional != params.len() {
        return Err(Error::invalid_input(format!(
            "the statement uses {next_positional} parameters but binds {}",
            params.len()
        )));
    }
    Ok(out)
}

/// Index just past the quoted run that starts at `start` and ends with `quote`; a doubled
/// `quote` and (with `backslash`) backslash escapes stay inside. An unterminated run ends at
/// the end of the text.
fn skip_quoted(chars: &[char], start: usize, quote: char, backslash: bool) -> usize {
    let mut j = start + 1;
    while j < chars.len() {
        let c = chars[j];
        if backslash && c == '\\' {
            j += 2;
            continue;
        }
        if c == quote {
            if chars.get(j + 1) == Some(&quote) {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    chars.len()
}

/// Index just past the first `end` at or after `from` (the end of the text when there is none).
fn skip_until(chars: &[char], from: usize, end: char) -> usize {
    chars[from.min(chars.len())..]
        .iter()
        .position(|&c| c == end)
        .map_or(chars.len(), |p| from + p + 1)
}

/// Index just past the PostgreSQL dollar-quoted string (`$tag$ ... $tag$`) that starts at
/// `start`, or `None` when the `$` does not open one.
fn skip_dollar_quoted(chars: &[char], start: usize) -> Option<usize> {
    let mut j = start + 1;
    while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
        j += 1;
    }
    if chars.get(j) != Some(&'$') {
        return None;
    }
    let tag = &chars[start..=j];
    let mut k = j + 1;
    while k + tag.len() <= chars.len() {
        if &chars[k..k + tag.len()] == tag {
            return Some(k + tag.len());
        }
        k += 1;
    }
    Some(chars.len())
}

/// Index just past the `*/` closing the block comment at `start`.
fn skip_block_comment(chars: &[char], start: usize) -> usize {
    let mut j = start + 2;
    while j + 1 < chars.len() {
        if chars[j] == '*' && chars[j + 1] == '/' {
            return j + 2;
        }
        j += 1;
    }
    chars.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AnsiQuoting, BacktickQuoting, BracketQuoting, PgQuoting};
    use pretty_assertions::assert_eq;

    fn key(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn key_bounds_are_lexicographic_and_bound() {
        let mut w = SqlWriter::new(&PgQuoting);
        write_key_after(&mut w, &key(&["id"]), &[Value::Int(5)]).unwrap();
        assert_eq!(w.sql(), "(\"id\" > $1)");

        let mut w = SqlWriter::new(&BacktickQuoting);
        write_key_after(&mut w, &key(&["a", "b"]), &[Value::Int(1), "x".into()]).unwrap();
        assert_eq!(w.sql(), "((`a` > ?) OR (`a` = ? AND `b` > ?))");
        let st = w.finish();
        assert_eq!(st.params, [Value::Int(1), Value::Int(1), "x".into()]);

        let mut w = SqlWriter::new(&BracketQuoting);
        write_key_at_most(&mut w, &key(&["a", "b"]), &[Value::Int(1), Value::Int(2)]).unwrap();
        assert_eq!(
            w.sql(),
            "(([a] < @P1) OR ([a] = @P2 AND [b] < @P3) OR ([a] = @P4 AND [b] = @P5))"
        );

        let mut w = SqlWriter::new(&AnsiQuoting);
        write_key_at_most(&mut w, &key(&["id"]), &[Value::Int(9)]).unwrap();
        assert_eq!(w.sql(), "(\"id\" < ? OR (\"id\" = ?))");

        let mut w = SqlWriter::new(&AnsiQuoting);
        assert!(write_key_after(&mut w, &key(&["a"]), &[]).is_err());
        assert!(write_key_after(&mut w, &[], &[]).is_err());
        assert!(write_key_at_most(&mut w, &key(&["a"]), &[Value::Null]).is_err());
    }

    #[test]
    fn ranges_combine_both_bounds() {
        let lo = [Value::Int(10)];
        let hi = [Value::Int(20)];
        let mut w = SqlWriter::new(&PgQuoting);
        KeyRange {
            after: Some(&lo),
            upto: Some(&hi),
        }
        .write(&mut w, &key(&["id"]))
        .unwrap();
        assert_eq!(w.sql(), "(\"id\" > $1) AND (\"id\" < $2 OR (\"id\" = $3))");
        let mut w = SqlWriter::new(&PgQuoting);
        KeyRange::default().write(&mut w, &key(&["id"])).unwrap();
        assert_eq!(w.sql(), "1 = 1");
    }

    #[test]
    fn inline_params_in_every_placeholder_style() {
        let st = BoundStatement::new(
            "UPDATE \"s\".\"t?$1\" SET \"v\" = $2 WHERE \"id\" = $1 AND note <> '$1?'",
            vec![Value::Int(7), "it's".into()],
        );
        assert_eq!(
            inline_params(&PgQuoting, &st).unwrap(),
            "UPDATE \"s\".\"t?$1\" SET \"v\" = 'it''s' WHERE \"id\" = 7 AND note <> '$1?'"
        );

        let st = BoundStatement::new(
            "INSERT INTO `t` (`a?`, b) VALUES (?, ?) -- done?",
            vec![Value::Null, Value::Bool(true)],
        );
        let text = inline_params(&BacktickQuoting, &st).unwrap();
        assert!(
            text.starts_with("INSERT INTO `t` (`a?`, b) VALUES (NULL, "),
            "{text}"
        );
        assert!(text.ends_with(") -- done?"), "{text}");

        let st = BoundStatement::new(
            "DELETE FROM [s].[t@P1] WHERE [id] = @P1 AND x = @P10",
            (1..=10).map(Value::Int).collect(),
        );
        assert_eq!(
            inline_params(&BracketQuoting, &st).unwrap(),
            "DELETE FROM [s].[t@P1] WHERE [id] = 1 AND x = 10"
        );

        // Missing and surplus parameters.
        let st = BoundStatement::new("SELECT $2", vec![Value::Int(1)]);
        assert!(inline_params(&PgQuoting, &st).is_err());
        let st = BoundStatement::new("SELECT ?", vec![Value::Int(1), Value::Int(2)]);
        assert!(inline_params(&AnsiQuoting, &st).is_err());
        let st = BoundStatement::new("SELECT $x$ $1 $x$, $1", vec![Value::Int(3)]);
        assert_eq!(
            inline_params(&PgQuoting, &st).unwrap(),
            "SELECT $x$ $1 $x$, 3"
        );
        let st = BoundStatement::new("SELECT 1 /* ? */", vec![]);
        assert_eq!(
            inline_params(&AnsiQuoting, &st).unwrap(),
            "SELECT 1 /* ? */"
        );
    }
}
