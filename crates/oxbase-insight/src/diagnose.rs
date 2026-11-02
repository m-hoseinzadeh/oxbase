//! Diagnostics (`docs/m6-contracts.md` §4.5). Owner of [`diagnose`]: the **diagnose** stream.
//!
//! The rule is **no false-positive errors**: only what the lexer proves wrong is an error
//! (an unterminated string, quoted identifier or comment; a parenthesis without its partner).
//! Anything that depends on the catalog, on the parser's reading of incomplete SQL or on
//! dialect features the parser may not know is a warning or a hint. [`Diagnostic::new`] caps
//! the severity at [`DiagnosticCode::max_severity`], so no code path can raise it.
//!
//! # How it works
//!
//! The text is cut with `oxbase_sql::split`; the lexer's tokens give the unterminated tokens
//! and, per statement, the parenthesis balance. Each statement is then read by the tolerant
//! parser of `analyze` (one statement of the splitter may hold several SQL statements: a
//! T-SQL batch, a routine body), which reports syntax it is sure about (clause order, stray
//! commas), syntax of another dialect (`LIMIT` on SQL Server, `TOP` elsewhere, `::`,
//! backticks, `ILIKE`, client commands), `UPDATE` / `DELETE` without `WHERE` and `DROP`
//! without `IF EXISTS`. With a cache, `checks` resolves relations (through CTEs, aliases,
//! derived tables and the search path), columns and functions. Names created earlier in the
//! script, temporary tables, system objects, other databases, routine bodies and anything
//! the parser is unsure about only ever give hints.

use std::fmt;
use std::ops::Range;

use oxbase_schema_cache::SchemaCache;
use oxbase_types::{Error, Result};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::Dialect;

pub(crate) mod analyze;
pub(crate) mod checks;
pub(crate) mod features;
mod lexical;
pub(crate) mod scan;
pub(crate) mod words;

/// How bad a finding is. Ordered: `Hint < Warning < Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Shown faintly (dotted underline); for uncertain findings.
    Hint,
    Warning,
    /// Certain: the server will reject the text.
    Error,
}

/// What a diagnostic is about. The serde form is the code shown to users.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticCode {
    /// A string literal without its closing quote (or dollar tag).
    UnterminatedString,
    /// A quoted identifier without its closing quote.
    UnterminatedIdentifier,
    /// A block comment without its `*/`.
    UnterminatedComment,
    /// A `)` without `(`, or a `(` never closed, inside one statement.
    UnbalancedParenthesis,
    /// A relation the cache does not know.
    UnknownRelation,
    /// A column none of the statement's relations has.
    UnknownColumn,
    /// An unqualified column several relations in scope have.
    AmbiguousColumn,
    /// A schema the cache does not know.
    UnknownSchema,
    /// A function neither built in nor in the cache.
    UnknownFunction,
    /// A built-in function called with a number of arguments no overload takes.
    WrongArgumentCount,
    /// `UPDATE` / `DELETE` without `WHERE` (affects every row).
    MissingWhere,
    /// Syntax the parser is sure is wrong in every dialect (clause order, stray commas), or a
    /// clause left empty while typing (hint).
    SyntaxError,
    /// Syntax of another SQL dialect (`LIMIT` on SQL Server, `TOP` on PostgreSQL, `::` on
    /// MySQL, backtick quotes on PostgreSQL, a `GO` line outside SQL Server tools).
    DialectMismatch,
    /// `DROP TABLE` / `DROP VIEW` without `IF EXISTS`.
    MissingIfExists,
}

impl DiagnosticCode {
    /// The highest severity this code may ever have:
    ///
    /// | Code | Max |
    /// |---|---|
    /// | unterminated string / identifier / comment, unbalanced parenthesis | error |
    /// | unknown relation, unknown column, unknown schema, ambiguous column, wrong argument count, missing where, syntax error, dialect mismatch | warning |
    /// | unknown function, missing if exists | hint |
    ///
    /// Unknown names are warnings only when the cache is complete and fresh and the name is
    /// not created earlier in the script, a temporary table, a CTE or an alias; otherwise
    /// hints (`docs/m6-contracts.md` §4.5).
    pub const fn max_severity(self) -> Severity {
        match self {
            Self::UnterminatedString
            | Self::UnterminatedIdentifier
            | Self::UnterminatedComment
            | Self::UnbalancedParenthesis => Severity::Error,
            Self::UnknownRelation
            | Self::UnknownColumn
            | Self::AmbiguousColumn
            | Self::UnknownSchema
            | Self::WrongArgumentCount
            | Self::MissingWhere
            | Self::SyntaxError
            | Self::DialectMismatch => Severity::Warning,
            Self::UnknownFunction | Self::MissingIfExists => Severity::Hint,
        }
    }

    /// The code as users see it (`unknown-relation`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnterminatedString => "unterminated-string",
            Self::UnterminatedIdentifier => "unterminated-identifier",
            Self::UnterminatedComment => "unterminated-comment",
            Self::UnbalancedParenthesis => "unbalanced-parenthesis",
            Self::UnknownRelation => "unknown-relation",
            Self::UnknownColumn => "unknown-column",
            Self::AmbiguousColumn => "ambiguous-column",
            Self::UnknownSchema => "unknown-schema",
            Self::UnknownFunction => "unknown-function",
            Self::WrongArgumentCount => "wrong-argument-count",
            Self::MissingWhere => "missing-where",
            Self::SyntaxError => "syntax-error",
            Self::DialectMismatch => "dialect-mismatch",
            Self::MissingIfExists => "missing-if-exists",
        }
    }
}

