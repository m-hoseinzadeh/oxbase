//! What a SQL dump (M5, `oxbase-io`'s SQL dump writer) needs from an engine besides
//! [`Quoting::literal`](crate::Quoting::literal): the session statements around the script, the
//! literal forms the target's server reads, and how explicit values go into identity columns.
//!
//! These are the [`Quoting`](crate::Quoting) methods `dump_session`, `dump_literal` and
//! `dump_identity_insert`; the types and spelling helpers they share live here. The writer only
//! calls them, so it has no engine special cases (docs/m5-contracts.md §10.10).

use std::fmt::Write as _;

use chrono::{NaiveDateTime, NaiveTime, Timelike};

/// Statements a dump runs before everything else and after its last statement, each without
/// the terminating `;`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DumpSession {
    /// Before the first statement (also before `DROP` / `CREATE`, whose comments and defaults
    /// are literals too).
    pub prologue: Vec<String>,
    /// After the last statement: undoes what the prologue changed, where that is possible.
    pub epilogue: Vec<String>,
}

/// One value as a dump literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpLiteral {
    /// The SQL text.
    pub sql: String,
    /// `Some(n)`: the value had more than `n` fractional-second digits; the target keeps `n`,
    /// so the rest were dropped (the writer reports it).
    pub cut_to: Option<u32>,
}

impl DumpLiteral {
    /// A literal that keeps the value exactly.
    pub fn exact(sql: String) -> Self {
        Self { sql, cut_to: None }
    }
}

/// What the inserts of a dump need when they write explicit values into identity or
/// auto-increment columns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentityInsert {
    /// Words after the column list of every `INSERT` (PostgreSQL `OVERRIDING SYSTEM VALUE`).
    pub overriding: Option<String>,
    /// A statement before the first `INSERT` (SQL Server `SET IDENTITY_INSERT <t> ON`).
    pub enable: Option<String>,
    /// A statement after the last `INSERT` (SQL Server `SET IDENTITY_INSERT <t> OFF`).
    pub disable: Option<String>,
}

/// Appends `.` and the first `digits` fractional digits of `nanos` without trailing zeros
/// (nothing for a whole second). `true` when non-zero digits were dropped.
pub(crate) fn push_fraction(nanos: u32, digits: u32, out: &mut String) -> bool {
    // A leap second carries nanos >= 10^9; it is written as the last instant of the second.
    let nanos = nanos.min(999_999_999);
    let digits = digits.min(9);
    let scale = 10_u32.pow(9 - digits);
    let kept = nanos / scale;
    if kept > 0 {
        let width = usize::try_from(digits).unwrap_or(9);
        let mut text = format!("{kept:0width$}");
        while text.ends_with('0') {
            text.pop();
        }
        out.push('.');
        out.push_str(&text);
    }
    !nanos.is_multiple_of(scale)
}

/// `yyyy-mm-dd<sep>hh:mm:ss[.f…]` with at most `digits` fractional digits; `true` when digits
/// were dropped.
pub(crate) fn push_datetime(t: &NaiveDateTime, sep: char, digits: u32, out: &mut String) -> bool {
    let _ = write!(out, "{}{sep}{}", t.format("%Y-%m-%d"), t.format("%H:%M:%S"));
    push_fraction(t.nanosecond(), digits, out)
}

/// `hh:mm:ss[.f…]` with at most `digits` fractional digits; `true` when digits were dropped.
pub(crate) fn push_time(t: NaiveTime, digits: u32, out: &mut String) -> bool {
    let _ = write!(out, "{}", t.format("%H:%M:%S"));
    push_fraction(t.nanosecond(), digits, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn fractions_keep_at_most_the_digits_asked() {
        let mut out = String::new();
        assert!(!push_fraction(0, 6, &mut out));
        assert_eq!(out, "");
        assert!(!push_fraction(500_000_000, 6, &mut out));
        assert_eq!(out, ".5");
        out.clear();
        assert!(push_fraction(123_456_789, 7, &mut out));
        assert_eq!(out, ".1234567");
        out.clear();
        // A leap second is the last instant of its second.
        assert!(push_fraction(1_500_000_000, 6, &mut out));
        assert_eq!(out, ".999999");
    }
}
