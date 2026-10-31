//! `CREATE` statements assembled from `information_schema` facts, for the `definition` fields of
//! routines, triggers and events.
//!
//! The server's own text (`SHOW CREATE ...`) would need the object name inside the SQL, which the
//! fetcher rules forbid (catalog queries take names as bound parameters only). The pieces come
//! from the catalog instead, in the layout `SHOW CREATE` uses; the body is the server's text,
//! unchanged.

use oxbase_dialect::{BacktickQuoting, Quoting};

/// A quoted identifier.
pub(crate) fn ident(s: &str) -> String {
    BacktickQuoting.quote_ident(s)
}

/// A quoted string literal.
pub(crate) fn literal(s: &str) -> String {
    BacktickQuoting.quote_str(s)
}

/// `` `user`@`host` `` from the catalog's `user@host` (the host is after the last `@`).
pub(crate) fn definer(definer: &str) -> String {
    match definer.rsplit_once('@') {
        Some((user, host)) => format!("{}@{}", ident(user), ident(host)),
        None => ident(definer),
    }
}

/// `CREATE DEFINER=... ` or `CREATE ` when the definer is unknown.
pub(crate) fn create_prefix(definer_text: Option<&str>) -> String {
    match definer_text {
        Some(d) if !d.is_empty() => format!("CREATE DEFINER={} ", definer(d)),
        _ => "CREATE ".to_owned(),
    }
}

/// The parts of a stored routine's `CREATE` statement.
#[derive(Debug, Clone, Default)]
pub(crate) struct RoutineText<'a> {
    pub definer: Option<&'a str>,
    /// `FUNCTION` or `PROCEDURE`.
    pub kind: &'a str,
    pub name: &'a str,
    /// Rendered parameters (`` `n` int `` or `` IN `n` int ``).
    pub params: Vec<String>,
    pub returns: Option<&'a str>,
    pub deterministic: bool,
    /// `SQL_DATA_ACCESS` (`CONTAINS SQL` is the default and is left out).
    pub data_access: Option<&'a str>,
    /// `SECURITY_TYPE` (`DEFINER` is the default and is left out).
    pub security: Option<&'a str>,
    pub comment: Option<&'a str>,
    pub body: &'a str,
}

impl RoutineText<'_> {
    pub(crate) fn render(&self) -> String {
        let mut out = format!(
            "{}{} {}({})",
            create_prefix(self.definer),
            self.kind,
            ident(self.name),
            self.params.join(", ")
        );
        if let Some(r) = self.returns {
            out.push_str(" RETURNS ");
            out.push_str(r);
        }
        let mut traits = Vec::new();
        if self.deterministic {
            traits.push("DETERMINISTIC".to_owned());
        }
        if let Some(a) = self
            .data_access
            .filter(|a| !a.eq_ignore_ascii_case("CONTAINS SQL"))
        {
            traits.push(a.to_owned());
        }
        if let Some(s) = self.security.filter(|s| !s.eq_ignore_ascii_case("DEFINER")) {
            traits.push(format!("SQL SECURITY {s}"));
        }
        if let Some(c) = self.comment.filter(|c| !c.is_empty()) {
            traits.push(format!("COMMENT {}", literal(c)));
        }
        for t in traits {
            out.push_str("\n    ");
            out.push_str(&t);
        }
        out.push('\n');
        out.push_str(self.body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definers() {
        assert_eq!(definer("oxbase@%"), "`oxbase`@`%`");
        assert_eq!(definer("a@b@localhost"), "`a@b`@`localhost`");
        assert_eq!(definer("x`y@h"), "`x``y`@`h`");
        assert_eq!(create_prefix(None), "CREATE ");
        assert_eq!(create_prefix(Some("")), "CREATE ");
    }

    #[test]
    fn routine_text() {
        let text = RoutineText {
            definer: Some("oxbase@%"),
            kind: "FUNCTION",
            name: "m2_fn",
            params: vec!["`n` int".into()],
            returns: Some("int"),
            deterministic: true,
            data_access: Some("NO SQL"),
            security: Some("DEFINER"),
            comment: Some("it's"),
            body: "RETURN n + 1",
        }
        .render();
        assert_eq!(
            text,
            "CREATE DEFINER=`oxbase`@`%` FUNCTION `m2_fn`(`n` int) RETURNS int\n    \
             DETERMINISTIC\n    NO SQL\n    COMMENT 'it''s'\nRETURN n + 1"
        );
    }
}
