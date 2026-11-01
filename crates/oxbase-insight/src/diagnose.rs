//! Diagnostics (`docs/m6-contracts.md` §4.5). Owner of [`diagnose`]: the **diagnose** stream.
//!
//! The rule is **no false-positive errors**: only what the lexer proves wrong is an error
//! (an unterminated string, quoted identifier or comment; a parenthesis without its partner).
//! Anything that depends on the catalog, on the parser's reading of incomplete SQL or on
//! dialect features the parser may not know is a warning or a hint. [`Diagnostic::new`] caps
//! the severity at [`DiagnosticCode::max_severity`], so no code path can raise it.

use std::fmt;
use std::ops::Range;

use oxbase_schema_cache::SchemaCache;
use oxbase_types::Result;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::Dialect;

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
}

impl DiagnosticCode {
    /// The highest severity this code may ever have:
    ///
    /// | Code | Max |
    /// |---|---|
    /// | unterminated string / identifier / comment, unbalanced parenthesis | error |
    /// | unknown relation, unknown column, unknown schema, ambiguous column, wrong argument count, missing where | warning |
    /// | unknown function | hint |
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
            | Self::MissingWhere => Severity::Warning,
            Self::UnknownFunction => Severity::Hint,
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

/// Every diagnostic of `text`, sorted by range start. Works without a cache (lexical findings
/// only). Must finish within 100 ms for a 1 MiB script in a release build; checks `cancel`
/// between statements and returns `Error::Cancelled` once it is cancelled.
pub fn diagnose(
    dialect: &Dialect,
    text: &str,
    cache: Option<&SchemaCache>,
    cancel: &CancellationToken,
) -> Result<Vec<Diagnostic>> {
    let _ = (dialect, text, cache, cancel);
    unimplemented!("m6: diagnose")
}

#[cfg(test)]
mod tests {
    use super::*;

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
