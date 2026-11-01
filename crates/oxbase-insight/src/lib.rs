//! Code insight for the SQL editor (M6, `docs/m6-contracts.md`): cursor context, schema-aware
//! completion, diagnostics, hover and formatting, for PostgreSQL, MySQL, MariaDB, SQLite and
//! SQL Server.
//!
//! # Approach
//!
//! No grammar library: a **tolerant, hand-written parser** over the tokens of `oxbase-sql`
//! (the same lexer the statement splitter uses), because incomplete and invalid SQL is the
//! normal case while typing. See `docs/m6-contracts.md` §1 for the evaluation of tree-sitter
//! grammars and `sqlparser`.
//!
//! - [`outline`] reads one statement into an [`Outline`]: its class, query blocks with their
//!   relations (tables, CTEs, derived tables, with aliases) and every identifier reference with
//!   its role. It never fails; missing pieces are simply absent.
//! - [`context_at`] finds the statement around the cursor, outlines it and says what may be
//!   typed at the cursor ([`CursorContext`]: expected item kinds, the partial word and its
//!   replace range, the qualifier before a `.`, the relations in scope).
//! - [`complete`] ranks completions from the [`SchemaCache`], the dialect's [`SqlLanguage`]
//!   data and snippets.
//! - [`diagnose`] reports lexical errors (unterminated strings, unbalanced parentheses) and,
//!   with a cache, unknown names as hints or warnings ([`DiagnosticCode::max_severity`]).
//! - [`hover`] describes the identifier under the cursor.
//! - [`format`] / [`format_range`] re-indent and re-case keywords; their output always passes
//!   [`check_format`] (same tokens apart from whitespace and keyword case).
//!
//! Offsets are **byte offsets** into the text everywhere in this crate; the app converts to
//! and from the editor's UTF-16 offsets (`oxbase_sql::byte_offset_from_utf16`,
//! `oxbase_sql::utf16_offset_from_byte`).
//!
//! # Owners (`docs/m6-contracts.md` §1)
//!
//! Everything here is final except the bodies marked `unimplemented!("m6: <stream>")`:
//! [`outline`] and [`context_at`] (**context**), [`complete`] (**complete**), [`diagnose`] and
//! [`hover`] (**diagnose**), [`format`] and [`format_range`] (**format**).

use std::fmt;
use std::sync::Arc;

use oxbase_dialect::{
    AnsiQuoting, BacktickQuoting, BracketQuoting, DialectRegistry, PgQuoting, Quoting,
};
use oxbase_types::{CaseRules, Engine, Error, IdentCase, Result, ServerVersion};

pub use oxbase_dialect::SqlLanguage;
pub use oxbase_schema_cache::SchemaCache;
pub use oxbase_sql::SqlDialect;
pub use tokio_util::sync::CancellationToken;

mod complete;
mod context;
mod diagnose;
mod format;
mod hover;
mod outline;
pub mod snippets;

pub use complete::{
    Completion, CompletionKind, CompletionList, RecentNames, complete, complete_with_recent,
};
pub use context::{CursorContext, Expected, context_at};
pub use diagnose::{Diagnostic, DiagnosticCode, Severity, diagnose};
pub use format::{FormatOptions, KeywordCase, TextEdit, check_format, format, format_range};
pub use hover::{Hover, hover};
pub use outline::{
    Clause, Cte, NameRef, Outline, QueryBlock, RefRole, RelationSource, ScopeRelation,
    StatementClass, outline,
};

/// An identifier as written: its value without quotes and whether it was quoted.
pub type Ident = oxbase_sql::NamePart;

/// Everything insight knows about the dialect of an editor.
#[derive(Clone)]
pub struct Dialect {
    pub engine: Engine,
    /// Lexing and statement rules.
    pub sql: SqlDialect,
    /// Keywords, functions, types (the engine's registered data or the standard core).
    pub language: Arc<SqlLanguage>,
    /// Identifier quoting for inserted names.
    pub quoting: Arc<dyn Quoting>,
    /// The connected server's version, when known (filters version-gated functions).
    pub version: Option<ServerVersion>,
    /// How names match; the engine default until a cache with the server's rules is at hand
    /// (completion and diagnostics prefer [`SchemaCache::case_rules`]).
    pub case_rules: CaseRules,
}

impl Dialect {
    /// The dialect of `engine` from the registry: its [`SqlLanguage`] (or the standard core)
    /// and its [`Quoting`]. `Error::Unsupported` for engines without SQL (MongoDB, Redis) or
    /// without a registered `Quoting`.
    pub fn from_registry(
        registry: &DialectRegistry,
        engine: Engine,
        version: Option<ServerVersion>,
    ) -> Result<Self> {
        let sql = SqlDialect::for_engine(engine).ok_or_else(|| {
            Error::unsupported(format!("{} has no SQL editor", engine.display_name()))
        })?;
        Ok(Self {
            engine,
            sql,
            language: registry.language(engine),
            quoting: registry.quoting(engine)?,
            version,
            case_rules: CaseRules::engine_default(engine),
        })
    }

