//! Generated SQL: DDL scripts ([`Script`], [`ScriptStep`]) and parameterized DML
//! ([`BoundStatement`]).
//!
//! # Scripts (M3)
//!
//! A [`Script`] is what a dialect `DdlGenerator` produces and what the designer previews and
//! applies. Each [`ScriptStep`] carries its SQL text (no trailing terminator), a human
//! description, whether it can lose data ([`ScriptStep::destructive`]) and whether it can run
//! inside a transaction on this engine ([`ScriptStep::transactional`]).
//!
//! Executors (the designer's apply, later sync and the CLI) run a script like this:
//!
//! 1. Every [`StepPhase::Prepare`] step, in order, outside any transaction. A failure stops here;
//!    nothing has changed yet except earlier prepare steps, which the finally steps undo.
//! 2. The [`StepPhase::Main`] steps, in order. When [`Script::is_atomic`] (every main step is
//!    transactional) and the engine has transactional DDL, they run in **one transaction**: any
//!    failure rolls all of them back. Otherwise each runs on its own (auto-commit) and a failure
//!    stops the script with the earlier main steps applied; previews must say so.
//! 3. Every [`StepPhase::Finally`] step, in order, after the transaction committed or rolled back
//!    (also after a failure in 1 or 2). Their failures become warnings.
//!
//! A step with [`ScriptStep::capture`] remembers the first column of the first row it returns (as
//! text; NULL and "no row" capture the empty string). A step with [`ScriptStep::only_if`] runs
//! only when that captured value equals the given text. A step with
//! [`StepCheck::ExpectNoRows`] fails when it returns any row. Together they express "turn
//! SQLite foreign keys off for a table rebuild, check them, and turn them back on only if they
//! were on":
//!
//! ```text
//! prepare  PRAGMA foreign_keys            capture "fk"
//! prepare  PRAGMA foreign_keys = OFF
//! main     CREATE TABLE ... / INSERT ... SELECT / DROP / RENAME / CREATE INDEX ...
//! main     PRAGMA foreign_key_check       expect no rows, only if fk = "1"
//! finally  PRAGMA foreign_keys = ON       only if fk = "1"
//! ```
//!
//! # Bound statements
//!
//! A [`BoundStatement`] is SQL text with positional placeholders in the engine's style
//! (`$1` PostgreSQL, `?` MySQL/MariaDB/SQLite) plus the [`Value`]s bound to them. Grid edits and
//! table-data queries only ever travel as bound statements: values are never written into SQL
//! text.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::Value;

/// When a [`ScriptStep`] runs relative to the script's transaction. See the module docs.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum StepPhase {
    /// Before the transaction opens, outside it (session settings that cannot change inside a
    /// transaction, such as SQLite `PRAGMA foreign_keys`).
    Prepare,
    /// The change itself.
    #[default]
    Main,
    /// After the transaction ended, whether it committed or not (restores what prepare steps
    /// changed).
    Finally,
}

/// What the executor checks after running a step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepCheck {
    /// Nothing; any result is ignored.
    #[default]
    None,
    /// The step is a query that must return no rows (SQLite `PRAGMA foreign_key_check`). Any row
    /// fails the script (rolling back its transaction); the error shows the first rows.
    ExpectNoRows,
}

/// Runs a step only when an earlier step captured `equals` under `capture`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepCondition {
    pub capture: String,
    pub equals: String,
}

/// One step of a [`Script`]. See the module docs for how executors run steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptStep {
    /// SQL text without a trailing terminator. Identifiers are quoted with the engine's
    /// `Quoting`; literals (comments, literal defaults) are escaped by the dialect.
    pub sql: String,
    /// What the step does, for the preview ("Add column note", "Rebuild table orders").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// `true` when running it can lose data: drops, a narrowing type change, a table rebuild that
    /// copies rows, deletes.
    #[serde(default)]
    pub destructive: bool,
    /// `true` when the step can run inside a transaction and is undone by its rollback. `false`
    /// for statements that commit implicitly (every MySQL/MariaDB DDL statement) or that must
    /// not run inside one.
    #[serde(default = "default_true")]
    pub transactional: bool,
    #[serde(default)]
    pub phase: StepPhase,
    #[serde(default)]
    pub check: StepCheck,
    /// Remember the first column of the first row this step returns under this name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<String>,
    /// Run only when an earlier capture has a given value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_if: Option<StepCondition>,
}

