//! Where an editor's statements come from (M7): the SQL splitter of `oxbase_sql` for the SQL
//! engines, the engine's registered [`ScriptLanguage`] (mongo shell, Redis console) for the
//! others. The query service and history only talk to [`StatementSource`], so they run every
//! engine's editor the same way and keep no engine special case. Contract:
//! `docs/m7-contracts.md` §4.1.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use oxbase_dialect::{DialectRegistry, ScriptLanguage, StatementGuard};
use oxbase_sql::{SqlDialect, Statement, StatementKind};
use oxbase_types::{Engine, Result};

/// How an editor's script is split, guarded and redacted.
#[derive(Clone)]
pub enum StatementSource {
    /// A SQL engine: `oxbase_sql::split` and `oxbase_sql::redact_secrets` for its dialect.
    Sql(SqlDialect),
    /// An engine without SQL: its registered editor language.
    Language(Arc<dyn ScriptLanguage>),
}

impl StatementSource {
    /// The source of `engine`'s editor: its SQL dialect, else its registered
    /// [`ScriptLanguage`] (`Unsupported` when it has neither).
    pub fn for_engine(dialects: &DialectRegistry, engine: Engine) -> Result<Self> {
        match SqlDialect::for_engine(engine) {
            Some(dialect) => Ok(Self::Sql(dialect)),
            None => dialects.script_language(engine).map(Self::Language),
        }
    }

    /// The SQL dialect, for SQL-only work (schema-change detection); `None` for other
    /// languages.
    pub fn sql_dialect(&self) -> Option<SqlDialect> {
        match self {
            Self::Sql(dialect) => Some(*dialect),
            Self::Language(_) => None,
        }
    }

    /// A short name for logs and `Debug`: the SQL dialect or the language's name.
    pub fn name(&self) -> Cow<'static, str> {
        match self {
            Self::Sql(dialect) => Cow::Owned(format!("sql:{dialect:?}")),
            Self::Language(language) => Cow::Borrowed(language.name()),
        }
    }

    /// The statements of `script`, in order.
    pub fn split(&self, script: &str) -> Vec<Statement> {
        match self {
            Self::Sql(dialect) => oxbase_sql::split(*dialect, script),
            Self::Language(language) => language.split(script),
        }
    }

    /// `text` as one statement spanning all of it (a bound statement or a result source is
    /// never split); its kind is the kind of the first statement the splitter finds.
    pub fn whole(&self, text: &str) -> Statement {
        let kind = self
            .split(text)
            .first()
            .map_or(StatementKind::Other, |s| s.kind);
        Statement {
            range: 0..text.len(),
            segment_end: text.len(),
            start: oxbase_sql::position_at(text, 0),
            end: oxbase_sql::position_at(text, text.len()),
            text: text.to_owned(),
            kind,
        }
    }

    /// `text` with secrets replaced (SQL: `oxbase_sql::redact_secrets`).
    pub fn redact_secrets<'a>(&self, text: &'a str) -> Cow<'a, str> {
        match self {
            Self::Sql(dialect) => oxbase_sql::redact_secrets(*dialect, text),
            Self::Language(language) => language.redact_secrets(text),
        }
    }

    /// Whether `statement` may run from the editor (SQL statements always may).
    pub fn guard(&self, statement: &Statement) -> StatementGuard {
        match self {
            Self::Sql(_) => StatementGuard::Allowed,
            Self::Language(language) => language.guard(statement),
        }
    }
}

impl fmt::Debug for StatementSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("StatementSource")
            .field(&self.name())
            .finish()
    }
}

impl PartialEq for StatementSource {
    /// Same SQL dialect, or languages with the same name.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Sql(a), Self::Sql(b)) => a == b,
            (Self::Language(a), Self::Language(b)) => a.name() == b.name(),
            _ => false,
        }
    }
}

impl Eq for StatementSource {}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_dialect::script::{statement, trim_range};

    /// One statement per non-empty line; `FLUSHALL` needs confirmation, `MONITOR` is blocked;
    /// `AUTH <pw>` is redacted.
    struct Console;

    impl ScriptLanguage for Console {
        fn name(&self) -> &'static str {
            "test-console"
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
                if let Some(trimmed) = trim_range(text, range.clone()) {
                    let kind = if text[trimmed.clone()].starts_with("GET") {
                        StatementKind::Query
                    } else {
                        StatementKind::Other
                    };
                    out.push(statement(text, trimmed, range.end, kind));
                }
            }
            out
        }

        fn redact_secrets<'a>(&self, text: &'a str) -> Cow<'a, str> {
            match text.strip_prefix("AUTH ") {
                Some(_) => Cow::Owned("AUTH ***".to_owned()),
                None => Cow::Borrowed(text),
            }
        }

        fn guard(&self, statement: &Statement) -> StatementGuard {
            match statement.text.as_str() {
                "FLUSHALL" => StatementGuard::Confirm("FLUSHALL deletes every key".to_owned()),
                "MONITOR" => StatementGuard::Blocked("use the monitor panel".to_owned()),
                _ => StatementGuard::Allowed,
            }
        }
    }

    #[test]
    fn sql_engines_use_the_sql_splitter() {
        let reg = DialectRegistry::new();
        let source = StatementSource::for_engine(&reg, Engine::Postgres).unwrap();
        assert_eq!(source.sql_dialect(), Some(SqlDialect::Postgres));
        let statements = source.split("SELECT 1; SELECT 2");
        assert_eq!(statements.len(), 2);
        assert_eq!(source.guard(&statements[0]), StatementGuard::Allowed);
        assert_eq!(
            source.redact_secrets("ALTER USER a PASSWORD 'x'"),
            "ALTER USER a PASSWORD '***'"
        );
        assert_eq!(source, StatementSource::Sql(SqlDialect::Postgres));
        assert_ne!(source, StatementSource::Sql(SqlDialect::Sqlite));
    }

    #[test]
    fn other_engines_use_their_registered_language() {
        let mut reg = DialectRegistry::new();
        let err = StatementSource::for_engine(&reg, Engine::Redis).unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::Unsupported);

        reg.register_script_language(Engine::Redis, Arc::new(Console));
        let source = StatementSource::for_engine(&reg, Engine::Redis).unwrap();
        assert_eq!(source.sql_dialect(), None);
        assert_eq!(source.name(), "test-console");
        let statements = source.split("GET a\nFLUSHALL\n\nMONITOR\n");
        assert_eq!(
            statements
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>(),
            ["GET a", "FLUSHALL", "MONITOR"]
        );
        assert!(matches!(
            source.guard(&statements[1]),
            StatementGuard::Confirm(_)
        ));
        assert!(matches!(
            source.guard(&statements[2]),
            StatementGuard::Blocked(_)
        ));
        assert_eq!(source.redact_secrets("AUTH hunter2"), "AUTH ***");
        let whole = source.whole("GET a\nGET b");
        assert_eq!(whole.range, 0..11);
        assert_eq!(whole.kind, StatementKind::Query);
        assert_eq!(format!("{source:?}"), "StatementSource(\"test-console\")");
        assert_eq!(source, StatementSource::Language(Arc::new(Console)));
    }
}