impl fmt::Display for DiagnosticCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Byte range in the full text (non-empty where possible).
    pub range: Range<usize>,
    pub severity: Severity,
    pub code: DiagnosticCode,
    /// One sentence for the user. Never contains secrets (string literal contents are not
    /// quoted in messages).
    pub message: String,
}

impl Diagnostic {
    /// A diagnostic with `severity` capped at [`DiagnosticCode::max_severity`].
    pub fn new(
        code: DiagnosticCode,
        range: Range<usize>,
        severity: Severity,
        message: impl Into<String>,
    ) -> Self {
        Self {
            range,
            severity: severity.min(code.max_severity()),
            code,
            message: message.into(),
        }
    }
}

/// Every diagnostic of `text`, sorted by range start. Works without a cache (then without the
/// catalog findings: lexical errors, syntax, dialect mismatches and dangerous statements
/// remain). Must finish within 100 ms for a 1 MiB script in a release build; checks `cancel`
/// between statements and returns `Error::Cancelled` once it is cancelled.
pub fn diagnose(
    dialect: &Dialect,
    text: &str,
    cache: Option<&SchemaCache>,
    cancel: &CancellationToken,
) -> Result<Vec<Diagnostic>> {
    let feat = features::features(dialect.sql);
    let mut out = Vec::new();
    let first_bad = lexical::unterminated(dialect, text, &mut out);
    let mut env = checks::Env::new(dialect, cache);
    for statement in oxbase_sql::split(dialect.sql, text) {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let toks = scan::Toks::new(dialect, &feat, text, statement.range.clone());
        // A string or comment that runs to the end swallows the closing parentheses: one
        // error is enough.
        if first_bad.is_none_or(|bad| bad >= statement.range.end) {
            lexical::parens(&toks, &mut out);
        }
        lexical::directives(&toks, &feat, dialect, &mut out);
        if cache.is_some() && may_change_schema(&statement.text) {
            env.apply_changes(&oxbase_sql::schema_changes(dialect.sql, &statement.text));
        }
        for st in analyze::analyze(&dialect.language, &feat, &toks) {
            out.extend_from_slice(&st.findings);
            if cache.is_some() {
                checks::check(&env, dialect, &st, &mut out);
            }
            env.after(&st);
        }
    }
    out.sort_by_key(|d| (d.range.start, d.range.end, d.code.as_str()));
    out.dedup();
    Ok(out)
}

/// `false` when `text` cannot hold a DDL verb `oxbase_sql::schema_changes` looks for
/// (`CREATE`, `ALTER`, `DROP`, `RENAME`, `sp_rename`, in any case): such statements skip a
/// second tokenization (most statements of a large script).
fn may_change_schema(text: &str) -> bool {
    const VERBS: [&[u8]; 4] = [b"create", b"alter", b"drop", b"rename"];
    let bytes = text.as_bytes();
    bytes.iter().enumerate().any(|(at, b)| {
        matches!(b.to_ascii_lowercase(), b'c' | b'a' | b'd' | b'r')
            && VERBS.iter().any(|verb| {
                bytes
                    .get(at..at + verb.len())
                    .is_some_and(|w| w.eq_ignore_ascii_case(verb))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_texts_with_a_ddl_verb_are_scanned_for_schema_changes() {
        assert!(may_change_schema("create table t (a int)"));
        assert!(may_change_schema("ALTER TABLE t ADD b int"));
        assert!(may_change_schema("exec Sp_Rename 'a', 'b'"));
        assert!(may_change_schema("x DROP"));
        assert!(!may_change_schema("SELECT a FROM t WHERE b = 'cre'"));
        assert!(!may_change_schema("drp"));
        assert!(!may_change_schema(""));
    }

    #[test]
    fn severities_are_capped() {
        let d = Diagnostic::new(DiagnosticCode::UnknownFunction, 0..3, Severity::Error, "x");
        assert_eq!(d.severity, Severity::Hint);
        let d = Diagnostic::new(DiagnosticCode::UnknownRelation, 0..3, Severity::Error, "x");
        assert_eq!(d.severity, Severity::Warning);
        let d = Diagnostic::new(DiagnosticCode::UnknownRelation, 0..3, Severity::Hint, "x");
        assert_eq!(d.severity, Severity::Hint);
        let d = Diagnostic::new(
            DiagnosticCode::UnterminatedString,
            0..3,
            Severity::Error,
            "x",
        );
        assert_eq!(d.severity, Severity::Error);
    }

    #[test]
    fn codes_serialize_as_shown() {
        for code in [
            DiagnosticCode::UnterminatedString,
            DiagnosticCode::UnterminatedIdentifier,
            DiagnosticCode::UnterminatedComment,
            DiagnosticCode::UnbalancedParenthesis,
            DiagnosticCode::UnknownRelation,
            DiagnosticCode::UnknownColumn,
            DiagnosticCode::AmbiguousColumn,
            DiagnosticCode::UnknownSchema,
            DiagnosticCode::UnknownFunction,
            DiagnosticCode::WrongArgumentCount,
            DiagnosticCode::MissingWhere,
            DiagnosticCode::SyntaxError,
            DiagnosticCode::DialectMismatch,
            DiagnosticCode::MissingIfExists,
        ] {
            assert_eq!(
                serde_json::to_value(code).unwrap(),
                serde_json::json!(code.as_str())
            );
            assert_eq!(code.to_string(), code.as_str());
        }
        assert_eq!(
            serde_json::to_value(Severity::Warning).unwrap(),
            serde_json::json!("warning")
        );
    }
}
