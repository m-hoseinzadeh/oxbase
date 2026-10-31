//! Generated SQL scripts.

use std::fmt;

use serde::{Deserialize, Serialize};

/// An ordered list of statements produced by a DDL generator, a DML writer or a sync, plus
/// warnings for the user (data loss, unsupported properties that were skipped, ...).
///
/// Statements are stored without trailing terminators; [`Script::to_sql`] joins them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Script {
    pub statements: Vec<Statement>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// One statement of a [`Script`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Statement {
    /// SQL text without a trailing terminator.
    pub sql: String,
    /// `true` when running it can lose data (drops, narrowing type changes, deletes).
    #[serde(default)]
    pub destructive: bool,
}

impl Statement {
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            destructive: false,
        }
    }

    pub fn destructive(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            destructive: true,
        }
    }
}

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    /// A script with one non-destructive statement.
    pub fn single(sql: impl Into<String>) -> Self {
        let mut s = Self::new();
        s.push(sql);
        s
    }

    /// Appends a non-destructive statement.
    pub fn push(&mut self, sql: impl Into<String>) -> &mut Self {
        self.statements.push(Statement::new(sql));
        self
    }

    /// Appends a statement that can lose data.
    pub fn push_destructive(&mut self, sql: impl Into<String>) -> &mut Self {
        self.statements.push(Statement::destructive(sql));
        self
    }

    /// Adds a warning for the user.
    pub fn warn(&mut self, warning: impl Into<String>) -> &mut Self {
        self.warnings.push(warning.into());
        self
    }

    /// Appends another script's statements and warnings, keeping order.
    pub fn append(&mut self, other: Script) -> &mut Self {
        self.statements.extend(other.statements);
        self.warnings.extend(other.warnings);
        self
    }

    /// `true` when there are no statements (warnings alone do not count).
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    pub fn len(&self) -> usize {
        self.statements.len()
    }

    /// `true` if any statement is destructive.
    pub fn is_destructive(&self) -> bool {
        self.statements.iter().any(|s| s.destructive)
    }

    /// Joins the statements with `terminator` and newlines (`";"` for most engines, `"\nGO"`
    /// for SQL Server batch scripts).
    pub fn to_sql(&self, terminator: &str) -> String {
        let mut out = String::new();
        for s in &self.statements {
            out.push_str(&s.sql);
            out.push_str(terminator);
            out.push('\n');
        }
        out
    }
}

impl fmt::Display for Script {
    /// Statements terminated with `;`, one per line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_sql(";"))
    }
}

impl Extend<Statement> for Script {
    fn extend<I: IntoIterator<Item = Statement>>(&mut self, iter: I) {
        self.statements.extend(iter);
    }
}

impl FromIterator<Statement> for Script {
    fn from_iter<I: IntoIterator<Item = Statement>>(iter: I) -> Self {
        Self {
            statements: iter.into_iter().collect(),
            warnings: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn build_and_render() {
        let mut s = Script::new();
        s.push("CREATE TABLE t (id int)")
            .push_destructive("DROP TABLE old")
            .warn("old will be dropped");
        let mut tail = Script::single("COMMENT ON TABLE t IS 'x'");
        tail.warn("comment");
        s.append(tail);
        assert_eq!(s.len(), 3);
        assert!(s.is_destructive());
        assert_eq!(s.warnings, ["old will be dropped", "comment"]);
        assert_eq!(
            s.to_string(),
            "CREATE TABLE t (id int);\nDROP TABLE old;\nCOMMENT ON TABLE t IS 'x';\n"
        );
        assert_eq!(Script::single("SELECT 1").to_sql("\nGO"), "SELECT 1\nGO\n");
        assert!(Script::new().is_empty());
    }
}
