//! What may be typed at the cursor (`docs/m6-contracts.md` §4.2). Owner of [`context_at`]: the
//! **context** stream.

use std::ops::Range;

use oxbase_types::IdentCase;

use crate::outline::{Clause, Cte, Outline, RelationSource, ScopeRelation, StatementClass};
use crate::{Dialect, Ident};

/// An item kind the cursor position accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Expected {
    /// A keyword ([`CursorContext::keywords`] lists the likely ones; empty = any).
    Keyword,
    Database,
    Schema,
    /// A table or view.
    Relation,
    Column,
    /// A scalar, aggregate or window function (built-in or user-defined).
    Function,
    /// A routine for `CALL` / `EXEC`.
    Procedure,
    Type,
    Sequence,
    /// Snippets (statement starts).
    Snippet,
}

/// The cursor's context in one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorContext {
    /// The cursor (byte offset in the full text).
    pub offset: usize,
    /// The statement around the cursor ([`oxbase_sql::pick_statement`] rules; an empty range
    /// at the cursor when the text has no statement).
    pub statement: Range<usize>,
    pub class: StatementClass,
    pub clause: Clause,
    /// The range a completion replaces: the identifier or keyword being typed, including an
    /// opening quote, up to the end of the word (it may extend past the cursor). Empty at the
    /// cursor when nothing is being typed.
    pub word: Range<usize>,
    /// The typed part of the word before the cursor, without its opening quote.
    pub prefix: String,
    /// The opening quote byte when the cursor is inside a quoted identifier (`"`, `` ` ``,
    /// `[`).
    pub quote: Option<u8>,
    /// The parts before the last `.` (`s.t.c|` → `[s, t]`); empty when unqualified.
    pub qualifier: Vec<Ident>,
    /// What may be typed here, most likely first. Empty inside a string literal or a comment,
    /// and where a new name is being defined (no completion).
    pub expected: Vec<Expected>,
    /// Keywords that fit here (upper case, most likely first), when [`Expected::Keyword`] is
    /// expected; empty = any keyword of the dialect.
    pub keywords: Vec<&'static str>,
    /// Relations visible at the cursor: the innermost block's first, then enclosing blocks'
    /// (for correlated subqueries). For a DML statement, its target first.
    pub scope: Vec<ScopeRelation>,
    /// CTEs visible at the cursor, innermost first.
    pub ctes: Vec<Cte>,
    /// The statement's outline.
    pub outline: Outline,
}

impl CursorContext {
    /// `true` when `kind` is expected.
    pub fn expects(&self, kind: Expected) -> bool {
        self.expected.contains(&kind)
    }

    /// The scope relations a column completion draws from: with a qualifier (`x.|`), those
    /// whose alias, or whose own name when it has no alias, equals the qualifier's last part
    /// under `case`; else every relation in scope.
    pub fn column_sources(&self, case: IdentCase) -> Vec<&ScopeRelation> {
        let Some(q) = self.qualifier.last() else {
            return self.scope.iter().collect();
        };
        self.scope
            .iter()
            .filter(|r| match (&r.alias, &r.source) {
                (Some(alias), _) => case.matches(&q.text, q.quoted, &alias.text),
                (None, RelationSource::Named(parts) | RelationSource::Function(parts)) => parts
                    .last()
                    .is_some_and(|last| case.matches(&q.text, q.quoted, &last.text)),
                (None, _) => false,
            })
            .collect()
    }
}

/// The context of the cursor at byte `offset` of `text` (clamped to the text and to a char
/// boundary). Never fails and never panics; pure function of its inputs (no cache: names are
/// resolved by [`crate::complete`]). Must take well under 5 ms for a statement of 10 000
/// tokens in a release build (`docs/m6-contracts.md` §5).
pub fn context_at(dialect: &Dialect, text: &str, offset: usize) -> CursorContext {
    let _ = (dialect, text, offset);
    unimplemented!("m6: context")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(text: &str) -> Ident {
        Ident {
            text: text.to_owned(),
            quoted: false,
        }
    }

    fn rel(name: &str, alias: Option<&str>) -> ScopeRelation {
        ScopeRelation {
            source: RelationSource::Named(vec![ident("public"), ident(name)]),
            alias: alias.map(ident),
            column_aliases: vec![],
            range: 0..0,
        }
    }

    fn ctx(qualifier: &[&str]) -> CursorContext {
        CursorContext {
            offset: 0,
            statement: 0..0,
            class: StatementClass::Select,
            clause: Clause::SelectList,
            word: 0..0,
            prefix: String::new(),
            quote: None,
            qualifier: qualifier.iter().copied().map(ident).collect(),
            expected: vec![Expected::Column],
            keywords: vec![],
            scope: vec![rel("orders", Some("o")), rel("customers", None)],
            ctes: vec![],
            outline: Outline::empty(0..0),
        }
    }

    #[test]
    fn column_sources_follow_the_qualifier() {
        let names = |c: &CursorContext, case| -> Vec<String> {
            c.column_sources(case)
                .iter()
                .map(|r| match &r.source {
                    RelationSource::Named(p) => p[1].text.clone(),
                    _ => String::new(),
                })
                .collect()
        };
        let lower = IdentCase::FoldLower;
        assert_eq!(names(&ctx(&[]), lower), ["orders", "customers"]);
        assert_eq!(names(&ctx(&["O"]), lower), ["orders"]);
        assert_eq!(names(&ctx(&["customers"]), lower), ["customers"]);
        // An aliased relation is reached through its alias only.
        assert_eq!(names(&ctx(&["orders"]), lower), Vec::<String>::new());
        assert_eq!(
            names(&ctx(&["CUSTOMERS"]), IdentCase::Sensitive),
            Vec::<String>::new()
        );
        assert!(ctx(&[]).expects(Expected::Column));
        assert!(!ctx(&[]).expects(Expected::Keyword));
    }
}
