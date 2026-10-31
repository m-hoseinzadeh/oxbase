//! Identifier and literal quoting.

use std::fmt::Write as _;

use oxbase_types::{ObjectKind, ObjectRef, Value};

/// Quotes identifiers and renders literals for one engine. One implementation per engine.
///
/// The provided methods follow standard SQL; engines override what differs.
pub trait Quoting: Send + Sync {
    /// Quotes an identifier unconditionally, escaping embedded quote characters.
    fn quote_ident(&self, ident: &str) -> String;

    /// Renders a string literal, doubling embedded single quotes.
    fn quote_str(&self, s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('\'');
        for c in s.chars() {
            if c == '\'' {
                out.push('\'');
            }
            out.push(c);
        }
        out.push('\'');
        out
    }

    /// Qualified name of `obj`, with each part quoted and joined with `.`.
    ///
    /// - Columns: catalog, schema, owning table (`obj.parent`) and name, as in a fully qualified
    ///   column reference.
    /// - Foreign keys and checks: the bare quoted name. Constraints are named unqualified in DDL
    ///   (`ALTER TABLE t DROP CONSTRAINT c`); the table is qualified separately.
    /// - Everything else, including indexes and triggers (schema-scoped in PostgreSQL): catalog,
    ///   schema and name.
    ///
    /// Engines whose syntax differs override this.
    fn qualify(&self, obj: &ObjectRef) -> String {
        let parts: [Option<&str>; 4] = match obj.kind {
            ObjectKind::ForeignKey | ObjectKind::Check => return self.quote_ident(&obj.name),
            ObjectKind::Column => [
                obj.catalog.as_deref(),
                obj.schema.as_deref(),
                obj.parent.as_deref(),
                Some(obj.name.as_str()),
            ],
            _ => [
                obj.catalog.as_deref(),
                obj.schema.as_deref(),
                None,
                Some(obj.name.as_str()),
            ],
        };
        parts
            .into_iter()
            .flatten()
            .map(|part| self.quote_ident(part))
            .collect::<Vec<_>>()
            .join(".")
    }

    /// Renders a value as a SQL literal.
    ///
    /// Defaults: `NULL`, `TRUE`/`FALSE`, plain numbers, `X'..'` for bytes, and quoted strings
    /// for text, decimals that are not plain numbers, non-finite floats, temporal values (ISO
    /// 8601), UUIDs and JSON.
    ///
    /// The `X'..'` bytes form is standard SQL and right for SQLite and MySQL, but not for every
    /// engine (in PostgreSQL it is a bit string, not `bytea`). Engines whose binary literal
    /// differs must override this ([`PgQuoting`], [`BracketQuoting`]).
    ///
    /// Literals are meant for previews and generated DDL. Prefer bind parameters for user data.
    fn literal(&self, v: &Value) -> String {
        match v {
            Value::Null => "NULL".to_owned(),
            Value::Bool(true) => "TRUE".to_owned(),
            Value::Bool(false) => "FALSE".to_owned(),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) if f.is_finite() => format!("{f:?}"),
            Value::Float(f) => self.quote_str(&f.to_string()),
            Value::Decimal(d) if is_plain_number(d) => d.clone(),
            Value::Bytes(b) => format!("X'{}'", hex(b)),
            Value::TimestampTz(t) => self.quote_str(&t.to_rfc3339()),
            Value::Decimal(_)
            | Value::Text(_)
            | Value::Date(_)
            | Value::Time(_)
            | Value::Timestamp(_)
            | Value::Uuid(_)
            | Value::Json(_) => self.quote_str(&v.to_string()),
        }
    }
}

/// Lowercase hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// `true` for text that is safe to emit unquoted as a numeric literal: optional sign, digits,
/// at most one dot, optional exponent.
fn is_plain_number(s: &str) -> bool {
    let body = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (mantissa, exponent) = match body.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e.strip_prefix(['-', '+']).unwrap_or(e))),
        None => (body, None),
    };
    let digits = mantissa.chars().filter(char::is_ascii_digit).count();
    let dots = mantissa.chars().filter(|c| *c == '.').count();
    digits > 0
        && dots <= 1
        && digits + dots == mantissa.len()
        && exponent.is_none_or(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_digit()))
}

fn wrap(ident: &str, open: char, close: char) -> String {
    let mut out = String::with_capacity(ident.len() + 2);
    out.push(open);
    for c in ident.chars() {
        if c == close {
            out.push(close);
        }
        out.push(c);
    }
    out.push(close);
    out
}

/// Standard SQL quoting: `"ident"` and `X'..'` binary literals (SQLite and standard SQL).
///
/// Not for PostgreSQL, where `X'..'` is a bit string: use [`PgQuoting`].
#[derive(Debug, Clone, Copy, Default)]
pub struct AnsiQuoting;

impl Quoting for AnsiQuoting {
    fn quote_ident(&self, ident: &str) -> String {
        wrap(ident, '"', '"')
    }
}

/// PostgreSQL quoting: like [`AnsiQuoting`], but bytes render as `'\xcafe'::bytea`.
///
/// The `'\x..'` form relies on `standard_conforming_strings = on` (the default since 9.1), where a
/// backslash in a plain string literal is an ordinary character.
#[derive(Debug, Clone, Copy, Default)]
pub struct PgQuoting;

impl Quoting for PgQuoting {
    fn quote_ident(&self, ident: &str) -> String {
        AnsiQuoting.quote_ident(ident)
    }

    fn literal(&self, v: &Value) -> String {
        match v {
            Value::Bytes(b) => format!("'\\x{}'::bytea", hex(b)),
            other => AnsiQuoting.literal(other),
        }
    }
}

