//! Identifier and literal quoting.

use std::fmt::Write as _;

use oxbase_types::{ColumnDef, ObjectKind, ObjectRef, Value};

use crate::dump::{DumpLiteral, DumpSession, IdentityInsert, push_datetime, push_time};

/// Lexical rules the expression check ([`crate::sql::check_expression`]) needs to find where
/// strings and quoted identifiers end on an engine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LexRules {
    /// A backslash escapes the next character inside `'...'` and `"..."` (MySQL/MariaDB with the
    /// default `sql_mode`).
    pub backslash_escapes: bool,
    /// `$tag$ ... $tag$` strings (PostgreSQL).
    pub dollar_quotes: bool,
    /// `E'...'` strings with backslash escapes (PostgreSQL).
    pub e_strings: bool,
    /// `#` starts a comment (MySQL/MariaDB).
    pub hash_comments: bool,
    /// `[...]` is a quoted identifier, with `]]` standing for one `]` (SQL Server; SQLite also
    /// accepts it). Without this rule a `'` inside brackets would look like the start of a
    /// string and hide what follows it (M4).
    pub bracket_identifiers: bool,
}

/// Quotes identifiers and renders literals for one engine, plus the small lexical facts that
/// generated SQL depends on (parameter placeholders, text casts, `NULLS FIRST` support, LIKE
/// specials, `LIMIT`). One implementation per engine.
///
/// The provided methods follow standard SQL; engines override what differs. M3 added
/// [`Quoting::placeholder`], [`Quoting::text_cast`], [`Quoting::nulls_ordering`],
/// [`Quoting::like_specials`], [`Quoting::limit_clause`] and [`Quoting::lex_rules`], all with
/// defaults. M4 added [`Quoting::top_clause`] (SQL Server `SELECT TOP (n)`). M5 added the SQL
/// dump rules [`Quoting::dump_session`], [`Quoting::dump_literal`] and
/// [`Quoting::dump_identity_insert`] (types in [`crate::dump`]).
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
        qualify_parts(self, obj)
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

    /// The placeholder of the `n`-th bound parameter (1-based): `?` by default (MySQL, MariaDB,
    /// SQLite); [`PgQuoting`] `$n`; [`BracketQuoting`] `@Pn`.
    fn placeholder(&self, n: usize) -> String {
        let _ = n;
        "?".to_owned()
    }

    /// `expr` converted to text, so a pattern operator can match a non-text column:
    /// `CAST(expr AS TEXT)` by default (PostgreSQL, SQLite); MySQL/MariaDB `CAST(expr AS CHAR)`.
    fn text_cast(&self, expr: &str) -> String {
        format!("CAST({expr} AS TEXT)")
    }

    /// `true` when `ORDER BY` accepts `NULLS FIRST` / `NULLS LAST` (PostgreSQL, SQLite 3.30+).
    /// Engines without it get an emulation (`CASE WHEN col IS NULL ...` before the key).
    fn nulls_ordering(&self) -> bool {
        true
    }

    /// Characters with a special meaning in a `LIKE` pattern: `%` and `_`, plus `[` on SQL
    /// Server. Literal matches escape them (and the escape character `!`).
    fn like_specials(&self) -> &'static str {
        "%_"
    }

    /// The clause that limits a `SELECT` to `n` rows, appended at its end: `LIMIT n`. Not used
    /// when [`Quoting::top_clause`] returns a clause.
    fn limit_clause(&self, n: u64) -> String {
        format!("LIMIT {n}")
    }

    /// The clause that limits a `SELECT` to `n` rows when it goes right after `SELECT` instead
    /// of at the end (M4): `None` by default; [`BracketQuoting`] `TOP (n)`. When this is `Some`,
    /// [`crate::sql::select_table_data`] writes `SELECT <top> * FROM ...` and no
    /// [`Quoting::limit_clause`].
    fn top_clause(&self, n: u64) -> Option<String> {
        let _ = n;
        None
    }

    /// Where strings, quoted identifiers and comments start and end (for
    /// [`crate::sql::check_expression`]).
    fn lex_rules(&self) -> LexRules {
        LexRules::default()
    }

    /// Statements a SQL dump of literal rows (M5) writes before everything else and after its
    /// last statement, so the session reads the literals as this dialect writes them. `zoned`:
    /// the dump writes timestamps with a time zone. Default: none.
    fn dump_session(&self, zoned: bool) -> DumpSession {
        let _ = zoned;
        DumpSession::default()
    }

    /// `v` as a SQL dump literal (M5): [`Quoting::literal`] by default. Engines override the
    /// forms their server does not read back exactly (date-times on MySQL / MariaDB and SQL
    /// Server, decimals in SQLite `TEXT` columns).
    fn dump_literal(&self, v: &Value) -> DumpLiteral {
        DumpLiteral::exact(self.literal(v))
    }

    /// What a dump's `INSERT`s into `table` (already qualified with this quoting) need when
    /// they write the values of `columns` (the written columns), some of which may be identity
    /// or auto-increment columns. Default: nothing.
    fn dump_identity_insert(&self, table: &str, columns: &[&ColumnDef]) -> IdentityInsert {
        let _ = (table, columns);
        IdentityInsert::default()
    }
}

