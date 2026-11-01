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
//! values (logical types text, char, varchar, json, enum, other) are checked, and text that is
//! a plain number (`-12.50`, `+1`, `-1.5e3`) is not guarded either: a spreadsheet reads it as
//! that number, never as a formula, and some drivers return exact decimals as text (SQL Server
//! `decimal`, PostgreSQL `numeric` without a precision), which must not change on export.

use std::borrow::Cow;

/// Characters that start a formula.
pub const TRIGGERS: [char; 6] = ['=', '+', '-', '@', '\t', '\r'];

/// `true` when `text` would be run as a formula by a spreadsheet: it starts with one of
/// [`TRIGGERS`] and is not a plain number ([`is_plain_number`]).
pub fn needs_guard(text: &str) -> bool {
    text.starts_with(TRIGGERS) && !is_plain_number(text)
}

/// `true` for a decimal number literal: an optional sign, digits with at most one `.` (at
/// least one digit), and an optional exponent (`e` / `E`, optional sign, digits). Nothing else,
/// not even spaces.
pub fn is_plain_number(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let mut digits = 0;
    let mut dot = false;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'0'..=b'9' => digits += 1,
            b'.' if !dot => dot = true,
            _ => break,
        }
        i += 1;
    }
    if digits == 0 {
        return false;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        i += usize::from(matches!(bytes.get(i), Some(b'+' | b'-')));
        let start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    i == bytes.len()
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
        for text in [
            "=1+1", "+1+1", "-2+3", "@SUM(A1)", "\t=x", "\r=x", "-", "+", "-.", "-1e", "-1e+",
            "- 1", "-1 ", "-1.2.3", "-0x10", "-1,5", "+A1", "-inf", "@1",
        ] {
            assert!(needs_guard(text), "{text:?}");
            assert_eq!(guard_text(text), format!("'{text}"));
        }
        for text in ["", "plain", "a=b", " =1", "1-2"] {
            assert!(!needs_guard(text), "{text:?}");
            assert!(matches!(guard_text(text), Cow::Borrowed(_)));
        }
        // Plain numbers stay as they are, signed or not.
        for text in [
            "+1",
            "-2",
            "-4920.81",
            "-0.75",
            "-.5",
            "-5.",
            "+1.5e300",
            "-1E-7",
            "-12345678901234567890.0123456789",
        ] {
            assert!(is_plain_number(text), "{text:?}");
            assert!(!needs_guard(text), "{text:?}");
            assert!(matches!(guard_text(text), Cow::Borrowed(_)));
        }
    }
}
