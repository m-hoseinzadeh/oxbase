//! Formula-injection protection for spreadsheet-bound output (CSV and XLSX writers).
//!
//! Spreadsheet programs run a cell as a formula when its text starts with `=`, `+`, `-` or `@`
//! (and some also after a leading tab or carriage return), so exported data such as
//! `=HYPERLINK("http://evil", "x")` would execute when the file is opened. With the writer's
//! `formula_guard` option (on by default):
//!
//! - **CSV**: such a field is written with a leading `'` ([`guard_text`]); spreadsheets show the
//!   text and do not evaluate it. The change is visible to other CSV consumers, which is why it
//!   is an option.
//! - **XLSX**: such text is written as a string cell with the *quote prefix* style
//!   ([`needs_guard`] decides); the cell value stays exactly the text. String cells are never
//!   formulas in the first place (the writer never calls `write_formula` for data), so this
//!   only protects against a later edit-and-enter in the spreadsheet.
//!
//! Numbers are never guarded: a negative number written as a number is not text. Only text
//! values (logical types text, char, varchar, json, enum, other) are checked.

use std::borrow::Cow;

/// Characters that start a formula.
pub const TRIGGERS: [char; 6] = ['=', '+', '-', '@', '\t', '\r'];

/// `true` when `text` would be run as a formula by a spreadsheet.
pub fn needs_guard(text: &str) -> bool {
    text.starts_with(TRIGGERS)
}

/// `text` with a leading `'` when [`needs_guard`], else unchanged.
pub fn guard_text(text: &str) -> Cow<'_, str> {
    if needs_guard(text) {
        Cow::Owned(format!("'{text}"))
    } else {
        Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_formula_triggers_only() {
        for text in ["=1+1", "+1", "-2+3", "@SUM(A1)", "\t=x", "\r=x"] {
            assert!(needs_guard(text), "{text:?}");
            assert_eq!(guard_text(text), format!("'{text}"));
        }
        for text in ["", "plain", "a=b", " =1", "1-2"] {
            assert!(!needs_guard(text), "{text:?}");
            assert!(matches!(guard_text(text), Cow::Borrowed(_)));
        }
    }
}