/// MySQL / MariaDB quoting: `` `ident` ``, and backslashes escaped in string literals (the
/// default `sql_mode` treats `\` as an escape character).
///
/// Under `sql_mode` `NO_BACKSLASH_ESCAPES` this escaping doubles every backslash in the stored
/// data. Use bind parameters for data, or check the session's `sql_mode` before inlining.
#[derive(Debug, Clone, Copy, Default)]
pub struct BacktickQuoting;

impl Quoting for BacktickQuoting {
    fn quote_ident(&self, ident: &str) -> String {
        wrap(ident, '`', '`')
    }

    fn quote_str(&self, s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('\'');
        for c in s.chars() {
            match c {
                '\'' => out.push_str("''"),
                '\\' => out.push_str("\\\\"),
                '\0' => out.push_str("\\0"),
                c => out.push(c),
            }
        }
        out.push('\'');
        out
    }
}

/// SQL Server quoting: `[ident]`, `N'...'` Unicode strings, `0x..` binary and `1`/`0` booleans.
#[derive(Debug, Clone, Copy, Default)]
pub struct BracketQuoting;

impl Quoting for BracketQuoting {
    fn quote_ident(&self, ident: &str) -> String {
        wrap(ident, '[', ']')
    }

    fn quote_str(&self, s: &str) -> String {
        format!("N{}", AnsiQuoting.quote_str(s))
    }

    fn literal(&self, v: &Value) -> String {
        match v {
            Value::Bool(b) => if *b { "1" } else { "0" }.to_owned(),
            Value::Bytes(b) => format!("0x{}", hex(b)),
            other => {
                // Only string-valued literals start with a quote; make them Unicode (N'...').
                let lit = AnsiQuoting.literal(other);
                if lit.starts_with('\'') {
                    format!("N{lit}")
                } else {
                    lit
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::ObjectKind;
    use pretty_assertions::assert_eq;

    #[test]
    fn identifiers() {
        assert_eq!(AnsiQuoting.quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(BacktickQuoting.quote_ident("a`b"), "`a``b`");
        assert_eq!(BracketQuoting.quote_ident("a]b"), "[a]]b]");
        let t = ObjectRef::table(Some("shop"), Some("public"), "Order Items");
        assert_eq!(
            AnsiQuoting.qualify(&t),
            "\"shop\".\"public\".\"Order Items\""
        );
        let col = t.child(ObjectKind::Column, "id");
        assert_eq!(
            BracketQuoting.qualify(&col),
            "[shop].[public].[Order Items].[id]"
        );
        // Constraints are named without qualification.
        assert_eq!(
            AnsiQuoting.qualify(&t.child(ObjectKind::ForeignKey, "fk")),
            "\"fk\""
        );
        assert_eq!(
            AnsiQuoting.qualify(&t.child(ObjectKind::Check, "ck")),
            "\"ck\""
        );
        // Indexes and triggers are schema-scoped (PostgreSQL).
        assert_eq!(
            PgQuoting.qualify(&t.child(ObjectKind::Index, "ix")),
            "\"shop\".\"public\".\"ix\""
        );
        assert_eq!(
            BacktickQuoting.qualify(&ObjectRef::table(None, None, "t")),
            "`t`"
        );
    }

    #[test]
    fn strings() {
        assert_eq!(AnsiQuoting.quote_str("it's"), "'it''s'");
        assert_eq!(AnsiQuoting.quote_str("a\\b"), "'a\\b'");
        assert_eq!(BacktickQuoting.quote_str("it's a\\b"), "'it''s a\\\\b'");
        assert_eq!(BracketQuoting.quote_str("é'"), "N'é'''");
    }

    #[test]
    fn literals() {
        let q = AnsiQuoting;
        assert_eq!(q.literal(&Value::Null), "NULL");
        assert_eq!(q.literal(&Value::Bool(true)), "TRUE");
        assert_eq!(q.literal(&Value::Int(-4)), "-4");
        assert_eq!(q.literal(&Value::Float(1.0)), "1.0");
        assert_eq!(q.literal(&Value::Float(f64::NAN)), "'NaN'");
        assert_eq!(q.literal(&Value::Decimal("-12.50".into())), "-12.50");
        assert_eq!(q.literal(&Value::Decimal("1e-3".into())), "1e-3");
        // Anything that is not a plain number is quoted, so it cannot inject SQL.
        assert_eq!(
            q.literal(&Value::Decimal("1; DROP TABLE t".into())),
            "'1; DROP TABLE t'"
        );
        assert_eq!(q.literal(&Value::Decimal(".".into())), "'.'");
        assert_eq!(q.literal(&Value::Bytes(vec![0xca, 0xfe])), "X'cafe'");
        assert_eq!(q.literal(&Value::Text("o'k".into())), "'o''k'");
        assert_eq!(
            q.literal(&Value::Json(serde_json_value())),
            "'{\"a\":\"b''c\"}'"
        );

        let pg = PgQuoting;
        assert_eq!(
            pg.literal(&Value::Bytes(vec![0xca, 0xfe])),
            "'\\xcafe'::bytea"
        );
        assert_eq!(pg.literal(&Value::Bool(true)), "TRUE");
        assert_eq!(pg.literal(&Value::Text("o'k".into())), "'o''k'");
        assert_eq!(pg.quote_ident("a\"b"), "\"a\"\"b\"");

        let ms = BracketQuoting;
        assert_eq!(ms.literal(&Value::Bool(false)), "0");
        assert_eq!(ms.literal(&Value::Bytes(vec![1])), "0x01");
        assert_eq!(ms.literal(&Value::Text("x".into())), "N'x'");
        assert_eq!(ms.literal(&Value::Int(3)), "3");
        assert_eq!(ms.literal(&Value::Null), "NULL");
    }

    fn serde_json_value() -> serde_json::Value {
        serde_json::json!({"a": "b'c"})
    }
}
