//! Findings the lexer proves: unterminated strings, quoted names and comments, unbalanced
//! parentheses; and client commands of another dialect (`DELIMITER`, `GO`).

use oxbase_sql::{SqlTokenKind, tokenize};

use super::features::Features;
use super::scan::{Kind, Toks};
use super::words::{self, has};
use super::{Diagnostic, DiagnosticCode, Severity};
use crate::Dialect;

/// Unterminated tokens of the whole text; returns where the first one starts.
pub(crate) fn unterminated(
    dialect: &Dialect,
    text: &str,
    out: &mut Vec<Diagnostic>,
) -> Option<usize> {
    let mut first = None;
    for tok in tokenize(dialect.sql, text) {
        if tok.is_terminated(text) {
            continue;
        }
        let (code, message) = match tok.kind {
            SqlTokenKind::String { .. } => (
                DiagnosticCode::UnterminatedString,
                "This string is never closed.",
            ),
            SqlTokenKind::QuotedIdent { .. } => (
                DiagnosticCode::UnterminatedIdentifier,
                "This quoted name is never closed.",
            ),
            SqlTokenKind::BlockComment | SqlTokenKind::ExecComment => (
                DiagnosticCode::UnterminatedComment,
                "This comment is never closed (`*/` is missing).",
            ),
            _ => continue,
        };
        // Underline the opening line only, not the rest of the script.
        let line_end = text[tok.start..tok.end]
            .find('\n')
            .map_or(tok.end, |p| tok.start + p);
        let end = if line_end > tok.start {
            line_end
        } else {
            tok.end
        };
        out.push(Diagnostic::new(
            code,
            tok.start..end,
            Severity::Error,
            message,
        ));
        first.get_or_insert(tok.start);
    }
    first
}

/// `)` without `(` and `(` never closed, within one statement.
pub(crate) fn parens(t: &Toks<'_>, out: &mut Vec<Diagnostic>) {
    let mut open: Vec<usize> = Vec::new();
    for tok in &t.all {
        match tok.kind {
            Kind::Punct(b'(') => open.push(tok.start),
            Kind::Punct(b')') => {
                let unmatched = open.pop().is_none();
                if unmatched {
                    out.push(Diagnostic::new(
                        DiagnosticCode::UnbalancedParenthesis,
                        tok.start..tok.end,
                        Severity::Error,
                        "This `)` has no matching `(`.",
                    ));
                }
            }
            _ => {}
        }
    }
    for start in open {
        out.push(Diagnostic::new(
            DiagnosticCode::UnbalancedParenthesis,
            start..start + 1,
            Severity::Error,
            "This `(` is never closed.",
        ));
    }
}

/// MySQL `DELIMITER` and T-SQL `GO` lines in a dialect whose client does not know them.
pub(crate) fn directives(
    t: &Toks<'_>,
    feat: &Features,
    dialect: &Dialect,
    out: &mut Vec<Diagnostic>,
) {
    for (i, tok) in t.all.iter().enumerate() {
        if tok.kind != Kind::Word || !tok.line_start {
            continue;
        }
        let statement_start = i == 0 || t.is_punct(i - 1, b';');
        if !feat.delimiter && statement_start && t.is_word(i, "DELIMITER") && i + 1 < t.len() {
            out.push(Diagnostic::new(
                DiagnosticCode::DialectMismatch,
                tok.start..tok.end,
                Severity::Warning,
                format!(
                    "`DELIMITER` is a command of the MySQL client; {} does not understand it.",
                    feat.name
                ),
            ));
        }
        if !feat.go && t.is_word(i, "GO") {
            let mut next = i + 1;
            if t.kind(next) == Some(Kind::Number) && !t.all[next].line_start {
                next += 1;
            }
            let alone = t.all.get(next).is_none_or(|n| n.line_start);
            let prev_ok = i == 0
                || (matches!(
                    t.kind(i - 1),
                    Some(
                        Kind::Word
                            | Kind::Quoted
                            | Kind::Number
                            | Kind::Str
                            | Kind::Param
                            | Kind::Punct(b')' | b';')
                    )
                ) && !(t.kind(i - 1) == Some(Kind::Word)
                    && dialect.language.is_keyword(t.text(i - 1))));
            let next_ok = t.all.get(next).is_none()
                || (t.kind(next) == Some(Kind::Word)
                    && has(words::STATEMENT_STARTS, &t.upper(next)));
            if alone && prev_ok && next_ok {
                out.push(Diagnostic::new(
                    DiagnosticCode::DialectMismatch,
                    tok.start..tok.end,
                    Severity::Warning,
                    format!(
                        "`GO` is the batch separator of SQL Server tools; {} does not understand it.",
                        feat.name
                    ),
                ));
            }
        }
    }
}