    /// The dialect of `engine` with the standard language data and the engine's usual quoting
    /// style, without a registry (tests, and engines whose cells are not registered).
    /// `Error::Unsupported` for MongoDB and Redis.
    pub fn builtin(engine: Engine) -> Result<Self> {
        let sql = SqlDialect::for_engine(engine).ok_or_else(|| {
            Error::unsupported(format!("{} has no SQL editor", engine.display_name()))
        })?;
        let quoting: Arc<dyn Quoting> = match sql {
            SqlDialect::Postgres => Arc::new(PgQuoting),
            SqlDialect::MySql { .. } => Arc::new(BacktickQuoting),
            SqlDialect::Sqlite => Arc::new(AnsiQuoting),
            SqlDialect::TSql => Arc::new(BracketQuoting),
        };
        Ok(Self {
            engine,
            sql,
            language: Arc::new(SqlLanguage::standard()),
            quoting,
            version: None,
            case_rules: CaseRules::engine_default(engine),
        })
    }

    /// The same dialect with other language data.
    #[must_use]
    pub fn with_language(mut self, language: SqlLanguage) -> Self {
        self.language = Arc::new(language);
        self
    }

    /// `name` as it must be written to mean the catalog name `name` under `case`: bare when
    /// [`SqlLanguage::needs_quotes`] says it may be, else quoted with the dialect's
    /// [`Quoting`].
    pub fn quote_name(&self, name: &str, case: IdentCase) -> String {
        if self.language.needs_quotes(name, case) {
            self.quoting.quote_ident(name)
        } else {
            name.to_owned()
        }
    }

    /// A qualified name (`schema.table`), each part quoted only where needed.
    pub fn quote_path(&self, parts: &[&str], case: IdentCase) -> String {
        parts
            .iter()
            .map(|p| self.quote_name(p, case))
            .collect::<Vec<_>>()
            .join(".")
    }
}

impl fmt::Debug for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dialect")
            .field("engine", &self.engine)
            .field("sql", &self.sql)
            .field("version", &self.version)
            .field("case_rules", &self.case_rules)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_dialects() {
        for engine in [
            Engine::Postgres,
            Engine::MySql,
            Engine::MariaDb,
            Engine::Sqlite,
            Engine::MsSql,
        ] {
            let d = Dialect::builtin(engine).unwrap();
            assert_eq!(Some(d.sql), SqlDialect::for_engine(engine));
            assert_eq!(d.case_rules, CaseRules::engine_default(engine));
        }
        assert_eq!(
            Dialect::builtin(Engine::Redis).unwrap_err().category(),
            oxbase_types::ErrorCategory::Unsupported
        );
        let d = Dialect::builtin(Engine::Postgres).unwrap();
        assert!(format!("{d:?}").starts_with("Dialect"));
    }

    #[test]
    fn from_registry_uses_the_registered_cells() {
        let mut reg = DialectRegistry::new();
        assert_eq!(
            Dialect::from_registry(&reg, Engine::MsSql, None)
                .unwrap_err()
                .category(),
            oxbase_types::ErrorCategory::Unsupported
        );
        reg.register_quoting(Engine::MsSql, Arc::new(BracketQuoting));
        let d = Dialect::from_registry(&reg, Engine::MsSql, None).unwrap();
        assert_eq!(d.sql, SqlDialect::TSql);
        assert_eq!(
            d.quote_name("my table", IdentCase::Insensitive),
            "[my table]"
        );
    }

    #[test]
    fn names_are_quoted_only_when_needed() {
        let pg = Dialect::builtin(Engine::Postgres).unwrap();
        let lower = IdentCase::FoldLower;
        assert_eq!(pg.quote_name("orders", lower), "orders");
        assert_eq!(pg.quote_name("Orders", lower), "\"Orders\"");
        assert_eq!(pg.quote_name("select", lower), "\"select\"");
        assert_eq!(
            pg.quote_path(&["sales", "Order Lines"], lower),
            "sales.\"Order Lines\""
        );
        let my = Dialect::builtin(Engine::MySql).unwrap();
        assert_eq!(my.quote_name("Orders", IdentCase::Sensitive), "Orders");
        assert_eq!(my.quote_name("a b", IdentCase::Sensitive), "`a b`");
        let pg = pg.with_language(SqlLanguage {
            reserved: &[],
            ..SqlLanguage::standard()
        });
        assert_eq!(pg.quote_name("select", lower), "select");
    }
}