fn default_true() -> bool {
    true
}

impl ScriptStep {
    /// A transactional, non-destructive main step with no description.
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            description: String::new(),
            destructive: false,
            transactional: true,
            phase: StepPhase::Main,
            check: StepCheck::None,
            capture: None,
            only_if: None,
        }
    }

    /// Builder: sets the human description.
    #[must_use]
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Builder: marks the step as able to lose data.
    #[must_use]
    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    /// Builder: marks the step as committing implicitly / not undoable by a rollback.
    #[must_use]
    pub fn non_transactional(mut self) -> Self {
        self.transactional = false;
        self
    }

    /// Builder: sets the phase.
    #[must_use]
    pub fn in_phase(mut self, phase: StepPhase) -> Self {
        self.phase = phase;
        self
    }

    /// Builder: the step must return no rows.
    #[must_use]
    pub fn expect_no_rows(mut self) -> Self {
        self.check = StepCheck::ExpectNoRows;
        self
    }

    /// Builder: remember the step's first value under `name`.
    #[must_use]
    pub fn capture(mut self, name: impl Into<String>) -> Self {
        self.capture = Some(name.into());
        self
    }

    /// Builder: run only when capture `name` equals `value`.
    #[must_use]
    pub fn only_if(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.only_if = Some(StepCondition {
            capture: name.into(),
            equals: value.into(),
        });
        self
    }
}

/// An ordered list of steps produced by a DDL generator (or a sync), plus warnings for the user
/// (data loss, properties the engine cannot express that were skipped, ...).
///
/// Steps are kept in the order they were added; [`Script::ordered`] yields them in execution
/// order (prepare, main, finally; stable within a phase).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Script {
    pub steps: Vec<ScriptStep>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    /// A script with one transactional, non-destructive main step.
    pub fn single(sql: impl Into<String>) -> Self {
        let mut s = Self::new();
        s.push(sql);
        s
    }

    /// Appends a transactional, non-destructive main step.
    pub fn push(&mut self, sql: impl Into<String>) -> &mut Self {
        self.steps.push(ScriptStep::new(sql));
        self
    }

    /// Appends a main step that can lose data.
    pub fn push_destructive(&mut self, sql: impl Into<String>) -> &mut Self {
        self.steps.push(ScriptStep::new(sql).destructive());
        self
    }

    /// Appends a fully described step.
    pub fn push_step(&mut self, step: ScriptStep) -> &mut Self {
        self.steps.push(step);
        self
    }

    /// Adds a warning for the user.
    pub fn warn(&mut self, warning: impl Into<String>) -> &mut Self {
        self.warnings.push(warning.into());
        self
    }

    /// Appends another script's steps and warnings, keeping order.
    pub fn append(&mut self, other: Script) -> &mut Self {
        self.steps.extend(other.steps);
        self.warnings.extend(other.warnings);
        self
    }

    /// `true` when there are no steps (warnings alone do not count).
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// `true` if any step is destructive.
    pub fn is_destructive(&self) -> bool {
        self.steps.iter().any(|s| s.destructive)
    }

    /// `true` when every main step can run inside one transaction (so, on an engine with
    /// transactional DDL, the change is all-or-nothing). A script with no main step is atomic.
    pub fn is_atomic(&self) -> bool {
        self.steps
            .iter()
            .filter(|s| s.phase == StepPhase::Main)
            .all(|s| s.transactional)
    }

    /// Steps in execution order: prepare, main, finally (stable within a phase).
    pub fn ordered(&self) -> impl Iterator<Item = &ScriptStep> {
        [StepPhase::Prepare, StepPhase::Main, StepPhase::Finally]
            .into_iter()
            .flat_map(move |phase| self.steps.iter().filter(move |s| s.phase == phase))
    }

    /// The SQL of every step in execution order, each followed by `terminator` and a newline
    /// (`";"` for most engines, `"\nGO"` for SQL Server batch scripts). No transaction control
    /// statements are added.
    pub fn to_sql(&self, terminator: &str) -> String {
        let mut out = String::new();
        for s in self.ordered() {
            out.push_str(&s.sql);
            out.push_str(terminator);
            out.push('\n');
        }
        out
    }
}

