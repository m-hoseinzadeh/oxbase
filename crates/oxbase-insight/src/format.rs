//! The SQL formatter (`docs/m6-contracts.md` §4.6). Owner of [`format`] and
//! [`format_range`]: the **format** stream. [`check_format`] is final.
//!
//! The formatter only changes whitespace and the case of keywords. Every result passes
//! [`check_format`] before it is returned; when it would not (a formatter bug, or text the
//! formatter cannot handle), the call fails and the editor keeps the original text.

use std::ops::Range;

use oxbase_types::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::Dialect;

/// How keywords are written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeywordCase {
    #[default]
    Upper,
    Lower,
    /// Keep as typed.
    Preserve,
}

/// Formatter settings (stored per user in the app settings; the editor sends them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FormatOptions {
    /// Spaces per indent level (1 to 8); ignored with `use_tabs`.
    pub indent_width: u8,
    pub use_tabs: bool,
    pub keyword_case: KeywordCase,
    /// Soft limit: lists longer than this break one item per line (40 to 400).
    pub max_line_length: u16,
    /// Put the comma at the start of the next line in broken lists.
    pub leading_commas: bool,
    /// One blank line between statements.
    pub blank_line_between_statements: bool,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            indent_width: 4,
            use_tabs: false,
            keyword_case: KeywordCase::Upper,
            max_line_length: 100,
            leading_commas: false,
            blank_line_between_statements: true,
        }
    }
}

impl FormatOptions {
    /// `Error::InvalidInput` when a value is out of its documented range.
    pub fn validate(self) -> Result<()> {
        if !(1..=8).contains(&self.indent_width) {
            return Err(Error::invalid_input("indent width must be 1 to 8"));
        }
        if !(40..=400).contains(&self.max_line_length) {
            return Err(Error::invalid_input(
                "maximum line length must be 40 to 400",
            ));
        }
        Ok(())
    }
}

/// A replacement of `range` (bytes of the original text) by `text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub range: Range<usize>,
    pub text: String,
}

/// The formatter invariant: `after` is `before` with only whitespace and keyword case changed
/// (`oxbase_sql::tokens_equivalent`, where a word may change case only when the dialect's
/// [`SqlLanguage`](crate::SqlLanguage) lists it as a keyword). `Error::Internal` describing the
/// first difference otherwise.
pub fn check_format(dialect: &Dialect, before: &str, after: &str) -> Result<()> {
    let is_keyword = |w: &str| dialect.language.is_keyword(w);
    oxbase_sql::tokens_equivalent(dialect.sql, before, after, &is_keyword)
        .map_err(|m| Error::internal(format!("the formatter would change the SQL: {m}")))
}

/// `text` formatted with `options`: clauses on their own lines, nested queries and lists
/// indented, keywords re-cased, comments kept where they are relative to their neighbours,
/// statements separated as `options` says; `GO` and `DELIMITER` lines kept on lines of their
/// own. Fails with `Error::InvalidInput` for invalid options, and with `Error::Internal` when
/// the result would not pass [`check_format`].
pub fn format(dialect: &Dialect, text: &str, options: &FormatOptions) -> Result<String> {
    let _ = (dialect, text, options);
    unimplemented!("m6: format")
}

/// Formats the statements that overlap `range` (bytes, clamped to the text) and returns one
/// edit that replaces exactly those statements (from the first one's start to the last one's
/// end), indented to the column the first statement starts at. Same failures as [`format`].
pub fn format_range(
    dialect: &Dialect,
    text: &str,
    range: Range<usize>,
    options: &FormatOptions,
) -> Result<TextEdit> {
    let _ = (dialect, text, range, options);
    unimplemented!("m6: format")
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::Engine;

    #[test]
    fn options_default_validate_and_deserialize() {
        let o = FormatOptions::default();
        o.validate().unwrap();
        let parsed: FormatOptions =
            serde_json::from_value(serde_json::json!({ "keywordCase": "lower", "useTabs": true }))
                .unwrap();
        assert_eq!(parsed.keyword_case, KeywordCase::Lower);
        assert!(parsed.use_tabs);
        assert_eq!(parsed.indent_width, 4);
        let bad = FormatOptions {
            indent_width: 0,
            ..o
        };
        assert!(bad.validate().is_err());
        let bad = FormatOptions {
            max_line_length: 10,
            ..o
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn the_check_allows_keyword_case_only() {
        let pg = Dialect::builtin(Engine::Postgres).unwrap();
        check_format(&pg, "select a from t", "SELECT a\nFROM t").unwrap();
        let err = check_format(&pg, "select A from t", "SELECT a FROM t").unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::Internal);
        assert!(check_format(&pg, "select 1 -- x\n", "select 1 -- x").is_ok());
        assert!(check_format(&pg, "select 1 -- x\nfrom t", "select 1 -- x from t").is_err());
    }
}