/// The default [`Quoting::qualify`]: the parts of `obj` that are set, each quoted with `q`,
/// joined with `.` (see the trait method for which parts a kind uses).
fn qualify_parts<Q: Quoting + ?Sized>(q: &Q, obj: &ObjectRef) -> String {
    let parts: [Option<&str>; 4] = match obj.kind {
        ObjectKind::ForeignKey | ObjectKind::Check => return q.quote_ident(&obj.name),
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
        .map(|part| q.quote_ident(part))
        .collect::<Vec<_>>()
        .join(".")
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
/// Not for PostgreSQL, where `X'..'` is a bit string: use [`PgQuoting`]. Its
/// [`LexRules::bracket_identifiers`] is on, because SQLite reads `[...]` as an identifier (M4).
#[derive(Debug, Clone, Copy, Default)]
pub struct AnsiQuoting;

impl Quoting for AnsiQuoting {
    fn quote_ident(&self, ident: &str) -> String {
        wrap(ident, '"', '"')
    }

    fn lex_rules(&self) -> LexRules {
        LexRules {
            bracket_identifiers: true,
            ..LexRules::default()
        }
    }

    /// Like [`Quoting::literal`], but decimals are quoted strings: SQLite reads an unquoted
    /// number as a double before the column's affinity applies, so a `TEXT`-affinity decimal
    /// column (the SQLite `DdlGenerator`'s `DECIMAL_TEXT`) would lose the digits past about 15.
    /// Standard SQL converts the string to a numeric column's type.
    fn dump_literal(&self, v: &Value) -> DumpLiteral {
        match v {
            Value::Decimal(d) => DumpLiteral::exact(self.quote_str(d)),
            other => DumpLiteral::exact(self.literal(other)),
        }
    }
}

/// PostgreSQL quoting: like [`AnsiQuoting`], but bytes render as `'\xcafe'::bytea`.
///
/// The `'\x..'` form and the plain string literals rely on `standard_conforming_strings = on`
/// (the default since 9.1), where a backslash in a plain string literal is an ordinary
/// character; SQL dumps set it first ([`Quoting::dump_session`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct PgQuoting;

impl Quoting for PgQuoting {
    fn quote_ident(&self, ident: &str) -> String {
        AnsiQuoting.quote_ident(ident)
    }

    /// Like the default, but without the catalog: a PostgreSQL session reads exactly one
    /// database (`DatabaseScope::Session`), and naming another one is an error, so the catalog
    /// is never part of a name. Services run statements on a session of `obj.catalog`.
    fn qualify(&self, obj: &ObjectRef) -> String {
        let mut local = obj.clone();
        local.catalog = None;
        AnsiQuoting.qualify(&local)
    }

    fn placeholder(&self, n: usize) -> String {
        format!("${n}")
    }

    fn lex_rules(&self) -> LexRules {
        LexRules {
            dollar_quotes: true,
            e_strings: true,
            ..LexRules::default()
        }
    }

    fn literal(&self, v: &Value) -> String {
        match v {
            Value::Bytes(b) => format!("'\\x{}'::bytea", hex(b)),
            other => AnsiQuoting.literal(other),
        }
    }

    /// `SET standard_conforming_strings = on` first: the literals are standard strings (a
    /// backslash is an ordinary character, `'\x..'` bytes), and a session where the setting
    /// is off (`ALTER DATABASE / ROLE ... SET`, `PGOPTIONS`, an old server configuration) would
    /// read a backslash as an escape, so text ending in `\` could end the string early. Not
    /// restored: `on` is the server default since 9.1.
    fn dump_session(&self, zoned: bool) -> DumpSession {
        let _ = zoned;
        DumpSession {
            prologue: vec!["SET standard_conforming_strings = on".to_owned()],
            epilogue: Vec::new(),
        }
    }

    /// `OVERRIDING SYSTEM VALUE` when a written column is a `GENERATED ALWAYS` identity (the
    /// server refuses explicit values otherwise).
    fn dump_identity_insert(&self, table: &str, columns: &[&ColumnDef]) -> IdentityInsert {
        let _ = table;
        let always = columns
            .iter()
            .any(|c| c.identity.as_ref().is_some_and(|i| i.always));
        IdentityInsert {
            overriding: always.then(|| "OVERRIDING SYSTEM VALUE".to_owned()),
            ..IdentityInsert::default()
        }
    }
}

/// MySQL / MariaDB quoting: `` `ident` ``, and backslashes escaped in string literals (the
/// default `sql_mode` treats `\` as an escape character).
///
/// Under `sql_mode` `NO_BACKSLASH_ESCAPES` this escaping doubles every backslash in the stored
/// data. Use bind parameters for data, or check the session's `sql_mode` before inlining; SQL
/// dumps turn the mode off for their statements ([`Quoting::dump_session`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct BacktickQuoting;

impl Quoting for BacktickQuoting {
    fn quote_ident(&self, ident: &str) -> String {
        wrap(ident, '`', '`')
    }

    fn text_cast(&self, expr: &str) -> String {
        format!("CAST({expr} AS CHAR)")
    }

    fn nulls_ordering(&self) -> bool {
        false
    }

    fn lex_rules(&self) -> LexRules {
        LexRules {
            backslash_escapes: true,
            hash_comments: true,
            ..LexRules::default()
        }
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

    /// Saved and restored around the dump: `sql_mode` without `NO_BACKSLASH_ESCAPES` (the
    /// literals escape backslashes and NULs), `SET NAMES utf8mb4` (the file is UTF-8) and,
    /// with `zoned`, `time_zone = '+00:00'` (timestamps with a zone are written as UTC
    /// wall-clock times, which a `TIMESTAMP` column reads in the session's zone).
    fn dump_session(&self, zoned: bool) -> DumpSession {
        let mut prologue = vec![
            "SET @oxbase_sql_mode = @@session.sql_mode".to_owned(),
            "SET sql_mode = TRIM(BOTH ',' FROM REPLACE(CONCAT(',', @@session.sql_mode, ','), \
             ',NO_BACKSLASH_ESCAPES,', ','))"
                .to_owned(),
            "SET @oxbase_character_set_client = @@session.character_set_client".to_owned(),
            "SET @oxbase_character_set_results = @@session.character_set_results".to_owned(),
            "SET @oxbase_collation_connection = @@session.collation_connection".to_owned(),
            "SET NAMES utf8mb4".to_owned(),
        ];
        let mut epilogue = Vec::new();
        if zoned {
            prologue.push("SET @oxbase_time_zone = @@session.time_zone".to_owned());
            prologue.push("SET time_zone = '+00:00'".to_owned());
            epilogue.push("SET time_zone = @oxbase_time_zone".to_owned());
        }
        epilogue.extend([
            "SET character_set_client = @oxbase_character_set_client".to_owned(),
            "SET character_set_results = @oxbase_character_set_results".to_owned(),
            "SET collation_connection = @oxbase_collation_connection".to_owned(),
            "SET sql_mode = @oxbase_sql_mode".to_owned(),
        ]);
        DumpSession { prologue, epilogue }
    }

    /// Date-times without an offset (MariaDB never reads one, MySQL not before 8.0.19), at
    /// most 6 fractional digits: timestamps with a zone as their UTC wall-clock time (the dump
    /// sets `time_zone = '+00:00'`, see [`Quoting::dump_session`]).
    fn dump_literal(&self, v: &Value) -> DumpLiteral {
        const DIGITS: u32 = 6;
        let mut text = String::new();
        let cut = match v {
            Value::Timestamp(t) => push_datetime(t, ' ', DIGITS, &mut text),
            Value::TimestampTz(t) => push_datetime(&t.naive_utc(), ' ', DIGITS, &mut text),
            Value::Time(t) => push_time(*t, DIGITS, &mut text),
            other => return DumpLiteral::exact(self.literal(other)),
        };
        DumpLiteral {
            sql: self.quote_str(&text),
            cut_to: cut.then_some(DIGITS),
        }
    }
}

/// SQL Server quoting: `[ident]` (`]]` escapes `]`), `N'...'` Unicode strings, `0x..` binary,
/// `1`/`0` booleans, `@Pn` placeholders, `SELECT TOP (n)` (M4).
///
/// Names are qualified without the database (`[schema].[name]`): a SQL Server session reads
/// one database (`DatabaseScope::Session`, as on PostgreSQL), several statements (`CREATE
/// VIEW`, `CREATE TRIGGER`, `CREATE FUNCTION`, ...) refuse a database prefix, and services run
/// statements on a session of `obj.catalog`.
#[derive(Debug, Clone, Copy, Default)]
pub struct BracketQuoting;

impl Quoting for BracketQuoting {
    fn quote_ident(&self, ident: &str) -> String {
        wrap(ident, '[', ']')
    }

    fn qualify(&self, obj: &ObjectRef) -> String {
        let mut local = obj.clone();
        local.catalog = None;
        qualify_parts(self, &local)
    }

    fn placeholder(&self, n: usize) -> String {
        format!("@P{n}")
    }

    fn text_cast(&self, expr: &str) -> String {
        format!("CAST({expr} AS nvarchar(max))")
    }

    fn nulls_ordering(&self) -> bool {
        false
    }

    fn like_specials(&self) -> &'static str {
        "%_["
    }

    /// `OFFSET 0 ROWS FETCH NEXT n ROWS ONLY`, which T-SQL accepts only after an `ORDER BY`;
    /// statements without one use [`Quoting::top_clause`].
    fn limit_clause(&self, n: u64) -> String {
        format!("OFFSET 0 ROWS FETCH NEXT {n} ROWS ONLY")
    }

    fn top_clause(&self, n: u64) -> Option<String> {
        Some(format!("TOP ({n})"))
    }

    fn lex_rules(&self) -> LexRules {
        LexRules {
            bracket_identifiers: true,
            ..LexRules::default()
        }
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

    /// Date-times with at most 7 fractional digits. A string converted to `datetime` may have
    /// at most 3 (the driver reads `datetime` ticks as `…00.003333`) and `yyyy-mm-dd` follows
    /// `DATEFORMAT` there, so timestamps without a zone are
    /// `CAST(N'yyyy-mm-ddThh:mm:ss.fffffff' AS datetime2(7))`, which converts to every
    /// date-time column type with rounding; timestamps with a zone keep their offset.
    fn dump_literal(&self, v: &Value) -> DumpLiteral {
        const DIGITS: u32 = 7;
        let mut text = String::new();
        let (cut, cast) = match v {
            Value::Timestamp(t) => (push_datetime(t, 'T', DIGITS, &mut text), true),
            Value::TimestampTz(t) => {
                let cut = push_datetime(&t.naive_local(), 'T', DIGITS, &mut text);
                text.push_str(&t.offset().to_string());
                (cut, false)
            }
            Value::Time(t) => (push_time(*t, DIGITS, &mut text), false),
            other => return DumpLiteral::exact(self.literal(other)),
        };
        let quoted = self.quote_str(&text);
        DumpLiteral {
            sql: if cast {
                format!("CAST({quoted} AS datetime2(7))")
            } else {
                quoted
            },
            cut_to: cut.then_some(DIGITS),
        }
    }

    /// `SET IDENTITY_INSERT <table> ON` / `OFF` around the inserts when a written column is an
    /// identity (or auto-increment) column.
    fn dump_identity_insert(&self, table: &str, columns: &[&ColumnDef]) -> IdentityInsert {
        if columns
            .iter()
            .any(|c| c.identity.is_some() || c.auto_increment)
        {
            IdentityInsert {
                overriding: None,
                enable: Some(format!("SET IDENTITY_INSERT {table} ON")),
                disable: Some(format!("SET IDENTITY_INSERT {table} OFF")),
            }
        } else {
            IdentityInsert::default()
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
            AnsiQuoting.qualify(&col),
            "\"shop\".\"public\".\"Order Items\".\"id\""
        );
        // SQL Server names never carry the database (one database per session, M4).
        assert_eq!(BracketQuoting.qualify(&col), "[public].[Order Items].[id]");
        assert_eq!(BracketQuoting.qualify(&t), "[public].[Order Items]");
        assert_eq!(
            BracketQuoting.qualify(&t.child(ObjectKind::Index, "i]x")),
            "[public].[i]]x]"
        );
        assert_eq!(
            BracketQuoting.qualify(&t.child(ObjectKind::Check, "ck")),
            "[ck]"
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
        // Indexes and triggers are schema-scoped (PostgreSQL), and PostgreSQL names never carry
        // the catalog (one database per session).
        assert_eq!(
            PgQuoting.qualify(&t.child(ObjectKind::Index, "ix")),
            "\"public\".\"ix\""
        );
        assert_eq!(PgQuoting.qualify(&t), "\"public\".\"Order Items\"");
        assert_eq!(
            AnsiQuoting.qualify(&t.child(ObjectKind::Index, "ix")),
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

    #[test]
    fn m3_syntax_hooks() {
        assert_eq!(AnsiQuoting.placeholder(3), "?");
        assert_eq!(BacktickQuoting.placeholder(3), "?");
        assert_eq!(PgQuoting.placeholder(3), "$3");
        assert_eq!(BracketQuoting.placeholder(3), "@P3");
        assert_eq!(PgQuoting.text_cast("\"c\""), "CAST(\"c\" AS TEXT)");
        assert_eq!(BacktickQuoting.text_cast("`c`"), "CAST(`c` AS CHAR)");
        assert!(PgQuoting.nulls_ordering() && AnsiQuoting.nulls_ordering());
        assert!(!BacktickQuoting.nulls_ordering() && !BracketQuoting.nulls_ordering());
        assert_eq!(BracketQuoting.like_specials(), "%_[");
        assert_eq!(AnsiQuoting.limit_clause(10), "LIMIT 10");
        assert_eq!(AnsiQuoting.top_clause(10), None);
        assert_eq!(PgQuoting.top_clause(10), None);
        assert_eq!(BacktickQuoting.top_clause(10), None);
        assert_eq!(BracketQuoting.top_clause(10).as_deref(), Some("TOP (10)"));
        assert_eq!(
            BracketQuoting.limit_clause(10),
            "OFFSET 0 ROWS FETCH NEXT 10 ROWS ONLY"
        );
        assert!(BracketQuoting.lex_rules().bracket_identifiers);
        assert!(AnsiQuoting.lex_rules().bracket_identifiers);
        assert!(!PgQuoting.lex_rules().bracket_identifiers);
        assert!(!BacktickQuoting.lex_rules().bracket_identifiers);
        assert!(PgQuoting.lex_rules().dollar_quotes && PgQuoting.lex_rules().e_strings);
        assert!(BacktickQuoting.lex_rules().backslash_escapes);
        assert_eq!(
            AnsiQuoting.lex_rules(),
            LexRules {
                bracket_identifiers: true,
                ..LexRules::default()
            }
        );
    }

    /// DDL literals (comments, literal defaults) are escaped by the dialect: quotes,
    /// backslashes, unicode and reserved words.
    #[test]
    fn ddl_literals_and_reserved_words() {
        let tricky = "it's a \\ test \u{e9}\u{1f600} 'x'";
        assert_eq!(
            AnsiQuoting.quote_str(tricky),
            "'it''s a \\ test \u{e9}\u{1f600} ''x'''"
        );
        assert_eq!(PgQuoting.quote_str(tricky), AnsiQuoting.quote_str(tricky));
        assert_eq!(
            BacktickQuoting.quote_str(tricky),
            "'it''s a \\\\ test \u{e9}\u{1f600} ''x'''"
        );
        assert_eq!(BacktickQuoting.quote_str("nul\0"), "'nul\\0'");
        for word in ["select", "order", "group", "user", "table"] {
            assert_eq!(AnsiQuoting.quote_ident(word), format!("\"{word}\""));
            assert_eq!(BacktickQuoting.quote_ident(word), format!("`{word}`"));
        }
        assert_eq!(
            PgQuoting.quote_ident("caf\u{e9} \"x\""),
            "\"caf\u{e9} \"\"x\"\"\""
        );
    }

    fn serde_json_value() -> serde_json::Value {
        serde_json::json!({"a": "b'c"})
    }
}
