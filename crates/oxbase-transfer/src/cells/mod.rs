//! The per-engine [`TypeMapper`](oxbase_dialect::TypeMapper) cells (`docs/m8-contracts.md`
//! §4.1), one module per SQL engine. Owner of the bodies marked `unimplemented!("m8:
//! transfer")`: the **transfer** stream.
//!
//! Each module documents its table: per logical type, the target spelling and the fidelity
//! with its reason. The facts come from the engines' public documentation (type ranges,
//! precision limits, time-zone handling); the golden tests per engine pair pin
//! [`crate::mapping::pair_report`] for [`crate::mapping::representative_types`].

pub mod mssql;
pub mod mysql;
pub mod postgres;
pub mod sqlite;

use oxbase_types::{Engine, ServerVersion};

/// How a literal default of a column of unbounded text (`text`, `longtext`, `nvarchar(max)`)
/// is written on the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextDefaults {
    /// As the literal (`DEFAULT 'x'`).
    Plain,
    /// As an expression in parentheses (`DEFAULT ('x')`: MySQL 8.0.13+, which refuses a
    /// literal default on `TEXT` / `BLOB` columns).
    Parenthesized,
    /// Not at all (older MySQL): the default is dropped with a plan warning.
    Dropped,
}

/// What a target engine accepts in a table the transfer creates and in the rows it loads,
/// beyond its [`TypeMapper`](oxbase_dialect::TypeMapper) (facts from the engines' public
/// documentation). One value per engine and server version ([`target_rules`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetRules {
    /// Unbounded text and binary columns can be primary-key columns (PostgreSQL, SQLite).
    /// Else a key column of such a type is created as `varchar(key_text_length)` /
    /// `varbinary(key_text_length)` (narrowed) across engines.
    pub keys_on_unbounded: bool,
    /// Length of a bounded key column (characters or bytes) when `keys_on_unbounded` is false.
    pub key_text_length: u32,
    /// The bytes one index key may hold, where `CREATE` fails beyond it (MySQL / MariaDB
    /// InnoDB: 3 072); `None`: no limit the DDL checks.
    pub index_key_bytes: Option<u32>,
    /// Bytes one character of a text key column counts for `index_key_bytes` (`utf8mb4`: 4).
    pub key_bytes_per_char: u32,
    /// How a literal default on unbounded text is written.
    pub text_defaults: TextDefaults,
    /// The literals true and false in a boolean column's default.
    pub bool_literals: (&'static str, &'static str),
    /// Text lengths count UTF-16 code units, not characters (SQL Server `nchar` / `nvarchar`).
    pub utf16_lengths: bool,
    /// Float columns store NaN and infinities (PostgreSQL, SQLite).
    pub non_finite_floats: bool,
    /// Switching foreign-key checks back on checks the rows loaded while they were off (SQL
    /// Server `WITH CHECK CHECK CONSTRAINT`); MySQL / MariaDB and SQLite do not.
    pub fk_reenable_validates: bool,
}

/// The [`TargetRules`] of `engine` on server `version` (MongoDB and Redis, which are not
/// transfer endpoints, get PostgreSQL's permissive rules).
pub fn target_rules(engine: Engine, version: &ServerVersion) -> TargetRules {
    match engine {
        Engine::MySql | Engine::MariaDb => mysql::target_rules(engine == Engine::MariaDb, version),
        Engine::MsSql => mssql::TARGET_RULES,
        Engine::Sqlite => sqlite::TARGET_RULES,
        _ => postgres::TARGET_RULES,
    }
}

/// `precision` capped at `max` fractional-second digits, and whether the cap cut digits.
/// `None` (the engine's default precision) stays `None`.
pub(crate) fn cap_precision(precision: Option<u8>, max: u8) -> (Option<u8>, bool) {
    match precision {
        Some(p) if p > max => (Some(max), true),
        other => (other, false),
    }
}

/// The labels of a list of SQL string literals (`'a', 'b''c'`): every single-quoted string,
/// with doubled quotes (and backslash escapes) undone.
pub(crate) fn quoted_labels(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' {
            continue;
        }
        let mut label = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\'' if chars.peek() == Some(&'\'') => {
                    chars.next();
                    label.push('\'');
                }
                '\'' => break,
                '\\' => {
                    if let Some(next) = chars.next() {
                        label.push(next);
                    }
                }
                c => label.push(c),
            }
        }
        out.push(label);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(cap_precision(Some(7), 6), (Some(6), true));
        assert_eq!(cap_precision(Some(3), 6), (Some(3), false));
        assert_eq!(cap_precision(None, 6), (None, false));
        assert_eq!(
            quoted_labels("'a', 'b''c' ,'d'"),
            vec!["a".to_owned(), "b'c".to_owned(), "d".to_owned()]
        );
        assert_eq!(quoted_labels(""), Vec::<String>::new());
    }
}
