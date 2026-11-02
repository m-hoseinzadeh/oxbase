//! Editor languages of engines that are not queried with SQL (M7): the MongoDB shell subset and
//! the Redis console. Contract: `docs/m7-contracts.md` §4.
//!
//! A [`ScriptLanguage`] is registered per engine
//! ([`DialectRegistry::register_script_language`](crate::DialectRegistry::register_script_language)).
//! `oxbase-services` resolves an editor's statement source from the engine: the SQL engines use
//! `oxbase_sql` (split, redaction) as before, every other engine its registered language. The
//! query service then runs the statements exactly like SQL ones: one
//! `Connection::execute(statement.text, &[], token)` per statement, in order, on the editor's
//! session, with the same events, result stores, paging, cancel and history. Nothing in the
//! services knows which engine it is.
//!
//! # Statements
//!
//! [`ScriptLanguage::split`] returns `oxbase_sql::Statement`s with the same range rules as the
//! SQL splitter (byte ranges into the script, no leading whitespace or comments, no
//! terminator, 1-based UTF-16 positions, ownership of the bytes up to `segment_end` for "run
//! current statement"). [`statement`] builds one from a range. `split` never fails: text it
//! cannot read becomes one statement that the engine will reject when it runs.
//!
//! # Guards
//!
//! [`ScriptLanguage::guard`] marks statements that need a confirmation (Redis `FLUSHALL`,
//! `FLUSHDB`, `KEYS`; mongo-shell `dropDatabase()`, `drop()`, `deleteMany({})`) or that the
//! editor refuses (`SUBSCRIBE` in the console: pub/sub has its own panel). The query service
//! checks every statement before the run starts: a blocked statement fails the run with
//! `InvalidInput`; one that needs confirmation fails it with `InvalidInput` whose message starts
//! with [`CONFIRMATION_REQUIRED`] unless the run's options confirm destructive statements.

use std::borrow::Cow;
use std::ops::Range;

use oxbase_sql::{Statement, StatementKind, position_at};

/// Start of the message of a run refused because a statement needs confirmation (the UI asks
/// and runs again with `RunOptions::confirm_destructive`).
pub const CONFIRMATION_REQUIRED: &str = "confirmation required: ";

/// What the editor may do with one statement.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StatementGuard {
    /// Runs as typed.
    Allowed,
    /// Runs only after the user confirmed; the text says what it would do ("FLUSHDB deletes
    /// every key of the current database").
    Confirm(String),
    /// Never runs from the editor; the text says why and what to use instead.
    Blocked(String),
}

/// The editor language of one non-SQL engine. One implementation per engine, registered in
/// its `dialect::register`. See the module docs.
pub trait ScriptLanguage: Send + Sync {
    /// Stable id: `mongo-shell`, `redis-console`.
    fn name(&self) -> &'static str;

    /// The Monaco language id the editor uses for highlighting (`javascript`, `plaintext`).
    fn editor_language(&self) -> &'static str;

    /// The statements of `text`, in order (see the module docs).
    fn split(&self, text: &str) -> Vec<Statement>;

    /// `text` with secrets replaced by `***` before it goes to history or into an error
    /// message: Redis `AUTH` / `HELLO ... AUTH` passwords, `ACL SETUSER` `>password` /
    /// `#hash` rules, `CONFIG SET requirepass` / `masterauth`, `MIGRATE ... AUTH`; mongo-shell
    /// `pwd` / `password` values in `createUser`, `updateUser`, `db.auth`,
    /// `changeUserPassword` and connection strings. Best effort, like
    /// `oxbase_sql::redact_secrets`; turning history off is the guarantee.
    fn redact_secrets<'a>(&self, text: &'a str) -> Cow<'a, str>;

    /// Whether `statement` may run from the editor. Default: [`StatementGuard::Allowed`].
    fn guard(&self, statement: &Statement) -> StatementGuard {
        let _ = statement;
        StatementGuard::Allowed
    }
}

/// A [`Statement`] for `range` of `text` (trimmed by the caller), owning the bytes up to
/// `segment_end` (at least `range.end`; clamped to the text).
///
/// Panics in debug builds when `range` is not inside `text` on character boundaries.
pub fn statement(
    text: &str,
    range: Range<usize>,
    segment_end: usize,
    kind: StatementKind,
) -> Statement {
    debug_assert!(range.start <= range.end && range.end <= text.len());
    let segment_end = segment_end.clamp(range.end, text.len());
    Statement {
        start: position_at(text, range.start),
        end: position_at(text, range.end),
        text: text[range.clone()].to_owned(),
        range,
        segment_end,
        kind,
    }
}

/// The trimmed byte range of `text[range]`: leading and trailing whitespace removed. `None`
/// when only whitespace is left.
pub fn trim_range(text: &str, range: Range<usize>) -> Option<Range<usize>> {
    let part = &text[range.clone()];
    let start = range.start + (part.len() - part.trim_start().len());
    let end = range.end - (part.len() - part.trim_end().len());
    (start < end).then_some(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lines;

    /// A toy language: one statement per non-empty line, `#` lines are comments.
    impl ScriptLanguage for Lines {
        fn name(&self) -> &'static str {
            "lines"
        }
        fn editor_language(&self) -> &'static str {
            "plaintext"
        }
        fn split(&self, text: &str) -> Vec<Statement> {
            let mut out = Vec::new();
            let mut offset = 0;
            for line in text.split_inclusive('\n') {
                let range = offset..offset + line.len();
                offset = range.end;
                if let Some(trimmed) = trim_range(text, range.clone())
                    && !text[trimmed.clone()].starts_with('#')
                {
                    out.push(statement(text, trimmed, range.end, StatementKind::Other));
                }
            }
            out
        }
        fn redact_secrets<'a>(&self, text: &'a str) -> Cow<'a, str> {
            Cow::Borrowed(text)
        }
    }

    #[test]
    fn statements_from_ranges() {
        let text = "  GET a\n# note\nSET ü 1\n\n";
        let statements = Lines.split(text);
        assert_eq!(statements.len(), 2);
        assert_eq!(statements[0].text, "GET a");
        assert_eq!(statements[0].range, 2..7);
        assert_eq!(statements[0].segment_end, 8);
        assert_eq!(statements[0].start.column, 3);
        assert_eq!(statements[1].text, "SET ü 1");
        assert_eq!(statements[1].start.line, 3);
        assert_eq!(statements[1].end.column, 8);
        assert_eq!(Lines.guard(&statements[0]), StatementGuard::Allowed);
        assert_eq!(trim_range(text, 7..8), None);
        let s = statement(text, 2..7, 1, StatementKind::Query);
        assert_eq!(s.segment_end, 7);
    }
}