impl fmt::Display for Script {
    /// Steps terminated with `;`, one per line, in execution order.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_sql(";"))
    }
}

impl Extend<ScriptStep> for Script {
    fn extend<I: IntoIterator<Item = ScriptStep>>(&mut self, iter: I) {
        self.steps.extend(iter);
    }
}

impl FromIterator<ScriptStep> for Script {
    fn from_iter<I: IntoIterator<Item = ScriptStep>>(iter: I) -> Self {
        Self {
            steps: iter.into_iter().collect(),
            warnings: Vec::new(),
        }
    }
}

/// One statement with positional placeholders in the engine's style and the values bound to
/// them, in placeholder order. See the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BoundStatement {
    pub sql: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Value>,
}

impl BoundStatement {
    pub fn new(sql: impl Into<String>, params: Vec<Value>) -> Self {
        Self {
            sql: sql.into(),
            params,
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
        assert!(s.is_atomic());
        assert_eq!(s.warnings, ["old will be dropped", "comment"]);
        assert_eq!(
            s.to_string(),
            "CREATE TABLE t (id int);\nDROP TABLE old;\nCOMMENT ON TABLE t IS 'x';\n"
        );
        assert_eq!(Script::single("SELECT 1").to_sql("\nGO"), "SELECT 1\nGO\n");
        assert_eq!(Script::new().len(), 0);
        assert!(Script::new().is_atomic());
    }

    #[test]
    fn phases_order_execution_and_atomicity() {
        let mut s = Script::new();
        s.push_step(
            ScriptStep::new("PRAGMA foreign_keys = ON")
                .in_phase(StepPhase::Finally)
                .only_if("fk", "1"),
        )
        .push_step(
            ScriptStep::new("PRAGMA foreign_keys")
                .in_phase(StepPhase::Prepare)
                .capture("fk"),
        )
        .push_step(ScriptStep::new("ALTER TABLE t ADD c int").described("Add column c"))
        .push_step(
            ScriptStep::new("PRAGMA foreign_key_check")
                .expect_no_rows()
                .only_if("fk", "1"),
        );
        let order: Vec<&str> = s.ordered().map(|st| st.sql.as_str()).collect();
        assert_eq!(
            order,
            [
                "PRAGMA foreign_keys",
                "ALTER TABLE t ADD c int",
                "PRAGMA foreign_key_check",
                "PRAGMA foreign_keys = ON"
            ]
        );
        assert!(s.is_atomic());
        // A non-transactional prepare/finally step does not break atomicity, a main one does.
        s.push_step(
            ScriptStep::new("SET x")
                .non_transactional()
                .in_phase(StepPhase::Prepare),
        );
        assert!(s.is_atomic());
        s.push_step(
            ScriptStep::new("ALTER TABLE t DROP c")
                .non_transactional()
                .destructive(),
        );
        assert!(!s.is_atomic());
        assert!(s.is_destructive());
    }

    #[test]
    fn serde_defaults_and_round_trip() {
        let step: ScriptStep = serde_json::from_str(r#"{"sql":"SELECT 1"}"#).unwrap();
        assert_eq!(step, ScriptStep::new("SELECT 1"));
        let full = ScriptStep::new("PRAGMA foreign_key_check")
            .described("Check foreign keys")
            .expect_no_rows()
            .only_if("fk", "1");
        let json = serde_json::to_value(&full).unwrap();
        assert_eq!(json["check"], "expect_no_rows");
        assert_eq!(json["phase"], "main");
        assert_eq!(json["only_if"]["capture"], "fk");
        assert_eq!(serde_json::from_value::<ScriptStep>(json).unwrap(), full);

        let bound = BoundStatement::new("SELECT * FROM t WHERE id = $1", vec![Value::Int(3)]);
        let json = serde_json::to_string(&bound).unwrap();
        assert_eq!(
            serde_json::from_str::<BoundStatement>(&json).unwrap(),
            bound
        );
    }
}
